// Copyright (c) 2026 Everlong Labs Limited

//! `FLAMMSwapLib` (c104 @ `80abd43`, `src/core/flamm/FLAMMSwapLib.sol`): one exact-input swap
//! between poolAsset and a loan asset. Core prices the context, sizes the ceiling (the sell's gate
//! room, notional cap and Router funding, re-planned on the fill's own consumption), asks the hook
//! for the fee (floor rejects, cap clips) and the fill in the numeraire, converts at the edge
//! (floor a payout, ceil a claim), validates it against the user's limit, the ceiling, the buy's
//! notional cap and the loan asset's band around the checked cross, then settles through the Router
//! and re-asserts the gate. Port of `swap.go`.

use alloy::primitives::{Address, U256};

use super::{
    context::{PoolContext, SwapContext},
    deps::{LeverageHook, Router, SwapHook},
    error::FlammError,
    gate::{self, Pool},
    hook,
    math::{checked_add, checked_div, checked_mul, div_ceil, mul_div, mul_div_up, WAD},
    state::{priced, FlammState, FEATURE_SWAP_BUY, FEATURE_SWAP_SELL},
};

/// `FLAMMSwapLib.MAX_FUNDING_PASSES` (`FLAMMSwapLib.sol:22`).
pub const SWAP_MAX_FUNDING_PASSES: u64 = 4;

/// `FLAMMSwapLib.Plan` (`FLAMMSwapLib.sol:24`) plus the book the priced fill materialised, which an
/// execution commits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapPlan {
    pub idx: u8,
    pub pool_asset_in: bool,
    pub price_wad: U256,
    pub scale: U256,
    pub cross_wad: U256,
    pub spot_wad: U256,
    pub fee_wad: U256,
    /// Native units of the output token.
    pub ceiling: U256,
    pub used_native: U256,
    pub net_native: U256,
    pub gross_native: U256,
    pub sctx: SwapContext,
    /// The hook's answer, loan leg in the numeraire.
    pub fill: hook::FillResult,
    pub book: hook::Book,
    /// `_maxInForGrossCap` solves of every `previewExactIn` the plan ran: non-zero exactly where
    /// the cap clipped the input. Not part of the on-chain plan; the plan tests assert it.
    pub cap_evals: u64,
    /// Funding passes the sell's plan ran (zero for a buy). Not part of the on-chain plan; the
    /// plan tests assert it.
    pub passes: u64,
}

/// What `FLAMM.swap` returns and its `Swap` event carries (`FLAMMStore.sol:131`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapResult {
    pub pool_asset_in: bool,
    pub amount_in_used: U256,
    pub amount_out: U256,
    /// `grossNative - netNative`, native units of the output token.
    pub fee_out: U256,
    pub fee_wad: U256,
    pub spot_after_wad: U256,
    /// The plan's loan-leg cross the band was checked at (not part of the return).
    pub price_wad: U256,
    /// `_maxInForGrossCap` solves of the whole transaction, non-zero exactly where the cap
    /// clipped the input (not part of the return).
    pub cap_evals: u64,
    /// The plan's funding passes (not part of the return).
    pub passes: u64,
}

/// `FLAMMSwapLib.toN18` (`FLAMMSwapLib.sol:41`): `mulDiv(native * scale, q, WAD)`, the product
/// checked.
pub fn to_n18(native: U256, scale: U256, q: U256) -> Result<U256, FlammError> {
    let n = checked_mul(native, scale)?;
    mul_div(n, q, WAD)
}

/// `FLAMMSwapLib.fromN18Floor` (`FLAMMSwapLib.sol:45`): `mulDiv(v, WAD, q) / scale`, a payout
/// floored twice.
pub fn from_n18_floor(v: U256, scale: U256, q: U256) -> Result<U256, FlammError> {
    let x = mul_div(v, WAD, q)?;
    checked_div(x, scale)
}

