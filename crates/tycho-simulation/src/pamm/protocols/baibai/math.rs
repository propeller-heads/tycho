//! The CurveBook's pricing of one side of a pair, in the contract's checked `uint256` arithmetic
//! and operation order. Quantities are base-token atomic units and costs quote-token atomic
//! units.

use alloy::primitives::{U256, U512};
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::{u256_num::u256_to_f64, utils::solidity_math::mul_div};

/// Most knots a side can hold.
pub(super) const MAX_KNOTS: usize = 36;

const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);

pub(super) fn invalid(reason: impl std::fmt::Display) -> SimulationError {
    SimulationError::InvalidInput(format!("BaiBai: {reason}"), None)
}

fn add(a: U256, b: U256) -> Result<U256, SimulationError> {
    a.checked_add(b)
        .ok_or_else(|| invalid("uint256 addition overflow"))
}

fn mul(a: U256, b: U256) -> Result<U256, SimulationError> {
    a.checked_mul(b)
        .ok_or_else(|| invalid("uint256 multiplication overflow"))
}

/// One knot of a side's cumulative impact curve as `CurveBook.knot` returns it: `q` in units of
/// the pair's `qUnit` and `extra` in units of the book's `cUnit`, each at most 48 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Knot {
    pub q: u64,
    pub extra: u64,
}

/// One side of a pair as `CurveBook.pair` and `CurveBook.knot` report it. The ask side sells
/// base, the bid side buys it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Side {
    pub spread_bps: i16,
    pub depth_bps: u16,
    /// Base quantity this side has already filled since the curve was last shaped.
    pub filled: U256,
    pub knots: Vec<Knot>,
}

/// A side scaled into atomic units, ready to price.
#[derive(Debug)]
pub(super) struct Curve {
    /// Quote atoms per 10^18 base atoms after the side's spread.
    price: U256,
    pub(super) filled: U256,
    /// The cursor the side fills up to: the last knot scaled by the side's depth.
    max: U256,
    /// Cumulative `(quantity, extra cost)` at each knot, in atomic units.
    knots: Vec<(U256, U256)>,
    asks: bool,
}

impl Curve {
    /// Scales `side` by the pair's `mid`, `q_unit` and the book's `c_unit`. Fails for parameters
    /// the contract would never store: more than [`MAX_KNOTS`] knots, a spread of -100% or less,
    /// a depth above 100%, or knots whose quantity does not increase or whose cost decreases.
    pub(super) fn new(
        side: &Side,
        mid: U256,
        q_unit: U256,
        c_unit: U256,
        asks: bool,
    ) -> Result<Self, SimulationError> {
        if side.knots.len() > MAX_KNOTS || side.spread_bps <= -10_000 || side.depth_bps > 10_000 {
            return Err(invalid("invalid curve parameters"));
        }
        let mut knots: Vec<(U256, U256)> = Vec::with_capacity(side.knots.len());
        for knot in &side.knots {
            let q = mul(U256::from(knot.q), q_unit)?;
            let c = mul(U256::from(knot.extra), c_unit)?;
            let (prev_q, prev_c) = knots
                .last()
                .copied()
                .unwrap_or((U256::ZERO, U256::ZERO));
            if q <= prev_q || c < prev_c {
                return Err(invalid("nonmonotonic knots"));
            }
            knots.push((q, c));
        }
        let last_q = knots
            .last()
            .map_or(U256::ZERO, |knot| knot.0);
        let max = mul(last_q, U256::from(side.depth_bps))? / BPS;
        let scale = U256::from((10_000 + i32::from(side.spread_bps)) as u32);
        Ok(Curve { price: mul(mid, scale)? / BPS, filled: side.filled, max, knots, asks })
    }

    fn extra(prev: (U256, U256), end: (U256, U256), x: U256) -> Result<U256, SimulationError> {
        add(prev.1, mul(end.1 - prev.1, x - prev.0)?.div_ceil(end.0 - prev.0))
    }

