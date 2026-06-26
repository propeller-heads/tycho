use std::collections::HashMap;

use alloy::primitives::U256;
use itertools::Itertools;
use num_traits::Zero;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::RamsesV3State;
use crate::{
    evm::protocol::utils::uniswap::tick_list::TickInfo,
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for RamsesV3State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `RamsesV3State`. Errors with an `InvalidSnapshotError`
    /// if the snapshot is missing any required attributes.
    ///
    /// Unlike Uniswap V3, `fee` is read from the (mutable) state attributes rather than the static
    /// attributes, and `tick_spacing` is read directly from the static attributes instead of being
    /// derived from the fee tier.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let static_attrs = snapshot.component.static_attributes;
        let state_attrs = snapshot.state.attributes;

        // The fee is governance-mutable, so it lives in the (dynamic) state attributes.
        let fee = u32::from(attribute(&state_attrs, "fee")?.clone());
        let liquidity = u128::from(attribute(&state_attrs, "liquidity")?.clone());
        let sqrt_price = U256::from_be_slice(attribute(&state_attrs, "sqrt_price_x96")?);
        let tick_spacing = u16::from(attribute(&static_attrs, "tick_spacing")?.clone());
        let tick = i32::from(attribute(&state_attrs, "tick")?.clone());

        // An empty tick list is valid: a pool that is not yet initialized (or has no liquidity)
        // simply decodes to a zero-liquidity state that gracefully returns "No liquidity" on quote.
        let ticks = state_attrs
            .iter()
            .filter_map(|(key, value)| {
                let tick_index = match key
                    .strip_prefix("ticks/")?
                    .parse::<i32>()
                {
                    Ok(tick_index) => tick_index,
                    Err(err) => return Some(Err(InvalidSnapshotError::ValueError(err.to_string()))),
                };

                let net_liquidity = i128::from(value.clone());
                if net_liquidity.is_zero() {
                    return None;
                }

                Some(
                    TickInfo::new(tick_index, net_liquidity)
                        .map_err(|err| InvalidSnapshotError::ValueError(err.to_string())),
                )
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .sorted_unstable_by_key(|tick| tick.index)
            .collect();

        RamsesV3State::new(liquidity, sqrt_price, fee, tick_spacing, tick, ticks)
            .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
    }
}

fn attribute<'a>(
    map: &'a HashMap<String, Bytes>,
    key: &str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    map.get(key)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(key.to_string()))
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

    fn ramses_component() -> ProtocolComponent {
        let creation_time = DateTime::from_timestamp(1622526000, 0)
            .unwrap()
            .naive_utc();

        // tick_spacing is the only V3-style static attribute; fee is dynamic.
        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes
            .insert("tick_spacing".to_string(), Bytes::from(60_u16.to_be_bytes().to_vec()));

        ProtocolComponent {
            id: "State1".to_string(),
            protocol_system: "ramses_v3".to_string(),
            protocol_type_name: "ramses_v3_pool".to_string(),
            chain: Chain::Polygon,
            tokens: Vec::new(),
            contract_addresses: Vec::new(),
            static_attributes,
            change: ChangeType::Creation,
            creation_tx: Bytes::from_str("0x0000").unwrap(),
            created_at: creation_time,
        }
    }

    fn ramses_attributes() -> HashMap<String, Bytes> {
        vec![
            ("liquidity".to_string(), Bytes::from(100_u64.to_be_bytes().to_vec())),
            ("sqrt_price_x96".to_string(), Bytes::from(200_u64.to_be_bytes().to_vec())),
            ("tick".to_string(), Bytes::from(300_i32.to_be_bytes().to_vec())),
            ("fee".to_string(), Bytes::from(3000_u32.to_be_bytes().to_vec())),
            ("ticks/60".to_string(), Bytes::from(400_i128.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect::<HashMap<String, Bytes>>()
    }

    #[tokio::test]
    async fn test_ramses_try_from() {
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: ramses_attributes(),
                balances: HashMap::new(),
            },
            component: ramses_component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        assert_eq!(
            RamsesV3State::new(
                100,
                U256::from(200),
                3000,
                60,
                300,
                vec![TickInfo::new(60, 400).unwrap()],
            )
            .unwrap(),
            try_decode_snapshot_with_defaults(snapshot)
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_liquidity("liquidity")]
    #[case::missing_sqrt_price("sqrt_price_x96")]
    #[case::missing_tick("tick")]
    #[case::missing_fee("fee")]
    #[case::missing_tick_spacing("tick_spacing")]
    async fn test_ramses_try_from_invalid(#[case] missing_attribute: String) {
        let mut attributes = ramses_attributes();
        attributes.remove(&missing_attribute);

        let mut component = ramses_component();
        component
            .static_attributes
            .remove(&missing_attribute);

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<RamsesV3State>(snapshot).await;

        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(attr) if attr == missing_attribute
        ));
    }

    #[tokio::test]
    async fn test_ramses_try_from_uninitialized() {
        // A created-but-not-initialized pool has no ticks and zero price/liquidity. It must still
        // decode (into a zero-liquidity state) rather than error.
        let mut attributes = ramses_attributes();
        attributes.remove("ticks/60");
        attributes.insert("liquidity".to_string(), Bytes::from(vec![0u8]));
        attributes.insert("sqrt_price_x96".to_string(), Bytes::from(vec![0u8]));

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: ramses_component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<RamsesV3State>(snapshot).await;

        assert!(result.is_ok());
    }
}
