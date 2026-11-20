//! The arithmetic of the Ascend hook and of its math library
//! (0x1717591399ef91d9ae434008d7084ea7cc755af3), ported operation by operation. The hook and the
//! library have no proxy and no `delegatecall`, so the constants below are fixed for good.
//!
//! The library function `0x15821c64` is pure: the tests replay its outputs, read with `eth_call`
//! on Elysium at block 8660000.
//!
//! Every error here is [`SimulationError::InvalidInput`] and means the same as a revert of the
//! hook: this one swap cannot be served, as when a checked operation of the contract overflows.
//! Faults of the pool itself (a curve the contract could not have produced, an EMA of 0) are
//! refused earlier, when the state is decoded.

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use super::{Observation, UINT32_LIMIT};
use crate::evm::protocol::utils::{
    solidity_math::{mul_div, mul_div_rounding_up},
    uniswap::{
        sqrt_price_math::{get_amount0_delta, get_amount1_delta},
        tick_math::get_sqrt_ratio_at_tick,
    },
};

/// The width of every launch position in ticks (the hook's `0xa3b4a42c()`, 200000).
const CURVE_WIDTH_TICKS: i32 = 200_000;
/// One tick spacing: a pool opens, and the fill price is bounded, this far outside the position.
const TICK_SPACING: i32 = 200;
/// The tokens every launch seeds the position with: 1e27.
pub(super) const LAUNCH_SUPPLY: U256 = U256::from_limbs([0x9fd0_803c_e800_0000, 0x033b_2e3c, 0, 0]);
/// The EMA moves in steps of at most this many seconds.
const EMA_STEP_SECONDS: u64 = 600;
/// The EMA takes at most this many steps.
const EMA_MAX_STEPS: u32 = 8;
/// The discount decays with this constant: `d1 = ceil(d * 43200 / (dt + 43200))`.
const DECAY_SECONDS: u64 = 43_200;
/// A fill needs the spot within `500 / 20000` of the EMA, and sizes `Qmax` from the same ratio.
const BAND_NUMERATOR: u64 = 500;
const BAND_DENOMINATOR: u64 = 20_000;
/// A bid window lasts this many seconds.
pub(super) const WINDOW_SECONDS: u64 = 600;
/// `type(int128).max`, the largest input the hook treats as a fill candidate and the largest
/// amount a v4 `BalanceDelta` carries.
pub(super) const INT128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX >> 1, 0, 0]);

/// One in the discount's fixed point: `d` is a share of it.
const Q32: U256 = U256::from_limbs([UINT32_LIMIT, 0, 0, 0]);
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);

/// Refuses the swap the contract's arithmetic would revert on, or no router can settle.
pub(super) fn refuse(reason: impl Into<String>) -> SimulationError {
    SimulationError::InvalidInput(format!("ascend hook: {}", reason.into()), None)
}

/// `floor(a * b / denominator)`; a result beyond 256 bits or a zero denominator refuses.
pub(super) fn mul_div_floor(a: U256, b: U256, denominator: U256) -> Result<U256, SimulationError> {
    mul_div(a, b, denominator).map_err(|e| refuse(format!("{a} * {b} / {denominator}: {e}")))
}

/// `ceil(a * b / denominator)`.
pub(super) fn mul_div_ceil(a: U256, b: U256, denominator: U256) -> Result<U256, SimulationError> {
    mul_div_rounding_up(a, b, denominator)
        .map_err(|e| refuse(format!("{a} * {b} / {denominator} rounded up: {e}")))
}

/// `a - b`, refusing an underflow; `what` names the quantity.
pub(super) fn sub(a: U256, b: U256, what: &str) -> Result<U256, SimulationError> {
    a.checked_sub(b)
        .ok_or_else(|| refuse(format!("{what}: {a} - {b} underflows")))
}

/// `a + b`, refusing an overflow; `what` names the quantity.
pub(super) fn add(a: U256, b: U256, what: &str) -> Result<U256, SimulationError> {
    a.checked_add(b)
        .ok_or_else(|| refuse(format!("{what}: {a} + {b} overflows")))
}

/// `sqrtAt(tick)`; a tick outside the price range refuses.
fn sqrt_ratio_at(tick: i32) -> Result<U256, SimulationError> {
    get_sqrt_ratio_at_tick(tick).map_err(|e| refuse(format!("the tick {tick}: {e}")))
}

