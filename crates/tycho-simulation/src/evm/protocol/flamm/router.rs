// Copyright (c) 2026 Everlong Labs Limited

//! `MMRouter` / `MMRouterLib` (`src/core/mm/MMRouter.sol`, `src/core/mm/MMRouterLib.sol` @ c104
//! `80abd43`; the c104 Router is `0x19A9b39E6710AAD109C829294b0841F0851c6bB4`): one pool's record
//! -- loan assets, venues, priorities, pin -- and the multi-venue bodies the pool settles through:
//! the funding plan and its ceiling, `fund` / `repayCascade` / `supplyCascade` / `reclaim`, and the
//! aggregate views. Recognized positions are `min(actual, managed)`; a venue unreadable past its
//! account's grace recognizes nothing and carries its debt at the 1.05x haircut. Then the
//! `FLAMMSwapLib` settlement legs that drive the Router (`payLoan`, `takeLoan`, `releaseExcess`,
//! `settleSell` / the buy branch of `execute`) and the single-venue `MMRouter` entries the pool's
//! pro-rata flows use.
//!
//! Router methods write through as they go and may leave a half-applied state behind an error;
//! callers run them on a clone and keep the result only on success, as a reverted transaction
//! leaves no trace. The repay snapshot (EIP-1153 transient storage on chain) is carried in
//! [`Router::transient_repay`] across the calls of one transaction, clones included, and callers
//! drop it with [`Router::end_transaction`] when that transaction ends. [`settle_sell`] and
//! [`settle_buy`] are whole swap transactions and do both themselves.

use std::collections::BTreeMap;

use alloy::primitives::U256;

use super::{
    error::FlammError,
    gate::{
        anchor, assert_entry_gate, assert_gate, price_wads, priced, required_posted_all, GateReads,
        Pool, FEATURE_SUPPLY_LENDING, RELEASE_HYSTERESIS_WAD,
    },
    math::{checked_add, checked_div, checked_mul, checked_sub, min_u, mul_div, mul_div_up, WAD},
    morpho::{VenueMarket, MAX_UINT128, ORACLE_PRICE_SCALE},
};

/// `WAD + MMRouterLib.QUARANTINE_HAIRCUT_WAD` (`MMRouterLib.sol:25`, `0.05e18`).
pub const QUARANTINE_COUNT_WAD: U256 = U256::from_limbs([1_050_000_000_000_000_000, 0, 0, 0]);

/// `MMRouterLib.Loan` (the token and the account addresses are the venues' own business here).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Loan {
    pub decimals: u8,
    pub loan_scale: U256,
    pub debt_cap: U256,
    pub supply_cap: U256,
    pub borrow_enabled: bool,
    pub retired: bool,
}

/// `MMRouterLib.Venue` with the financing account's Morpho market behind it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Venue {
    pub morpho: VenueMarket,
    pub kind: u8,
    pub loan_index: u8,
    pub lltv_wad: U256,
    pub borrow_enabled: bool,
    pub supply_enabled: bool,
    pub retired: bool,
    pub debt_cap: U256,
    pub supply_cap: U256,
    pub max_borrow_rate_wad: U256,
    pub managed_collateral: U256,
    pub managed_supply_shares: U256,
}

/// The `(debt, collateral)` `MMRouterLib.snapshotForProportional` parks in transient storage before
/// a repay, read back by a proportional `withdrawCollateral` in the same transaction.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepaySnapshot {
    pub debt: U256,
    pub coll: U256,
}

/// One pool's `MMRouterLib.PoolRecord` plus the Router-wide `globalPaused`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Router {
    pub global_paused: bool,
    pub pin_ltv_wad: U256,
    pub safety_gap_wad: U256,
    pub oracle_band_wad: U256,
    pub max_drawn_assets: u8,
    pub loans: Vec<Loan>,
    pub venues: Vec<Venue>,
    pub borrow_order: Vec<u16>,
    pub supply_order: Vec<u16>,
    pub withdraw_order: Vec<u16>,
    pub repay_order: Vec<u16>,
    /// The per-transaction repay snapshot (transient storage), cleared by
    /// [`Router::end_transaction`]; never persisted.
    pub transient_repay: BTreeMap<u16, RepaySnapshot>,
}

/// `MMRouterLib.Read`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Read {
    pub readable: bool,
    pub collateral: U256,
    pub recognized_collateral: U256,
    pub supplied: U256,
    pub recognized_supplied: U256,
    pub debt: U256,
}

/// `MMRouter.positions`: per loan asset recognized collateral, supply and counted debt, and the
/// total recognized collateral (the pool's posted poolAsset).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Positions {
    pub coll: Vec<U256>,
    pub sup: Vec<U256>,
    pub debt: Vec<U256>,
    pub total_coll: U256,
}

/// `MMRouter.quarantine`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quarantine {
    pub any: bool,
    pub frozen_debt: U256,
    pub frozen_coll: U256,
}

/// `MMRouterLib.Plan`, indexed by venue id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub withdraw_take: Vec<U256>,
    pub borrow_slice: Vec<U256>,
    pub post: Vec<U256>,
    pub remaining: U256,
}

/// The `tstore` / `tload` round trip of the snapshot (`MMRouterLib.sol:804-822`): `packed = (debt
/// << 128) | coll`, read back as `(packed >> 128, packed & type(uint128).max)`. The unchecked shift
/// drops a debt's bits at and above `2^128`.
pub fn pack_repay_snapshot(debt: U256, coll: U256) -> RepaySnapshot {
    let packed = (debt << 128) | coll;
    RepaySnapshot { debt: packed >> 128, coll: packed & MAX_UINT128 }
}

/// `MMRouterLib._counted` (`MMRouterLib.sol:594-596`): readable debt as is, an unreadable venue's
/// at the 1.05x flat allowance, ceiled.
pub fn counted(r: &Read) -> Result<U256, FlammError> {
    if r.readable {
        return Ok(r.debt);
    }
    mul_div_up(r.debt, QUARANTINE_COUNT_WAD, WAD)
}

/// `uint8(1 << idx)`: the bit of loan index `idx` in the drawn mask, zero past the eighth asset.
fn loan_bit(idx: u8) -> u8 {
    if idx >= 8 {
        0
    } else {
        1u8 << idx
    }
}

