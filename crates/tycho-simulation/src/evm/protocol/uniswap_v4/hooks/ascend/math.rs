//! The arithmetic of the Ascend hook and of its math library
//! (0x1717591399ef91d9ae434008d7084ea7cc755af3), ported operation by operation. Each function names
//! the spec fact (`S..`) it reproduces; the hook and the library are not upgradeable (S3), so the
//! constants below are fixed.

use alloy::primitives::U256;
use tycho_common::simulation::errors::SimulationError;

use super::Observation;
use crate::evm::protocol::utils::{
    solidity_math::{mul_div, mul_div_rounding_up},
    uniswap::tick_math::get_sqrt_ratio_at_tick,
};

/// The width of every launch position in ticks (`0xa3b4a42c()` = 200000, S24).
const CURVE_WIDTH_TICKS: i32 = 200_000;
/// One tick spacing: a pool opens, and the fill price is bounded, this far outside the position
/// (S14, S15, S20).
const TICK_SPACING: i32 = 200;
/// The tokens every launch seeds the position with: 1e27 (S7, S25).
pub(super) const LAUNCH_SUPPLY: U256 = U256::from_limbs([0x9fd0_803c_e800_0000, 0x033b_2e3c, 0, 0]);
/// The EMA moves in steps of at most this many seconds (S14).
const EMA_STEP_SECONDS: u64 = 600;
/// The EMA takes at most this many steps (S14).
const EMA_MAX_STEPS: u32 = 8;
/// The discount decays with this constant: `d1 = ceil(d * 43200 / (dt + 43200))` (S14).
const DECAY_SECONDS: u64 = 43_200;
/// A fill needs the spot within `500 / 20000` of the EMA, and sizes `Qmax` from the same ratio
/// (S16).
const BAND_NUMERATOR: u64 = 500;
const BAND_DENOMINATOR: u64 = 20_000;
/// A bid window lasts this many seconds (S15).
pub(super) const WINDOW_SECONDS: u64 = 600;
/// `type(int128).max`, the largest input the hook treats as a fill candidate and the largest
/// amount a v4 `BalanceDelta` carries.
pub(super) const INT128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX >> 1, 0, 0]);

const Q32: U256 = U256::from_limbs([1 << 32, 0, 0, 0]);
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);

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

fn checked_sub(a: U256, b: U256) -> Result<U256, SimulationError> {
    a.checked_sub(b)
        .ok_or_else(|| refuse(format!("{a} - {b} underflows")))
}

fn checked_add(a: U256, b: U256) -> Result<U256, SimulationError> {
    a.checked_add(b)
        .ok_or_else(|| refuse(format!("{a} + {b} overflows")))
}

/// The launch curve of a pool: its position, its liquidity and the prices that bound a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Curve {
    pub(super) token_is_currency0: bool,
    /// The liquidity of the single position, from S7's formula.
    pub(super) liquidity: U256,
    /// Where selling the token ends: `sqrtAt(start)` when the token is `currency0`,
    /// `sqrtAt(-start)` otherwise. A sell past it enters the empty range.
    pub(super) curve_end: U256,
    /// Where buying the token ends: the far end of the position.
    pub(super) far_end: U256,
    /// `sqrtAt(start - 200)` (`sqrtAt(-start + 200)` mirrored): the bound of the fill price X on
    /// the start side (S15) and of the observation's clamp.
    pub(super) start_side_bound: U256,
    /// The range the observation clamps the spot to (S14).
    pub(super) clamp: (U256, U256),
}

