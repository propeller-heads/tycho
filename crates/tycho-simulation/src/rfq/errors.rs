use thiserror::Error;
use tycho_common::simulation::errors::SimulationError;

#[derive(Debug, Error)]
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
        let msg = err.to_string();
        match err {
            // A venue that has no quote for these tokens at this size, or that could not be
            // reached, says nothing about the pool being broken - the same request may succeed on
            // the next block or at a smaller size.
            RFQError::QuoteNotFound(_) | RFQError::ConnectionError(_) => {
                SimulationError::RecoverableError(msg)
            }
            RFQError::InvalidInput(_) => SimulationError::InvalidInput(msg, None),
            RFQError::ParsingError(_) | RFQError::FatalError(_) => SimulationError::FatalError(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulation_error_classification() {
        assert!(matches!(
            SimulationError::from(RFQError::QuoteNotFound("no depth".into())),
            SimulationError::RecoverableError(_)
        ));
        assert!(matches!(
            SimulationError::from(RFQError::ConnectionError("timed out".into())),
            SimulationError::RecoverableError(_)
        ));
        assert!(matches!(
            SimulationError::from(RFQError::InvalidInput("missing key".into())),
            SimulationError::InvalidInput(_, None)
        ));
        assert!(matches!(
            SimulationError::from(RFQError::FatalError("unsupported chain".into())),
            SimulationError::FatalError(_)
        ));
        assert!(matches!(
            SimulationError::from(RFQError::ParsingError("bad json".into())),
            SimulationError::FatalError(_)
        ));
    }
}
