use substreams::store::{
    StoreGet, StoreGetInt64, StoreSet, StoreSetInt64, StoreSetSum, StoreSetSumBigInt,
};

use crate::pb::ramses::v3::{
    events::pool_event::Typ, Events, LiquidityChange, LiquidityChangeType, LiquidityChanges,
};

use substreams::{scalar::BigInt, store::StoreNew};

#[substreams::handlers::store]
pub fn store_pool_current_tick(events: Events, store: StoreSetInt64) {
    events
        .pool_events
        .into_iter()
        .filter_map(|event| {
            let tick = maybe_current_tick(event.typ.unwrap())?;
            Some((hex::encode(&event.pool_address), event.log_ordinal, tick))
        })
        .for_each(|(pool, ordinal, tick)| store.set(ordinal, pool, &tick.into()));
}

#[substreams::handlers::map]
pub fn map_liquidity_changes(
    events: Events,
    pools_current_tick_store: StoreGetInt64,
) -> LiquidityChanges {
    let changes = events
        .pool_events
        .into_iter()
        .filter_map(|event| {
            let (value, change_type) = maybe_liquidity_change(
                || {
                    pools_current_tick_store
                        .get_at(event.log_ordinal, hex::encode(&event.pool_address))
                        .expect("current tick to exist for pool")
                        .try_into()
                        .expect("current tick to fit into i32")
                },
                event.typ.unwrap(),
            )?;

            Some(LiquidityChange {
                pool_address: event.pool_address,
                value,
                change_type: change_type.into(),
                ordinal: event.log_ordinal,
                transaction: event.transaction,
            })
        })
        .collect();

    LiquidityChanges { changes }
}

#[substreams::handlers::store]
pub fn store_liquidity(ticks_deltas: LiquidityChanges, store: StoreSetSumBigInt) {
    for changes in ticks_deltas.changes {
        let ord = changes.ordinal;
        let key = hex::encode(&changes.pool_address);
        let value = BigInt::from_signed_bytes_be(&changes.value);

        match changes.change_type() {
            LiquidityChangeType::Delta => {
                store.sum(ord, key, value);
            }
            LiquidityChangeType::Absolute => {
                store.set(ord, key, value);
            }
        }
    }
}

fn maybe_current_tick(ty: Typ) -> Option<i32> {
    Some(match ty {
        Typ::Initialize(init) => init.tick,
        Typ::Swap(swap) => swap.tick,
        _ => return None,
    })
}

fn maybe_liquidity_change(
    current_tick_fn: impl FnOnce() -> i32,
    ty: Typ,
) -> Option<(Vec<u8>, LiquidityChangeType)> {
    match ty {
        Typ::Mint(mint) => {
            let current_tick = current_tick_fn();
            (current_tick >= mint.tick_lower && current_tick < mint.tick_upper).then(|| {
                (
                    BigInt::from_unsigned_bytes_be(&mint.amount).to_signed_bytes_be(),
                    LiquidityChangeType::Delta,
                )
            })
        }
        Typ::Burn(burn) => {
            let current_tick = current_tick_fn();
            (current_tick >= burn.tick_lower && current_tick < burn.tick_upper).then(|| {
                (
                    BigInt::from_unsigned_bytes_be(&burn.amount)
                        .neg()
                        .to_signed_bytes_be(),
                    LiquidityChangeType::Delta,
                )
            })
        }
        Typ::Swap(swap) => Some((
            BigInt::from_unsigned_bytes_be(&swap.liquidity).to_signed_bytes_be(),
            LiquidityChangeType::Absolute,
        )),
        _ => None,
    }
}
