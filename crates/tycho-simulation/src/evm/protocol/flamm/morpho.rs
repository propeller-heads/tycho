// Copyright (c) 2026 Everlong Labs Limited

//! Morpho Blue (`0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` on Base, morpho-org/morpho-blue
//! v1.0.0, the verified singleton) as the FLAMM financing account drives it: `MathLib`,
//! `SharesMathLib` and the market transitions of `src/Morpho.sol`.
//!
//! Every function keeps Solidity's operation order: `MathLib.mulDivDown` is `(x * y) / d` with a
//! CHECKED 256-bit product (the arithmetic panic Morpho reverts with), `mulDivUp` is `(x * y + (d -
//! 1)) / d`, and every `uint128` total is re-checked on write (`UtilsLib.toUint128`'s require, then
//! the checked `uint128` add or subtract). Morpho reverts with `require` strings (`ErrorsLib`) and
//! Solidity panics rather than custom errors; the [`FlammError::Morpho*`](FlammError) variants name
//! the strings so a refused transition says which check the real call would fail.

use alloy::primitives::U256;

use super::{
    error::FlammError,
    irm::borrow_rate,
    math::{checked_add, checked_div, checked_mul, checked_sub, WAD},
};

/// `SharesMathLib.VIRTUAL_SHARES = 1e6` (`SharesMathLib.sol:20`).
pub const VIRTUAL_SHARES: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
/// `SharesMathLib.VIRTUAL_ASSETS = 1` (`SharesMathLib.sol:24`).
pub const VIRTUAL_ASSETS: U256 = U256::from_limbs([1, 0, 0, 0]);
/// `ConstantsLib.ORACLE_PRICE_SCALE = 1e36` (`ConstantsLib.sol:8`).
pub const ORACLE_PRICE_SCALE: U256 =
    U256::from_limbs([0xb34b_9f10_0000_0000, 0x00c0_97ce_7bc9_0715, 0, 0]);
/// `type(uint128).max`.
pub const MAX_UINT128: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
/// `2 * WAD`.
pub const TWO_WAD: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);
/// `3 * WAD`.
pub const THREE_WAD: U256 = U256::from_limbs([3_000_000_000_000_000_000, 0, 0, 0]);

/// Morpho's `Market` storage struct as `market(id)` returns it (`IMorpho.sol`); every total is a
/// `uint128` on chain and is re-checked on every write here.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Market {
    pub total_supply_assets: U256,
    pub total_supply_shares: U256,
    pub total_borrow_assets: U256,
    pub total_borrow_shares: U256,
    pub last_update: U256,
    pub fee: U256,
}

/// Morpho's `Position` of the financing account (`onBehalf == account`), `position(id, account)`:
/// `supplyShares` is a `uint256`, `borrowShares` and `collateral` are `uint128`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub supply_shares: U256,
    pub borrow_shares: U256,
    pub collateral: U256,
}

/// One Morpho market as one financing account sees it: Blue's market totals and the account's
/// position, the market params the account registered (`lltv`, whether an IRM is set), the
/// `AdaptiveCurveIrm`'s `rateAtTarget(id)` and the market oracle's answer. Value fields only, so a
/// copy is a deep copy.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VenueMarket {
    pub market: Market,
    pub position: Position,
    /// `MarketParams.lltv`, WAD-scaled.
    pub lltv: U256,
    /// `marketParams.irm != address(0)`; Blue then accrues nothing and the account reads a zero
    /// rate.
    pub has_irm: bool,
    /// False while the IRM reverts (`borrowRateView` and `borrowRate` alike): the account's grace
    /// / quarantine branches and Blue's accrual revert key off it.
    pub irm_readable: bool,
    /// `AdaptiveCurveIrm.rateAtTarget(id)`, the raw `int256` slot word (never negative on any
    /// market the IRM has touched; read as two's complement by the rate model).
    pub rate_at_target: U256,
    /// `MorphoBlueAccount.oraclePrice(id)`: the oracle answered non-zero inside the 300k gas cap.
    /// Blue's own health check reads the same price uncapped.
    pub oracle_ok: bool,
    pub oracle_price: U256,
    /// Separates the two cases `oraclePrice` folds into `ok == false`: the oracle answered
    /// `price() == 0` without reverting. Blue's health check then reads that zero (and fails
    /// `insufficient collateral`) where a reverting oracle bubbles its own revert. A state
    /// read through `oraclePrice` alone cannot tell them apart and leaves it false: the
    /// transition is refused either way, only the reported revert differs.
    pub oracle_zero: bool,
}