impl Curve {
    /// The curve of a pool whose launched token is `currency0` (or not) and whose start tick is
    /// `start_tick`. Refuses a tick whose price is out of range, and a liquidity beyond `uint128`.
    pub(super) fn new(token_is_currency0: bool, start_tick: i32) -> Result<Self, SimulationError> {
        let (lower_tick, upper_tick) = if token_is_currency0 {
            (start_tick, start_tick + CURVE_WIDTH_TICKS)
        } else {
            (-(start_tick + CURVE_WIDTH_TICKS), -start_tick)
        };
        let lower = get_sqrt_ratio_at_tick(lower_tick)?;
        let upper = get_sqrt_ratio_at_tick(upper_tick)?;
        let width = checked_sub(upper, lower)?;
        let liquidity = if token_is_currency0 {
            let product = mul_div_floor(lower, upper, Q96)?;
            mul_div_floor(LAUNCH_SUPPLY, product, width)?
        } else {
            mul_div_floor(LAUNCH_SUPPLY, Q96, width)?
        };
        if liquidity > U256::from(u128::MAX) {
            return Err(refuse(format!("the liquidity {liquidity} does not fit uint128")));
        }
        let start_side_bound = get_sqrt_ratio_at_tick(if token_is_currency0 {
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

/// `getAmount0Delta` between two prices with `liquidity`, in v4's `SqrtPriceMath` rounding.
pub(super) fn amount0_delta(
    a: U256,
    b: U256,
    liquidity: U256,
    round_up: bool,
) -> Result<U256, SimulationError> {
    let (lower, upper) = if a > b { (b, a) } else { (a, b) };
    if lower.is_zero() {
        return Err(refuse("a price of 0"));
    }
    let numerator = liquidity << 96usize;
    let spread = upper - lower;
    if round_up {
        let step = mul_div_ceil(numerator, spread, upper)?;
        Ok(step.div_ceil(lower))
    } else {
        Ok(mul_div_floor(numerator, spread, upper)? / lower)
    }
}

/// `getAmount1Delta` rounded up: `ceil(L * (b - a) / 2^96)`.
fn amount1_delta_up(a: U256, b: U256, liquidity: U256) -> Result<U256, SimulationError> {
    let (lower, upper) = if a > b { (b, a) } else { (a, b) };
    mul_div_ceil(liquidity, upper - lower, Q96)
}

/// `floor(x * 500 / 20000)`, the band width at `x`.
fn band(x: U256) -> Result<U256, SimulationError> {
    mul_div_floor(x, U256::from(BAND_NUMERATOR), U256::from(BAND_DENOMINATOR))
}

/// The observation `beforeSwap` runs on every swap that reaches the hook (S14), at the pool's
/// current `spot` and `tick`, at the block timestamp `now`.
///
/// Nothing changes when `dt == 0`. A `now` before `emaUpdatedAt` refuses: the hook's subtraction
/// would revert.
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
    if old_ema.is_zero() {
        return Err(refuse("the EMA is 0"));
    }
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
    // (4) re-anchor the bid price to the new EMA, flooring `d` at 0 when it would go negative
    // (S14, S18).
    let kept = checked_sub(Q32, decayed)?;
    let anchored = if curve.token_is_currency0 {
        mul_div_floor(kept, old_ema, ema)?
    } else {
        mul_div_floor(kept, ema, old_ema)?
    };
    let discount = if anchored >= Q32 { U256::ZERO } else { Q32 - anchored };
    let discount = u64::try_from(discount)
        .ok()
        .filter(|d| *d < (1 << 32))
        .ok_or_else(|| refuse("the re-anchored discount does not fit uint32"))?;
    Ok(Observation { ema, updated_at: now, tick, discount })
}

/// The fill price X (S15) after the observation: `max(floor(ema * (2^32 - d) / 2^32), bound)`
/// when the token is `currency0`, `min(ceil(ema * 2^32 / (2^32 - d)), bound)` otherwise, where
/// `bound` is the start-side bound.
pub(super) fn fill_price(curve: &Curve, ema: U256, discount: u64) -> Result<U256, SimulationError> {
    let kept = checked_sub(Q32, U256::from(discount))?;
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

/// Library function `0x15821c64` (S16): the fill a sell of `request.amount_in` gets from the bid,
/// or `None` when the library returns no fill.
///
/// Refuses the one state S16 does not settle: a spot on the wrong side of the curve end while X
/// is past it, where the library's tokens-to-the-curve-end `V` is not defined by the spec.
pub(super) fn size_fill(
    curve: &Curve,
    request: &FillRequest,
) -> Result<Option<Fill>, SimulationError> {
    let FillRequest { spot, fill_price: x, ema, amount_in, inventory, max_clip, filled } = *request;
    let liquidity = curve.liquidity;
    let scaled_liquidity = liquidity << 96usize;
    let tokens_is_0 = curve.token_is_currency0;

    // No fill when X is at or past the curve end (before the A <= T check).
    let past_curve_end = if tokens_is_0 { x <= curve.curve_end } else { x >= curve.curve_end };
    if past_curve_end {
        return Ok(None);
    }
    // No fill when the spot is outside the band around the EMA.
    let ema_band = band(ema)?;
    if spot < checked_sub(ema, ema_band)? || spot > checked_add(ema, ema_band)? {
        return Ok(None);
    }

    // T: the token amount that brings the spot to X (plus one), E = A - T the rest, and Qmax.
    let (x0, to_fill_price, qmax) = if tokens_is_0 {
        let t = if spot > x {
            amount0_delta(x, spot, liquidity, true)? + U256::from(1)
        } else {
            U256::ZERO
        };
        let x0 = x.min(spot);
        let qmax = mul_div_floor(liquidity, band(x0)?, Q96)?;
        (x0, t, qmax)
    } else {
        let t = if x > spot {
            amount1_delta_up(spot, x, liquidity)? + U256::from(1)
        } else {
            U256::ZERO
        };
        let x0 = x.max(spot);
        let x_high = checked_add(x0, band(x0)?)?;
        let inner = mul_div_floor(scaled_liquidity, x_high - x0, x_high)?;
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

    // E = min(E, V): V is what the curve takes between X0 and its end.
    let to_curve_end = if tokens_is_0 {
        if x0 < curve.curve_end {
            return Err(refuse("a fill with the spot below the curve end"));
        }
        amount0_delta(curve.curve_end, x0, liquidity, false)?
    } else {
        if x0 > curve.curve_end {
            return Err(refuse("a fill with the spot above the curve end"));
        }
        mul_div_floor(liquidity, curve.curve_end - x0, Q96)?
    };
    tokens_in = tokens_in.min(to_curve_end);

    // The end price Xe for `tokens_in` and the quote q it pays out.
    let end_price_and_quote = |tokens: U256| -> Result<(U256, U256), SimulationError> {
        if tokens_is_0 {
            let new_denominator = checked_add(scaled_liquidity / x0, tokens)?;
            let xe = scaled_liquidity.div_ceil(new_denominator);
            let q = mul_div_floor(liquidity, checked_sub(x0, xe)?, Q96)?;
            Ok((xe, q))
        } else {
            let xe = checked_add(x0, (tokens << 96usize) / liquidity)?;
            let q = mul_div_floor(scaled_liquidity, checked_sub(xe, x0)?, xe)? / x0;
            Ok((xe, q))
        }
    };
    let (mut end_price, mut quote) = end_price_and_quote(tokens_in)?;
    if quote > cap {
        tokens_in = if tokens_is_0 {
            let d_sqrt = (cap << 96usize) / liquidity;
            let scaled = mul_div_floor(scaled_liquidity, d_sqrt, x0)?;
            scaled / checked_sub(x0, d_sqrt)?
        } else {
            let capped_inverse = checked_sub(scaled_liquidity.div_ceil(x0), cap)?;
            if capped_inverse.is_zero() {
                return Err(refuse("the capped fill reaches an infinite price"));
            }
            let capped_price = scaled_liquidity / capped_inverse;
            mul_div_floor(liquidity, checked_sub(capped_price, x0)?, Q96)?
        };
        (end_price, quote) = end_price_and_quote(tokens_in)?;
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
    let discount = checked_sub(Q32, ratio)?;
    let discount = u64::try_from(discount)
        .map_err(|_overflow| refuse("the fill's discount does not fit uint32"))?;
    Ok(Some(Fill { tokens_in, quote_out: quote + U256::from(1), discount }))
}
