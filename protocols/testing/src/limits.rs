//! Strict mode's checks of a component's limits, one swap direction at a time.
//!
//! `ProtocolSim::get_limits` says how much a direction can take (`max_input`) and pay out
//! (`max_output`). The ladder trades at most 10% of `max_input`, so on its own it passes a limit
//! ten times too high. These checks test the limit itself:
//!
//! - `limits.at_limit`: `max_input` must quote, and its quote goes to execution, which must
//!   succeed;
//! - `limits.max_output`: the quote at `max_input` must not pay more than `max_output`;
//! - `limits.past_limit`: past the limit, the venue behaves as the declaration claims. Past a
//!   [`Limits::Hard`] bound (`max_input + rounding + 1`) the quote errors and the execution
//!   reverts. One unit past a [`Limits::UnderCap`] limit, below the venue's bound, the quote and
//!   the execution succeed. Twice a [`Limits::Soft`] limit, which the venue does not bound, still
//!   quotes.

use num_bigint::BigUint;

use crate::{config::Limits, report::CheckId};

/// A quote: what `get_amount_out` returns, without the new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    pub amount: BigUint,
    pub gas: BigUint,
}

/// A direction's limit as `get_limits` reports it, after the `int128` execution cap.
pub struct Limit<'a> {
    pub max_input: &'a BigUint,
    pub max_output: &'a BigUint,
    /// Whether the cap applied: a capped limit is not the venue's bound, so nothing is probed past
    /// it.
    pub capped: bool,
}

/// The limit checks of one direction, before execution.
pub struct DirectionLimits {
    /// The quote at `max_input`, when it succeeded. It is executed like the ladder's rungs.
    pub at_limit_quote: Option<Quote>,
    /// The swap past the limit to execute, when the simulation behaved as declared: past a hard
    /// limit's bound, or one unit past an under-cap limit.
    pub past_limit_swap: Option<PastLimitSwap>,
    /// The error the simulation rejected the input past a hard limit with, for the logs.
    pub past_limit_rejection: Option<String>,
    /// Every check that failed, with the reason.
    pub failures: Vec<(CheckId, String)>,
}

/// An input past the limit, and what its execution must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastLimitSwap {
    pub amount_in: BigUint,
    /// Revert, past a hard limit's bound, or execute, one unit past an under-cap limit.
    pub must_revert: bool,
}