// ------------------------------------------------------------------ MathLib / SharesMathLib

/// `MathLib.mulDivDown(x, y, d)` (`MathLib.sol:27-29`): `(x * y) / d`, the product checked.
pub fn mul_div_down(x: U256, y: U256, d: U256) -> Result<U256, FlammError> {
    checked_div(checked_mul(x, y)?, d)
}

/// `MathLib.mulDivUp(x, y, d)` (`MathLib.sol:32-34`): `(x * y + (d - 1)) / d`, the product, the
/// decrement and the sum checked.
pub fn mul_div_up(x: U256, y: U256, d: U256) -> Result<U256, FlammError> {
    let prod = checked_mul(x, y)?;
    let dm1 = checked_sub(d, U256::from(1))?;
    checked_div(checked_add(prod, dm1)?, d)
}

/// `MathLib.wMulDown(x, y)` (`MathLib.sol:12-14`).
pub fn w_mul_down(x: U256, y: U256) -> Result<U256, FlammError> {
    mul_div_down(x, y, WAD)
}

/// `MathLib.wDivDown(x, y)` (`MathLib.sol:17-19`).
pub fn w_div_down(x: U256, y: U256) -> Result<U256, FlammError> {
    mul_div_down(x, WAD, y)
}

/// `MathLib.wTaylorCompounded(x, n)` (`MathLib.sol:38-44`): the first three terms of `e^(x * n) -
/// 1`.
pub fn w_taylor_compounded(x: U256, n: U256) -> Result<U256, FlammError> {
    let first = checked_mul(x, n)?;
    let second = mul_div_down(first, first, TWO_WAD)?;
    let third = mul_div_down(second, first, THREE_WAD)?;
    checked_add(checked_add(first, second)?, third)
}

/// `SharesMathLib.toSharesDown(assets, totalAssets, totalShares)` (`SharesMathLib.sol:27-29`):
/// `assets * (totalShares + 1e6) / (totalAssets + 1)`.
pub fn to_shares_down(
    assets: U256,
    total_assets: U256,
    total_shares: U256,
) -> Result<U256, FlammError> {
    let vs = checked_add(total_shares, VIRTUAL_SHARES)?;
    let va = checked_add(total_assets, VIRTUAL_ASSETS)?;
    mul_div_down(assets, vs, va)
}

/// `SharesMathLib.toAssetsDown(shares, totalAssets, totalShares)` (`SharesMathLib.sol:32-34`):
/// `shares * (totalAssets + 1) / (totalShares + 1e6)`.
pub fn to_assets_down(
    shares: U256,
    total_assets: U256,
    total_shares: U256,
) -> Result<U256, FlammError> {
    let va = checked_add(total_assets, VIRTUAL_ASSETS)?;
    let vs = checked_add(total_shares, VIRTUAL_SHARES)?;
    mul_div_down(shares, va, vs)
}

/// `SharesMathLib.toSharesUp` (`SharesMathLib.sol:37-39`).
pub fn to_shares_up(
    assets: U256,
    total_assets: U256,
    total_shares: U256,
) -> Result<U256, FlammError> {
    let vs = checked_add(total_shares, VIRTUAL_SHARES)?;
    let va = checked_add(total_assets, VIRTUAL_ASSETS)?;
    mul_div_up(assets, vs, va)
}

/// `SharesMathLib.toAssetsUp` (`SharesMathLib.sol:42-44`).
pub fn to_assets_up(
    shares: U256,
    total_assets: U256,
    total_shares: U256,
) -> Result<U256, FlammError> {
    let va = checked_add(total_assets, VIRTUAL_ASSETS)?;
    let vs = checked_add(total_shares, VIRTUAL_SHARES)?;
    mul_div_up(shares, va, vs)
}

