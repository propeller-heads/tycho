use std::{collections::HashMap, sync::LazyLock, time::SystemTime};

use alloy::primitives::{utils::keccak256, Address};
use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::{interval, timeout, Duration};
use tracing::{info, warn};
use tycho_common::{
    models::{
        protocol::{GetAmountOutParams, ProtocolComponent, ProtocolComponentState},
        Chain,
    },
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::{
    rfq::{
        client::RFQClient,
        errors::RFQError,
        models::TimestampHeader,
        protocols::propamm::models::{
            parse_biguint, PropAmmFirmQuoteResponse, PropAmmLevelsResponse,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
};

static PROPAMM_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

fn bytes_to_address_string(address: &Bytes) -> Result<String, RFQError> {
    if address.len() != 20 {
        return Err(RFQError::InvalidInput(format!("Invalid EVM address length: {address}")));
    }
    Ok(Address::from_slice(address).to_checksum(None))
}

/// Validates that PropAMM supports the chain and returns its numeric chain id.
///
/// The PropAMM API also serves Base Sepolia (chain id 84532), but tycho-common has no built-in
/// testnet chain, so this integration is mainnet Base only.
fn validate_chain(chain: Chain) -> Result<u64, RFQError> {
    match chain {
        Chain::Base => Ok(chain.id()),
        _ => Err(RFQError::FatalError(format!("Unsupported chain for PropAMM: {chain:?}"))),
    }
}

/// Client for the PropAMM RFQ API.
///
/// Unlike Bebop's push-based WebSocket feed, PropAMM exposes a plain HTTP levels endpoint, so
/// `stream()` polls `/v1/levels` for each configured pair at a fixed interval and converts every
/// poll into the same absolute `StateSyncMessage` snapshot shape Bebop emits from its WS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropAmmClient {
    chain: Chain,
    chain_id: u64,
    levels_endpoint: String,
    firm_quote_endpoint: String,
    // Directed (token_in, token_out) pairs to poll levels for. PropAMM ladders are
    // one-directional; poll the reverse pair separately if both directions are needed.
    pairs: Vec<(Bytes, Bytes)>,
    poll_interval: Duration,
    quote_timeout: Duration,
}

impl PropAmmClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:propamm";

    pub fn new(
        chain: Chain,
        pairs: Vec<(Bytes, Bytes)>,
        base_url: String,
        poll_interval: Duration,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        let chain_id = validate_chain(chain)?;
        let base_url = base_url.trim_end_matches('/');
        Ok(Self {
            chain,
            chain_id,
            levels_endpoint: format!("{base_url}/v1/levels"),
            firm_quote_endpoint: format!("{base_url}/v1/firm-quote"),
            pairs,
            poll_interval,
            quote_timeout,
        })
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn http_client(&self) -> &Client {
        &PROPAMM_HTTP_CLIENT
    }

    /// Deterministic component id for a directed pair, following the Bebop id convention.
    pub fn component_id(token_in: &Bytes, token_out: &Bytes) -> String {
        let pair_str = format!("propamm_{}/{}", hex::encode(token_in), hex::encode(token_out));
        format!("{}", keccak256(pair_str.as_bytes()))
    }

    async fn fetch_levels(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<PropAmmLevelsResponse, RFQError> {
        let response = self
            .http_client()
            .get(&self.levels_endpoint)
            .query(&[
                ("chainId", self.chain_id.to_string()),
                ("tokenIn", bytes_to_address_string(token_in)?),
                ("tokenOut", bytes_to_address_string(token_out)?),
            ])
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                RFQError::ConnectionError(format!("Failed to fetch PropAMM levels: {e}"))
            })?;

        if !response.status().is_success() {
            return Err(RFQError::ConnectionError(format!(
                "PropAMM levels HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse PropAMM levels response: {e}"))
        })
    }

    pub fn create_component_with_state(
        &self,
        component_id: String,
        levels: &PropAmmLevelsResponse,
    ) -> ComponentWithState {
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "propamm_pool".to_string(),
            chain: self.chain,
            tokens: vec![levels.token_in.clone(), levels.token_out.clone()],
            contract_addresses: vec![], // empty for RFQ
            static_attributes: Default::default(),
            change: Default::default(),
            creation_tx: Default::default(),
            created_at: Default::default(),
        };

        // Store the ladders as raw JSON strings, since attributes cannot hold arrays. Both views
        // are kept: `makers` is the source of truth for the sweep math, `merged` is the
        // pre-merged display ladder.
        let mut attributes = HashMap::new();
        attributes.insert(
            "makers".to_string(),
            serde_json::to_vec(&levels.makers)
                .unwrap_or_default()
                .into(),
        );
        attributes.insert(
            "merged".to_string(),
            serde_json::to_vec(&levels.merged)
                .unwrap_or_default()
                .into(),
        );
        attributes.insert(
            "as_of".to_string(),
            levels
                .as_of
                .to_string()
                .into_bytes()
                .into(),
        );

        ComponentWithState {
            state: ProtocolComponentState::new(&component_id, attributes, HashMap::new()),
            component: protocol_component,
            // PropAMM levels carry no USD normalization data, so no TVL is reported.
            component_tvl: None,
            entrypoints: vec![],
        }
    }

    fn process_firm_quote_response(
        quote: PropAmmFirmQuoteResponse,
        params: &GetAmountOutParams,
        expected_chain_id: u64,
    ) -> Result<SignedQuote, RFQError> {
        quote.validate(params, expected_chain_id)?;

        let amount_in = parse_biguint(&quote.amount_in, "amountIn")?;
        let amount_out = parse_biguint(&quote.amount_out, "amountOut")?;
        let gas_estimate: u64 = quote
            .gas_estimate
            .parse()
            .map_err(|_| {
                RFQError::ParsingError(format!(
                    "Failed to parse gasEstimate: {}",
                    quote.gas_estimate
                ))
            })?;

        // Provider-specific opaque attributes. IMPORTANT PropAMM rule: `valid_until` is a hard
        // on-chain deadline. Consumers must refetch a firm quote immediately before broadcast
        // and never replay a stale response.
        let mut quote_attributes: HashMap<String, Bytes> = HashMap::new();
        quote_attributes.insert("quote_id".into(), quote.quote_id.clone());
        quote_attributes.insert(
            "calls".into(),
            serde_json::to_vec(&quote.calls)
                .map_err(|e| {
                    RFQError::ParsingError(format!("Failed to serialize firm quote calls: {e}"))
                })?
                .into(),
        );
        quote_attributes
            .insert("valid_until".into(), Bytes::from(quote.valid_until.to_be_bytes().to_vec()));
        quote_attributes
            .insert("gas_estimate".into(), Bytes::from(gas_estimate.to_be_bytes().to_vec()));

        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in,
            amount_out,
            quote_attributes,
        })
    }
}

