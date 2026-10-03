use crate::{
    parameters::{self, BIN_POOLS_MAPPING_SLOT},
    pb::pancakeswap::infinity::bin::{
        events::{pool_event, PoolEvent},
        BinDelta, BinDeltas, Events,
    },
};
use std::collections::HashMap;
use substreams::store::{StoreGet, StoreGetInt64};
use substreams_ethereum::pb::eth::v2::{self as eth, Call, StorageChange};

/// The bin tree only visits non-empty bins, so no real swap comes near this. Turns an active-id
/// bookkeeping bug into a fast failure instead of a 16M-iteration loop.
const MAX_SWAP_BIN_SPAN: u64 = 65_536;

/// Absolute per-bin reserves, read out of BinPoolManager storage diffs.
///
/// No CL counterpart. No Bin event carries per-bin reserves: `Mint`/`Burn` give `ids[]` and a
/// packed `amounts[]`, `Swap` gives only the net amounts for the whole swap. Deriving them would
/// mean porting `BinPool.swap` and keeping it bit-exact forever, so read what the contract wrote.
#[substreams::handlers::map]
pub fn map_bin_changes(
    params: String,
    block: eth::Block,
    events: Events,
    active_id_store: StoreGetInt64,
) -> Result<BinDeltas, substreams::errors::Error> {
    let pool_manager = hex::decode(&params).expect("pool manager is hex");

    // Per transaction, because two transactions in one block can write the same bin. Every write
    // kept, not just the last: one transaction can mint into a bin then swap through it, which is
    // two events needing two before/after pairs.
    let mut writes_by_tx: HashMap<u64, HashMap<Vec<u8>, Vec<&StorageChange>>> = HashMap::new();
    for tx in block
        .transaction_traces
        .iter()
        .filter(|tx| tx.status == 1)
    {
        let slots = writes_by_tx
            .entry(u64::from(tx.index))
            .or_default();
        for change in tx
            .calls
            .iter()
            .filter(|call: &&Call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .filter(|change| change.address == pool_manager)
        {
            slots
                .entry(change.key.clone())
                .or_default()
                .push(change);
        }
    }

    let no_writes = HashMap::new();
    let mut deltas: Vec<BinDelta> = Vec::new();
    // Floor for the storage-write lookup, per transaction: writes at or below the previous
    // BinPoolManager log belong to that log, not to this one.
    let mut prev_log: HashMap<u64, u64> = HashMap::new();
    for event in &events.pool_events {
        let tx_index = event
            .transaction
            .as_ref()
            .expect("3_map_events sets a transaction on every event")
            .index;
        // Not an error: events that touch no bins never look one up.
        let writes = writes_by_tx
            .get(&tx_index)
            .unwrap_or(&no_writes);
        let pre_active_id = match event.r#type.as_ref().unwrap() {
            pool_event::Type::Swap(..) => {
                pre_swap_active_id(&active_id_store, &event.pool_id, event.log_ordinal)
            }
            _ => None,
        };
        let floor = prev_log
            .get(&tx_index)
            .copied()
            .unwrap_or(0);
        deltas.extend(event_to_deltas(event, writes, pre_active_id, floor)?);
        prev_log.insert(tx_index, event.log_ordinal);
    }

    // Deterministic for the join in 6_map_protocol_changes.
    deltas.sort_unstable_by_key(|delta| (delta.ordinal, delta.bin_id));

    Ok(BinDeltas { deltas })
}

/// Active bin before a swap, `None` when the store has no earlier value.
///
/// `store_active_id` writes the post-swap id at the swap's own log ordinal, and `get_at` returns
/// the state *including* the write at the ordinal it is given, so reading at `log_ordinal` would
/// return the post-swap id and collapse the crossed-bin range to one bin.
fn pre_swap_active_id<S: StoreGet<i64>>(store: &S, pool_id: &str, log_ordinal: u64) -> Option<u32> {
    store
        .get_at(log_ordinal.saturating_sub(1), format!("pool:{pool_id}"))
        .map(|id| id as u32)
}