/// `FLAMMSwapLib.fromN18Ceil` (`FLAMMSwapLib.sol:49`): `ceilDiv(mulDiv(v, WAD, q, Up), scale)`, a
/// claim ceiled twice.
pub fn from_n18_ceil(v: U256, scale: U256, q: U256) -> Result<U256, FlammError> {
    let x = mul_div_up(v, WAD, q)?;
    div_ceil(x, scale)
}

impl<H: SwapHook, L: LeverageHook, R: Router> FlammState<H, L, R> {
    /// `FLAMMSwapLib.preview` (`FLAMMSwapLib.sol:54`), `FLAMM.previewSwap`'s body: loan asset 0, no
    /// slippage limit. The view's read guard never binds outside a flow.
    pub fn preview_swap(
        &self,
        pool_asset_in: bool,
        amount_in: U256,
        now: u64,
    ) -> Result<SwapPlan, FlammError> {
        if amount_in.is_zero() {
            return Err(FlammError::InvalidAmount);
        }
        let mut pool = self.pool.clone();
        let b = priced(&self.feed, &self.router, &mut pool, now)?;
        let p = self.swap_plan(&pool, &b, 0, pool_asset_in, amount_in, now)?;
        self.swap_validate(&pool, &p, U256::ZERO)?;
        Ok(p)
    }

    /// `FLAMMSwapLib.execute` (`FLAMMSwapLib.sol:65`) as one transaction: the post-state is
    /// returned and the receiver is never written. The recipient check (`to == address(0)`) is
    /// the caller's business and not modelled. The committed fee and fill (`executeFeeWad`,
    /// `executeExactIn`) are the same pure functions of the same storage and context the plan
    /// priced, so `FeeMismatch` and `FillMismatch` cannot fire and the plan's book is committed
    /// as is.
    pub fn execute_swap(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        min_amount_out: U256,
        deadline: u64,
        now: u64,
    ) -> Result<(SwapResult, Self), FlammError> {
        if now > deadline {
            return Err(FlammError::Expired);
        }
        if amount_in.is_zero() {
            return Err(FlammError::InvalidAmount);
        }
        let (idx, pool_asset_in) = self.pair(token_in, token_out)?;
        let mut post = self.clone();
        let b = priced(&post.feed, &post.router, &mut post.pool, now)?;
        let p = post.swap_plan(&post.pool, &b, idx, pool_asset_in, amount_in, now)?;
        post.swap_validate(&post.pool, &p, min_amount_out)?;
        // validate refused a zero amountInUsed: executeExactIn commits (EverlongHook.sol
        // executeExactIn).
        post.hooks
            .swap
            .port_mut()?
            .commit(&p.book);
        if pool_asset_in {
            post.router.settle_sell_legs(
                &mut post.pool,
                idx,
                p.price_wad,
                p.used_native,
                p.net_native,
                now,
            )?;
        } else {
            post.router
                .settle_buy_legs(&mut post.pool, idx, p.used_native, p.net_native, now)?;
        }
        post.router.end_transaction();
        // A frame of this timestamp, not state.
        post.pool.price_wad.clear();
        post.pool.cross_wad.clear();
        // executeExactIn re-runs the plan's last fill on the same context (FLAMMSwapLib.sol:87),
        // bisection included.
        let r = SwapResult {
            pool_asset_in,
            amount_in_used: p.used_native,
            amount_out: p.net_native,
            // fromN18Floor is monotone and feeOut <= grossOut, so grossNative >= netNative.
            fee_out: p.gross_native - p.net_native,
            fee_wad: p.fee_wad,
            spot_after_wad: p.fill.spot_after_wad,
            price_wad: p.price_wad,
            cap_evals: p.cap_evals + p.fill.cap_evals,
            passes: p.passes,
        };
        Ok((r, post))
    }

