//! What the property checks in `tycho-test` need from protocol states.
//!
//! The generic `ProtocolSim` properties hold for every state. A state the checks run on implements
//! [`CheckableState`]: the kind of limit its venue has, which states the venue can hold, and the
//! amounts where the state's own behaviour changes. Behind the `proptest` feature, [`generator`]
//! generates states to check.

use num_bigint::BigUint;
use tycho_common::Bytes;

#[cfg(feature = "proptest")]
pub mod generator;

/// What kind of bound a direction's `get_limits` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// `max_input` is the largest input the venue accepts (a vault cap, a redemption limit, the
    /// end of the liquidity range), whichever side of the swap the venue bounds. One unit more,
    /// `get_amount_out` errors and execution reverts.
    Hard,
    /// The venue has no bound on the input, so `max_input` is a recommended size, as the
    /// `ProtocolSim::get_limits` documentation allows. Twice it still quotes.
    Soft,
    /// The venue has a bound, and `max_input` stays below it because `get_limits` cannot report
    /// the largest input the venue accepts, such as a bound on the output without an exact
    /// inverse. One unit past it, the quote and the execution still succeed.
    UnderCap,
}

/// What the checks ask of a state they hold, decoded or generated.
pub trait CheckableState {
    /// The kind of bound `get_limits(sell_token, buy_token)` reports in this state.
    fn limit_kind(&self, sell_token: &Bytes, buy_token: &Bytes) -> LimitKind;

    /// Whether the state is one the venue can hold on chain, with what is wrong when it is not.
    fn check_valid(&self) -> Result<(), String>;

    /// Amounts to quote `get_amount_out(_, sell_token, buy_token)` at in this state besides the
    /// ones every state is quoted at: where its own behaviour changes, such as the smallest input
    /// that buys any output. None by default.
    fn amounts(&self, _sell_token: &Bytes, _buy_token: &Bytes) -> Vec<BigUint> {
        Vec::new()
    }
}
