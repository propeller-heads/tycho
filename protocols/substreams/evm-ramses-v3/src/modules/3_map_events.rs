use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log, TransactionTrace},
    Event,
};

use crate::{
    abi::pool::events::{
        Burn, Collect, CollectProtocol, FeeAdjustment, Flash, Initialize, Mint, Swap,
    },
    pb::ramses::v3::{
        events::{
            pool_event::{self, Typ},
            PoolEvent,
        },
        Events, Pool,
    },
};

#[substreams::handlers::map]
pub fn map_events(block: eth::Block, pools_store: StoreGetProto<Pool>) -> Events {
    let pool_events = block
        .transactions()
        .flat_map(|tx| {
            tx.logs_with_calls()
                .filter_map(|(log, _)| {
                    let pool = pools_store.get_last(hex::encode(&log.address))?;
                    maybe_pool_event(log, pool, tx)
                })
        })
        .collect();

    Events { pool_events }
}

// The symmetric `else if let Some(..)` chain reads better here than the `?` form clippy wants.
#[allow(clippy::question_mark)]
fn maybe_pool_event(log: &Log, pool: Pool, tx: &TransactionTrace) -> Option<PoolEvent> {
    let typ = if let Some(init) = Initialize::match_and_decode(log) {
        Typ::Initialize(pool_event::Initialize {
            sqrt_price: init.sqrt_price_x96.to_bytes_be().1,
            tick: init.tick.into(),
        })
    } else if let Some(swap) = Swap::match_and_decode(log) {
        Typ::Swap(pool_event::Swap {
            // amount0/amount1 are signed pool balance deltas (int256).
            amount_0: swap.amount0.to_signed_bytes_be(),
            amount_1: swap.amount1.to_signed_bytes_be(),
            sqrt_price: swap.sqrt_price_x96.to_bytes_be().1,
            liquidity: swap.liquidity.to_bytes_be().1,
            tick: swap.tick.into(),
        })
    } else if let Some(flash) = Flash::match_and_decode(log) {
        Typ::Flash(pool_event::Flash {
            paid_0: flash.paid0.to_bytes_be().1,
            paid_1: flash.paid1.to_bytes_be().1,
        })
    } else if let Some(mint) = Mint::match_and_decode(log) {
        Typ::Mint(pool_event::Mint {
            tick_lower: mint.tick_lower.into(),
            tick_upper: mint.tick_upper.into(),
            amount: mint.amount.to_bytes_be().1,
            amount_0: mint.amount0.to_bytes_be().1,
            amount_1: mint.amount1.to_bytes_be().1,
        })
    } else if let Some(burn) = Burn::match_and_decode(log) {
        Typ::Burn(pool_event::Burn {
            tick_lower: burn.tick_lower.into(),
            tick_upper: burn.tick_upper.into(),
            amount: burn.amount.to_bytes_be().1,
        })
    } else if let Some(collect) = Collect::match_and_decode(log) {
        Typ::Collect(pool_event::Collect {
            amount_0: collect.amount0.to_bytes_be().1,
            amount_1: collect.amount1.to_bytes_be().1,
        })
    } else if let Some(cp) = CollectProtocol::match_and_decode(log) {
        Typ::CollectProtocol(pool_event::CollectProtocol {
            amount_0: cp.amount0.to_bytes_be().1,
            amount_1: cp.amount1.to_bytes_be().1,
        })
    } else if let Some(fee_adjustment) = FeeAdjustment::match_and_decode(log) {
        Typ::FeeAdjustment(pool_event::FeeAdjustment {
            new_fee: fee_adjustment.new_fee.to_bytes_be().1,
        })
    } else {
        return None;
    };

    Some(PoolEvent {
        log_ordinal: log.ordinal,
        pool_address: log.address.clone(),
        token0: pool.token0,
        token1: pool.token1,
        transaction: Some(tx.into()),
        typ: Some(typ),
    })
}
