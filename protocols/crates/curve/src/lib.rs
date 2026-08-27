//! Native Curve processor: derives the protocol deltas a pending block would produce, without a
//! Substreams runtime.
//!
//! Curve is a VM protocol. Its state lives in contract storage, not in its logs, so unlike the
//! UniswapV2/V3/V4 processors this one cannot decode a pending block's pool state from
//! `TxInput`s alone. It instead reads the pool's view getters against the pending block's
//! post-execution accounts, which [`tycho_common::models::blockchain::PendingBlock`] carries, and
//! passes the readings on as a state-delta attribute for `CurveState` to rebuild from.
//!
//! Component balances take the other route: they are derived from the block's ERC20 transfer logs
//! exactly as the substreams package derives them, so they can be compared byte for byte against
//! it.

pub mod processor;

mod balance;
mod overrides;
mod registry;
