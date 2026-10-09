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
        let message = err.to_string();
        match err {
            RFQError::ConnectionError(_) |
            RFQError::ParsingError(_) |
            RFQError::QuoteNotFound(_) => SimulationError::RecoverableError(message),
            RFQError::FatalError(_) => SimulationError::FatalError(message),
            RFQError::InvalidInput(_) => SimulationError::InvalidInput(message, None),
        }
    }
}