/// Bins one event touched. `bin_id` comes from the event, the packed reserves from the storage
/// write at `bin_reserve_slot`.
///
/// `pre_active_id`: active bin immediately before this event, `None` if the store has no value.
/// Swaps only.
fn event_to_deltas(
    event: &PoolEvent,
    writes: &HashMap<Vec<u8>, Vec<&StorageChange>>,
    pre_active_id: Option<u32>,
    prev_log_ordinal: u64,
) -> Result<Vec<BinDelta>, substreams::errors::Error> {
    // Raw 32 bytes, not `pool_id.as_bytes()`, which is UTF-8 of the "0x..." string.
    // bin_reserve_slot hashes these, so the wrong form addresses nothing.
    let pool_id = hex::decode(event.pool_id.trim_start_matches("0x")).expect("pool_id is hex");
    let base = parameters::pool_state_base_slot(
        pool_id
            .as_slice()
            .try_into()
            .expect("pool id is 32 bytes"),
        BIN_POOLS_MAPPING_SLOT,
    );

    // Carried onto every delta so map_balance_changes needs no second lookup.
    let currency0 = hex::decode(event.currency0.trim_start_matches("0x")).expect("currency0 hex");
    let currency1 = hex::decode(event.currency1.trim_start_matches("0x")).expect("currency1 hex");

    // A missing write means opposite things per event, so the flag travels with the ids.
    let (bin_ids, missing_is_fatal): (Vec<u32>, bool) = match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(..) | pool_event::Type::ProtocolFeeUpdated(..) => {
            (vec![], false)
        }
        pool_event::Type::Mint(mint) => (mint.ids.clone(), true),
        pool_event::Type::Burn(burn) => (burn.ids.clone(), true),
        pool_event::Type::Donate(donate) => (vec![donate.bin_id], true),
        pool_event::Type::Swap(swap) => {
            let post = swap.active_id;
            // Missing only if no Initialize or Swap preceded this log for the pool.
            let pre = pre_active_id.unwrap_or(post);

            // Ordered: `a..=b` is empty when a > b, dropping every downward swap.
            let (low, high) = (pre.min(post), pre.max(post));
            let span = u64::from(high - low) + 1;
            if span > MAX_SWAP_BIN_SPAN {
                return Err(anyhow::anyhow!(
                    "pool {} swap at ordinal {} spans {} bins, {} to {}",
                    event.pool_id,
                    event.log_ordinal,
                    span,
                    low,
                    high
                ));
            }
            ((low..=high).collect(), false)
        }
    };

    let mut deltas = Vec::with_capacity(bin_ids.len());
    for bin_id in bin_ids {
        let slot = parameters::bin_reserve_slot(&base, bin_id);

        // Contract writes the bin before emitting, so take the latest write between the previous
        // log and this one. Both bounds matter: a later event in the same tx may rewrite the slot,
        // and an earlier one's write must not be replayed as this event's.
        let write = writes
            .get(slot.as_slice())
            .and_then(|slot_writes| {
                slot_writes
                    .iter()
                    .filter(|change| {
                        change.ordinal > prev_log_ordinal && change.ordinal < event.log_ordinal
                    })
                    .max_by_key(|change| change.ordinal)
            });

        match write {
            Some(change) => deltas.push(BinDelta {
                pool_id: pool_id.clone(),
                currency0: currency0.clone(),
                currency1: currency1.clone(),
                bin_id,
                old_packed: change.old_value.clone(),
                new_packed: change.new_value.clone(),
                ordinal: event.log_ordinal,
                transaction: event.transaction.clone(),
            }),
            // Event named the bin, so an absent slot means the layout assumption is broken.
            None if missing_is_fatal => {
                return Err(anyhow::anyhow!(
                    "pool {} bin {} has no reserveOfBin write at ordinal {}: storage layout \
                     assumption is broken",
                    event.pool_id,
                    bin_id,
                    event.log_ordinal
                ))
            }
            // Swap range only: bin was empty, never written.
            None => {}
        }
    }

    Ok(deltas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::pancakeswap::infinity::bin::{events::pool_event::Type, Transaction};

    const POOL_MANAGER: [u8; 20] = [0xc6; 20];
    const POOL_ID: &str = "abababababababababababababababababababababababababababababababab";

    fn pool_id_bytes() -> [u8; 32] {
        hex::decode(POOL_ID)
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn packed(x: u128, y: u128) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[..16].copy_from_slice(&y.to_be_bytes());
        out[16..].copy_from_slice(&x.to_be_bytes());
        out
    }

    fn write(bin_id: u32, old: Vec<u8>, new: Vec<u8>, ordinal: u64) -> StorageChange {
        let base = parameters::pool_state_base_slot(&pool_id_bytes(), BIN_POOLS_MAPPING_SLOT);
        StorageChange {
            address: POOL_MANAGER.to_vec(),
            key: parameters::bin_reserve_slot(&base, bin_id).to_vec(),
            old_value: old,
            new_value: new,
            ordinal,
        }
    }

    fn index(changes: &[StorageChange]) -> HashMap<Vec<u8>, Vec<&StorageChange>> {
        let mut map: HashMap<Vec<u8>, Vec<&StorageChange>> = HashMap::new();
        for change in changes {
            map.entry(change.key.clone())
                .or_default()
                .push(change);
        }
        map
    }

    fn event(kind: Type, ordinal: u64) -> PoolEvent {
        PoolEvent {
            log_ordinal: ordinal,
            pool_id: format!("0x{POOL_ID}"),
            currency0: String::new(),
            currency1: String::new(),
            transaction: Some(Transaction { hash: vec![1], from: vec![], to: vec![], index: 0 }),
            r#type: Some(kind),
        }
    }

    fn mint(ids: Vec<u32>) -> Type {
        Type::Mint(pool_event::Mint { sender: String::new(), ids, salt: String::new() })
    }

    fn swap(active_id: u32) -> Type {
        Type::Swap(pool_event::Swap {
            sender: String::new(),
            amount0: "0".into(),
            amount1: "0".into(),
            active_id,
            fee: 0,
        })
    }

    #[test]
    fn mint_emits_one_delta_per_written_bin() {
        let changes = vec![
            write(100, packed(0, 0), packed(10, 20), 1),
            write(101, packed(0, 0), packed(11, 21), 2),
            write(102, packed(0, 0), packed(12, 22), 3),
        ];
        let deltas =
            event_to_deltas(&event(mint(vec![100, 101, 102]), 9), &index(&changes), None, 0)
                .unwrap();

        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].bin_id, 100);
        assert_eq!(deltas[0].pool_id, pool_id_bytes().to_vec());
        assert_eq!(deltas[0].old_packed, packed(0, 0));
        assert_eq!(deltas[0].new_packed, packed(10, 20));
        assert_eq!(deltas[2].new_packed, packed(12, 22));
    }

    /// Event names the bin, so an absent slot means a broken layout assumption.
    #[test]
    fn mint_with_a_missing_write_is_an_error() {
        let changes = vec![
            write(100, packed(0, 0), packed(10, 20), 1),
            write(102, packed(0, 0), packed(12, 22), 3),
        ];
        let err = event_to_deltas(&event(mint(vec![100, 101, 102]), 9), &index(&changes), None, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bin 101"), "{err}");
    }

    /// Range is a guess; unwritten bins were empty.
    #[test]
    fn swap_skips_unwritten_bins_in_its_range() {
        let changes = vec![
            write(5, packed(5, 0), packed(4, 1), 1),
            write(6, packed(6, 0), packed(3, 2), 2),
            write(8, packed(8, 0), packed(2, 3), 3),
        ];
        let deltas = event_to_deltas(&event(swap(8), 9), &index(&changes), Some(5), 0).unwrap();

        let bins: Vec<u32> = deltas
            .iter()
            .map(|d| d.bin_id)
            .collect();
        assert_eq!(bins, vec![5, 6, 8], "bin 7 was empty, not an error");
    }

    /// `a..=b` is empty when a > b, so an unordered range drops every downward swap.
    #[test]
    fn swap_range_is_ordered_in_both_directions() {
        let changes = vec![
            write(5, packed(5, 0), packed(4, 1), 1),
            write(6, packed(6, 0), packed(3, 2), 2),
            write(8, packed(8, 0), packed(2, 3), 3),
        ];
        let up = event_to_deltas(&event(swap(8), 9), &index(&changes), Some(5), 0).unwrap();
        let down = event_to_deltas(&event(swap(5), 9), &index(&changes), Some(8), 0).unwrap();

        let bins = |d: &[BinDelta]| {
            d.iter()
                .map(|x| x.bin_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(bins(&up), bins(&down));
        assert_eq!(bins(&down), vec![5, 6, 8]);
    }

    /// Two events in one tx writing the same bin need their own pairs, else balances double-count.
    #[test]
    fn later_event_in_the_same_tx_gets_its_own_write() {
        let changes = vec![
            write(100, packed(0, 0), packed(10, 10), 1), // the mint's write
            write(100, packed(10, 10), packed(7, 13), 11), // the swap's write
        ];
        let writes = index(&changes);

        let minted = event_to_deltas(&event(mint(vec![100]), 5), &writes, None, 0).unwrap();
        assert_eq!(minted[0].old_packed, packed(0, 0));
        assert_eq!(minted[0].new_packed, packed(10, 10));

        let swapped = event_to_deltas(&event(swap(100), 15), &writes, Some(100), 5).unwrap();
        assert_eq!(swapped[0].old_packed, packed(10, 10), "must not reuse the mint's pair");
        assert_eq!(swapped[0].new_packed, packed(7, 13));
    }

    /// Stands in for the host store with the engine's semantics: `get_at(ord)` returns the state
    /// including every write at or below `ord` (`storage/store/value_get.go`, `getAt` rewinds only
    /// deltas whose ordinal is strictly greater).
    struct FakeStore(Vec<(u64, String, i64)>);

    impl StoreGet<i64> for FakeStore {
        fn new(_idx: u32) -> Self {
            Self(Vec::new())
        }

        fn get_at<K: AsRef<str>>(&self, ord: u64, key: K) -> Option<i64> {
            self.0
                .iter()
                .filter(|(written_at, written_key, _)| {
                    *written_at <= ord && written_key == key.as_ref()
                })
                .max_by_key(|(written_at, _, _)| *written_at)
                .map(|(_, _, value)| *value)
        }

        fn get_last<K: AsRef<str>>(&self, _key: K) -> Option<i64> {
            unimplemented!("tests only read at an ordinal")
        }

        fn get_first<K: AsRef<str>>(&self, _key: K) -> Option<i64> {
            unimplemented!("tests only read at an ordinal")
        }

        fn has_at<K: AsRef<str>>(&self, _ord: u64, _key: K) -> bool {
            unimplemented!("tests only read at an ordinal")
        }

        fn has_last<K: AsRef<str>>(&self, _key: K) -> bool {
            unimplemented!("tests only read at an ordinal")
        }

        fn has_first<K: AsRef<str>>(&self, _key: K) -> bool {
            unimplemented!("tests only read at an ordinal")
        }
    }

    /// `store_active_id` writes the post-swap id at the swap's own ordinal, so reading at that
    /// ordinal returns the post value and the crossed-bin range collapses to one bin.
    #[test]
    fn pre_swap_active_id_ignores_this_logs_own_write() {
        let key = format!("pool:0x{POOL_ID}");
        let store = FakeStore(vec![(5, key.clone(), 8_388_608), (9, key, 8_388_610)]);

        let pre = pre_swap_active_id(&store, &format!("0x{POOL_ID}"), 9);

        assert_eq!(pre, Some(8_388_608), "read the state before this swap, not after it");
    }

    /// With no earlier write the swap is the pool's first, and the caller falls back to the post
    /// id.
    #[test]
    fn pre_swap_active_id_is_none_without_an_earlier_write() {
        let store = FakeStore(vec![(9, format!("pool:0x{POOL_ID}"), 8_388_610)]);

        assert_eq!(pre_swap_active_id(&store, &format!("0x{POOL_ID}"), 9), None);
    }

    /// An earlier event's write in the same tx must not be replayed as this swap's pair, or the
    /// bin's reserve is counted twice.
    #[test]
    fn earlier_events_write_is_not_reused_across_the_range() {
        let changes = vec![
            write(101, packed(10, 10), packed(0, 0), 3), // a burn empties bin 101
            write(100, packed(10, 10), packed(7, 13), 11), // the swap's own write
        ];

        let deltas =
            event_to_deltas(&event(swap(100), 15), &index(&changes), Some(101), 3).unwrap();

        assert_eq!(deltas.len(), 1, "only the swap's own write counts");
        assert_eq!(deltas[0].bin_id, 100);
    }

    /// Guards active-id bookkeeping, not any real swap.
    #[test]
    fn absurd_swap_span_errors() {
        let err = event_to_deltas(&event(swap(1), 9), &index(&[]), Some(1 << 23), 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("spans"), "{err}");
    }

    /// No bins touched, no lookups.
    #[test]
    fn protocol_fee_updated_yields_nothing() {
        let kind = Type::ProtocolFeeUpdated(pool_event::ProtocolFeeUpdated {
            pool_id: format!("0x{POOL_ID}"),
            protocol_fee: 1,
        });
        assert!(
            event_to_deltas(&event(kind, 9), &index(&[]), None, 0)
                .unwrap()
                .is_empty(),
            "ProtocolFeeUpdated touches no bins, so it must yield no deltas"
        );
    }
}
