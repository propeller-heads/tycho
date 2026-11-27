//! The amounts the properties quote a direction at. A case's amounts are drawn before its state's
//! limits are known, so each [`Amount`] is a description, resolved against a direction's
//! `max_input` when that direction is checked. They work on any state, generated or decoded.

use num_bigint::BigUint;
use num_traits::CheckedSub;

/// An amount to quote, relative to the direction's `max_input` or absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Amount {
    /// This many base units.
    Raw(BigUint),
    /// `max_input` plus this many base units, which may be negative.
    LimitPlus(i64),
    /// `max_input` times this many millionths.
    PartsPerMillion(u64),
}

impl Amount {
    /// The amount for a direction whose limit is `max_input`; `None` when it would be negative.
    pub fn resolve(&self, max_input: &BigUint) -> Option<BigUint> {
        match self {
            Amount::Raw(raw) => Some(raw.clone()),
            Amount::LimitPlus(plus) => {
                let magnitude = BigUint::from(plus.unsigned_abs());
                if *plus >= 0 {
                    Some(max_input + magnitude)
                } else {
                    max_input.checked_sub(&magnitude)
                }
            }
            Amount::PartsPerMillion(ppm) => Some(max_input * *ppm / 1_000_000u32),
        }
    }
}
