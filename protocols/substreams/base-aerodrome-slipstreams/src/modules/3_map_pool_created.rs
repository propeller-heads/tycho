use crate::{
    abi::factory::events::PoolCreated,
    modules::utils::{tick_spacing_fee_key, Params},
};
use ethabi::ethereum_types::Address;
use std::str::FromStr;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetInt64},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};
use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
    tick_spacing_to_fee_store: StoreGetInt64,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionChanges> = vec![];
    let params = Params::parse_from_query(&params)?;
    let factory_addresses = params
        .factories
        .iter()
        .map(|f| Address::from_str(f).expect("invalid address"))
        .collect::<Vec<_>>();
    let creation_dynamic_fee_module = params.creation_dynamic_fee_module_address()?;
    get_new_pools(
        &block,
        &mut new_pools,
        factory_addresses,
        &tick_spacing_to_fee_store,
        &params.protocol_type_name,
        creation_dynamic_fee_module.as_deref(),
    );

    Ok(BlockChanges { block: Some((&block).into()), changes: new_pools, ..Default::default() })
}

// Extract new pools from PoolCreated events
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionChanges>,
    factory_addresses: Vec<Address>,
    tick_spacing_to_fee_store: &impl StoreGet<i64>,
    protocol_type_name: &str,
    creation_dynamic_fee_module: Option<&[u8]>,
) {
    // Extract new pools from PoolCreated events
    let mut on_pool_created = |event: PoolCreated, _tx: &eth::TransactionTrace, log: &eth::Log| {
        let tycho_tx: Transaction = _tx.into();
        let fee_key = tick_spacing_fee_key(&log.address, event.tick_spacing.to_i32());
        // A factory enables a tick spacing before it can create a pool on it, so a module started
        // at the package's initial block always has the fee. A module started later misses the
        // enabling event; skipping keeps the stream alive and avoids publishing a wrong
        // default_fee, and any test asserting the pool still fails on the missing component.
        let Some(default_fee) = tick_spacing_to_fee_store.get_last(&fee_key) else {
            return;
        };
        let mut attributes = vec![
            Attribute {
                name: "liquidity".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "tick".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "sqrt_price_x96".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "observationIndex".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "observationCardinality".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "dfc_baseFee".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "dfc_scalingFactor".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "dfc_feeCap".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "dfc_initialFeeEnabled".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "dfc_initialFee".to_string(),
                value: BigInt::from(0).to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            },
        ];
        if let Some(module) = creation_dynamic_fee_module {
            attributes.push(Attribute {
                name: "dynamic_fee_module".to_string(),
                value: module.to_vec(),
                change: ChangeType::Creation.into(),
            });
        }
        new_pools.push(TransactionChanges {
            tx: Some(tycho_tx.clone()),
            contract_changes: vec![],
            entity_changes: vec![EntityChanges { component_id: event.pool.to_hex(), attributes }],
            component_changes: vec![ProtocolComponent {
                id: event.pool.to_hex(),
                tokens: vec![event.token0, event.token1],
                contracts: vec![],
                static_att: vec![
                    Attribute {
                        name: "default_fee".to_string(),
                        value: BigInt::from(default_fee).to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "tick_spacing".to_string(),
                        value: event.tick_spacing.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "pool_address".to_string(),
                        value: event.pool,
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory".to_string(),
                        value: log.address.to_hex().into_bytes(),
                        change: ChangeType::Creation.into(),
                    },
                ],
                change: i32::from(ChangeType::Creation),
                protocol_type: Option::from(ProtocolType {
                    name: protocol_type_name.to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Custom.into(),
                }),
            }],
            balance_changes: vec![],
            ..Default::default()
        })
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(factory_addresses);

    eh.on::<PoolCreated, _>(&mut on_pool_created);
    eh.handle_events();
}

#[cfg(test)]
mod tests {
    use super::get_new_pools;
    use crate::modules::utils::tick_spacing_fee_key;
    use ethabi::ethereum_types::Address;
    use substreams::{scalar::BigInt, store::StoreGet};
    use substreams_ethereum::pb::eth::v2::{self as eth};
    use tiny_keccak::{Hasher, Keccak};
    use tycho_substreams::prelude::*;

    const FACTORY: [u8; 20] = [0x54; 20];
    const POOL: [u8; 20] = [0x75; 20];
    const MODULE: [u8; 20] = [0x86; 20];
    const TICK_SPACING: i32 = 200;

    /// The tick spacing fee store with a fee for `FACTORY`'s `TICK_SPACING` only.
    struct TickSpacingFees;

    impl StoreGet<i64> for TickSpacingFees {
        fn new(_idx: u32) -> Self {
            Self
        }

        fn get_at<K: AsRef<str>>(&self, _ord: u64, key: K) -> Option<i64> {
            self.get_last(key)
        }

        fn get_last<K: AsRef<str>>(&self, key: K) -> Option<i64> {
            (key.as_ref() == tick_spacing_fee_key(&FACTORY, TICK_SPACING)).then_some(3000)
        }

        fn get_first<K: AsRef<str>>(&self, key: K) -> Option<i64> {
            self.get_last(key)
        }

        fn has_at<K: AsRef<str>>(&self, _ord: u64, key: K) -> bool {
            self.get_last(key).is_some()
        }

        fn has_last<K: AsRef<str>>(&self, key: K) -> bool {
            self.get_last(key).is_some()
        }

        fn has_first<K: AsRef<str>>(&self, key: K) -> bool {
            self.get_last(key).is_some()
        }
    }

    fn left_padded(bytes: &[u8]) -> Vec<u8> {
        let mut word = vec![0; 32 - bytes.len()];
        word.extend_from_slice(bytes);
        word
    }

    fn block_creating_a_pool() -> eth::Block {
        let mut topic = [0u8; 32];
        let mut keccak = Keccak::v256();
        keccak.update(b"PoolCreated(address,address,int24,address)");
        keccak.finalize(&mut topic);
        let log = eth::Log {
            address: FACTORY.to_vec(),
            topics: vec![
                topic.to_vec(),
                left_padded(&[0x11; 20]),
                left_padded(&[0x22; 20]),
                left_padded(&TICK_SPACING.to_be_bytes()),
            ],
            data: left_padded(&POOL),
            ..Default::default()
        };
        eth::Block {
            number: 1,
            transaction_traces: vec![eth::TransactionTrace {
                hash: vec![0xaa; 32],
                status: eth::TransactionTraceStatus::Succeeded.into(),
                receipt: Some(eth::TransactionReceipt { logs: vec![log], ..Default::default() }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn created_pool_attributes(creation_dynamic_fee_module: Option<&[u8]>) -> Vec<Attribute> {
        let mut new_pools = vec![];
        get_new_pools(
            &block_creating_a_pool(),
            &mut new_pools,
            vec![Address::from(FACTORY)],
            &TickSpacingFees,
            "raphael_slipstreams_pool",
            creation_dynamic_fee_module,
        );
        let [pool] = new_pools.as_slice() else {
            panic!("expected one created pool, got {}", new_pools.len());
        };
        let [entity] = pool.entity_changes.as_slice() else {
            panic!("expected one entity change, got {}", pool.entity_changes.len());
        };
        entity.attributes.clone()
    }

    fn zero_creation_attribute(name: &str) -> Attribute {
        Attribute {
            name: name.to_string(),
            value: BigInt::from(0).to_signed_bytes_be(),
            change: ChangeType::Creation.into(),
        }
    }

    fn initial_state_attributes() -> Vec<Attribute> {
        [
            "liquidity",
            "tick",
            "sqrt_price_x96",
            "observationIndex",
            "observationCardinality",
            "dfc_baseFee",
            "dfc_scalingFactor",
            "dfc_feeCap",
            "dfc_initialFeeEnabled",
            "dfc_initialFee",
        ]
        .into_iter()
        .map(zero_creation_attribute)
        .collect()
    }

    #[test]
    fn a_pool_created_without_a_marker_has_no_dynamic_fee_module() {
        assert_eq!(created_pool_attributes(None), initial_state_attributes());
    }

    #[test]
    fn a_pool_created_with_a_marker_carries_the_raw_module_address() {
        let mut expected = initial_state_attributes();
        expected.push(Attribute {
            name: "dynamic_fee_module".to_string(),
            value: MODULE.to_vec(),
            change: ChangeType::Creation.into(),
        });

        assert_eq!(created_pool_attributes(Some(&MODULE)), expected);
    }
}
