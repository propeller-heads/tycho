use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::aero_basic::state::{AeroBasicFees, AeroBasicState},
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

fn attribute<'a>(
    attributes: &'a HashMap<String, Bytes>,
    name: &str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
}

fn decimals(
    all_tokens: &HashMap<Bytes, Token>,
    address: &Bytes,
) -> Result<u8, InvalidSnapshotError> {
    all_tokens
        .get(address)
        .map(|token| token.decimals as u8)
        .ok_or_else(|| InvalidSnapshotError::ValueError(format!("Token {address} not found")))
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for AeroBasicState {
    type Error = InvalidSnapshotError;

    /// Decodes an `aero_basic` component. Errors if an attribute the `evm-aero-basic` Substreams
    /// package always emits is missing, or a token is unknown.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let state = &snapshot.state.attributes;
        let [token0, token1] = snapshot.component.tokens.as_slice() else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Expected 2 tokens, got {}",
                snapshot.component.tokens.len()
            )));
        };
        let stable = attribute(&snapshot.component.static_attributes, "is_stable")?.as_ref() == [1];
        let fees = AeroBasicFees {
            custom_fee: u32::from(attribute(state, "custom_fee")?.clone()),
            default_fee: u32::from(attribute(state, "default_fee")?.clone()),
            fee_module: attribute(state, "fee_module")?.clone(),
            paused: attribute(state, "paused")?
                .iter()
                .any(|byte| *byte != 0),
        };
        let reserves = (
            U256::from_be_slice(attribute(state, "reserve0")?),
            U256::from_be_slice(attribute(state, "reserve1")?),
        );

        AeroBasicState::new(
            reserves,
            stable,
            fees,
            (decimals(all_tokens, token0)?, decimals(all_tokens, token1)?),
        )
        .map_err(InvalidSnapshotError::ValueError)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            token::Token,
            Chain,
        },
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    };

    use super::*;

    fn tokens() -> HashMap<Bytes, Token> {
        [
            ("0x0000000000000000000000000000000000000001", 18),
            ("0x0000000000000000000000000000000000000002", 6),
        ]
        .into_iter()
        .map(|(address, decimals)| {
            let address = Bytes::from_str(address).unwrap();
            let token = Token::new(&address, "T", decimals, 0, &[Some(10_000)], Chain::Base, 100);
            (address, token)
        })
        .collect()
    }

    fn snapshot(missing: Option<&str>) -> ComponentWithState {
        let mut attributes = HashMap::from([
            ("reserve0".to_string(), Bytes::from(vec![0x0f, 0x42, 0x40])),
            ("reserve1".to_string(), Bytes::from(vec![0x0f, 0x42, 0x40])),
            ("custom_fee".to_string(), Bytes::from(vec![0])),
            ("default_fee".to_string(), Bytes::from(vec![30])),
            (
                "fee_module".to_string(),
                Bytes::from_str("0x7949DE8d67E4F6b8509Bdbb43FF0503aF1247a7A").unwrap(),
            ),
            ("paused".to_string(), Bytes::from(vec![0])),
        ]);
        if let Some(name) = missing {
            attributes.remove(name);
        }
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "0x00".to_string(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                tokens: tokens()
                    .into_keys()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                static_attributes: HashMap::from([("is_stable".to_string(), Bytes::from(vec![0]))]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    async fn decode(snapshot: ComponentWithState) -> Result<AeroBasicState, InvalidSnapshotError> {
        AeroBasicState::try_from_with_header(
            snapshot,
            BlockHeader::default(),
            &HashMap::new(),
            &tokens(),
            &DecoderContext::new(),
        )
        .await
    }

    #[tokio::test]
    async fn decodes_a_snapshot_that_quotes() {
        let state = decode(snapshot(None)).await.unwrap();
        let tokens = tokens();
        let token0 =
            &tokens[&Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap()];
        let token1 =
            &tokens[&Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap()];

        let out = state
            .get_amount_out(BigUint::from(10_000u64), token0, token1)
            .unwrap();

        // 30 bps default fee on a 1e6/1e6 volatile pool.
        assert_eq!(out.amount, BigUint::from(9_871u64));
    }

    #[rstest]
    #[case::reserve0("reserve0")]
    #[case::custom_fee("custom_fee")]
    #[case::default_fee("default_fee")]
    #[case::fee_module("fee_module")]
    #[case::paused("paused")]
    #[tokio::test]
    async fn missing_attribute_is_an_error(#[case] name: &str) {
        let err = decode(snapshot(Some(name)))
            .await
            .unwrap_err();

        assert!(matches!(err, InvalidSnapshotError::MissingAttribute(missing) if missing == name));
    }
}
