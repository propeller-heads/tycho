//! Strict mode's checks of a component's limits, one swap direction at a time.
//!
//! `ProtocolSim::get_limits` says how much a direction can take (`max_input`) and pay out
//! (`max_output`). The ladder trades at most 10% of `max_input`, so on its own it passes a limit
//! ten times too high. These checks test the limit itself:
//!
//! - `limits.at_limit`: `max_input` must quote, and its quote goes to execution, which must succeed
//!   and pay the quote within the execution tolerance;
//! - `limits.max_output`: the quote at `max_input` must not pay more than `max_output`;
//! - `limits.past_limit`: past the limit, the venue behaves as the declaration claims. One unit
//!   past a [`Limits::Hard`] limit, the largest input the venue accepts, the quote errors and the
//!   execution reverts. One unit past an [`Limits::UnderCap`] limit, below the venue's bound, the
//!   quote and the execution succeed. Twice a [`Limits::Soft`] limit, which the venue does not
//!   bound, still quotes. Nothing is probed past a limit whose own quote failed, since what happens
//!   past it then proves nothing.

use num_bigint::BigUint;

use crate::{checks::outcome::CheckId, expectations::Limits};

/// A quote: what `get_amount_out` returns, without the new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    pub amount: BigUint,
    pub gas: BigUint,
}

/// A direction's limit as `get_limits` reports it, after the `int128` execution cap.
pub struct LimitSizes<'a> {
    pub max_input: &'a BigUint,
    pub max_output: &'a BigUint,
    /// Whether the cap applied: a capped limit is not the venue's bound, so nothing is probed past
    /// it.
    pub capped: bool,
}

/// What the simulation found about one direction's limit, before execution.
pub struct SimulatedLimitChecks {
    /// The quote at `max_input`, when it succeeded. It is executed like the ladder's rungs.
    pub at_limit_quote: Option<Quote>,
    /// The swap one unit past the limit to execute, when the simulation behaved as declared.
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
    pub claim: PastLimitClaim,
}

/// What the execution of a swap past the limit must do, as the declared kind claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastLimitClaim {
    /// One unit past a hard limit, the venue rejects the swap.
    Reverts,
    /// One unit past an under-cap limit, below the venue's bound, the swap executes.
    Executes,
}

/// Checks one direction's limit against what the component declares. `quote` is the direction's
/// `get_amount_out`.
pub fn check_direction(
    quote: impl Fn(BigUint) -> Result<Quote, String>,
    direction: &str,
    sizes: &LimitSizes<'_>,
    declared: &Limits,
) -> SimulatedLimitChecks {
    let LimitSizes { max_input, max_output, capped } = *sizes;
    let mut failures = Vec::new();

    let at_limit = match quote(max_input.clone()) {
        Ok(at_limit) => at_limit,
        Err(e) => {
            return SimulatedLimitChecks {
                at_limit_quote: None,
                past_limit_swap: None,
                past_limit_rejection: None,
                failures: vec![(
                    CheckId::LimitsAtLimit,
                    format!("{direction}: quoting the limit of {max_input} failed: {e}"),
                )],
            };
        }
    };
    if at_limit.amount > *max_output {
        failures.push((
            CheckId::LimitsMaxOutput,
            format!(
                "{direction}: quoting the limit of {max_input} pays {}, more than the max output \
                 of {max_output} that get_limits reports",
                at_limit.amount
            ),
        ));
    }
    let mut past_limit_rejection = None;
    let past_limit_swap = match declared {
        // The cap is not the venue's bound, so nothing past it says anything about the venue.
        _ if capped => None,
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
        Limits::Hard {} | Limits::UnderCap { .. } => {
            let past = max_input + 1u32;
            match (declared, quote(past.clone())) {
                (Limits::Hard {}, Err(e)) => {
                    past_limit_rejection = Some(e);
                    Some(PastLimitSwap { amount_in: past, claim: PastLimitClaim::Reverts })
                }
                (Limits::Hard {}, Ok(past_quote)) => {
                    failures.push((
                        CheckId::LimitsPastLimit,
                        format!(
                            "{direction}: the limit is declared hard, but {past}, one unit past \
                             it, quotes {}. Either get_limits reports less than the largest input \
                             the venue accepts (declare it `under_cap`, with the capacity it gives \
                             up) or get_amount_out does not enforce the bound",
                            past_quote.amount
                        ),
                    ));
                    None
                }
                (Limits::UnderCap { .. }, Ok(_)) => {
                    Some(PastLimitSwap { amount_in: past, claim: PastLimitClaim::Executes })
                }
                (Limits::UnderCap { .. }, Err(e)) => {
                    failures.push((
                        CheckId::LimitsPastLimit,
                        format!(
                            "{direction}: the limit is declared under_cap, but {past}, one unit \
                             past it, does not quote: {e}. The limit is the largest input the \
                             venue accepts: declare it hard"
                        ),
                    ));
                    None
                }
                // Handled above: a soft limit is probed at twice the limit.
                (Limits::Soft { .. }, _) => None,
            }
        }
    };

    SimulatedLimitChecks {
        at_limit_quote: Some(at_limit),
        past_limit_swap,
        past_limit_rejection,
        failures,
    }
}