/// The launch curve of a pool: its position, its liquidity and the prices that bound a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Curve {
    pub(super) token_is_currency0: bool,
    /// The liquidity of the single position: the launch supply over the position's width.
    pub(super) liquidity: u128,
    /// Where selling the token ends: `sqrtAt(start)` when the token is `currency0`,
    /// `sqrtAt(-start)` otherwise. A sell past it enters the empty range.
    pub(super) curve_end: U256,
    /// Where buying the token ends: the far end of the position.
    pub(super) far_end: U256,
    /// `sqrtAt(start - 200)` (`sqrtAt(-start + 200)` mirrored): the bound of the fill price on
    /// the start side and of the observation's clamp.
    pub(super) start_side_bound: U256,
    /// The range the observation clamps the spot to.
    pub(super) clamp: (U256, U256),
}

impl Curve {
    /// The curve of a pool whose launched token is `currency0` (or not) and whose start tick is
    /// `start_tick`. Refuses a tick whose price is out of range and a liquidity that is 0 or
    /// beyond `uint128`: no launch the hook accepts has either.
    pub(super) fn try_new(
        token_is_currency0: bool,
        start_tick: i32,
    ) -> Result<Self, SimulationError> {
        let (lower_tick, upper_tick) = if token_is_currency0 {
            (start_tick, start_tick + CURVE_WIDTH_TICKS)
        } else {
            (-(start_tick + CURVE_WIDTH_TICKS), -start_tick)
        };
        let lower = sqrt_ratio_at(lower_tick)?;
        let upper = sqrt_ratio_at(upper_tick)?;
        let width = sub(upper, lower, "the position's width")?;
        let liquidity = if token_is_currency0 {
            let product = mul_div_floor(lower, upper, Q96)?;
            mul_div_floor(LAUNCH_SUPPLY, product, width)?
        } else {
            mul_div_floor(LAUNCH_SUPPLY, Q96, width)?
        };
        let liquidity = u128::try_from(liquidity)
            .ok()
            .filter(|liquidity| *liquidity > 0)
            .ok_or_else(|| {
                refuse(format!(
                    "the launch liquidity {liquidity} of start tick {start_tick} is 0 or does not \
                     fit uint128"
                ))
            })?;
        let start_side_bound = sqrt_ratio_at(if token_is_currency0 {
            start_tick - TICK_SPACING
        } else {
            -start_tick + TICK_SPACING
        })?;
        Ok(if token_is_currency0 {
            Self {
                token_is_currency0,
                liquidity,
                curve_end: lower,
                far_end: upper,
                start_side_bound,
                clamp: (start_side_bound, upper),
            }
        } else {
            Self {
                token_is_currency0,
                liquidity,
                curve_end: upper,
                far_end: lower,
                start_side_bound,
                clamp: (lower, start_side_bound),
            }
        })
    }
}

/// `getAmount0Delta` rounded down, as v4's `SqrtPriceMath` computes it: `mulDiv(L << 96, b - a, b)
/// / a`, with the prices in either order.
///
/// The shared `get_amount0_delta(.., round_up = false)` divides the rounded-up product instead,
/// where v4 floors it, so it can come out one unit above v4's; this one is local until that is
/// fixed in its own change. The rounded-up case is the shared function's.
pub(super) fn amount0_delta_down(
    a: U256,
    b: U256,
    liquidity: u128,
) -> Result<U256, SimulationError> {
    let (lower, upper) = if a > b { (b, a) } else { (a, b) };
    if lower.is_zero() {
        return Err(refuse("a price of 0"));
    }
    let spread = sub(upper, lower, "the price spread")?;
    Ok(mul_div_floor(U256::from(liquidity) << 96usize, spread, upper)? / lower)
}

/// `floor(x * 500 / 20000)`, the band width at `x`.
fn band(x: U256) -> Result<U256, SimulationError> {
    mul_div_floor(x, U256::from(BAND_NUMERATOR), U256::from(BAND_DENOMINATOR))
}

