use std::str::FromStr;

use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::abi::factory::events::Pool;

use tycho_substreams::prelude::*;

/// What differs between the DEXs and chains this package is deployed for.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Params {
    /// The DEX's Algebra factory, whose `Pool` events name its pools.
    factory: String,
    /// Protocol type of the pools, which carries the DEX's name: `nami_v4_pool`.
    protocol_type_name: String,
}

#[substreams::handlers::map]
pub fn map_pools_created(params: String, block: eth::Block) -> BlockEntityChanges {
    let params: Params = serde_qs::from_str(&params).expect("invalid map_pools_created params");
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];
    let factory_address = Address::from_str(&params.factory).expect("Invalid factory address");

    get_new_pools(&block, &mut new_pools, factory_address, &params.protocol_type_name);

    BlockEntityChanges { block: None, changes: new_pools }
}

// Extract new pools from the factory's `Pool` event, which it also emits for custom pools.
//
// An Algebra pool has no fee tier: its fee, tick spacing and plugin are set by pool events
// (`Fee`, `Plugin`, `PluginConfig`) emitted in the creation and initialization transactions, so
// every attribute starts at zero here and is filled in by `map_events`.
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    factory_address: Address,
    protocol_type_name: &str,
) {
    let mut on_pool_created = |event: Pool, tx: &eth::TransactionTrace, _log: &eth::Log| {
        let tycho_tx: Transaction = tx.into();
        let zero_attribute = |name: &str, value: Vec<u8>| Attribute {
            name: name.to_string(),
            value,
            change: ChangeType::Creation.into(),
        };

        new_pools.push(TransactionEntityChanges {
            tx: Some(tycho_tx),
            entity_changes: vec![EntityChanges {
                component_id: event.pool.to_hex(),
                attributes: vec![
                    zero_attribute("liquidity", BigInt::from(0).to_bytes_be().1),
                    zero_attribute("tick", BigInt::from(0).to_signed_bytes_be()),
                    zero_attribute("sqrt_price_x96", BigInt::from(0).to_bytes_be().1),
                    zero_attribute("fee", BigInt::from(0).to_bytes_be().1),
                    zero_attribute("plugin_config", BigInt::from(0).to_bytes_be().1),
                ],
            }],
            component_changes: vec![ProtocolComponent {
                id: event.pool.to_hex(),
                tokens: vec![event.token0, event.token1],
                contracts: vec![],
                static_att: vec![],
                change: ChangeType::Creation.into(),
                protocol_type: Some(ProtocolType {
                    name: protocol_type_name.to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
            }],
            balance_changes: vec![],
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![factory_address]);

    eh.on::<Pool, _>(&mut on_pool_created);
    eh.handle_events();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_name_the_factory_and_the_protocol_type() {
        let params: Params = serde_qs::from_str(
            "factory=0xDeDB80903df19f9dB9b393eb66E07511CB9ABdA2&protocol_type_name=nami_v4_pool",
        )
        .expect("the params should parse");

        assert_eq!(
            params,
            Params {
                factory: "0xDeDB80903df19f9dB9b393eb66E07511CB9ABdA2".to_string(),
                protocol_type_name: "nami_v4_pool".to_string(),
            }
        );
    }

    #[test]
    fn unknown_param_is_rejected() {
        let params = serde_qs::from_str::<Params>("factory=0x00&protocol_type_name=x&dci=true");

        assert!(params.is_err());
    }
}
