use crate::{
    events::get_log_changed_attributes,
    modules::{
        fee_settings::{creation_attributes, fee_changes, FeeLookups, StoreFeeLookups},
        utils::Params,
    },
    pb::tycho::evm::aerodrome::Pool,
};

use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetProto, StoreGetRaw, StoreGetString},
};
use substreams_ethereum::pb::eth::v2::{self as eth, StorageChange};
use substreams_helper::hex::Hexable;
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

#[allow(clippy::too_many_arguments)]
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    protocol_components: BlockChanges,
    pools_store: StoreGetProto<Pool>,
    pool_fee_slots: StoreGetString,
    pool_ids: StoreGetString,
    fee_settings: StoreGetRaw,
    balance_store: StoreDeltas,
    balance_deltas: BlockBalanceDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let lookups = StoreFeeLookups { pools: &pools_store, pool_fee_slots, pool_ids, fee_settings };
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    let created_in = add_pool_creations(protocol_components, &lookups, &mut transaction_changes);

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

    add_pool_storage_changes(&block, &pools_store, &mut transaction_changes);

    for change in fee_changes(&params, &block, &created_in, &lookups) {
        transaction_changes
            .entry(change.tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&change.tx))
            .add_entity_change(&EntityChanges {
                component_id: change.pool,
                attributes: change.attributes,
            });
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

/// Adds this block's new pools with their fee attributes, returning each new pool's transaction
/// index.
fn add_pool_creations(
    protocol_components: BlockChanges,
    lookups: &dyn FeeLookups,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> HashMap<String, u64> {
    let mut created_in = HashMap::new();
    for change in protocol_components.changes {
        let tx = change
            .tx
            .as_ref()
            .expect("pool creation without transaction");
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &change.component_changes {
            builder.add_protocol_component(component);
            builder.add_entity_change(&EntityChanges {
                component_id: component.id.clone(),
                attributes: creation_attributes(lookups),
            });
            created_in.insert(component.id.clone(), tx.index);
        }
        for entity_change in &change.entity_changes {
            builder.add_entity_change(entity_change);
        }
    }
    created_in
}

/// Pool state (slot0, liquidity, ticks, observations) from the pool's storage writes in each
/// transaction that emitted a pool event.
///
/// Writes are collected from every call frame of the transaction, not only the frame that emitted
/// the event: v1.1 pools run `mint` and `burn` in a delegatecall to `CLPoolNonSwapPath`, so their
/// writes land in a child frame of the pool's call.
fn add_pool_storage_changes(
    block: &eth::Block,
    pools_store: &StoreGetProto<Pool>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for trx in block.transactions() {
        let tx: Transaction = trx.into();
        let writes = storage_writes_by_address(trx);
        for log in trx
            .logs_with_calls()
            .map(|(log, _)| log)
        {
            let Some(pool) = pools_store.get_last(format!("Pool:{}", log.address.to_hex())) else {
                continue;
            };
            let Some(pool_writes) = writes.get(&pool.address) else {
                continue;
            };
            let changed_attributes = get_log_changed_attributes(
                log,
                pool_writes,
                pool.address
                    .as_slice()
                    .try_into()
                    .expect("Pool address is not 20 bytes long"),
            );
            if changed_attributes.is_empty() {
                continue;
            }
            transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                .add_entity_change(&EntityChanges {
                    component_id: pool.address.to_hex(),
                    attributes: changed_attributes,
                });
        }
    }
}

/// The transaction's storage writes outside reverted calls, by storage owner, in write order.
pub(crate) fn storage_writes_by_address(
    trx: &eth::TransactionTrace,
) -> HashMap<Vec<u8>, Vec<StorageChange>> {
    let mut writes: HashMap<Vec<u8>, Vec<StorageChange>> = HashMap::new();
    for change in trx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .sorted_by_key(|change| change.ordinal)
    {
        writes
            .entry(change.address.clone())
            .or_default()
            .push(change.clone());
    }
    writes
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{Call, StorageChange, TransactionTrace};

    use super::storage_writes_by_address;

    fn write(address: u8, ordinal: u64) -> StorageChange {
        StorageChange { address: vec![address; 20], ordinal, ..Default::default() }
    }

    #[test]
    fn collects_writes_from_every_frame_in_write_order() {
        let mut reverted = Call { storage_changes: vec![write(1, 5)], ..Default::default() };
        reverted.state_reverted = true;
        let trx = TransactionTrace {
            calls: vec![
                // The pool frame, and the delegatecall frame where `mint` writes the pool's
                // storage.
                Call { storage_changes: vec![write(1, 9)], ..Default::default() },
                Call { storage_changes: vec![write(1, 3), write(2, 4)], ..Default::default() },
                reverted,
            ],
            ..Default::default()
        };

        let writes = storage_writes_by_address(&trx);

        let ordinals: Vec<u64> = writes[&vec![1u8; 20]]
            .iter()
            .map(|change| change.ordinal)
            .collect();
        assert_eq!(ordinals, vec![3, 9]);
        assert_eq!(writes[&vec![2u8; 20]].len(), 1);
    }
}
