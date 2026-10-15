//! Reference model and random extractor histories for delta catch-up tests.
//!
//! [`DeltaStateModel`] is the oracle: the state a client holds after applying a sequence of
//! WebSocket messages. A client that caught up after a gap must end in a state that matches the
//! canonical chain ([`DeltaStateModel::check_against`]), or take a snapshot.
//!
//! [`generate_history`] produces extractor histories with the shapes that make catch-up hard:
//! nested reverts, reverts below a client's block, components created on orphaned branches,
//! window evictions, extractor restarts and fingerprint changes. [`catch_up_spec`] says, for a
//! client that missed part of a history, whether catch-up must succeed and with which messages,
//! or why it must fall back to a snapshot. [`follower_states`] and [`sample_gaps`] give the state
//! of a client that follows the WebSocket and the gaps worth checking.

mod clients;
mod generator;
mod model;
mod spec;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

pub use clients::{follower_states, sample_gaps};
pub use generator::{
    generate_history, History, HistoryConfig, HistoryEvent, RestartKind, CHAIN, EXTRACTOR,
};
pub use model::{through_the_wire, DeltaStateModel, StateMismatch};
pub use spec::{catch_up_spec, CatchUpSpec, FallbackReason, Gap};