/// `total += x.toUint128()`: the cast's require (`UtilsLib.sol:27-30`), then the checked `uint128`
/// add.
pub fn add_128(total: &mut U256, x: U256) -> Result<(), FlammError> {
    if x > MAX_UINT128 {
        return Err(FlammError::MorphoMaxUint128Exceeded);
    }
    let z = checked_add(*total, x)?;
    if z > MAX_UINT128 {
        return Err(FlammError::PanicArithmetic);
    }
    *total = z;
    Ok(())
}

/// `total -= x.toUint128()`: the cast's require, then the checked `uint128` subtraction.
pub fn sub_128(total: &mut U256, x: U256) -> Result<(), FlammError> {
    if x > MAX_UINT128 {
        return Err(FlammError::MorphoMaxUint128Exceeded);
    }
    if x > *total {
        return Err(FlammError::PanicArithmetic);
    }
    *total -= x;
    Ok(())
}

// ------------------------------------------------------------------ Morpho.sol transitions

impl VenueMarket {
    /// `Morpho._accrueInterest` (`Morpho.sol:483-509`) at `now`: the IRM's (mutating) `borrowRate`
    /// over the stored market, the 3-term Taylor interest onto both borrow and supply totals,
    /// the fee shares minted at the post-interest supply less the fee, and `lastUpdate`
    /// stamped. A zero elapsed returns before the IRM is touched; a market with no IRM only
    /// stamps `lastUpdate`. The IRM runs even on a market with no borrows, so `rateAtTarget`
    /// still adapts.
    pub fn accrue(&mut self, now: u64) -> Result<(), FlammError> {
        let now_u = U256::from(now);
        if now_u < self.market.last_update {
            return Err(FlammError::PanicArithmetic);
        }
        let elapsed = now_u - self.market.last_update;
        if elapsed.is_zero() {
            return Ok(());
        }
        if self.has_irm {
            if !self.irm_readable {
                return Err(FlammError::MorphoIrmReverted);
            }
            let (rate, end) = borrow_rate(&self.market, self.rate_at_target, now)?;
            self.rate_at_target = end;
            let growth = w_taylor_compounded(rate, elapsed)?;
            let interest = w_mul_down(self.market.total_borrow_assets, growth)?;
            add_128(&mut self.market.total_borrow_assets, interest)?;
            add_128(&mut self.market.total_supply_assets, interest)?;
            if !self.market.fee.is_zero() {
                let fee_amount = w_mul_down(interest, self.market.fee)?;
                let net = checked_sub(self.market.total_supply_assets, fee_amount)?;
                let fee_shares = to_shares_down(fee_amount, net, self.market.total_supply_shares)?;
                add_128(&mut self.market.total_supply_shares, fee_shares)?;
            }
        }
        self.market.last_update = now_u;
        Ok(())
    }

    /// `Morpho._isHealthy` (`Morpho.sol:515-538`) for the account: no borrow shares is healthy
    /// without an oracle read; otherwise `toAssetsUp(borrowShares) <= wMulDown(collateral *
    /// price / 1e36, lltv)`. A reverting oracle reverts the call; one answering zero is read as
    /// zero, bounding the borrow at zero (unhealthy: the borrow shares are non-zero, so
    /// `toAssetsUp` is at least one).
    pub fn is_healthy(&self) -> Result<bool, FlammError> {
        if self.position.borrow_shares.is_zero() {
            return Ok(true);
        }
        let price = if self.oracle_ok {
            self.oracle_price
        } else if self.oracle_zero {
            U256::ZERO
        } else {
            return Err(FlammError::MorphoOracleReverted);
        };
        let borrowed = to_assets_up(
            self.position.borrow_shares,
            self.market.total_borrow_assets,
            self.market.total_borrow_shares,
        )?;
        let quoted = mul_div_down(self.position.collateral, price, ORACLE_PRICE_SCALE)?;
        let max_borrow = w_mul_down(quoted, self.lltv)?;
        Ok(max_borrow >= borrowed)
    }

