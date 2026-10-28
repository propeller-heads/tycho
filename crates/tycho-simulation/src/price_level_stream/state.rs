use std::{
    any::Any,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

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

use super::{overrides::PammSimulationOverrides, telemetry};

/// How long the quotes of one price level message stay quotable: one slot after the message's `ts`.
/// Titan prices the block it is building, so quotes older than one slot were priced for a block
/// already built. The guard is loose on purpose: the builder usually refreshes a venue before
/// inclusion, so a quote for the previous block often still fills, and the fallback router covers
/// the cases where it does not.
pub const QUOTE_TTL: Duration = super::SLOT;

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
/// Holds the latest complete quotes for both trade directions. Quotes are absolute
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
    /// Monotonic instant from which this state refuses every query. `None` means the state never
    /// expires. The field is never serialized and deserializes as `None`, so a replayed
    /// recording always quotes. Only [`with_quotable_until`](Self::with_quotable_until) sets it,
    /// and equality ignores it: two states carrying the same quotes compare equal whichever
    /// messages they came from.
    #[serde(skip)]
    quotable_until: Option<Instant>,
    /// The account state Titan priced these quotes on, when the tracker matched a quote stream
    /// entry to them. An `Arc`, because every state a quote returns keeps it. Unlike
    /// `quotable_until` it is serialized, so a recording replays it;
    /// [`with_simulation_overrides`](Self::with_simulation_overrides) is the only setter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overrides: Option<Arc<PammSimulationOverrides>>,
}

