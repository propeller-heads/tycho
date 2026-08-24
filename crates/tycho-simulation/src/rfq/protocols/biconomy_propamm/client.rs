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
        protocols::biconomy_propamm::models::{
            parse_biguint, BiconomyChainLevelsResponse, BiconomyFirmQuoteResponse,
            BiconomyLevelsResponse,
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

/// Validates that Biconomy supports the chain and returns its numeric chain id.
///
/// Base and BSC mainnet. The Biconomy API also serves Base Sepolia (chain id 84532), but
/// tycho-common has no built-in testnet chain, so testnets are not wired up.
fn validate_chain(chain: Chain) -> Result<u64, RFQError> {
    match chain {
        Chain::Base | Chain::Bsc => Ok(chain.id()),
        _ => Err(RFQError::FatalError(format!("Unsupported chain for Biconomy: {chain:?}"))),
    }
}

/// Client for the Biconomy RFQ API.
///
/// Unlike Bebop's push-based WebSocket feed, the Biconomy API exposes a plain HTTP levels
/// endpoint, so `stream()` polls the chain-batch `/v1/levels?chainId=` once per interval (all
/// configured pairs in one request) and converts every poll into the same absolute
/// `StateSyncMessage` snapshot shape Bebop emits from its WS.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BiconomyClient {
    chain: Chain,
    chain_id: u64,
    levels_endpoint: String,
    firm_quote_endpoint: String,
    // Directed (token_in, token_out) pairs to poll levels for. Biconomy ladders are
    // one-directional; poll the reverse pair separately if both directions are needed.
    pairs: Vec<(Bytes, Bytes)>,
    poll_interval: Duration,
    quote_timeout: Duration,
    // Hosted-API key, sent as `x-api-key` on every request. Issued by the Biconomy team.
    // Never serialized: a dumped client (logs, snapshots) must not leak the credential; a
    // deserialized client re-reads it from configuration instead.
    #[serde(skip_serializing, default)]
    api_key: Option<String>,
}

