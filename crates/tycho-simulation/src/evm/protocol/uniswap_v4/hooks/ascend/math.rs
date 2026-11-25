//! The arithmetic of the Ascend hook and of its math library
//! (0x1717591399ef91d9ae434008d7084ea7cc755af3), ported operation by operation. The hook and the
//! library have no proxy and no `delegatecall`, so the constants below are fixed for good.
//!
//! The library function `0x15821c64` is pure: the tests replay its outputs, read with `eth_call`
//! on Elysium at block 8660000. Besides the exits its arithmetic implies, the function returns no
//! fill in two cases that only those `eth_call`s show: when the pool's price is past the curve
//! end on the far side from the fill price, and when the tokens left up to the curve end round
//! down to 0 (a spot at the end, or close enough to it that the whole tokens up to it floor to
//! 0). `size_fill` reproduces both.
//!
//! The module raises two kinds of error:
//!
//! - [`SimulationError::InvalidInput`] ([`refuse`]) for what a revert of the hook means: this one
//!   swap cannot be served, as when a checked operation of the contract overflows.
//! - [`SimulationError::FatalError`] ([`broken_invariant`], [`sub_invariant`]) for an invariant of
//!   the arithmetic that the inputs keep whatever the swap: a failure is a fault of the model.
//!
//! Faults of the pool itself (a curve the contract could not have produced, an EMA of 0) are
//! refused earlier, when the state is decoded; the errors of the shared Uniswap functions (a price
//! of 0, an overflow of the pool's own price range) are faults of the pool and pass through as
//! they are.

use alloy::primitives::{uint, U256};
use tycho_common::simulation::errors::SimulationError;

use super::{super::launchpad::INT128_MAX, Observation};
use crate::evm::protocol::utils::{
    solidity_math::{mul_div, mul_div_rounding_up},
    uniswap::{
        sqrt_price_math::{get_amount0_delta, get_amount1_delta, Q96},
        tick_math::get_sqrt_ratio_at_tick,
    },
};

/// The width of every launch position in ticks (the hook's `0xa3b4a42c()`, 200000).
const CURVE_WIDTH_TICKS: i32 = 200_000;
/// One tick spacing: a pool opens, and the fill price is bounded, this far outside the position.
const TICK_SPACING: i32 = 200;
/// The tokens every launch seeds the position with: 1e27.
const LAUNCH_SUPPLY: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
/// The EMA moves in steps of at most this many seconds.
const EMA_STEP_SECONDS: u64 = 600;
/// The EMA takes at most this many steps.
const EMA_MAX_STEPS: u32 = 8;
/// The discount decays with this constant: `d1 = ceil(d * 43200 / (dt + 43200))`.
const DECAY_SECONDS: u64 = 43_200;
/// A fill needs the spot within `BAND_NUMERATOR / BAND_DENOMINATOR` of the EMA, and sizes `Qmax`
/// from the same ratio.
const BAND_NUMERATOR: u64 = 500;
/// The denominator of the band ratio, see [`BAND_NUMERATOR`].
const BAND_DENOMINATOR: u64 = 20_000;
/// A bid window lasts this many seconds.
pub(super) const WINDOW_SECONDS: u64 = 600;

