//! Properties every `ProtocolSim` state must have, checked by [`check_state`]. They mirror what
//! execution on chain does; where a venue differs, its state declares how through
//! [`CheckableState`].

mod copies;
mod limits;
mod quotes;
mod spot_prices;
#[cfg(test)]
mod tests;

use std::{
    marker::PhantomData,
    panic::{self, AssertUnwindSafe},
};

use derive_more::Display;
use itertools::Itertools;
use num_bigint::BigUint;
use tycho_simulation::{
    evm::protocol::checkable::{CheckableState, LimitKind},
    tycho_common::{
        models::token::Token,
        simulation::{
            errors::SimulationError,
            protocol_sim::{GetAmountOutResult, ProtocolSim},
        },
    },
};

use self::{
    copies::{CopiesAlike, SerdeRoundTrip},
    limits::{LimitsAsDeclared, LimitsWithoutError},
    quotes::{
        LimitsNotGrown, MonotonicOutput, NewStateSameType, NewStateValid, NoRoundTripGain,
        NoSplitGain, QuotesWithoutPanic,
    },
    spot_prices::{
        FeeFraction, FiniteSpotPrice, QuoteAtLeastSpotBefore, QuoteAtMostSpotAfter,
        SpotPriceWhereTradable, SpotPricesProductAtLeastOne,
    },
};
use crate::checks::amounts::Amount;

/// A broken property, with what broke it.
#[derive(Debug, Display, Clone, PartialEq, Eq)]
#[display("{property}: {detail}")]
pub struct Violation {
    /// The name of the broken property.
    pub property: &'static str,
    /// What breaks it: the direction, amounts and values involved.
    pub detail: String,
}

/// Checks every property of `state` in every direction between `tokens`, at `amounts`, resolved
/// against the direction's `max_input`, and at [`CheckableState::amounts`].
pub fn check_state<S: ProtocolSim + CheckableState>(
    state: &S,
    tokens: &[Token],
    amounts: &[Amount],
) -> Vec<Violation> {
    let mut violations = Violations::default();
    let subject = "the state";

    violations.check(FeeFraction { state, subject });
    let serde_copy = violations
        .check(SerdeRoundTrip { state, subject })
        .flatten();

    for (a, b) in tokens.iter().tuple_combinations() {
        violations.check(SpotPricesProductAtLeastOne { state, a, b });
    }

    for (token_in, token_out) in tokens
        .iter()
        .permutations(2)
        .map(|pair| (pair[0], pair[1]))
    {
        let direction = &Direction { token_in, token_out };

        let spot = violations
            .check(FiniteSpotPrice { state, subject, direction })
            .flatten();

        let Some(Some((max_input, max_output))) =
            violations.check(LimitsWithoutError { state, subject, direction })
        else {
            continue;
        };
        let kind = state.limit_kind(&token_in.address, &token_out.address);
        let limit = &Limit { max_input, max_output, kind };
        violations.check(LimitsAsDeclared { state, direction, limit });

        let tested = amounts
            .iter()
            .filter_map(|amount| amount.resolve(&limit.max_input))
            .chain(state.amounts(&token_in.address, &token_out.address))
            .sorted()
            .dedup()
            .collect_vec();

        let serde_copy = serde_copy.as_deref();
        violations.check(CopiesAlike { state, subject, serde_copy, direction, amounts: &tested });

        let Some(ref quotes) =
            violations.check(QuotesWithoutPanic { state, direction, amounts: &tested })
        else {
            continue;
        };

        violations.check(SpotPriceWhereTradable { direction, limit, spot, quotes });
        violations.check(MonotonicOutput { direction, quotes });
        violations.check(NoSplitGain { state, direction, quotes, amounts: &tested });

        violations.check(NewStateSameType::<S> { direction, quotes, state_type: PhantomData });
        violations.check(NewStateValid::<S> { direction, quotes, state_type: PhantomData });
        violations.check(NoRoundTripGain { direction, quotes });

        if let Some(spot_before) = spot {
            violations.check(QuoteAtLeastSpotBefore { direction, spot_before, quotes });
        }

        let mut sales = Vec::with_capacity(quotes.len());
        for (amount, quote) in quotes {
            let state = quote.new_state.as_ref();
            let subject = &format!("the state after selling {amount}");

            violations.check(FeeFraction { state, subject });
            let spot_after = violations
                .check(FiniteSpotPrice { state, subject, direction })
                .flatten();
            let max_after = violations
                .check(LimitsWithoutError { state, subject, direction })
                .flatten();

            let serde_copy = violations
                .check(SerdeRoundTrip { state, subject })
                .flatten();
            let serde_copy = serde_copy.as_deref();
            violations.check(CopiesAlike { state, subject, serde_copy, direction, amounts: &[] });

            sales.push(Sale { amount, quote, max_after, spot_after });
        }

        violations.check(LimitsNotGrown { direction, limit, sales: &sales });
        violations.check(QuoteAtMostSpotAfter { direction, sales: &sales });
    }

    violations.0
}

