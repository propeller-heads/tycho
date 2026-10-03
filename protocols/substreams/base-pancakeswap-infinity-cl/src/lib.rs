#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! PancakeSwap Infinity concentrated-liquidity pools.
//!
//! Reference: `protocols/substreams/ethereum-uniswap-v4` (`shared/` + `no-hooks/`). Infinity's
//! CLPoolManager is a Uniswap v4 fork with three differences that matter here:
//!
//! 1. Funds and the lock live in a separate Vault (`0x238a358808379702088667322f80aC48bAd5e6c4`),
//!    so `balance_owner` is the Vault.
//! 2. `tickSpacing` and the hook permission bitmap are packed in `PoolKey.parameters`
//!    (`parameters.rs`); `Initialize` carries `parameters` instead of `tickSpacing`.
//! 3. The protocol fee is set at `initialize` without an event; it is read from the pool's slot0
//!    storage write in the creating transaction.
//!
//! Solidity references are paths in `pancakeswap/infinity-core` at commit `d0e87933`
//! (<https://github.com/pancakeswap/infinity-core/tree/d0e879334da8ea789a895d864dbe34259ea9fb65>).
//!
//! Attribute names match v4 so `UniswapV4State` decodes the components. Static: `key_lp_fee`,
//! `tick_spacing`, `pool_id`, `parameters`, `pool_manager`, optional `hook_address`. State:
//! `balance_owner`, `liquidity`, `sqrt_price_x96`, `tick`, `protocol_fees/zero2one`,
//! `protocol_fees/one2zero`, `ticks/{i}/net-liquidity`.

pub mod abi;
pub mod modules;
pub mod parameters;
pub mod pb;

pub use modules::*;
