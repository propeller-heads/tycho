// Copyright (c) 2026 Everlong Labs Limited

//! Wei-exact port of `EverlongLeverageHook` (`src/hooks/everlong/lev/EverlongLeverageHook.sol` @
//! c104 `80abd43`, deployed on Base at `0xE0A98d8e60035832B8BaD7f7af7B9B0b3A7308F3`): the leverage
//! venue's frame over the swap hook's book and its exact fill. The hook is stateless, so
//! `executeLever == previewLever` and [`quote`] serves both.
//!
//! Reverts are reproduced in Solidity's evaluation order (`EverlongLeverageHook.sol:42-46`). A
//! checked-arithmetic `Panic(0x11)` and OpenZeppelin 4.8 `Math.mulDiv`'s bare
//! `require(denominator > prod1)` map to [`FlammError::PanicArithmetic`] and
//! [`FlammError::MulDivOverflow`]. `NotPool()` and `InvalidConfig()` (`:40-41`) are the
//! constructor's and `executeLever`'s caller checks, not quote outcomes, and are not modelled.

use alloy::primitives::{I256, U256};

use super::{
    context::{LeverContext, PoolContext},
    error::FlammError,
    levcurve::{anchor_and_base, deleverage_quote, leverage_quote, LEVERAGE_RATIO_WAD},
    math::{
        checked_add, checked_mul, int256_is_positive, mul_div, mul_div_up, signed_add_checked,
        signed_sub_checked, PPM, WAD,
    },
};

/// `CR_CEILING_WAD = 2.2e18` (`EverlongLeverageHook.sol:23`).
pub const CR_CEILING_WAD: U256 = U256::from_limbs([2_200_000_000_000_000_000, 0, 0, 0]);
/// `PPM + LEV_MAX_CONCESSION_PPM` (`:24`, `:114`).
pub const MAX_CONCESSION_PPM: U256 = U256::from_limbs([1_010_000, 0, 0, 0]);
/// `WAD * PRICE_BAND_NUM` (`:25`, `:129`).
pub const PRICE_BAND_UPPER_WAD: U256 = U256::from_limbs([2_000_000_000_000_000_000, 0, 0, 0]);
/// `WAD / PRICE_BAND_NUM` (`:25`, `:129`).
pub const PRICE_BAND_LOWER_WAD: U256 = U256::from_limbs([500_000_000_000_000_000, 0, 0, 0]);

/// What `frame` consumes from the swap hook: the four legs of `EverlongHook.bookFor(ctx)` (the book
/// already rescaled pro rata to the context's gross poolAsset) and
/// `EverlongHook.reservationPriceWad()` (L18 per base unit, times `WAD`). Kappa and `x` are not
/// read. The integrator fills it from the swap hook's `bookFor` port for the same [`PoolContext`]
/// the fill is quoted on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LevBook {
    pub rs: U256,
    pub is: U256,
    pub rv: U256,
    pub iv: U256,
    pub reservation_price_wad: U256,
}

/// `EverlongLeverageHook.Frame` (`:28-34`): `cv` and `s` in L18, `v` in poolAsset base units, `d`
/// signed and `x_anchor` the frozen curve's anchor on `(cv, d)` when `d > 0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LevFrame {
    pub cv: U256,
    pub d: I256,
    pub v: U256,
    pub s: U256,
    pub x_anchor: U256,
}

/// `IFLAMMLeverage.LeverFill` (`IFLAMMLeverage.sol:24-29`): `gross_out` is the curve's output
/// before the virtual-leg netting (L18 up, poolAsset base units down), `virtual_leg_l18` the
/// virtual stable leg the fill mints (up) or burns (down), `cr_after_wad` the frame's collateral
/// ratio after the fill at the pool's feed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LevFill {
    pub amount_in_used: U256,
    pub gross_out: U256,
    pub virtual_leg_l18: U256,
    pub cr_after_wad: U256,
}

