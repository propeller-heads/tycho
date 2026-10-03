use crate::{
    modules::map_protocol_changes::tick_store_key,
    pb::pancakeswap::infinity::cl::{
        events::{pool_event, PoolEvent},
        Events, TickDelta, TickDeltas,
    },
};
use std::str::FromStr;
use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};

/// `ModifyLiquidity` -> `+delta` at `tick_lower`, `-delta` at `tick_upper`.
///
/// Ported unchanged from `ethereum-uniswap-v4/shared/src/modules/5_map_store_ticks.rs`.
#[substreams::handlers::map]
pub fn map_ticks_changes(events: Events) -> Result<TickDeltas, anyhow::Error> {
    let ticks_deltas = events
        .pool_events
        .into_iter()
        .flat_map(event_to_ticks_deltas)
        .collect();

    Ok(TickDeltas { deltas: ticks_deltas })
}

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    let mut deltas = ticks_deltas.deltas;

    deltas.sort_unstable_by_key(|delta| delta.ordinal);

    deltas.iter().for_each(|delta| {
        store.add(
            delta.ordinal,
            tick_store_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
    });
}

fn event_to_ticks_deltas(event: PoolEvent) -> Vec<TickDelta> {
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(modify) => {
            let delta = BigInt::from_str(&modify.liquidity_delta).expect("Failed to parse BigInt");
            let pool_address = hex::decode(event.pool_id.trim_start_matches("0x")).unwrap();
            // Net liquidity is what crossing the tick left to right adds, so the upper tick takes
            // the opposite sign:
            // https://github.com/pancakeswap/infinity-core/blob/d0e879334da8ea789a895d864dbe34259ea9fb65/src/pool-cl/libraries/Tick.sol#L168
            [
                (modify.tick_lower, delta.to_signed_bytes_be()),
                (modify.tick_upper, delta.neg().to_signed_bytes_be()),
            ]
            .into_iter()
            .map(|(tick_index, liquidity_net_delta)| TickDelta {
                pool_address: pool_address.clone(),
                tick_index,
                liquidity_net_delta,
                ordinal: event.log_ordinal,
                transaction: event.transaction.clone(),
            })
            .collect()
        }
        pool_event::Type::Initialize(_) |
        pool_event::Type::Swap(_) |
        pool_event::Type::Donate(_) |
        pool_event::Type::ProtocolFeeUpdated(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn event(r#type: pool_event::Type) -> PoolEvent {
        PoolEvent {
            log_ordinal: 7,
            pool_id: format!("0x{}", "ab".repeat(32)),
            currency0: String::new(),
            currency1: String::new(),
            transaction: None,
            r#type: Some(r#type),
        }
    }

    #[rstest]
    #[case::mint("500", 500, -500)]
    #[case::burn("-500", -500, 500)]
    fn modify_liquidity_moves_both_ticks_in_opposite_directions(
        #[case] liquidity_delta: &str,
        #[case] expected_lower: i64,
        #[case] expected_upper: i64,
    ) {
        let deltas = event_to_ticks_deltas(event(pool_event::Type::ModifyLiquidity(
            pool_event::ModifyLiquidity {
                tick_lower: -60,
                tick_upper: 60,
                liquidity_delta: liquidity_delta.to_string(),
                ..Default::default()
            },
        )));

        let net = |delta: &TickDelta| BigInt::from_signed_bytes_be(&delta.liquidity_net_delta);
        assert_eq!(deltas.len(), 2, "one delta per position boundary");
        assert_eq!((deltas[0].tick_index, net(&deltas[0])), (-60, BigInt::from(expected_lower)));
        assert_eq!((deltas[1].tick_index, net(&deltas[1])), (60, BigInt::from(expected_upper)));
        assert!(
            deltas
                .iter()
                .all(|delta| delta.ordinal == 7 && delta.pool_address == vec![0xab; 32]),
            "both deltas carry the log's ordinal and the raw pool id"
        );
    }

    #[test]
    fn swap_moves_no_tick_liquidity() {
        let swap = event(pool_event::Type::Swap(Default::default()));

        assert!(event_to_ticks_deltas(swap).is_empty(), "only ModifyLiquidity changes tick nets");
    }
}