/// One property of a state, with what checking it needs. A check that panics breaks its property.
trait Check {
    /// The property's name in violations.
    const NAME: &'static str;

    /// What the check returns besides its violations, for the checks after it.
    type Output;

    fn check(self, violations: &mut PropertyViolations<'_>) -> Self::Output;
}

/// The violations found so far.
#[derive(Default)]
struct Violations(Vec<Violation>);

impl Violations {
    /// Runs `check` and records its violations, or its panic as one. `None` when it panicked.
    fn check<C: Check>(&mut self, check: C) -> Option<C::Output> {
        let mut violations = PropertyViolations { violations: self, property: C::NAME };
        let output = panic::catch_unwind(AssertUnwindSafe(|| check.check(&mut violations)));
        output
            .map_err(|payload| {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|message| message.to_string())
                    .or_else(|| {
                        payload
                            .downcast_ref::<String>()
                            .cloned()
                    })
                    .unwrap_or_default();
                violations.push(format!("panicked: {message}"));
            })
            .ok()
    }
}

/// Records violations of one property.
struct PropertyViolations<'a> {
    violations: &'a mut Violations,
    property: &'static str,
}

impl PropertyViolations<'_> {
    fn push(&mut self, detail: String) {
        self.violations
            .0
            .push(Violation { property: self.property, detail });
    }
}

/// Selling `token_in` for `token_out`.
#[derive(Display)]
#[display("{} -> {}", token_in.address, token_out.address)]
struct Direction<'a> {
    token_in: &'a Token,
    token_out: &'a Token,
}

impl Direction<'_> {
    fn quote(
        &self,
        state: &dyn ProtocolSim,
        amount: BigUint,
    ) -> Result<GetAmountOutResult, SimulationError> {
        state.get_amount_out(amount, self.token_in, self.token_out)
    }

    fn limits(&self, state: &dyn ProtocolSim) -> Result<(BigUint, BigUint), SimulationError> {
        state.get_limits(self.token_in.address.clone(), self.token_out.address.clone())
    }

    fn reversed(&self) -> Direction<'_> {
        Direction { token_in: self.token_out, token_out: self.token_in }
    }
}

/// A direction's limits, with the kind the state declares.
struct Limit {
    max_input: BigUint,
    max_output: BigUint,
    kind: LimitKind,
}

/// The tested amounts that quote, each with its quote.
type Quotes = Vec<(BigUint, GetAmountOutResult)>;

/// A quote, with the limits and spot price of the state it returns in its direction, where
/// [`LimitsWithoutError`] and [`FiniteSpotPrice`] found them usable.
struct Sale<'a> {
    amount: &'a BigUint,
    quote: &'a GetAmountOutResult,
    /// `(max_input, max_output)` after the sale.
    max_after: Option<(BigUint, BigUint)>,
    /// The finite, positive spot price after the sale.
    spot_after: Option<f64>,
}

/// Whether a spot price is one: finite and positive.
fn finite_positive(price: f64) -> bool {
    price.is_finite() && price > 0.0
}
