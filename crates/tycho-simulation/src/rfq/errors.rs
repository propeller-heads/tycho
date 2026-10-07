use thiserror::Error;
use tycho_common::simulation::errors::SimulationError;

#[derive(Clone, Debug, Error)]
pub enum RFQError {
    #[error("RFQ connection error: {0}")]
    ConnectionError(String),
    #[error("RFQ parsing error: {0}")]
    ParsingError(String),
    #[error("RFQ fatal error: {0}")]
    FatalError(String),
    #[error("RFQ invalid input error: {0}")]
    InvalidInput(String),
    #[error("RFQ quote not found error: {0}")]
    QuoteNotFound(String),
}

impl From<reqwest::Error> for RFQError {
    fn from(err: reqwest::Error) -> Self {
        RFQError::ConnectionError(err.to_string())
    }
}

impl From<RFQError> for SimulationError {
    fn from(err: RFQError) -> Self {
        SimulationError::FatalError(err.to_string())
    }
}
