//! What a feed reports when a snapshot does not arrive.

use thiserror::Error;
use tycho_common::simulation::errors::SimulationError;

/// Failures of the feed layer: fetching, decoding, and publishing a provider's snapshots.
///
/// The variant says what a caller can do about it, which for the feed loops is the difference
/// between another attempt and giving up. Match on it; the message beside it is diagnostic, is
/// written for a log line, and its wording carries no promise across versions. A distinction
/// worth acting on belongs in a variant, not in the text.
///
/// The payloads are messages rather than source errors: the causes are heterogeneous (HTTP,
/// WebSocket, JSON, protobuf), nothing inspects them, and a library's public error type should
/// not hand its consumers an opaque one to downcast through.
///
/// The quoting layer has an error type of its own; it never reaches a public signature.
#[derive(Clone, Debug, Error)]
pub enum FeedError {
    /// The provider could not be reached, or answered with something a later attempt may not
    /// answer with: a transport failure, a timeout, a server-side error.
    #[error("feed connection error: {0}")]
    Connection(String),
    /// The provider's answer could not be turned into a snapshot.
    #[error("feed parsing error: {0}")]
    Parsing(String),
    /// No attempt can answer differently, so the feed gives up whatever its failure budget says.
    /// A failure someone can fix while the feed keeps retrying is not one of these.
    #[error("feed fatal error: {0}")]
    Fatal(String),
    /// The feed was configured with a value it cannot run with.
    #[error("feed invalid input error: {0}")]
    InvalidInput(String),
}

impl FeedError {
    /// Whether another attempt is pointless. The feed loops stop on these, whatever failure
    /// budget they were configured with.
    pub fn is_fatal(&self) -> bool {
        matches!(self, FeedError::Fatal(_))
    }

    /// The same failure with `context` in front of its message: a caller can say what it was
    /// doing when the failure reached it without reclassifying what went wrong, which stays
    /// the judgement of whoever found out.
    pub(crate) fn in_context(self, context: impl std::fmt::Display) -> Self {
        match self {
            FeedError::Connection(message) => {
                FeedError::Connection(format!("{context}: {message}"))
            }
            FeedError::Parsing(message) => FeedError::Parsing(format!("{context}: {message}")),
            FeedError::Fatal(message) => FeedError::Fatal(format!("{context}: {message}")),
            FeedError::InvalidInput(message) => {
                FeedError::InvalidInput(format!("{context}: {message}"))
            }
        }
    }
}

impl From<FeedError> for SimulationError {
    /// A state that cannot answer from the book it holds reports why in the vocabulary its
    /// caller acts on: a newer book may serve what this one cannot, so only a genuinely fatal
    /// feed error marks the component as one to stop simulating. The message moves across
    /// unchanged — the variant it lands in classifies it, and `SimulationError` says so in its
    /// own `Display`.
    fn from(err: FeedError) -> Self {
        match err {
            FeedError::Connection(message) | FeedError::Parsing(message) => {
                SimulationError::RecoverableError(message)
            }
            FeedError::Fatal(message) => SimulationError::FatalError(message),
            FeedError::InvalidInput(message) => SimulationError::InvalidInput(message, None),
        }
    }
}