    fn extra_at(&self, x: U256) -> Result<U256, SimulationError> {
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if x <= end.0 {
                return Self::extra(prev, end, x);
            }
            prev = end;
        }
        Ok(prev.1)
    }

    fn cost(
        &self,
        prev: (U256, U256),
        end: (U256, U256),
        from: U256,
        to: U256,
    ) -> Result<U256, SimulationError> {
        // Solidity adds before it subtracts, and the addition is checked.
        Ok(add(mul(self.price, to - from)?.div_ceil(WAD), Self::extra(prev, end, to)?)? -
            Self::extra(prev, end, from)?)
    }

    /// The input this side absorbs before it stops paying out: the whole remaining depth on the
    /// ask side, and on the bid side the depth up to the first segment whose marginal proceeds
    /// are not positive, since past it more input buys less output.
    pub(super) fn capacity(&self) -> Result<U256, SimulationError> {
        if self.filled >= self.max {
            return Ok(U256::ZERO);
        }
        let mut total = U256::ZERO;
        let mut q = self.filled;
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if end.0 > q {
                if !self.asks &&
                    U512::from(self.price) * U512::from(end.0 - prev.0) <=
                        U512::from(end.1 - prev.1) * U512::from(WAD)
                {
                    break;
                }
                let stop = end.0.min(self.max);
                total = if self.asks {
                    add(total, self.cost(prev, end, q, stop)?)?
                } else {
                    stop - self.filled
                };
                q = stop;
                if q == self.max {
                    break;
                }
            }
            prev = end;
        }
        Ok(total)
    }

    /// An upper bound on [`Self::quote`]'s output for `input` that never decreases as the input
    /// grows, within [`Self::capacity`]. Bid proceeds floor the mid term and ceil the extra term
    /// separately, so the exact output can dip by one atom from one input to the next; flooring
    /// their unrounded difference bounds every smaller input too.
    pub(super) fn output_bound(&self, input: U256) -> Result<U256, SimulationError> {
        if self.asks {
            return Ok(self.quote(input)?.0);
        }
        let q = add(self.filled, input)?;
        let mid = mul(self.price, input)?;
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if q <= end.0 {
                let span = end.0 - prev.0;
                let remainder = mul(end.1 - prev.1, q - prev.0)? % span;
                let carry = remainder != U256::ZERO &&
                    U512::from(mid % WAD) * U512::from(span) >=
                        U512::from(remainder) * U512::from(WAD);
                let extra = Self::extra(prev, end, q)? - self.extra_at(self.filled)?;
                return Ok((mid / WAD + U256::from(carry)).saturating_sub(extra));
            }
            prev = end;
        }
        Err(invalid("beyond bid depth"))
    }

    /// How many knots lie below `cursor`: the knots a walk ending there has passed.
    pub(super) fn knots_below(&self, cursor: U256) -> u64 {
        self.knots
            .iter()
            .filter(|(q, _)| *q < cursor)
            .count() as u64
    }

    /// The gross output for `input` and the side's cursor after it. A zero output is an input
    /// below the curve's atomic precision, which the contract refuses.
    pub(super) fn quote(&self, input: U256) -> Result<(U256, U256), SimulationError> {
        if input == U256::ZERO {
            return Ok((U256::ZERO, self.filled));
        }
        if !self.asks {
            let q = add(self.filled, input)?;
            if q > self.max {
                return Err(invalid("beyond bid depth"));
            }
            let mid = mul(self.price, input)? / WAD;
            let extra = self.extra_at(q)? - self.extra_at(self.filled)?;
            return Ok((mid.saturating_sub(extra), q));
        }
        if self.filled >= self.max {
            return Err(invalid("beyond ask depth"));
        }
        let mut q = self.filled;
        let mut remaining = input;
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if end.0 > q {
                let stop = end.0.min(self.max);
                let cost = self.cost(prev, end, q, stop)?;
                if remaining < cost {
                    // The only full-width (OpenZeppelin `Math.mulDiv`) step in the contract.
                    let dq = mul_div(remaining, stop - q, cost)?;
                    if dq == U256::ZERO {
                        return Ok((U256::ZERO, self.filled));
                    }
                    q += dq;
                    remaining = U256::ZERO;
                } else {
                    remaining -= cost;
                    q = stop;
                }
                if remaining == U256::ZERO || q == self.max {
                    break;
                }
            }
            prev = end;
        }
        if remaining != U256::ZERO {
            return Err(invalid("beyond ask depth"));
        }
        Ok((q - self.filled, q))
    }

    /// The marginal price at the cursor, in output atoms per input atom: what the next atom of
    /// input would fetch. Fails when no depth remains or the next segment pays nothing.
    pub(super) fn marginal_price(&self) -> Result<f64, SimulationError> {
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if end.0 > self.filled && self.filled < self.max {
                let slope = u256_to_f64(end.1 - prev.1)? / u256_to_f64(end.0 - prev.0)?;
                let mid = u256_to_f64(self.price)? / 1e18;
                let price = if self.asks { 1.0 / (mid + slope) } else { mid - slope };
                if price.is_finite() && price > 0.0 {
                    return Ok(price);
                }
                break;
            }
            prev = end;
        }
        Err(invalid("no marginal liquidity"))
    }
}