impl Router {
    // ------------------------------------------------------------------ reads

    /// `MMRouterLib.read` (`MMRouterLib.sol:556-563`): `tryPosition`, and for a readable venue the
    /// recognized figures -- collateral `min(actual, managed)`, supply the full valuation when
    /// every share is managed, else the managed shares valued.
    pub fn read(&self, v: &Venue, now: u64) -> Result<Read, FlammError> {
        let t = v.morpho.try_position(now)?;
        let mut r = Read {
            readable: t.readable,
            collateral: t.collateral,
            supplied: t.supplied,
            debt: t.debt,
            ..Default::default()
        };
        if !r.readable {
            return Ok(r);
        }
        r.recognized_collateral = min_u(r.collateral, v.managed_collateral);
        let managed = min_u(t.supply_shares, v.managed_supply_shares);
        r.recognized_supplied = if managed == t.supply_shares {
            r.supplied
        } else {
            v.morpho
                .supply_shares_to_assets(managed, now)?
        };
        Ok(r)
    }

    /// `MMRouterLib.loanAt` (`MMRouterLib.sol:636-639`).
    pub fn loan_at(&self, idx: u8) -> Result<&Loan, FlammError> {
        self.loans
            .get(idx as usize)
            .ok_or(FlammError::BadLoanIndex)
    }

    /// `MMRouterLib.venueAt` (`MMRouterLib.sol:641-644`).
    pub fn venue_at(&self, id: u16) -> Result<&Venue, FlammError> {
        self.venues
            .get(id as usize)
            .ok_or(FlammError::BadVenueId)
    }

    /// `MMRouterLib.live` (`MMRouterLib.sol:646-649`).
    pub fn live(&self, id: u16) -> Result<&Venue, FlammError> {
        let v = self.venue_at(id)?;
        if v.retired {
            return Err(FlammError::VenueIsRetired);
        }
        Ok(v)
    }

    /// `p.loans[v.loanIndex]`: a storage index, `Panic(0x32)` past the end.
    fn loan_of(&self, v: &Venue) -> Result<&Loan, FlammError> {
        self.loans
            .get(v.loan_index as usize)
            .ok_or(FlammError::PanicIndex)
    }

    /// `MMRouterLib.positions` (`MMRouterLib.sol:488-507`) over the live (non-retired) venues.
    pub fn positions(&self, now: u64) -> Result<Positions, FlammError> {
        let m = self.loans.len();
        let mut out = Positions {
            coll: vec![U256::ZERO; m],
            sup: vec![U256::ZERO; m],
            debt: vec![U256::ZERO; m],
            total_coll: U256::ZERO,
        };
        for v in &self.venues {
            if v.retired {
                continue;
            }
            let r = self.read(v, now)?;
            let c = counted(&r)?;
            let k = v.loan_index as usize;
            if k >= m {
                return Err(FlammError::PanicIndex);
            }
            out.coll[k] = checked_add(out.coll[k], r.recognized_collateral)?;
            out.sup[k] = checked_add(out.sup[k], r.recognized_supplied)?;
            out.debt[k] = checked_add(out.debt[k], c)?;
            out.total_coll = checked_add(out.total_coll, r.recognized_collateral)?;
        }
        Ok(out)
    }

    /// `MMRouterLib.aggregate` (`MMRouterLib.sol:472-486`) for loan asset `idx`: `(recognized
    /// collateral, recognized supply, counted debt)`.
    pub fn aggregate(&self, idx: u8, now: u64) -> Result<(U256, U256, U256), FlammError> {
        let (mut coll, mut sup, mut debt) = (U256::ZERO, U256::ZERO, U256::ZERO);
        for v in &self.venues {
            if v.retired || v.loan_index != idx {
                continue;
            }
            let r = self.read(v, now)?;
            let c = counted(&r)?;
            coll = checked_add(coll, r.recognized_collateral)?;
            sup = checked_add(sup, r.recognized_supplied)?;
            debt = checked_add(debt, c)?;
        }
        Ok((coll, sup, debt))
    }

    /// `MMRouter.position` (`MMRouter.sol:396-405`): `loanAt`'s bound check, then `aggregate`.
    pub fn position(&self, idx: u8, now: u64) -> Result<(U256, U256, U256), FlammError> {
        self.loan_at(idx)?;
        self.aggregate(idx, now)
    }

    /// `MMRouterLib.quarantine` (`MMRouterLib.sol:516-531`): the unreadable venues of loan asset
    /// `idx`, their counted debt and the collateral the quarantine de-recognised (the same
    /// `min(actual, managed)` a readable read uses).
    pub fn quarantine(&self, idx: u8, now: u64) -> Result<Quarantine, FlammError> {
        let mut q = Quarantine::default();
        for v in &self.venues {
            if v.retired || v.loan_index != idx {
                continue;
            }
            let r = self.read(v, now)?;
            if r.readable {
                continue;
            }
            q.any = true;
            let c = counted(&r)?;
            q.frozen_debt = checked_add(q.frozen_debt, c)?;
            q.frozen_coll = checked_add(q.frozen_coll, min_u(r.collateral, v.managed_collateral))?;
        }
        Ok(q)
    }

    /// `MMRouterLib.drawn` (`MMRouterLib.sol:543-554`): the loan assets carrying debt (readable or
    /// not) as a count and a bitmask.
    pub fn drawn(&self, now: u64) -> Result<(u8, u8), FlammError> {
        let (mut count, mut mask) = (0u8, 0u8);
        for v in &self.venues {
            let bit = loan_bit(v.loan_index);
            if v.retired || mask & bit != 0 {
                continue;
            }
            let r = self.read(v, now)?;
            if !r.debt.is_zero() {
                mask |= bit;
                count = count.wrapping_add(1);
            }
        }
        Ok((count, mask))
    }

    /// `MMRouterLib.drawable` (`MMRouterLib.sol:664-668`): a one-asset pool always; else an asset
    /// already drawn or a free draw slot.
    pub fn drawable(&self, idx: u8, now: u64) -> Result<bool, FlammError> {
        if self.loans.len() == 1 {
            return Ok(true);
        }
        let (count, mask) = self.drawn(now)?;
        Ok(mask & loan_bit(idx) != 0 || count < self.max_drawn_assets)
    }

