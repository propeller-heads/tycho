use std::{
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::keccak256;
use async_trait::async_trait;
use futures::stream::BoxStream;
use num_bigint::BigUint;
use reqwest::Client;
use tokio::time::interval;
use tracing::{error, info};
use tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage};
use tycho_common::{
    models::{
        protocol::{GetAmountOutParams, ProtocolComponent, ProtocolComponentState},
        Chain,
    },
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use super::models::{Aqua0Market, Aqua0QuoteRequest, Aqua0QuoteResponse, Aqua0StateResponse};
use crate::rfq::{client::RFQClient, errors::RFQError, models::TimestampHeader};

static AQUA0_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Aqua0Client {
    chain: Chain,
    base_url: String,
    market: Aqua0Market,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl fmt::Debug for Aqua0Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Aqua0Client")
            .field("chain", &self.chain)
            .field("market", &self.market)
            .finish_non_exhaustive()
    }
}

impl Aqua0Client {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:aqua0";

    pub fn new(
        chain: Chain,
        base_url: String,
        market: Aqua0Market,
        poll_time: Duration,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        if !matches!(chain, Chain::Base | Chain::Arbitrum | Chain::Polygon | Chain::Robinhood) {
            return Err(RFQError::InvalidInput(format!(
                "Aqua0 has no existing Tycho Router V3 and Uniswap V4 executor on {chain}"
            )));
        }
        if market.amount0_samples.is_empty() || market.amount1_samples.is_empty() {
            return Err(RFQError::InvalidInput(
                "Aqua0 requires at least one sample amount in each direction".into(),
            ));
        }
        if poll_time.is_zero() || quote_timeout.is_zero() {
            return Err(RFQError::InvalidInput("Aqua0 timeouts must be positive".into()));
        }
        let pool_id = Bytes::from_str(&market.pool_id)
            .map_err(|_| RFQError::InvalidInput("Invalid Aqua0 pool ID".into()))?;
        if pool_id.len() != 32 || BigUint::from_str(&market.class_id).is_err() {
            return Err(RFQError::InvalidInput("Invalid Aqua0 pool or class ID".into()));
        }
        for amount in market
            .amount0_samples
            .iter()
            .chain(&market.amount1_samples)
        {
            if BigUint::from_str(amount).map_or(true, |value| value == BigUint::default()) {
                return Err(RFQError::InvalidInput(format!(
                    "Invalid Aqua0 sample amount: {amount}"
                )));
            }
        }

        Ok(Self {
            chain,
            base_url: base_url
                .trim_end_matches('/')
                .to_string(),
            market,
            poll_time,
            quote_timeout,
        })
    }

    // This is the backend's aqua0RfqComponentId wire format. It depends only on the configured
    // market, so obtaining a binding quote never needs another indicative-state request.
    pub fn component_id(&self) -> Result<String, RFQError> {
        let class_id = BigUint::from_str(&self.market.class_id)
            .map_err(|_| RFQError::InvalidInput("Invalid Aqua0 class ID".into()))?;
        Ok(format!(
            "{:#x}",
            keccak256(format!(
                "aqua0-rfq-v1:{}:{}:{}",
                self.chain.id(),
                self.market.pool_id.to_lowercase(),
                class_id
            ))
        ))
    }

    async fn response_text(response: reqwest::Response, seam: &str) -> Result<String, RFQError> {
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(RFQError::from)?;
        if !status.is_success() {
            return Err(RFQError::QuoteNotFound(format!("Aqua0 {seam} returned {status}: {body}")));
        }
        Ok(body)
    }

