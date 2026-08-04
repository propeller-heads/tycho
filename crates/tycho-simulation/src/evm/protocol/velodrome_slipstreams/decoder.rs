use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::VelodromeSlipstreamsState;
use crate::{
    evm::protocol::utils::uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for VelodromeSlipstreamsState {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `AerodromeSlipstreamsState`. Errors with a
    /// `InvalidSnapshotError` if the snapshot is missing any required attributes or if the fee
    /// amount is not supported.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let liquidity = u128::from(
            snapshot
                .state
                .attributes
                .get("liquidity")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("liquidity".to_string()))?
                .clone(),
        );

        let sqrt_price = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("sqrt_price_x96")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("sqrt_price".to_string()))?,
        );

        let custom_fee = u32::from(
            snapshot
                .state
                .attributes
                .get("custom_fee")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("custom_fee".to_string()))?
                .clone(),
        );

        let tick_spacing = snapshot
            .component
            .static_attributes
            .get("tick_spacing")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick_spacing".to_string()))?
            .clone();

        let tick_spacing_4_bytes = if tick_spacing.len() == 32 {
            // Make sure it only happens for 0 values, otherwise error.
            if tick_spacing == Bytes::zero(32) {
                Bytes::from([0; 4])
            } else {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Tick Spacing bytes too long for {tick_spacing}, expected 4"
                )));
            }
        } else {
            tick_spacing
        };

        let tick_spacing = i24_be_bytes_to_i32(&tick_spacing_4_bytes);

        let default_fee = u32::from(
            snapshot
                .component
                .static_attributes
                .get("default_fee")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("default_fee".to_string()))?
                .clone(),
        );

        let tick = i32::from(
            snapshot
                .state
                .attributes
                .get("tick")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick".to_string()))?
                .clone(),
        );

        let ticks: Result<Vec<_>, _> = snapshot
            .state
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                if key.starts_with("ticks/") {
                    Some(
                        key.split('/')
                            .nth(1)?
                            .parse::<i32>()
                            .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
                            .and_then(|tick_index| {
                                TickInfo::new(tick_index, i128::from(value.clone())).map_err(
                                    |err| InvalidSnapshotError::ValueError(err.to_string()),
                                )
                            }),
                    )
                } else {
                    None
                }
            })
            .collect();

        let mut ticks = match ticks {
            Ok(ticks) if !ticks.is_empty() => ticks
                .into_iter()
                .filter(|t| t.net_liquidity != 0)
                .collect::<Vec<_>>(),
            _ => return Err(InvalidSnapshotError::MissingAttribute("tick_liquidities".to_string())),
        };

        ticks.sort_by_key(|tick| tick.index);

        VelodromeSlipstreamsState::new(
            liquidity,
            sqrt_price,
            default_fee,
            custom_fee,
            tick_spacing,
            tick,
            ticks,
        )
        .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::DateTime;
    use rstest::rstest;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;
    use crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults;

    fn velodrome_component() -> ProtocolComponent {
        let creation_time = DateTime::from_timestamp(1622526000, 0)
            .unwrap()
            .naive_utc();

        let static_attributes = HashMap::from([
            ("tick_spacing".to_string(), Bytes::from(1_i32.to_be_bytes().to_vec())),
            ("default_fee".to_string(), Bytes::from(3000_u32.to_be_bytes().to_vec())),
        ]);

        ProtocolComponent {
            id: "State1".to_string(),
            protocol_system: "system1".to_string(),
            protocol_type_name: "typename1".to_string(),
            chain: Chain::Ethereum,
            tokens: Vec::new(),
            contract_addresses: Vec::new(),
            static_attributes,
            change: ChangeType::Creation,
            creation_tx: Bytes::from_str("0x0000").unwrap(),
            created_at: creation_time,
        }
    }

    fn velodrome_attributes() -> HashMap<String, Bytes> {
        HashMap::from([
            ("liquidity".to_string(), Bytes::from(100_u128.to_be_bytes().to_vec())),
            ("sqrt_price_x96".to_string(), Bytes::from(200_u64.to_be_bytes().to_vec())),
            ("custom_fee".to_string(), Bytes::from(500_u32.to_be_bytes().to_vec())),
            ("tick".to_string(), Bytes::from(0_i32.to_be_bytes().to_vec())),
            ("ticks/60".to_string(), Bytes::from(400_i128.to_be_bytes().to_vec())),
        ])
    }

    fn snapshot(
        attributes: HashMap<String, Bytes>,
        component: ProtocolComponent,
    ) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_velodrome_try_from() {
        let decoded = try_decode_snapshot_with_defaults::<VelodromeSlipstreamsState>(snapshot(
            velodrome_attributes(),
            velodrome_component(),
        ))
        .await
        .expect("a complete snapshot should decode");

        let expected = VelodromeSlipstreamsState::new(
            100,
            U256::from(200),
            3000,
            500,
            1,
            0,
            vec![TickInfo::new(60, 400).unwrap()],
        )
        .unwrap();
        assert_eq!(decoded, expected);
    }

    /// Every attribute the decoder reads is required; dropping any one of them must be reported as
    /// that attribute rather than decoding a pool with a silently defaulted field.
    #[rstest]
    #[case::missing_liquidity("liquidity")]
    #[case::missing_sqrt_price("sqrt_price")]
    #[case::missing_custom_fee("custom_fee")]
    #[case::missing_tick("tick")]
    #[case::missing_tick_spacing("tick_spacing")]
    #[case::missing_default_fee("default_fee")]
    #[tokio::test]
    async fn test_velodrome_try_from_invalid(#[case] missing_attribute: String) {
        let mut attributes = velodrome_attributes();
        let mut component = velodrome_component();

        // The sqrt price is reported under a shorter name than the attribute holding it.
        let attribute_key =
            if missing_attribute == "sqrt_price" { "sqrt_price_x96" } else { &missing_attribute };
        attributes.remove(attribute_key);
        component
            .static_attributes
            .remove(&missing_attribute);

        let result = try_decode_snapshot_with_defaults::<VelodromeSlipstreamsState>(snapshot(
            attributes, component,
        ))
        .await;

        assert!(matches!(
            result.expect_err("a snapshot missing a required attribute must not decode"),
            InvalidSnapshotError::MissingAttribute(attr) if attr == missing_attribute
        ));
    }
}
