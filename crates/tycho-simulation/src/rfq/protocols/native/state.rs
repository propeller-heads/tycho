use std::{any::Any, collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::GetAmountOutParams, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::rfq::{
    client::RFQClient,
    protocols::native::{client::NativeClient, models::NativePriceData},
};

/// Native Relay's liquidity on one chain: one book per pair, bids and asks in one entry.
///
/// Native names no market maker, so a swap marks the whole venue used and a later swap on that
/// state finds no liquidity.
// `Deserialize` bypasses `new`, so it does not validate the books or remove zero-quantity levels.
// This is harmless while `delta_transition` cannot update the books; use `TryFrom`-based
// deserialization before supporting state deltas.
#[derive(Clone, Serialize, Deserialize)]
pub struct NativeState {
    /// One entry per pair. A bid takes the pair's base token in; an ask takes its quote token in.
    pub books: Arc<Vec<NativePriceData>>,
    pub tokens: Arc<HashMap<Bytes, Token>>,
    /// Whether a swap on this state already took Native's quote.
    pub used: bool,
    pub client: Arc<NativeClient>,
}

impl fmt::Debug for NativeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeState")
            .field("books", &self.books.len())
            .field("tokens", &self.tokens.len())
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

impl NativeState {
    /// Keeps only the levels with liquidity and checks every book names two known tokens and
    /// carries finite, positive prices and non-negative minimums.
    pub fn new(
        mut books: Vec<NativePriceData>,
        tokens: HashMap<Bytes, Token>,
        client: NativeClient,
    ) -> Result<Self, SimulationError> {
        for book in &mut books {
            // Zero-quantity levels carry no liquidity and must not influence spot prices or
            // limits.
            book.bids
                .retain(|level| level.quantity != 0.0);
            book.asks
                .retain(|level| level.quantity != 0.0);
        }
        let state = NativeState {
            books: Arc::new(books),
            tokens: Arc::new(tokens),
            used: false,
            client: Arc::new(client),
        };
        for book in state.books.iter() {
            state.validate_book(book)?;
        }
        Ok(state)
    }

    fn validate_book(&self, book: &NativePriceData) -> Result<(), SimulationError> {
        if !self
            .tokens
            .contains_key(&book.base_address) ||
            !self
                .tokens
                .contains_key(&book.quote_address)
        {
            return Err(SimulationError::FatalError(
                "Native book token addresses do not match state tokens".to_string(),
            ));
        }
        let minimums = [
            book.minimum_in_base,
            book.minimum_in_quote,
            book.minimum_out_base,
            book.minimum_out_quote,
        ];
        if minimums
            .iter()
            .any(|minimum| !minimum.is_finite() || *minimum < 0.0)
        {
            return Err(SimulationError::FatalError(
                "Native book contains an invalid minimum amount".to_string(),
            ));
        }
        if book
            .bids
            .iter()
            .chain(book.asks.iter())
            .any(|level| {
                !level.quantity.is_finite() ||
                    level.quantity < 0.0 ||
                    !level.price.is_finite() ||
                    level.price <= 0.0
            })
        {
            return Err(SimulationError::FatalError(
                "Native book contains an invalid price level".to_string(),
            ));
        }

        Ok(())
    }

    /// The book that trades `token_in` for `token_out`, and whether that sells its base token.
    /// A book quoting the pair as given beats one quoting it the other way round.
    fn book(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(&NativePriceData, bool), SimulationError> {
        let sells_base = |book: &&NativePriceData| {
            &book.base_address == token_in && &book.quote_address == token_out
        };
        let sells_quote = |book: &&NativePriceData| {
            &book.base_address == token_out && &book.quote_address == token_in
        };
        if let Some(book) = self.books.iter().find(sells_base) {
            return Ok((book, true));
        }
        if let Some(book) = self.books.iter().find(sells_quote) {
            return Ok((book, false));
        }
        Err(SimulationError::InvalidInput(
            format!("Invalid token addresses. Got in={token_in}, out={token_out}"),
            None,
        ))
    }

    fn token(&self, address: &Bytes) -> Result<&Token, SimulationError> {
        self.tokens.get(address).ok_or_else(|| {
            SimulationError::InvalidInput(format!("Native does not quote token {address}"), None)
        })
    }

    fn used_state(&self) -> Self {
        Self {
            books: self.books.clone(),
            tokens: self.tokens.clone(),
            used: true,
            client: self.client.clone(),
        }
    }

    fn enforce_minimum(
        amount: &BigUint,
        minimum: f64,
        amount_kind: &str,
    ) -> Result<(), SimulationError> {
        if minimum == 0.0 {
            return Ok(())
        }

        // Native Relay reports minimums in atomic units, matching `amount`. Round up defensively
        // because the JSON number is deserialized into an f64 by the orderbook model.
        let minimum = BigUint::from_f64(minimum.ceil()).ok_or_else(|| {
            SimulationError::FatalError(format!(
                "Can't convert Native minimum {amount_kind} amount to BigUint"
            ))
        })?;
        if amount < &minimum {
            return Err(SimulationError::RecoverableError(format!(
                "Amount below minimum {amount_kind}. Amount: {amount}, min amount: {minimum}"
            )))
        }

        Ok(())
    }
}

#[typetag::serde]
impl ProtocolSim for NativeState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let (book, sell_base) = self.book(&base.address, &quote.address)?;
        if self.used {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }
        let best_bid = book.bids.first().map(|lvl| lvl.price);
        let best_ask = book.asks.first().map(|lvl| lvl.price);
        let average_price = match (best_bid, best_ask) {
            (Some(bid), Some(ask)) => bid.midpoint(ask),
            (Some(bid), None) => bid,
            (None, Some(ask)) => ask,
            (None, None) => {
                return Err(SimulationError::RecoverableError("No liquidity".to_string()))
            }
        };
        let spot_price = if sell_base { average_price } else { average_price.recip() };
        if !spot_price.is_finite() || spot_price <= 0.0 {
            return Err(SimulationError::RecoverableError(
                "Native spot price is not positive and finite".to_string(),
            ))
        }
        Ok(spot_price)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let (book, is_sell_base) = self.book(&token_in.address, &token_out.address)?;
        if amount_in == BigUint::ZERO {
            return Err(SimulationError::InvalidInput(
                "Native swap amount must be greater than zero".to_string(),
                None,
            ));
        }
        if self.used {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let (minimum_in, minimum_out) = if is_sell_base {
            (book.minimum_in_base, book.minimum_out_quote)
        } else {
            (book.minimum_in_quote, book.minimum_out_base)
        };
        Self::enforce_minimum(&amount_in, minimum_in, "input")?;

        let amount_in_f64 = amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10f64.powi(token_in.decimals as i32);

        let levels = if is_sell_base {
            book.bids.clone()
        } else {
            NativePriceData::invert_price_levels(&book.asks)
        };
        if levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let (amount_out_f64, remaining) =
            NativePriceData::get_amount_out_from_levels(amount_in_f64, &levels);

        let res = GetAmountOutResult {
            amount: BigUint::from_f64(amount_out_f64 * 10f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUint".into())
                })?,
            gas: BigUint::from(134_000u64), // Approximate standard gas for Native swap
            new_state: Box::new(self.used_state()),
        };

        if remaining > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!("Pool has not enough liquidity to support complete swap. Input amount: {}, consumed: {}", amount_in_f64, amount_in_f64 - remaining),
                Some(res),
            ));
        }

        Self::enforce_minimum(&res.amount, minimum_out, "output")?;
        Ok(res)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (book, is_sell_base) = self.book(&sell_token, &buy_token)?;
        if self.used {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let levels = if is_sell_base {
            book.bids.clone()
        } else {
            NativePriceData::invert_price_levels(&book.asks)
        };
        if levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let (total_sell_amount, total_buy_amount) =
            levels
                .iter()
                .fold((0.0, 0.0), |(sell_sum, buy_sum), level| {
                    (sell_sum + level.quantity, buy_sum + level.quantity * level.price)
                });

        let sell_decimals = self.token(&sell_token)?.decimals;
        let buy_decimals = self.token(&buy_token)?.decimals;
        let sell_limit = BigUint::from_f64(total_sell_amount * 10f64.powi(sell_decimals as i32))
            .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert limit to BigUInt".into())
            })?;
        let buy_limit = BigUint::from_f64(total_buy_amount * 10f64.powi(buy_decimals as i32))
            .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert limit to BigUInt".into())
            })?;
        Ok((sell_limit, buy_limit))
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
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<NativeState>()
        {
            self.books == other_state.books && self.used == other_state.used
        } else {
            false
        }
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for NativeState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        Ok(self
            .client
            .request_binding_quote(&params)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use rstest::rstest;
    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::native::models::NativePriceLevel;

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[],
            Chain::Ethereum,
            100,
        )
    }

    fn weth() -> Token {
        token("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", "WETH", 18)
    }

    fn usdc() -> Token {
        token("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "USDC", 6)
    }

    fn state() -> NativeState {
        let book = NativePriceData {
            base_address: weth().address,
            quote_address: usdc().address,
            minimum_in_base: 100_000_000_000.0,
            minimum_in_quote: 100.0,
            minimum_out_base: 0.0,
            minimum_out_quote: 0.0,
            bids: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
            asks: vec![NativePriceLevel { quantity: 1.0, price: 2_000.0 }],
        };
        let client = NativeClient::new(
            Chain::Ethereum,
            String::new(),
            HashSet::new(),
            0.0,
            HashSet::new(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        NativeState::new(
            vec![book],
            HashMap::from([(weth().address, weth()), (usdc().address, usdc())]),
            client,
        )
        .unwrap()
    }

    /// The same state built again from its own parts, after a test edited a book.
    fn rebuilt(state: NativeState) -> Result<NativeState, SimulationError> {
        NativeState::new(
            Arc::unwrap_or_clone(state.books),
            Arc::unwrap_or_clone(state.tokens),
            Arc::unwrap_or_clone(state.client),
        )
    }

    fn book_mut(state: &mut NativeState) -> &mut NativePriceData {
        &mut Arc::make_mut(&mut state.books)[0]
    }

    #[test]
    fn accepts_base_sell_at_atomic_input_minimum() {
        let state = state();
        let result = state.get_amount_out(BigUint::from(100_000_000_000u64), &weth(), &usdc());
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_base_sell_below_atomic_input_minimum() {
        let state = state();
        let result = state.get_amount_out(BigUint::from(99_999_999_999u64), &weth(), &usdc());
        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn accepts_quote_sell_at_atomic_input_minimum() {
        let state = state();
        let result = state.get_amount_out(BigUint::from(100u64), &usdc(), &weth());
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_quote_sell_below_atomic_input_minimum() {
        let state = state();
        let result = state.get_amount_out(BigUint::from(99u64), &usdc(), &weth());
        assert!(matches!(result, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn calculates_amount_out_for_base_sell() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(500_000_000_000_000_000u64), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(1_000_000_000u64));
    }

    #[test]
    fn once_per_venue() {
        let state = state();
        let first = state
            .get_amount_out(BigUint::from(500_000_000_000_000_000u64), &weth(), &usdc())
            .unwrap();
        let after_first = first
            .new_state
            .as_any()
            .downcast_ref::<NativeState>()
            .unwrap();
        assert!(after_first.used);
        assert!(matches!(
            after_first.get_amount_out(BigUint::from(1_000_000_000u64), &usdc(), &weth()),
            Err(SimulationError::RecoverableError(message)) if message == "No liquidity"
        ));
        assert!(matches!(
            after_first.spot_price(&weth(), &usdc()),
            Err(SimulationError::RecoverableError(message)) if message == "No liquidity"
        ));
        assert!(matches!(
            after_first.get_limits(weth().address, usdc().address),
            Err(SimulationError::RecoverableError(message)) if message == "No liquidity"
        ));
    }

    #[test]
    fn book_quoting_the_direction_beats_the_inverted_one() {
        let mut state = state();
        // The USDC/WETH book pays 1 WETH for 2000 USDC; the WETH/USDC book's asks sell 1 WETH for
        // 2000 USDC too, but its minimum output would reject the swap.
        let mut reverse = state.books[0].clone();
        reverse.base_address = usdc().address;
        reverse.quote_address = weth().address;
        reverse.minimum_in_base = 0.0;
        reverse.bids = vec![NativePriceLevel { quantity: 2_000.0, price: 0.0005 }];
        reverse.asks = vec![];
        book_mut(&mut state).minimum_out_base = 2_000_000_000_000_000_000.0;
        Arc::make_mut(&mut state.books).push(reverse);
        let result = state
            .get_amount_out(BigUint::from(2_000_000_000u64), &usdc(), &weth())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn ignores_zero_quantity_levels() {
        let mut state = state();
        book_mut(&mut state)
            .bids
            .insert(0, NativePriceLevel { quantity: 0.0, price: 1_000.0 });
        let state = rebuilt(state).unwrap();
        assert_eq!(state.books[0].bids.len(), 1);
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            2_000.0
        );
        let result = state
            .get_amount_out(BigUint::from(500_000_000_000_000_000u64), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(1_000_000_000u64));
    }

    #[test]
    fn returns_finite_spot_price_for_large_finite_levels() {
        let mut state = state();
        book_mut(&mut state).bids[0] = NativePriceLevel { quantity: 1e-306, price: 1e308 };
        book_mut(&mut state).asks[0] = NativePriceLevel { quantity: 1e-306, price: 1e308 };
        let state = rebuilt(state).unwrap();
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            1e308
        );
    }

    #[test]
    fn calculates_midpoint_spot_price_in_both_directions() {
        let mut state = state();
        book_mut(&mut state).bids[0].price = 1_900.0;
        book_mut(&mut state).asks[0].price = 2_100.0;
        let direct = state
            .spot_price(&weth(), &usdc())
            .unwrap();
        let inverse = state
            .spot_price(&usdc(), &weth())
            .unwrap();
        assert_eq!(direct, 2_000.0);
        assert_eq!(inverse, direct.recip());
    }

    #[test]
    fn rejects_non_finite_inverted_spot_price() {
        let mut state = state();
        let smallest_positive_price = f64::from_bits(1);
        book_mut(&mut state).bids[0].price = smallest_positive_price;
        book_mut(&mut state).asks[0].price = smallest_positive_price;
        let state = rebuilt(state).unwrap();
        let result = state.spot_price(&usdc(), &weth());
        assert!(matches!(
            result,
            Err(SimulationError::RecoverableError(message))
                if message.contains("not positive and finite")
        ));
    }

    #[test]
    fn calculates_amount_out_for_quote_sell() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(1_000_000_000u64), &usdc(), &weth())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(500_000_000_000_000_000u64));
    }

    #[rstest]
    #[case::sell_base(true, 2_500_000_000_000_000_000, 3_500_000_000)]
    #[case::sell_quote(false, 8_192_000_000, 2_500_000_000_000_000_000)]
    fn consumes_multiple_price_levels(
        #[case] sell_base: bool,
        #[case] amount_in: u64,
        #[case] expected_amount_out: u64,
    ) {
        let mut state = state();
        book_mut(&mut state).bids = vec![
            NativePriceLevel { quantity: 1.0, price: 2_000.0 },
            NativePriceLevel { quantity: 2.0, price: 1_000.0 },
        ];
        book_mut(&mut state).asks = vec![
            NativePriceLevel { quantity: 1.0, price: 2_048.0 },
            NativePriceLevel { quantity: 2.0, price: 4_096.0 },
        ];
        // Sell base: 1 * 2000 + 1.5 * 1000 = 3500 USDC.
        // Sell quote: 2048 buys the first WETH; the remaining 6144 buys 1.5 WETH.
        let (token_in, token_out) = if sell_base { (weth(), usdc()) } else { (usdc(), weth()) };
        let result = state
            .get_amount_out(BigUint::from(amount_in), &token_in, &token_out)
            .unwrap();
        assert_eq!(result.amount, BigUint::from(expected_amount_out));
    }

    #[test]
    fn enforces_base_sell_atomic_output_minimum() {
        let mut state = state();
        book_mut(&mut state).minimum_out_quote = 1_000_000_000.0;
        let amount_in = BigUint::from(500_000_000_000_000_000u64);
        assert!(state
            .get_amount_out(amount_in.clone(), &weth(), &usdc())
            .is_ok());
        book_mut(&mut state).minimum_out_quote = 1_000_000_001.0;
        assert!(matches!(
            state.get_amount_out(amount_in, &weth(), &usdc()),
            Err(SimulationError::RecoverableError(message)) if message.contains("minimum output")
        ));
    }

    #[test]
    fn enforces_quote_sell_atomic_output_minimum() {
        let mut state = state();
        book_mut(&mut state).minimum_out_base = 500_000_000_000_000.0;
        let amount_in = BigUint::from(1_000_000u64);
        assert!(state
            .get_amount_out(amount_in.clone(), &usdc(), &weth())
            .is_ok());
        book_mut(&mut state).minimum_out_base = 500_000_000_000_001.0;
        assert!(matches!(
            state.get_amount_out(amount_in, &usdc(), &weth()),
            Err(SimulationError::RecoverableError(message)) if message.contains("minimum output")
        ));
    }

    #[test]
    fn returns_partial_result_when_amount_exceeds_depth() {
        let state = state();
        let result =
            state.get_amount_out(BigUint::from(2_000_000_000_000_000_000u64), &weth(), &usdc());
        match result {
            Err(SimulationError::InvalidInput(_, Some(partial))) => {
                assert_eq!(partial.amount, BigUint::from(2_000_000_000u64));
            }
            other => panic!("Expected insufficient-liquidity result, got {other:?}"),
        }
    }

    #[test]
    fn rejects_sub_unit_partial_fill() {
        let mut state = state();
        book_mut(&mut state).minimum_in_base = 0.0;
        book_mut(&mut state).bids[0].quantity = 0.5e-18;
        let result = state.get_amount_out(BigUint::from(1u64), &weth(), &usdc());
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, Some(_)))));
    }

    #[test]
    fn rejects_zero_amount() {
        let state = state();
        let result = state.get_amount_out(BigUint::ZERO, &weth(), &usdc());
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn gets_base_sell_limits() {
        let state = state();
        let limits = state
            .get_limits(weth().address, usdc().address)
            .unwrap();
        assert_eq!(limits.0, BigUint::from(1_000_000_000_000_000_000u64));
        assert_eq!(limits.1, BigUint::from(2_000_000_000u64));
    }

    #[test]
    fn gets_quote_sell_limits() {
        let state = state();
        let limits = state
            .get_limits(usdc().address, weth().address)
            .unwrap();
        assert_eq!(limits.0, BigUint::from(2_000_000_000u64));
        assert_eq!(limits.1, BigUint::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn rejects_invalid_pair() {
        let mut state = state();
        let other = token("0x1111111111111111111111111111111111111111", "OTHER", 18);
        assert!(matches!(
            state.get_amount_out(BigUint::from(1u64), &other, &usdc()),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert!(matches!(
            state.get_limits(other.address.clone(), usdc().address),
            Err(SimulationError::InvalidInput(_, None))
        ));
        // Direction validation must win even when the book has no liquidity.
        book_mut(&mut state).bids.clear();
        book_mut(&mut state).asks.clear();
        assert!(matches!(
            state.spot_price(&other, &usdc()),
            Err(SimulationError::InvalidInput(message, None))
                if message.contains("Invalid token addresses")
        ));
    }

    #[test]
    fn rejects_invalid_book_state() {
        let mut state = state();
        book_mut(&mut state).bids[0].price = 0.0;
        assert!(matches!(rebuilt(state), Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn rejects_invalid_output_minimum() {
        let mut state = state();
        book_mut(&mut state).minimum_out_base = -1.0;
        assert!(matches!(rebuilt(state), Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn rejects_mismatched_book_tokens() {
        let mut state = state();
        book_mut(&mut state).base_address = Bytes::zero(20);
        assert!(matches!(rebuilt(state), Err(SimulationError::FatalError(_))));
    }

    #[test]
    fn reports_no_liquidity_for_empty_direction() {
        let mut state = state();
        book_mut(&mut state).bids.clear();
        assert!(matches!(
            state.get_amount_out(BigUint::from(500_000_000_000_000_000u64), &weth(), &usdc()),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            state.get_limits(weth().address, usdc().address),
            Err(SimulationError::RecoverableError(_))
        ));
    }
}
