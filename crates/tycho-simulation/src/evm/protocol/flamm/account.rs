// Copyright (c) 2026 Everlong Labs Limited

//! `MorphoBlueAccount` (`src/core/mm/MorphoBlueAccount.sol` @ c104 `80abd43`, the kind-0 financing
//! account; c104's is `0x6760E3b032eE2d670Cb684d9076b8f48cb066c48`): the accrued views the Router
//! prices every leg from, and the normal-lane mutators as transitions on the tracked Morpho market.
//!
//! The views use OpenZeppelin 4.8 `Math.mulDiv` (512-bit intermediates, reverting only when the
//! quotient overflows), unlike Blue's checked-product `MathLib`; the two agree everywhere Blue
//! itself does not revert. The mutators are Blue's transitions ([`VenueMarket`]) behind the
//! account's own exact-delta checks, which hold by construction on a modelled transition and are
//! kept where they change the revert class (`_repay`).

use alloy::primitives::U256;

use super::{
    error::FlammError,
    irm::borrow_rate,
    math::{checked_add, checked_mul, mul_div, mul_div_up, WAD},
    morpho::{
        Market, VenueMarket, MAX_UINT128, THREE_WAD, TWO_WAD, VIRTUAL_ASSETS, VIRTUAL_SHARES,
    },
};

/// `MorphoBlueAccount.IRM_STALE_GRACE = 1 hours` (`MorphoBlueAccount.sol:32`).
pub const IRM_STALE_GRACE: U256 = U256::from_limbs([3600, 0, 0, 0]);
/// `MorphoBlueAccount.GRACE_RATE_WAD = uint256(8e18) / 365 days` (`MorphoBlueAccount.sol:41`).
pub const GRACE_RATE_WAD: U256 =
    U256::from_limbs([8_000_000_000_000_000_000 / (365 * 86_400), 0, 0, 0]);

/// The four market totals as `MorphoBlueAccount._state` reports them, with its `readable` flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccruedTotals {
    pub readable: bool,
    pub tsa: U256,
    pub tss: U256,
    pub tba: U256,
    pub tbs: U256,
}

/// `IFinancingAccount.tryPosition(id)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VenueRead {
    pub readable: bool,
    pub collateral: U256,
    pub supply_shares: U256,
    pub supplied: U256,
    pub debt: U256,
}

/// `MorphoBlueAccount._wTaylorCompounded(x, n)` (`MorphoBlueAccount.sol:545-550`): Blue's three
/// terms, the squares through `Math.mulDiv`.
pub fn account_taylor(x: U256, n: U256) -> Result<U256, FlammError> {
    let first = checked_mul(x, n)?;
    let second = mul_div(first, first, TWO_WAD)?;
    let third = mul_div(second, first, THREE_WAD)?;
    checked_add(checked_add(first, second)?, third)
}

/// `Math.mulDiv(shares, totalAssets + VIRTUAL_ASSETS, totalShares + VIRTUAL_SHARES[,
/// Rounding.Up])`, the valuation `tryPosition`, `_debt` and `supplySharesToAssets` share
/// (`MorphoBlueAccount.sol:307-308, 332, 433`).
pub fn shares_to_assets_oz(
    shares: U256,
    total_assets: U256,
    total_shares: U256,
    up: bool,
) -> Result<U256, FlammError> {
    let va = checked_add(total_assets, VIRTUAL_ASSETS)?;
    let vs = checked_add(total_shares, VIRTUAL_SHARES)?;
    if up {
        mul_div_up(shares, va, vs)
    } else {
        mul_div(shares, va, vs)
    }
}