#[async_trait]
impl RFQClient for PropAmmClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_components: HashMap<String, ComponentWithState> = HashMap::new();
            let mut ticker = interval(client.poll_interval);

            info!(
                "Starting PropAMM polling every {} ms for {} pairs",
                client.poll_interval.as_millis(),
                client.pairs.len()
            );
            loop {
                ticker.tick().await;

                let mut new_components = HashMap::new();
                // Snapshot timestamp mirrors the API's own `asOf`. With several pairs per poll
                // the freshest one is used; if every fetch fails the wall clock is the fallback.
                let mut latest_as_of: Option<u64> = None;

                for (token_in, token_out) in &client.pairs {
                    let levels = match client.fetch_levels(token_in, token_out).await {
                        Ok(levels) => levels,
                        Err(e) => {
                            warn!(
                                "Failed to fetch PropAMM levels for {token_in} -> {token_out}: {e}"
                            );
                            continue;
                        }
                    };

                    if levels.chain_id != client.chain_id {
                        warn!(
                            "PropAMM levels chain id mismatch: expected {}, got {}. Skipping.",
                            client.chain_id, levels.chain_id
                        );
                        continue;
                    }
                    if &levels.token_in != token_in || &levels.token_out != token_out {
                        warn!(
                            "PropAMM levels pair mismatch: requested {token_in} -> {token_out}, \
                             got {} -> {}. Skipping.",
                            levels.token_in, levels.token_out
                        );
                        continue;
                    }
                    // Pairs without any maker liquidity are dropped, so they show up in
                    // removed_components just like Bebop's TVL-filtered empty books.
                    if levels
                        .makers
                        .iter()
                        .all(|maker| maker.levels.is_empty())
                    {
                        continue;
                    }

                    latest_as_of = Some(latest_as_of.unwrap_or(0).max(levels.as_of));
                    let component_id = Self::component_id(token_in, token_out);
                    let component_with_state =
                        client.create_component_with_state(component_id.clone(), &levels);
                    new_components.insert(component_id, component_with_state);
                }

                let removed_components: HashMap<String, ProtocolComponent> = current_components
                    .iter()
                    .filter(|(id, _)| !new_components.contains_key(*id))
                    .map(|(id, component)| (id.clone(), component.component.clone()))
                    .collect();

                current_components = new_components.clone();

                let timestamp = match latest_as_of {
                    Some(as_of) => as_of,
                    None => SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_err(|_| {
                            RFQError::ParsingError("SystemTime before UNIX EPOCH".to_string())
                        })?
                        .as_secs(),
                };

                yield Ok(("propamm".to_string(), StateSyncMessage {
                    header: TimestampHeader { timestamp },
                    snapshots: Snapshot { states: new_components, vm_storage: HashMap::new() },
                    deltas: None, // Deltas are always None - all the changes are absolute
                    removed_components,
                }));
            }
        })
    }

    /// Fetches a binding firm quote from `GET /v1/firm-quote`.
    ///
    /// IMPORTANT PropAMM rule: the returned quote is single-use and expires hard at
    /// `valid_until`. Consumers must refetch a firm quote immediately before broadcasting the
    /// settlement transaction and must never replay a stale response.
    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let request = self
            .http_client()
            .get(&self.firm_quote_endpoint)
            .query(&[
                ("chainId", self.chain_id.to_string()),
                ("tokenIn", bytes_to_address_string(&params.token_in)?),
                ("tokenOut", bytes_to_address_string(&params.token_out)?),
                ("amountIn", params.amount_in.to_string()),
                ("receiver", bytes_to_address_string(&params.receiver)?),
            ])
            .header("accept", "application/json");

        let response = timeout(self.quote_timeout, request.send())
            .await
            .map_err(|_| {
                RFQError::ConnectionError(format!(
                    "PropAMM firm quote request timed out after {} seconds",
                    self.quote_timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                RFQError::ConnectionError(format!("Failed to send PropAMM firm quote request: {e}"))
            })?;

        if !response.status().is_success() {
            return Err(RFQError::QuoteNotFound(format!(
                "PropAMM firm quote HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        let quote = response
            .json::<PropAmmFirmQuoteResponse>()
            .await
            .map_err(|e| {
                RFQError::ParsingError(format!("Failed to parse PropAMM firm quote response: {e}"))
            })?;

        Self::process_firm_quote_response(quote, params, self.chain_id)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use futures::StreamExt;
    use num_bigint::BigUint;
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    use super::*;
    use crate::rfq::protocols::propamm::models::PropAmmCall;

    fn weth() -> Bytes {
        Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap()
    }

    fn usdc() -> Bytes {
        Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap()
    }

    fn router() -> Bytes {
        Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap()
    }

    fn test_client(base_url: &str) -> PropAmmClient {
        PropAmmClient::new(
            Chain::Base,
            vec![(weth(), usdc())],
            base_url.to_string(),
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn levels_fixture() -> PropAmmLevelsResponse {
        let json = std::fs::read_to_string("src/rfq/protocols/propamm/test_responses/levels.json")
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn quote_params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from_str("15000000000000000000").unwrap(),
            token_in: weth(),
            token_out: usdc(),
            sender: router(),
            receiver: router(),
        }
    }

    /// Serves the given JSON fixture for every incoming HTTP request.
    async fn create_json_server(fixture_path: &str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let json_response = std::fs::read_to_string(fixture_path).unwrap();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let json_response_clone = json_response.clone();
                tokio::spawn(async move {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        json_response_clone.len(),
                        json_response_clone
                    );
                    let _ = stream
                        .write_all(response.as_bytes())
                        .await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        addr
    }

    #[test]
    fn test_unsupported_chain() {
        let result = PropAmmClient::new(
            Chain::Ethereum,
            vec![],
            "http://localhost:8080".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_create_component_with_state() {
        let client = test_client("http://localhost:8080");
        let levels = levels_fixture();
        let component_id = PropAmmClient::component_id(&weth(), &usdc());

        let component = client.create_component_with_state(component_id.clone(), &levels);

        assert_eq!(component.component.id, component_id);
        assert_eq!(component.component.protocol_system, "rfq:propamm");
        assert_eq!(component.component.protocol_type_name, "propamm_pool");
        assert_eq!(component.component.chain, Chain::Base);
        assert_eq!(component.component.tokens, vec![weth(), usdc()]);
        assert!(component
            .component
            .contract_addresses
            .is_empty());
        assert_eq!(component.component_tvl, None);

        let attrs = &component.state.attributes;
        assert_eq!(String::from_utf8(attrs["as_of"].to_vec()).unwrap(), "1784889534");
        let makers: Vec<crate::rfq::protocols::propamm::models::PropAmmMakerLevels> =
            serde_json::from_slice(&attrs["makers"]).unwrap();
        assert_eq!(makers, levels.makers);
        let merged: Vec<crate::rfq::protocols::propamm::models::PropAmmMergedLevel> =
            serde_json::from_slice(&attrs["merged"]).unwrap();
        assert_eq!(merged, levels.merged);
    }

    #[test]
    fn test_component_id_is_deterministic() {
        let id_a = PropAmmClient::component_id(&weth(), &usdc());
        let id_b = PropAmmClient::component_id(&weth(), &usdc());
        let id_reverse = PropAmmClient::component_id(&usdc(), &weth());
        assert_eq!(id_a, id_b);
        // Directions are separate components
        assert_ne!(id_a, id_reverse);
    }

    #[test]
    fn test_process_firm_quote_response() {
        let json =
            std::fs::read_to_string("src/rfq/protocols/propamm/test_responses/firm_quote.json")
                .unwrap();
        let quote: PropAmmFirmQuoteResponse = serde_json::from_str(&json).unwrap();
        let params = quote_params();

        let signed =
            PropAmmClient::process_firm_quote_response(quote.clone(), &params, 8453).unwrap();

        assert_eq!(signed.base_token, params.token_in);
        assert_eq!(signed.quote_token, params.token_out);
        assert_eq!(signed.amount_in, BigUint::from_str("15000000000000000000").unwrap());
        assert_eq!(signed.amount_out, BigUint::from_str("28164999999").unwrap());

        assert_eq!(signed.quote_attributes["quote_id"], quote.quote_id);

        let mut valid_until = [0u8; 8];
        valid_until.copy_from_slice(signed.quote_attributes["valid_until"].as_ref());
        assert_eq!(u64::from_be_bytes(valid_until), 1751536030);

        let mut gas_estimate = [0u8; 8];
        gas_estimate.copy_from_slice(signed.quote_attributes["gas_estimate"].as_ref());
        assert_eq!(u64::from_be_bytes(gas_estimate), 265000);

        let calls: Vec<PropAmmCall> =
            serde_json::from_slice(&signed.quote_attributes["calls"]).unwrap();
        assert_eq!(calls, quote.calls);
    }

    #[test]
    fn test_process_firm_quote_response_rejects_wrong_receiver() {
        let json =
            std::fs::read_to_string("src/rfq/protocols/propamm/test_responses/firm_quote.json")
                .unwrap();
        let quote: PropAmmFirmQuoteResponse = serde_json::from_str(&json).unwrap();
        let mut params = quote_params();
        params.receiver = Bytes::from_str("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();

        let result = PropAmmClient::process_firm_quote_response(quote, &params, 8453);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_polling_stream_emits_snapshots() {
        let addr = create_json_server("src/rfq/protocols/propamm/test_responses/levels.json").await;
        let client = test_client(&format!("http://127.0.0.1:{}", addr.port()));

        let mut stream = client.stream();

        // First poll: the pair appears as a new component with the fixture's asOf timestamp.
        let (provider, msg) = stream.next().await.unwrap().unwrap();
        assert_eq!(provider, "propamm");
        assert_eq!(msg.header.timestamp, 1784889534);
        assert_eq!(msg.snapshots.states.len(), 1);
        assert!(msg.removed_components.is_empty());

        let component_id = PropAmmClient::component_id(&weth(), &usdc());
        let component = &msg.snapshots.states[&component_id];
        assert_eq!(component.component.protocol_system, "rfq:propamm");
        assert!(component
            .state
            .attributes
            .contains_key("makers"));

        // Second poll: absolute snapshot again, nothing removed.
        let (_, msg) = stream.next().await.unwrap().unwrap();
        assert_eq!(msg.snapshots.states.len(), 1);
        assert!(msg.removed_components.is_empty());
    }

    #[tokio::test]
    async fn test_request_binding_quote_over_http() {
        let addr =
            create_json_server("src/rfq/protocols/propamm/test_responses/firm_quote.json").await;
        let client = test_client(&format!("http://127.0.0.1:{}", addr.port()));

        let signed = client
            .request_binding_quote(&quote_params())
            .await
            .unwrap();

        assert_eq!(signed.amount_in, BigUint::from_str("15000000000000000000").unwrap());
        assert_eq!(signed.amount_out, BigUint::from_str("28164999999").unwrap());
        assert!(signed
            .quote_attributes
            .contains_key("calls"));
    }

    #[test]
    fn test_client_serialize_deserialize_roundtrip() {
        let original = test_client("http://localhost:8080");

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: PropAmmClient = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.chain, original.chain);
        assert_eq!(deserialized.chain_id, original.chain_id);
        assert_eq!(deserialized.levels_endpoint, original.levels_endpoint);
        assert_eq!(deserialized.firm_quote_endpoint, original.firm_quote_endpoint);
        assert_eq!(deserialized.pairs, original.pairs);
        assert_eq!(deserialized.poll_interval, original.poll_interval);
        assert_eq!(deserialized.quote_timeout, original.quote_timeout);
    }
}
