use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, simulation::protocol_sim::BlockContext, Bytes};

use crate::{
    evm::protocol::algebra_integral::{
        plugin::PLUGIN_CONFIG_ATTRIBUTE,
        state::{
            decode_optional_address, decode_sqrt_price, is_plugin_module_attribute, parse_tick_key,
            AlgebraIntegralState, PluginSetup,
        },
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for AlgebraIntegralState {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into an `AlgebraIntegralState`.
    ///
    /// `plugin`, `plugin_config` and `plugin_impl` are optional: a pool without them has no
    /// plugin and charges its stored `fee`. Every `ticks/<index>` attribute is an initialized
    /// tick, including ones with zero net liquidity, and every other `<module>/<field>`
    /// attribute belongs to the plugin.
    ///
    /// Fails when the component id is not the pool address or a required attribute is missing
    /// or malformed. A pool that calls a plugin the simulation cannot reproduce, or that calls
    /// its plugin without `decoder_context.chain`, decodes but is not quoted.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let pool = Address::from_str(&snapshot.component.id).map_err(|err| {
            InvalidSnapshotError::ValueError(format!(
                "expected the component id to be the pool address: {err}"
            ))
        })?;
        let attrs = snapshot.state.attributes;

        let liquidity = u128::try_from(attribute(&attrs, "liquidity")?.clone())?;
        let sqrt_price = decode_sqrt_price(attribute(&attrs, "sqrt_price_x96")?)
            .map_err(InvalidSnapshotError::ValueError)?;
        let tick = i32::try_from(attribute(&attrs, "tick")?.clone())?;
        let last_fee = u32::try_from(attribute(&attrs, "fee")?.clone())?;
        let optional_address = |key: &str| match attrs.get(key) {
            Some(value) => decode_optional_address(value)
                .map_err(|err| InvalidSnapshotError::ValueError(format!("attribute {key}: {err}"))),
            None => Ok(None),
        };
        let plugin = optional_address("plugin")?;
        let implementation = optional_address("plugin_impl")?;
        let config = match attrs.get(PLUGIN_CONFIG_ATTRIBUTE) {
            Some(value) => u8::try_from(value.clone())?,
            None => 0,
        };

        let mut ticks = Vec::new();
        let mut module_attributes = HashMap::new();
        for (key, value) in &attrs {
            if let Some(index) = parse_tick_key(key) {
                let index = index.map_err(InvalidSnapshotError::ValueError)?;
                ticks.push((index, i128::try_from(value.clone())?));
            } else if is_plugin_module_attribute(key) {
                module_attributes.insert(key.clone(), value.clone());
            }
        }

        Ok(AlgebraIntegralState::new(
            pool,
            decoder_context.chain,
            liquidity,
            sqrt_price,
            tick,
            ticks,
            last_fee,
            PluginSetup { plugin, config, implementation, attributes: module_attributes },
            // Seed value only: the stream decoder points every state at the execution block
            // before the snapshot reaches a consumer.
            BlockContext::new(block.number, block.timestamp),
        ))
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
    use tycho_common::{
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain, ChangeType,
        },
        simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::evm::protocol::algebra_integral::state::test_support::{
        register_handlers, robinhood_module_attributes, robinhood_plugin, token0, token1,
        NAMI_PLUGIN_CONFIG, PLUGIN, PLUGIN_IMPL_ROBINHOOD, SESSION,
    };

    const POOL: &str = "0x1111111111111111111111111111111111111111";

    /// The pool's own attributes, with no plugin.
    fn pool_attributes() -> HashMap<String, Bytes> {
        [
            ("liquidity", Bytes::from(100_u64.to_be_bytes().to_vec())),
            ("sqrt_price_x96", Bytes::from(200_u64.to_be_bytes().to_vec())),
            ("tick", Bytes::from((-300_i32).to_be_bytes().to_vec())),
            ("fee", Bytes::from(3000_u32.to_be_bytes().to_vec())),
            ("ticks/60", Bytes::from(400_i128.to_be_bytes().to_vec())),
            ("ticks/-60", Bytes::from(0_i128.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
    }

    /// The pool's attributes with Nami's Robinhood plugin, as the indexer emits them.
    fn plugin_pool_attributes() -> HashMap<String, Bytes> {
        let mut attributes = pool_attributes();
        attributes.insert("plugin".to_string(), Bytes::from(PLUGIN.to_vec()));
        attributes.insert("plugin_config".to_string(), Bytes::from(vec![NAMI_PLUGIN_CONFIG]));
        attributes.insert("plugin_impl".to_string(), Bytes::from(PLUGIN_IMPL_ROBINHOOD.to_vec()));
        attributes.extend(robinhood_module_attributes());
        attributes
    }

    async fn decode(
        attributes: HashMap<String, Bytes>,
        chain: Option<Chain>,
    ) -> Result<AlgebraIntegralState, InvalidSnapshotError> {
        register_handlers();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: POOL.to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: POOL.to_string(),
                protocol_system: "nami_v4".to_string(),
                protocol_type_name: "nami_v4_pool".to_string(),
                chain: Chain::Robinhood,
                tokens: Vec::new(),
                contract_addresses: Vec::new(),
                static_attributes: HashMap::new(),
                change: ChangeType::Creation,
                creation_tx: Bytes::from(vec![0u8; 32]),
                created_at: DateTime::from_timestamp(1_622_526_000, 0)
                    .expect("a valid timestamp")
                    .naive_utc(),
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        let context = match chain {
            Some(chain) => DecoderContext::new().chain(chain),
            None => DecoderContext::new(),
        };
        let header = BlockHeader { number: 100, timestamp: SESSION, ..Default::default() };
        AlgebraIntegralState::try_from_with_header(
            snapshot,
            header,
            &HashMap::new(),
            &HashMap::new(),
            &context,
        )
        .await
    }

    fn expected(chain: Option<Chain>, plugin: PluginSetup) -> AlgebraIntegralState {
        AlgebraIntegralState::new(
            Address::from_str(POOL).expect("a valid address"),
            chain,
            100,
            alloy::primitives::U256::from(200),
            -300,
            vec![(60, 400), (-60, 0)],
            3000,
            plugin,
            BlockContext::new(100, SESSION),
        )
    }

    #[rstest]
    #[case::without_a_chain(None)]
    #[case::on_a_chain(Some(Chain::Robinhood))]
    #[tokio::test]
    async fn pool_without_a_plugin_decodes_and_keeps_zero_net_liquidity_ticks(
        #[case] chain: Option<Chain>,
    ) {
        let decoded = decode(pool_attributes(), chain)
            .await
            .expect("a pool without a plugin should decode");

        assert_eq!(decoded, expected(chain, PluginSetup::default()));
        assert_eq!(decoded.fee(), 0.003);
    }

    #[tokio::test]
    async fn pool_with_a_registered_plugin_decodes_its_modules() {
        let decoded = decode(plugin_pool_attributes(), Some(Chain::Robinhood))
            .await
            .expect("Nami's Robinhood plugin should decode");

        assert_eq!(decoded, expected(Some(Chain::Robinhood), robinhood_plugin()));
        // The NYSE session's fee, not the pool's stored 0.3%.
        assert_eq!(decoded.fee(), 0.0014);
    }

    /// A failed snapshot decode drops every protocol's state in its message, so a plugin the
    /// simulation cannot reproduce decodes to a pool that is not quoted.
    #[rstest]
    #[case::no_chain(plugin_pool_attributes(), None, "needs the pool's chain")]
    #[case::implementation_of_another_chain(
        plugin_pool_attributes(),
        Some(Chain::Base),
        "no native handler registered"
    )]
    #[case::unknown_fee_source(
        with(plugin_pool_attributes(), "fee_override/source", vec![0xf5; 20]),
        Some(Chain::Robinhood),
        "unsupported fee source"
    )]
    #[case::adaptive_fee(
        with(plugin_pool_attributes(), "dynamic_fee/alpha1", vec![0x05, 0xdc]),
        Some(Chain::Robinhood),
        "adapt to volatility"
    )]
    #[tokio::test]
    async fn pool_whose_plugin_cannot_be_reproduced_decodes_and_is_not_quoted(
        #[case] attributes: HashMap<String, Bytes>,
        #[case] chain: Option<Chain>,
        #[case] reason: &str,
    ) {
        let decoded = decode(attributes, chain)
            .await
            .expect("the pool should decode");

        let error = decoded
            .spot_price(&token0(), &token1())
            .expect_err("the pool should not be quoted");
        assert!(error.to_string().contains(reason), "{error}");
    }

    fn with(
        mut attributes: HashMap<String, Bytes>,
        name: &str,
        value: Vec<u8>,
    ) -> HashMap<String, Bytes> {
        attributes.insert(name.to_string(), Bytes::from(value));
        attributes
    }

    #[rstest]
    #[case::missing_liquidity("liquidity")]
    #[case::missing_sqrt_price("sqrt_price_x96")]
    #[case::missing_tick("tick")]
    #[case::missing_fee("fee")]
    #[tokio::test]
    async fn missing_required_attribute_is_rejected(#[case] missing: &str) {
        let mut attributes = pool_attributes();
        attributes.remove(missing);

        let error = decode(attributes, None)
            .await
            .expect_err("an incomplete pool should not decode");

        assert!(
            matches!(&error, InvalidSnapshotError::MissingAttribute(name) if name == missing),
            "{error:?}"
        );
    }

    #[rstest]
    #[case::tick("tick", vec![0x01; 9])]
    #[case::fee("fee", vec![0x01; 5])]
    #[case::sqrt_price("sqrt_price_x96", vec![0x01; 33])]
    #[case::plugin("plugin", vec![0x01; 3])]
    #[case::tick_index("ticks/x", vec![0x01])]
    #[tokio::test]
    async fn malformed_attribute_is_rejected(#[case] name: &str, #[case] value: Vec<u8>) {
        let mut attributes = pool_attributes();
        attributes.insert(name.to_string(), Bytes::from(value));

        let result = decode(attributes, None).await;

        assert!(result.is_err());
    }
}
