// Copyright (c) 2026 Everlong Labs Limited

//! Wei-exact port of the swap path of `EverlongHook.sol` (c104 @ `80abd43`,
//! `src/hooks/everlong/EverlongHook.sol`): the lazy book rescale (`_book`), the fee role (`_fee`),
//! the invariant role (`_fill`, `_fillSell`, `_fillBuy`, `_swap`, `_maxInForGrossCap`, `_spotAt`)
//! and the book commit an `executeExactIn` writes. The curve's stable leg is the pool numeraire N18
//! and the volatile leg pool-asset base units; prices held here are the pool's `priceWad` times
//! `WAD`.
//!
//! Nothing here mutates a [`HookState`] in place: [`HookState::execute_exact_in`] returns the
//! post-state the hook would store, so a caller can keep the pre-state for a preview and adopt the
//! post-state when the fill is applied.

use alloy::primitives::{uint, U256};

use super::{
    almcurve::{self, Support},
    context::{PoolContext, SwapContext},
    error::FlammError,
    fee::{self, FeeParams, FeeState},
    math::{checked_add, checked_mul, checked_sub, div, mul_div, HALF_WAD, WAD},
};

/// `EverlongHook.KAPPA_SEED` (`EverlongHook.sol:41`): the scale an empty accounted book is
/// re-seeded at.
pub const KAPPA_SEED: U256 = uint!(1_000_000_000_000_000_000_000_000_000_000_U256);

/// The `EverlongHook` storage the swap path reads, slot for slot (`EverlongHook.sol:94-111`).
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookState {
    /// `_p.aWad`, read by `_spotAt` (`EverlongHook.sol:654`).
    pub a_wad: U256,
    /// `_sup`, read by the curve solve and the seed.
    pub support: Support,
    pub anchor_sqrt_x96: U256,
    pub reservation_price_wad: U256,
    pub kappa: U256,
    pub x_wad: U256,
    pub reserve_stable: U256,
    pub idle_stable: U256,
    pub reserve_volatile: U256,
    pub idle_volatile: U256,
    pub rv_wad: U256,
    /// `_p.tuning.fee`.
    pub fee: FeeParams,
    /// `_p.tuning.invSkewKappaWad`.
    pub inv_skew_kappa_wad: U256,
    /// `_p.tuning.invSkewBandWad`.
    pub inv_skew_band_wad: U256,
    /// The immutable `LOAN_SCALE = 10 ** (18 - loanDecimals)` (`EverlongHook.sol:156`).
    pub loan_scale: U256,
}

/// `EverlongHook.Book` (`EverlongHook.sol:84-91`): the book as an execution materialises it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Book {
    pub kappa: U256,
    pub rs: U256,
    pub is: U256,
    pub rv: U256,
    pub iv: U256,
    pub x: U256,
}

/// `IFLAMMHooks.FillResult` (`IFLAMMHooks.sol:42-47`): the input consumed, the gross output, the
/// fee retained on it, and the post-fill spot in the pool's price units. `cap_evals` is not part of
/// the on-chain struct: it counts the curve solves `_maxInForGrossCap` ran for the fill, which a
/// settlement's gas grows with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillResult {
    pub amount_in_used: U256,
    pub gross_out: U256,
    pub fee_out: U256,
    pub spot_after_wad: U256,
    pub cap_evals: u64,
}

/// The shared cap step's outcome (`_fillSell` / `_fillBuy` before the grid snap).
struct CapFill {
    gross: U256,
    x_after: U256,
    used: U256,
    net: U256,
    evals: u64,
}

