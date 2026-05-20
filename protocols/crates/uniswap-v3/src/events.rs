use substreams_ethereum::Event;

use crate::abi::pool::events::{
    Burn, Collect, CollectProtocol, Flash, Initialize, Mint, SetFeeProtocol, Swap,
};

#[derive(Clone)]
pub struct Pool {
    pub address: Vec<u8>,
    pub token0: Vec<u8>,
    pub token1: Vec<u8>,
}

pub trait PoolRegistry {
    fn get_pool(&self, pool_address_hex: &str) -> Option<&Pool>;
}

#[derive(Debug, Clone)]
pub struct TxRef {
    pub hash: Vec<u8>,
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub index: u64,
}

pub enum PoolEventKind {
    Initialize { sqrt_price: String, tick: i32 },
    Swap { amount0: String, amount1: String, sqrt_price: String, liquidity: String, tick: i32 },
    Mint { tick_lower: i32, tick_upper: i32, amount: String, amount0: String, amount1: String },
    Burn { tick_lower: i32, tick_upper: i32, amount: String, amount0: String, amount1: String },
    Collect { amount0: String, amount1: String },
    Flash { paid0: String, paid1: String },
    CollectProtocol { amount0: String, amount1: String },
    SetFeeProtocol { fee0_new: u64, fee1_new: u64 },
}

pub struct PoolEvent {
    pub log_ordinal: u64,
    pub pool_address: Vec<u8>,
    pub token0: Vec<u8>,
    pub token1: Vec<u8>,
    pub tx: TxRef,
    pub kind: PoolEventKind,
}

pub fn decode_log(
    log: &substreams_ethereum::pb::eth::v2::Log,
    pool: &Pool,
    tx: &TxRef,
) -> Option<PoolEvent> {
    let tx_ref =
        TxRef { hash: tx.hash.clone(), from: tx.from.clone(), to: tx.to.clone(), index: tx.index };

    if let Some(init) = Initialize::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::Initialize {
                sqrt_price: init.sqrt_price_x96.to_string(),
                tick: init.tick.into(),
            },
        });
    }

    if let Some(swap) = Swap::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::Swap {
                amount0: swap.amount0.to_string(),
                amount1: swap.amount1.to_string(),
                sqrt_price: swap.sqrt_price_x96.to_string(),
                liquidity: swap.liquidity.to_string(),
                tick: swap.tick.into(),
            },
        });
    }

    if let Some(flash) = Flash::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::Flash {
                paid0: flash.paid0.to_string(),
                paid1: flash.paid1.to_string(),
            },
        });
    }

    if let Some(mint) = Mint::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::Mint {
                tick_lower: mint.tick_lower.into(),
                tick_upper: mint.tick_upper.into(),
                amount: mint.amount.to_string(),
                amount0: mint.amount0.to_string(),
                amount1: mint.amount1.to_string(),
            },
        });
    }

    if let Some(burn) = Burn::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::Burn {
                tick_lower: burn.tick_lower.into(),
                tick_upper: burn.tick_upper.into(),
                amount: burn.amount.to_string(),
                amount0: burn.amount0.to_string(),
                amount1: burn.amount1.to_string(),
            },
        });
    }

    if let Some(collect) = Collect::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::Collect {
                amount0: collect.amount0.to_string(),
                amount1: collect.amount1.to_string(),
            },
        });
    }

    if let Some(set_fp) = SetFeeProtocol::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::SetFeeProtocol {
                fee0_new: set_fp.fee_protocol0_new.to_u64(),
                fee1_new: set_fp.fee_protocol1_new.to_u64(),
            },
        });
    }

    if let Some(cp) = CollectProtocol::match_and_decode(log) {
        return Some(PoolEvent {
            log_ordinal: log.ordinal,
            pool_address: pool.address.clone(),
            token0: pool.token0.clone(),
            token1: pool.token1.clone(),
            tx: tx_ref,
            kind: PoolEventKind::CollectProtocol {
                amount0: cp.amount0.to_string(),
                amount1: cp.amount1.to_string(),
            },
        });
    }

    None
}
