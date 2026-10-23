use crate::{common::*, config::DeploymentConfig, pb::tessera::v1::*};
use anyhow::Result;
use std::collections::HashSet;
use substreams::store::{StoreGet, StoreGetArray};
use substreams_ethereum::pb::eth;

#[substreams::handlers::map]
pub fn map_storage_changes(
    params: String,
    block: eth::v2::Block,
    pair_store: StoreGetArray<String>,
) -> Result<BlockStorageChanges> {
    let config = DeploymentConfig::parse(&params)?;
    // The pair list already holds the Pairs discovered in this block. Attribute emission
    // downstream checks creation transaction indices, so an earlier transaction cannot
    // update a new Pair.
    let known = pair_store
        .get_last("pairs")
        .unwrap_or_default()
        .into_iter()
        .map(|pair| hex::decode(pair.trim_start_matches("0x")))
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(filter_storage_changes(&config, &block, &known))
}

/// Retain all stateful-contract writes, not only dependency slots: arbitrary Pair/Engine
/// writes invalidate quotes. Keep candidate fee writes before a fee module is assigned too.
/// Filtering uses storage owners (including delegatecalls), never transaction destinations.
pub(crate) fn filter_storage_changes(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    known: &HashSet<Vec<u8>>,
) -> BlockStorageChanges {
    let transactions = block
        .transactions()
        .filter_map(|tx| {
            let mut writes: Vec<_> = tx
                .calls
                .iter()
                .filter(|call| !call.state_reverted)
                .flat_map(|call| &call.storage_changes)
                .filter(|w| {
                    *w.address == config.tesseraswap ||
                        *w.address == config.engine ||
                        *w.key == FEE_TAG_ZERO_SLOT ||
                        known.contains(&w.address)
                })
                .collect();
            if writes.is_empty() {
                return None;
            }
            // Call vectors follow call entry order, not write execution order. Keep every
            // matching write (including clears); consumers need both old values and ordering.
            writes.sort_by_key(|w| w.ordinal);
            Some(TransactionStorageChanges {
                tx: Some(tx.into()),
                writes: writes
                    .into_iter()
                    .map(|w| StorageChange {
                        address: w.address.clone(),
                        key: w.key.clone(),
                        old_value: w.old_value.clone(),
                        new_value: w.new_value.clone(),
                        ordinal: w.ordinal,
                    })
                    .collect(),
            })
        })
        .collect();
    BlockStorageChanges { transactions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{common::testing::slot, config::TEST_CONFIG};

    fn write(
        address: impl AsRef<[u8]>,
        key: impl AsRef<[u8]>,
        ordinal: u64,
    ) -> eth::v2::StorageChange {
        eth::v2::StorageChange {
            address: address.as_ref().to_vec(),
            key: key.as_ref().to_vec(),
            ordinal,
            old_value: slot(7).to_vec(),
            new_value: slot(0).to_vec(),
        }
    }

    fn transaction(calls: Vec<eth::v2::Call>) -> eth::v2::TransactionTrace {
        eth::v2::TransactionTrace {
            status: 1,
            index: 3,
            hash: vec![9; 32],
            calls,
            ..Default::default()
        }
    }

    #[test]
    fn filters_noise_and_keeps_all_stateful_writes_and_unassigned_tag_zero_fee() {
        let c = TEST_CONFIG;
        let pair = vec![4; 20];
        let unknown = vec![5; 20];
        let mut writes: Vec<_> = (0..100)
            .map(|n| write(unknown.clone(), slot(n), n))
            .collect();
        let relevant = vec![
            write(c.tesseraswap, slot(1), 101),
            write(c.engine, slot(999), 102),
            write(pair.clone(), slot(888), 103),
            write(unknown, FEE_TAG_ZERO_SLOT, 104),
        ];
        writes.extend(relevant.clone());
        let block = eth::v2::Block {
            transaction_traces: vec![transaction(vec![eth::v2::Call {
                storage_changes: writes,
                ..Default::default()
            }])],
            ..Default::default()
        };
        let out = filter_storage_changes(&c, &block, &HashSet::from([pair]));
        assert_eq!(out.transactions.len(), 1);
        let group = &out.transactions[0];
        assert_eq!(group.tx.as_ref().unwrap().index, 3);
        assert_eq!(group.tx.as_ref().unwrap().hash, vec![9; 32]);
        assert_eq!(group.writes.len(), relevant.len());
        for (actual, expected) in group.writes.iter().zip(relevant) {
            assert_eq!(actual.address, expected.address);
            assert_eq!(actual.key, expected.key);
            assert_eq!(actual.old_value, expected.old_value);
            assert_eq!(actual.new_value, expected.new_value);
            assert_eq!(actual.ordinal, expected.ordinal);
        }
    }

    #[test]
    fn preserves_nested_write_order_and_clears_but_excludes_reverts() {
        let c = TEST_CONFIG;
        let w = |n| write(c.tesseraswap, slot(1), n);
        let block = eth::v2::Block {
            transaction_traces: vec![transaction(vec![
                eth::v2::Call { storage_changes: vec![w(10), w(30)], ..Default::default() },
                eth::v2::Call { storage_changes: vec![w(20)], ..Default::default() },
                eth::v2::Call {
                    storage_changes: vec![w(40)],
                    state_reverted: true,
                    ..Default::default()
                },
            ])],
            ..Default::default()
        };
        let out = filter_storage_changes(&c, &block, &HashSet::new());
        assert_eq!(
            out.transactions[0]
                .writes
                .iter()
                .map(|w| w.ordinal)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn omits_failed_and_irrelevant_transactions() {
        let c = TEST_CONFIG;
        let mut failed = transaction(vec![eth::v2::Call {
            storage_changes: vec![write(c.tesseraswap, slot(1), 1)],
            ..Default::default()
        }]);
        failed.status = 2;
        let unrelated = transaction(vec![eth::v2::Call {
            storage_changes: vec![write(vec![8; 20], slot(52), 2)],
            ..Default::default()
        }]);
        let block =
            eth::v2::Block { transaction_traces: vec![failed, unrelated], ..Default::default() };
        assert!(filter_storage_changes(&c, &block, &HashSet::new())
            .transactions
            .is_empty());
    }
}
