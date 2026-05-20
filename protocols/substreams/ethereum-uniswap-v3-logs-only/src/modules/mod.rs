pub use map_pool_created::map_pools_created;
pub use map_protocol_changes::map_protocol_changes;
pub use store_pools::store_pools;
use substreams_ethereum::pb::eth::v2::TransactionTrace;

use crate::pb::uniswap::v3::{
    events::{pool_event, PoolEvent as ProtoPoolEvent},
    LiquidityChange, LiquidityChangeType, TickDelta as ProtoTickDelta, Transaction,
};
use uniswap_v3_core::{
    events::{PoolEvent as CorePoolEvent, PoolEventKind, TxRef},
    liquidity::{LiquidityChangeKind, LiquidityDelta as CoreLiquidityDelta},
    ticks::TickDelta as CoreTickDelta,
};

#[path = "1_map_pool_created.rs"]
mod map_pool_created;

#[path = "2_store_pools.rs"]
mod store_pools;

#[path = "3_map_events.rs"]
mod map_events;

#[path = "4_map_and_store_balance_changes.rs"]
mod map_store_balance_changes;

#[path = "4_map_and_store_ticks.rs"]
mod map_store_ticks;

#[path = "4_map_and_store_liquidity.rs"]
mod map_store_liquidity;

#[path = "5_map_protocol_changes.rs"]
mod map_protocol_changes;

impl From<TransactionTrace> for Transaction {
    fn from(value: TransactionTrace) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index.into() }
    }
}

impl From<&TransactionTrace> for Transaction {
    fn from(value: &TransactionTrace) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index.into(),
        }
    }
}

impl From<&Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: &Transaction) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index,
        }
    }
}

impl From<Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: Transaction) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index }
    }
}

/// Converts a proto `PoolEvent` to a core `PoolEvent`.
/// Proto stores addresses as hex strings; core uses raw bytes.
impl From<&ProtoPoolEvent> for CorePoolEvent {
    fn from(proto: &ProtoPoolEvent) -> Self {
        let pool_address = hex::decode(&proto.pool_address).unwrap_or_default();
        let token0 = hex::decode(&proto.token0).unwrap_or_default();
        let token1 = hex::decode(&proto.token1).unwrap_or_default();
        let tx = proto.transaction.as_ref().map_or_else(
            || TxRef { hash: vec![], from: vec![], to: vec![], index: 0 },
            |t| TxRef {
                hash: t.hash.clone(),
                from: t.from.clone(),
                to: t.to.clone(),
                index: t.index,
            },
        );
        let kind = match proto.r#type.as_ref().unwrap() {
            pool_event::Type::Initialize(e) => {
                PoolEventKind::Initialize { sqrt_price: e.sqrt_price.clone(), tick: e.tick }
            }
            pool_event::Type::Swap(e) => PoolEventKind::Swap {
                amount0: e.amount_0.clone(),
                amount1: e.amount_1.clone(),
                sqrt_price: e.sqrt_price.clone(),
                liquidity: e.liquidity.clone(),
                tick: e.tick,
            },
            pool_event::Type::Mint(e) => PoolEventKind::Mint {
                tick_lower: e.tick_lower,
                tick_upper: e.tick_upper,
                amount: e.amount.clone(),
                amount0: e.amount_0.clone(),
                amount1: e.amount_1.clone(),
            },
            pool_event::Type::Burn(e) => PoolEventKind::Burn {
                tick_lower: e.tick_lower,
                tick_upper: e.tick_upper,
                amount: e.amount.clone(),
                amount0: e.amount_0.clone(),
                amount1: e.amount_1.clone(),
            },
            pool_event::Type::Collect(e) => {
                PoolEventKind::Collect { amount0: e.amount_0.clone(), amount1: e.amount_1.clone() }
            }
            pool_event::Type::Flash(e) => {
                PoolEventKind::Flash { paid0: e.paid_0.clone(), paid1: e.paid_1.clone() }
            }
            pool_event::Type::CollectProtocol(e) => PoolEventKind::CollectProtocol {
                amount0: e.amount_0.clone(),
                amount1: e.amount_1.clone(),
            },
            pool_event::Type::SetFeeProtocol(e) => PoolEventKind::SetFeeProtocol {
                fee0_new: e.fee_protocol_0_new,
                fee1_new: e.fee_protocol_1_new,
            },
        };
        CorePoolEvent { log_ordinal: proto.log_ordinal, pool_address, token0, token1, tx, kind }
    }
}

