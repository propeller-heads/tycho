use crate::{
    abi::pool_manager::events::{Initialize, ModifyLiquidity, ProtocolFeeUpdated, Swap},
    pb::uniswap::v4::{
        events::{pool_event, pool_event::Type, PoolEvent},
        Events, Pool,
    },
};
use anyhow::Ok;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;

#[substreams::handlers::map]
pub fn map_events(
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<Events, anyhow::Error> {
    let mut pool_manager_events = block
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
                .filter_map(|log| log_to_event(log, &tx, &pools_store))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    pool_manager_events.sort_unstable_by_key(|e| e.log_ordinal);

    Ok(Events { pool_events: pool_manager_events })
}

fn log_to_event(
    event: &Log,
    tx: &TransactionTrace,
    pools_store: &StoreGetProto<Pool>,
) -> Option<PoolEvent> {
    if let Some(init) = Initialize::match_and_decode(event) {
        // We need to track initialization again to keep track of pool current tick, which is set on
        // initialization and changed on swaps.
        let pool_id = init.id.to_vec().to_hex();
        let pool = pools_store.get_last(format!("{}:{}", "pool", &pool_id))?;
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_id,
            currency0: pool.currency0.to_hex(),
            currency1: pool.currency1.to_hex(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Initialize(pool_event::Initialize {
                sqrt_price_x96: init.sqrt_price_x96.to_string(),
                tick: init.tick.into(),
                fee: init.fee.into(),
                tick_spacing: init.tick_spacing.into(),
                hooks: init.hooks.to_vec().to_hex(),
            })),
        })
    } else if let Some(swap) = Swap::match_and_decode(event) {
        let pool_id = swap.id.to_vec().to_hex();
        let pool = pools_store.get_last(format!("{}:{}", "pool", &pool_id))?;
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_id,
            currency0: pool.currency0.to_hex(),
            currency1: pool.currency1.to_hex(),
            transaction: Some(tx.into()),
            r#type: Some(Type::Swap(pool_event::Swap {
                sender: swap.sender.to_hex(),
                amount0: swap.amount0.to_string(),
                amount1: swap.amount1.to_string(),
                sqrt_price_x96: swap.sqrt_price_x96.to_string(),
                liquidity: swap.liquidity.to_string(),
                tick: swap.tick.into(),
                fee: swap.fee.into(),
            })),
        })
    // Skipped because Donate doesn't seem to affect pool liquidity?
    // } else if let Some(flash) = Donate::match_and_decode(event) {
    //     let pool_id = flash.id.to_vec().to_hex();
    //     let pool = pools_store.get_last(format!("{}:{}", "pool", &pool_id))?;
    //     Some(PoolEvent {
    //         log_ordinal: event.ordinal,
    //         pool_id,
    //         currency0: pool.currency0.to_hex(),
    //         currency1: pool.currency1.to_hex(),
    //         transaction: Some(tx.into()),
    //         r#type: Some(Type::Donate(pool_event::Donate {
    //             sender: flash.sender.to_hex(),
    //             amount0: flash.amount0.to_string(),
    //             amount1: flash.amount1.to_string(),
    //         })),
    //     })
    } else if let Some(modify_liquidity) = ModifyLiquidity::match_and_decode(event) {
        let pool_id = modify_liquidity.id.to_vec().to_hex();
        let pool = pools_store.get_last(format!("{}:{}", "pool", &pool_id))?;
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_id,
            currency0: pool.currency0.to_hex(),
            currency1: pool.currency1.to_hex(),
            transaction: Some(tx.into()),
            r#type: Some(Type::ModifyLiquidity(pool_event::ModifyLiquidity {
                sender: modify_liquidity.sender.to_hex(),
                tick_lower: modify_liquidity.tick_lower.into(),
                tick_upper: modify_liquidity.tick_upper.into(),
                liquidity_delta: modify_liquidity
                    .liquidity_delta
                    .to_string(),
                salt: modify_liquidity.salt.to_vec().to_hex(),
            })),
        })
    } else if let Some(protocol_fee_updated) = ProtocolFeeUpdated::match_and_decode(event) {
        let pool_id = protocol_fee_updated
            .id
            .to_vec()
            .to_hex();
        let pool = pools_store.get_last(format!("{}:{}", "pool", &pool_id))?;
        Some(PoolEvent {
            log_ordinal: event.ordinal,
            pool_id: pool_id.clone(),
            currency0: pool.currency0.to_hex(),
            currency1: pool.currency1.to_hex(),
            transaction: Some(tx.into()),
            r#type: Some(Type::ProtocolFeeUpdated(pool_event::ProtocolFeeUpdated {
                pool_id,
                protocol_fee: protocol_fee_updated.protocol_fee.into(),
            })),
        })
    } else {
        None
    }
}