    /// `MMRouterLib.loanDebtRoom` (`MMRouterLib.sol:670-674`).
    pub fn loan_debt_room(&self, l: &Loan, idx: u8, now: u64) -> Result<U256, FlammError> {
        if l.debt_cap.is_zero() {
            return Ok(U256::MAX);
        }
        let (_, _, debt) = self.aggregate(idx, now)?;
        Ok(l.debt_cap.saturating_sub(debt))
    }

    /// `MMRouterLib.loanSupplyRoom` (`MMRouterLib.sol:676-680`).
    pub fn loan_supply_room(&self, l: &Loan, idx: u8, now: u64) -> Result<U256, FlammError> {
        if l.supply_cap.is_zero() {
            return Ok(U256::MAX);
        }
        let (_, sup, _) = self.aggregate(idx, now)?;
        Ok(l.supply_cap.saturating_sub(sup))
    }

    /// `MMRouterLib.minLltv` (`MMRouterLib.sol:792-800`): over every live venue that may borrow or
    /// still carries debt.
    pub fn min_lltv(&self, now: u64) -> Result<U256, FlammError> {
        let mut lltv = U256::ZERO;
        for v in &self.venues {
            if v.retired {
                continue;
            }
            if !v.borrow_enabled && self.read(v, now)?.debt.is_zero() {
                continue;
            }
            if lltv.is_zero() || v.lltv_wad < lltv {
                lltv = v.lltv_wad;
            }
        }
        Ok(lltv)
    }

    /// `MMRouterLib.scaleOf` (`MMRouterLib.sol:759-761`).
    pub fn scale_of(&self, v: &Venue) -> Result<U256, FlammError> {
        Ok(self.loan_of(v)?.loan_scale)
    }

    /// `MMRouterLib.maxDebtAtPin` (`MMRouterLib.sol:763-769`): `mulDiv(mulDiv(collateral, price,
    /// scale), pin, WAD)`.
    pub fn max_debt_at_pin(
        &self,
        v: &Venue,
        collateral: U256,
        price_wad: U256,
    ) -> Result<U256, FlammError> {
        let x = mul_div(collateral, price_wad, self.scale_of(v)?)?;
        mul_div(x, self.pin_ltv_wad, WAD)
    }

    /// `MMRouterLib.requiredCollateral` (`MMRouterLib.sol:771-778`): `mulDivUp(mulDivUp(debt, WAD,
    /// pin), scale, price)`.
    pub fn required_collateral(
        &self,
        v: &Venue,
        debt: U256,
        price_wad: U256,
    ) -> Result<U256, FlammError> {
        let value = mul_div_up(debt, WAD, self.pin_ltv_wad)?;
        mul_div_up(value, self.scale_of(v)?, price_wad)
    }

    /// `MMRouterLib.bandOk` (`MMRouterLib.sol:780-788`): the market oracle readable, and (with a
    /// band set) within the band of the PriceFeed cross lifted to the oracle's 1e36 scale; a
    /// zero cross fails a banded check.
    pub fn band_ok(&self, v: &Venue, price_wad: U256) -> Result<bool, FlammError> {
        if !v.morpho.oracle_ok {
            return Ok(false);
        }
        if self.oracle_band_wad.is_zero() {
            return Ok(true);
        }
        if price_wad.is_zero() {
            return Ok(false);
        }
        let expected = mul_div(price_wad, ORACLE_PRICE_SCALE, self.scale_of(v)?)?;
        let price = v.morpho.oracle_price;
        let dev = if price > expected { price - expected } else { expected - price };
        let tol = mul_div(expected, self.oracle_band_wad, WAD)?;
        Ok(dev <= tol)
    }

    /// `MMRouterLib.supplyRoom` (`MMRouterLib.sol:713-718`).
    pub fn supply_room(&self, v: &Venue, now: u64) -> Result<U256, FlammError> {
        let rd = self.read(v, now)?;
        if !rd.readable {
            return Ok(U256::ZERO);
        }
        if v.supply_cap.is_zero() {
            return Ok(U256::MAX);
        }
        Ok(v.supply_cap
            .saturating_sub(rd.recognized_supplied))
    }

    /// `MMRouterLib.managedShares` (`MMRouterLib.sol:733-736`).
    pub fn managed_shares(&self, v: &Venue) -> U256 {
        min_u(v.morpho.position.supply_shares, v.managed_supply_shares)
    }

    /// `MMRouterLib.recognizedSupplied` (`MMRouterLib.sol:738-740`; reverts `IrmUnreadable` past
    /// the grace).
    pub fn recognized_supplied(&self, v: &Venue, now: u64) -> Result<U256, FlammError> {
        v.morpho
            .supply_shares_to_assets(self.managed_shares(v), now)
    }

    /// `MMRouterLib.freeCollateral` (`MMRouterLib.sol:749-757`): collateral above the posting law
    /// at the pin, capped by the recognized figure; all of it for a debt-free venue, nothing
    /// for an indebted one at an unchecked or out-of-band cross.
    pub fn free_collateral(
        &self,
        v: &Venue,
        price_wad: U256,
        now: u64,
    ) -> Result<U256, FlammError> {
        let rd = self.read(v, now)?;
        if !rd.readable {
            return Ok(U256::ZERO);
        }
        if rd.debt.is_zero() {
            return Ok(rd.recognized_collateral);
        }
        if price_wad.is_zero() || !self.band_ok(v, price_wad)? {
            return Ok(U256::ZERO);
        }
        let required = self.required_collateral(v, rd.debt, price_wad)?;
        let free = rd.collateral.saturating_sub(required);
        Ok(min_u(free, rd.recognized_collateral))
    }

    /// `MMRouter.reclaimable` (`MMRouterLib.sol:533-540`): every live venue's free collateral at
    /// its loan asset's cross.
    pub fn reclaimable(&self, price_wads: &[U256], now: u64) -> Result<U256, FlammError> {
        if price_wads.len() != self.loans.len() {
            return Err(FlammError::InvalidConfig);
        }
        let mut total = U256::ZERO;
        for v in &self.venues {
            if v.retired {
                continue;
            }
            let price = price_wads
                .get(v.loan_index as usize)
                .copied()
                .ok_or(FlammError::PanicIndex)?;
            total = checked_add(total, self.free_collateral(v, price, now)?)?;
        }
        Ok(total)
    }

