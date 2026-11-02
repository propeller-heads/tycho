use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log, TransactionTrace},
    Event,
};

use crate::{
    abi::pool::events::{
        Burn, Collect, Fee, Flash, Initialize, Mint, Plugin, PluginConfig, Skim, Swap,
    },
    pb::nami::v4::{
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
            sqrt_price: init.price.to_bytes_be().1,
            tick: init.tick.into(),
        })
    } else if let Some(swap) = Swap::match_and_decode(log) {
        Typ::Swap(pool_event::Swap {
            amount_0: swap.amount0.to_signed_bytes_be(),
            amount_1: swap.amount1.to_signed_bytes_be(),
            sqrt_price: swap.price.to_bytes_be().1,
            liquidity: swap.liquidity.to_bytes_be().1,
            tick: swap.tick.into(),
        })
    } else if let Some(mint) = Mint::match_and_decode(log) {
        Typ::Mint(pool_event::Mint {
            tick_lower: mint.bottom_tick.into(),
            tick_upper: mint.top_tick.into(),
            amount: mint.liquidity_amount.to_bytes_be().1,
            amount_0: mint.amount0.to_bytes_be().1,
            amount_1: mint.amount1.to_bytes_be().1,
        })
    } else if let Some(burn) = Burn::match_and_decode(log) {
        Typ::Burn(pool_event::Burn {
            tick_lower: burn.bottom_tick.into(),
            tick_upper: burn.top_tick.into(),
            amount: burn.liquidity_amount.to_bytes_be().1,
        })
    } else if let Some(collect) = Collect::match_and_decode(log) {
        Typ::Collect(pool_event::Collect {
            amount_0: collect.amount0.to_bytes_be().1,
            amount_1: collect.amount1.to_bytes_be().1,
        })
    } else if let Some(flash) = Flash::match_and_decode(log) {
        Typ::Flash(pool_event::Flash {
            paid_0: flash.paid0.to_bytes_be().1,
            paid_1: flash.paid1.to_bytes_be().1,
        })
    } else if let Some(skim) = Skim::match_and_decode(log) {
        Typ::Skim(pool_event::Skim {
            amount_0: skim.amount0.to_bytes_be().1,
            amount_1: skim.amount1.to_bytes_be().1,
        })
    } else if let Some(fee) = Fee::match_and_decode(log) {
        Typ::Fee(pool_event::Fee { fee: fee.fee.to_u64() as u32 })
    } else if let Some(plugin) = Plugin::match_and_decode(log) {
        Typ::Plugin(pool_event::Plugin { plugin: plugin.new_plugin_address })
    } else if let Some(config) = PluginConfig::match_and_decode(log) {
        Typ::PluginConfig(pool_event::PluginConfig {
            plugin_config: config.new_plugin_config.to_u64() as u32,
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
