use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    time::SystemTime,
};

use alloy::primitives::keccak256;
use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use http::Request;
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{handshake::client::generate_key, Message},
};
use tracing::{error, info, warn};
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::{
    rfq::{
        client::RFQClient,
        errors::RFQError,
        models::TimestampHeader,
        protocols::euclid::models::{
            EuclidFirmResponse, EuclidLevelsFrame, EuclidPairLevels, EuclidPriceData,
        },
    },
    tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState},
};

/// Euclid Protocol RFQ client.
///
/// Euclid's liquidity lives on its own app chain (VSL); the RFQ gateway quotes
/// that depth and settles fills atomically on the taker's chain from
/// pre-positioned inventory. Indicative levels stream over a WebSocket as
/// full-state JSON frames; binding quotes come from an HTTP endpoint returning
/// ready-to-execute settlement calldata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EuclidClient {
    chain: Chain,
    price_ws: String,
    quote_endpoint: String,
    // Tokens that we want prices for
    tokens: HashSet<Bytes>,
    // Min tvl value in the quote token.
    tvl: f64,
    // x-api-key for the firm-quote endpoint; levels are public.
    #[serde(skip_serializing, default)]
    api_key: Option<String>,
    quote_timeout: Duration,
}

fn chain_query(chain: Chain) -> Result<u64, RFQError> {
    match chain {
        Chain::Ethereum => Ok(chain.id()),
        _ => Err(RFQError::FatalError(format!("Unsupported chain: {chain:?}"))),
    }
}

impl EuclidClient {
    pub const PROTOCOL_SYSTEM: &'static str = "rfq:euclid";

    /// Creates a fully configured client. Prefer constructing through
    /// [`EuclidClientBuilder`](super::client_builder::EuclidClientBuilder).
    pub fn new(
        chain: Chain,
        base_url: String,
        tokens: HashSet<Bytes>,
        tvl: f64,
        api_key: Option<String>,
        quote_timeout: Duration,
    ) -> Result<Self, RFQError> {
        let chain_id = chain_query(chain)?;
        let base = base_url.trim_end_matches('/');
        let ws_base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        Ok(Self {
            price_ws: format!("{ws_base}/levels?chainId={chain_id}"),
            quote_endpoint: format!("{base}/firm"),
            tokens,
            chain,
            tvl,
            api_key,
            quote_timeout,
        })
    }

    fn create_component_with_state(
        &self,
        component_id: String,
        tokens: Vec<Bytes>,
        price_data: &EuclidPriceData,
        tvl: f64,
    ) -> ComponentWithState {
        let protocol_component = ProtocolComponent {
            id: component_id.clone(),
            protocol_system: Self::PROTOCOL_SYSTEM.to_string(),
            protocol_type_name: "euclid_pool".to_string(),
            chain: self.chain,
            tokens,
            contract_addresses: vec![], // empty for RFQ
            static_attributes: Default::default(),
            change: Default::default(),
            creation_tx: Default::default(),
            created_at: Default::default(),
        };

        // Store bids and asks as JSON pair arrays, matching the shape the
        // decoder rebuilds price data from.
        let mut attributes = HashMap::new();
        if !price_data.bids.is_empty() {
            let bids_json = serde_json::to_string(&price_data.bids).unwrap_or_default();
            attributes.insert("bids".to_string(), bids_json.as_bytes().to_vec().into());
        }
        if !price_data.asks.is_empty() {
            let asks_json = serde_json::to_string(&price_data.asks).unwrap_or_default();
            attributes.insert("asks".to_string(), asks_json.as_bytes().to_vec().into());
        }

        ComponentWithState {
            state: ProtocolComponentState::new(&component_id, attributes, HashMap::new()),
            component: protocol_component,
            component_tvl: Some(tvl),
            entrypoints: vec![],
        }
    }