/// What the simulation alone decides about `limits.past_limit`, before any execution. Both
/// runners record from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastLimitInSimulation {
    /// The limit did not quote, so nothing was probed past it, whatever the kind: not run.
    AtLimitFailed,
    /// The limit is at the `int128` cap, which is not the venue's bound: nothing was probed past
    /// it.
    Capped,
    /// A soft limit, whose whole claim, that twice the limit quotes, was checked in simulation.
    Passed,
    /// A hard or under-cap limit: the swap past it must be executed.
    NeedsExecution,
}

/// What `limits.past_limit` is in simulation alone, for a limit whose quote at the limit
/// succeeded (`quoted`), at the cap (`capped`), declared `limits`. A failure the simulation
/// recorded outranks it.
pub fn past_limit_in_simulation(
    quoted: bool,
    capped: bool,
    limits: &Limits,
) -> PastLimitInSimulation {
    if !quoted {
        PastLimitInSimulation::AtLimitFailed
    } else if capped {
        PastLimitInSimulation::Capped
    } else if let Limits::Soft { .. } = limits {
        PastLimitInSimulation::Passed
    } else {
        PastLimitInSimulation::NeedsExecution
    }
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

    /// The quote function of a venue paying 2 out per unit in, up to `bound` in.
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

    /// The quote function of a venue paying one unit out per million in, as a 6-decimal token
    /// sold for an 18-decimal one rounds, that rejects any swap paying more than `out_bound`.
    fn output_bound_venue(out_bound: u64) -> impl Fn(BigUint) -> Result<Quote, String> {
        move |amount_in| {
            let out = amount_in / 1_000_000u32;
            if out > BigUint::from(out_bound) {
                return Err("pays past the reserve".to_string());
            }
            Ok(Quote { amount: out, gas: BigUint::from(100u32) })
        }
    }

    fn hard() -> Limits {
        Limits::Hard {}
    }

    fn soft() -> Limits {
        Limits::Soft { reason: "r".to_string() }
    }

    fn under_cap() -> Limits {
        Limits::UnderCap { reason: "r".to_string() }
    }

    fn check_with(
        quote: impl Fn(BigUint) -> Result<Quote, String>,
        max_input: &BigUint,
        max_output: &BigUint,
        declared: &Limits,
    ) -> SimulatedLimitChecks {
        check_direction(
            quote,
            "A -> B",
            &LimitSizes { max_input, max_output, capped: false },
            declared,
        )
    }

    /// Checks a venue with `bound` and `past_bound` whose get_limits reports `max_input` and
    /// `max_output`, declared as `declared`.
    fn check(
        bound: u64,
        past_bound: PastBound,
        max_input: u64,
        max_output: u64,
        declared: &Limits,
    ) -> SimulatedLimitChecks {
        let (max_input, max_output) = (BigUint::from(max_input), BigUint::from(max_output));
        check_with(venue(bound, past_bound), &max_input, &max_output, declared)
    }

    fn failed_checks(result: &SimulatedLimitChecks) -> Vec<CheckId> {
        result
            .failures
            .iter()
            .map(|(id, _)| *id)
            .collect()
    }

    fn past(amount_in: u64, claim: PastLimitClaim) -> PastLimitSwap {
        PastLimitSwap { amount_in: BigUint::from(amount_in), claim }
    }

    #[test]
    fn an_exact_hard_limit_passes_and_is_probed_on_chain() {
        let result = check(1000, PastBound::Errors, 1000, 2000, &hard());
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert_eq!(
            result.at_limit_quote,
            Some(Quote { amount: BigUint::from(2000u32), gas: BigUint::from(100u32) })
        );
        assert_eq!(result.past_limit_swap, Some(past(1001, PastLimitClaim::Reverts)));
    }

    #[test]
    fn a_limit_above_the_bound_fails_only_at_the_limit_whatever_its_kind() {
        // Ten times the venue's bound: the ladder's 10% rung would still quote. Nothing is probed
        // past a limit that does not quote, so no kind is told to change.
        for declared in [hard(), under_cap(), soft()] {
            let result = check(1000, PastBound::Errors, 10_000, 20_000, &declared);
            assert_eq!(failed_checks(&result), vec![CheckId::LimitsAtLimit], "{declared:?}");
            assert!(result.at_limit_quote.is_none());
            assert!(result.past_limit_swap.is_none());
        }
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
        assert_eq!(result.past_limit_swap, Some(past(901, PastLimitClaim::Executes)));
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
    fn a_quote_at_the_limit_above_max_output_fails() {
        let result = check(1000, PastBound::Errors, 1000, 1999, &hard());
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsMaxOutput]);
    }

    #[test]
    fn a_max_output_above_the_quote_at_the_limit_passes() {
        // Such as a rounded-up price estimate.
        let result = check(1000, PastBound::Errors, 1000, 2001, &hard());
        assert!(result.failures.is_empty(), "{:?}", result.failures);
    }

    #[test]
    fn a_capped_limit_is_not_probed_past() {
        let (max_input, max_output) = (BigUint::from(1000u32), BigUint::from(u64::MAX));
        let result = check_direction(
            venue(1000, PastBound::Unbounded),
            "A -> B",
            &LimitSizes { max_input: &max_input, max_output: &max_output, capped: true },
            &hard(),
        );
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.past_limit_swap.is_none());
    }

    #[test]
    fn an_output_bound_is_hard_when_get_limits_reports_the_largest_input_it_accepts() {
        // The venue pays at most 5000 out, so the largest input it accepts is 5_000_999_999.
        let max_output = BigUint::from(5000u32);
        let quote = output_bound_venue(5000);
        let max_input = BigUint::from(5_000_999_999u64);
        let result = check_with(&quote, &max_input, &max_output, &hard());
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert_eq!(result.past_limit_swap, Some(past(5_001_000_000, PastLimitClaim::Reverts)));
    }

    #[test]
    fn an_output_bound_whose_get_limits_rounds_down_is_under_cap() {
        // get_limits inverts the bound of 5000 out with a floor division: 5000 * 1_000_000, short
        // of the largest input the venue accepts, so one unit past it still quotes 5000.
        let (max_input, max_output) = (BigUint::from(5_000_000_000u64), BigUint::from(5000u32));
        let quote = output_bound_venue(5000);

        let as_hard = check_with(&quote, &max_input, &max_output, &hard());
        assert_eq!(failed_checks(&as_hard), vec![CheckId::LimitsPastLimit]);
        assert!(
            as_hard.failures[0]
                .1
                .contains("under_cap"),
            "{:?}",
            as_hard.failures
        );

        let as_under_cap = check_with(&quote, &max_input, &max_output, &under_cap());
        assert!(as_under_cap.failures.is_empty(), "{:?}", as_under_cap.failures);
        assert_eq!(
            as_under_cap.past_limit_swap,
            Some(past(5_000_000_001, PastLimitClaim::Executes))
        );
    }

    #[test]
    fn past_limit_in_simulation_needs_the_quote_at_the_limit_first() {
        use PastLimitInSimulation::*;
        for limits in [hard(), soft(), under_cap()] {
            assert_eq!(past_limit_in_simulation(false, false, &limits), AtLimitFailed);
            assert_eq!(past_limit_in_simulation(false, true, &limits), AtLimitFailed);
            assert_eq!(past_limit_in_simulation(true, true, &limits), Capped);
        }
        assert_eq!(past_limit_in_simulation(true, false, &soft()), Passed);
        assert_eq!(past_limit_in_simulation(true, false, &hard()), NeedsExecution);
        assert_eq!(past_limit_in_simulation(true, false, &under_cap()), NeedsExecution);
    }
}
