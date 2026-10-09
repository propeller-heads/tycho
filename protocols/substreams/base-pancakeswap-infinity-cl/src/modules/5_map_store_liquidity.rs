use crate::{
    modules::map_protocol_changes::liquidity_store_key,
    pb::pancakeswap::infinity::cl::{
        events::{pool_event, PoolEvent},
        Events, LiquidityChange, LiquidityChangeType, LiquidityChanges,
    },
};
use anyhow::Ok;
use std::str::FromStr;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetInt64, StoreSetSum, StoreSetSumBigInt},
};

/// In-range liquidity: `ModifyLiquidity` counts only when `tick_lower <= current_tick <
/// tick_upper` (delta), `Swap` sets the absolute value from the event.
/// [modifyLiquidity](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/libraries/CLPool.sol#L114-L128).
///
/// Ported unchanged from `ethereum-uniswap-v4/shared/src/modules/5_map_store_liquidity.rs`.
#[substreams::handlers::map]
pub fn map_liquidity_changes(
    events: Events,
    pools_current_tick_store: StoreGetInt64,
) -> Result<LiquidityChanges, anyhow::Error> {
    let mut changes = events
        .pool_events
        .into_iter()
        .filter(PoolEvent::can_introduce_liquidity_changes)
        .map(|e| {
            (
                pools_current_tick_store
                    .get_at(e.log_ordinal, format!("pool:{0}", e.pool_id))
                    .unwrap_or(0),
                e,
            )
        })
        .filter_map(|(current_tick, event)| event_to_liquidity_deltas(current_tick, event))
        .collect::<Vec<_>>();

    changes.sort_unstable_by_key(|l| l.ordinal);
    Ok(LiquidityChanges { changes })
}

#[substreams::handlers::store]
pub fn store_liquidity(liquidity_changes: LiquidityChanges, store: StoreSetSumBigInt) {
    for change in &liquidity_changes.changes {
        let key = liquidity_store_key(&change.pool_address);
        let value = BigInt::from_signed_bytes_be(&change.value);
        match change.change_type() {
            LiquidityChangeType::Delta => store.sum(change.ordinal, key, value),
            LiquidityChangeType::Absolute => store.set(change.ordinal, key, value),
        }
    }
}

fn event_to_liquidity_deltas(current_tick: i64, event: PoolEvent) -> Option<LiquidityChange> {
    let (value, change_type) = match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(modify) => {
            let in_range =
                current_tick >= modify.tick_lower.into() && current_tick < modify.tick_upper.into();
            if !in_range {
                return None;
            }
            (&modify.liquidity_delta, LiquidityChangeType::Delta)
        }
        pool_event::Type::Swap(swap) => (&swap.liquidity, LiquidityChangeType::Absolute),
        pool_event::Type::Initialize(_) |
        pool_event::Type::Donate(_) |
        pool_event::Type::ProtocolFeeUpdated(_) => return None,
    };

    Some(LiquidityChange {
        pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
        value: BigInt::from_str(value)
            .unwrap()
            .to_signed_bytes_be(),
        change_type: change_type.into(),
        ordinal: event.log_ordinal,
        transaction: Some(event.transaction.unwrap()),
    })
}

impl PoolEvent {
    fn can_introduce_liquidity_changes(&self) -> bool {
        matches!(
            self.r#type.as_ref().unwrap(),
            pool_event::Type::ModifyLiquidity(_) | pool_event::Type::Swap(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::pancakeswap::infinity::cl::Transaction;
    use rstest::rstest;

    fn event(r#type: pool_event::Type) -> PoolEvent {
        PoolEvent {
            log_ordinal: 7,
            pool_id: format!("0x{}", "ab".repeat(32)),
            currency0: String::new(),
            currency1: String::new(),
            transaction: Some(Transaction::default()),
            r#type: Some(r#type),
        }
    }

    fn modify_liquidity(liquidity_delta: &str) -> PoolEvent {
        event(pool_event::Type::ModifyLiquidity(pool_event::ModifyLiquidity {
            tick_lower: -60,
            tick_upper: 60,
            liquidity_delta: liquidity_delta.to_string(),
            ..Default::default()
        }))
    }

    /// The range is closed below and open above.
    #[rstest]
    #[case::at_lower(-60)]
    #[case::inside(0)]
    #[case::just_below_upper(59)]
    fn position_in_range_changes_liquidity_by_its_delta(#[case] current_tick: i64) {
        let change = event_to_liquidity_deltas(current_tick, modify_liquidity("-500"))
            .expect("an in-range position changes active liquidity");

        assert_eq!(BigInt::from_signed_bytes_be(&change.value), BigInt::from(-500));
        assert_eq!(change.change_type(), LiquidityChangeType::Delta);
        assert_eq!((change.pool_address, change.ordinal), (vec![0xab; 32], 7));
    }

    #[rstest]
    #[case::just_below_lower(-61)]
    #[case::at_upper(60)]
    #[case::above(1_000)]
    fn position_out_of_range_leaves_liquidity_alone(#[case] current_tick: i64) {
        assert!(
            event_to_liquidity_deltas(current_tick, modify_liquidity("500")).is_none(),
            "tick {current_tick} is outside [-60, 60)"
        );
    }

    /// The event carries the pool's liquidity after the swap, whatever ticks it crossed.
    #[test]
    fn swap_sets_liquidity_to_the_event_value() {
        let swap = event(pool_event::Type::Swap(pool_event::Swap {
            liquidity: "9000".to_string(),
            ..Default::default()
        }));

        let change = event_to_liquidity_deltas(0, swap).expect("a swap reports liquidity");

        assert_eq!(BigInt::from_signed_bytes_be(&change.value), BigInt::from(9_000));
        assert_eq!(change.change_type(), LiquidityChangeType::Absolute);
    }
}