    /// The gate a swap passes before anything is priced, and the checked price it reads there
    /// (`FLAMMSwapLib.sol:134-141`): the pool's pause bit and the loan asset's peg band guard the
    /// pool-asset-in direction only, the feature bitmap guards both directions with its own mask,
    /// and `FLAMMStore.price` is read between them. Returns the plan's `(p0, priceTs)`.
    pub fn swap_open(
        &self,
        idx: usize,
        pool_asset_in: bool,
        now: u64,
    ) -> Result<(U256, u64), FlammError> {
        if pool_asset_in {
            if self.paused {
                return Err(FlammError::Paused);
            }
            self.feature(FEATURE_SWAP_SELL)?;
        } else {
            self.feature(FEATURE_SWAP_BUY)?;
        }
        let (p0, price_ts) = self.price(now)?;
        if pool_asset_in && !self.peg_ok(idx, now)? {
            return Err(FlammError::PegBroken);
        }
        Ok((p0, price_ts))
    }

    /// `FLAMMSwapLib._plan` (`FLAMMSwapLib.sol:125`) over the priced book `b` (`pool` carries its
    /// price frame).
    pub fn swap_plan(
        &self,
        pool: &Pool,
        b: &gate::Book,
        idx: u8,
        pool_asset_in: bool,
        amount_in: U256,
        now: u64,
    ) -> Result<SwapPlan, FlammError> {
        let i = idx as usize;
        let (Some(cfg), Some(leg)) = (pool.loans.get(i), b.legs.get(i)) else {
            return Err(FlammError::PanicIndex);
        };
        let (p0, price_ts) = self.swap_open(i, pool_asset_in, now)?;
        let mut p = SwapPlan {
            idx,
            pool_asset_in,
            scale: cfg.scale,
            price_wad: leg.price_wad,
            cross_wad: leg.cross_wad,
            ..SwapPlan::default()
        };
        if p.price_wad.is_zero() || p.cross_wad.is_zero() {
            return Err(FlammError::PriceUnchecked);
        }
        let ctx = gate::context(b, p0, price_ts, self.share_supply)?;
        p.spot_wad = self.hooks.swap.port()?.spot()?;
        if !pool_asset_in {
            p.ceiling = gate::gross(b)?;
            self.swap_fill(pool, &mut p, &ctx, amount_in)?;
            return Ok(p);
        }
        // Sell ceiling: the gate room, the notional cap and the Router's funding of the loan leg,
        // which counts the incoming poolAsset as collateral; a partial fill is re-planned
        // on its own consumption.
        let u = gate::exposure_pw(b)?;
        let mut room = gate::room_native(pool, b, i, u)?;
        if !cfg.max_swap_notional.is_zero() && cfg.max_swap_notional < room {
            room = cfg.max_swap_notional;
        }
        let liquid = leg.liquid;
        let mut collateral_in = amount_in;
        for _ in 0..SWAP_MAX_FUNDING_PASSES {
            p.passes += 1;
            let funding = self.swap_funding(b, idx, collateral_in, p.price_wad, now)?;
            let funding = checked_add(liquid, funding)?;
            p.ceiling = room.min(funding); // `room < funding ? room : funding`
            if p.ceiling.is_zero() {
                return Err(FlammError::RoomExhausted);
            }
            self.swap_fill(pool, &mut p, &ctx, amount_in)?;
            if p.used_native >= collateral_in {
                return Ok(p);
            }
            collateral_in = p.used_native;
            let need = p.net_native.saturating_sub(liquid);
            let funding = self.swap_funding(b, idx, collateral_in, p.price_wad, now)?;
            if need <= funding {
                return Ok(p);
            }
        }
        Err(FlammError::OutputAboveCeiling)
    }