impl VenueMarket {
    /// `MorphoBlueAccount._state(id)` (`MorphoBlueAccount.sol:476-500`) at `now`: Blue's totals as
    /// its next `_accrueInterest` will leave them. Nothing accrues when no time has passed or
    /// nothing is borrowed. While the IRM is unreadable the figures stay the last accrued ones:
    /// beyond `IRM_STALE_GRACE` the venue is unreadable; inside it the DEBT leg alone is marked
    /// up at `GRACE_RATE_WAD` and the supply legs are frozen. Otherwise the interest lands on
    /// both legs and the fee shares are minted at the post-interest supply less the fee (Blue's
    /// `toSharesDown` with `Math.mulDiv`).
    pub fn account_state(&self, now: u64) -> Result<AccruedTotals, FlammError> {
        let m = &self.market;
        let mut s = AccruedTotals {
            readable: true,
            tsa: m.total_supply_assets,
            tss: m.total_supply_shares,
            tba: m.total_borrow_assets,
            tbs: m.total_borrow_shares,
        };
        let now_u = U256::from(now);
        if now_u <= m.last_update || s.tba.is_zero() {
            return Ok(s);
        }
        let elapsed = now_u - m.last_update;
        let (ok, rate) = self.try_borrow_rate(U256::ZERO, U256::ZERO, now)?;
        if !ok {
            if elapsed > IRM_STALE_GRACE {
                s.readable = false;
                return Ok(s);
            }
            let growth = account_taylor(GRACE_RATE_WAD, elapsed)?;
            let markup = mul_div(s.tba, growth, WAD)?;
            s.tba = checked_add(s.tba, markup)?;
            return Ok(s);
        }
        let growth = account_taylor(rate, elapsed)?;
        let interest = mul_div(s.tba, growth, WAD)?;
        s.tba = checked_add(s.tba, interest)?;
        s.tsa = checked_add(s.tsa, interest)?;
        if !m.fee.is_zero() {
            let fee_amount = mul_div(interest, m.fee, WAD)?;
            let vs = checked_add(s.tss, VIRTUAL_SHARES)?;
            let va = checked_add(super::math::checked_sub(s.tsa, fee_amount)?, VIRTUAL_ASSETS)?;
            let fee_shares = mul_div(fee_amount, vs, va)?;
            s.tss = checked_add(s.tss, fee_shares)?;
        }
        Ok(s)
    }

    /// `MorphoBlueAccount._tryBorrowRate(id, deltaBorrow, deltaSupplyDown)`
    /// (`MorphoBlueAccount.sol:517-543`): the market's rate after `deltaBorrow` more debt and
    /// `deltaSupplyDown` less supply, read from the IRM view over the STORED (unaccrued) market
    /// with the borrow total saturated at `uint128` max. A market with no IRM runs at exactly
    /// zero. A non-zero supply delta reaching the whole supply fails closed (no defined
    /// post-state); the supply shares are deliberately not scaled. An IRM revert (unreadable,
    /// or its own timestamp underflow) is `ok == false`. The only revert is the
    /// unchecked-looking `tba + deltaBorrow`, which is a checked `uint256` add.
    pub fn try_borrow_rate(
        &self,
        delta_borrow: U256,
        delta_supply_down: U256,
        now: u64,
    ) -> Result<(bool, U256), FlammError> {
        if !self.has_irm {
            return Ok((true, U256::ZERO));
        }
        let m = &self.market;
        if !delta_supply_down.is_zero() && delta_supply_down >= m.total_supply_assets {
            return Ok((false, U256::ZERO));
        }
        let mut nb = checked_add(m.total_borrow_assets, delta_borrow)?;
        if nb > MAX_UINT128 {
            nb = MAX_UINT128;
        }
        if !self.irm_readable {
            return Ok((false, U256::ZERO));
        }
        let s = Market {
            total_supply_assets: m.total_supply_assets - delta_supply_down,
            total_borrow_assets: nb,
            ..*m
        };
        match borrow_rate(&s, self.rate_at_target, now) {
            Ok((rate, _)) => Ok((true, rate)),
            Err(_) => Ok((false, U256::ZERO)),
        }
    }