impl HookState {
    /// `EverlongHook.bookFor` / `_book` (`EverlongHook.sol:598-622`): the stored book rescaled
    /// lazily by `gross / (rv + iv)`. An empty accounted book (`rv + iv == 0`) is first
    /// re-seeded at [`KAPPA_SEED`], at the stored coordinate, or at `WAD / 2` when that
    /// coordinate holds no volatile, with idle cleared.
    pub fn book_for(&self, ctx: &PoolContext) -> Result<Book, FlammError> {
        let mut b = Book {
            kappa: self.kappa,
            rs: self.reserve_stable,
            is: self.idle_stable,
            rv: self.reserve_volatile,
            iv: self.idle_volatile,
            x: self.x_wad,
        };
        let gross = checked_add(ctx.physical_pool_asset, ctx.posted_pool_asset)?;
        let mut acc = checked_add(b.rv, b.iv)?;
        if acc.is_zero() {
            b.kappa = KAPPA_SEED;
            (b.rs, b.rv) =
                almcurve::reserves_at(&self.support, self.anchor_sqrt_x96, KAPPA_SEED, b.x)?;
            if b.rv.is_zero() {
                b.x = HALF_WAD;
                (b.rs, b.rv) = almcurve::reserves_at(
                    &self.support,
                    self.anchor_sqrt_x96,
                    KAPPA_SEED,
                    HALF_WAD,
                )?;
            }
            b.is = U256::ZERO;
            b.iv = U256::ZERO;
            acc = b.rv;
            if acc.is_zero() {
                return Ok(b);
            }
        }
        if gross == acc {
            return Ok(b);
        }
        b.kappa = mul_div(b.kappa, gross, acc)?;
        let mut rv = mul_div(b.rv, gross, acc)?;
        if rv > gross {
            rv = gross;
        }
        b.rv = rv;
        b.iv = gross - rv;
        b.rs = mul_div(b.rs, gross, acc)?;
        b.is = mul_div(b.is, gross, acc)?;
        Ok(b)
    }

    /// `EverlongHook._spotAt` (`EverlongHook.sol:653-655`): `priceAtX(x, _p.aWad) *
    /// reservationPriceWad / WAD`, the hook's WAD price (the pool's price units times `WAD`).
    pub fn spot_at(&self, x: U256) -> Result<U256, FlammError> {
        let p = almcurve::price_at_x(x, self.a_wad)?;
        mul_div(p, self.reservation_price_wad, WAD)
    }

    /// `EverlongHook.spot` (`EverlongHook.sol:407-409`): the spot at the stored coordinate in the
    /// pool's price units (the context is ignored on chain and is not taken).
    pub fn spot(&self) -> Result<U256, FlammError> {
        Ok(self.spot_at(self.x_wad)? / WAD)
    }

    /// `EverlongHook.previewFeeWad` / `executeFeeWad` (`_fee`, `EverlongHook.sol:499-504`): the
    /// fill fee on the book as the execution would materialise it. The pool's `spotBefore`
    /// argument is ignored on chain and is not taken.
    pub fn preview_fee_wad(&self, ctx: &SwapContext) -> Result<U256, FlammError> {
        let b = self.book_for(&ctx.pool)?;
        let st = FeeState {
            reserve_stable: b.rs,
            reserve_volatile: b.rv,
            anchor_wad: self.reservation_price_wad,
            spot_wad: self.spot_at(b.x)?,
            rv_wad: self.rv_wad,
        };
        fee::fill_fee(
            &self.fee,
            &st,
            !ctx.pool_asset_in,
            self.inv_skew_kappa_wad,
            self.inv_skew_band_wad,
        )
    }

    /// `EverlongHook.previewExactIn` (`EverlongHook.sol:411-413`): the exact-input fill at
    /// `fee_wad` without a commit.
    pub fn preview_exact_in(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
    ) -> Result<FillResult, FlammError> {
        Ok(self.fill(ctx, fee_wad)?.0)
    }

    /// `EverlongHook.executeExactIn` (`EverlongHook.sol:415-420`): the same fill, returning the
    /// post-state the hook stores. The book is committed (kappa, reserves, idle, x) only when
    /// the fill consumed input; otherwise the state is returned unchanged, lazy rescale
    /// included.
    pub fn execute_exact_in(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
    ) -> Result<(FillResult, HookState), FlammError> {
        let mut post = *self;
        let (fr, b) = self.fill(ctx, fee_wad)?;
        if !fr.amount_in_used.is_zero() {
            post.commit(&b);
        }
        Ok((fr, post))
    }