    /// `Morpho.supply(assets, 0)` (`Morpho.sol:169-196`): shares minted at `toSharesDown` after
    /// accrual.
    pub fn supply(&mut self, assets: U256, now: u64) -> Result<U256, FlammError> {
        if assets.is_zero() {
            return Err(FlammError::MorphoInconsistentInput);
        }
        self.accrue(now)?;
        let m = &mut self.market;
        let shares = to_shares_down(assets, m.total_supply_assets, m.total_supply_shares)?;
        self.position.supply_shares = checked_add(self.position.supply_shares, shares)?;
        add_128(&mut m.total_supply_shares, shares)?;
        add_128(&mut m.total_supply_assets, assets)?;
        Ok(shares)
    }

    /// `Morpho.withdraw` (`Morpho.sol:200-231`): exactly one of `assets` / `shares`; assets burn
    /// `toSharesUp`, shares pay `toAssetsDown`; the market must stay solvent
    /// (`totalBorrowAssets <= totalSupplyAssets`). Returns `(assets, shares)`.
    pub fn withdraw(
        &mut self,
        assets: U256,
        shares: U256,
        now: u64,
    ) -> Result<(U256, U256), FlammError> {
        if assets.is_zero() == shares.is_zero() {
            return Err(FlammError::MorphoInconsistentInput);
        }
        self.accrue(now)?;
        let m = &mut self.market;
        let (out_a, out_s) = if !assets.is_zero() {
            (assets, to_shares_up(assets, m.total_supply_assets, m.total_supply_shares)?)
        } else {
            (to_assets_down(shares, m.total_supply_assets, m.total_supply_shares)?, shares)
        };
        if out_s > self.position.supply_shares {
            return Err(FlammError::PanicArithmetic);
        }
        self.position.supply_shares -= out_s;
        sub_128(&mut m.total_supply_shares, out_s)?;
        sub_128(&mut m.total_supply_assets, out_a)?;
        if m.total_borrow_assets > m.total_supply_assets {
            return Err(FlammError::MorphoInsufficientLiquidity);
        }
        Ok((out_a, out_s))
    }

    /// `Morpho.borrow(assets, 0)` (`Morpho.sol:235-265`): shares at `toSharesUp`, then the health
    /// and liquidity requires.
    pub fn borrow(&mut self, assets: U256, now: u64) -> Result<U256, FlammError> {
        if assets.is_zero() {
            return Err(FlammError::MorphoInconsistentInput);
        }
        self.accrue(now)?;
        let shares =
            to_shares_up(assets, self.market.total_borrow_assets, self.market.total_borrow_shares)?;
        add_128(&mut self.position.borrow_shares, shares)?;
        add_128(&mut self.market.total_borrow_shares, shares)?;
        add_128(&mut self.market.total_borrow_assets, assets)?;
        if !self.is_healthy()? {
            return Err(FlammError::MorphoInsufficientCollateral);
        }
        if self.market.total_borrow_assets > self.market.total_supply_assets {
            return Err(FlammError::MorphoInsufficientLiquidity);
        }
        Ok(shares)
    }

    /// `Morpho.repay` (`Morpho.sol:269-297`): exactly one of `assets` / `shares`; assets burn
    /// `toSharesDown`, shares cost `toAssetsUp`, and the borrow total floors at zero
    /// (`UtilsLib.zeroFloorSub`, `UtilsLib.sol:33-37`: the repaid assets may exceed it by one wei).
    /// Returns `(assets, shares)`.
    pub fn repay(
        &mut self,
        assets: U256,
        shares: U256,
        now: u64,
    ) -> Result<(U256, U256), FlammError> {
        if assets.is_zero() == shares.is_zero() {
            return Err(FlammError::MorphoInconsistentInput);
        }
        self.accrue(now)?;
        let m = &mut self.market;
        let (out_a, out_s) = if !assets.is_zero() {
            (assets, to_shares_down(assets, m.total_borrow_assets, m.total_borrow_shares)?)
        } else {
            (to_assets_up(shares, m.total_borrow_assets, m.total_borrow_shares)?, shares)
        };
        sub_128(&mut self.position.borrow_shares, out_s)?;
        sub_128(&mut m.total_borrow_shares, out_s)?;
        m.total_borrow_assets = m
            .total_borrow_assets
            .saturating_sub(out_a);
        Ok((out_a, out_s))
    }