/// One in the discount's fixed point (2^32): `d` is a share of it.
const Q32: U256 = uint!(4_294_967_296_U256);

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
    /// The tick the launch position starts at (`PoolData.startTick`), which with the orientation
    /// decides every other field.
    pub(super) start_tick: i32,
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
        // `sqrtAt` grows with the tick and `upper_tick` is above `lower_tick`.
        let width = sub_invariant(upper, lower, "the position's width")?;
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
                start_tick,
                liquidity,
                curve_end: lower,
                far_end: upper,
                start_side_bound,
                clamp: (start_side_bound, upper),
            }
        } else {
            Self {
                token_is_currency0,
                start_tick,
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
/// where v4 floors it, so it can come out one unit above v4's. Changing the shared function would
/// change every Uniswap v3 and v4 quote, so this copy stays local. A test pins a case where the
/// two differ (and that this one matches a big-integer reference), so that fixing the shared
/// function fails it and calls for this copy's removal. The rounded-up case is the shared
/// function's.
///
/// Its only caller passes the curve end and a price at or past it, both above 0, so a price of 0
/// is a fault of the model.
pub(super) fn amount0_delta_down(
    a: U256,
    b: U256,
    liquidity: u128,
) -> Result<U256, SimulationError> {
    let (lower, upper) = if a > b { (b, a) } else { (a, b) };
    if lower.is_zero() {
        return Err(broken_invariant("a price of 0"));
    }
    let spread = sub_invariant(upper, lower, "the price spread")?;
    Ok(mul_div_floor(U256::from(liquidity) << 96usize, spread, upper)? / lower)
}

/// `floor(x * BAND_NUMERATOR / BAND_DENOMINATOR)`, the band width at `x`, which is at most `x`.
fn band(x: U256) -> Result<U256, SimulationError> {
    mul_div_floor(x, U256::from(BAND_NUMERATOR), U256::from(BAND_DENOMINATOR))
}

/// A pool's square-root price and the tick it is in, which the hook reads together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PoolPrice {
    pub(super) sqrt_price: U256,
    pub(super) tick: i32,
}

/// The observation `beforeSwap` runs on every swap that reaches the hook, at the pool's current
/// `price`, at the block timestamp `now`.
///
/// Nothing changes when `dt == 0`. A `now` before the last observation refuses: the hook's
/// subtraction would revert.
pub(super) fn observe(
    curve: &Curve,
    observation: &Observation,
    price: PoolPrice,
    now: u64,
) -> Result<Observation, SimulationError> {
    let PoolPrice { sqrt_price: spot, tick } = price;
    if observation.is_ahead_of(now) {
        return Err(refuse(format!(
            "the block timestamp {now} is before the last observation {}",
            observation.updated_at
        )));
    }
    let dt = now - observation.updated_at;
    if dt == 0 {
        return Ok(*observation);
    }
    let old_ema = observation.ema;
    // (1) the spot, clamped to the range around the curve.
    let spot = spot.clamp(curve.clamp.0, curve.clamp.1);
    // (2) up to `EMA_MAX_STEPS` steps of at most `EMA_STEP_SECONDS`, each moving the EMA a share
    // `step / (step + EMA_STEP_SECONDS)` of the way to the spot, rounded down.
    let mut ema = old_ema;
    let mut remaining = dt;
    for _ in 0..EMA_MAX_STEPS {
        if remaining == 0 {
            break;
        }
        let step = remaining.min(EMA_STEP_SECONDS);
        let distance = spot.abs_diff(ema);
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
    // `decayed = ceil(d * DECAY_SECONDS / (dt + DECAY_SECONDS))` is at most `d`, which is below
    // 2^32.
    let kept = sub_invariant(Q32, decayed, "the decayed discount")?;
    let anchored = if curve.token_is_currency0 {
        mul_div_floor(kept, old_ema, ema)?
    } else {
        mul_div_floor(kept, ema, old_ema)?
    };
    let discount = Q32.saturating_sub(anchored);
    let discount = u32::try_from(discount).map_err(|_overflow| {
        refuse(format!("the re-anchored discount {discount} does not fit uint32"))
    })?;
    Ok(Observation { ema, updated_at: now, tick, discount })
}

/// The fill price X after the observation: `max(floor(ema * (2^32 - d) / 2^32), bound)` when the
/// token is `currency0`, `min(ceil(ema * 2^32 / (2^32 - d)), bound)` otherwise, where `bound` is
/// the start-side bound.
pub(super) fn fill_price(curve: &Curve, ema: U256, discount: u32) -> Result<U256, SimulationError> {
    // A `uint32` discount is below 2^32, so `kept` is at least 1.
    let kept = Q32 - U256::from(discount);
    if curve.token_is_currency0 {
        Ok(mul_div_floor(ema, kept, Q32)?.max(curve.start_side_bound))
    } else {
        Ok(mul_div_ceil(ema, Q32, kept)?.min(curve.start_side_bound))
    }
}

/// A bid fill: the tokens it takes, the quote it pays (before its fee) and the discount after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fill {
    pub(super) tokens_in: U256,
    pub(super) quote_out: U256,
    pub(super) discount: u32,
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

/// A fault of the model: an invariant of the hook's arithmetic that the caller's inputs keep does
/// not hold, which no swap causes.
pub(super) fn broken_invariant(what: &str) -> SimulationError {
    SimulationError::FatalError(format!("ascend hook: {what}"))
}

/// `a - b` where the arithmetic keeps `a >= b` whatever the swap: an underflow is a fault of the
/// model (a `FatalError`), not a refused input, unlike [`sub`]. `what` names the quantity.
pub(super) fn sub_invariant(a: U256, b: U256, what: &str) -> Result<U256, SimulationError> {
    a.checked_sub(b)
        .ok_or_else(|| broken_invariant(&format!("{what}: {a} - {b} underflows")))
}

/// The tokens of a fill whose quote is capped at `cap`, inverting [`end_price_and_quote`].
///
/// `cap` stays below the quote of the whole band at `x0` (`size_fill` takes it from `qmax`, which
/// is at most `liquidity * x0 / 2^96 / 40`), so the price move and the inverse price below
/// are positive; a `cap` that breaks that is a fault of the model.
pub(super) fn tokens_for_capped_quote(
    curve: &Curve,
    x0: U256,
    cap: U256,
) -> Result<U256, SimulationError> {
    let liquidity = U256::from(curve.liquidity);
    let scaled_liquidity = liquidity << 96usize;
    if curve.token_is_currency0 {
        let d_sqrt = (cap << 96usize) / liquidity;
        let scaled = mul_div_floor(scaled_liquidity, d_sqrt, x0)?;
        let remaining_price = sub_invariant(x0, d_sqrt, "the capped fill's price move")?;
        if remaining_price.is_zero() {
            return Err(broken_invariant("the capped fill reaches a price of 0"));
        }
        Ok(scaled / remaining_price)
    } else {
        let capped_inverse =
            sub_invariant(scaled_liquidity.div_ceil(x0), cap, "the capped fill's inverse price")?;
        if capped_inverse.is_zero() {
            return Err(broken_invariant("the capped fill reaches an infinite price"));
        }
        let capped_price = scaled_liquidity / capped_inverse;
        mul_div_floor(
            liquidity,
            sub_invariant(capped_price, x0, "the capped fill's price move")?,
            Q96,
        )
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
    if spot < sub_invariant(ema, ema_band, "the band below the EMA")? ||
        spot > add(ema, ema_band, "the band above the EMA")?
    {
        return Ok(None);
    }

    // `to_fill_price`: the tokens that bring the spot to the fill price (plus one), which the
    // swap's input must exceed; `x0`: the price the fill starts at; `qmax`: the most quote one
    // fill pays out. The tokens that remain are the input less `to_fill_price`.
    let (x0, to_fill_price, qmax) = if tokens_is_0 {
        let t = if spot > x {
            add(
                get_amount0_delta(x, spot, curve.liquidity, true)?,
                U256::ONE,
                "the tokens to the fill price",
            )?
        } else {
            U256::ZERO
        };
        let x0 = x.min(spot);
        let qmax = mul_div_floor(liquidity, band(x0)?, Q96)?;
        (x0, t, qmax)
    } else {
        let t = if x > spot {
            add(
                get_amount1_delta(spot, x, curve.liquidity, true)?,
                U256::ONE,
                "the tokens to the fill price",
            )?
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
    if cap <= U256::ONE {
        return Ok(None);
    }
    let cap = cap - U256::ONE;

    // The tokens are at most what the curve takes between `x0` and its end. With the spot on the
    // far side of the curve end the library returns no fill, in both orientations (read with
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
    // Whole tokens of 0 up to the curve end leave nothing to fill: the spot is at the end, or
    // close enough that the tokens floor to 0. The library returns no fill at the end itself
    // (`eth_call` at block 8660000, token0 spot equal to the end, fill price above it), where the
    // arithmetic below would round the fill's end price to the price it starts at or above it.
    if tokens_in.is_zero() {
        return Ok(None);
    }

    // The price the fill ends at for `tokens_in`, and the quote it pays out.
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
    let discount = u32::try_from(discount).map_err(|_overflow| {
        refuse(format!("the fill's discount {discount} does not fit uint32"))
    })?;
    Ok(Some(Fill { tokens_in, quote_out: add(quote, U256::ONE, "the fill's quote")?, discount }))
}
