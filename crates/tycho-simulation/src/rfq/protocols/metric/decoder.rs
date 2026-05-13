use std::collections::{HashMap, HashSet};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    client_builder::MetricClientBuilder,
    models::{MetricBidAskResponse, MetricDepth, MetricMetadata},
    state::MetricState,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::models::TimestampHeader,
};

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for MetricState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _timestamp_header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        if snapshot.component.tokens.len() != 2 {
            return Err(InvalidSnapshotError::ValueError(
                "Metric component must have token0 and token1".to_string(),
            ));
        }

        let token0_address = &snapshot.component.tokens[0];
        let token1_address = &snapshot.component.tokens[1];
        let token0 = all_tokens
            .get(token0_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Metric token0 not found: {token0_address}"
                ))
            })?
            .clone();
        let token1 = all_tokens
            .get(token1_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Metric token1 not found: {token1_address}"
                ))
            })?
            .clone();

        // RFQ snapshots do not carry balances; all Metric pricing data is stored as attributes.
        let attrs = snapshot.state.attributes;
        let metadata = MetricMetadata {
            pair: read_string_attr(&attrs, "pair")?,
            pool_address: read_bytes_attr(&attrs, "pool_address")?,
            price_provider_address: read_bytes_attr(&attrs, "price_provider_address")?,
            quoter_address: read_bytes_attr(&attrs, "quoter_address")?,
            token0: token0_address.clone(),
            token1: token1_address.clone(),
            cex_step: read_optional_f64_attr(&attrs, "cex_step")?,
            dex_step: read_optional_f64_attr(&attrs, "dex_step")?,
        };
        let bid_ask = MetricBidAskResponse {
            pair: metadata.pair.clone(),
            bid_adj: read_string_attr(&attrs, "bid_adj")?,
            ask_adj: read_string_attr(&attrs, "ask_adj")?,
            quote_available: true,
            total_token0_available: read_string_attr(&attrs, "total_token0_available")?,
            total_token1_available: read_string_attr(&attrs, "total_token1_available")?,
            latest_block: read_u64_attr(&attrs, "latest_block")?,
            block_ts: read_u64_attr(&attrs, "block_ts")?,
            server_ts: read_u64_attr(&attrs, "server_ts")?,
            quote_expiration: read_u64_attr(&attrs, "quote_expiration")?,
            depth: read_optional_depth_attr(&attrs, "depth")?,
        };

        let client = MetricClientBuilder::new(snapshot.component.chain.into())
            .tokens(HashSet::from([token0_address.clone(), token1_address.clone()]))
            .build()
            .map_err(|e| {
                InvalidSnapshotError::ValueError(format!("Couldn't create MetricClient: {e}"))
            })?;

        Ok(MetricState::new(token0, token1, metadata, bid_ask, client))
    }
}

