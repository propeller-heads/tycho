//! Shared pieces of the `ProtocolSim` implementations of level-based book states: which way a
//! swap runs through a pair, conversion between atomic and human amounts, and the results a
//! fill against a ladder turns into.

use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
use tycho_common::{
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::book::levels::{Fill, Levels};

/// The direction of a swap relative to a pair's `base`/`quote` orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapDirection {
    /// Sell base, receive quote.
    BaseToQuote,
    /// Sell quote, receive base.
    QuoteToBase,
}

impl SwapDirection {
    /// Which way `token_in -> token_out` runs through the `base`/`quote` pair; `None` when the
    /// tokens are not that pair.
    pub fn detect(
        base: &Bytes,
        quote: &Bytes,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Option<Self> {
        if token_in == base && token_out == quote {
            Some(SwapDirection::BaseToQuote)
        } else if token_in == quote && token_out == base {
            Some(SwapDirection::QuoteToBase)
        } else {
            None
        }
    }

    /// The direction, or the error every book state reports for tokens outside its pair.
    pub fn require(
        base: &Bytes,
        quote: &Bytes,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Self, SimulationError> {
        Self::detect(base, quote, token_in, token_out).ok_or_else(|| {
            SimulationError::InvalidInput(
                format!(
                    "Invalid token addresses. Got in={token_in}, out={token_out}, expected {base} / {quote}"
                ),
                None,
            )
        })
    }

    /// For books that price one direction only: `Ok` when `token_in -> token_out` sells base
    /// for quote, the same error as [`Self::require`] otherwise.
    pub fn require_base_to_quote(
        base: &Bytes,
        quote: &Bytes,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(), SimulationError> {
        match Self::detect(base, quote, token_in, token_out) {
            Some(SwapDirection::BaseToQuote) => Ok(()),
            _ => Err(SimulationError::InvalidInput(
                format!(
                    "Invalid token addresses. Got in={token_in}, out={token_out}, expected in={base}, out={quote}"
                ),
                None,
            )),
        }
    }
}

/// An atomic amount in human units, the units price levels are quoted in.
pub fn to_human(amount: &BigUint, decimals: u32) -> Result<f64, SimulationError> {
    let amount = amount
        .to_f64()
        .ok_or_else(|| SimulationError::RecoverableError("Can't convert amount to f64".into()))?;
    Ok(amount / 10f64.powi(decimals as i32))
}

/// A human amount back in atomic units. Fails for amounts a `BigUint` cannot hold (negative,
/// non-finite).
pub fn to_atomic(amount: f64, decimals: u32) -> Result<BigUint, SimulationError> {
    BigUint::from_f64(amount * 10f64.powi(decimals as i32))
        .ok_or_else(|| SimulationError::RecoverableError("Can't convert amount to BigUint".into()))
}

/// Turns a fill of `amount_in` (human units) into the swap result, with the partially filled
/// result attached to the `InvalidInput` error when the ladder ran out of liquidity.
pub fn fill_result(
    fill: Fill,
    amount_in: f64,
    out_decimals: u32,
    gas: u64,
    new_state: Box<dyn ProtocolSim>,
) -> Result<GetAmountOutResult, SimulationError> {
    let result = GetAmountOutResult {
        amount: to_atomic(fill.amount_out, out_decimals)?,
        gas: BigUint::from(gas),
        new_state,
    };
    if fill.is_complete() {
        Ok(result)
    } else {
        Err(SimulationError::InvalidInput(
            format!(
                "Pool has not enough liquidity to support complete swap. Input amount: {amount_in}, consumed amount: {}",
                amount_in - fill.remaining_in
            ),
            Some(result),
        ))
    }
}

/// The swap limits of a ladder: the input it absorbs in total and the output that buys, in atomic
/// units of the respective tokens.
pub fn limits(
    levels: &Levels,
    in_decimals: u32,
    out_decimals: u32,
) -> Result<(BigUint, BigUint), SimulationError> {
    let (total_in, total_out) = levels.totals();
    Ok((to_atomic(total_in, in_decimals)?, to_atomic(total_out, out_decimals)?))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{book::levels::PriceLevel, evm::decoder::MockProtocolSim};

    fn addr(last: u8) -> Bytes {
        Bytes::from(vec![last; 20])
    }

    #[test]
    fn detects_both_directions_and_rejects_other_tokens() {
        let (base, quote, other) = (addr(1), addr(2), addr(3));

        assert_eq!(
            SwapDirection::detect(&base, &quote, &base, &quote),
            Some(SwapDirection::BaseToQuote)
        );
        assert_eq!(
            SwapDirection::detect(&base, &quote, &quote, &base),
            Some(SwapDirection::QuoteToBase)
        );
        assert_eq!(SwapDirection::detect(&base, &quote, &base, &other), None);
        assert!(matches!(
            SwapDirection::require(&base, &quote, &base, &base),
            Err(SimulationError::InvalidInput(msg, None)) if msg.contains("Invalid token addresses")
        ));
    }

    #[test]
    fn converts_between_atomic_and_human_units() {
        let atomic = BigUint::from_str("1500000").unwrap();

        assert_eq!(to_human(&atomic, 6).unwrap(), 1.5);
        assert_eq!(to_atomic(1.5, 6).unwrap(), atomic);
        assert!(to_atomic(-1.0, 6).is_err());
        assert!(to_atomic(f64::NAN, 6).is_err());
    }

    /// Two levels of one unit each at 2000; the fill of 2 units is complete, the fill of 3 stops
    /// at the ladder's depth.
    fn ladder() -> Levels {
        Levels::new(vec![
            PriceLevel { quantity: 1.0, price: 2_000.0 },
            PriceLevel { quantity: 1.0, price: 2_000.0 },
        ])
        .unwrap()
    }

    fn new_state() -> Box<dyn ProtocolSim> {
        Box::new(MockProtocolSim::new())
    }

    #[test]
    fn fill_result_attaches_the_partial_result_when_the_ladder_runs_out() {
        let ladder = ladder();

        let complete = fill_result(ladder.fill(2.0), 2.0, 6, 100, new_state()).unwrap();
        assert_eq!(complete.amount, BigUint::from(4_000_000_000u64));
        assert_eq!(complete.gas, BigUint::from(100u64));

        let partial = fill_result(ladder.fill(3.0), 3.0, 6, 100, new_state());
        match partial {
            Err(SimulationError::InvalidInput(message, Some(result))) => {
                assert!(message.contains("consumed amount: 2"));
                assert_eq!(result.amount, BigUint::from(4_000_000_000u64));
            }
            other => panic!("expected the partially filled result, got {other:?}"),
        }
    }

    #[test]
    fn limits_are_the_ladder_totals_in_atomic_units() {
        let ladder = Levels::new(vec![
            PriceLevel { price: 2.0, quantity: 1.5 },
            PriceLevel { price: 3.0, quantity: 0.5 },
        ])
        .unwrap();

        let (sell, buy) = limits(&ladder, 6, 2).unwrap();

        assert_eq!(sell, BigUint::from(2_000_000u64));
        assert_eq!(buy, BigUint::from(450u64));
    }
}