    pub async fn fetch_state(&self) -> Result<Aqua0StateResponse, RFQError> {
        let response = AQUA0_HTTP_CLIENT
            .get(format!("{}/state", self.base_url))
            .timeout(self.quote_timeout)
            .query(&[
                ("chainId", self.chain.id().to_string()),
                ("poolId", self.market.pool_id.clone()),
                ("classId", self.market.class_id.clone()),
                ("amount0Samples", self.market.amount0_samples.join(",")),
                ("amount1Samples", self.market.amount1_samples.join(",")),
            ])
            .send()
            .await?;
        let body = Self::response_text(response, "state request").await?;
        let state: Aqua0StateResponse = serde_json::from_str(&body)
            .map_err(|error| RFQError::ParsingError(format!("Invalid Aqua0 state: {error}")))?;
        if state.chain_id != self.chain.id() ||
            !state
                .pool_id
                .eq_ignore_ascii_case(&self.market.pool_id) ||
            state.component_id != self.component_id()? ||
            BigUint::from_str(&state.class_id).ok() !=
                BigUint::from_str(&self.market.class_id).ok()
        {
            return Err(RFQError::FatalError(
                "Aqua0 state identity does not match the configured market".into(),
            ));
        }
        if state.schema_version != "aqua0-rfq-state-v1" ||
            state.protocol_system != Self::PROTOCOL_SYSTEM ||
            state.protocol_type_name != "aqua0_jit_pool"
        {
            return Err(RFQError::FatalError(
                "Aqua0 state uses an unsupported schema or protocol identity".into(),
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| RFQError::FatalError(error.to_string()))?
            .as_secs();
        if state.expires_at <= now {
            return Err(RFQError::QuoteNotFound("Aqua0 state is expired".into()));
        }
        Ok(state)
    }

    pub async fn fetch_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<Aqua0QuoteResponse, RFQError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let component_id = self.component_id()?;
        let request = Aqua0QuoteRequest {
            request_id: request_id.clone(),
            component_id: component_id.clone(),
            chain_id: self.chain.id(),
            pool_id: self.market.pool_id.clone(),
            class_id: self.market.class_id.clone(),
            token_in: params.token_in.to_string(),
            token_out: params.token_out.to_string(),
            amount_in: params.amount_in.to_string(),
            expected_router: params.sender.to_string(),
        };

        let response = AQUA0_HTTP_CLIENT
            .post(format!("{}/quote", self.base_url))
            .timeout(self.quote_timeout)
            .json(&request)
            .send()
            .await?;
        let body = Self::response_text(response, "binding quote").await?;
        let quote: Aqua0QuoteResponse = serde_json::from_str(&body)
            .map_err(|error| RFQError::ParsingError(format!("Invalid Aqua0 quote: {error}")))?;

        let amount_out = BigUint::from_str(&quote.amount_out)
            .map_err(|_| RFQError::ParsingError("Invalid Aqua0 amountOut".into()))?;
        let deadline = quote
            .deadline
            .parse::<u64>()
            .map_err(|_| RFQError::ParsingError("Invalid Aqua0 deadline".into()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| RFQError::FatalError(error.to_string()))?
            .as_secs();

        if quote.schema_version != "aqua0-rfq-quote-v1" ||
            quote.request_id != request_id ||
            quote.component_id != component_id ||
            quote.chain_id != self.chain.id() ||
            quote.token_in.to_lowercase() !=
                params
                    .token_in
                    .to_string()
                    .to_lowercase() ||
            quote.token_out.to_lowercase() !=
                params
                    .token_out
                    .to_string()
                    .to_lowercase() ||
            quote.amount_in != params.amount_in.to_string() ||
            quote.router.to_lowercase() != params.sender.to_string().to_lowercase() ||
            quote.executor.to_lowercase() != params.sender.to_string().to_lowercase() ||
            amount_out == BigUint::default() ||
            deadline <= now
        {
            return Err(RFQError::FatalError(
                "Aqua0 binding quote does not match the requested swap and Tycho router".into(),
            ));
        }
        Ok(quote)
    }

    fn component_with_state(
        &self,
        state: &Aqua0StateResponse,
    ) -> Result<ComponentWithState, RFQError> {
        let tokens = state
            .tokens
            .iter()
            .map(|token| {
                Bytes::from_str(token).map_err(|error| {
                    RFQError::ParsingError(format!("Invalid Aqua0 token {token}: {error}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let hooks = Bytes::from_str(&state.hooks)
            .map_err(|error| RFQError::ParsingError(format!("Invalid Aqua0 hook: {error}")))?;
        let manager = Bytes::from_str(&state.pool_manager)
            .map_err(|error| RFQError::ParsingError(format!("Invalid pool manager: {error}")))?;

        let mut static_attributes = HashMap::new();
        static_attributes.insert("key_lp_fee".into(), state.fee.to_be_bytes().to_vec().into());
        static_attributes.insert(
            "tick_spacing".into(),
            state
                .tick_spacing
                .to_be_bytes()
                .to_vec()
                .into(),
        );
        static_attributes.insert("hooks".into(), hooks.clone());
        static_attributes.insert("api_url".into(), self.base_url.as_bytes().to_vec().into());
        static_attributes.insert(
            "amount0_samples".into(),
            self.market
                .amount0_samples
                .join(",")
                .into_bytes()
                .into(),
        );
        static_attributes.insert(
            "amount1_samples".into(),
            self.market
                .amount1_samples
                .join(",")
                .into_bytes()
                .into(),
        );

        let component = ProtocolComponent {
            id: state.component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.into(),
            protocol_type_name: "aqua0_jit_pool".into(),
            chain: self.chain,
            tokens,
            contract_addresses: vec![manager, hooks],
            static_attributes,
            ..Default::default()
        };
        let mut attributes = HashMap::new();
        attributes.insert(
            "state".into(),
            serde_json::to_vec(state)
                .map_err(|error| RFQError::ParsingError(error.to_string()))?
                .into(),
        );
        Ok(ComponentWithState {
            state: ProtocolComponentState::new(&state.component_id, attributes, HashMap::new()),
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        })
    }
}

#[async_trait]
impl RFQClient for Aqua0Client {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let client = self.clone();
        Box::pin(async_stream::stream! {
            let mut ticker = interval(client.poll_time);
            info!("Starting Aqua0 RFQ polling every {} seconds", client.poll_time.as_secs());
            loop {
                ticker.tick().await;
                match client.fetch_state().await {
                    Ok(state) => {
                        let component = client.component_with_state(&state)?;
                        let states = HashMap::from([(state.component_id.clone(), component)]);
                        yield Ok(("aqua0".into(), StateSyncMessage {
                            header: TimestampHeader { timestamp: state.generated_at },
                            snapshots: Snapshot { states, vm_storage: HashMap::new() },
                            deltas: None,
                            removed_components: HashMap::new(),
                        }));
                    }
                    Err(error) => {
                        error!("Failed to fetch Aqua0 RFQ state: {error}");
                    }
                }
            }
        })
    }

    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let quote = self.fetch_binding_quote(params).await?;
        let hook_data = Bytes::from_str(&quote.hook_data)
            .map_err(|error| RFQError::ParsingError(format!("Invalid hookData: {error}")))?;
        let amount_out = BigUint::from_str(&quote.amount_out)
            .map_err(|_| RFQError::ParsingError("Invalid Aqua0 amountOut".into()))?;
        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: params.amount_in.clone(),
            amount_out,
            quote_attributes: HashMap::from([("hook_data".into(), hook_data)]),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use super::*;

    #[tokio::test]
    async fn binding_quote_uses_one_keyless_post_without_fetching_state() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let (header_end, body_len) = loop {
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
                if let Some(end) = request
                    .windows(4)
                    .position(|b| b == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    assert!(headers.starts_with("post /quote "));
                    assert!(!headers.contains("x-api-key") && !headers.contains("x-operator-key"));
                    let len = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    if request.len() >= end + 4 + len {
                        break (end + 4, len);
                    }
                }
            };
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end..header_end + body_len]).unwrap();
            let response = serde_json::json!({
                "schemaVersion": "aqua0-rfq-quote-v1", "requestId": body["requestId"],
                "componentId": body["componentId"], "chainId": 8453,
                "tokenIn": body["tokenIn"], "tokenOut": body["tokenOut"],
                "amountIn": body["amountIn"], "amountOut": "200",
                "router": body["expectedRouter"], "executor": body["expectedRouter"],
                "hookData": "0xdeadbeef", "swapId": format!("0x{}", "22".repeat(32)),
                "nonce": "1", "deadline": "4102444800", "ranges": [],
            })
            .to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        });
        let client = Aqua0Client::new(
            Chain::Base,
            url,
            Aqua0Market {
                pool_id: format!("0x{}", "11".repeat(32)),
                class_id: "001".into(),
                amount0_samples: vec!["100".into()],
                amount1_samples: vec!["100".into()],
            },
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        let params = GetAmountOutParams {
            amount_in: 100u32.into(),
            token_in: Bytes::zero(20),
            token_out: Bytes::from(vec![1; 20]),
            sender: Bytes::from(vec![2; 20]),
            receiver: Bytes::from(vec![2; 20]),
        };
        let result = client
            .fetch_binding_quote(&params)
            .await;
        server.join().unwrap();
        assert_eq!(result.unwrap().amount_out, "200");
    }
}
