//! Shared pieces of the `ProtocolSim` implementations of level-based book states: which way a
//! swap runs through a pair, conversion between atomic and human amounts, and the results a
//! fill against a ladder turns into.

use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
use tycho_common::{
    models::token::Token,
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
    fn detect(base: &Bytes, quote: &Bytes, token_in: &Bytes, token_out: &Bytes) -> Option<Self> {
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
        base: &Token,
        quote: &Token,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Self, SimulationError> {
        let (base, quote) = (&base.address, &quote.address);
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
        base: &Token,
        quote: &Token,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(), SimulationError> {
        let (base, quote) = (&base.address, &quote.address);
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

    /// The decimals of the token sold and the token bought by a swap in this direction through
    /// the `base`/`quote` pair.
    pub fn decimals(self, base: &Token, quote: &Token) -> (u32, u32) {
        match self {
            SwapDirection::BaseToQuote => (base.decimals, quote.decimals),
            SwapDirection::QuoteToBase => (quote.decimals, base.decimals),
        }
    }
}

/// An atomic amount in human units, the units price levels are quoted in. An amount past
/// `f64::MAX` becomes infinity, which no ladder absorbs, so its fill reports the whole amount as
/// unfilled.
pub fn to_human(amount: &BigUint, decimals: u32) -> f64 {
    let amount = amount
        .to_f64()
        .expect("BigUint::to_f64 saturates to infinity instead of failing");
    amount / 10f64.powi(decimals as i32)
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

    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{book::levels::PriceLevel, evm::decoder::MockProtocolSim};

    fn addr(last: u8) -> Bytes {
        Bytes::from(vec![last; 20])
    }

    fn token(last: u8, decimals: u32) -> Token {
        Token::new(&addr(last), &format!("T{last}"), decimals, 0, &[], Chain::Ethereum, 100)
    }

    #[test]
    fn detects_both_directions_and_rejects_other_tokens() {
        let (base, quote, other) = (token(1, 18), token(2, 6), addr(3));

        assert_eq!(
            SwapDirection::require(&base, &quote, &base.address, &quote.address).unwrap(),
            SwapDirection::BaseToQuote
        );
        assert_eq!(
            SwapDirection::require(&base, &quote, &quote.address, &base.address).unwrap(),
            SwapDirection::QuoteToBase
        );
        assert!(matches!(
            SwapDirection::require(&base, &quote, &base.address, &other),
            Err(SimulationError::InvalidInput(msg, None)) if msg.contains("Invalid token addresses")
        ));
        assert!(matches!(
            SwapDirection::require_base_to_quote(&base, &quote, &quote.address, &base.address),
            Err(SimulationError::InvalidInput(msg, None)) if msg.contains("Invalid token addresses")
        ));
    }

    #[test]
    fn the_direction_says_which_decimals_are_sold_and_bought() {
        let (base, quote) = (token(1, 18), token(2, 6));

        assert_eq!(SwapDirection::BaseToQuote.decimals(&base, &quote), (18, 6));
        assert_eq!(SwapDirection::QuoteToBase.decimals(&base, &quote), (6, 18));
    }

    #[test]
    fn converts_between_atomic_and_human_units() {
        let atomic = BigUint::from_str("1500000").unwrap();

        assert_eq!(to_human(&atomic, 6), 1.5);
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

    /// What `get_limits` promises, `get_amount_out` must be able to fill: the limit goes out in
    /// atomic units and comes back through `to_human`, and neither conversion may leave the
    /// caller a hair short of a complete fill.
    ///
    /// The single level is a measured case where the round trip alone overshoots: 75559.18038262217
    /// atomic-ises and comes back as 75559.18038262219, just under one ulp high.
    #[rstest]
    #[case::eighteen_decimals(18, &[1.1, 2.2, 3.3, 4.4, 5.5, 6.6, 7.7, 8.8, 9.9, 48.508172])]
    #[case::six_decimals(6, &[1.1, 2.2, 3.3, 4.4, 5.5, 6.6, 7.7, 8.8, 9.9, 48.508172])]
    #[case::two_decimals(2, &[1.1, 2.2, 3.3, 4.4, 5.5, 6.6, 7.7, 8.8, 9.9, 48.508172])]
    #[case::round_trip_overshoots(18, &[75559.18038262217])]
    fn the_reported_limit_fills_completely(#[case] decimals: u32, #[case] quantities: &[f64]) {
        let ladder = Levels::new(
            quantities
                .iter()
                .map(|&quantity| PriceLevel { price: 1.5, quantity })
                .collect(),
        )
        .unwrap();

        let (sell_limit, _) = limits(&ladder, decimals, 6).unwrap();
        let fill = ladder.fill(to_human(&sell_limit, decimals));

        assert!(fill.is_complete(), "{decimals} decimals left {} unfilled", fill.remaining_in);
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
