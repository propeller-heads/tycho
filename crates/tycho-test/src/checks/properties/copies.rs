//! Copies of a state: [`SerdeRoundTrip`] and [`CopiesAlike`].

use num_bigint::BigUint;
use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

use super::{Check, Direction, PropertyViolations};

/// The state serialises and deserialises. The check returns the copy.
pub struct SerdeRoundTrip<'a> {
    pub state: &'a dyn ProtocolSim,
    /// Names the state in the violations.
    pub subject: &'a str,
}

impl Check for SerdeRoundTrip<'_> {
    const NAME: &'static str = "serde_round_trip";
    type Output = Option<Box<dyn ProtocolSim>>;

    fn check(self, violations: &mut PropertyViolations<'_>) -> Option<Box<dyn ProtocolSim>> {
        let Self { state, subject } = self;
        serde_json::to_string(state)
            .and_then(|json| serde_json::from_str(&json))
            .map_err(|e| violations.push(format!("{subject} does not round-trip: {e}")))
            .ok()
    }
}

/// A state's `clone_box`, and its serde copy when it has one, return what the state returns in this
/// direction: its fee, limits and spot price, and its quotes at `amounts`.
pub struct CopiesAlike<'a> {
    pub state: &'a dyn ProtocolSim,
    /// Names the state in the violations.
    pub subject: &'a str,
    /// What [`SerdeRoundTrip`] handed back.
    pub serde_copy: Option<&'a dyn ProtocolSim>,
    pub direction: &'a Direction<'a>,
    pub amounts: &'a [BigUint],
}

impl Check for CopiesAlike<'_> {
    const NAME: &'static str = "copies_alike";
    type Output = ();

    fn check(self, violations: &mut PropertyViolations<'_>) {
        let Self { state, subject, serde_copy, direction, amounts } = self;

        let clone = state.clone_box();
        let mut copies = vec![("clone", clone.as_ref())];
        copies.extend(serde_copy.map(|copy| ("serde copy", copy)));

        let fee = |state: &dyn ProtocolSim| state.fee().to_bits();
        let limits = |state: &dyn ProtocolSim| {
            direction
                .limits(state)
                .map_err(|e| e.to_string())
        };
        let spot = |state: &dyn ProtocolSim| {
            state
                .spot_price(direction.token_out, direction.token_in)
                .map(f64::to_bits)
                .map_err(|e| e.to_string())
        };

        for (kind, copy) in copies {
            let mut differs = |what: String| {
                violations.push(format!("{direction}: the {kind} of {subject} differs in {what}"));
            };

            if fee(state) != fee(copy) {
                differs(format!("fee: {} vs {}", state.fee(), copy.fee()));
            }
            if limits(state) != limits(copy) {
                differs(format!("limits: {:?} vs {:?}", limits(state), limits(copy)));
            }
            if spot(state) != spot(copy) {
                differs(format!("spot price: {:?} vs {:?}", spot(state), spot(copy)));
            }

            for amount in amounts {
                let quote = |state: &dyn ProtocolSim| {
                    direction
                        .quote(state, amount.clone())
                        .map(|quote| (quote.amount, quote.gas))
                        .map_err(|e| e.to_string())
                };
                if quote(state) != quote(copy) {
                    differs(format!(
                        "the quote for {amount}: {:?} vs {:?}",
                        quote(state),
                        quote(copy)
                    ));
                }
            }
        }
    }
}
