use std::{any::Any, collections::HashMap};

use num_bigint::BigUint;
use num_traits::{CheckedSub, ToPrimitive};
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

/// A single price level: the total `amount_out` a swap of exactly `amount_in` would deliver.
///
/// Levels are absolute quotes, not marginal order book sizes: each one already includes all
/// smaller levels' liquidity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLevelStreamQuote {
    pub amount_in: BigUint,
    pub amount_out: BigUint,
}

impl PriceLevelStreamQuote {
    pub fn new(amount_in: BigUint, amount_out: BigUint) -> Self {
        Self { amount_in, amount_out }
    }
}

/// State of a single pAMM pair fed from the price level stream.
///
/// Holds the latest complete quote ladders for both trade directions. Quotes are absolute
/// (`amount_in` → total `amount_out`), sorted ascending by `amount_in`; amounts between two
/// quotes are interpolated linearly, mirroring how Titan itself densifies the simulated levels.
/// Amounts outside the quoted range are not served.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevelStreamState {
    pub token0: Bytes,
    pub token1: Bytes,
    pub quotes_0_to_1: Vec<PriceLevelStreamQuote>,
    pub quotes_1_to_0: Vec<PriceLevelStreamQuote>,
    pub gas_cost: BigUint,
}

impl PriceLevelStreamState {
    /// Creates the state for the pair `(token0, token1)`.
    ///
    /// Both quote ladders are sorted ascending by `amount_in` and deduplicated on it, so callers
    /// may pass them in stream order.
    pub fn new(
        token0: Bytes,
        token1: Bytes,
        mut quotes_0_to_1: Vec<PriceLevelStreamQuote>,
        mut quotes_1_to_0: Vec<PriceLevelStreamQuote>,
        gas_cost: BigUint,
    ) -> Self {
        for quotes in [&mut quotes_0_to_1, &mut quotes_1_to_0] {
            quotes.sort_by(|a, b| a.amount_in.cmp(&b.amount_in));
            quotes.dedup_by(|a, b| a.amount_in == b.amount_in);
        }
        Self { token0, token1, quotes_0_to_1, quotes_1_to_0, gas_cost }
    }

    /// Returns the quote ladder selling `token_in` for `token_out`, or an error if the pair does
    /// not match this state's tokens.
    fn quotes(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<&[PriceLevelStreamQuote], SimulationError> {
        if token_in == &self.token0 && token_out == &self.token1 {
            Ok(&self.quotes_0_to_1)
        } else if token_in == &self.token1 && token_out == &self.token0 {
            Ok(&self.quotes_1_to_0)
        } else {
            Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses for pair {}/{}: {token_in}, {token_out}",
                self.token0, self.token1
            )))
        }
    }

    /// Computes the output amount for `amount_in` on the given ladder by linear interpolation
    /// between the two enclosing quotes.
    ///
    /// Callers must ensure `amount_in` lies within the quoted range (smallest to largest
    /// `amount_in`) — the ladder holds no information outside of it. Errors if the enclosing
    /// quotes are not monotonically increasing in `amount_out`: such a ladder is unreliable,
    /// and a venue with corrupt data should not be quoted at any price.
    fn interpolate(
        &self,
        quotes: &[PriceLevelStreamQuote],
        amount_in: &BigUint,
    ) -> Result<BigUint, SimulationError> {
        // First quote with amount_in >= the requested amount; the caller-guaranteed range makes
        // both it and (when needed) its predecessor exist.
        let idx = quotes.partition_point(|quote| &quote.amount_in < amount_in);
        let upper = &quotes[idx];
        if &upper.amount_in == amount_in {
            return Ok(upper.amount_out.clone());
        }
        let lower = &quotes[idx - 1];
        let Some(out_span) = upper
            .amount_out
            .checked_sub(&lower.amount_out)
        else {
            // Recoverable: the next snapshot replaces the ladder wholesale.
            return Err(SimulationError::RecoverableError(format!(
                "Quote ladder {}/{} is not monotonically increasing in amount_out around the \
                 requested amount {amount_in}: {} -> {}, but {} -> {}",
                self.token0,
                self.token1,
                lower.amount_in,
                lower.amount_out,
                upper.amount_in,
                upper.amount_out,
            )));
        };
        let in_span = &upper.amount_in - &lower.amount_in;
        let offset = amount_in - &lower.amount_in;
        Ok(&lower.amount_out + out_span * offset / in_span)
    }

    /// The state after a fill: both ladders are consumed. The snapshot quotes fills of the
    /// pre-fill venue only — post-fill pricing is unknown in either direction until the next
    /// snapshot, and re-reading the cumulative ladder would double-count the maker's liquidity.
    fn consumed(&self) -> Box<dyn ProtocolSim> {
        Box::new(Self {
            token0: self.token0.clone(),
            token1: self.token1.clone(),
            quotes_0_to_1: Vec::new(),
            quotes_1_to_0: Vec::new(),
            gas_cost: self.gas_cost.clone(),
        })
    }
}

