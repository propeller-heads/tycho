use crate::{
    events::get_log_changed_attributes,
    modules::utils::{
        dynamic_fee_config_initialized_key, dynamic_fee_config_key,
        should_process_dynamic_fee_config, DynamicFeeEvent, Params, DYNAMIC_FEE_CONFIG_ATTRIBUTES,
    },
    pb::tycho::evm::aerodrome::Pool,
};

use itertools::Itertools;
use std::{collections::HashMap, vec};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetBigInt, StoreGetProto},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

fn is_first_dynamic_fee_config_event(deltas: &StoreDeltas, ordinal: u64, pool: &[u8]) -> bool {
    deltas.deltas.iter().any(|delta| {
        delta.ordinal == ordinal &&
            delta.key == dynamic_fee_config_initialized_key(pool) &&
            delta.old_value.is_empty()
    })
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    protocol_components: BlockChanges,
    pools_store: StoreGetProto<Pool>,
    dynamic_fee_config_deltas: StoreDeltas,
    dynamic_fee_config_store: StoreGetBigInt,
    balance_store: StoreDeltas,
    balance_deltas: BlockBalanceDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let dynamic_fee_modules = params
        .dynamic_fee_modules
        .iter()
        .map(|f| hex::decode(f).expect("Invalid dynamic_fee_module hex"))
        .collect::<Vec<Vec<u8>>>();
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    for change in protocol_components.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        change
            .component_changes
            .iter()
            .for_each(|c| {
                builder.add_protocol_component(c);
            });
        change
            .entity_changes
            .iter()
            .for_each(|c| {
                builder.add_entity_change(c);
            });
    }

    aggregate_balances_changes(balance_store, balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    for trx in block.transactions() {
        let tx = Transaction {
            to: trx.to.clone(),
            from: trx.from.clone(),
            hash: trx.hash.clone(),
            index: trx.index.into(),
        };
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));

        for (log, call_view) in trx.logs_with_calls() {
            if let Some(pool) = pools_store.get_last(format!("{}:{}", "Pool", log.address.to_hex()))
            {
                let changed_attributes = get_log_changed_attributes(
                    log,
                    &call_view.call.storage_changes,
                    pool.address
                        .clone()
                        .as_slice()
                        .try_into()
                        .expect("Pool address is not 20 bytes long"),
                );
                if !changed_attributes.is_empty() {
                    builder.add_entity_change(&EntityChanges {
                        component_id: pool.address.clone().to_hex(),
                        attributes: changed_attributes,
                    });
                }
            }
            if should_process_dynamic_fee_config(block.number) &&
                dynamic_fee_modules.contains(&log.address)
            {
                let Some(event) = DynamicFeeEvent::match_and_decode(log) else {
                    continue;
                };
                let pool = event.pool();
                let pool_key = format!("Pool:{}", pool.to_hex());
                if pools_store
                    // store_pools writes current-block creations at ordinal zero, so get_at also
                    // recognizes a pool whose first fee event occurs in its creation block.
                    .get_at(log.ordinal, &pool_key)
                    .is_none()
                {
                    continue;
                }

                let is_first_event = is_first_dynamic_fee_config_event(
                    &dynamic_fee_config_deltas,
                    log.ordinal,
                    pool,
                );
                let attributes = if is_first_event {
                    // During the database rollback and replay, a pool's first configured-module
                    // event is its migration boundary. Emit every field with the new module marker
                    // once so fields absent from the current module explicitly replace stale
                    // retired-module values with zero. The same rule also initializes pools first
                    // configured after the replay has caught up.
                    DYNAMIC_FEE_CONFIG_ATTRIBUTES
                        .into_iter()
                        .map(|attribute| Attribute {
                            name: attribute.into(),
                            value: dynamic_fee_config_store
                                .get_at(log.ordinal, dynamic_fee_config_key(pool, attribute))
                                .unwrap_or_default()
                                .to_signed_bytes_be(),
                            change: ChangeType::Update.into(),
                        })
                        .chain(std::iter::once(Attribute {
                            name: "dynamic_fee_module".into(),
                            value: log.address.clone(),
                            change: ChangeType::Update.into(),
                        }))
                        .collect()
                } else {
                    event
                        .config_updates()
                        .into_iter()
                        .map(|(attribute, value)| Attribute {
                            name: attribute.into(),
                            value: value.to_signed_bytes_be(),
                            change: ChangeType::Update.into(),
                        })
                        .collect()
                };
                builder
                    .add_entity_change(&EntityChanges { component_id: pool.to_hex(), attributes });
            }
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::{StoreDelta, StoreDeltas};

    use super::{dynamic_fee_config_initialized_key, is_first_dynamic_fee_config_event};

    #[test]
    fn only_the_first_pool_event_requests_a_complete_snapshot() {
        let pool = [0x33; 20];
        let mut deltas = StoreDeltas {
            deltas: vec![StoreDelta {
                ordinal: 42,
                key: dynamic_fee_config_initialized_key(&pool),
                old_value: Vec::new(),
                new_value: b"1".to_vec(),
                ..Default::default()
            }],
        };

        assert!(is_first_dynamic_fee_config_event(&deltas, 42, &pool));
        assert!(!is_first_dynamic_fee_config_event(&deltas, 43, &pool));

        deltas.deltas[0].old_value = b"1".to_vec();
        assert!(!is_first_dynamic_fee_config_event(&deltas, 42, &pool));
    }
}