/// The observation `beforeSwap` runs on every swap that reaches the hook, at the pool's current
/// `spot` and `tick`, at the block timestamp `now`.
///
/// Nothing changes when `dt == 0`. A `now` before the last observation refuses: the hook's
/// subtraction would revert.
pub(super) fn observe(
    curve: &Curve,
    observation: &Observation,
    spot: U256,
    tick: i32,
    now: u64,
) -> Result<Observation, SimulationError> {
    let dt = now
        .checked_sub(observation.updated_at)
        .ok_or_else(|| {
            refuse(format!(
                "the block timestamp {now} is before the last observation {}",
                observation.updated_at
            ))
        })?;
    if dt == 0 {
        return Ok(*observation);
    }
    let old_ema = observation.ema;
    // (1) the spot, clamped to the range around the curve.
    let spot = spot.clamp(curve.clamp.0, curve.clamp.1);
    // (2) up to 8 steps of at most 600 s, each moving the EMA a share `step / (step + 600)` of
    // the way to the spot, rounded down.
    let mut ema = old_ema;
    let mut remaining = dt;
    for _ in 0..EMA_MAX_STEPS {
        if remaining == 0 {
            break;
        }
        let step = remaining.min(EMA_STEP_SECONDS);
        let distance = if spot > ema { spot - ema } else { ema - spot };
        let moved = mul_div_floor(distance, U256::from(step), U256::from(step + EMA_STEP_SECONDS))?;
        ema = if spot > ema { ema + moved } else { ema - moved };
        remaining -= step;
    }
    // (3) the discount decays over the full dt, rounded up.
    let decayed = mul_div_ceil(
        U256::from(observation.discount),
        U256::from(DECAY_SECONDS),
        U256::from(dt) + U256::from(DECAY_SECONDS),
    )?;
    // (4) re-anchor the bid price to the new EMA, flooring `d` at 0 when it would go negative.
    let kept = sub(Q32, decayed, "the decayed discount")?;
    let anchored = if curve.token_is_currency0 {
        mul_div_floor(kept, old_ema, ema)?
    } else {
        mul_div_floor(kept, ema, old_ema)?
    };
    let discount = if anchored >= Q32 { U256::ZERO } else { Q32 - anchored };
    let discount = super::to_uint32(discount).ok_or_else(|| {
        refuse(format!("the re-anchored discount {discount} does not fit uint32"))
    })?;
    Ok(Observation { ema, updated_at: now, tick, discount })
}

/// The fill price X after the observation: `max(floor(ema * (2^32 - d) / 2^32), bound)` when the
/// token is `currency0`, `min(ceil(ema * 2^32 / (2^32 - d)), bound)` otherwise, where `bound` is
/// the start-side bound.
pub(super) fn fill_price(curve: &Curve, ema: U256, discount: u64) -> Result<U256, SimulationError> {
    let kept = sub(Q32, U256::from(discount), "the discount")?;
    if curve.token_is_currency0 {
        Ok(mul_div_floor(ema, kept, Q32)?.max(curve.start_side_bound))
    } else {
        if kept.is_zero() {
            return Err(refuse("the discount is 2^32"));
        }
        Ok(mul_div_ceil(ema, Q32, kept)?.min(curve.start_side_bound))
    }
}

/// A bid fill: the tokens it takes, the quote it pays (before its fee) and the discount after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fill {
    pub(super) tokens_in: U256,
    pub(super) quote_out: U256,
    pub(super) discount: u64,
}

/// What the library's fill sizing reads besides the curve: the pool's spot, the fill price and
/// the EMA, the bid and the swap's input.
pub(super) struct FillRequest {
    pub(super) spot: U256,
    pub(super) fill_price: U256,
    pub(super) ema: U256,
    pub(super) amount_in: U256,
    pub(super) inventory: U256,
    pub(super) max_clip: U256,
    pub(super) filled: U256,
}

/// The price a fill of `tokens` ends at, from the price `x0` it starts at, and the quote `q` it
/// pays out before the `+ 1` the library adds.
fn end_price_and_quote(
    curve: &Curve,
    x0: U256,
    tokens: U256,
) -> Result<(U256, U256), SimulationError> {
    let liquidity = U256::from(curve.liquidity);
    let scaled_liquidity = liquidity << 96usize;
    if curve.token_is_currency0 {
        let new_denominator = add(scaled_liquidity / x0, tokens, "the fill's price denominator")?;
        let end_price = scaled_liquidity.div_ceil(new_denominator);
        let quote = mul_div_floor(liquidity, sub(x0, end_price, "the fill's price move")?, Q96)?;
        Ok((end_price, quote))
    } else {
        let end_price = add(x0, (tokens << 96usize) / liquidity, "the fill's end price")?;
        let moved = sub(end_price, x0, "the fill's price move")?;
        let quote = mul_div_floor(scaled_liquidity, moved, end_price)? / x0;
        Ok((end_price, quote))
    }
}

/// The tokens of a fill whose quote is capped at `cap`, inverting [`end_price_and_quote`].
fn tokens_for_capped_quote(curve: &Curve, x0: U256, cap: U256) -> Result<U256, SimulationError> {
    let liquidity = U256::from(curve.liquidity);
    let scaled_liquidity = liquidity << 96usize;
    if curve.token_is_currency0 {
        let d_sqrt = (cap << 96usize) / liquidity;
        let scaled = mul_div_floor(scaled_liquidity, d_sqrt, x0)?;
        Ok(scaled / sub(x0, d_sqrt, "the capped fill's price move")?)
    } else {
        let capped_inverse =
            sub(scaled_liquidity.div_ceil(x0), cap, "the capped fill's inverse price")?;
        if capped_inverse.is_zero() {
            return Err(refuse("the capped fill reaches an infinite price"));
        }
        let capped_price = scaled_liquidity / capped_inverse;
        mul_div_floor(liquidity, sub(capped_price, x0, "the capped fill's price move")?, Q96)
    }
}