/// Checks one direction's limit against what the component declares. `quote` is the direction's
/// `get_amount_out`.
pub fn check_direction(
    quote: impl Fn(BigUint) -> Result<Quote, String>,
    direction: &str,
    limit: &Limit<'_>,
    declared: &Limits,
) -> DirectionLimits {
    let Limit { max_input, max_output, capped } = *limit;
    let mut failures = Vec::new();

    let at_limit_quote = match quote(max_input.clone()) {
        Ok(at_limit) => {
            if at_limit.amount > *max_output {
                failures.push((
                    CheckId::LimitsMaxOutput,
                    format!(
                        "{direction}: quoting the limit of {max_input} pays {}, more than the \
                         max output of {max_output} that get_limits reports",
                        at_limit.amount
                    ),
                ));
            }
            Some(at_limit)
        }
        Err(e) => {
            failures.push((
                CheckId::LimitsAtLimit,
                format!("{direction}: quoting the limit of {max_input} failed: {e}"),
            ));
            None
        }
    };

    let mut past_limit_rejection = None;
    let past_limit_swap = match declared {
        // The cap is not the venue's bound, so nothing past it says anything about the venue.
        _ if capped => None,
        Limits::Hard { rounding } => {
            // A bound derived with a rounding division can exceed the limit by up to `rounding`.
            let past = max_input + *rounding + 1u32;
            match quote(past.clone()) {
                Ok(past_quote) => {
                    let first_past = if *rounding == 0 {
                        "one unit past it".to_string()
                    } else {
                        format!("past it and its declared rounding of {rounding}")
                    };
                    failures.push((
                        CheckId::LimitsPastLimit,
                        format!(
                            "{direction}: the limit is declared hard, but {past}, {first_past}, \
                             quotes {}. Either the limit is below the venue's bound (declare it \
                             `under_cap`, or the rounding) or get_amount_out does not enforce it",
                            past_quote.amount
                        ),
                    ));
                    None
                }
                Err(e) => {
                    past_limit_rejection = Some(e);
                    Some(PastLimitSwap { amount_in: past, must_revert: true })
                }
            }
        }
        Limits::UnderCap { .. } => {
            let past = max_input + 1u32;
            match quote(past.clone()) {
                Ok(_) => Some(PastLimitSwap { amount_in: past, must_revert: false }),
                Err(e) => {
                    failures.push((
                        CheckId::LimitsPastLimit,
                        format!(
                            "{direction}: the limit is declared under_cap, but {past}, one unit \
                             past it, does not quote: {e}. The limit is the venue's bound: declare \
                             it hard"
                        ),
                    ));
                    None
                }
            }
        }
        Limits::Soft { .. } => {
            let twice = max_input * 2u32;
            if let Err(e) = quote(twice.clone()) {
                failures.push((
                    CheckId::LimitsPastLimit,
                    format!(
                        "{direction}: the limit is declared soft, but {twice}, twice the limit, \
                         does not quote: {e}. The venue has a bound: declare the limit hard or \
                         under_cap"
                    ),
                ));
            }
            None
        }
    };

    DirectionLimits { at_limit_quote, past_limit_swap, past_limit_rejection, failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a scripted venue does with an input above its real bound.
    #[derive(Debug, Clone, Copy)]
    enum PastBound {
        /// Errors, as a hard limit must.
        Errors,
        /// Fills only up to the bound and quotes that, instead of erroring.
        Clamps,
        /// Has no bound: quotes any input.
        Unbounded,
    }

    /// The quote function of a venue paying 2 out per unit in, up to `bound`.
    fn venue(bound: u64, past_bound: PastBound) -> impl Fn(BigUint) -> Result<Quote, String> {
        move |amount_in| {
            let bound = BigUint::from(bound);
            let filled = if amount_in <= bound {
                amount_in
            } else {
                match past_bound {
                    PastBound::Errors => return Err("past the bound".to_string()),
                    PastBound::Clamps => bound,
                    PastBound::Unbounded => amount_in,
                }
            };
            Ok(Quote { amount: filled * 2u32, gas: BigUint::from(100u32) })
        }
    }

    fn hard() -> Limits {
        Limits::Hard { rounding: 0 }
    }

    fn soft() -> Limits {
        Limits::Soft { reason: "r".to_string() }
    }

    fn under_cap() -> Limits {
        Limits::UnderCap { reason: "r".to_string() }
    }

    /// Checks a venue with `bound` and `past_bound` whose get_limits reports `max_input` and
    /// `max_output`, declared as `declared`.
    fn check(
        bound: u64,
        past_bound: PastBound,
        max_input: u64,
        max_output: u64,
        declared: &Limits,
    ) -> DirectionLimits {
        let (max_input, max_output) = (BigUint::from(max_input), BigUint::from(max_output));
        check_direction(
            venue(bound, past_bound),
            "A -> B",
            &Limit { max_input: &max_input, max_output: &max_output, capped: false },
            declared,
        )
    }

    fn failed_checks(result: &DirectionLimits) -> Vec<CheckId> {
        result
            .failures
            .iter()
            .map(|(id, _)| *id)
            .collect()
    }

    #[test]
    fn an_exact_hard_limit_passes_and_is_probed_on_chain() {
        let result = check(1000, PastBound::Errors, 1000, 2000, &hard());
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert_eq!(
            result.at_limit_quote,
            Some(Quote { amount: BigUint::from(2000u32), gas: BigUint::from(100u32) })
        );
        assert_eq!(
            result.past_limit_swap,
            Some(PastLimitSwap { amount_in: BigUint::from(1001u32), must_revert: true })
        );
    }

    #[test]
    fn a_limit_above_the_bound_fails_at_the_limit() {
        // Ten times the venue's bound: the ladder's 10% rung would still quote.
        let result = check(1000, PastBound::Errors, 10_000, 20_000, &hard());
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsAtLimit]);
        assert!(result.at_limit_quote.is_none());
    }

    #[test]
    fn a_hard_limit_that_clamps_past_the_bound_fails_past_the_limit() {
        let result = check(1000, PastBound::Clamps, 1000, 2000, &hard());
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsPastLimit]);
        assert!(result.past_limit_swap.is_none());
    }

    #[test]
    fn a_hard_limit_below_the_bound_fails_past_the_limit() {
        let result = check(1000, PastBound::Errors, 900, 1800, &hard());
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsPastLimit]);
    }

    #[test]
    fn an_under_cap_limit_below_the_bound_is_executed_one_unit_past() {
        let result = check(1000, PastBound::Errors, 900, 1800, &under_cap());
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.at_limit_quote.is_some());
        assert_eq!(
            result.past_limit_swap,
            Some(PastLimitSwap { amount_in: BigUint::from(901u32), must_revert: false })
        );
    }

    #[test]
    fn an_under_cap_limit_at_the_bound_fails_past_the_limit() {
        let result = check(1000, PastBound::Errors, 1000, 2000, &under_cap());
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsPastLimit]);
        assert!(result.past_limit_swap.is_none());
    }

    #[test]
    fn a_soft_limit_quotes_twice_the_limit_and_executes_nothing_past_it() {
        let result = check(1000, PastBound::Unbounded, 1000, 2000, &soft());
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.at_limit_quote.is_some());
        assert!(result.past_limit_swap.is_none());
    }

    #[test]
    fn a_soft_limit_on_a_bounded_venue_fails_past_the_limit() {
        let result = check(1000, PastBound::Errors, 1000, 2000, &soft());
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsPastLimit]);
    }

    #[test]
    fn the_quote_at_the_limit_must_not_pay_more_than_max_output() {
        let understated = check(1000, PastBound::Errors, 1000, 1999, &hard());
        assert_eq!(failed_checks(&understated), vec![CheckId::LimitsMaxOutput]);

        // A max output above what the limit pays, such as a rounded-up price estimate, passes.
        let overstated = check(1000, PastBound::Errors, 1000, 2001, &hard());
        assert!(overstated.failures.is_empty(), "{:?}", overstated.failures);
    }

    #[test]
    fn a_capped_limit_is_not_probed_past() {
        let (max_input, max_output) = (BigUint::from(1000u32), BigUint::from(u64::MAX));
        let result = check_direction(
            venue(1000, PastBound::Unbounded),
            "A -> B",
            &Limit { max_input: &max_input, max_output: &max_output, capped: true },
            &hard(),
        );
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.past_limit_swap.is_none());
    }

    #[test]
    fn a_hard_limit_with_a_declared_rounding_is_probed_past_the_rounding() {
        // The venue's bound is 1002 and get_limits rounds it down to 1000.
        let exact = check(1002, PastBound::Errors, 1000, 2000, &hard());
        assert_eq!(failed_checks(&exact), vec![CheckId::LimitsPastLimit]);

        let rounded = check(1002, PastBound::Errors, 1000, 2000, &Limits::Hard { rounding: 2 });
        assert!(rounded.failures.is_empty(), "{:?}", rounded.failures);
        assert_eq!(
            rounded.past_limit_swap,
            Some(PastLimitSwap { amount_in: BigUint::from(1003u32), must_revert: true })
        );
    }
}