/// Converts a core `PoolEvent` to a proto `PoolEvent`.
/// Core uses raw bytes; proto stores addresses as hex strings.
impl From<CorePoolEvent> for ProtoPoolEvent {
    fn from(e: CorePoolEvent) -> Self {
        let transaction = Some(crate::pb::uniswap::v3::Transaction {
            hash: e.tx.hash,
            from: e.tx.from,
            to: e.tx.to,
            index: e.tx.index,
        });
        let r#type = Some(match e.kind {
            PoolEventKind::Initialize { sqrt_price, tick } => {
                pool_event::Type::Initialize(pool_event::Initialize { sqrt_price, tick })
            }
            PoolEventKind::Swap { amount0, amount1, sqrt_price, liquidity, tick } => {
                pool_event::Type::Swap(pool_event::Swap {
                    sender: String::new(),
                    recipient: String::new(),
                    amount_0: amount0,
                    amount_1: amount1,
                    sqrt_price,
                    liquidity,
                    tick,
                })
            }
            PoolEventKind::Mint { tick_lower, tick_upper, amount, amount0, amount1 } => {
                pool_event::Type::Mint(pool_event::Mint {
                    sender: String::new(),
                    owner: String::new(),
                    tick_lower,
                    tick_upper,
                    amount,
                    amount_0: amount0,
                    amount_1: amount1,
                })
            }
            PoolEventKind::Burn { tick_lower, tick_upper, amount, amount0, amount1 } => {
                pool_event::Type::Burn(pool_event::Burn {
                    owner: String::new(),
                    tick_lower,
                    tick_upper,
                    amount,
                    amount_0: amount0,
                    amount_1: amount1,
                })
            }
            PoolEventKind::Collect { amount0, amount1 } => {
                pool_event::Type::Collect(pool_event::Collect {
                    owner: String::new(),
                    recipient: String::new(),
                    tick_lower: 0,
                    tick_upper: 0,
                    amount_0: amount0,
                    amount_1: amount1,
                })
            }
            PoolEventKind::Flash { paid0, paid1 } => pool_event::Type::Flash(pool_event::Flash {
                sender: String::new(),
                recipient: String::new(),
                amount_0: String::new(),
                amount_1: String::new(),
                paid_0: paid0,
                paid_1: paid1,
            }),
            PoolEventKind::CollectProtocol { amount0, amount1 } => {
                pool_event::Type::CollectProtocol(pool_event::CollectProtocol {
                    sender: String::new(),
                    recipient: String::new(),
                    amount_0: amount0,
                    amount_1: amount1,
                })
            }
            PoolEventKind::SetFeeProtocol { fee0_new, fee1_new } => {
                pool_event::Type::SetFeeProtocol(pool_event::SetFeeProtocol {
                    fee_protocol_0_old: 0,
                    fee_protocol_1_old: 0,
                    fee_protocol_0_new: fee0_new,
                    fee_protocol_1_new: fee1_new,
                })
            }
        });
        ProtoPoolEvent {
            log_ordinal: e.log_ordinal,
            pool_address: hex::encode(&e.pool_address),
            token0: hex::encode(&e.token0),
            token1: hex::encode(&e.token1),
            transaction,
            r#type,
        }
    }
}

impl From<CoreTickDelta> for ProtoTickDelta {
    fn from(d: CoreTickDelta) -> Self {
        Self {
            pool_address: d.pool_address,
            tick_index: d.tick_index,
            liquidity_net_delta: d
                .liquidity_net_delta
                .to_signed_bytes_be(),
            ordinal: 0,
            transaction: None,
        }
    }
}

impl From<CoreLiquidityDelta> for LiquidityChange {
    fn from(d: CoreLiquidityDelta) -> Self {
        let change_type = match d.kind {
            LiquidityChangeKind::Delta => LiquidityChangeType::Delta,
            LiquidityChangeKind::Absolute => LiquidityChangeType::Absolute,
        };
        Self {
            pool_address: d.pool_address,
            value: d.value.to_signed_bytes_be(),
            change_type: change_type.into(),
            ordinal: 0,
            transaction: None,
        }
    }
}
