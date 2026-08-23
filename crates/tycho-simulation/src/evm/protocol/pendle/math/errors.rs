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

    // The market's own failure modes. Each mirrors a custom error in `Errors.sol`, and the names
    // are kept so a reader can match a quote failure against the revert the contract would give.
    /// `blockTime >= expiry`. The market no longer trades; PT redeems through the YT instead.
    MarketExpired { expiry: u64, block_time: u64 },
    /// More PT was asked of the market than it holds.
    MarketInsufficientPtForTrade { total_pt: String, required: String },
    /// The trade would push the PT proportion past 96%.
    MarketProportionTooHigh { proportion: String, max: String },
    /// The post-trade exchange rate would fall below par, which PT may never do.
    MarketExchangeRateBelowOne { rate: String },
    /// An empty market: nothing to price against.
    MarketZeroTotalPtOrTotalAsset { total_pt: String, total_asset: String },
    /// A proportion of exactly one, where the logit is undefined.
    MarketProportionMustNotEqualOne,
    /// `rateScalar` at or below zero, which only happens past expiry.
    MarketRateScalarBelowZero { rate_scalar: String },
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
            PendleError::MarketExpired { expiry, block_time } => write!(
                f,
                "market expired at {expiry} and the quote is for {block_time}; PT redeems through \
                 the yield token from here, it does not trade"
            ),
            PendleError::MarketInsufficientPtForTrade { total_pt, required } => {
                write!(f, "market holds {total_pt} PT but the trade needs {required}")
            }
            PendleError::MarketProportionTooHigh { proportion, max } => {
                write!(f, "trade would take the PT proportion to {proportion}, past the {max} cap")
            }
            PendleError::MarketExchangeRateBelowOne { rate } => write!(
                f,
                "trade would put the exchange rate at {rate}, below par; PT cannot be priced above \
                 its redemption value"
            ),
            PendleError::MarketZeroTotalPtOrTotalAsset { total_pt, total_asset } => {
                write!(f, "market is empty: totalPt {total_pt}, totalAsset {total_asset}")
            }
            PendleError::MarketProportionMustNotEqualOne => {
                write!(f, "a PT proportion of exactly one has no defined logit")
            }
            PendleError::MarketRateScalarBelowZero { rate_scalar } => {
                write!(f, "rateScalar {rate_scalar} is not positive")
            }
        }
    }
}

impl std::error::Error for PendleError {}

pub type PendleResult<T> = Result<T, PendleError>;