    /// `MorphoBlueAccount.tryPosition` (`MorphoBlueAccount.sol:294-309`): the raw position valued
    /// at the `_state` totals, supply floored and debt ceiled; the figures are filled in even
    /// when the venue is unreadable.
    pub fn try_position(&self, now: u64) -> Result<VenueRead, FlammError> {
        let mut r = VenueRead {
            collateral: self.position.collateral,
            supply_shares: self.position.supply_shares,
            ..Default::default()
        };
        let s = self.account_state(now)?;
        r.readable = s.readable;
        if !r.supply_shares.is_zero() {
            r.supplied = shares_to_assets_oz(r.supply_shares, s.tsa, s.tss, false)?;
        }
        if !self.position.borrow_shares.is_zero() {
            r.debt = shares_to_assets_oz(self.position.borrow_shares, s.tba, s.tbs, true)?;
        }
        Ok(r)
    }

    /// `MorphoBlueAccount._expectedState` (`MorphoBlueAccount.sol:437-441`): `_state`, reverting
    /// `IrmUnreadable` past the grace.
    pub fn expected_state(&self, now: u64) -> Result<AccruedTotals, FlammError> {
        let s = self.account_state(now)?;
        if !s.readable {
            return Err(FlammError::IrmUnreadable);
        }
        Ok(s)
    }

    /// `MorphoBlueAccount.debtOf` / `_debt` (`MorphoBlueAccount.sol:312-315, 430-434`): the borrow
    /// shares at the expected totals, rounded up.
    pub fn debt_of(&self, now: u64) -> Result<U256, FlammError> {
        if self.position.borrow_shares.is_zero() {
            return Ok(U256::ZERO);
        }
        let s = self.expected_state(now)?;
        shares_to_assets_oz(self.position.borrow_shares, s.tba, s.tbs, true)
    }

    /// `MorphoBlueAccount.supplySharesToAssets` (`MorphoBlueAccount.sol:329-333`): floor at the
    /// expected totals.
    pub fn supply_shares_to_assets(&self, shares: U256, now: u64) -> Result<U256, FlammError> {
        if shares.is_zero() {
            return Ok(U256::ZERO);
        }
        let s = self.expected_state(now)?;
        shares_to_assets_oz(shares, s.tsa, s.tss, false)
    }

    /// `MorphoBlueAccount.suppliedOf` (`MorphoBlueAccount.sol:323-326`).
    pub fn supplied_of(&self, now: u64) -> Result<U256, FlammError> {
        self.supply_shares_to_assets(self.position.supply_shares, now)
    }

    /// `MorphoBlueAccount.freeLiquidity` (`MorphoBlueAccount.sol:336-339`): the STORED totals'
    /// cash, never accrued.
    pub fn free_liquidity(&self) -> U256 {
        self.market
            .total_supply_assets
            .saturating_sub(self.market.total_borrow_assets)
    }

    // ------------------------------------------------------------------ normal lane
    // (Router-driven)
    //
    // `MorphoBlueAccount.supplyCollateral` / `withdrawCollateral` / `borrow` / `supply`
    // (`MorphoBlueAccount.sol:119-126`, `:129-136`, `:139-145`, `:153-161`) are Blue's own calls
    // plus the approvals and the `ExactDelta` / `BadReceiver` asserts on the amounts they just
    // passed; the port's ledger is those amounts, so the asserts cannot fail and the Router calls
    // Blue's [`VenueMarket::supply_collateral`], [`VenueMarket::withdraw_collateral`],
    // [`VenueMarket::borrow`] and [`VenueMarket::supply`] directly. `_repay` and `_withdraw` do
    // carry their own logic and keep their `account_` methods below.