    /// `EverlongHook._commit` (`EverlongHook.sol:624-631`).
    pub fn commit(&mut self, b: &Book) {
        self.kappa = b.kappa;
        self.reserve_stable = b.rs;
        self.idle_stable = b.is;
        self.reserve_volatile = b.rv;
        self.idle_volatile = b.iv;
        self.x_wad = b.x;
    }

    /// `EverlongHook._fill` (`EverlongHook.sol:507-512`): a retracted book, a zero input or a fee
    /// at 100% fills nothing. Returns the fill and the book it was made on (the rescaled book
    /// when nothing fills).
    pub fn fill(&self, ctx: &SwapContext, fee_wad: U256) -> Result<(FillResult, Book), FlammError> {
        let b = self.book_for(&ctx.pool)?;
        if b.kappa.is_zero() || ctx.amount_in.is_zero() || fee_wad >= WAD {
            return Ok((FillResult::default(), b));
        }
        if ctx.pool_asset_in {
            self.fill_sell(ctx, fee_wad, b)
        } else {
            self.fill_buy(ctx, fee_wad, b)
        }
    }

    /// The shared cap step of `_fillSell` / `_fillBuy` (`EverlongHook.sol:520-529`, `:549-558`):
    /// when the net output exceeds `maxAmountOut`, the input is re-solved as the largest whose
    /// gross stays within `cap * WAD / (WAD - fee)`, and the net is then clamped to the cap. A
    /// buy never bisects in practice: its `maxAmountOut` is the pool's gross pool asset
    /// (`FLAMMSwapLib.sol:151`), the same gross `_book` clamps `rv` to (`:617`), and `_swap`
    /// clamps the output to `rv` (`:578`), so `net <= gross <= rv <= maxAmountOut`.
    fn cap_fill(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
        b: &Book,
        stable_in: bool,
    ) -> Result<CapFill, FlammError> {
        let (mut gross, mut x_after, mut unspent) = self.swap(b, stable_in, ctx.amount_in)?;
        let mut used = ctx.amount_in - unspent;
        let mut net = net_of(gross, fee_wad)?;
        let mut evals = 0;
        if net > ctx.max_amount_out {
            let cap_gross = mul_div(ctx.max_amount_out, WAD, WAD - fee_wad)?;
            let (max_in, n) = self.max_in_for_gross_cap(b, stable_in, ctx.amount_in, cap_gross)?;
            evals = n;
            (gross, x_after, unspent) = self.swap(b, stable_in, max_in)?;
            used = max_in - unspent;
            net = net_of(gross, fee_wad)?;
            if net > ctx.max_amount_out {
                net = ctx.max_amount_out;
            }
        }
        Ok(CapFill { gross, x_after, used, net, evals })
    }

    /// `EverlongHook._fillSell` (`EverlongHook.sol:514-541`; pool asset in, N18 out). The fill is
    /// snapped to the loan asset's native grid: the taker is paid `floor(net / LOAN_SCALE)`
    /// native units, the reported gross and fee are grid multiples, and the unpaid residue
    /// `gross - netNative * LOAN_SCALE` is booked to idle stable.
    fn fill_sell(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
        mut b: Book,
    ) -> Result<(FillResult, Book), FlammError> {
        let c = self.cap_fill(ctx, fee_wad, &b, false)?;
        let fr = FillResult { cap_evals: c.evals, ..Default::default() };
        let net_native = div(c.net, self.loan_scale)?;
        if c.used.is_zero() || c.gross.is_zero() || net_native.is_zero() {
            return Ok((fr, b));
        }
        let gross_native = div(c.gross, self.loan_scale)?;
        b.rv = checked_add(b.rv, c.used)?;
        b.rs = checked_sub(b.rs, c.gross)?;
        // netNative * LOAN_SCALE <= net <= gross.
        let paid = net_native * self.loan_scale;
        b.is = checked_add(b.is, c.gross - paid)?;
        b.x = c.x_after;
        let spot = self.spot_at(c.x_after)?;
        let fr = FillResult {
            amount_in_used: c.used,
            gross_out: gross_native * self.loan_scale,
            fee_out: (gross_native - net_native) * self.loan_scale,
            spot_after_wad: spot / WAD,
            cap_evals: c.evals,
        };
        Ok((fr, b))
    }

