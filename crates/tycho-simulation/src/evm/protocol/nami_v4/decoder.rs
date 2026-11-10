use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{Address, U256};
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, simulation::protocol_sim::BlockContext, Bytes};

use super::state::{decode_optional_address, decode_u64, parse_tick_key, NamiV4State};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for NamiV4State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `NamiV4State`. Errors with an `InvalidSnapshotError`
    /// if the component id is not the pool address or a required attribute is missing.
    ///
    /// `plugin` and `plugin_config` are optional: a pool without them has no plugin and charges
    /// its stored `fee`. Every `ticks/<index>` attribute is an initialized tick, including ones
    /// with zero net liquidity.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let pool = Address::from_str(&snapshot.component.id).map_err(|err| {
            InvalidSnapshotError::ValueError(format!(
                "expected nami_v4 component id to be the pool address: {err}"
            ))
        })?;
        let attrs = snapshot.state.attributes;

        let liquidity = u128::from(attribute(&attrs, "liquidity")?.clone());
        let sqrt_price = U256::from_be_slice(attribute(&attrs, "sqrt_price_x96")?);
        let tick = i32::from(attribute(&attrs, "tick")?.clone());
        let last_fee = u32::from(attribute(&attrs, "fee")?.clone());
        let optional_address = |key: &str| match attrs.get(key) {
            Some(value) => decode_optional_address(value).map_err(InvalidSnapshotError::ValueError),
            None => Ok(None),
        };
        let plugin = optional_address("plugin")?;
        let mevx_router = optional_address("mevx_router")?;
        let incentive = optional_address("incentive")?;
        let min_gas_left = attrs
            .get("min_gas_left")
            .map_or(0, decode_u64);
        let call_gas_budget = attrs
            .get("call_gas_budget")
            .map_or(0, decode_u64);
        let plugin_config = attrs
            .get("plugin_config")
            .map_or(0, |value| u8::from(value.clone()));

        let mut ticks = Vec::new();
        for (key, value) in &attrs {
            if let Some(index) = parse_tick_key(key) {
                let index = index.map_err(InvalidSnapshotError::ValueError)?;
                ticks.push((index, i128::from(value.clone())));
            }
        }

        Ok(NamiV4State::new(
            pool,
            liquidity,
            sqrt_price,
            tick,
            ticks,
            last_fee,
            plugin,
            plugin_config,
            // Seed value only: the stream decoder points every state at the execution block
            // before the snapshot reaches a consumer.
            BlockContext::new(block.number, block.timestamp),
        )
        .with_plugin_settings(mevx_router, min_gas_left, call_gas_budget, incentive))
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
    use chrono::DateTime;
    use rstest::rstest;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;
    use crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults;

    const POOL: &str = "0x1111111111111111111111111111111111111111";

    fn component() -> ProtocolComponent {
        ProtocolComponent {
            id: POOL.to_string(),
            protocol_system: "nami_v4".to_string(),
            protocol_type_name: "nami_v4_pool".to_string(),
            chain: Chain::Base,
            tokens: Vec::new(),
            contract_addresses: Vec::new(),
            static_attributes: HashMap::new(),
            change: ChangeType::Creation,
            creation_tx: Bytes::from_str("0x0000").unwrap(),
            created_at: DateTime::from_timestamp(1622526000, 0)
                .unwrap()
                .naive_utc(),
        }
    }

    fn attributes() -> HashMap<String, Bytes> {
        [
            ("liquidity", Bytes::from(100_u64.to_be_bytes().to_vec())),
            ("sqrt_price_x96", Bytes::from(200_u64.to_be_bytes().to_vec())),
            ("tick", Bytes::from((-300_i32).to_be_bytes().to_vec())),
            ("fee", Bytes::from(3000_u32.to_be_bytes().to_vec())),
            ("plugin", Bytes::from(vec![0x22; 20])),
            ("plugin_config", Bytes::from(vec![0xC3])),
            ("ticks/60", Bytes::from(400_i128.to_be_bytes().to_vec())),
            ("ticks/-60", Bytes::from(0_i128.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
    }

    fn snapshot(attributes: HashMap<String, Bytes>) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: POOL.to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_decodes_state_and_keeps_zero_net_liquidity_ticks() {
        let decoded: NamiV4State = try_decode_snapshot_with_defaults(snapshot(attributes()))
            .await
            .unwrap();

        let expected = NamiV4State::new(
            Address::from_str(POOL).unwrap(),
            100,
            U256::from(200),
            -300,
            vec![(60, 400), (-60, 0)],
            3000,
            Some(Address::repeat_byte(0x22)),
            0xC3,
            BlockContext::default(),
        );
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn test_decodes_the_plugin_settings() {
        let mut attributes = attributes();
        attributes.insert("mevx_router".to_string(), Bytes::from(vec![0x33; 20]));
        attributes.insert("min_gas_left".to_string(), Bytes::from(vec![0x0F, 0x42, 0x40]));
        attributes.insert("call_gas_budget".to_string(), Bytes::from(5_000_000_u64));
        attributes.insert("incentive".to_string(), Bytes::from(vec![0x44; 20]));

        let decoded: NamiV4State = try_decode_snapshot_with_defaults(snapshot(attributes))
            .await
            .unwrap();

        let expected = NamiV4State::new(
            Address::from_str(POOL).unwrap(),
            100,
            U256::from(200),
            -300,
            vec![(60, 400), (-60, 0)],
            3000,
            Some(Address::repeat_byte(0x22)),
            0xC3,
            BlockContext::default(),
        )
        .with_plugin_settings(
            Some(Address::repeat_byte(0x33)),
            1_000_000,
            5_000_000,
            Some(Address::repeat_byte(0x44)),
        );
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn test_pool_without_plugin_attributes_decodes_as_plugin_less() {
        let mut attributes = attributes();
        attributes.remove("plugin");
        attributes.remove("plugin_config");

        let decoded: NamiV4State = try_decode_snapshot_with_defaults(snapshot(attributes))
            .await
            .unwrap();

        let expected = NamiV4State::new(
            Address::from_str(POOL).unwrap(),
            100,
            U256::from(200),
            -300,
            vec![(60, 400), (-60, 0)],
            3000,
            None,
            0,
            BlockContext::default(),
        );
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_liquidity("liquidity")]
    #[case::missing_sqrt_price("sqrt_price_x96")]
    #[case::missing_tick("tick")]
    #[case::missing_fee("fee")]
    async fn test_missing_required_attribute_is_rejected(#[case] missing_attribute: String) {
        let mut attributes = attributes();
        attributes.remove(&missing_attribute);

        let result = try_decode_snapshot_with_defaults::<NamiV4State>(snapshot(attributes)).await;

        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(attr) if attr == missing_attribute
        ));
    }
}
