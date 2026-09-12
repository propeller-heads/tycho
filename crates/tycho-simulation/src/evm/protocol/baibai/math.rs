//! Integer arithmetic for CurveBook v3. Quantities and costs are token atomic units.
use alloy::primitives::{U256, U512};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::utils::solidity_math::mul_div;

pub(super) fn invalid(reason: &str) -> SimulationError {
    SimulationError::InvalidInput(format!("BaiBai: {reason}"), None)
}

pub(super) fn add(a: U256, b: U256) -> Result<U256, SimulationError> {
    a.checked_add(b)
        .ok_or_else(|| invalid("uint256 addition overflow"))
}

fn mul(a: U256, b: U256) -> Result<U256, SimulationError> {
    a.checked_mul(b)
        .ok_or_else(|| invalid("uint256 multiplication overflow"))
}

fn ceil_div(a: U256, b: U256) -> U256 {
    a / b + U256::from(a % b != U256::ZERO)
}

pub(super) fn big(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

pub(super) fn uint(value: &BigUint) -> Result<U256, SimulationError> {
    if value.bits() > 256 {
        return Err(invalid("amount exceeds uint256"));
    }
    Ok(U256::from_be_slice(&value.to_bytes_be()))
}

#[derive(Debug)]
pub(super) struct Side {
    pub price: U256,
    pub filled: U256,
    pub max: U256,
    pub knots: Vec<(U256, U256)>,
    pub asks: bool,
}

impl Side {
    fn extra(prev: (U256, U256), end: (U256, U256), x: U256) -> Result<U256, SimulationError> {
        add(prev.1, ceil_div(mul(end.1 - prev.1, x - prev.0)?, end.0 - prev.0))
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
        // Preserve Solidity's evaluation order, including its checked addition before subtraction.
        Ok(add(
            ceil_div(mul(self.price, to - from)?, U256::from(10u64.pow(18))),
            Self::extra(prev, end, to)?,
        )? - Self::extra(prev, end, from)?)
    }

    pub fn capacity(&self) -> Result<U256, SimulationError> {
        if self.filled >= self.max {
            return Ok(U256::ZERO);
        }
        let mut total = U256::ZERO;
        let mut q = self.filled;
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if end.0 > q {
                // Advertise only the initial interval with positive marginal proceeds.
                // Later decreasing segments spend more input for less output.
                if !self.asks &&
                    U512::from(self.price) * U512::from(end.0 - prev.0) <=
                        U512::from(end.1 - prev.1) * U512::from(10u64.pow(18))
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

    /// Monotonic custody bound on the interval returned by capacity(). Bid proceeds
    /// floor the mid term and ceil the extra term separately, so their exact output
    /// can dip by one atom. Flooring their unrounded difference bounds every prefix.
    pub fn output_bound(&self, input: U256) -> Result<U256, SimulationError> {
        if self.asks {
            return Ok(self.quote(input)?.0);
        }
        let q = add(self.filled, input)?;
        let mid = mul(self.price, input)?;
        let wad = U256::from(10u64.pow(18));
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if q <= end.0 {
                let span = end.0 - prev.0;
                let remainder = mul(end.1 - prev.1, q - prev.0)? % span;
                let carry = remainder != U256::ZERO &&
                    U512::from(mid % wad) * U512::from(span) >=
                        U512::from(remainder) * U512::from(wad);
                let extra = Self::extra(prev, end, q)? - self.extra_at(self.filled)?;
                return Ok((mid / wad + U256::from(carry)).saturating_sub(extra));
            }
            prev = end;
        }
        Err(invalid("beyond bid depth"))
    }

    /// Returns (output, new base cursor). Zero output denotes an input below atomic precision.
    pub fn quote(&self, input: U256) -> Result<(U256, U256), SimulationError> {
        if input == U256::ZERO {
            return Ok((U256::ZERO, self.filled));
        }
        if !self.asks {
            let q = add(self.filled, input)?;
            if q > self.max {
                return Err(invalid("beyond bid depth"));
            }
            let mid = mul(self.price, input)? / U256::from(10u64.pow(18));
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
                    // Solidity Math.mulDiv uses a full-width intermediate here only.
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

    pub fn marginal_price(&self) -> Result<f64, SimulationError> {
        let mut prev = (U256::ZERO, U256::ZERO);
        for &end in &self.knots {
            if end.0 > self.filled && self.filled < self.max {
                let slope = big(end.1 - prev.1)
                    .to_f64()
                    .ok_or_else(|| invalid("price conversion"))? /
                    big(end.0 - prev.0)
                        .to_f64()
                        .ok_or_else(|| invalid("quantity conversion"))?;
                let mid = big(self.price)
                    .to_f64()
                    .ok_or_else(|| invalid("price conversion"))? /
                    1e18;
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