    /// `MorphoBlueAccount._repay` (`MorphoBlueAccount.sol:373-390`) with the account holding at
    /// least `assets` (the Router pulls the leg in first): accrue (a revert there is
    /// `IrmUnreadable`); nothing to do without shares or assets; the whole position is closed
    /// by SHARES when the leg covers the accrued debt (so no dust share survives), otherwise
    /// repaid by assets, which must burn at least one share (`ExactDelta` otherwise). Returns
    /// the assets repaid.
    pub fn account_repay(&mut self, assets: U256, now: u64) -> Result<U256, FlammError> {
        if self.accrue(now).is_err() {
            return Err(FlammError::IrmUnreadable);
        }
        let shares = self.position.borrow_shares;
        if shares.is_zero() || assets.is_zero() {
            return Ok(U256::ZERO);
        }
        let owed = self.debt_of(now)?;
        let pay = assets.min(owed);
        if pay.is_zero() {
            return Ok(U256::ZERO);
        }
        let full = pay >= owed;
        let (repaid, _) = if full {
            self.repay(U256::ZERO, shares, now)?
        } else {
            self.repay(pay, U256::ZERO, now)?
        };
        let after = self.position.borrow_shares;
        if (full && !after.is_zero()) || (!full && after >= shares) {
            return Err(FlammError::ExactDelta);
        }
        if repaid != pay {
            return Err(FlammError::ExactDelta);
        }
        Ok(repaid)
    }

    /// `MorphoBlueAccount._withdraw` (`MorphoBlueAccount.sol:392-407`): exactly one of `assets` /
    /// `shares` (`InvalidConfig` otherwise), accrue (`IrmUnreadable` on a revert), nothing out
    /// of an empty position, then Blue's `withdraw`. Returns `(withdrawn, sharesBurned)`.
    pub fn account_withdraw(
        &mut self,
        assets: U256,
        shares: U256,
        now: u64,
    ) -> Result<(U256, U256), FlammError> {
        if assets.is_zero() == shares.is_zero() {
            return Err(FlammError::InvalidConfig);
        }
        if self.accrue(now).is_err() {
            return Err(FlammError::IrmUnreadable);
        }
        if self.position.supply_shares.is_zero() {
            return Ok((U256::ZERO, U256::ZERO));
        }
        self.withdraw(assets, shares, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(
            GRACE_RATE_WAD,
            U256::from(8_000_000_000_000_000_000u64) / U256::from(365u64 * 86_400)
        );
        assert_eq!(GRACE_RATE_WAD, U256::from(253_678_335_870u64));
    }

    #[test]
    fn no_irm_market_runs_at_zero() {
        let v = VenueMarket { has_irm: false, ..Default::default() };
        assert_eq!(v.try_borrow_rate(U256::from(5), U256::ZERO, 10), Ok((true, U256::ZERO)));
        let s = v.account_state(10).unwrap();
        assert!(s.readable);
    }

    #[test]
    fn grace_marks_only_the_debt_up() {
        let mut v = VenueMarket {
            has_irm: true,
            irm_readable: false,
            market: Market {
                total_supply_assets: U256::from(1_000_000),
                total_supply_shares: U256::from(1_000_000_000_000u64),
                total_borrow_assets: U256::from(500_000),
                total_borrow_shares: U256::from(500_000_000_000u64),
                last_update: U256::from(1000),
                fee: U256::ZERO,
            },
            ..Default::default()
        };
        // inside the grace the supply legs are frozen and the debt leg is marked up
        let s = v.account_state(1000 + 3600).unwrap();
        assert!(s.readable);
        assert_eq!(s.tsa, U256::from(1_000_000));
        let growth = account_taylor(GRACE_RATE_WAD, U256::from(3600)).unwrap();
        assert_eq!(s.tba, U256::from(500_000) + mul_div(U256::from(500_000), growth, WAD).unwrap());
        // one second past it the venue is unreadable and the figures are the stored ones
        let s = v.account_state(1000 + 3601).unwrap();
        assert!(!s.readable);
        assert_eq!(s.tba, U256::from(500_000));
        assert_eq!(v.debt_of(1000 + 3601), Ok(U256::ZERO));
        v.position.borrow_shares = U256::from(1);
        assert_eq!(v.debt_of(1000 + 3601), Err(FlammError::IrmUnreadable));
        assert_eq!(v.account_repay(U256::from(1), 1000 + 3601), Err(FlammError::IrmUnreadable));
    }
}
