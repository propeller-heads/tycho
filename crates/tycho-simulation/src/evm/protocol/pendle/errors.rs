//! Mapping the ported math's failures onto tycho's three-variant `SimulationError`.
//!
//! `SimulationError` has exactly three variants, so several distinct Pendle failures share one.
//! The variant chosen carries the meaning a caller acts on:
//!
//! - **`InvalidInput`** — the caller asked for something this pool cannot do at this size. A router
//!   should try a smaller amount or a different venue. The variant can carry a suggested
//!   `GetAmountOutResult`, but Pendle never fills a smaller amount on the caller's behalf: the
//!   right size comes from `get_limits`, which is exact here, so the suggestion is always `None`.
//! - **`RecoverableError`** — the state is stale or incomplete; a fresh snapshot may fix it.
//! - **`FatalError`** — the request is malformed, or the state is structurally wrong. Retrying
//!   changes nothing.
//!
//! Expiry is `FatalError` on purpose: an expired market never trades again, so there is no amount
//! and no later block at which the same call succeeds. The router should drop the component.

use tycho_common::simulation::errors::SimulationError;

use super::math::errors::PendleError;

impl From<PendleError> for SimulationError {
    fn from(error: PendleError) -> Self {
        let message = error.to_string();
        match error {
            // Past expiry the market is gone for good.
            PendleError::MarketExpired { .. } => SimulationError::FatalError(message),

            // The trade is too big for this market in this direction. `get_limits` exists to keep
            // a router from reaching these, but a caller that ignores it gets a usable answer.
            PendleError::MarketInsufficientPtForTrade { .. } |
            PendleError::MarketProportionTooHigh { .. } |
            PendleError::MarketExchangeRateBelowOne { .. } |
            PendleError::ApproxRangeOverflow |
            PendleError::ApproxExhausted { .. } => SimulationError::InvalidInput(message, None),

            // Too small to price, which is also the caller's input.
            PendleError::ApproxRangeUnderflow => SimulationError::InvalidInput(message, None),

            // An empty market has nothing to quote against, but it may be seeded later.
            PendleError::MarketZeroTotalPtOrTotalAsset { .. } => {
                SimulationError::RecoverableError(message)
            }

            // Everything below means the state or the arithmetic is wrong, not the request.
            PendleError::ExponentOutOfBounds { .. } |
            PendleError::LogarithmOutOfBounds { .. } |
            PendleError::NegativeResult { .. } |
            PendleError::CastOutOfRange { .. } |
            PendleError::Overflow { .. } |
            PendleError::DivisionByZero { .. } |
            PendleError::MarketProportionMustNotEqualOne |
            PendleError::MarketRateScalarBelowZero { .. } |
            PendleError::ApproxInvalidBounds { .. } => SimulationError::FatalError(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expiry is terminal. Reporting it as recoverable would have a router re-request the same
    /// dead market on every block.
    #[test]
    fn expiry_is_fatal_not_recoverable() {
        let error: SimulationError =
            PendleError::MarketExpired { expiry: 100, block_time: 200 }.into();
        assert!(matches!(error, SimulationError::FatalError(_)));
    }

    /// Depth failures are the caller's input, so a router can respond by trying less.
    #[test]
    fn depth_failures_are_invalid_input() {
        for error in [
            PendleError::ApproxRangeOverflow,
            PendleError::MarketProportionTooHigh { proportion: "1".into(), max: "0".into() },
            PendleError::MarketExchangeRateBelowOne { rate: "0".into() },
            PendleError::MarketInsufficientPtForTrade {
                total_pt: "1".into(),
                required: "2".into(),
            },
        ] {
            let mapped: SimulationError = error.clone().into();
            assert!(
                matches!(mapped, SimulationError::InvalidInput(_, _)),
                "{error:?} should be InvalidInput"
            );
        }
    }

    /// An empty market may be seeded on a later block, so it is worth re-reading.
    #[test]
    fn an_empty_market_is_recoverable() {
        let error: SimulationError = PendleError::MarketZeroTotalPtOrTotalAsset {
            total_pt: "0".into(),
            total_asset: "0".into(),
        }
        .into();
        assert!(matches!(error, SimulationError::RecoverableError(_)));
    }

    /// Arithmetic that should never happen is fatal: retrying cannot help.
    #[test]
    fn arithmetic_failures_are_fatal() {
        let error: SimulationError = PendleError::Overflow { operation: "test" }.into();
        assert!(matches!(error, SimulationError::FatalError(_)));
    }

    /// The message survives the mapping, so a log line says what actually went wrong rather than
    /// only which of the three buckets it fell into.
    #[test]
    fn the_reason_survives_the_mapping() {
        let error: SimulationError =
            PendleError::MarketExpired { expiry: 1830124800, block_time: 1830124801 }.into();
        let SimulationError::FatalError(message) = error else { panic!("wrong variant") };
        assert!(message.contains("1830124800"), "{message}");
        assert!(message.contains("expired"), "{message}");
    }
}
