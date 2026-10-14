// Copyright (c) 2026 Everlong Labs Limited

//! The frames core hands its hooks (c104 @ `80abd43`): `IFLAMMHooks.PoolContext`
//! (`src/interfaces/core/flamm/IFLAMMHooks.sol:10-20`), `IFLAMMHooks.SwapContext`
//! (`IFLAMMHooks.sol:28-37`) and `IFLAMMLeverage.LeverContext`
//! (`src/interfaces/core/flamm/IFLAMMLeverage.sol:11-17`). `FLAMMGateLib.context` builds the pool
//! context ([`super::gate::context`]); the swap hook reads it for its lazy rescale
//! ([`super::hook`]), the leverage hook for its frame ([`super::levhook`]).

use alloy::primitives::U256;

/// `IFLAMMHooks.PoolContext` (`IFLAMMHooks.sol:10-20`): the factual pool state every hook call
/// receives. The pool-asset side is native base units; the loan side is aggregated into the
/// numeraire (N18); `price_wad` is N18 per pool-asset base unit. The swap hook reads only
/// `physical_pool_asset` and `posted_pool_asset` (`EverlongHook.sol:600`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolContext {
    pub physical_pool_asset: U256,
    pub posted_pool_asset: U256,
    pub liquid_loan_asset: U256,
    pub supplied_loan_asset: U256,
    pub debt_loan_asset: U256,
    pub share_supply: U256,
    pub price_wad: U256,
    /// `uint48 priceTs`.
    pub price_ts: u64,
    pub loan_count: u8,
}

/// `IFLAMMHooks.SwapContext` (`IFLAMMHooks.sol:28-37`) without `loanAsset`, an address no hook
/// reads. `pool_asset_in` true is a sell of the pool asset. The loan leg (`amount_in` of a buy,
/// `max_amount_out` of a sell) is N18.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapContext {
    pub pool: PoolContext,
    pub pool_asset_in: bool,
    pub amount_in: U256,
    pub max_amount_out: U256,
    pub loan_index: u8,
    pub cross_wad: U256,
    pub fee_floor_wad: U256,
}

/// `IFLAMMLeverage.LeverContext` (`IFLAMMLeverage.sol:11-17`): `amount_in` is poolAsset base units
/// for a lever-up and L18 for a lever-down; `max_out` is core's ceiling (the credit-zero room up,
/// the releasable poolAsset down), which the hook ignores; `spread_ppm` is the spread core
/// resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeverContext {
    pub pool: PoolContext,
    pub up: bool,
    pub spread_ppm: U256,
    pub amount_in: U256,
    pub max_out: U256,
}