    /// `EverlongHook._fillBuy` (`EverlongHook.sol:543-569`; N18 in, pool asset out). The charge is
    /// the used input rounded UP to the native grid; the over-collected residue `paid - used`
    /// is booked to idle stable, and the fee is retained in kind as idle volatile.
    fn fill_buy(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
        mut b: Book,
    ) -> Result<(FillResult, Book), FlammError> {
        let c = self.cap_fill(ctx, fee_wad, &b, true)?;
        let fr = FillResult { cap_evals: c.evals, ..Default::default() };
        if c.used.is_zero() || c.gross.is_zero() || c.net.is_zero() {
            return Ok((fr, b));
        }
        // Math.ceilDiv (OZ 4.8, Math.sol:45-48): (a - 1) / b + 1 for a != 0.
        let used_native = div(c.used - U256::from(1), self.loan_scale)? + U256::from(1);
        let paid_l18 = checked_mul(used_native, self.loan_scale)?;
        b.rs = checked_add(b.rs, c.used)?;
        if paid_l18 > c.used {
            b.is = checked_add(b.is, paid_l18 - c.used)?;
        }
        b.rv = checked_sub(b.rv, c.gross)?;
        let fee_out = c.gross - c.net;
        b.iv = checked_add(b.iv, fee_out)?;
        b.x = c.x_after;
        let spot = self.spot_at(c.x_after)?;
        let fr = FillResult {
            amount_in_used: paid_l18,
            gross_out: c.gross,
            fee_out,
            spot_after_wad: spot / WAD,
            cap_evals: c.evals,
        };
        Ok((fr, b))
    }

    /// `EverlongHook._swap` (`EverlongHook.sol:572-580`): the curve solve, its gross output clamped
    /// to the deployed reserve of the output leg (`rv` for a stable-in fill, `rs` for a
    /// volatile-in fill).
    ///
    /// Returns `(gross, x_after, unspent)`.
    pub fn swap(
        &self,
        b: &Book,
        stable_in: bool,
        amount_in: U256,
    ) -> Result<(U256, U256, U256), FlammError> {
        let f = almcurve::swap_exact_in_x96(
            &self.support,
            self.anchor_sqrt_x96,
            b.kappa,
            b.x,
            stable_in,
            amount_in,
        )?;
        let available = if stable_in { b.rv } else { b.rs };
        let gross = if f.amount_out > available { available } else { f.amount_out };
        Ok((gross, f.x_after, f.amount_in_unspent))
    }

    /// `EverlongHook._maxInForGrossCap` (`EverlongHook.sol:583-595`): the largest input in `[0,
    /// hi]` whose clamped gross stays within `cap_gross`, by a 64-step bisection on the
    /// monotone solve (`lo + hi` checked). Also returns the number of `_swap` solves the loop
    /// ran, `min(64, about log2(hi))`.
    pub fn max_in_for_gross_cap(
        &self,
        b: &Book,
        stable_in: bool,
        hi: U256,
        cap_gross: U256,
    ) -> Result<(U256, u64), FlammError> {
        let mut lo = U256::ZERO;
        let mut hi = hi;
        let mut evals = 0;
        for _ in 0..64 {
            let mid = checked_add(lo, hi)? >> 1;
            if mid == lo {
                break;
            }
            evals += 1;
            let (g, _, _) = self.swap(b, stable_in, mid)?;
            if g <= cap_gross {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok((lo, evals))
    }
}

/// `gross - mulDiv(gross, feeWad, WAD)` (`EverlongHook.sol:522`): the output after the fee haircut
/// (`feeWad < WAD`).
fn net_of(gross: U256, fee_wad: U256) -> Result<U256, FlammError> {
    let haircut = mul_div(gross, fee_wad, WAD)?;
    Ok(gross - haircut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kappa_seed_is_1e30() {
        assert_eq!(KAPPA_SEED, U256::from(10u64).pow(U256::from(30)));
    }
}
