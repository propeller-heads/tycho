use substreams::store::{StoreNew, StoreSet, StoreSetInt64};

use crate::pb::pancakeswap::infinity::bin::{
    events::{pool_event, PoolEvent},
    Events,
};

/// Tracks the active bin per pool (`pool:{id}`) from `Initialize` and `Swap`.
///
/// Writes at the LOG ordinal, never 0, so a reader can ask for the state as of any log in the
/// block. A Bin `Swap` reports only the active bin AFTER the swap, and `get_at` includes the write
/// at the ordinal it is given, so `6_map_bin_changes` reads one ordinal below the swap's log to
/// recover the pre-swap bin.
#[substreams::handlers::store]
pub fn store_active_id(events: Events, store: StoreSetInt64) {
    events
        .pool_events
        .into_iter()
        .filter_map(event_to_active_id)
        .for_each(|(pool, ordinal, active_id)| {
            store.set(ordinal, format!("pool:{pool}"), &i64::from(active_id))
        });
}

/// `(pool_id, ordinal, active_id)`
fn event_to_active_id(event: PoolEvent) -> Option<(String, u64, u32)> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::Initialize(initialize) => {
            Some((event.pool_id, event.log_ordinal, initialize.active_id))
        }
        pool_event::Type::Swap(swap) => Some((event.pool_id, event.log_ordinal, swap.active_id)),
        pool_event::Type::ProtocolFeeUpdated(..) |
        pool_event::Type::Burn(..) |
        pool_event::Type::Mint(..) |
        pool_event::Type::Donate(..) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const POOL: &str = "0xabab";

    fn event(kind: pool_event::Type) -> PoolEvent {
        PoolEvent {
            log_ordinal: 9,
            pool_id: POOL.to_string(),
            currency0: String::new(),
            currency1: String::new(),
            transaction: None,
            r#type: Some(kind),
        }
    }

    fn initialize(active_id: u32) -> pool_event::Type {
        pool_event::Type::Initialize(pool_event::Initialize {
            fee: 0,
            bin_step: 10,
            active_id,
            hooks: String::new(),
            parameters: Vec::new(),
        })
    }

    fn swap(active_id: u32) -> pool_event::Type {
        pool_event::Type::Swap(pool_event::Swap {
            sender: String::new(),
            amount0: "0".into(),
            amount1: "0".into(),
            active_id,
            fee: 0,
        })
    }

    fn mint() -> pool_event::Type {
        pool_event::Type::Mint(pool_event::Mint {
            sender: String::new(),
            ids: vec![1],
            salt: String::new(),
        })
    }

    fn donate() -> pool_event::Type {
        pool_event::Type::Donate(pool_event::Donate {
            sender: String::new(),
            amount0: "0".into(),
            amount1: "0".into(),
            bin_id: 1,
        })
    }

    /// Only `Initialize` and `Swap` move the active bin. Mint, Burn and Donate touch bins without
    /// moving it, and recording them would overwrite the pre-swap value `6_map_bin_changes` reads
    /// back at a swap's ordinal.
    #[rstest]
    #[case::initialize(initialize(8_388_608), Some(8_388_608))]
    #[case::swap(swap(8_388_600), Some(8_388_600))]
    #[case::mint(mint(), None)]
    #[case::donate(donate(), None)]
    fn only_initialize_and_swap_set_the_active_id(
        #[case] kind: pool_event::Type,
        #[case] expected: Option<u32>,
    ) {
        let active_id = event_to_active_id(event(kind));

        assert_eq!(active_id.map(|(_, _, id)| id), expected);
    }

    /// The write goes at the log's ordinal, never 0, so `6_map_bin_changes` can read the state as
    /// of the log before a swap. A write at 0 would return the block's final value for every read.
    #[test]
    fn writes_at_the_log_ordinal() {
        let (pool, ordinal, _) = event_to_active_id(event(swap(1))).expect("swap sets it");

        assert_eq!((pool.as_str(), ordinal), (POOL, 9));
    }
}