    /// `router.fundingCeiling(pool, idx, b.physical + collateralIn, priceWad)`
    /// (`FLAMMSwapLib.sol:164`, `:171`), the sum checked.
    fn swap_funding(
        &self,
        b: &gate::Book,
        idx: u8,
        collateral_in: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<U256, FlammError> {
        let coll = checked_add(b.physical, collateral_in)?;
        self.router
            .funding_ceiling(idx, coll, price_wad, now)
    }

    /// `FLAMMSwapLib._fill` (`FLAMMSwapLib.sol:176`): the swap hook's fee, floored by the larger of
    /// the pool's and the asset's floor (a rejection) and capped by the pool's cap (a clip),
    /// then the swap hook's fill on the clipped fee converted to native units: a sell's payout
    /// floored, a buy's claim ceiled (and never above `amountIn`). The fee and invariant roles
    /// are one hook.
    fn swap_fill(
        &self,
        pool: &Pool,
        p: &mut SwapPlan,
        ctx: &PoolContext,
        amount_in: U256,
    ) -> Result<(), FlammError> {
        let hook = self.hooks.swap.port()?;
        let cfg = pool
            .loans
            .get(p.idx as usize)
            .ok_or(FlammError::PanicIndex)?;
        p.sctx = SwapContext {
            pool: *ctx,
            pool_asset_in: p.pool_asset_in,
            loan_index: p.idx,
            cross_wad: p.cross_wad,
            fee_floor_wad: cfg.fee_floor_wad,
            ..SwapContext::default()
        };
        if p.pool_asset_in {
            p.sctx.amount_in = amount_in;
            p.sctx.max_amount_out = to_n18(p.ceiling, p.scale, p.cross_wad)?;
        } else {
            p.sctx.amount_in = to_n18(amount_in, p.scale, p.cross_wad)?;
            p.sctx.max_amount_out = p.ceiling;
        }
        p.fee_wad = self.bounded_fee(hook.preview_fee_wad(&p.sctx)?, p.sctx.fee_floor_wad)?;
        let (fill, book) = hook.fill(&p.sctx, p.fee_wad)?;
        p.fill = fill;
        p.book = book;
        p.cap_evals += p.fill.cap_evals;
        let f = &p.fill;
        if p.pool_asset_in {
            p.used_native = f.amount_in_used;
            p.gross_native = from_n18_floor(f.gross_out, p.scale, p.cross_wad)?;
            p.net_native = if f.fee_out > f.gross_out {
                U256::ZERO
            } else {
                from_n18_floor(f.gross_out - f.fee_out, p.scale, p.cross_wad)?
            };
            return Ok(());
        }
        p.used_native = if f.amount_in_used.is_zero() {
            U256::ZERO
        } else {
            from_n18_ceil(f.amount_in_used, p.scale, p.cross_wad)?
        };
        if p.used_native > amount_in {
            return Err(FlammError::FillInvalid);
        }
        p.gross_native = f.gross_out;
        p.net_native = if f.fee_out > f.gross_out { U256::ZERO } else { f.gross_out - f.fee_out };
        Ok(())
    }

    /// The fee bounds of `FLAMMSwapLib._fill` (`FLAMMSwapLib.sol:188`, `:195-196`): a fee below
    /// the larger of the pool's floor and the asset's is refused (`FeeOutOfBounds`), one above the
    /// pool's cap is clipped to it.
    pub fn bounded_fee(&self, fee_wad: U256, asset_floor_wad: U256) -> Result<U256, FlammError> {
        let floor_wad =
            if asset_floor_wad > self.fee_floor_wad { asset_floor_wad } else { self.fee_floor_wad };
        if fee_wad < floor_wad {
            return Err(FlammError::FeeOutOfBounds);
        }
        Ok(if fee_wad > self.fee_cap_wad { self.fee_cap_wad } else { fee_wad })
    }

    /// The fee a swap in one direction fills at right now (`FLAMMSwapLib._fill`'s
    /// `previewFeeWad` on the priced context, `FLAMMSwapLib.sol:187`, bounded by
    /// [`Self::bounded_fee`], `:188`, `:195-196`): the hook's fee law reads the direction and the
    /// book, never the size, so this is the `feeWad` every `previewSwap` of that direction
    /// returns. Runs the same gate and the same reads a plan does up to the fee
    /// ([`Self::swap_open`], the Router positions and the feed at `now`), so a direction the pool
    /// would refuse outright has no fee here rather than the law's number.
    ///
    /// The gate bits are not the whole of that for a sell. `_plan` reads the gate room before it
    /// looks at `amountIn` at all (`FLAMMGateLib.roomNative`, `FLAMMSwapLib.sol:159`), and the
    /// sell ceiling is `min(room, funding)`, so a zero room is `RoomExhausted` (`:165-166`) for
    /// every size the pool is ever asked for, no matter how the funding moves. A pool standing
    /// at its exposure cap is in that state, which is an operational one for a levered pool, and
    /// it has no sell fee. The funding itself is NOT checked here: it grows with the collateral
    /// the sell brings in, so a funding that refuses one size can admit a larger one, which is a
    /// property of the size and belongs to the fill. The buy direction never reads the room
    /// (its ceiling is the book, `:151`) and keeps its own fee throughout.
    pub fn swap_fee_wad(&self, pool_asset_in: bool, now: u64) -> Result<U256, FlammError> {
        let cfg = self
            .pool
            .loans
            .first()
            .ok_or(FlammError::PanicIndex)?;
        let mut pool = self.pool.clone();
        let b = priced(&self.feed, &self.router, &mut pool, now)?;
        let (p0, price_ts) = self.swap_open(0, pool_asset_in, now)?;
        if pool_asset_in && gate::room_native(&pool, &b, 0, gate::exposure_pw(&b)?)?.is_zero() {
            return Err(FlammError::RoomExhausted);
        }
        let ctx = gate::context(&b, p0, price_ts, self.share_supply)?;
        let sctx = SwapContext {
            pool: ctx,
            pool_asset_in,
            loan_index: 0,
            cross_wad: WAD,
            fee_floor_wad: cfg.fee_floor_wad,
            ..SwapContext::default()
        };
        let fee = self
            .hooks
            .swap
            .port()?
            .preview_fee_wad(&sctx)?;
        self.bounded_fee(fee, cfg.fee_floor_wad)
    }

    /// `FLAMMSwapLib._validate` (`FLAMMSwapLib.sol:211`): a well-formed non-zero fill, the user's
    /// limit, the ceiling, the buy's notional cap, and the NET inside the loan asset's band
    /// around the checked cross (both legs valued in L18: a sell's `net*scale` against
    /// `used*priceWad`, a buy's `net*priceWad` against `used*scale`).
    pub fn swap_validate(
        &self,
        pool: &Pool,
        p: &SwapPlan,
        min_amount_out: U256,
    ) -> Result<(), FlammError> {
        let f = &p.fill;
        if f.amount_in_used.is_zero() ||
            f.amount_in_used > p.sctx.amount_in ||
            f.fee_out > f.gross_out
        {
            return Err(FlammError::FillInvalid);
        }
        let net = p.net_native;
        if p.used_native.is_zero() || net.is_zero() {
            return Err(FlammError::FillInvalid);
        }
        if net < min_amount_out {
            return Err(FlammError::Slippage);
        }
        if net > p.ceiling {
            return Err(FlammError::OutputAboveCeiling);
        }
        let cfg = pool
            .loans
            .get(p.idx as usize)
            .ok_or(FlammError::PanicIndex)?;
        let (out_value, in_value) = if p.pool_asset_in {
            (checked_mul(net, p.scale)?, checked_mul(p.used_native, p.price_wad)?)
        } else {
            if !cfg.max_swap_notional.is_zero() && p.used_native > cfg.max_swap_notional {
                return Err(FlammError::NotionalCap);
            }
            (checked_mul(net, p.price_wad)?, checked_mul(p.used_native, p.scale)?)
        };
        swap_band(out_value, in_value, cfg.swap_price_band_wad)
    }
}

/// The shared band test `out < mulDiv(in, WAD - band, WAD) || out > mulDiv(in, WAD + band, WAD)`
/// (`FLAMMSwapLib.sol:233`), evaluated left to right with `||`'s short circuit.
pub fn swap_band(out_value: U256, in_value: U256, band_wad: U256) -> Result<(), FlammError> {
    if band_wad > WAD {
        return Err(FlammError::PanicArithmetic);
    }
    let lo = mul_div(in_value, WAD - band_wad, WAD)?;
    if out_value < lo {
        return Err(FlammError::PriceBand);
    }
    let hi = mul_div(in_value, checked_add(WAD, band_wad)?, WAD)?;
    if out_value > hi {
        return Err(FlammError::PriceBand);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(x: u64) -> U256 {
        U256::from(x)
    }

    const SCALE: U256 = U256::from_limbs([1_000_000_000_000, 0, 0, 0]); // USDC: 10**(18-6)

    #[test]
    fn numeraire_edge_rounds_as_named() {
        // q == WAD: the numeraire is L18, so the edge is the scale alone.
        assert_eq!(to_n18(w(11_301_759), SCALE, WAD).unwrap(), w(11_301_759) * SCALE);
        assert_eq!(
            from_n18_floor(w(11_301_759) * SCALE + SCALE - w(1), SCALE, WAD).unwrap(),
            w(11_301_759)
        );
        assert_eq!(from_n18_ceil(w(11_301_759) * SCALE + w(1), SCALE, WAD).unwrap(), w(11_301_760));
        assert_eq!(from_n18_ceil(w(11_301_759) * SCALE, SCALE, WAD).unwrap(), w(11_301_759));
        assert_eq!(from_n18_ceil(U256::ZERO, SCALE, WAD).unwrap(), U256::ZERO);
        // q != WAD: mulDiv(v, WAD, q) floors, then the scale division floors again; the ceil
        // variant ceils both.
        let q = w(3) * WAD / w(2); // 1.5
        assert_eq!(to_n18(w(2), w(1), q).unwrap(), w(3));
        assert_eq!(from_n18_floor(w(4), w(1), q).unwrap(), w(2)); // 4 / 1.5 = 2.67
        assert_eq!(from_n18_ceil(w(4), w(1), q).unwrap(), w(3));
        assert_eq!(from_n18_floor(w(4), w(2), q).unwrap(), w(1)); // floor(2.67) / 2 = 1
        assert_eq!(from_n18_ceil(w(4), w(2), q).unwrap(), w(2)); // ceil(ceil(2.67) / 2) = 2
                                                                 // Reverts: the checked product, a zero q, a zero scale.
        assert_eq!(to_n18(U256::MAX, w(2), WAD), Err(FlammError::PanicArithmetic));
        assert_eq!(to_n18(w(1), w(1), U256::ZERO), Ok(U256::ZERO));
        assert_eq!(from_n18_floor(w(1), w(1), U256::ZERO), Err(FlammError::PanicDivZero));
        assert_eq!(from_n18_floor(w(1), U256::ZERO, WAD), Err(FlammError::PanicDivZero));
        assert_eq!(from_n18_ceil(w(1), U256::ZERO, WAD), Err(FlammError::PanicDivZero));
        assert_eq!(from_n18_ceil(U256::ZERO, U256::ZERO, WAD), Ok(U256::ZERO));
    }

    #[test]
    fn band_is_inclusive_on_both_edges() {
        let band = w(80_000_000_000_000_000); // 8%
        let in_value = w(1_000_000) * WAD;
        let lo = in_value * (WAD - band) / WAD;
        let hi = in_value * (WAD + band) / WAD;
        assert!(swap_band(lo, in_value, band).is_ok());
        assert!(swap_band(hi, in_value, band).is_ok());
        assert_eq!(swap_band(lo - w(1), in_value, band), Err(FlammError::PriceBand));
        assert_eq!(swap_band(hi + w(1), in_value, band), Err(FlammError::PriceBand));
        // A zero band is an exact-price test.
        assert!(swap_band(in_value, in_value, U256::ZERO).is_ok());
        assert_eq!(swap_band(in_value + w(1), in_value, U256::ZERO), Err(FlammError::PriceBand));
        // A band above WAD is a checked underflow.
        assert_eq!(swap_band(w(1), w(1), WAD + w(1)), Err(FlammError::PanicArithmetic));
        // The low edge fires before the high mulDiv is evaluated: an overflowing high side is
        // masked.
        assert_eq!(swap_band(U256::ZERO, U256::MAX, band), Err(FlammError::PriceBand));
        assert_eq!(swap_band(U256::MAX, U256::MAX, band), Err(FlammError::MulDivOverflow));
    }
}