    /// `Morpho.supplyCollateral` (`Morpho.sol:303-317`): no accrual, the collateral add checked.
    pub fn supply_collateral(&mut self, assets: U256) -> Result<(), FlammError> {
        if assets.is_zero() {
            return Err(FlammError::MorphoZeroAssets);
        }
        add_128(&mut self.position.collateral, assets)
    }

    /// `Morpho.withdrawCollateral` (`Morpho.sol:323-341`): accrual, the subtraction, then the
    /// health require.
    pub fn withdraw_collateral(&mut self, assets: U256, now: u64) -> Result<(), FlammError> {
        if assets.is_zero() {
            return Err(FlammError::MorphoZeroAssets);
        }
        self.accrue(now)?;
        sub_128(&mut self.position.collateral, assets)?;
        if !self.is_healthy()? {
            return Err(FlammError::MorphoInsufficientCollateral);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(ORACLE_PRICE_SCALE, U256::from(10u64).pow(U256::from(36)));
        assert_eq!(MAX_UINT128, (U256::from(1) << 128) - U256::from(1));
        assert_eq!(TWO_WAD, WAD * U256::from(2));
        assert_eq!(THREE_WAD, WAD * U256::from(3));
    }

    #[test]
    fn shares_math_rounding() {
        // an empty market: assets map 1:1e6 to shares both ways
        assert_eq!(
            to_shares_down(U256::from(7), U256::ZERO, U256::ZERO),
            Ok(U256::from(7_000_000))
        );
        assert_eq!(to_shares_up(U256::from(7), U256::ZERO, U256::ZERO), Ok(U256::from(7_000_000)));
        assert_eq!(
            to_assets_down(U256::from(7_000_001), U256::ZERO, U256::ZERO),
            Ok(U256::from(7))
        );
        assert_eq!(to_assets_up(U256::from(7_000_001), U256::ZERO, U256::ZERO), Ok(U256::from(8)));
        assert_eq!(
            mul_div_up(U256::from(1), U256::from(1), U256::ZERO),
            Err(FlammError::PanicArithmetic)
        );
        assert_eq!(
            mul_div_down(U256::MAX, U256::from(2), U256::from(2)),
            Err(FlammError::PanicArithmetic)
        );
        assert_eq!(
            mul_div_down(U256::from(2), U256::from(2), U256::ZERO),
            Err(FlammError::PanicDivZero)
        );
    }

    #[test]
    fn uint128_writes() {
        let mut t = MAX_UINT128 - U256::from(1);
        assert_eq!(add_128(&mut t, U256::from(1)), Ok(()));
        assert_eq!(t, MAX_UINT128);
        assert_eq!(add_128(&mut t, U256::from(1)), Err(FlammError::PanicArithmetic));
        assert_eq!(
            add_128(&mut t, MAX_UINT128 + U256::from(1)),
            Err(FlammError::MorphoMaxUint128Exceeded)
        );
        assert_eq!(
            sub_128(&mut t, MAX_UINT128 + U256::from(1)),
            Err(FlammError::MorphoMaxUint128Exceeded)
        );
        assert_eq!(sub_128(&mut t, MAX_UINT128), Ok(()));
        assert_eq!(t, U256::ZERO);
        assert_eq!(sub_128(&mut t, U256::from(1)), Err(FlammError::PanicArithmetic));
    }

    #[test]
    fn taylor_matches_solidity_form() {
        // x*n = 1e18 (100% for the period): 1 + 1/2 + 1/6 of WAD
        let g = w_taylor_compounded(U256::from(1_000_000_000u64), U256::from(1_000_000_000u64))
            .unwrap();
        assert_eq!(g, U256::from(1_666_666_666_666_666_666u64));
    }
}