    /// `MMRouter.venuePosition` (`MMRouter.sol:384-393`): `(collateral, recognized, supplied,
    /// recognized, readable ? debt : 0)`.
    pub fn venue_position(&self, id: u16, now: u64) -> Result<[U256; 5], FlammError> {
        let r = self.read(self.venue_at(id)?, now)?;
        Ok([
            r.collateral,
            r.recognized_collateral,
            r.supplied,
            r.recognized_supplied,
            if r.readable { r.debt } else { U256::ZERO },
        ])
    }

    /// `MMRouter.venueHealth` (`MMRouter.sol:459-466`): `maxDebt * lltv / pin * WAD / debt`, max
    /// for an unreadable or debt-free venue.
    pub fn venue_health(&self, id: u16, price_wad: U256, now: u64) -> Result<U256, FlammError> {
        let v = self.venue_at(id)?;
        let r = self.read(v, now)?;
        if !r.readable || r.debt.is_zero() {
            return Ok(U256::MAX);
        }
        let max_debt = self.max_debt_at_pin(v, r.collateral, price_wad)?;
        let h = checked_div(checked_mul(max_debt, v.lltv_wad)?, self.pin_ltv_wad)?;
        checked_div(checked_mul(h, WAD)?, r.debt)
    }

    // ------------------------------------------------------------------ the funding plan

