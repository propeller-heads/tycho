//! Ordering the balance deltas a block produces before they reach a store.

use tycho_substreams::prelude::*;

/// The block's deltas sorted by transaction, then by ordinal within it.
///
/// `aggregate_balances_changes` groups consecutive deltas by transaction hash and then collects
/// the groups by hash, so a transaction whose deltas are split around another's keeps only its
/// last run. Deltas gathered in several passes over the block, such as market reserves followed by
/// SY supply, interleave that way unless sorted. The sort is stable, so deltas sharing a
/// transaction and an ordinal keep the order they were pushed in.
pub fn in_transaction_order(mut balance_deltas: Vec<BalanceDelta>) -> BlockBalanceDeltas {
    balance_deltas.sort_by_key(|delta| {
        let tx_index = delta
            .tx
            .as_ref()
            .expect("balance delta without a transaction")
            .index;
        (tx_index, delta.ord)
    });
    BlockBalanceDeltas { balance_deltas }
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::{store_delta::Operation, StoreDelta, StoreDeltas};
    use tycho_substreams::balances::aggregate_balances_changes;

    use super::*;

    const MARKET: &str = "0x34";
    const SY: &str = "0xcb";
    const OTHER_MARKET: &str = "0x56";

    fn tx(index: u64) -> Transaction {
        Transaction { hash: vec![index as u8; 32], index, ..Default::default() }
    }

    fn delta(tx_index: u64, ord: u64, component: &str) -> BalanceDelta {
        BalanceDelta {
            ord,
            tx: Some(tx(tx_index)),
            token: vec![0xaa; 20],
            delta: vec![1],
            component_id: component.as_bytes().to_vec(),
        }
    }

    /// The store delta each balance delta would produce, in the same order.
    fn store_deltas(deltas: &BlockBalanceDeltas) -> StoreDeltas {
        StoreDeltas {
            deltas: deltas
                .balance_deltas
                .iter()
                .map(|delta| StoreDelta {
                    operation: Operation::Update.into(),
                    ordinal: delta.ord,
                    key: format!(
                        "{}:{}",
                        String::from_utf8(delta.component_id.clone()).unwrap(),
                        hex::encode(&delta.token)
                    ),
                    old_value: vec![],
                    new_value: b"1".to_vec(),
                })
                .collect(),
        }
    }

    /// Transaction 1 swaps on a market and mints SY, transaction 2 swaps elsewhere. Gathered
    /// market-first, the deltas run `[1:market, 2:other, 1:SY]`; the aggregation would then keep
    /// only the SY change for transaction 1. Sorted, both of its changes survive.
    #[test]
    fn a_transaction_split_around_another_keeps_every_change() {
        let gathered = vec![delta(1, 10, MARKET), delta(2, 20, OTHER_MARKET), delta(1, 12, SY)];
        let sorted = in_transaction_order(gathered);
        let aggregated = aggregate_balances_changes(store_deltas(&sorted), sorted);

        let (_, first) = &aggregated[&tx(1).hash];
        let mut components: Vec<&[u8]> = first
            .keys()
            .map(Vec::as_slice)
            .collect();
        components.sort();
        assert_eq!(components, vec![MARKET.as_bytes(), SY.as_bytes()]);
        assert_eq!(aggregated[&tx(2).hash].1.len(), 1);
    }

    /// A creation seed is written at ordinal 0 of its transaction and must stay after every
    /// earlier transaction's deltas and before that transaction's later ones.
    #[test]
    fn deltas_run_by_transaction_then_ordinal() {
        let sorted = in_transaction_order(vec![
            delta(2, 30, MARKET),
            delta(1, 12, SY),
            delta(2, 0, SY),
            delta(1, 10, MARKET),
        ]);
        let order: Vec<(u64, u64)> = sorted
            .balance_deltas
            .iter()
            .map(|d| (d.tx.as_ref().unwrap().index, d.ord))
            .collect();
        assert_eq!(order, vec![(1, 10), (1, 12), (2, 0), (2, 30)]);
    }
}