fn read_bytes_attr(
    attrs: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Bytes, InvalidSnapshotError> {
    attrs.get(name).cloned().ok_or_else(|| {
        InvalidSnapshotError::MissingAttribute(format!("{name} attribute not found"))
    })
}

fn read_string_attr(
    attrs: &HashMap<String, Bytes>,
    name: &str,
) -> Result<String, InvalidSnapshotError> {
    let bytes = attrs.get(name).ok_or_else(|| {
        InvalidSnapshotError::MissingAttribute(format!("{name} attribute not found"))
    })?;
    String::from_utf8(bytes.to_vec())
        .map_err(|_| InvalidSnapshotError::ValueError(format!("Invalid {name} encoding")))
}

fn read_u64_attr(attrs: &HashMap<String, Bytes>, name: &str) -> Result<u64, InvalidSnapshotError> {
    read_string_attr(attrs, name)?
        .parse()
        .map_err(|_| InvalidSnapshotError::ValueError(format!("Invalid {name} integer")))
}

fn read_optional_f64_attr(
    attrs: &HashMap<String, Bytes>,
    name: &str,
) -> Result<Option<f64>, InvalidSnapshotError> {
    match attrs.get(name) {
        Some(_) => read_string_attr(attrs, name)?
            .parse()
            .map(Some)
            .map_err(|_| InvalidSnapshotError::ValueError(format!("Invalid {name} float"))),
        None => Ok(None),
    }
}

fn read_optional_depth_attr(
    attrs: &HashMap<String, Bytes>,
    name: &str,
) -> Result<MetricDepth, InvalidSnapshotError> {
    match attrs.get(name) {
        Some(bytes) => serde_json::from_slice(bytes)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid {name} JSON: {e}"))),
        None => Ok(MetricDepth::default()),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::{
        dto::{Chain, ChangeType, ProtocolComponent, ResponseProtocolState},
        models::Chain as ModelChain,
    };

    use super::*;

    fn weth() -> Token {
        Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(2300)],
            ModelChain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(1)],
            ModelChain::Ethereum,
            100,
        )
    }

    fn create_snapshot() -> (ComponentWithState, HashMap<Bytes, Token>) {
        let weth = weth();
        let usdc = usdc();
        let mut tokens = HashMap::new();
        tokens.insert(weth.address.clone(), weth.clone());
        tokens.insert(usdc.address.clone(), usdc.clone());

        let mut attrs = HashMap::new();
        attrs.insert("pair".to_string(), "ethusdc".as_bytes().to_vec().into());
        attrs.insert(
            "pool_address".to_string(),
            Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
        );
        attrs.insert(
            "price_provider_address".to_string(),
            Bytes::from_str("0xbD321D18a7ce5fb91F8b16e026e3258f7b310598").unwrap(),
        );
        attrs.insert(
            "quoter_address".to_string(),
            Bytes::from_str("0x58F9d1865d4Aeb59a9a7Dc68A3b4e0B42D9Ef5eD").unwrap(),
        );
        attrs.insert(
            "bid_adj".to_string(),
            "55340232221128654848000"
                .as_bytes()
                .to_vec()
                .into(),
        );
        attrs.insert(
            "ask_adj".to_string(),
            "55524699661865750400000"
                .as_bytes()
                .to_vec()
                .into(),
        );
        attrs.insert(
            "total_token0_available".to_string(),
            "10000000000000000000"
                .as_bytes()
                .to_vec()
                .into(),
        );
        attrs
            .insert("total_token1_available".to_string(), "30000000000".as_bytes().to_vec().into());
        attrs.insert("latest_block".to_string(), "100".as_bytes().to_vec().into());
        attrs.insert("block_ts".to_string(), "1700000000".as_bytes().to_vec().into());
        attrs.insert("server_ts".to_string(), "1700000001".as_bytes().to_vec().into());
        attrs.insert("quote_expiration".to_string(), "1700000005".as_bytes().to_vec().into());
        attrs.insert("depth".to_string(), r#"{"asks":[],"bids":[]}"#.as_bytes().to_vec().into());

        let snapshot = ComponentWithState {
            state: ResponseProtocolState {
                attributes: attrs,
                component_id: "metric_ethusdc".to_string(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: "metric_ethusdc".to_string(),
                protocol_system: "rfq:metric".to_string(),
                protocol_type_name: "metric_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![weth.address.clone(), usdc.address.clone()],
                contract_ids: Vec::new(),
                static_attributes: HashMap::new(),
                change: ChangeType::Creation,
                creation_tx: Bytes::default(),
                created_at: chrono::NaiveDateTime::default(),
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        (snapshot, tokens)
    }

    #[tokio::test]
    async fn test_try_from_with_header() {
        let (snapshot, tokens) = create_snapshot();
        let state = MetricState::try_from_with_header(
            snapshot,
            TimestampHeader { timestamp: 1_700_000_000 },
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await
        .expect("decode metric state");

        assert_eq!(state.base_token.symbol, "WETH");
        assert_eq!(state.quote_token.symbol, "USDC");
        assert_eq!(state.metadata.pair, "ethusdc");
        assert_eq!(state.bid_ask.latest_block, 100);
    }

    #[tokio::test]
    async fn test_try_from_missing_attribute() {
        let (mut snapshot, tokens) = create_snapshot();
        snapshot
            .state
            .attributes
            .remove("bid_adj");

        let result = MetricState::try_from_with_header(
            snapshot,
            TimestampHeader::default(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::new(),
        )
        .await;

        assert!(result.is_err());
    }
}