/// Library function `0x15821c64`: the fill a sell of `request.amount_in` gets from the bid, or
/// `None` when the library returns no fill.
pub(super) fn size_fill(
    curve: &Curve,
    request: &FillRequest,
) -> Result<Option<Fill>, SimulationError> {
    let FillRequest { spot, fill_price: x, ema, amount_in, inventory, max_clip, filled } = *request;
    let liquidity = U256::from(curve.liquidity);
    let tokens_is_0 = curve.token_is_currency0;

    // No fill when X is at or past the curve end (before the A <= T check).
    let past_curve_end = if tokens_is_0 { x <= curve.curve_end } else { x >= curve.curve_end };
    if past_curve_end {
        return Ok(None);
    }
    // No fill when the spot is outside the band around the EMA.
    let ema_band = band(ema)?;
    if spot < sub(ema, ema_band, "the band below the EMA")? ||
        spot > add(ema, ema_band, "the band above the EMA")?
    {
        return Ok(None);
    }

    // T: the token amount that brings the spot to X (plus one), E = A - T the rest, and Qmax.
    let (x0, to_fill_price, qmax) = if tokens_is_0 {
        let t = if spot > x {
            get_amount0_delta(x, spot, curve.liquidity, true)
                .map_err(|e| refuse(format!("the tokens to the fill price: {e}")))? +
                U256::from(1)
        } else {
            U256::ZERO
        };
        let x0 = x.min(spot);
        let qmax = mul_div_floor(liquidity, band(x0)?, Q96)?;
        (x0, t, qmax)
    } else {
        let t = if x > spot {
            get_amount1_delta(spot, x, curve.liquidity, true)
                .map_err(|e| refuse(format!("the tokens to the fill price: {e}")))? +
                U256::from(1)
        } else {
            U256::ZERO
        };
        let x0 = x.max(spot);
        let band_x0 = band(x0)?;
        let x_high = add(x0, band_x0, "the band above the fill price")?;
        let inner = mul_div_floor(liquidity << 96usize, band_x0, x_high)?;
        (x0, t, inner / x0)
    };
    if amount_in <= to_fill_price {
        return Ok(None);
    }
    let mut tokens_in = amount_in - to_fill_price;

    let per_window = qmax.min(max_clip);
    if filled >= per_window {
        return Ok(None);
    }
    let cap = (per_window - filled)
        .min(inventory)
        .min(INT128_MAX);
    if cap <= U256::from(1) {
        return Ok(None);
    }
    let cap = cap - U256::from(1);

    // E = min(E, V): V is what the curve takes between X0 and its end. With the spot on the far
    // side of the curve end the library returns no fill, in both orientations (read with
    // `eth_call` at block 8660000 for a spot 1 unit and 0.1% beyond the end).
    let to_curve_end = if tokens_is_0 {
        if x0 < curve.curve_end {
            return Ok(None);
        }
        amount0_delta_down(curve.curve_end, x0, curve.liquidity)?
    } else {
        if x0 > curve.curve_end {
            return Ok(None);
        }
        mul_div_floor(liquidity, curve.curve_end - x0, Q96)?
    };
    tokens_in = tokens_in.min(to_curve_end);

    // The end price Xe for `tokens_in` and the quote q it pays out.
    let (mut end_price, mut quote) = end_price_and_quote(curve, x0, tokens_in)?;
    if quote > cap {
        tokens_in = tokens_for_capped_quote(curve, x0, cap)?;
        (end_price, quote) = end_price_and_quote(curve, x0, tokens_in)?;
    }
    if quote.is_zero() {
        return Ok(None);
    }
    // The new discount: how far below the EMA the fill ended.
    let ratio = if tokens_is_0 {
        mul_div_floor(end_price, Q32, ema)?
    } else {
        mul_div_floor(ema, Q32, end_price)?
    };
    let discount = sub(Q32, ratio, "the new discount")?;
    let discount = super::to_uint32(discount)
        .ok_or_else(|| refuse(format!("the fill's discount {discount} does not fit uint32")))?;
    Ok(Some(Fill { tokens_in, quote_out: quote + U256::from(1), discount }))
}
