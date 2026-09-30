use std::str::FromStr;

use substreams::store::StoreAddBigInt;

use crate::pb::uniswap::v4::{
    events::{pool_event, PoolEvent},
    Events, TickDelta, TickDeltas,
};

use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreNew},
};

use anyhow::Ok;
use substreams_helper::hex::Hexable;

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
            format!("pool:{0}:tick:{1}", delta.pool_address.to_hex(), delta.tick_index,),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
    });
}

#[substreams::handlers::store]
pub fn store_ticks_gross_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    let mut deltas = ticks_deltas.deltas;

    deltas.sort_unstable_by_key(|delta| delta.ordinal);

    deltas.iter().for_each(|delta| {
        store.add(
            delta.ordinal,
            format!("pool:{0}:tick:{1}", delta.pool_address.to_hex(), delta.tick_index,),
            BigInt::from_signed_bytes_be(&delta.liquidity_gross_delta),
        );
    });
}

fn event_to_ticks_deltas(event: PoolEvent) -> Vec<TickDelta> {
    // On UniswapV4, the only event that changes liquidity is ModifyLiquidity. Liquidity Delta is
    // now expressed as a signed int256. A positive number indicates a mint, while a negative
    // indicates a burn.
    // Mint events will have negative deltas for the upper tick and positive deltas for the lower.
    // Burn events will have positive deltas for the upper tick and negative deltas for the lower.
    match event.r#type.as_ref().unwrap() {
        pool_event::Type::ModifyLiquidity(liq_change) => {
            let amount =
                BigInt::from_str(&liq_change.liquidity_delta).expect("Failed to parse BigInt");
            vec![
                TickDelta {
                    pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    tick_index: liq_change.tick_lower,
                    liquidity_net_delta: amount.to_signed_bytes_be(),
                    liquidity_gross_delta: amount.to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction.clone(),
                },
                TickDelta {
                    pool_address: hex::decode(event.pool_id.trim_start_matches("0x")).unwrap(),
                    tick_index: liq_change.tick_upper,
                    liquidity_net_delta: amount.neg().to_signed_bytes_be(),
                    liquidity_gross_delta: amount.to_signed_bytes_be(),
                    ordinal: event.log_ordinal,
                    transaction: event.transaction,
                },
            ]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::uniswap::v4::events::pool_event::{ModifyLiquidity, Type};

    fn modify_liquidity(
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
        ordinal: u64,
    ) -> PoolEvent {
        PoolEvent {
            log_ordinal: ordinal,
            pool_id: "00".repeat(32),
            currency0: String::new(),
            currency1: String::new(),
            transaction: None,
            r#type: Some(Type::ModifyLiquidity(ModifyLiquidity {
                sender: String::new(),
                tick_lower,
                tick_upper,
                liquidity_delta: liquidity_delta.to_string(),
                salt: String::new(),
            })),
        }
    }

    fn liquidity_at_tick(
        deltas: impl IntoIterator<Item = TickDelta>,
        tick: i32,
    ) -> (BigInt, BigInt) {
        deltas
            .into_iter()
            .filter(|delta| delta.tick_index == tick)
            .fold((BigInt::zero(), BigInt::zero()), |(net, gross), delta| {
                (
                    net + BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
                    gross + BigInt::from_signed_bytes_be(&delta.liquidity_gross_delta),
                )
            })
    }

    #[test]
    fn adjacent_positions_produce_a_gross_positive_zero_net_boundary() {
        let deltas = [modify_liquidity(-180, -120, 100, 1), modify_liquidity(-120, 120, 100, 2)]
            .into_iter()
            .flat_map(event_to_ticks_deltas);

        assert_eq!(liquidity_at_tick(deltas, -120), (BigInt::zero(), BigInt::from(200)));
    }

    #[test]
    fn burning_adjacent_positions_reduces_the_shared_boundary_gross_to_zero() {
        let mint_deltas =
            [modify_liquidity(-180, -120, 100, 1), modify_liquidity(-120, 120, 100, 2)]
                .into_iter()
                .flat_map(event_to_ticks_deltas);
        let burn_deltas =
            [modify_liquidity(-180, -120, -100, 3), modify_liquidity(-120, 120, -100, 4)]
                .into_iter()
                .flat_map(event_to_ticks_deltas);

        assert_eq!(
            liquidity_at_tick(mint_deltas.chain(burn_deltas), -120),
            (BigInt::zero(), BigInt::zero())
        );
    }
}
