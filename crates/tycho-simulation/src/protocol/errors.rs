//! Protocol generic errors
use std::io;

use serde_json::Error as SerdeError;
use thiserror::Error;
use tycho_common::{hex_bytes::IntegerFromBytesError, simulation::errors::SimulationError};

#[derive(Debug, Error)]
pub enum InvalidSnapshotError {
    #[error("Missing attributes {0}")]
    MissingAttribute(String),
    #[error("Value error {0}")]
    ValueError(String),
    #[error("Unable to set up vm state on the engine: {0}")]
    VMError(SimulationError),
}

impl From<SimulationError> for InvalidSnapshotError {
    fn from(error: SimulationError) -> Self {
        InvalidSnapshotError::VMError(error)
    }
}

impl From<IntegerFromBytesError> for InvalidSnapshotError {
    fn from(error: IntegerFromBytesError) -> Self {
        InvalidSnapshotError::ValueError(error.to_string())
    }
}

impl From<FileError> for SimulationError {
    fn from(error: FileError) -> Self {
        SimulationError::FatalError(error.to_string())
    }
}

#[derive(Debug, Error)]
pub enum FileError {
    /// Occurs when the ABI file cannot be read
    #[error("Malformed ABI error: {0}")]
    MalformedABI(String),
    /// Occurs when the parent directory of the current file cannot be retrieved
    #[error("Structure error {0}")]
    Structure(String),
    /// Occurs when a bad file path was given, which cannot be converted to string.
    #[error("File path conversion error {0}")]
    FilePath(String),
    #[error("I/O error {0}")]
    Io(io::Error),
    #[error("Json parsing error {0}")]
    Parse(SerdeError),
}

impl From<io::Error> for FileError {
    fn from(err: io::Error) -> Self {
        FileError::Io(err)
    }
}

impl From<SerdeError> for FileError {
    fn from(err: SerdeError) -> Self {
        FileError::Parse(err)
    }
}