/// `frame(ctx)` (`:73-81`): `v = rv + iv`, `s = rs + is_`, `cv = s + floor(v * reservationPriceWad
/// / WAD)`, `d = int256(s) + int256(debt) - int256(supplied + liquid)`, and
/// `xAnchor = anchorAndBase(cv, d, WAD, RATIO)` when `d > 0` (zero otherwise).
pub fn frame(ctx: &PoolContext, book: &LevBook) -> Result<LevFrame, FlammError> {
    let v = checked_add(book.rv, book.iv)?;
    let s = checked_add(book.rs, book.is)?;
    let v_value = mul_div(v, book.reservation_price_wad, WAD)?;
    let cv = checked_add(s, v_value)?;
    let credit = checked_add(ctx.supplied_loan_asset, ctx.liquid_loan_asset)?;
    let d = signed_add_checked(s, ctx.debt_loan_asset)?;
    let d = signed_sub_checked(d, credit)?;
    let x_anchor = if int256_is_positive(d) {
        anchor_and_base(cv, d, WAD, LEVERAGE_RATIO_WAD).0
    } else {
        U256::ZERO
    };
    Ok(LevFrame { cv, d: I256::from_raw(d), v, s, x_anchor })
}

/// `_quote(ctx)` (`:88-122`), the body of `previewLever` and `executeLever`.
///
/// Up: `dq = max(D, ceil(cv * WAD / 2.2e18))` is the CR-ceiling-clamped debt the curve prices on,
/// `dCv = floor(cv * in / v)` the collateral delta and `dS = ceil(s * in / v)` the virtual leg the
/// join mints. The gross is `leverageQuote(cv, dq, WAD, RATIO, spread, dCv)` capped at
/// `floor(dCv * (PPM - spread) / PPM)` and must exceed `dS`.
///
/// Down: `in18 < D`, `collOut = deleverageQuote(cv, D, WAD, RATIO, spread, in18)` in `(0, cv)`,
/// bounded by `floor(in18 * 1.01)`; the taker receives `volOut = floor(v * collOut / cv)` and
/// `dSBurn = floor(s * collOut / cv)` of the virtual leg is burnt, which `in18` must exceed.
///
/// Both sides assert the anchor and the 2x value band on the post-fill `(cv, D)` and report
/// `crAfter = floor(gavAfter * WAD / dAfter)`, `gavAfter` the book at the pool's feed scaled by the
/// fill.
pub fn quote(ctx: &LeverContext, book: &LevBook) -> Result<LevFill, FlammError> {
    let f = frame(&ctx.pool, book)?;
    if f.v.is_zero() || f.cv.is_zero() || !f.d.is_positive() {
        return Err(FlammError::FrameUnquotable);
    }
    let d = f.d.into_raw();
    let v_at_feed = checked_mul(f.v, ctx.pool.price_wad)?;
    let gav_at_feed = checked_add(f.s, v_at_feed)?;

    if ctx.up {
        let mut dq = mul_div_up(f.cv, WAD, CR_CEILING_WAD)?;
        if dq < d {
            dq = d;
        }
        let d_cv = mul_div(f.cv, ctx.amount_in, f.v)?;
        let d_s = mul_div_up(f.s, ctx.amount_in, f.v)?;
        let mut gross = leverage_quote(f.cv, dq, WAD, LEVERAGE_RATIO_WAD, ctx.spread_ppm, d_cv).out;
        // The clamped debt describes a book that does not exist: never pay more than the delta is
        // worth at its own mark, net of the spread (`PPM - spreadPpm` is checked).
        if ctx.spread_ppm > PPM {
            return Err(FlammError::PanicArithmetic);
        }
        let cap = mul_div(d_cv, PPM - ctx.spread_ppm, PPM)?;
        if gross > cap {
            gross = cap;
        }
        if gross <= d_s {
            return Err(FlammError::NothingToFill);
        }
        let d_after = checked_add(d, gross)?;
        let cv_after = checked_add(f.cv, d_cv)?;
        assert_anchor_and_band(f.x_anchor, cv_after, d_after)?;
        let v_after = checked_add(f.v, ctx.amount_in)?;
        let gav_after = mul_div(gav_at_feed, v_after, f.v)?;
        let cr_after = mul_div(gav_after, WAD, d_after)?;
        return Ok(LevFill {
            amount_in_used: ctx.amount_in,
            gross_out: gross,
            virtual_leg_l18: d_s,
            cr_after_wad: cr_after,
        });
    }

    let in18 = ctx.amount_in;
    if in18 >= d {
        return Err(FlammError::NothingToFill);
    }
    let coll_out = deleverage_quote(f.cv, d, WAD, LEVERAGE_RATIO_WAD, ctx.spread_ppm, in18).out;
    if coll_out.is_zero() || coll_out >= f.cv {
        return Err(FlammError::NothingToFill);
    }
    // Bounded, not banned: near target the curve pays a small, honest premium; far from it, it must
    // not.
    let concession = mul_div(in18, MAX_CONCESSION_PPM, PPM)?;
    if coll_out > concession {
        return Err(FlammError::LevValueLeak);
    }
    let vol_out = mul_div(f.v, coll_out, f.cv)?;
    let d_s_burn = mul_div(f.s, coll_out, f.cv)?;
    if vol_out.is_zero() || in18 <= d_s_burn {
        return Err(FlammError::NothingToFill);
    }
    let d_after_down = d - in18;
    let cv_after_down = f.cv - coll_out;
    assert_anchor_and_band(f.x_anchor, cv_after_down, d_after_down)?;
    let gav_after_down = mul_div(gav_at_feed, cv_after_down, f.cv)?;
    let cr_after = mul_div(gav_after_down, WAD, d_after_down)?;
    Ok(LevFill {
        amount_in_used: in18,
        gross_out: vol_out,
        virtual_leg_l18: d_s_burn,
        cr_after_wad: cr_after,
    })
}