impl PriceLevelStreamState {
    /// Creates the state for the pair `(token0, token1)`.
    ///
    /// Both quote lists are sorted ascending by `amount_in` and deduplicated on it, so callers
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
        Self {
            token0,
            token1,
            quotes_0_to_1,
            quotes_1_to_0,
            gas_cost,
            quotable_until: None,
            overrides: None,
        }
    }

    /// Sets the monotonic instant from which this state refuses every query.
    pub fn with_quotable_until(mut self, until: Instant) -> Self {
        self.quotable_until = Some(until);
        self
    }

    /// Attaches the account state Titan priced these quotes on.
    pub fn with_simulation_overrides(
        mut self,
        overrides: impl Into<Arc<PammSimulationOverrides>>,
    ) -> Self {
        self.overrides = Some(overrides.into());
        self
    }

    /// The account state Titan priced these quotes on, to convert and pass to `eth_call` as state
    /// overrides, or `None` when the stream matched no quote stream entry to this state's book, or
    /// runs without simulation overrides. Quoting never reads it.
    pub fn simulation_overrides(&self) -> Option<&PammSimulationOverrides> {
        self.overrides.as_deref()
    }

    /// The monotonic instant from which this state refuses every query, or `None` if it never
    /// expires.
    pub fn quotable_until(&self) -> Option<Instant> {
        self.quotable_until
    }

    /// Returns a `RecoverableError` once `now` reaches `quotable_until`. A state without
    /// `quotable_until` never errors.
    fn ensure_quotable(&self, now: Instant) -> Result<(), SimulationError> {
        match self.quotable_until {
            Some(until) if now >= until => {
                telemetry::record_quote_refused(telemetry::QuoteRefusedReason::PriceLevelsExpired);
                Err(SimulationError::RecoverableError(format!(
                    "price levels expired: the message that carried them is older than {} s (one \
                     slot)",
                    QUOTE_TTL.as_secs()
                )))
            }
            Some(_) | None => Ok(()),
        }
    }

    /// Returns the quotes selling `token_in` for `token_out`, or an error if the pair does
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
            telemetry::record_quote_refused(telemetry::QuoteRefusedReason::UnknownPair);
            Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses for pair {}/{}: {token_in}, {token_out}",
                self.token0, self.token1
            )))
        }
    }

    /// Computes the output amount for `amount_in` on the given quotes by linear interpolation
    /// between the two enclosing quotes.
    ///
    /// Callers must ensure `amount_in` lies within the quoted range (smallest to largest
    /// `amount_in`) — the quotes hold no information outside of it. Errors if the enclosing
    /// quotes are not monotonically increasing in `amount_out`: such quotes are unreliable,
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
            // Recoverable: the next message replaces the quotes wholesale.
            telemetry::record_quote_refused(telemetry::QuoteRefusedReason::NotMonotonic);
            return Err(SimulationError::RecoverableError(format!(
                "Quotes {}/{} are not monotonically increasing in amount_out around the \
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

    /// The state after a fill: both quote lists are consumed. The quotes price fills of the
    /// pre-fill venue only: post-fill pricing is unknown in either direction until the next
    /// message, and re-reading the cumulative quotes would double-count the maker's liquidity.
    ///
    /// The state after the fill keeps the simulation overrides: they are the state the fill was
    /// priced on, and a consumer simulates the swap after quoting it.
    fn consumed(&self) -> Box<dyn ProtocolSim> {
        Box::new(Self {
            token0: self.token0.clone(),
            token1: self.token1.clone(),
            quotes_0_to_1: Vec::new(),
            quotes_1_to_0: Vec::new(),
            gas_cost: self.gas_cost.clone(),
            quotable_until: self.quotable_until,
            overrides: self.overrides.clone(),
        })
    }
}

#[typetag::serde]
impl ProtocolSim for PriceLevelStreamState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.ensure_quotable(Instant::now())?;
        let quotes = self.quotes(&base.address, &quote.address)?;
        let best = quotes
            .iter()
            .find(|q| q.amount_in > BigUint::ZERO && q.amount_out > BigUint::ZERO)
            .ok_or_else(|| {
                telemetry::record_quote_refused(telemetry::QuoteRefusedReason::NoLiquidity);
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
        self.ensure_quotable(Instant::now())?;
        let quotes = self.quotes(&token_in.address, &token_out.address)?;
        let (Some(first), Some(last)) = (quotes.first(), quotes.last()) else {
            telemetry::record_quote_refused(telemetry::QuoteRefusedReason::NoLiquidity);
            return Err(SimulationError::RecoverableError("No liquidity available".to_string()));
        };
        // Below the smallest quote nothing is served. The venue itself could still fill —
        // FermiSwap was observed quoting below the smallest streamed level — but if so, on its
        // own price curve, which the quotes hold no information about: unlike interpolation
        // between two quoted levels, whose result is bracketed by genuine samples on both
        // sides, extrapolating linearly from (0, 0) like Titan's quote API does is an unbounded
        // guess even off healthy quotes. (0, 0) is an assumption, not a sample, and the
        // bottom of that line is not even fillable: FermiSwap reverts below a venue-side
        // minimum (~$0.02 at the time of measurement) where the API keeps quoting. And
        // malformed quotes turn the guess absurd: on FermiSwap's flat cbBTC-input books
        // (18-decimals grid bug) the smallest quote is the venue's depth clamp, not a price
        // sample, and the extrapolated quotes land ~3e6x below the venue's own (measured
        // 2026-07). All the rejection gives up are trades smaller than the first level —
        // ~$10-30 on healthy quotes, negligible for routing. Hence no partial result either.
        if amount_in < first.amount_in {
            telemetry::record_quote_refused(telemetry::QuoteRefusedReason::BelowSmallestQuote);
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
        self.ensure_quotable(Instant::now())?;
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

    /// Equal when the pair, both quote lists and the gas cost match, and either neither state
    /// carries simulation overrides, or both carry overrides of the same block over the same
    /// account addresses. A state that gains or loses its overrides is a changed state.
    ///
    /// The overridden values are left out. Quoting never reads them, and Titan republishes a
    /// venue's accounts several times a second while its prices stand still, so comparing them
    /// would report a changed quote for an unchanged price.
    ///
    /// The block is compared because prices can be identical across a block boundary: without it,
    /// a consumer that keeps the state it holds would simulate on the previous block's overrides.
    /// The account addresses are compared because a merge can add or lose another quote address's
    /// entry (see Metric); without them, a consumer would keep overrides missing an account the
    /// swap reads.
    ///
    /// `quotable_until` is left out too: this process derives it and never serializes it. A
    /// consumer that keeps the state it holds whenever `eq` says equal therefore keeps the earlier
    /// deadline, and that state stops quoting one slot after its own message, even while newer
    /// messages repeat the same quotes. Replace the held state on every update to always quote on
    /// the newest deadline.
    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .is_some_and(|other| {
                let Self {
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    gas_cost,
                    quotable_until: _,
                    overrides,
                } = other;
                let same_overrides = match (&self.overrides, overrides) {
                    (Some(held), Some(arriving)) => {
                        held.block_number == arriving.block_number &&
                            held.accounts
                                .keys()
                                .eq(arriving.accounts.keys())
                    }
                    (None, None) => true,
                    _ => false,
                };
                &self.token0 == token0 &&
                    &self.token1 == token1 &&
                    &self.quotes_0_to_1 == quotes_0_to_1 &&
                    &self.quotes_1_to_0 == quotes_1_to_0 &&
                    &self.gas_cost == gas_cost &&
                    same_overrides
            })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rstest::rstest;

    use super::{
        super::{
            convert::quotes_for_book,
            telemetry::recorded::{counter_value, record_async},
            test_support::{captured_book, test_overrides, token, tokens, USDC, WBTC, WETH},
        },
        *,
    };

    /// The tag typetag writes for this state, as it appears in a recording.
    const TAG: &str = r#""PriceLevelStreamState""#;

    /// A recording written before the state carried overrides, verbatim: it has no `overrides`
    /// key at all, and its `BigUint` amounts are sequences of base-2^32 digits.
    const LEGACY_STATE: &str = r#"{
      "token0": "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",
      "token1": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
      "quotes_0_to_1": [{"amount_in": [100000000], "amount_out": [1215752192, 23]}],
      "quotes_1_to_0": [{"amount_in": [1215752192, 23], "amount_out": [99000000]}],
      "gas_cost": [120000]
    }"#;

    /// A recording of an adjacently tagged state with the tag before the content, as typetag
    /// itself writes it.
    fn tag_first(tag: &str, state: &str) -> String {
        format!(r#"{{"protocol":{tag},"state":{state}}}"#)
    }

    /// The same two fields the other way round: the order that makes typetag buffer the whole
    /// state before it knows what to read it as.
    fn content_first(tag: &str, state: &str) -> String {
        format!(r#"{{"state":{state},"protocol":{tag}}}"#)
    }

    fn wbtc() -> Token {
        token(WBTC, "WBTC", 8)
    }

    fn usdc() -> Token {
        token(USDC, "USDC", 6)
    }

    fn weth() -> Token {
        token(WETH, "WETH", 18)
    }

    fn quote(amount_in: u64, amount_out: u64) -> PriceLevelStreamQuote {
        PriceLevelStreamQuote::new(BigUint::from(amount_in), BigUint::from(amount_out))
    }

    /// WBTC (token0) / USDC (token1) quotes: 1 WBTC -> 100k USDC flat, then the second level
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

    /// The Bebop WETH/USDC quotes of the captured live message, converted as the stream converts
    /// them: the amounts Titan quoted.
    fn captured_state() -> PriceLevelStreamState {
        let tokens = tokens();
        let book = captured_book("Bebop", "WETH", "USDC");
        let weth = tokens
            .get(&weth().address)
            .expect("WETH is admitted");
        let usdc = tokens
            .get(&usdc().address)
            .expect("USDC is admitted");
        let (bids, asks) = quotes_for_book(&book, weth, usdc);
        PriceLevelStreamState::new(
            weth.address.clone(),
            usdc.address.clone(),
            bids.expect("the captured bids convert"),
            asks.expect("the captured asks convert"),
            BigUint::from(120_000u64),
        )
    }

    #[test]
    fn captured_state_carries_the_whole_captured_book() {
        let state = captured_state();
        assert_eq!(state.quotes_0_to_1.len(), 15);
        assert_eq!(state.quotes_1_to_0.len(), 14);
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

    /// A WBTC/USDC state whose WBTC quotes are not monotonically increasing in amount_out, as
    /// after a stream glitch.
    fn glitched_state() -> PriceLevelStreamState {
        PriceLevelStreamState::new(
            wbtc().address,
            usdc().address,
            vec![quote(100, 200), quote(200, 150)],
            vec![],
            BigUint::ZERO,
        )
    }

    /// A WBTC/USDC state with no quotes in either direction.
    fn empty_state() -> PriceLevelStreamState {
        PriceLevelStreamState::new(wbtc().address, usdc().address, vec![], vec![], BigUint::ZERO)
    }

    #[test]
    fn get_amount_out_on_glitched_quotes_is_rejected() {
        // The data is unreliable, so a quote landing in the glitched segment is refused instead
        // of interpolated (or underflowing).
        let state = glitched_state();
        let result = state.get_amount_out(BigUint::from(150u64), &wbtc(), &usdc());
        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));

        // Hitting a level exactly returns that genuine sample even on glitched quotes.
        let result = state
            .get_amount_out(BigUint::from(100u64), &wbtc(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(200u64));
    }

    #[test]
    fn get_amount_out_below_smallest_level_is_rejected() {
        // The quotes hold no information below the smallest one, and there is no partial
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
    fn get_amount_out_consumes_both_quote_lists() {
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
        let result = empty_state().get_amount_out(BigUint::from(1u64), &wbtc(), &usdc());
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

    #[rstest]
    #[case::one_nanosecond_before(|until| until - Duration::from_nanos(1), true)]
    #[case::at_quotable_until(|until| until, false)]
    #[case::one_second_after(|until| until + Duration::from_secs(1), false)]
    fn ensure_quotable_around_quotable_until(
        #[case] now: fn(Instant) -> Instant,
        #[case] quotable: bool,
    ) {
        let until = Instant::now() + Duration::from_secs(60);
        let state = state().with_quotable_until(until);
        assert_eq!(
            state
                .ensure_quotable(now(until))
                .is_ok(),
            quotable
        );
    }

    /// Each refusal is counted under its own `reason`, once per refused call.
    #[rstest]
    #[case::price_levels_expired(
        state().with_quotable_until(Instant::now()),
        100_000_000,
        wbtc(),
        "price_levels_expired"
    )]
    #[case::unknown_pair(state(), 1, weth(), "unknown_pair")]
    #[case::no_liquidity(empty_state(), 1, wbtc(), "no_liquidity")]
    #[case::below_smallest_quote(state(), 50_000_000, wbtc(), "below_smallest_quote")]
    #[case::not_monotonic(glitched_state(), 150, wbtc(), "not_monotonic")]
    fn a_refused_quote_is_counted_under_its_reason(
        #[case] state: PriceLevelStreamState,
        #[case] amount_in: u64,
        #[case] token_in: Token,
        #[case] reason: &str,
    ) {
        let (result, snapshot) = record_async(async {
            state.get_amount_out(BigUint::from(amount_in), &token_in, &usdc())
        });

        assert!(result.is_err());
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_quotes_refused_total",
                &[("reason", reason)]
            ),
            1
        );
    }

    /// `spot_price` counts its own refusal, as `get_amount_out` does.
    #[test]
    fn a_refused_spot_price_is_counted() {
        let (result, snapshot) = record_async(async { empty_state().spot_price(&wbtc(), &usdc()) });

        assert!(result.is_err());
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_quotes_refused_total",
                &[("reason", "no_liquidity")]
            ),
            1
        );
    }

    #[test]
    fn state_without_quotable_until_never_expires() {
        let far = Instant::now() + Duration::from_secs(1_000_000);
        assert!(state().ensure_quotable(far).is_ok());
    }

    #[test]
    fn expired_state_refuses_every_query() {
        // `quotable_until` is set to `Instant::now()`, which has passed by the time the queries
        // run.
        let state = state().with_quotable_until(Instant::now());
        assert!(matches!(
            state.get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc()),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            state.spot_price(&wbtc(), &usdc()),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            state.get_limits(wbtc().address, usdc().address),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    #[test]
    fn fresh_state_answers_every_query() {
        let state = state().with_quotable_until(Instant::now() + Duration::from_secs(60));
        assert!(state
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .is_ok());
        assert!(state
            .spot_price(&wbtc(), &usdc())
            .is_ok());
        assert!(state
            .get_limits(wbtc().address, usdc().address)
            .is_ok());
    }

    #[test]
    fn successor_state_keeps_quotable_until() {
        let until = Instant::now() + Duration::from_secs(60);
        let state = state().with_quotable_until(until);
        let result = state
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .expect("fresh state quotes");
        let successor = result
            .new_state
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(successor.quotable_until, Some(until));
    }

    #[test]
    fn quotable_until_serde_round_trip() {
        let live = state().with_quotable_until(Instant::now());
        let json = serde_json::to_value(&live).unwrap();
        assert!(json
            .as_object()
            .unwrap()
            .get("quotable_until")
            .is_none());
        // A deserialized state has no `quotable_until`, so a recording replays without expiring.
        let replayed: PriceLevelStreamState = serde_json::from_value(json).unwrap();
        assert_eq!(replayed.quotable_until, None);
        assert!(replayed
            .spot_price(&wbtc(), &usdc())
            .is_ok());
    }

    /// Two messages carrying the same quotes yield equal states, whatever their guards.
    #[test]
    fn eq_ignores_quotable_until() {
        let until = Instant::now() + Duration::from_secs(60);
        assert!(state().eq(&state().with_quotable_until(until)));
        assert!(state()
            .with_quotable_until(until)
            .eq(&state().with_quotable_until(until + Duration::from_secs(1))));
    }

    #[test]
    fn with_simulation_overrides_sets_and_exposes_them() {
        assert_eq!(state().simulation_overrides(), None);
        assert_eq!(
            state()
                .with_simulation_overrides(test_overrides())
                .simulation_overrides(),
            Some(&test_overrides())
        );
    }

    /// `eq` compares the block and the overridden account addresses of the simulation overrides,
    /// not the overridden values: see `eq`'s own documentation.
    #[rstest]
    #[case::the_same_overrides(Some(test_overrides()), true)]
    #[case::no_overrides(None, false)]
    #[case::rebuilt_with_other_values(Some(rebuilt_with_other_values()), true)]
    #[case::another_block(Some(in_another_block()), false)]
    #[case::fewer_accounts(Some(with_one_account_fewer()), false)]
    #[case::another_account(Some(with_another_account()), false)]
    fn eq_compares_the_block_and_the_accounts_not_the_values(
        #[case] other: Option<PammSimulationOverrides>,
        #[case] equal: bool,
    ) {
        let held = state().with_simulation_overrides(test_overrides());
        let other = match other {
            Some(overrides) => state().with_simulation_overrides(overrides),
            None => state(),
        };

        assert_eq!(held.eq(&other), equal);
        assert_eq!(other.eq(&held), equal);
    }

    /// [`test_overrides`] taken later in the same block: every overridden value and the build time
    /// differ, the account addresses and the block do not.
    fn rebuilt_with_other_values() -> PammSimulationOverrides {
        let mut rebuilt = test_overrides();
        rebuilt.built_at_ns += 1;
        for account in rebuilt.accounts.values_mut() {
            account.balance = Some(Bytes::from(vec![0x42]));
            account.nonce = account.nonce.map(|nonce| nonce + 1);
            account.state_diff.clear();
        }
        rebuilt
    }

    /// [`test_overrides`] in the next block.
    fn in_another_block() -> PammSimulationOverrides {
        PammSimulationOverrides { block_number: Some(26_201_955), ..test_overrides() }
    }

    /// [`test_overrides`] with as many accounts, one of them under another address, as when a merge
    /// swaps one quote address's entry for another's.
    fn with_another_account() -> PammSimulationOverrides {
        let mut swapped = test_overrides();
        let (_, account) = swapped
            .accounts
            .pop_first()
            .expect("an account");
        swapped
            .accounts
            .insert(Bytes::from(vec![0xab; 20]), account);
        swapped
    }

    /// [`test_overrides`] without its first account, as when a merge loses another quote
    /// address's entry.
    fn with_one_account_fewer() -> PammSimulationOverrides {
        let mut fewer = test_overrides();
        fewer.accounts.pop_first();
        fewer
    }

    /// The consumed quotes are empty, but the successor still carries the state the fill was
    /// priced on.
    #[test]
    fn successor_state_keeps_the_overrides() {
        let state = state().with_simulation_overrides(test_overrides());
        let result = state
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .expect("fresh state quotes");
        let successor = result
            .new_state
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(successor.simulation_overrides(), Some(&test_overrides()));
    }

    /// A state built from captured quotes and carrying captured overrides survives both
    /// serde_json shapes. Nothing in the overrides is wider than `u64`, because a
    /// `serde_json::Value` cannot hold a wider integer.
    #[test]
    fn an_override_bearing_captured_state_round_trips_as_text_and_as_a_value() {
        let state = captured_state().with_simulation_overrides(test_overrides());

        let text = serde_json::to_string(&state).expect("serializes");
        let value = serde_json::to_value(&state).expect("serializes into a Value");
        let from_text: PriceLevelStreamState =
            serde_json::from_str(&text).expect("reads back from text");
        let from_value: PriceLevelStreamState =
            serde_json::from_value(value).expect("reads back from a Value");

        for read in [&from_text, &from_value] {
            assert_eq!(read.simulation_overrides(), Some(&test_overrides()));
            assert!(state.eq(read));
        }
    }

    /// The same state through a boxed `dyn ProtocolSim`, in both orders typetag's adjacent tag
    /// can arrive in. The order decides the path: with the tag first typetag streams the state
    /// straight into its deserializer, while with the content first it buffers the whole state
    /// into its own `Content` enum and replays it — and that buffer, like a `serde_json::Value`,
    /// holds no integer wider than `u64`.
    #[rstest]
    #[case::tag_before_content(tag_first)]
    #[case::content_before_tag(content_first)]
    fn an_override_bearing_captured_state_round_trips_through_a_boxed_protocol_sim(
        #[case] recording: fn(&str, &str) -> String,
    ) {
        let state = captured_state().with_simulation_overrides(test_overrides());
        let text = recording(TAG, &serde_json::to_string(&state).expect("serializes"));

        let boxed: Box<dyn ProtocolSim> = serde_json::from_str(&text).expect("reads back");

        assert!(state.eq(boxed.as_ref()));
        let read = boxed
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(read.simulation_overrides(), Some(&test_overrides()));
    }

    /// The tag the recordings in these tests carry is the one typetag itself writes.
    #[test]
    fn typetag_writes_the_tag_the_recordings_carry() {
        let text = serde_json::to_string(&state() as &dyn ProtocolSim).expect("serializes");

        assert!(text.starts_with(&format!(r#"{{"protocol":{TAG},"#)), "{text}");
    }

    /// The state serializes its overrides under the top-level key `overrides`, in the typed form
    /// and inside typetag's `state` content alike.
    ///
    /// A rename would break every recording without an error: the old key would be ignored as an
    /// unknown field and the state would read back without overrides, as recordings made before
    /// the field do (see `a_legacy_typed_recording_reads_back_without_overrides`). So this test
    /// pins the name.
    #[test]
    fn the_serialized_overrides_key_is_named_overrides() {
        let state = captured_state().with_simulation_overrides(test_overrides());

        let text = serde_json::to_string(&state).expect("serializes");
        let typed = serde_json::to_value(&state).expect("serializes");
        let boxed = serde_json::to_value(&state as &dyn ProtocolSim).expect("serializes");

        assert!(text.contains(r#""overrides":"#), "no `overrides` key in the state JSON");
        for json in [&typed, &boxed["state"]] {
            let overrides = json
                .as_object()
                .expect("an object")
                .get("overrides")
                .expect("the state object names them `overrides`");
            let read: PammSimulationOverrides =
                serde_json::from_value(overrides.clone()).expect("the overrides under that key");
            assert_eq!(read, test_overrides());
        }
    }

    /// A recording made before the field existed reads back as a state without overrides, rather
    /// than failing on the missing key.
    #[test]
    fn a_legacy_typed_recording_reads_back_without_overrides() {
        let read: PriceLevelStreamState = serde_json::from_str(LEGACY_STATE).expect("reads back");
        assert_eq!(read.simulation_overrides(), None);
        assert_eq!(read.quotes_0_to_1, vec![quote(100_000_000, 100_000_000_000)]);
        assert_eq!(read.quotes_1_to_0, vec![quote(100_000_000_000, 99_000_000)]);
    }

    /// The same legacy recording boxed, in both tag orders: the content-first path buffers the
    /// absent key as absent too.
    #[rstest]
    #[case::tag_before_content(tag_first)]
    #[case::content_before_tag(content_first)]
    fn a_legacy_boxed_recording_reads_back_without_overrides(
        #[case] recording: fn(&str, &str) -> String,
    ) {
        let boxed: Box<dyn ProtocolSim> =
            serde_json::from_str(&recording(TAG, LEGACY_STATE)).expect("reads back");
        let read = boxed
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(read.simulation_overrides(), None);
        assert_eq!(read.quotes_0_to_1, vec![quote(100_000_000, 100_000_000_000)]);
    }

    /// A state without overrides writes the legacy shape, so a consumer built before the field
    /// existed can read a recording made now.
    #[test]
    fn a_state_without_overrides_serializes_without_the_key() {
        let typed = serde_json::to_value(state()).expect("serializes");
        let boxed = serde_json::to_value(&state() as &dyn ProtocolSim).expect("serializes");

        for json in [&typed, &boxed["state"]] {
            assert!(json
                .as_object()
                .expect("an object")
                .get("overrides")
                .is_none());
        }
    }
}
