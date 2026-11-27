//! The quotes in one direction: [`QuotesWithoutPanic`], [`MonotonicOutput`], [`NoSplitGain`],
//! [`NewStateSameType`], [`NewStateValid`], [`LimitsNotGrown`] and [`NoRoundTripGain`].

use std::marker::PhantomData;

use itertools::Itertools;
use num_bigint::BigUint;
use tycho_simulation::{
    evm::protocol::checkable::{CheckableState, LimitKind},
    tycho_common::simulation::protocol_sim::ProtocolSim,
};

use super::{Check, Direction, Limit, PropertyViolations, Quotes, Sale};

/// `get_amount_out` returns a quote or an error for every amount, without panicking. The check
/// returns the quotes.
pub struct QuotesWithoutPanic<'a> {
    pub state: &'a dyn ProtocolSim,
    pub direction: &'a Direction<'a>,
    pub amounts: &'a [BigUint],
}

impl Check for QuotesWithoutPanic<'_> {
    const NAME: &'static str = "quotes_without_panic";
    type Output = Quotes;

    fn check(self, _violations: &mut PropertyViolations<'_>) -> Quotes {
        let Self { state, direction, amounts } = self;
        amounts
            .iter()
            .filter_map(|amount| {
                Some((
                    amount.clone(),
                    direction
                        .quote(state, amount.clone())
                        .ok()?,
                ))
            })
            .collect()
    }
}

/// Output never decreases with input.
pub struct MonotonicOutput<'a> {
    pub direction: &'a Direction<'a>,
    pub quotes: &'a Quotes,
}

impl Check for MonotonicOutput<'_> {
    const NAME: &'static str = "monotonic_output";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, quotes } = self;
        let quoted = quotes
            .iter()
            .map(|(amount, quote)| (amount, &quote.amount));
        for ((smaller_in, smaller_out), (larger_in, larger_out)) in quoted.tuple_windows() {
            if smaller_out > larger_out {
                violations.push(format!(
                    "{direction}: {smaller_in} buys {smaller_out}, more than the {larger_out} \
                     that {larger_in} buys"
                ));
            }
        }
    }
}

/// Splitting a trade never beats one trade: selling one amount and then another on the state the
/// first returns buys at most what selling both at once buys.
pub struct NoSplitGain<'a> {
    pub state: &'a dyn ProtocolSim,
    pub direction: &'a Direction<'a>,
    pub quotes: &'a Quotes,
    /// The amounts to sell second.
    pub amounts: &'a [BigUint],
}

impl Check for NoSplitGain<'_> {
    const NAME: &'static str = "no_split_gain";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { state, direction, quotes, amounts } = self;
        for (first, quote) in quotes {
            let after = quote.new_state.as_ref();
            for second in amounts {
                let (Ok(whole), Ok(rest)) = (
                    direction.quote(state, first + second),
                    direction.quote(after, second.clone()),
                ) else {
                    continue;
                };
                let split = &quote.amount + &rest.amount;
                if whole.amount < split {
                    violations.push(format!(
                        "{direction}: selling {first} then {second} buys {split}, more than the \
                         {} that selling {} at once buys",
                        whole.amount,
                        first + second
                    ));
                }
            }
        }
    }
}

/// The state a quote returns is an `S`, the type of the state quoted.
pub struct NewStateSameType<'a, S> {
    pub direction: &'a Direction<'a>,
    pub quotes: &'a Quotes,
    pub state_type: PhantomData<S>,
}

impl<S: ProtocolSim> Check for NewStateSameType<'_, S> {
    const NAME: &'static str = "new_state_same_type";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, quotes, state_type: _ } = self;
        for (amount, quote) in quotes {
            if !quote.new_state.as_any().is::<S>() {
                violations.push(format!(
                    "{direction}: the state after selling {amount} is not a {}",
                    std::any::type_name::<S>()
                ));
            }
        }
    }
}

/// The state a quote returns is valid, when it is an `S`.
pub struct NewStateValid<'a, S> {
    pub direction: &'a Direction<'a>,
    pub quotes: &'a Quotes,
    pub state_type: PhantomData<S>,
}

impl<S: ProtocolSim + CheckableState> Check for NewStateValid<'_, S> {
    const NAME: &'static str = "new_state_valid";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, quotes, state_type: _ } = self;
        for (amount, quote) in quotes {
            let Some(after) = quote
                .new_state
                .as_any()
                .downcast_ref::<S>()
            else {
                continue
            };
            if let Err(e) = after.check_valid() {
                violations
                    .push(format!("{direction}: the state after selling {amount} is invalid: {e}"));
            }
        }
    }
}

/// A quote never grows its direction's limits: `max_output` stays at most what it was, and so does
/// a bounded `max_input`.
pub struct LimitsNotGrown<'a> {
    pub direction: &'a Direction<'a>,
    pub limit: &'a Limit,
    pub sales: &'a [Sale<'a>],
}

impl Check for LimitsNotGrown<'_> {
    const NAME: &'static str = "limits_not_grown";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, limit, sales } = self;
        let bound = matches!(limit.kind, LimitKind::Hard | LimitKind::UnderCap);
        for Sale { amount, max_after, .. } in sales {
            // `LimitsWithoutError` reports an error.
            let Some((max_in_after, max_out_after)) = max_after else { continue };
            if *max_out_after > limit.max_output {
                violations.push(format!(
                    "{direction}: selling {amount} grows max_output from {} to {max_out_after}",
                    limit.max_output
                ));
            }
            if bound && *max_in_after > limit.max_input {
                violations.push(format!(
                    "{direction}: selling {amount} grows the bounded max_input from {} to \
                     {max_in_after}",
                    limit.max_input
                ));
            }
        }
    }
}

/// Selling the output back on the state a quote returns returns at most the input.
pub struct NoRoundTripGain<'a> {
    pub direction: &'a Direction<'a>,
    pub quotes: &'a Quotes,
}

impl Check for NoRoundTripGain<'_> {
    const NAME: &'static str = "no_round_trip_gain";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { direction, quotes } = self;
        let back = direction.reversed();
        for (amount, quote) in quotes {
            let Ok(returned) = back.quote(quote.new_state.as_ref(), quote.amount.clone()) else {
                continue;
            };
            if returned.amount > *amount {
                violations.push(format!(
                    "{direction}: selling {amount} buys {}, and selling that back returns {}",
                    quote.amount, returned.amount
                ));
            }
        }
    }
}
