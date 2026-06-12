use substreams_ethereum::Event;

use crate::abi::pool_manager::events::{Initialize, ModifyLiquidity, ProtocolFeeUpdated, Swap};

/// A UniswapV4 pool tracked by the processor. Pools are identified by their
/// 32-byte `PoolId` rather than a contract address — all events are emitted
/// by the singleton `PoolManager`.
#[derive(Clone)]
pub struct Pool {
    pub id: Vec<u8>,
    pub currency0: Vec<u8>,
    pub currency1: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TxRef {
    pub hash: Vec<u8>,
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub index: u64,
}

pub enum PoolEventKind {
    Initialize {
        sqrt_price: String,
        tick: i32,
    },
    Swap {
        amount0: String,
        amount1: String,
        sqrt_price: String,
        liquidity: String,
        tick: i32,
        fee: u32,
    },
    ModifyLiquidity {
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: String,
    },
    ProtocolFeeUpdated {
        protocol_fee: u32,
    },
}

pub struct PoolEvent {
    pub log_ordinal: u64,
    pub pool_id: Vec<u8>,
    pub currency0: Vec<u8>,
    pub currency1: Vec<u8>,
    pub tx: TxRef,
    pub kind: PoolEventKind,
}

/// Decodes a `PoolManager` log into the pool id it targets and the event payload.
///
/// Returns `None` for logs that are not one of the handled UniswapV4 events
/// (`Initialize`, `Swap`, `ModifyLiquidity`, `ProtocolFeeUpdated`). `Donate` is
/// intentionally ignored — it does not affect pool liquidity or tracked state.
pub fn decode_pool_event(
    log: &substreams_ethereum::pb::eth::v2::Log,
) -> Option<(Vec<u8>, PoolEventKind)> {
    if let Some(init) = Initialize::match_and_decode(log) {
        return Some((
            init.id.to_vec(),
            PoolEventKind::Initialize {
                sqrt_price: init.sqrt_price_x96.to_string(),
                tick: init.tick.into(),
            },
        ));
    }

    if let Some(swap) = Swap::match_and_decode(log) {
        return Some((
            swap.id.to_vec(),
            PoolEventKind::Swap {
                amount0: swap.amount0.to_string(),
                amount1: swap.amount1.to_string(),
                sqrt_price: swap.sqrt_price_x96.to_string(),
                liquidity: swap.liquidity.to_string(),
                tick: swap.tick.into(),
                fee: swap.fee.into(),
            },
        ));
    }

    if let Some(modify) = ModifyLiquidity::match_and_decode(log) {
        return Some((
            modify.id.to_vec(),
            PoolEventKind::ModifyLiquidity {
                tick_lower: modify.tick_lower.into(),
                tick_upper: modify.tick_upper.into(),
                liquidity_delta: modify.liquidity_delta.to_string(),
            },
        ));
    }

    if let Some(fee_update) = ProtocolFeeUpdated::match_and_decode(log) {
        return Some((
            fee_update.id.to_vec(),
            PoolEventKind::ProtocolFeeUpdated { protocol_fee: fee_update.protocol_fee.into() },
        ));
    }

    None
}
