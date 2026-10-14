use crate::{common::*, config::DeploymentConfig, pb::tessera::v1::*};
use anyhow::Result;
use std::collections::HashSet;
use substreams::store::{StoreGet, StoreGetString};
use substreams_ethereum::pb::eth;

#[substreams::handlers::map]
pub fn map_storage_changes(
    params: String,
    block: eth::v2::Block,
    pair_store: StoreGetString,
) -> Result<BlockStorageChanges> {
    let config = DeploymentConfig::parse(&params)?;
    // get_last includes this block's discoveries. Downstream attribute emission separately
    // checks creation transaction indices so an earlier transaction cannot update a new pair.
    let known = pairs(&pair_store, "pairs")
        .into_iter()
        .map(|pair| hex::decode(pair.trim_start_matches("0x")))
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(filter_storage_changes(&config, &block, &known))
}

/// Retain all stateful-contract writes, not only dependency slots: arbitrary Pair/Engine
/// writes invalidate quotes. Keep candidate fee writes before a helper is assigned too.
/// Filtering uses storage owners (including delegatecalls), never transaction destinations.
pub(crate) fn filter_storage_changes(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    known: &HashSet<Vec<u8>>,
) -> BlockStorageChanges {
    let fee_key = fee_tag_zero_slot();
    let transactions = block
        .transactions()
        .filter_map(|tx| {
            let mut writes: Vec<_> = tx
                .calls
                .iter()
                .filter(|call| !call.state_reverted)
                .flat_map(|call| &call.storage_changes)
                .filter(|w| {
                    w.address == config.tesseraswap ||
                        w.address == config.engine ||
                        w.key == fee_key ||
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
    use prost::Message;

    fn config() -> DeploymentConfig {
        DeploymentConfig::parse("tesseraswap=1111111111111111111111111111111111111111&engine=2222222222222222222222222222222222222222&treasury=3333333333333333333333333333333333333333&treasury_slot=1&pair_map_slot=8&pair_base_token_slot=48&pair_quote_token_slot=49&pair_lib_slot=51&pair_write_helper_slot=52").unwrap()
    }

    fn write(address: Vec<u8>, key: Vec<u8>, ordinal: u64) -> eth::v2::StorageChange {
        eth::v2::StorageChange { address, key, ordinal, old_value: slot(7), new_value: slot(0) }
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
    fn filters_noise_and_keeps_all_stateful_writes_and_unassigned_helper_fee() {
        let c = config();
        let pair = vec![4; 20];
        let unknown = vec![5; 20];
        let mut writes: Vec<_> = (0..100)
            .map(|n| write(unknown.clone(), slot(n), n))
            .collect();
        let relevant = vec![
            write(c.tesseraswap.clone(), slot(1), 101),
            write(c.engine.clone(), slot(999), 102),
            write(pair.clone(), slot(888), 103),
            write(unknown, fee_tag_zero_slot(), 104),
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
        assert_eq!(BlockStorageChanges::decode(out.encode_to_vec().as_slice()).unwrap(), out);
    }

    #[test]
    fn preserves_nested_write_order_and_clears_but_excludes_reverts() {
        let c = config();
        let w = |n| write(c.tesseraswap.clone(), slot(1), n);
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
        assert!(zero(
            &out.transactions[0]
                .writes
                .last()
                .unwrap()
                .new_value
        ));
    }

    #[test]
    fn omits_failed_and_irrelevant_transactions() {
        let c = config();
        let mut failed = transaction(vec![eth::v2::Call {
            storage_changes: vec![write(c.tesseraswap.clone(), slot(1), 1)],
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