/// `_assertAnchorAndBand` (`:125-130`): the post-fill anchor must not fall below the frame's
/// (`FillValueDrop`, also on a zero `baseX`) and `floor(baseX * WAD / cvAfter)` must lie in
/// `[WAD / 2, 2 * WAD]` (`FillPriceBand`).
pub fn assert_anchor_and_band(
    x_anchor_before: U256,
    cv_after: U256,
    d_after: U256,
) -> Result<(), FlammError> {
    let (x_anchor_after, base_x_after) =
        anchor_and_base(cv_after, d_after, WAD, LEVERAGE_RATIO_WAD);
    if x_anchor_after < x_anchor_before || base_x_after.is_zero() {
        return Err(FlammError::FillValueDrop);
    }
    let internal_value_after = mul_div(base_x_after, WAD, cv_after)?;
    if internal_value_after > PRICE_BAND_UPPER_WAD || internal_value_after < PRICE_BAND_LOWER_WAD {
        return Err(FlammError::FillPriceBand);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: u64) -> U256 {
        U256::from(v)
    }

    #[test]
    fn constants() {
        assert_eq!(CR_CEILING_WAD, u(22) * WAD / u(10));
        assert_eq!(MAX_CONCESSION_PPM, PPM + u(10_000));
        assert_eq!(PRICE_BAND_UPPER_WAD, WAD * u(2));
        assert_eq!(PRICE_BAND_LOWER_WAD, WAD / u(2));
    }

    #[test]
    fn frame_signs_and_refusals() {
        // A book with no debt and a credit: D <= 0, no anchor, unquotable.
        let book = LevBook {
            rs: WAD,
            is: U256::ZERO,
            rv: WAD,
            iv: U256::ZERO,
            reservation_price_wad: WAD,
        };
        let pool =
            PoolContext { liquid_loan_asset: u(2) * WAD, price_wad: WAD, ..Default::default() };
        let f = frame(&pool, &book).unwrap();
        assert_eq!(f.cv, u(2) * WAD);
        assert!(f.d.is_negative());
        assert_eq!(f.x_anchor, U256::ZERO);
        let ctx = LeverContext { pool, up: true, amount_in: WAD, ..Default::default() };
        assert_eq!(quote(&ctx, &book), Err(FlammError::FrameUnquotable));

        // Checked overflow on the legs.
        let wide = LevBook { rs: U256::MAX, is: u(1), ..book };
        assert_eq!(frame(&pool, &wide), Err(FlammError::PanicArithmetic));
        let wide = LevBook { reservation_price_wad: U256::MAX, rv: U256::MAX, ..book };
        assert_eq!(frame(&pool, &wide), Err(FlammError::MulDivOverflow));
    }

    #[test]
    fn band_rejects_empty_post_state() {
        assert_eq!(
            assert_anchor_and_band(U256::ZERO, U256::ZERO, U256::ZERO),
            Err(FlammError::FillValueDrop)
        );
        assert_eq!(
            assert_anchor_and_band(U256::MAX, u(2) * WAD, WAD),
            Err(FlammError::FillValueDrop)
        );
        assert_eq!(assert_anchor_and_band(U256::ZERO, u(2) * WAD, WAD), Ok(()));
    }
}