    /// `MMRouterLib._slice` (`MMRouterLib.sol:598-633`): one venue's borrow slice and the
    /// collateral it must post. The venue must be in band and readable; the slice is bounded by
    /// the cash this plan has not already withdrawn, the venue debt cap and the collateral it
    /// could hold at the pin; the rate ceiling is priced against the post-withdrawal supply (a
    /// slice that fails it is dropped whole, which is why the ceiling is not monotone in the
    /// collateral); if the posting law needs more than is available, the slice shrinks to what
    /// the available collateral carries. Returns `(slice, post)`.
    pub fn slice(
        &self,
        v: &Venue,
        want: U256,
        coll_avail: U256,
        cash_used: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<(U256, U256), FlammError> {
        let zero = (U256::ZERO, U256::ZERO);
        if !self.band_ok(v, price_wad)? {
            return Ok(zero);
        }
        let rd = self.read(v, now)?;
        if !rd.readable {
            return Ok(zero);
        }
        let (debt, coll) = (rd.debt, rd.collateral);
        let cash = v
            .morpho
            .free_liquidity()
            .saturating_sub(cash_used);
        let mut sl = min_u(want, cash);
        if !v.debt_cap.is_zero() {
            let room = v.debt_cap.saturating_sub(debt);
            if room < sl {
                sl = room;
            }
        }
        let mut max_debt = self.max_debt_at_pin(v, checked_add(coll, coll_avail)?, price_wad)?;
        let by_coll = max_debt.saturating_sub(debt);
        if by_coll < sl {
            sl = by_coll;
        }
        if sl.is_zero() {
            return Ok(zero);
        }
        if !v.max_borrow_rate_wad.is_zero() {
            let (ok, rate) = v
                .morpho
                .borrow_rate_after(sl, cash_used, now)?;
            if !ok || rate > v.max_borrow_rate_wad {
                return Ok(zero);
            }
        }
        let need = self.required_collateral(v, checked_add(debt, sl)?, price_wad)?;
        let mut post = need.saturating_sub(coll);
        if post > coll_avail {
            post = coll_avail;
            max_debt = self.max_debt_at_pin(v, checked_add(coll, post)?, price_wad)?;
            sl = max_debt.saturating_sub(debt);
            if sl.is_zero() {
                return Ok(zero);
            }
        }
        Ok((sl, post))
    }

    /// `MMRouterLib.buildPlan` (`MMRouterLib.sol:431-470`), the plan the quote and the execution
    /// share: the asset's own recognized supply withdrawn first (withdraw order, capped by each
    /// market's cash), then -- unless paused, the asset borrow-disabled or outside the drawn
    /// set -- borrow slices in borrow order under the asset debt cap, with the posted
    /// collateral drawn from `collateral_in`.
    pub fn build_plan(
        &self,
        idx: u8,
        assets: U256,
        collateral_in: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<Plan, FlammError> {
        let n = self.venues.len();
        let mut plan = Plan {
            withdraw_take: vec![U256::ZERO; n],
            borrow_slice: vec![U256::ZERO; n],
            post: vec![U256::ZERO; n],
            remaining: assets,
        };
        for &id in &self.withdraw_order {
            if plan.remaining.is_zero() {
                break;
            }
            let v = self
                .venues
                .get(id as usize)
                .ok_or(FlammError::PanicIndex)?;
            if v.loan_index != idx {
                continue;
            }
            let rd = self.read(v, now)?;
            let avail = min_u(rd.recognized_supplied, v.morpho.free_liquidity());
            let take = min_u(plan.remaining, avail);
            plan.withdraw_take[id as usize] = take;
            plan.remaining -= take;
        }
        if plan.remaining.is_zero() || self.global_paused {
            return Ok(plan);
        }
        let l = self.loan_at(idx)?;
        if !l.borrow_enabled || !self.drawable(idx, now)? {
            return Ok(plan);
        }
        let mut debt_room = self.loan_debt_room(l, idx, now)?;
        let mut coll_avail = collateral_in;
        for &id in &self.borrow_order {
            if plan.remaining.is_zero() || debt_room.is_zero() {
                break;
            }
            let v = self
                .venues
                .get(id as usize)
                .ok_or(FlammError::PanicIndex)?;
            if v.loan_index != idx || !v.borrow_enabled {
                continue;
            }
            let want = min_u(plan.remaining, debt_room);
            let (sl, post) =
                self.slice(v, want, coll_avail, plan.withdraw_take[id as usize], price_wad, now)?;
            if sl.is_zero() {
                continue;
            }
            plan.borrow_slice[id as usize] = sl;
            plan.post[id as usize] = post;
            plan.remaining -= sl;
            coll_avail -= post;
            debt_room -= sl;
        }
        Ok(plan)
    }

    /// `MMRouter.fundingCeiling` (`MMRouter.sol:444-450`): what a max-size plan would raise,
    /// `type(uint256).max - remaining`.
    pub fn funding_ceiling(
        &self,
        idx: u8,
        collateral_in: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<U256, FlammError> {
        let plan = self.build_plan(idx, U256::MAX, collateral_in, price_wad, now)?;
        Ok(U256::MAX - plan.remaining)
    }

    // ------------------------------------------------------------------ execution

    /// `MMRouterLib.requireRate` (`MMRouterLib.sol:701-705`): the execution-side ceiling with a
    /// ZERO supply delta (the plan's withdrawals have already settled on the market).
    pub fn require_rate(&self, v: &Venue, assets: U256, now: u64) -> Result<(), FlammError> {
        if v.max_borrow_rate_wad.is_zero() {
            return Ok(());
        }
        let (ok, rate) = v
            .morpho
            .borrow_rate_after(assets, U256::ZERO, now)?;
        if !ok || rate > v.max_borrow_rate_wad {
            return Err(FlammError::RateCeiling);
        }
        Ok(())
    }

    /// `MMRouterLib.requireBorrowable` (`MMRouterLib.sol:682-699`), in its revert order. `debt +
    /// assets` is a checked add evaluated only where Solidity evaluates it: behind `debtCap !=
    /// 0` (the `&&` short-circuits), and again for the pin comparison after the liquidity
    /// check, before `maxDebtAtPin` (via-IR evaluates binary operands left to right).
    pub fn require_borrowable(
        &self,
        v: &Venue,
        assets: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<(), FlammError> {
        if self.global_paused {
            return Err(FlammError::GlobalPaused);
        }
        if !v.borrow_enabled || !self.loan_of(v)?.borrow_enabled {
            return Err(FlammError::VenueDisabled);
        }
        if !self.drawable(v.loan_index, now)? {
            return Err(FlammError::DrawOutsideDrawnSet);
        }
        if !self.band_ok(v, price_wad)? {
            return Err(FlammError::OracleBand);
        }
        let debt = v.morpho.debt_of(now)?;
        if !v.debt_cap.is_zero() && checked_add(debt, assets)? > v.debt_cap {
            return Err(FlammError::DebtCapExceeded);
        }
        self.require_rate(v, assets, now)?;
        if v.morpho.free_liquidity() < assets {
            return Err(FlammError::InsufficientLiquidity);
        }
        let total = checked_add(debt, assets)?;
        if total > self.max_debt_at_pin(v, v.morpho.position.collateral, price_wad)? {
            return Err(FlammError::Unhealthy);
        }
        Ok(())
    }

    /// `MMRouterLib.withdrawSupplied` (`MMRouterLib.sol:720-731`): max withdraws the managed
    /// shares, else an asset amount bounded by the recognized supply (a zero amount refuses
    /// before the supply is valued: the `||` short-circuits); the managed shares shrink by what
    /// burned (floored at zero).
    pub fn withdraw_supplied(
        &mut self,
        id: u16,
        assets: U256,
        now: u64,
    ) -> Result<U256, FlammError> {
        let v = self
            .venues
            .get(id as usize)
            .ok_or(FlammError::PanicIndex)?;
        let (withdrawn, burned) = if assets == U256::MAX {
            let shares = self.managed_shares(v);
            if shares.is_zero() {
                return Ok(U256::ZERO);
            }
            self.venues[id as usize]
                .morpho
                .account_withdraw(U256::ZERO, shares, now)?
        } else {
            if assets.is_zero() || assets > self.recognized_supplied(v, now)? {
                return Err(FlammError::InsufficientLiquidity);
            }
            self.venues[id as usize]
                .morpho
                .account_withdraw(assets, U256::ZERO, now)?
        };
        let v = &mut self.venues[id as usize];
        v.managed_supply_shares = v
            .managed_supply_shares
            .saturating_sub(burned);
        Ok(withdrawn)
    }

    /// `MMRouterLib.fund` (`MMRouterLib.sol:265-297`): build the plan (any remainder reverts
    /// `InsufficientLiquidity`), drain every withdraw leg, then per borrow leg post its collateral,
    /// re-check it with `requireBorrowable` and borrow. Returns `(withdrawn, borrowed, posted)`.
    pub fn fund(
        &mut self,
        idx: u8,
        assets: U256,
        collateral_in: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<(U256, U256, U256), FlammError> {
        let plan = self.build_plan(idx, assets, collateral_in, price_wad, now)?;
        if !plan.remaining.is_zero() {
            return Err(FlammError::InsufficientLiquidity);
        }
        let (mut withdrawn, mut borrowed, mut posted) = (U256::ZERO, U256::ZERO, U256::ZERO);
        for id in self.withdraw_order.clone() {
            let take = plan.withdraw_take[id as usize];
            if take.is_zero() {
                continue;
            }
            withdrawn = checked_add(withdrawn, self.withdraw_supplied(id, take, now)?)?;
        }
        for id in self.borrow_order.clone() {
            let sl = plan.borrow_slice[id as usize];
            if sl.is_zero() {
                continue;
            }
            let post = plan.post[id as usize];
            if !post.is_zero() {
                let v = &mut self.venues[id as usize];
                v.morpho
                    .account_supply_collateral(post)?;
                v.managed_collateral = checked_add(v.managed_collateral, post)?;
                posted = checked_add(posted, post)?;
            }
            self.require_borrowable(&self.venues[id as usize], sl, price_wad, now)?;
            self.venues[id as usize]
                .morpho
                .account_borrow(sl, now)?;
            borrowed = checked_add(borrowed, sl)?;
        }
        Ok((withdrawn, borrowed, posted))
    }

    /// `MMRouterLib.repayCascade` (`MMRouterLib.sol:299-315`): repay order over the asset's
    /// readable indebted venues; each leg is `min(remaining, debt)` and counts what the account
    /// actually repaid (a remainder goes back to the pool).
    pub fn repay_cascade(&mut self, idx: u8, assets: U256, now: u64) -> Result<U256, FlammError> {
        self.loan_at(idx)?;
        let mut repaid = U256::ZERO;
        let mut remaining = assets;
        for id in self.repay_order.clone() {
            if remaining.is_zero() {
                break;
            }
            let v = self
                .venues
                .get(id as usize)
                .ok_or(FlammError::PanicIndex)?;
            if v.loan_index != idx {
                continue;
            }
            let rd = self.read(v, now)?;
            if !rd.readable || rd.debt.is_zero() {
                continue;
            }
            let pay = min_u(remaining, rd.debt);
            self.snapshot_for_proportional(id, now)?;
            let got = self.venues[id as usize]
                .morpho
                .account_repay(pay, now)?;
            repaid = checked_add(repaid, got)?;
            remaining = checked_sub(remaining, got)?;
        }
        Ok(repaid)
    }

    /// `MMRouterLib.supplyCascade` (`MMRouterLib.sol:317-337`): nothing while paused; supply order
    /// over the asset's supply-enabled venues, each take bounded by the venue's and the asset's
    /// remaining supply room.
    pub fn supply_cascade(&mut self, idx: u8, assets: U256, now: u64) -> Result<U256, FlammError> {
        if self.global_paused {
            return Ok(U256::ZERO);
        }
        let l = *self.loan_at(idx)?;
        let mut supplied = U256::ZERO;
        let mut remaining = assets;
        let mut asset_room = self.loan_supply_room(&l, idx, now)?;
        for id in self.supply_order.clone() {
            if remaining.is_zero() || asset_room.is_zero() {
                break;
            }
            let v = self
                .venues
                .get(id as usize)
                .ok_or(FlammError::PanicIndex)?;
            if v.loan_index != idx || !v.supply_enabled {
                continue;
            }
            let mut room = self.supply_room(v, now)?;
            if room > asset_room {
                room = asset_room;
            }
            let take = min_u(remaining, room);
            if take.is_zero() {
                continue;
            }
            let shares = self.venues[id as usize]
                .morpho
                .account_supply(take, now)?;
            let v = &mut self.venues[id as usize];
            v.managed_supply_shares = checked_add(v.managed_supply_shares, shares)?;
            supplied = checked_add(supplied, take)?;
            remaining -= take;
            asset_room -= take;
        }
        Ok(supplied)
    }

    /// `MMRouterLib.reclaim` (`MMRouterLib.sol:339-355`): REVERSE borrow order, each venue giving
    /// up its free collateral. The strict entry (`MMRouter.reclaim`, `MMRouter.sol:280-283`)
    /// reverts `InsufficientCollateral` on a shortfall; `reclaimBestEffort` (`:286-288`)
    /// returns what it got.
    pub fn reclaim(
        &mut self,
        assets: U256,
        price_wads: &[U256],
        strict: bool,
        now: u64,
    ) -> Result<U256, FlammError> {
        if price_wads.len() != self.loans.len() {
            return Err(FlammError::InvalidConfig);
        }
        let mut got = U256::ZERO;
        let mut remaining = assets;
        for i in (0..self.borrow_order.len()).rev() {
            if remaining.is_zero() {
                break;
            }
            let id = self.borrow_order[i] as usize;
            let v = self
                .venues
                .get(id)
                .ok_or(FlammError::PanicIndex)?;
            let price = price_wads
                .get(v.loan_index as usize)
                .copied()
                .ok_or(FlammError::PanicIndex)?;
            let free = self.free_collateral(v, price, now)?;
            let take = min_u(remaining, free);
            if take.is_zero() {
                continue;
            }
            let v = &mut self.venues[id];
            v.morpho
                .account_withdraw_collateral(take, now)?;
            v.managed_collateral = checked_sub(v.managed_collateral, take)?;
            got = checked_add(got, take)?;
            remaining -= take;
        }
        if strict && got != assets {
            return Err(FlammError::InsufficientCollateral);
        }
        Ok(got)
    }

    // ------------------------------------------------------------------ MMRouter single-venue
    // entries

    /// `MMRouterLib.snapshotForProportional` (`MMRouterLib.sol:804-812`); the snapshot lives in
    /// `transient_repay` until [`Router::end_transaction`].
    pub fn snapshot_for_proportional(&mut self, id: u16, now: u64) -> Result<(), FlammError> {
        let v = self
            .venues
            .get(id as usize)
            .ok_or(FlammError::PanicIndex)?;
        let debt = v.morpho.debt_of(now)?;
        let snap = pack_repay_snapshot(debt, v.morpho.position.collateral);
        self.transient_repay.insert(id, snap);
        Ok(())
    }

    /// Clears the transient repay snapshots, as EIP-1153 storage is at the end of a transaction: a
    /// proportional `withdrawCollateral` in a later transaction finds none and reverts
    /// `NoRepaySnapshot`.
    pub fn end_transaction(&mut self) {
        self.transient_repay.clear();
    }

    /// `MMRouter.postCollateral` (`MMRouter.sol:199-207`).
    pub fn post_collateral(&mut self, id: u16, assets: U256) -> Result<(), FlammError> {
        self.live(id)?;
        if assets.is_zero() {
            return Err(FlammError::InvalidConfig);
        }
        let v = &mut self.venues[id as usize];
        v.morpho
            .account_supply_collateral(assets)?;
        v.managed_collateral = checked_add(v.managed_collateral, assets)?;
        Ok(())
    }

    /// `MMRouterLib.withdrawCollateral` (`MMRouterLib.sol:193-211`): bounded by the recognized
    /// collateral; an indebted venue must stay no worse than its repay snapshot (proportional) or
    /// within the pin at an in-band cross.
    pub fn withdraw_collateral(
        &mut self,
        id: u16,
        assets: U256,
        price_wad: U256,
        proportional: bool,
        now: u64,
    ) -> Result<(), FlammError> {
        let v = self.venue_at(id)?;
        let recognized = min_u(v.morpho.position.collateral, v.managed_collateral);
        if assets.is_zero() || assets > recognized {
            return Err(FlammError::InsufficientCollateral);
        }
        let debt = v.morpho.debt_of(now)?;
        let coll = v.morpho.position.collateral;
        if !debt.is_zero() {
            if proportional {
                let snap = self
                    .transient_repay
                    .get(&id)
                    .copied()
                    .unwrap_or_default();
                if snap.coll.is_zero() {
                    return Err(FlammError::NoRepaySnapshot);
                }
                let lhs = checked_mul(debt, snap.coll)?;
                let rhs = checked_mul(snap.debt, checked_sub(coll, assets)?)?;
                if lhs > rhs {
                    return Err(FlammError::Unhealthy);
                }
            } else {
                if !self.band_ok(v, price_wad)? {
                    return Err(FlammError::OracleBand);
                }
                if debt > self.max_debt_at_pin(v, checked_sub(coll, assets)?, price_wad)? {
                    return Err(FlammError::Unhealthy);
                }
            }
        }
        let v = &mut self.venues[id as usize];
        v.morpho
            .account_withdraw_collateral(assets, now)?;
        v.managed_collateral = checked_sub(v.managed_collateral, assets)?;
        Ok(())
    }

    /// `MMRouter.borrow` (`MMRouter.sol:215-224`): `requireBorrowable`, then the asset debt cap.
    pub fn borrow(
        &mut self,
        id: u16,
        assets: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<(), FlammError> {
        let v = self.live(id)?;
        if assets.is_zero() {
            return Err(FlammError::InvalidConfig);
        }
        self.require_borrowable(v, assets, price_wad, now)?;
        let l = self.loan_of(v)?;
        if assets > self.loan_debt_room(l, v.loan_index, now)? {
            return Err(FlammError::DebtCapExceeded);
        }
        self.venues[id as usize]
            .morpho
            .account_borrow(assets, now)
    }

    /// `MMRouter.repay` (`MMRouter.sol:227-234`): snapshot, then `min(assets, debtOf)` repaid into
    /// the account.
    pub fn repay(&mut self, id: u16, assets: U256, now: u64) -> Result<U256, FlammError> {
        self.venue_at(id)?;
        self.snapshot_for_proportional(id, now)?;
        let v = &mut self.venues[id as usize];
        let debt = v.morpho.debt_of(now)?;
        v.morpho
            .account_repay(min_u(assets, debt), now)
    }

    /// `MMRouter.supply` (`MMRouter.sol:237-248`): `requireSuppliable` (`MMRouterLib.sol:707-711`),
    /// the asset supply cap, then the managed shares grow by the mint.
    pub fn supply(&mut self, id: u16, assets: U256, now: u64) -> Result<U256, FlammError> {
        let v = self.live(id)?;
        if assets.is_zero() {
            return Err(FlammError::InvalidConfig);
        }
        if self.global_paused {
            return Err(FlammError::GlobalPaused);
        }
        if !v.supply_enabled {
            return Err(FlammError::VenueDisabled);
        }
        if assets > self.supply_room(v, now)? {
            return Err(FlammError::SupplyCapExceeded);
        }
        let l = self.loan_of(v)?;
        if assets > self.loan_supply_room(l, v.loan_index, now)? {
            return Err(FlammError::SupplyCapExceeded);
        }
        let v = &mut self.venues[id as usize];
        let shares = v.morpho.account_supply(assets, now)?;
        v.managed_supply_shares = checked_add(v.managed_supply_shares, shares)?;
        Ok(shares)
    }

    /// `MMRouter.withdrawSupplied` (`MMRouter.sol:251-256`).
    pub fn withdraw_supplied_entry(
        &mut self,
        id: u16,
        assets: U256,
        now: u64,
    ) -> Result<U256, FlammError> {
        self.venue_at(id)?;
        self.withdraw_supplied(id, assets, now)
    }
}

impl GateReads for Router {
    fn positions(&self, now: u64) -> Result<Positions, FlammError> {
        Router::positions(self, now)
    }

    fn quarantine(&self, idx: u8, now: u64) -> Result<Quarantine, FlammError> {
        Router::quarantine(self, idx, now)
    }
}

// ------------------------------------------------------------------ FLAMMSwapLib settlement legs

/// `FLAMMSwapLib.payLoan` (`FLAMMSwapLib.sol:241-256`): pay `net` of loan asset `idx` from tracked
/// liquid, funding the shortfall through `fund` with the whole physical poolAsset offered as
/// collateral; the posted collateral leaves physical and the withdrawn + borrowed cash lands in
/// liquid.
pub fn pay_loan(
    pool: &mut Pool,
    r: &mut Router,
    idx: u8,
    price_wad: U256,
    net: U256,
    now: u64,
) -> Result<(), FlammError> {
    let i = idx as usize;
    if i >= pool.loans.len() {
        return Err(FlammError::PanicIndex);
    }
    if net > pool.loans[i].liquid {
        let rest = net - pool.loans[i].liquid;
        let coll = pool.physical;
        let (withdrawn, borrowed, posted) = r.fund(idx, rest, coll, price_wad, now)?;
        pool.physical = checked_sub(pool.physical, posted)?;
        let cfg = &mut pool.loans[i];
        cfg.liquid = checked_add(cfg.liquid, withdrawn)?;
        cfg.liquid = checked_add(cfg.liquid, borrowed)?;
    }
    let cfg = &mut pool.loans[i];
    cfg.liquid = checked_sub(cfg.liquid, net)?;
    Ok(())
}

/// `FLAMMSwapLib.takeLoan` (`FLAMMSwapLib.sol:265-287`): `used` lands in liquid, repays the asset's
/// counted debt through `repayCascade`, and the surplus above the reserve target is lent through
/// `supplyCascade` when lending is on.
pub fn take_loan(
    pool: &mut Pool,
    r: &mut Router,
    idx: u8,
    used: U256,
    now: u64,
) -> Result<(), FlammError> {
    let i = idx as usize;
    if i >= pool.loans.len() {
        return Err(FlammError::PanicIndex);
    }
    pool.loans[i].liquid = checked_add(pool.loans[i].liquid, used)?;
    let (_, _, debt) = r.position(idx, now)?;
    let pay = min_u(debt, pool.loans[i].liquid);
    if !pay.is_zero() {
        let repaid = r.repay_cascade(idx, pay, now)?;
        pool.loans[i].liquid = checked_sub(pool.loans[i].liquid, repaid)?;
    }
    let excess = pool.loans[i]
        .liquid
        .saturating_sub(pool.loans[i].reserve_target);
    if !excess.is_zero() && !(pool.features & FEATURE_SUPPLY_LENDING).is_zero() {
        let supplied = r.supply_cascade(idx, excess, now)?;
        pool.loans[i].liquid = checked_sub(pool.loans[i].liquid, supplied)?;
    }
    Ok(())
}

/// `FLAMMSwapLib.releaseExcess` (`FLAMMSwapLib.sol:313-326`): posted collateral above the posting
/// law, released best-effort once the excess is at least `RELEASE_HYSTERESIS` of posted; a Router
/// revert is swallowed (its writes undone).
pub fn release_excess(pool: &mut Pool, r: &mut Router, now: u64) -> Result<(), FlammError> {
    let b = priced(pool, r, now)?;
    if b.legs
        .iter()
        .any(|l| !l.debt.is_zero() && l.price_wad.is_zero())
    {
        return Ok(());
    }
    let need = required_posted_all(&b, pool.ltv_wad)?;
    if b.posted <= need {
        return Ok(());
    }
    let excess = b.posted - need;
    if mul_div(excess, WAD, b.posted)? < RELEASE_HYSTERESIS_WAD {
        return Ok(());
    }
    let mut trial = r.clone();
    if let Ok(got) = trial.reclaim(excess, &price_wads(&b), false, now) {
        *r = trial;
        pool.physical = checked_add(pool.physical, got)?;
    }
    Ok(())
}

/// `FLAMMSwapLib.settleSell` (`FLAMMSwapLib.sol:290-295`) as one swap transaction: run on clones,
/// written to the pool and router only on success, with the transient repay snapshots ending with
/// the transaction.
pub fn settle_sell(
    pool: &mut Pool,
    r: &mut Router,
    idx: u8,
    price_wad: U256,
    used: U256,
    net: U256,
    now: u64,
) -> Result<(), FlammError> {
    let (mut pc, mut rc) = (pool.clone(), r.clone());
    settle_sell_legs(&mut pc, &mut rc, idx, price_wad, used, net, now)?;
    rc.end_transaction();
    *pool = pc;
    *r = rc;
    Ok(())
}

/// The body of `FLAMMSwapLib.settleSell`, writing through: `used` poolAsset in, `net` of loan asset
/// `idx` paid out at the leg's checked cross `price_wad`, then `assertGate`.
pub fn settle_sell_legs(
    pc: &mut Pool,
    rc: &mut Router,
    idx: u8,
    price_wad: U256,
    used: U256,
    net: U256,
    now: u64,
) -> Result<(), FlammError> {
    pc.physical = checked_add(pc.physical, used)?;
    pay_loan(pc, rc, idx, price_wad, net, now)?;
    assert_gate(pc, rc, now)
}

/// The buy branch of `FLAMMSwapLib.execute` (`FLAMMSwapLib.sol:103-105`) as one swap transaction:
/// run on clones, written to the pool and router only on success, with the transient repay
/// snapshots ending with the transaction.
pub fn settle_buy(
    pool: &mut Pool,
    r: &mut Router,
    idx: u8,
    used: U256,
    net: U256,
    now: u64,
) -> Result<(), FlammError> {
    let (mut pc, mut rc) = (pool.clone(), r.clone());
    settle_buy_legs(&mut pc, &mut rc, idx, used, net, now)?;
    rc.end_transaction();
    *pool = pc;
    *r = rc;
    Ok(())
}

/// The buy branch of `FLAMMSwapLib.execute`, writing through: `anchor`, `settleBuy`
/// (`FLAMMSwapLib.sol:299-309`: `takeLoan`; reclaim the payout beyond physical, strictly, at the
/// pool's price vector; pay `net` poolAsset; `releaseExcess`), then the entry gate against the
/// anchor.
pub fn settle_buy_legs(
    pc: &mut Pool,
    rc: &mut Router,
    idx: u8,
    used: U256,
    net: U256,
    now: u64,
) -> Result<(), FlammError> {
    let (u0, gross0, q0) = anchor(pc, rc, now)?;
    take_loan(pc, rc, idx, used, now)?;
    if pc.physical < net {
        let short = net - pc.physical;
        let got = rc.reclaim(short, &pc.price_wad, true, now)?;
        pc.physical = checked_add(pc.physical, got)?;
    }
    pc.physical = checked_sub(pc.physical, net)?;
    release_excess(pc, rc, now)?;
    assert_entry_gate(pc, rc, now, &u0, gross0, q0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repay_snapshot_packing() {
        let two128 = U256::from(1) << 128;
        let max128 = two128 - U256::from(1);
        for (debt, coll, want_debt, want_coll) in [
            (
                U256::from(85_209_356),
                U256::from(196_499),
                U256::from(85_209_356),
                U256::from(196_499),
            ),
            (max128, max128, max128, max128),
            (two128, U256::from(7), U256::ZERO, U256::from(7)),
            (two128 + U256::from(3), U256::from(1), U256::from(3), U256::from(1)),
        ] {
            let s = pack_repay_snapshot(debt, coll);
            assert_eq!(s.debt, want_debt, "debt {debt}");
            assert_eq!(s.coll, want_coll, "coll {coll}");
        }
    }

    #[test]
    fn loan_bits() {
        assert_eq!(loan_bit(0), 1);
        assert_eq!(loan_bit(7), 128);
        assert_eq!(loan_bit(8), 0);
        assert_eq!(loan_bit(255), 0);
    }

    #[test]
    fn counted_haircut_ceils() {
        let r = Read { readable: false, debt: U256::from(100), ..Default::default() };
        assert_eq!(counted(&r), Ok(U256::from(105)));
        let r = Read { readable: false, debt: U256::from(1), ..Default::default() };
        assert_eq!(counted(&r), Ok(U256::from(2)));
        let r = Read { readable: true, debt: U256::from(1), ..Default::default() };
        assert_eq!(counted(&r), Ok(U256::from(1)));
    }

    #[test]
    fn empty_router_views() {
        let r = Router { loans: vec![Loan::default()], ..Default::default() };
        assert_eq!(r.positions(0).unwrap().total_coll, U256::ZERO);
        assert_eq!(r.drawn(0), Ok((0, 0)));
        assert!(r.drawable(0, 0).unwrap());
        assert_eq!(r.min_lltv(0), Ok(U256::ZERO));
        assert_eq!(r.reclaimable(&[U256::ZERO], 0), Ok(U256::ZERO));
        assert_eq!(r.reclaimable(&[], 0), Err(FlammError::InvalidConfig));
        assert_eq!(r.position(1, 0), Err(FlammError::BadLoanIndex));
        assert_eq!(r.venue_position(0, 0), Err(FlammError::BadVenueId));
        assert_eq!(r.funding_ceiling(0, U256::ZERO, U256::ZERO, 0), Ok(U256::ZERO));
    }
}