    fn parse_pair(pair: &EuclidPairLevels) -> Result<(Bytes, Bytes, EuclidPriceData), RFQError> {
        let parse_addr = |value: &str| -> Result<Bytes, RFQError> {
            let stripped = value
                .strip_prefix("0x")
                .unwrap_or(value);
            let decoded = hex::decode(stripped)
                .map_err(|_| RFQError::ParsingError(format!("Invalid token address: {value}")))?;
            if decoded.len() != 20 {
                return Err(RFQError::ParsingError(format!("Invalid token address: {value}")));
            }
            Ok(Bytes::from(decoded))
        };
        let base = parse_addr(&pair.base_address)?;
        let quote = parse_addr(&pair.quote_address)?;
        let price_data = EuclidPriceData::from_pair(pair, base.to_vec(), quote.to_vec())?;
        Ok((base, quote, price_data))
    }

    fn process_quote_response(
        response: EuclidFirmResponse,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        match response {
            EuclidFirmResponse::Success(quote) => {
                quote.validate(params)?;

                let mut quote_attributes: HashMap<String, Bytes> = HashMap::new();
                quote_attributes.insert("tx_to".into(), quote.tx_to_bytes()?);
                quote_attributes.insert("calldata".into(), quote.calldata_bytes()?);
                quote_attributes.insert(
                    "partial_fill_offset".into(),
                    Bytes::from(
                        quote
                            .partial_fill_offset
                            .to_be_bytes()
                            .to_vec(),
                    ),
                );

                Ok(SignedQuote {
                    base_token: params.token_in.clone(),
                    quote_token: params.token_out.clone(),
                    amount_in: BigUint::from_str(&quote.amount_in).map_err(|_| {
                        RFQError::ParsingError(format!(
                            "Failed to parse amount_in: {}",
                            quote.amount_in
                        ))
                    })?,
                    amount_out: BigUint::from_str(&quote.amount_out).map_err(|_| {
                        RFQError::ParsingError(format!(
                            "Failed to parse amount_out: {}",
                            quote.amount_out
                        ))
                    })?,
                    quote_attributes,
                })
            }
            EuclidFirmResponse::Error(err) => {
                Err(RFQError::FatalError(format!("Euclid API error: {}", err.error)))
            }
        }
    }
}