impl BiconomyClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:biconomy_propamm";

    pub fn new(
        chain: Chain,
        pairs: Vec<(Bytes, Bytes)>,
        base_url: String,
        poll_interval: Duration,
        quote_timeout: Duration,
        api_key: Option<String>,
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
            api_key,
        })
    }

    // Every Biconomy endpoint requires the key; failing here beats an opaque 401. Kept out of
    // construction so deserialized clients (the key is never serialized) still build.
    fn apply_auth(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, RFQError> {
        match &self.api_key {
            Some(key) => Ok(request.header("x-api-key", key)),
            None => Err(RFQError::FatalError(
                "api_key is required for the Biconomy RFQ API; set it via \
                 BiconomyClientBuilder::api_key"
                    .to_string(),
            )),
        }
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

    /// Fetches every direction this chain quotes in one request (`/v1/levels?chainId=`).
    async fn fetch_chain_levels(&self) -> Result<BiconomyChainLevelsResponse, RFQError> {
        let response = self
            .apply_auth(
                self.http_client()
                    .get(&self.levels_endpoint),
            )?
            .query(&[("chainId", self.chain_id.to_string())])
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                RFQError::ConnectionError(format!("Failed to fetch Biconomy levels: {e}"))
            })?;

        if !response.status().is_success() {
            return Err(RFQError::ConnectionError(format!(
                "Biconomy levels HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        response.json().await.map_err(|e| {
            RFQError::ParsingError(format!("Failed to parse Biconomy levels response: {e}"))
        })
    }

    pub fn create_component_with_state(
        &self,
        component_id: String,
        levels: &BiconomyLevelsResponse,
    ) -> ComponentWithState {
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "biconomy_propamm_pool".to_string(),
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
            // Biconomy levels carry no USD normalization data, so no TVL is reported.
            component_tvl: None,
            entrypoints: vec![],
        }
    }

    fn process_firm_quote_response(
        quote: BiconomyFirmQuoteResponse,
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

        // Provider-specific opaque attributes. IMPORTANT Biconomy rule: `valid_until` is a hard
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
impl RFQClient for BiconomyClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_components: HashMap<String, ComponentWithState> = HashMap::new();
            let mut ticker = interval(client.poll_interval);

            info!(
                "Starting Biconomy polling every {} ms for {} pairs",
                client.poll_interval.as_millis(),
                client.pairs.len()
            );
            loop {
                ticker.tick().await;

                let mut new_components = HashMap::new();
                // Snapshot timestamp mirrors the API's own `asOf`. With several pairs per poll
                // the freshest one is used; if every fetch fails the wall clock is the fallback.
                let mut latest_as_of: Option<u64> = None;

                // One batch poll covers every configured pair. A failed poll keeps the
                // previous snapshot instead of emitting an empty book: fetch failure and
                // empty liquidity stay distinguishable.
                let chain_levels = match client.fetch_chain_levels().await {
                    Ok(resp) => resp,
                    Err(e) => {
                        warn!("Failed to fetch Biconomy levels for chain {}: {e}", client.chain_id);
                        continue;
                    }
                };
                if chain_levels.chain_id != client.chain_id {
                    warn!(
                        "Biconomy levels chain id mismatch: expected {}, got {}. Skipping poll.",
                        client.chain_id, chain_levels.chain_id
                    );
                    continue;
                }
                latest_as_of = Some(latest_as_of.unwrap_or(0).max(chain_levels.as_of));

                for (token_in, token_out) in &client.pairs {
                    // Pairs missing from the response or without maker liquidity are dropped,
                    // so they show up in removed_components just like Bebop's TVL-filtered
                    // empty books.
                    let Some(entry) = chain_levels
                        .pairs
                        .iter()
                        .find(|p| &p.token_in == token_in && &p.token_out == token_out)
                    else {
                        continue;
                    };
                    if entry
                        .makers
                        .iter()
                        .all(|maker| maker.levels.is_empty())
                    {
                        continue;
                    }

                    let levels = BiconomyLevelsResponse {
                        chain_id: chain_levels.chain_id,
                        token_in: entry.token_in.clone(),
                        token_out: entry.token_out.clone(),
                        merged: entry.merged.clone(),
                        makers: entry.makers.clone(),
                        as_of: chain_levels.as_of,
                    };
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

                yield Ok(("biconomy_propamm".to_string(), StateSyncMessage {
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
    /// IMPORTANT Biconomy rule: the returned quote is single-use and expires hard at
    /// `valid_until`. Consumers must refetch a firm quote immediately before broadcasting the
    /// settlement transaction and must never replay a stale response.
    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let request = self
            .apply_auth(
                self.http_client()
                    .get(&self.firm_quote_endpoint),
            )?
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
                    "Biconomy firm quote request timed out after {} seconds",
                    self.quote_timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                RFQError::ConnectionError(format!(
                    "Failed to send Biconomy firm quote request: {e}"
                ))
            })?;

        if !response.status().is_success() {
            return Err(RFQError::QuoteNotFound(format!(
                "Biconomy firm quote HTTP error {}: {}",
                response.status(),
                response
                    .text()
                    .await
                    .unwrap_or_default()
            )));
        }

        let quote = response
            .json::<BiconomyFirmQuoteResponse>()
            .await
            .map_err(|e| {
                RFQError::ParsingError(format!("Failed to parse Biconomy firm quote response: {e}"))
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
    use crate::rfq::protocols::biconomy_propamm::models::BiconomyCall;

    fn weth() -> Bytes {
        Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap()
    }

    fn usdc() -> Bytes {
        Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap()
    }

    fn router() -> Bytes {
        Bytes::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap()
    }

    fn test_client(base_url: &str) -> BiconomyClient {
        BiconomyClient::new(
            Chain::Base,
            vec![(weth(), usdc())],
            base_url.to_string(),
            Duration::from_millis(50),
            Duration::from_secs(5),
            Some("test-api-key".to_string()),
        )
        .unwrap()
    }

    fn levels_fixture() -> BiconomyLevelsResponse {
        let json = std::fs::read_to_string(
            "src/rfq/protocols/biconomy_propamm/test_responses/levels.json",
        )
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
        let result = BiconomyClient::new(
            Chain::Ethereum,
            vec![],
            "http://localhost:8080".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(5),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_create_component_with_state() {
        let client = test_client("http://localhost:8080");
        let levels = levels_fixture();
        let component_id = BiconomyClient::component_id(&weth(), &usdc());

        let component = client.create_component_with_state(component_id.clone(), &levels);

        assert_eq!(component.component.id, component_id);
        assert_eq!(component.component.protocol_system, "rfq:biconomy_propamm");
        assert_eq!(component.component.protocol_type_name, "biconomy_propamm_pool");
        assert_eq!(component.component.chain, Chain::Base);
        assert_eq!(component.component.tokens, vec![weth(), usdc()]);
        assert!(component
            .component
            .contract_addresses
            .is_empty());
        assert_eq!(component.component_tvl, None);

        let attrs = &component.state.attributes;
        assert_eq!(String::from_utf8(attrs["as_of"].to_vec()).unwrap(), "1784889534");
        let makers: Vec<crate::rfq::protocols::biconomy_propamm::models::BiconomyMakerLevels> =
            serde_json::from_slice(&attrs["makers"]).unwrap();
        assert_eq!(makers, levels.makers);
        let merged: Vec<crate::rfq::protocols::biconomy_propamm::models::BiconomyMergedLevel> =
            serde_json::from_slice(&attrs["merged"]).unwrap();
        assert_eq!(merged, levels.merged);
    }

    #[test]
    fn test_component_id_is_deterministic() {
        let id_a = BiconomyClient::component_id(&weth(), &usdc());
        let id_b = BiconomyClient::component_id(&weth(), &usdc());
        let id_reverse = BiconomyClient::component_id(&usdc(), &weth());
        assert_eq!(id_a, id_b);
        // Directions are separate components
        assert_ne!(id_a, id_reverse);
    }

    #[test]
    fn test_process_firm_quote_response() {
        let json = std::fs::read_to_string(
            "src/rfq/protocols/biconomy_propamm/test_responses/firm_quote.json",
        )
        .unwrap();
        let quote: BiconomyFirmQuoteResponse = serde_json::from_str(&json).unwrap();
        let params = quote_params();

        let signed =
            BiconomyClient::process_firm_quote_response(quote.clone(), &params, 8453).unwrap();

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

        let calls: Vec<BiconomyCall> =
            serde_json::from_slice(&signed.quote_attributes["calls"]).unwrap();
        assert_eq!(calls, quote.calls);
    }

    #[test]
    fn test_process_firm_quote_response_rejects_wrong_receiver() {
        let json = std::fs::read_to_string(
            "src/rfq/protocols/biconomy_propamm/test_responses/firm_quote.json",
        )
        .unwrap();
        let quote: BiconomyFirmQuoteResponse = serde_json::from_str(&json).unwrap();
        let mut params = quote_params();
        params.receiver = Bytes::from_str("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();

        let result = BiconomyClient::process_firm_quote_response(quote, &params, 8453);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_polling_stream_emits_snapshots() {
        let addr = create_json_server(
            "src/rfq/protocols/biconomy_propamm/test_responses/chain_levels.json",
        )
        .await;
        let client = test_client(&format!("http://127.0.0.1:{}", addr.port()));

        let mut stream = client.stream();

        // First poll: the pair appears as a new component with the fixture's asOf timestamp.
        let (provider, msg) = stream.next().await.unwrap().unwrap();
        assert_eq!(provider, "biconomy_propamm");
        assert_eq!(msg.header.timestamp, 1784889534);
        assert_eq!(msg.snapshots.states.len(), 1);
        assert!(msg.removed_components.is_empty());

        let component_id = BiconomyClient::component_id(&weth(), &usdc());
        let component = &msg.snapshots.states[&component_id];
        assert_eq!(component.component.protocol_system, "rfq:biconomy_propamm");
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
            create_json_server("src/rfq/protocols/biconomy_propamm/test_responses/firm_quote.json")
                .await;
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
        let deserialized: BiconomyClient = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.chain, original.chain);
        assert_eq!(deserialized.chain_id, original.chain_id);
        assert_eq!(deserialized.levels_endpoint, original.levels_endpoint);
        assert_eq!(deserialized.firm_quote_endpoint, original.firm_quote_endpoint);
        assert_eq!(deserialized.pairs, original.pairs);
        assert_eq!(deserialized.poll_interval, original.poll_interval);
        assert_eq!(deserialized.quote_timeout, original.quote_timeout);
    }

    /// Live integration against the production API. Ignored by default; run with:
    ///   BICONOMY_PROPAMM_API_KEY=<key> cargo test -p tycho-simulation \
    ///     biconomy_propamm::client::tests::live_levels_and_firm_quote -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_levels_and_firm_quote() {
        let Ok(api_key) = std::env::var("BICONOMY_PROPAMM_API_KEY") else {
            eprintln!("BICONOMY_PROPAMM_API_KEY not set - skipping live test");
            return;
        };
        let client = BiconomyClient::new(
            Chain::Base,
            vec![(weth(), usdc())],
            crate::rfq::constants::DEFAULT_BICONOMY_PROPAMM_API_URL.to_string(),
            Duration::from_millis(1000),
            Duration::from_secs(10),
            Some(api_key),
        )
        .expect("client");

        let chain_levels = client
            .fetch_chain_levels()
            .await
            .expect("levels request failed");
        let levels = chain_levels
            .pairs
            .iter()
            .find(|p| p.token_in == weth() && p.token_out == usdc())
            .expect("WETH/USDC missing from the chain book");
        assert!(!levels.makers.is_empty(), "no live makers for WETH/USDC");
        eprintln!(
            "levels OK: {} makers, first ladder {} levels",
            levels.makers.len(),
            levels.makers[0].levels.len()
        );

        let params = GetAmountOutParams {
            amount_in: num_bigint::BigUint::from(100_000_000_000_000_000u128), // 0.1 WETH
            token_in: weth(),
            token_out: usdc(),
            sender: router(),
            receiver: router(),
        };
        let quote = client
            .request_binding_quote(&params)
            .await
            .expect("firm quote failed");
        assert!(quote
            .quote_attributes
            .contains_key("calls"));
        eprintln!("firm quote OK: attributes {:?}", quote.quote_attributes.keys());
    }
}
