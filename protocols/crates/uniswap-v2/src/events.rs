use substreams_ethereum::Event;

use crate::abi::pool::events::Sync;

#[derive(Clone)]
pub struct Pool {
    pub address: Vec<u8>,
    pub token0: Vec<u8>,
    pub token1: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TxRef {
    pub hash: Vec<u8>,
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub index: u64,
}

pub enum PoolEventKind {
    /// Emitted on every reserve-altering call. Carries the pool's absolute reserves,
    /// so it fully describes the post-event state without any prior context.
    Sync { reserve0: String, reserve1: String },
}

pub struct PoolEvent {
    pub log_ordinal: u64,
    pub pool_address: Vec<u8>,
    pub token0: Vec<u8>,
    pub token1: Vec<u8>,
    pub tx: TxRef,
    pub kind: PoolEventKind,
}

/// Decodes a pool log into a `PoolEvent`, or `None` if it is not a handled event.
///
/// UniswapV2 only needs the `Sync` event: it is emitted on every reserve-altering
/// call (mint, burn, swap), so the reserves it carries are sufficient to track pool
/// state and component balances.
pub fn decode_log(
    log: &substreams_ethereum::pb::eth::v2::Log,
    pool: &Pool,
    tx: &TxRef,
) -> Option<PoolEvent> {
    let sync = Sync::match_and_decode(log)?;
    Some(PoolEvent {
        log_ordinal: log.ordinal,
        pool_address: pool.address.clone(),
        token0: pool.token0.clone(),
        token1: pool.token1.clone(),
        tx: tx.clone(),
        kind: PoolEventKind::Sync {
            reserve0: sync.reserve0.to_string(),
            reserve1: sync.reserve1.to_string(),
        },
    })
}
