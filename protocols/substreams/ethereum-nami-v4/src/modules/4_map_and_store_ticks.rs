use crate::pb::nami::v4::{events::pool_event::Typ, Events, TickDelta, TickDeltas};

use substreams::{
    scalar::BigInt,
    store::{StoreAdd, StoreAddBigInt, StoreNew},
};

#[substreams::handlers::map]
pub fn map_ticks_changes(events: Events) -> TickDeltas {
    let deltas = events
        .pool_events
        .into_iter()
        .flat_map(|event| {
            let ticks = maybe_tick_deltas(event.typ.unwrap())?;
            let pool_address = event.pool_address;
            let ordinal = event.log_ordinal;
            let tx = event.transaction;

            Some(ticks.into_iter().map(
                move |(tick_index, liquidity_net_delta, liquidity_gross_delta)| TickDelta {
                    pool_address: pool_address.clone(),
                    tick_index,
                    liquidity_net_delta: liquidity_net_delta.to_signed_bytes_be(),
                    liquidity_gross_delta: liquidity_gross_delta.to_signed_bytes_be(),
                    ordinal,
                    transaction: tx.clone(),
                },
            ))
        })
        .flatten()
        .collect();

    TickDeltas { deltas }
}

/// Key under which a tick's net liquidity is summed.
pub fn net_liquidity_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("{}:{}:net", hex::encode(pool_address), tick_index)
}

/// Key under which a tick's gross liquidity is summed.
pub fn gross_liquidity_key(pool_address: &[u8], tick_index: i32) -> String {
    format!("{}:{}:gross", hex::encode(pool_address), tick_index)
}

#[substreams::handlers::store]
pub fn store_ticks_liquidity(ticks_deltas: TickDeltas, store: StoreAddBigInt) {
    for delta in ticks_deltas.deltas {
        store.add(
            delta.ordinal,
            net_liquidity_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_net_delta),
        );
        store.add(
            delta.ordinal,
            gross_liquidity_key(&delta.pool_address, delta.tick_index),
            BigInt::from_signed_bytes_be(&delta.liquidity_gross_delta),
        );
    }
}

/// Returns `(tick, net delta, gross delta)` for both ticks of a position change.
fn maybe_tick_deltas(ty: Typ) -> Option<[(i32, BigInt, BigInt); 2]> {
    Some(match ty {
        Typ::Mint(mint) => {
            let amount = BigInt::from_unsigned_bytes_be(&mint.amount);
            [
                (mint.tick_upper, amount.neg(), amount.clone()),
                (mint.tick_lower, amount.clone(), amount),
            ]
        }
        Typ::Burn(burn) => {
            let amount = BigInt::from_unsigned_bytes_be(&burn.amount);
            [
                (burn.tick_lower, amount.neg(), amount.neg()),
                (burn.tick_upper, amount.clone(), amount.neg()),
            ]
        }
        // Spelled out so a new event type must be classified here.
        Typ::Initialize(_) |
        Typ::Collect(_) |
        Typ::Swap(_) |
        Typ::Flash(_) |
        Typ::Skim(_) |
        Typ::Fee(_) |
        Typ::Plugin(_) |
        Typ::PluginConfig(_) |
        Typ::MevxRouter(_) |
        Typ::MinGasLeft(_) |
        Typ::CallGasBudget(_) |
        Typ::Incentive(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::nami::v4::events::pool_event;

    fn amount(value: u64) -> Vec<u8> {
        BigInt::from(value).to_bytes_be().1
    }

    /// `(tick, net delta, gross delta)` as strings, sorted by tick.
    fn sorted(deltas: [(i32, BigInt, BigInt); 2]) -> Vec<(i32, String, String)> {
        let mut deltas: Vec<_> = deltas
            .into_iter()
            .map(|(tick, net, gross)| (tick, net.to_string(), gross.to_string()))
            .collect();
        deltas.sort();
        deltas
    }

    #[test]
    fn test_mint_adds_gross_liquidity_to_both_ticks() {
        let deltas = maybe_tick_deltas(Typ::Mint(pool_event::Mint {
            tick_lower: -60,
            tick_upper: 60,
            amount: amount(1_000),
            amount_0: amount(1),
            amount_1: amount(2),
        }))
        .unwrap();

        assert_eq!(
            sorted(deltas),
            vec![
                (-60, "1000".to_string(), "1000".to_string()),
                (60, "-1000".to_string(), "1000".to_string())
            ]
        );
    }

    #[test]
    fn test_burn_removes_gross_liquidity_from_both_ticks() {
        let deltas = maybe_tick_deltas(Typ::Burn(pool_event::Burn {
            tick_lower: -60,
            tick_upper: 60,
            amount: amount(1_000),
        }))
        .unwrap();

        assert_eq!(
            sorted(deltas),
            vec![
                (-60, "-1000".to_string(), "-1000".to_string()),
                (60, "1000".to_string(), "-1000".to_string())
            ]
        );
    }

    #[test]
    fn test_other_events_move_no_ticks() {
        let swap = Typ::Swap(pool_event::Swap {
            amount_0: amount(1),
            amount_1: amount(2),
            sqrt_price: amount(3),
            liquidity: amount(4),
            tick: 5,
        });
        let setting = Typ::MinGasLeft(pool_event::MinGasLeft { min_gas_left: amount(1_000_000) });

        assert!(maybe_tick_deltas(swap).is_none());
        assert!(maybe_tick_deltas(setting).is_none());
    }
}
