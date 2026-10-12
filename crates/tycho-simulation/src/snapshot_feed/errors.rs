//! What a feed reports when a snapshot does not arrive.

use thiserror::Error;
use tycho_common::simulation::errors::SimulationError;

/// Failures of the feed layer: fetching, decoding, and publishing a provider's snapshots.
///
/// Match on the variant: it says whether another attempt could answer differently, which is what
/// a feed's retrying and the conversion to [`SimulationError`] both turn on. The message beside
/// it is written for a log line, and its wording carries no promise across versions.
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
    /// Whether another attempt is pointless. A feed stops on these, whatever failure budget it
    /// was configured with.
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
