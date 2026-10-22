use std::collections::HashMap;

use base_pancakeswap_infinity_cl::pb::pancakeswap::infinity::cl::Pool;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::prelude::*;

use super::{
    alpha::{decode_start_update, parse_hooks, START_TIMESTAMP_ATTRIBUTE},
    store_alpha_start_times::HookParams,
};

/// An `alpha_start_timestamp` update for every `PoolStartedAtUpdated` on a pool that exists by
/// the time of the event.
///
/// An event for a pool not yet initialized is not an update: `map_alpha_pools_created` reads it
/// from the store when the pool is created. That includes a pool created later in this block, in
/// a later transaction, which `store_pools` already lists.
#[substreams::handlers::map]
pub fn map_alpha_start_updates(
    params: String,
    block: eth::Block,
    pools: StoreGetProto<Pool>,
    created: BlockEntityChanges,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let params: HookParams = serde_qs::from_str(&params).map_err(|err| {
        anyhow::anyhow!("Invalid map_alpha_start_updates params {params:?}: {err}")
    })?;
    let hooks = parse_hooks(&params.hooks)?;

    let updates = start_updates(&block, &hooks, &pools, &created);
    Ok(BlockEntityChanges { block: None, changes: updates })
}

/// The updates `map_alpha_start_updates` emits, against any `store_pools` view.
fn start_updates<S: StoreGet<Pool>>(
    block: &eth::Block,
    hooks: &[Vec<u8>],
    pools: &S,
    created: &BlockEntityChanges,
) -> Vec<TransactionEntityChanges> {
    // Transaction index each pool created in this block was created in.
    let mut created_in: HashMap<String, u64> = HashMap::new();
    for change in &created.changes {
        let index = change
            .tx
            .as_ref()
            .map_or(0, |tx| tx.index);
        for component in &change.component_changes {
            created_in.insert(component.id.clone(), index);
        }
    }

    let mut updates = vec![];
    for tx in block
        .transaction_traces
        .iter()
        .filter(|tx| tx.status == 1)
    {
        let Some(receipt) = tx.receipt.as_ref() else { continue };
        let index = u64::from(tx.index);
        for log in &receipt.logs {
            let Some((pool_id, start)) = decode_start_update(log, hooks) else { continue };
            if pools
                .get_last(format!("pool:{pool_id}"))
                .is_none() ||
                created_in
                    .get(&pool_id)
                    .is_some_and(|created| *created > index)
            {
                continue;
            }
            updates.push(TransactionEntityChanges {
                tx: Some(tx.into()),
                entity_changes: vec![EntityChanges {
                    component_id: pool_id,
                    attributes: vec![Attribute {
                        name: START_TIMESTAMP_ATTRIBUTE.to_string(),
                        value: BigInt::from_unsigned_bytes_be(&start).to_signed_bytes_be(),
                        change: ChangeType::Update.into(),
                    }],
                }],
                component_changes: vec![],
                balance_changes: vec![],
            });
        }
    }

    updates
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{Log, TransactionReceipt, TransactionTrace};

    use super::*;
    use crate::variant_modules::{alpha::POOL_STARTED_AT_UPDATED, test_store::FakeStore};

    const HOOK: [u8; 20] = [0xb0; 20];
    const POOL_ID: [u8; 32] = [0xab; 32];

    fn pool_id() -> String {
        format!("0x{}", "ab".repeat(32))
    }

    fn start_update_tx(index: u32, start: u64) -> TransactionTrace {
        let mut data = vec![0u8; 64];
        data[24..32].copy_from_slice(&start.to_be_bytes());
        TransactionTrace {
            index,
            status: 1,
            receipt: Some(TransactionReceipt {
                logs: vec![Log {
                    address: HOOK.to_vec(),
                    topics: vec![POOL_STARTED_AT_UPDATED.to_vec(), POOL_ID.to_vec()],
                    data,
                    ordinal: u64::from(index) * 10,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn known_pools() -> FakeStore<Pool> {
        FakeStore::with(vec![(format!("pool:{}", pool_id()).as_str(), 0, Pool::default())])
    }

    fn created_in_tx(index: u64) -> BlockEntityChanges {
        BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                tx: Some(Transaction { index, ..Default::default() }),
                component_changes: vec![ProtocolComponent { id: pool_id(), ..Default::default() }],
                ..Default::default()
            }],
        }
    }

    fn updates(
        txs: Vec<TransactionTrace>,
        pools: &FakeStore<Pool>,
        created: &BlockEntityChanges,
    ) -> Vec<u64> {
        let block = eth::Block { transaction_traces: txs, ..Default::default() };
        start_updates(&block, &[HOOK.to_vec()], pools, created)
            .into_iter()
            .map(|change| {
                assert_eq!(change.entity_changes[0].component_id, pool_id());
                let attribute = &change.entity_changes[0].attributes[0];
                assert_eq!(attribute.name, START_TIMESTAMP_ATTRIBUTE);
                assert_eq!(attribute.change, i32::from(ChangeType::Update));
                BigInt::from_signed_bytes_be(&attribute.value).to_u64()
            })
            .collect()
    }

    /// A post-launch reschedule on a live pool, as the first hook version allows.
    #[test]
    fn existing_pool_gets_every_update() {
        let got = updates(
            vec![start_update_tx(1, 7_200), start_update_tx(2, 0)],
            &known_pools(),
            &BlockEntityChanges::default(),
        );
        assert_eq!(got, vec![7_200, 0]);
    }

    /// The first hook version sets the start time before `initialize`; that value reaches the
    /// pool through its creation, not as an update to a pool that does not exist yet.
    #[test]
    fn unknown_pool_gets_no_update() {
        let got = updates(
            vec![start_update_tx(1, 7_200)],
            &FakeStore::with(vec![]),
            &BlockEntityChanges::default(),
        );
        assert!(got.is_empty());
    }

    #[test]
    fn pool_created_later_in_the_block_gets_no_update_from_before_its_creation() {
        let got = updates(
            vec![start_update_tx(1, 100), start_update_tx(5, 200)],
            &known_pools(),
            &created_in_tx(3),
        );
        assert_eq!(got, vec![200], "only the event after the creating transaction is an update");
    }

    #[test]
    fn reverted_transactions_are_ignored() {
        let mut tx = start_update_tx(1, 7_200);
        tx.status = 2;
        assert!(updates(vec![tx], &known_pools(), &BlockEntityChanges::default()).is_empty());
    }
}
