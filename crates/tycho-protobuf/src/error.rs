use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum DecodeError {
    #[error("Failed to decode: {0}")]
    Decode(String),
    #[error("Empty protobuf message")]
    Empty,
}
