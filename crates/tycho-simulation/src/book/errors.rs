use thiserror::Error;
use tycho_common::simulation::errors::SimulationError;

/// Failures of the book-feed layer: fetching, decoding, and publishing provider books.
///
/// Quoting failures are a different layer — see `rfq::errors::RFQError`.
#[derive(Clone, Debug, Error)]
pub enum FeedError {
    #[error("feed connection error: {0}")]
    ConnectionError(String),
    #[error("feed parsing error: {0}")]
    ParsingError(String),
    #[error("feed fatal error: {0}")]
    FatalError(String),
    #[error("feed invalid input error: {0}")]
    InvalidInput(String),
}

impl From<reqwest::Error> for FeedError {
    fn from(err: reqwest::Error) -> Self {
        FeedError::ConnectionError(err.to_string())
    }
}

impl From<FeedError> for SimulationError {
    fn from(err: FeedError) -> Self {
        SimulationError::FatalError(err.to_string())
    }
}