#[typetag::serde]
impl ProtocolSim for PriceLevelStreamState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let quotes = self.quotes(&base.address, &quote.address)?;
        let best = quotes
            .iter()
            .find(|q| q.amount_in > BigUint::ZERO && q.amount_out > BigUint::ZERO)
            .ok_or_else(|| {
                SimulationError::RecoverableError("No liquidity available".to_string())
            })?;
        let amount_in = best.amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".to_string())
        })?;
        let amount_out = best
            .amount_out
            .to_f64()
            .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert amount out to f64".to_string())
            })?;
        Ok((amount_out / 10f64.powi(quote.decimals as i32)) /
            (amount_in / 10f64.powi(base.decimals as i32)))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let quotes = self.quotes(&token_in.address, &token_out.address)?;
        let (Some(first), Some(last)) = (quotes.first(), quotes.last()) else {
            return Err(SimulationError::RecoverableError("No liquidity available".to_string()));
        };
        // Below the smallest quote nothing is served. The venue itself could still fill —
        // FermiSwap was observed quoting below the smallest streamed level — but if so, on its
        // own price curve, which the ladder holds no information about: unlike interpolation
        // between two quoted levels, whose result is bracketed by genuine samples on both
        // sides, extrapolating linearly from (0, 0) like Titan's quote API does is an unbounded
        // guess even off a healthy ladder. (0, 0) is an assumption, not a sample, and the
        // bottom of that line is not even fillable: FermiSwap reverts below a venue-side
        // minimum (~$0.02 at the time of measurement) where the API keeps quoting. And a
        // malformed ladder turns the guess absurd: on FermiSwap's flat cbBTC-input books
        // (18-decimals grid bug) the smallest quote is the venue's depth clamp, not a price
        // sample, and the extrapolated quotes land ~3e6x below the venue's own (measured
        // 2026-07). All the rejection gives up are trades smaller than the first level —
        // ~$10-30 on healthy ladders, negligible for routing. Hence no partial result either.
        if amount_in < first.amount_in {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Input amount is below the smallest quote. input amount: {amount_in}, minimum quoted amount: {}",
                    first.amount_in
                ),
                None,
            ));
        }
        // The requested amount exceeds the largest quote; report the output at the limit as a
        // partial result, like other level-based protocols do.
        if amount_in > last.amount_in {
            let res = GetAmountOutResult {
                amount: last.amount_out.clone(),
                gas: self.gas_cost.clone(),
                new_state: self.consumed(),
            };
            return Err(SimulationError::InvalidInput(
                format!(
                    "Not enough liquidity to support complete swap. input amount: {amount_in}, maximum quoted amount: {}",
                    last.amount_in
                ),
                Some(res),
            ));
        }
        Ok(GetAmountOutResult {
            amount: self.interpolate(quotes, &amount_in)?,
            gas: self.gas_cost.clone(),
            new_state: self.consumed(),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let quotes = self.quotes(&sell_token, &buy_token)?;
        match quotes.last() {
            Some(largest) => Ok((largest.amount_in.clone(), largest.amount_out.clone())),
            None => Ok((BigUint::ZERO, BigUint::ZERO)),
        }
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError("Not implemented".into()))
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .is_some_and(|other| {
                let Self { token0, token1, quotes_0_to_1, quotes_1_to_0, gas_cost } = other;
                &self.token0 == token0 &&
                    &self.token1 == token1 &&
                    &self.quotes_0_to_1 == quotes_0_to_1 &&
                    &self.quotes_1_to_0 == quotes_1_to_0 &&
                    &self.gas_cost == gas_cost
            })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::Chain;

    use super::*;

    fn wbtc() -> Token {
        Token::new(
            &Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap(),
            "WBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn weth() -> Token {
        Token::new(
            &Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn quote(amount_in: u64, amount_out: u64) -> PriceLevelStreamQuote {
        PriceLevelStreamQuote::new(BigUint::from(amount_in), BigUint::from(amount_out))
    }

    /// WBTC (token0) / USDC (token1) ladder: 1 WBTC -> 100k USDC flat, then the second level
    /// fills at a worse marginal price.
    fn state() -> PriceLevelStreamState {
        PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![quote(100_000_000, 100_000_000_000), quote(200_000_000, 190_000_000_000)],
            vec![quote(100_000_000_000, 99_000_000), quote(200_000_000_000, 190_000_000)],
            BigUint::from(120_000u64),
        )
    }

    #[test]
    fn new_sorts_and_dedups_quotes() {
        let state = PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![quote(200, 380), quote(100, 200), quote(200, 999)],
            vec![],
            BigUint::ZERO,
        );
        assert_eq!(state.quotes_0_to_1, vec![quote(100, 200), quote(200, 380)]);
    }

    #[test]
    fn get_amount_out_exact_level() {
        let result = state()
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(100_000_000_000u64));
        assert_eq!(result.gas, BigUint::from(120_000u64));
    }

    #[test]
    fn get_amount_out_interpolates_between_levels() {
        // Halfway between the two levels: 100k + (190k - 100k) / 2 = 145k USDC.
        let result = state()
            .get_amount_out(BigUint::from(150_000_000u64), &wbtc(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(145_000_000_000u64));
    }

    #[test]
    fn get_amount_out_on_glitched_ladder_is_rejected() {
        // A ladder that is not monotonically increasing in amount_out (a stream glitch): the
        // data is unreliable, so a quote landing in the glitched segment is refused instead of
        // interpolated (or underflowing).
        let state = PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![quote(100, 200), quote(200, 150)],
            vec![],
            BigUint::ZERO,
        );
        let result = state.get_amount_out(BigUint::from(150u64), &wbtc(), &usdc());
        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));

        // Hitting a level exactly returns that genuine sample even on a glitched ladder.
        let result = state
            .get_amount_out(BigUint::from(100u64), &wbtc(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(200u64));
    }

    #[test]
    fn get_amount_out_below_smallest_level_is_rejected() {
        // The ladder holds no information below its smallest quote, and there is no partial
        // result to offer.
        let result = state().get_amount_out(BigUint::from(50_000_000u64), &wbtc(), &usdc());
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn get_amount_out_reverse_direction() {
        let result = state()
            .get_amount_out(BigUint::from(100_000_000_000u64), &usdc(), &wbtc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(99_000_000u64));
    }

    #[test]
    fn get_amount_out_beyond_largest_level_is_partial() {
        let result = state().get_amount_out(BigUint::from(300_000_000u64), &wbtc(), &usdc());
        match result {
            Err(SimulationError::InvalidInput(_, Some(partial))) => {
                assert_eq!(partial.amount, BigUint::from(190_000_000_000u64));
            }
            other => panic!("expected partial InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn get_amount_out_consumes_both_ladders() {
        let result = state()
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .unwrap();
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert!(new_state.quotes_0_to_1.is_empty());
        assert!(new_state.quotes_1_to_0.is_empty());
    }

    #[test]
    fn get_amount_out_rejects_unknown_tokens() {
        let result = state().get_amount_out(BigUint::from(1u64), &weth(), &usdc());
        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn get_amount_out_without_liquidity() {
        let state = PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![],
            vec![],
            BigUint::ZERO,
        );
        let result = state.get_amount_out(BigUint::from(1u64), &wbtc(), &usdc());
        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn spot_price_uses_smallest_quote() {
        // 1 WBTC (1e8) -> 100_000 USDC (1e11 at 6 decimals).
        let price = state()
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert!((price - 100_000.0).abs() < 1e-9);

        let inverse = state()
            .spot_price(&usdc(), &wbtc())
            .unwrap();
        // 100k USDC (1e11 at 6 decimals) -> 0.99 WBTC: 0.99 / 100_000 = 9.9e-6.
        assert!((inverse - 9.9e-6).abs() < 1e-15);
    }

    #[test]
    fn spot_price_skips_zero_amount_out_quotes() {
        // A dust level rounding to zero output must not produce a spot price of 0 — consumers
        // computing 1/spot_price would divide by zero.
        let state = PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![quote(1, 0), quote(100_000_000, 100_000_000_000)],
            vec![],
            BigUint::ZERO,
        );
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert!((price - 100_000.0).abs() < 1e-9);
    }

    #[test]
    fn get_limits_returns_largest_quote() {
        let (max_in, max_out) = state()
            .get_limits(wbtc().address, usdc().address)
            .unwrap();
        assert_eq!(max_in, BigUint::from(200_000_000u64));
        assert_eq!(max_out, BigUint::from(190_000_000_000u64));
    }

    #[test]
    fn get_limits_without_liquidity() {
        let state = PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![],
            vec![],
            BigUint::ZERO,
        );
        let (max_in, max_out) = state
            .get_limits(wbtc().address, usdc().address)
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);
        assert_eq!(max_out, BigUint::ZERO);
    }

    #[test]
    fn eq_compares_quotes() {
        let a = state();
        let mut b = state();
        assert!(a.eq(&b as &dyn ProtocolSim));
        b.quotes_0_to_1[0].amount_out += 1u32;
        assert!(!a.eq(&b as &dyn ProtocolSim));
    }
}
