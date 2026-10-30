//! BinPoolManager storage writes, indexed the way `map_tree_changes` and `map_bin_changes` read
//! them: by transaction, then by slot, and attributed to an event by the window between the
//! previous BinPoolManager log in the same transaction and the event's own log.

use std::collections::{HashMap, HashSet};

use substreams_ethereum::pb::eth::v2::{self as eth, StorageChange};

use crate::pb::pancakeswap::infinity::bin::events::PoolEvent;

/// Every write to one slot, in call order. Not sorted: a nested call's writes sit between its
/// caller's, so readers take the min and max ordinal rather than the first and last entry.
pub(crate) type WritesBySlot<'a> = HashMap<Vec<u8>, Vec<&'a StorageChange>>;

/// Successful, non-reverted writes to `pool_manager` in the transactions `events` came from.
///
/// Per transaction because two transactions in one block can write the same slot; every write
/// because one transaction can mint into a bin then swap through it. Transactions without an
/// event are skipped: nothing reads their writes, and most blocks have no event at all.
pub(crate) fn storage_writes_by_tx<'a>(
    block: &'a eth::Block,
    pool_manager: &[u8],
    events: &[PoolEvent],
) -> HashMap<u64, WritesBySlot<'a>> {
    let with_events: HashSet<u64> = events.iter().map(tx_index).collect();
    let mut writes_by_tx: HashMap<u64, WritesBySlot<'a>> = HashMap::new();
    for tx in block
        .transaction_traces
        .iter()
        .filter(|tx| tx.status == 1 && with_events.contains(&u64::from(tx.index)))
    {
        let slots = writes_by_tx
            .entry(u64::from(tx.index))
            .or_default();
        for change in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.address == pool_manager)
        {
            slots
                .entry(change.key.clone())
                .or_default()
                .push(change);
        }
    }
    writes_by_tx
}

/// Index of the transaction an event came from.
pub(crate) fn tx_index(event: &PoolEvent) -> u64 {
    event
        .transaction
        .as_ref()
        .expect("3_map_events sets a transaction on every event")
        .index
}

/// Each event with the floor of its write window: the ordinal of the previous BinPoolManager log
/// in the same transaction, or 0. Writes at or below the floor belong to that earlier log.
///
/// `events` must be in ordinal order, as `3_map_events` emits them.
pub(crate) fn with_floors(events: &[PoolEvent]) -> impl Iterator<Item = (&PoolEvent, u64)> {
    let mut prev_log: HashMap<u64, u64> = HashMap::new();
    events.iter().map(move |event| {
        let floor = prev_log
            .insert(tx_index(event), event.log_ordinal)
            .unwrap_or(0);
        (event, floor)
    })
}

/// Whether a write at `ordinal` belongs to the event at `log_ordinal` with `floor` below it.
///
/// The contract writes before it emits, so an event's writes sit strictly between the previous
/// log and its own. Both bounds matter: a later event in the same transaction may rewrite the
/// slot, and an earlier one's write must not be replayed as this event's.
pub(crate) fn in_window(ordinal: u64, floor: u64, log_ordinal: u64) -> bool {
    ordinal > floor && ordinal < log_ordinal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::test_fixtures::{event_in_tx, mint, POOL_MANAGER};

    fn event(tx_index: u64, ordinal: u64) -> PoolEvent {
        event_in_tx(mint(vec![]), ordinal, tx_index)
    }

    fn write(address: [u8; 20], slot: u8, ordinal: u64) -> StorageChange {
        StorageChange {
            address: address.to_vec(),
            key: vec![slot; 32],
            old_value: vec![],
            new_value: vec![1],
            ordinal,
        }
    }

    fn tx(index: u32, status: i32, calls: Vec<eth::Call>) -> eth::TransactionTrace {
        eth::TransactionTrace { index, status, calls, ..Default::default() }
    }

    fn call(reverted: bool, changes: Vec<StorageChange>) -> eth::Call {
        eth::Call { state_reverted: reverted, storage_changes: changes, ..Default::default() }
    }

    /// Only writes an event can read are indexed: its own transaction, successful, in a call that
    /// did not revert, to the pool manager.
    #[test]
    fn indexes_only_readable_writes() {
        let block = eth::Block {
            transaction_traces: vec![
                tx(
                    0,
                    1,
                    vec![
                        call(false, vec![write(POOL_MANAGER, 1, 1), write([0x11; 20], 1, 2)]),
                        call(true, vec![write(POOL_MANAGER, 1, 3)]),
                    ],
                ),
                tx(1, 1, vec![call(false, vec![write(POOL_MANAGER, 1, 4)])]),
                tx(2, 0, vec![call(false, vec![write(POOL_MANAGER, 1, 5)])]),
            ],
            ..Default::default()
        };

        let writes = storage_writes_by_tx(&block, &POOL_MANAGER, &[event(0, 9), event(2, 9)]);

        let ordinals: Vec<u64> = writes[&0][&vec![1u8; 32]]
            .iter()
            .map(|change| change.ordinal)
            .collect();
        assert_eq!(ordinals, vec![1], "foreign address and reverted call are out");
        assert!(!writes.contains_key(&1), "a transaction without an event is not walked");
        assert!(!writes.contains_key(&2), "a failed transaction is not walked");
    }

    #[test]
    fn nothing_is_walked_without_events() {
        let block = eth::Block {
            transaction_traces: vec![tx(0, 1, vec![call(false, vec![write(POOL_MANAGER, 1, 1)])])],
            ..Default::default()
        };

        assert!(storage_writes_by_tx(&block, &POOL_MANAGER, &[]).is_empty());
    }

    /// The floor is per transaction: a log in another transaction does not bound this one.
    #[test]
    fn floors_follow_the_previous_log_in_the_same_transaction() {
        let events = [event(0, 5), event(1, 7), event(0, 9), event(0, 12)];

        let floors: Vec<u64> = with_floors(&events)
            .map(|(_, floor)| floor)
            .collect();

        assert_eq!(floors, vec![0, 0, 5, 9]);
    }

    #[test]
    fn window_is_open_at_both_ends() {
        assert!(!in_window(5, 5, 9), "the previous log's own ordinal is out");
        assert!(in_window(6, 5, 9));
        assert!(in_window(8, 5, 9));
        assert!(!in_window(9, 5, 9), "the event's own ordinal is out");
    }
}
