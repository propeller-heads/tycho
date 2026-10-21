use anyhow::Ok;
use substreams::{
    store::{StoreGet, StoreGetProto},
    Hex,
};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;

use crate::{
    abi::pool::events::{
        Burn, Collect, CollectProtocol, Flash, Initialize, Mint, SetFeeProtocol, Swap,
    },
    pb::uniswap::v3::{
        events::{
            pool_event::{self, Type},
            PoolEvent,
        },
        Events, Pool,
    },
};

#[substreams::handlers::map]
pub fn map_events(
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<Events, anyhow::Error> {
    let mut pool_events = block
        .transaction_traces
        .into_iter()
        .filter(|tx| tx.status == 1)
        .flat_map(|tx| {
            let receipt = tx
                .receipt
                .as_ref()
                .expect("all transaction traces have a receipt");

            receipt
                .logs
                .iter()
                .filter_map(|log| {
                    let key = format!("{}:{}", "Pool", log.address.to_hex());
                    // Skip if the log is not from a known uniswapV3 pool.
                    if let Some(pool) = pools_store.get_last(key) {
                        log_to_event(log, pool, &tx)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    pool_events.sort_unstable_by_key(|e| e.log_ordinal);

    Ok(Events { pool_events })
}

fn log_to_event(event: &Log, pool: Pool, tx: &TransactionTrace) -> Option<PoolEvent> {
    if let Some(init) = Initialize::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Initialize(pool_event::Initialize {
                sqrt_price: init.sqrt_price_x96.to_string(),
                tick: init.tick.into(),
            })),
        })
    } else if let Some(swap) = Swap::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Swap(pool_event::Swap {
                sender: Hex(swap.sender).to_string(),
                recipient: Hex(swap.recipient).to_string(),
                amount_0: swap.amount0.to_string(),
                amount_1: swap.amount1.to_string(),
                sqrt_price: swap.sqrt_price_x96.to_string(),
                liquidity: swap.liquidity.to_string(),
                tick: swap.tick.into(),
            })),
        })
    } else if let Some(flash) = Flash::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Flash(pool_event::Flash {
                sender: Hex(flash.sender).to_string(),
                recipient: Hex(flash.recipient).to_string(),
                amount_0: flash.amount0.to_string(),
                amount_1: flash.amount1.to_string(),
                paid_0: flash.paid0.to_string(),
                paid_1: flash.paid1.to_string(),
            })),
        })
    } else if let Some(mint) = Mint::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Mint(pool_event::Mint {
                sender: Hex(mint.sender).to_string(),
                owner: Hex(mint.owner).to_string(),
                tick_lower: mint.tick_lower.into(),
                tick_upper: mint.tick_upper.into(),
                amount: mint.amount.to_string(),
                amount_0: mint.amount0.to_string(),
                amount_1: mint.amount1.to_string(),
            })),
        })
    } else if let Some(burn) = Burn::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Burn(pool_event::Burn {
                owner: Hex(burn.owner).to_string(),
                tick_lower: burn.tick_lower.into(),
                tick_upper: burn.tick_upper.into(),
                amount: burn.amount.to_string(),
                amount_0: burn.amount0.to_string(),
                amount_1: burn.amount1.to_string(),
            })),
        })
    } else if let Some(collect) = Collect::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Collect(pool_event::Collect {
                owner: Hex(collect.owner).to_string(),
                recipient: Hex(collect.recipient).to_string(),
                tick_lower: collect.tick_lower.into(),
                tick_upper: collect.tick_upper.into(),
                amount_0: collect.amount0.to_string(),
                amount_1: collect.amount1.to_string(),
            })),
        })
    } else if let Some(set_fp) = SetFeeProtocol::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::SetFeeProtocol(pool_event::SetFeeProtocol {
                fee_protocol_0_old: set_fp.fee_protocol0_old.to_u64(),
                fee_protocol_1_old: set_fp.fee_protocol1_old.to_u64(),
                fee_protocol_0_new: set_fp.fee_protocol0_new.to_u64(),
                fee_protocol_1_new: set_fp.fee_protocol1_new.to_u64(),
            })),
        })
    } else if let Some(cp) = CollectProtocol::match_and_decode(event) {
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_address: Hex(pool.address).to_string(),
            token0: Hex(pool.token0).to_string(),
            token1: Hex(pool.token1).to_string(),
            transaction: Some(tx.into()),
            r#type: Some(Type::CollectProtocol(pool_event::CollectProtocol {
                sender: Hex(cp.sender).to_string(),
                recipient: Hex(cp.recipient).to_string(),
                amount_0: cp.amount0.to_string(),
                amount_1: cp.amount1.to_string(),
            })),
        })
    } else {
        None
    }
}
