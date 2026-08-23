//! Failures the ported Pendle math can produce.
//!
//! Every variant corresponds to a `require` or a revert in the Solidity. The mapping onto
//! `SimulationError` lives with the state, not here — this layer stays free of tycho types so the
//! math can be tested on its own.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendleError {
    /// `LogExpMath.exp` outside `[-41e18, 130e18]`.
    ExponentOutOfBounds { x: String },
    /// `LogExpMath.ln` at or below zero, where the logarithm is undefined.
    LogarithmOutOfBounds { a: String },
    /// `PMath.subNoNeg`: a subtraction that would go negative where the contract forbids it.
    NegativeResult { a: String, b: String },
    /// A cast the contract guards with `require` — `PMath.Int` or `PMath.Uint`.
    CastOutOfRange { value: String, target: &'static str },
    /// Arithmetic that would exceed 256 bits. The Solidity runs `unchecked` and would wrap; in
    /// the domain the contract accepts, neither happens, so reaching this means the input was
    /// outside that domain.
    Overflow { operation: &'static str },
    /// Division by zero.
    DivisionByZero { operation: &'static str },
}

impl fmt::Display for PendleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PendleError::ExponentOutOfBounds { x } => write!(
                f,
                "exp({x}) is outside the supported domain [-41e18, 130e18]; the contract reverts \
                 with \"Invalid exponent\""
            ),
            PendleError::LogarithmOutOfBounds { a } => {
                write!(f, "ln({a}) is undefined; the argument must be strictly positive")
            }
            PendleError::NegativeResult { a, b } => {
                write!(f, "{a} - {b} would be negative, which subNoNeg forbids")
            }
            PendleError::CastOutOfRange { value, target } => {
                write!(f, "{value} does not fit in {target}")
            }
            PendleError::Overflow { operation } => {
                write!(f, "{operation} overflowed 256 bits")
            }
            PendleError::DivisionByZero { operation } => {
                write!(f, "{operation} divided by zero")
            }
        }
    }
}

impl std::error::Error for PendleError {}

pub type PendleResult<T> = Result<T, PendleError>;
