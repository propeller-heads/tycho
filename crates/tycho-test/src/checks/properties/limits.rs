//! The limits of a direction: [`LimitsWithoutError`] and [`LimitsAsDeclared`].

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::{
    evm::protocol::checkable::LimitKind, tycho_common::simulation::protocol_sim::ProtocolSim,
};

use super::{Check, Direction, Limit, PropertyViolations};
use crate::checks::limits::{check_direction, LimitSizes, Quote};

/// `get_limits` returns limits, not an error. The check returns them, `(max_input, max_output)`,
/// for the checks after it.
pub struct LimitsWithoutError<'a> {
    pub state: &'a dyn ProtocolSim,
    /// Names the state in the violations.
    pub subject: &'a str,
    pub direction: &'a Direction<'a>,
}

impl Check for LimitsWithoutError<'_> {
    const NAME: &'static str = "limits_without_error";
    type Output = Option<(BigUint, BigUint)>;

    fn check(self, violations: &mut PropertyViolations<'_>) -> Option<(BigUint, BigUint)> {
        let Self { state, subject, direction } = self;
        direction
            .limits(state)
            .map_err(|e| {
                violations.push(format!("{direction}: get_limits on {subject} errors: {e}"))
            })
            .ok()
    }
}

/// What happens past `max_input` is what the declared limit kind says, and the quote at `max_input`
/// pays at most `max_output`.
pub struct LimitsAsDeclared<'a> {
    pub state: &'a dyn ProtocolSim,
    pub direction: &'a Direction<'a>,
    pub limit: &'a Limit,
}

impl Check for LimitsAsDeclared<'_> {
    const NAME: &'static str = "limits_as_declared";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { state, direction, limit: Limit { max_input, max_output, kind } } = self;
        if max_input.is_zero() {
            // Nothing is asserted about an amount of zero; a hard zero limit refuses every amount
            // above it.
            if *kind == LimitKind::Hard {
                if let Ok(quote) = direction.quote(state, BigUint::from(1u8)) {
                    violations.push(format!(
                        "{direction}: the hard limit is zero, but 1 quotes {}",
                        quote.amount
                    ));
                }
            }
            if !max_output.is_zero() {
                violations
                    .push(format!("{direction}: max_input is zero but max_output is {max_output}"));
            }
            return;
        }
        let quote = |amount: BigUint| {
            direction
                .quote(state, amount)
                .map(|quote| Quote { amount: quote.amount, gas: quote.gas })
                .map_err(|e| e.to_string())
        };
        let checked = check_direction(
            quote,
            &direction.to_string(),
            &LimitSizes { max_input, max_output, capped: false },
            *kind,
        );
        for (_, failure) in checked.failures {
            violations.push(failure);
        }
    }
}
