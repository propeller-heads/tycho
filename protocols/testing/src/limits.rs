//! Strict mode's checks of a component's limits, one swap direction at a time.
//!
//! `ProtocolSim::get_limits` says how much a direction can take (`max_input`) and pay out
//! (`max_output`). The ladder trades at most 10% of `max_input`, so on its own it passes a limit
//! ten times too high. These checks test the limit itself:
//!
//! - `limits.at_limit`: `max_input` must quote, and its quote goes to execution, which must settle;
//! - `limits.max_output`: the quote at `max_input` must not pay more than `max_output`;
//! - `limits.past_limit`: for a [`LimitKind::Hard`] limit, `max_input + 1` must not quote, and its
//!   execution must revert.

use num_bigint::BigUint;

use crate::{config::LimitKind, report::CheckId};

/// A quote: what `get_amount_out` returns, without the new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    pub amount: BigUint,
    pub gas: BigUint,
}

/// The limit checks of one direction, before execution.
pub struct DirectionLimits {
    /// The quote at `max_input`, when it succeeded. It is executed like the ladder's rungs.
    pub at_limit: Option<Quote>,
    /// The input one unit past a hard limit, when the simulation rejected it as it must. Its
    /// execution must revert.
    pub past_limit: Option<BigUint>,
    /// The error the simulation rejected that input with, for the logs.
    pub past_limit_rejection: Option<String>,
    /// Every check that failed, with the reason.
    pub failures: Vec<(CheckId, String)>,
}

/// Checks one direction's limits against what the component declares.
///
/// `quote` is the direction's `get_amount_out`. `max_input` is the limit after the `int128`
/// execution cap, and `capped` says whether the cap applied: a capped limit is not the venue's
/// bound, so nothing is probed past it.
pub fn check_direction(
    quote: impl Fn(BigUint) -> Result<Quote, String>,
    direction: &str,
    max_input: &BigUint,
    max_output: &BigUint,
    capped: bool,
    kind: LimitKind,
) -> DirectionLimits {
    let mut failures = Vec::new();

    let at_limit = match quote(max_input.clone()) {
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
    let past_limit = if kind == LimitKind::Hard && !capped {
        let past = max_input + 1u32;
        match quote(past.clone()) {
            Ok(past_quote) => {
                failures.push((
                    CheckId::LimitsPastLimit,
                    format!(
                        "{direction}: the limit is declared hard, but {past}, one unit past it, \
                         quotes {}. Either the limit is below the venue's bound (declare it \
                         `under_cap`) or get_amount_out does not enforce it",
                        past_quote.amount
                    ),
                ));
                None
            }
            Err(e) => {
                past_limit_rejection = Some(e);
                Some(past)
            }
        }
    } else {
        None
    };

    DirectionLimits { at_limit, past_limit, past_limit_rejection, failures }
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

    /// Checks a venue with `bound` and `past_bound` whose get_limits reports `max_input` and
    /// `max_output`, declared as `kind`.
    fn check(
        bound: u64,
        past_bound: PastBound,
        max_input: u64,
        max_output: u64,
        kind: LimitKind,
    ) -> DirectionLimits {
        check_direction(
            venue(bound, past_bound),
            "A -> B",
            &BigUint::from(max_input),
            &BigUint::from(max_output),
            false,
            kind,
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
        let result = check(1000, PastBound::Errors, 1000, 2000, LimitKind::Hard);
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert_eq!(
            result.at_limit,
            Some(Quote { amount: BigUint::from(2000u32), gas: BigUint::from(100u32) })
        );
        assert_eq!(result.past_limit, Some(BigUint::from(1001u32)));
    }

    #[test]
    fn a_limit_above_the_bound_fails_at_the_limit() {
        // Ten times the venue's bound: the ladder's 10% rung would still quote.
        let result = check(1000, PastBound::Errors, 10_000, 20_000, LimitKind::Hard);
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsAtLimit]);
        assert!(result.at_limit.is_none());
    }

    #[test]
    fn a_hard_limit_that_clamps_past_the_bound_fails_past_the_limit() {
        let result = check(1000, PastBound::Clamps, 1000, 2000, LimitKind::Hard);
        assert_eq!(failed_checks(&result), vec![CheckId::LimitsPastLimit]);
        assert!(result.past_limit.is_none());
    }

    #[test]
    fn a_limit_below_the_bound_fails_past_the_limit_unless_declared_under_cap() {
        let hard = check(1000, PastBound::Errors, 900, 1800, LimitKind::Hard);
        assert_eq!(failed_checks(&hard), vec![CheckId::LimitsPastLimit]);

        let under_cap = check(1000, PastBound::Errors, 900, 1800, LimitKind::UnderCap);
        assert!(under_cap.failures.is_empty(), "{:?}", under_cap.failures);
        assert!(under_cap.at_limit.is_some());
        assert!(under_cap.past_limit.is_none());
    }

    #[test]
    fn a_soft_limit_is_tested_at_the_limit_only() {
        let result = check(1000, PastBound::Unbounded, 1000, 2000, LimitKind::Soft);
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.at_limit.is_some());
        assert!(result.past_limit.is_none());
    }

    #[test]
    fn the_quote_at_the_limit_must_not_pay_more_than_max_output() {
        let understated = check(1000, PastBound::Errors, 1000, 1999, LimitKind::Hard);
        assert_eq!(failed_checks(&understated), vec![CheckId::LimitsMaxOutput]);

        // A max output above what the limit pays, such as a rounded-up price estimate, passes.
        let overstated = check(1000, PastBound::Errors, 1000, 2001, LimitKind::Hard);
        assert!(overstated.failures.is_empty(), "{:?}", overstated.failures);
    }

    #[test]
    fn a_capped_limit_is_not_probed_past() {
        let result = check_direction(
            venue(1000, PastBound::Unbounded),
            "A -> B",
            &BigUint::from(1000u32),
            &BigUint::from(u64::MAX),
            true,
            LimitKind::Hard,
        );
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        assert!(result.past_limit.is_none());
    }
}