#[async_trait]
impl RFQClient for EuclidClient {
    fn stream(
        &self,
    ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>> {
        let tokens = self.tokens.clone();
        let url = self.price_ws.clone();
        let tvl_threshold = self.tvl;
        let client = self.clone();

        Box::pin(async_stream::stream! {
            let mut current_components: HashMap<String, ComponentWithState> = HashMap::new();
            let mut consecutive_failures = 0;
            const MAX_CONSECUTIVE_FAILURES: u32 = 10;

            loop {
                let host = url
                    .split('/')
                    .nth(2)
                    .unwrap_or_default()
                    .to_string();
                let request = Request::builder()
                    .method("GET")
                    .uri(&url)
                    .header("Host", host)
                    .header("Upgrade", "websocket")
                    .header("Connection", "Upgrade")
                    .header("Sec-WebSocket-Key", generate_key())
                    .header("Sec-WebSocket-Version", "13")
                    .body(())
                    .map_err(|_| RFQError::FatalError("Failed to build request".into()))?;

                let (ws_stream, _) = match connect_async_with_config(request, None, false).await {
                    Ok(connection) => {
                        info!("Successfully connected to Euclid WebSocket");
                        connection
                    },
                    Err(e) => {
                        consecutive_failures += 1;
                        error!("Failed to connect to Euclid WebSocket (consecutive failure {}): {}", consecutive_failures, e);

                        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                            yield Err(RFQError::ConnectionError(format!("Failed to connect after {MAX_CONSECUTIVE_FAILURES} consecutive failures: {e}")));
                            return;
                        }

                        let backoff_duration = Duration::from_secs(2_u64.pow(consecutive_failures.min(5)));
                        info!("Retrying connection in {} seconds...", backoff_duration.as_secs());
                        sleep(backoff_duration).await;
                        continue;
                    }
                };

                let (_, mut ws_receiver) = ws_stream.split();

                while let Some(msg) = ws_receiver.next().await {
                    match msg {
                        Ok(Message::Text(data)) => {
                            match serde_json::from_str::<EuclidLevelsFrame>(&data) {
                                Ok(frame) => {
                                    // A completed handshake says nothing about whether the
                                    // connection works, so only pricing data clears the counter.
                                    consecutive_failures = 0;

                                    let mut new_components = HashMap::new();
                                    for pair in &frame.pairs {
                                        let (base, quote, price_data) = match Self::parse_pair(pair) {
                                            Ok(parsed) => parsed,
                                            Err(e) => {
                                                warn!("Skipping malformed Euclid pair: {e}");
                                                continue;
                                            }
                                        };
                                        if !tokens.contains(&base) || !tokens.contains(&quote) {
                                            continue;
                                        }
                                        // Levels are published stable-quoted, so the bid-side
                                        // quote sum is a direct TVL measure.
                                        let tvl = price_data.quote_tvl();
                                        if tvl < tvl_threshold {
                                            continue;
                                        }
                                        let pair_str = format!("euclid_{}/{}", hex::encode(&base), hex::encode(&quote));
                                        let component_id = format!("{}", keccak256(pair_str.as_bytes()));
                                        let component_with_state = client.create_component_with_state(
                                            component_id.clone(),
                                            vec![base, quote],
                                            &price_data,
                                            tvl,
                                        );
                                        new_components.insert(component_id, component_with_state);
                                    }

                                    // Frames are full-state: anything not present was removed.
                                    let removed_components: HashMap<String, ProtocolComponent> = current_components
                                        .iter()
                                        .filter(|&(id, _)| !new_components.contains_key(id))
                                        .map(|(k, v)| (k.clone(), v.component.clone()))
                                        .collect();

                                    current_components = new_components.clone();

                                    let snapshot = Snapshot {
                                        states: new_components,
                                        vm_storage: HashMap::new(),
                                    };
                                    let timestamp = SystemTime::now().duration_since(
                                        SystemTime::UNIX_EPOCH
                                    ).map_err(
                                        |_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".into())
                                    )?.as_secs();

                                    let msg = StateSyncMessage::<TimestampHeader> {
                                        header: TimestampHeader { timestamp },
                                        snapshots: snapshot,
                                        deltas: None, // full-state frames — all changes are absolute
                                        removed_components,
                                    };

                                    yield Ok(("euclid".to_string(), msg));
                                },
                                Err(e) => {
                                    error!("Failed to parse Euclid levels frame: {}", e);
                                    break;
                                }
                            }
                        }
                        Ok(Message::Close(frame)) => {
                            match frame {
                                Some(frame) => warn!("WebSocket closed by server: {frame}"),
                                None => warn!("WebSocket closed by server without a close frame"),
                            }
                            break;
                        }
                        Err(e) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        _ => {} // Ignore other message types
                    }
                }

                // Message loop exited — always attempt to reconnect. Pricing data
                // resets the counter, so it only grows while the feed stays unusable.
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    yield Err(RFQError::ConnectionError(format!("No pricing data received after {MAX_CONSECUTIVE_FAILURES} consecutive failures")));
                    return;
                }

                let backoff_duration = Duration::from_secs(2_u64.pow(consecutive_failures.min(5)));
                info!("Reconnecting in {} seconds (consecutive failure {})...", backoff_duration.as_secs(), consecutive_failures);
                sleep(backoff_duration).await;
            }
        })
    }

    async fn request_binding_quote(
        &self,
        params: &GetAmountOutParams,
    ) -> Result<SignedQuote, RFQError> {
        let to_addr = |bytes: &Bytes, label: &str| -> Result<String, RFQError> {
            if bytes.len() == 20 {
                Ok(format!("0x{}", hex::encode(bytes)))
            } else {
                Err(RFQError::InvalidInput(format!("Invalid {label} address: {bytes:?}")))
            }
        };

        let body = serde_json::json!({
            "chain_id": chain_query(self.chain)?,
            "token_in": to_addr(&params.token_in, "token_in")?,
            "token_out": to_addr(&params.token_out, "token_out")?,
            "amount_in": params.amount_in.to_string(),
            "sender": to_addr(&params.sender, "sender")?,
            "receiver": to_addr(&params.receiver, "receiver")?,
        });

        let client = Client::new();
        let start_time = std::time::Instant::now();
        const MAX_RETRIES: u32 = 3;
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            let elapsed = start_time.elapsed();
            if elapsed >= self.quote_timeout {
                return Err(last_error.unwrap_or_else(|| {
                    RFQError::ConnectionError(format!(
                        "Euclid quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    ))
                }));
            }
            let remaining_time = self.quote_timeout - elapsed;

            let mut request = client
                .post(&self.quote_endpoint)
                .json(&body)
                .header("accept", "application/json");
            if let Some(key) = &self.api_key {
                request = request.header("x-api-key", key);
            }

            let response = match timeout(remaining_time, request.send()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    warn!(
                        "Euclid quote request failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    last_error = Some(RFQError::ConnectionError(format!(
                        "Failed to send Euclid quote request: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
                Err(_) => {
                    return Err(RFQError::ConnectionError(format!(
                        "Euclid quote request timed out after {} seconds",
                        self.quote_timeout.as_secs()
                    )));
                }
            };

            let quote_response = match response
                .json::<EuclidFirmResponse>()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(
                        "Euclid quote response parsing failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    last_error = Some(RFQError::ParsingError(format!(
                        "Failed to parse Euclid quote response: {e}"
                    )));
                    if attempt < MAX_RETRIES - 1 {
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
            };

            return Self::process_quote_response(quote_response, params);
        }

        Err(last_error.unwrap_or_else(|| {
            RFQError::ConnectionError("Euclid quote request failed after retries".to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use futures::SinkExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };
    use tokio_tungstenite::accept_async;

    use super::*;
    use crate::rfq::protocols::euclid::models::EuclidFirmQuote;

    fn weth_bytes() -> Bytes {
        Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
    }

    fn usdc_bytes() -> Bytes {
        Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap()
    }

    fn test_client() -> EuclidClient {
        EuclidClient::new(
            Chain::Ethereum,
            "https://rfq.example.com/tycho".to_string(),
            HashSet::new(),
            0.0,
            Some("key".to_string()),
            Duration::from_secs(3),
        )
        .unwrap()
    }

    fn test_params(amount_in: &str) -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from_str(amount_in).unwrap(),
            token_in: weth_bytes(),
            token_out: usdc_bytes(),
            sender: Bytes::from(vec![2u8; 20]),
            receiver: Bytes::from(vec![2u8; 20]),
        }
    }

    fn valid_calldata() -> String {
        // fillOrderRFQTo selector + 9 zero words — long enough for offset 8.
        format!("0x5a099843{}", "00".repeat(9 * 32))
    }

    fn firm_quote_json(amount_in: &str, amount_out: &str) -> String {
        format!(
            r#"{{"tx_to":"0x1111111254eeb25477b68fb85ed929f73a960582","calldata":"{}","partial_fill_offset":8,"amount_in":"{amount_in}","amount_out":"{amount_out}","expiry":99999999999,"order_hash":"0xabc"}}"#,
            valid_calldata()
        )
    }

    fn levels_frame_json(pairs: &[(&str, &str, f64, f64)]) -> String {
        // (base_addr, quote_addr, bid_price, bid_size)
        let pairs_json: Vec<String> = pairs
            .iter()
            .map(|(base, quote, price, size)| {
                format!(
                    r#"{{"base_symbol":"ETH","quote_symbol":"USDC","base_address":"{base}","quote_address":"{quote}","bids":[["{price}","{size}"]],"asks":[["{ask}","{size}"]],"timestamp":1757500000000}}"#,
                    ask = price + 2.0
                )
            })
            .collect();
        format!(r#"{{"chain_id":1,"timestamp":1757500000000,"pairs":[{}]}}"#, pairs_json.join(","))
    }

    // ---------- URL construction ----------

    #[test]
    fn test_urls() {
        let client = test_client();
        assert_eq!(client.price_ws, "wss://rfq.example.com/tycho/levels?chainId=1");
        assert_eq!(client.quote_endpoint, "https://rfq.example.com/tycho/firm");
    }

    #[test]
    fn test_http_base_maps_to_ws() {
        let client = EuclidClient::new(
            Chain::Ethereum,
            "http://127.0.0.1:8080/tycho/".to_string(), // trailing slash trimmed
            HashSet::new(),
            0.0,
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(client.price_ws, "ws://127.0.0.1:8080/tycho/levels?chainId=1");
        assert_eq!(client.quote_endpoint, "http://127.0.0.1:8080/tycho/firm");
    }

    #[test]
    fn test_unsupported_chain_rejected() {
        let result = EuclidClient::new(
            Chain::Base,
            "https://rfq.example.com/tycho".to_string(),
            HashSet::new(),
            0.0,
            None,
            Duration::from_secs(1),
        );
        assert!(result.is_err());
    }

    // ---------- Quote response processing ----------

    #[test]
    fn test_process_quote_response_success() {
        let response = EuclidFirmResponse::Success(EuclidFirmQuote {
            tx_to: "0x1111111254eeb25477b68fb85ed929f73a960582".to_string(),
            calldata: valid_calldata(),
            partial_fill_offset: 8,
            amount_in: "1000000000000000000".to_string(),
            amount_out: "3496000000".to_string(),
            expiry: u64::MAX,
            order_hash: "0xabc".to_string(),
        });
        let params = test_params("1000000000000000000");
        let quote = EuclidClient::process_quote_response(response, &params).expect("valid");
        assert_eq!(quote.amount_out, BigUint::from_str("3496000000").unwrap());
        assert_eq!(
            quote
                .quote_attributes
                .get("tx_to")
                .unwrap()
                .len(),
            20
        );
        assert_eq!(
            quote
                .quote_attributes
                .get("partial_fill_offset")
                .unwrap()
                .as_ref(),
            8u64.to_be_bytes()
        );
        // Calldata attribute decodes back to the selector-prefixed bytes.
        let calldata = quote
            .quote_attributes
            .get("calldata")
            .unwrap();
        assert_eq!(&calldata[0..4], [0x5a, 0x09, 0x98, 0x43]);
    }

    #[test]
    fn test_process_quote_response_error() {
        let response = EuclidFirmResponse::Error(super::super::models::EuclidFirmError {
            error: "insufficient_liquidity".to_string(),
        });
        assert!(EuclidClient::process_quote_response(response, &test_params("1")).is_err());
    }

    #[test]
    fn test_process_quote_response_amount_mismatch_rejected() {
        let response = EuclidFirmResponse::Success(EuclidFirmQuote {
            tx_to: "0x1111111254eeb25477b68fb85ed929f73a960582".to_string(),
            calldata: valid_calldata(),
            partial_fill_offset: 8,
            amount_in: "999".to_string(), // != requested
            amount_out: "3496000000".to_string(),
            expiry: u64::MAX,
            order_hash: "0xabc".to_string(),
        });
        assert!(EuclidClient::process_quote_response(response, &test_params("1000")).is_err());
    }

    // ---------- Pair parsing ----------

    #[test]
    fn test_parse_pair_rejects_bad_address() {
        let pair = EuclidPairLevels {
            base_symbol: "ETH".to_string(),
            quote_symbol: "USDC".to_string(),
            base_address: "0xnothex".to_string(),
            quote_address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            bids: vec![],
            asks: vec![],
            timestamp: 0,
        };
        assert!(EuclidClient::parse_pair(&pair).is_err());
    }

    // ---------- Levels stream over a local WebSocket server ----------

    #[tokio::test]
    async fn test_stream_snapshots_filters_and_removals() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let wbtc = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"; // untracked

        // Frame 1: tracked pair (rich), untracked pair, and a below-TVL pair
        // that reuses the tracked tokens (price*size = $3 < $100 threshold).
        // Frame 2: no pairs → the tracked component must show up as removed.
        let frame1 = format!(
            r#"{{"chain_id":1,"timestamp":1,"pairs":[
                {{"base_symbol":"ETH","quote_symbol":"USDC","base_address":"{weth}","quote_address":"{usdc}","bids":[["3000.0","2.0"]],"asks":[["3002.0","2.0"]],"timestamp":1}},
                {{"base_symbol":"WBTC","quote_symbol":"USDC","base_address":"{wbtc}","quote_address":"{usdc}","bids":[["65000.0","1.0"]],"asks":[["65100.0","1.0"]],"timestamp":1}}
            ]}}"#
        );
        let frame2 = r#"{"chain_id":1,"timestamp":2,"pairs":[]}"#.to_string();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(ws_stream) = accept_async(stream).await {
                    let (mut ws_sender, _) = ws_stream.split();
                    let _ = ws_sender
                        .send(Message::Text(frame1.into()))
                        .await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = ws_sender
                        .send(Message::Text(frame2.into()))
                        .await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = EuclidClient::new(
            Chain::Ethereum,
            format!("http://127.0.0.1:{}", addr.port()),
            HashSet::from([weth_bytes(), usdc_bytes()]),
            100.0,
            None,
            Duration::from_secs(3),
        )
        .unwrap();

        let mut stream = client.stream();

        let (name, msg1) = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("first frame")
            .expect("stream alive")
            .expect("no error");
        assert_eq!(name, "euclid");
        // Only the tracked WETH/USDC pair survives the token filter.
        assert_eq!(msg1.snapshots.states.len(), 1);
        let component = msg1
            .snapshots
            .states
            .values()
            .next()
            .unwrap();
        assert_eq!(component.component.protocol_system, "rfq:euclid");
        assert_eq!(component.component.tokens.len(), 2);
        // TVL = bid quote sum = 3000 * 2 = $6000.
        assert_eq!(component.component_tvl, Some(6000.0));
        let bids_attr = component
            .state
            .attributes
            .get("bids")
            .expect("bids attribute");
        let bids: Vec<(f64, f64)> = serde_json::from_slice(bids_attr).unwrap();
        assert_eq!(bids, vec![(3000.0, 2.0)]);

        let (_, msg2) = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("second frame")
            .expect("stream alive")
            .expect("no error");
        assert!(msg2.snapshots.states.is_empty());
        assert_eq!(msg2.removed_components.len(), 1);
    }

    #[tokio::test]
    async fn test_stream_tvl_threshold_filters_thin_pairs() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        // $6 of bid depth — below the $100 threshold.
        let frame = levels_frame_json(&[(weth, usdc, 3.0, 2.0)]);
        let follow = levels_frame_json(&[(weth, usdc, 3000.0, 2.0)]);

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(ws_stream) = accept_async(stream).await {
                    let (mut ws_sender, _) = ws_stream.split();
                    let _ = ws_sender
                        .send(Message::Text(frame.into()))
                        .await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = ws_sender
                        .send(Message::Text(follow.into()))
                        .await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = EuclidClient::new(
            Chain::Ethereum,
            format!("http://127.0.0.1:{}", addr.port()),
            HashSet::from([weth_bytes(), usdc_bytes()]),
            100.0,
            None,
            Duration::from_secs(3),
        )
        .unwrap();
        let mut stream = client.stream();

        let (_, msg1) = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("frame")
            .expect("alive")
            .expect("ok");
        assert!(msg1.snapshots.states.is_empty(), "thin pair must be filtered");
        let (_, msg2) = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("frame")
            .expect("alive")
            .expect("ok");
        assert_eq!(msg2.snapshots.states.len(), 1, "rich pair must pass");
    }

    #[tokio::test]
    async fn test_websocket_reconnection() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let connection_count = Arc::new(Mutex::new(0u32));
        let connection_count_clone = connection_count.clone();

        let weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
        let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let frame = levels_frame_json(&[(weth, usdc, 3000.0, 2.0)]);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                *connection_count_clone.lock().unwrap() += 1;
                let count = *connection_count_clone.lock().unwrap();
                let frame = frame.clone();
                tokio::spawn(async move {
                    if let Ok(ws_stream) = accept_async(stream).await {
                        let (mut ws_sender, _) = ws_stream.split();
                        let _ = ws_sender
                            .send(Message::Text(frame.into()))
                            .await;
                        if count == 1 {
                            // Drop the first connection after delivering one frame.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            let _ = ws_sender.close().await;
                        } else {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = EuclidClient::new(
            Chain::Ethereum,
            format!("http://127.0.0.1:{}", addr.port()),
            HashSet::from([weth_bytes(), usdc_bytes()]),
            100.0,
            None,
            Duration::from_secs(3),
        )
        .unwrap();
        let mut stream = client.stream();

        // One frame from each connection proves the reconnect loop works.
        for _ in 0..2 {
            let (_, msg) = timeout(Duration::from_secs(10), stream.next())
                .await
                .expect("frame within timeout")
                .expect("stream alive")
                .expect("no error");
            assert_eq!(msg.snapshots.states.len(), 1);
        }
        assert!(*connection_count.lock().unwrap() >= 2, "client must have reconnected");
    }

    // ---------- Binding quotes over a local HTTP server ----------

    /// One-shot HTTP server that captures the request and returns `body`.
    async fn spawn_http_server(body: String) -> (std::net::SocketAddr, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_clone = captured.clone();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = body.clone();
                let captured = captured_clone.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    *captured.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream
                        .write_all(response.as_bytes())
                        .await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, captured)
    }

    fn client_for(addr: std::net::SocketAddr, api_key: Option<String>) -> EuclidClient {
        EuclidClient::new(
            Chain::Ethereum,
            format!("http://127.0.0.1:{}", addr.port()),
            HashSet::new(),
            0.0,
            api_key,
            Duration::from_secs(3),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_request_binding_quote_success_with_key() {
        let (addr, captured) =
            spawn_http_server(firm_quote_json("1000000000000000000", "3496000000")).await;
        let client = client_for(addr, Some("solver-key".to_string()));

        let quote = client
            .request_binding_quote(&test_params("1000000000000000000"))
            .await
            .expect("quote");
        assert_eq!(quote.amount_out, BigUint::from_str("3496000000").unwrap());

        let request = captured.lock().unwrap().clone();
        assert!(request.starts_with("POST /firm"), "unexpected request: {request}");
        assert!(request
            .to_ascii_lowercase()
            .contains("x-api-key: solver-key"));
        // Request body carries the params.
        assert!(request.contains(r#""amount_in":"1000000000000000000""#));
        assert!(request.contains(r#""chain_id":1"#));
    }

    #[tokio::test]
    async fn test_request_binding_quote_keyless_omits_header() {
        let (addr, captured) =
            spawn_http_server(firm_quote_json("1000000000000000000", "3496000000")).await;
        let client = client_for(addr, None);

        client
            .request_binding_quote(&test_params("1000000000000000000"))
            .await
            .expect("quote");
        let request = captured.lock().unwrap().clone();
        assert!(!request
            .to_ascii_lowercase()
            .contains("x-api-key"));
    }

    #[tokio::test]
    async fn test_request_binding_quote_api_error() {
        let (addr, _) =
            spawn_http_server(r#"{"error":"insufficient_liquidity"}"#.to_string()).await;
        let client = client_for(addr, None);
        let err = client
            .request_binding_quote(&test_params("1"))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("insufficient_liquidity"));
    }

    #[tokio::test]
    async fn test_request_binding_quote_timeout() {
        // Server that accepts but never responds within the client timeout.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(stream);
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = EuclidClient::new(
            Chain::Ethereum,
            format!("http://127.0.0.1:{}", addr.port()),
            HashSet::new(),
            0.0,
            None,
            Duration::from_millis(300),
        )
        .unwrap();
        let err = client
            .request_binding_quote(&test_params("1"))
            .await
            .unwrap_err();
        assert!(matches!(err, RFQError::ConnectionError(_)));
    }
}
