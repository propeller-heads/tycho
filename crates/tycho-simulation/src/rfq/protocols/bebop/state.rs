use std::{any::Any, collections::HashMap, fmt};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{FromPrimitive, Pow, ToPrimitive};
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
    models::QuoteRule,
    protocols::bebop::{client::BebopClient, models::BebopPriceData},
};

/// Bebop's liquidity on one chain: one book per pair, bids and asks in one entry.
///
/// Bebop names no market maker and picks the makers behind a firm quote itself, so the state
/// tracks the venue as a whole: under [`QuoteRule::OncePerVenue`] a swap marks the state used
/// and a later swap on that state finds no liquidity.
#[derive(Clone, Serialize, Deserialize)]
pub struct BebopState {
    /// One entry per pair. Bids sell the pair's base token, asks buy it.
    pub books: Vec<BebopPriceData>,
    /// Every token a book names, by address.
    pub tokens: HashMap<Bytes, Token>,
    pub quote_rule: QuoteRule,
    /// Whether a swap on this state already took Bebop's quote.
    pub used: bool,
    pub client: BebopClient,
}

impl fmt::Debug for BebopState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BebopState")
            .field("books", &self.books.len())
            .field("tokens", &self.tokens.len())
            .field("quote_rule", &self.quote_rule)
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

impl BebopState {
    pub fn new(
        books: Vec<BebopPriceData>,
        tokens: HashMap<Bytes, Token>,
        quote_rule: QuoteRule,
        client: BebopClient,
    ) -> Self {
        BebopState { books, tokens, quote_rule, used: false, client }
    }

    /// Whether this state may still quote under its reuse rule.
    fn available(&self) -> bool {
        !(self.used && self.quote_rule == QuoteRule::OncePerVenue)
    }

    /// The book that trades `token_in` for `token_out`, and whether that sells its base token.
    fn book(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(&BebopPriceData, bool), SimulationError> {
        for book in &self.books {
            if book.base == token_in.as_ref() && book.quote == token_out.as_ref() {
                return Ok((book, true));
            }
            if book.base == token_out.as_ref() && book.quote == token_in.as_ref() {
                return Ok((book, false));
            }
        }
        Err(SimulationError::RecoverableError(format!(
            "Invalid token addresses: {token_in}, {token_out}"
        )))
    }

    fn token(&self, address: &Bytes) -> Result<&Token, SimulationError> {
        self.tokens.get(address).ok_or_else(|| {
            SimulationError::RecoverableError(format!("Bebop does not quote token {address}"))
        })
    }
}

#[typetag::serde]
impl ProtocolSim for BebopState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if !self.available() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let (book, sell_base) = self.book(&base.address, &quote.address)?;
        // Since this method does not care about sell direction, we average the price of the best
        // bid and ask
        let best_bid = book
            .get_bids()
            .first()
            .map(|(price, _)| *price);
        let best_ask = book
            .get_asks()
            .first()
            .map(|(price, _)| *price);

        // If just one is available, only consider that one
        let average_price = match (best_bid, best_ask) {
            (Some(best_bid), Some(best_ask)) => (best_bid + best_ask) / 2.0,
            (Some(best_bid), None) => best_bid,
            (None, Some(best_ask)) => best_ask,
            (None, None) => {
                return Err(SimulationError::RecoverableError("No liquidity available".to_string()))
            }
        };

        // The book prices its base token in its quote token, so the other direction inverts.
        if sell_base {
            Ok(average_price)
        } else {
            Ok(1.0 / average_price)
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if !self.available() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let (book, sell_base) = self.book(&token_in.address, &token_out.address)?;

        // if sell base is true -> use bids
        // if sell base is false -> use asks AND amount is in quote token so the levels need to be
        // adjusted
        let price_levels = if sell_base {
            book.get_bids()
        } else {
            book.get_asks()
                .iter()
                .map(|(price, size)| (1.0 / price, price * size))
                .collect()
        };

        if price_levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let amount_in = amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10f64.powi(token_in.decimals as i32);
        let (amount_out, remaining_amount_in) =
            book.get_amount_out_from_levels(amount_in, price_levels);

        let mut new_state = self.clone();
        new_state.used = true;
        let res = GetAmountOutResult {
            amount: BigUint::from_f64(amount_out * 10f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUInt".into())
                })?,
            gas: BigUint::from(70_000u64), // Rough gas estimation
            new_state: Box::new(new_state),
        };

        if remaining_amount_in > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!("Pool has not enough liquidity to support complete swap. input amount: {amount_in}, consumed amount: {}", amount_in-remaining_amount_in),
                Some(res)));
        }
        Ok(res)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (book, sell_base) = self.book(&sell_token, &buy_token)?;
        let sell_decimals = self.token(&sell_token)?.decimals;
        let buy_decimals = self.token(&buy_token)?.decimals;

        // If selling BASE for QUOTE, we need to look at [BASE/QUOTE].bids
        // If buying BASE with QUOTE, we need to look at [BASE/QUOTE].asks
        let price_levels = if sell_base { book.get_bids() } else { book.get_asks() };

        // A used venue and an empty book alike hold nothing to sell.
        if price_levels.is_empty() || !self.available() {
            return Ok((BigUint::from(0u64), BigUint::from(0u64)));
        }

        let total_base_amount: f64 = price_levels
            .iter()
            .map(|(_, amount)| amount)
            .sum();
        let total_quote_amount: f64 = price_levels
            .iter()
            .map(|(price, amount)| price * amount)
            .sum();

        let (total_sell_amount, total_buy_amount) = if sell_base {
            (total_base_amount, total_quote_amount)
        } else {
            (total_quote_amount, total_base_amount)
        };

        let sell_limit =
            BigUint::from((total_sell_amount * 10_f64.pow(sell_decimals as f64)) as u128);
        let buy_limit = BigUint::from((total_buy_amount * 10_f64.pow(buy_decimals as f64)) as u128);
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
            .downcast_ref::<BebopState>()
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
impl IndicativelyPriced for BebopState {
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

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;

    fn wbtc() -> Token {
        Token::new(
            &hex::decode("2260fac5e5542a773aa44fbcfedf7c193bc2c599")
                .unwrap()
                .into(),
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
            &hex::decode("a0b86991c6218a76c1d19d4a2e9eb0ce3606eb48")
                .unwrap()
                .into(),
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
            &[],
            Default::default(),
            100,
        )
    }

    fn empty_bebop_client() -> BebopClient {
        BebopClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            "".to_string(),
            HashSet::new(),
            Duration::from_secs(30),
            None,
            None,
            None,
            QuoteRule::OncePerVenue,
        )
        .unwrap()
    }

    fn book(base: &Token, quote: &Token, bids: &[f32], asks: &[f32]) -> BebopPriceData {
        BebopPriceData {
            base: base.address.to_vec(),
            quote: quote.address.to_vec(),
            last_update_ts: 1703097600,
            bids: bids.to_vec(),
            asks: asks.to_vec(),
        }
    }

    /// WBTC/USDC and WETH/USDC books, quoted once per route.
    fn create_test_bebop_state() -> BebopState {
        BebopState::new(
            vec![
                book(
                    &wbtc(),
                    &usdc(),
                    &[65000.0, 1.5, 64950.0, 2.0, 64900.0, 0.5],
                    &[65100.0, 1.0, 65150.0, 2.5, 65200.0, 1.5],
                ),
                book(&weth(), &usdc(), &[3000.0, 2.0, 2900.0, 2.5], &[3100.0, 1.5, 3000.0, 3.0]),
            ],
            HashMap::from([
                (wbtc().address, wbtc()),
                (usdc().address, usdc()),
                (weth().address, weth()),
            ]),
            QuoteRule::OncePerVenue,
            empty_bebop_client(),
        )
    }

    #[test]
    fn test_spot_price_matching_base_and_quote() {
        let state = create_test_bebop_state();
        assert_eq!(
            state
                .spot_price(&wbtc(), &usdc())
                .unwrap(),
            65050.0
        );
    }

    #[test]
    fn test_spot_price_inverted_base_and_quote() {
        let state = create_test_bebop_state();
        let price = state
            .spot_price(&usdc(), &wbtc())
            .unwrap();
        let expected = 0.00001537279;
        assert!((price - expected).abs() < 1e-10);
    }

    #[test]
    fn test_spot_price_empty_asks() {
        let mut state = create_test_bebop_state();
        state.books[0].asks = vec![];
        assert_eq!(
            state
                .spot_price(&wbtc(), &usdc())
                .unwrap(),
            65000.0
        );
    }

    #[test]
    fn test_spot_price_empty_bids() {
        let mut state = create_test_bebop_state();
        state.books[0].bids = vec![];
        assert_eq!(
            state
                .spot_price(&wbtc(), &usdc())
                .unwrap(),
            65100.0
        );
    }

    #[test]
    fn test_spot_price_no_liquidity() {
        let mut state = create_test_bebop_state();
        state.books[0].bids = vec![];
        state.books[0].asks = vec![];
        assert!(state
            .spot_price(&wbtc(), &usdc())
            .is_err());
    }

    #[test]
    fn test_spot_price_other_book() {
        let state = create_test_bebop_state();
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            3050.0
        );
    }

    #[test]
    fn test_get_limits_sell_base_for_quote() {
        let state = create_test_bebop_state();
        let (wbtc_limit, usdc_limit) = state
            .get_limits(wbtc().address, usdc().address)
            .unwrap();
        // Bids: 1.5 + 2.0 + 0.5 = 4.0 WBTC for 97500 + 129900 + 32450 = 259850 USDC.
        assert_eq!(wbtc_limit, BigUint::from(4u64) * BigUint::from(10u64).pow(8u32));
        assert_eq!(usdc_limit, BigUint::from(259850u64) * BigUint::from(10u64).pow(6u32));
    }

    #[test]
    fn test_get_limits_buy_base_with_quote() {
        let state = create_test_bebop_state();
        let (usdc_limit, wbtc_limit) = state
            .get_limits(usdc().address, wbtc().address)
            .unwrap();
        // Asks: 65100 + 162875 + 97800 = 325775 USDC for 1.0 + 2.5 + 1.5 = 5.0 WBTC.
        assert_eq!(usdc_limit, BigUint::from(325775u64) * BigUint::from(10u64).pow(6u32));
        assert_eq!(wbtc_limit, BigUint::from(5u64) * BigUint::from(10u64).pow(8u32));
    }

    #[test]
    fn test_get_limits_no_bids() {
        let mut state = create_test_bebop_state();
        state.books[0].bids = vec![];
        let (token_limit, quote_limit) = state
            .get_limits(wbtc().address, usdc().address)
            .unwrap();
        assert_eq!(token_limit, BigUint::from(0u64));
        assert_eq!(quote_limit, BigUint::from(0u64));
    }

    #[test]
    fn test_get_limits_used_venue() {
        let mut state = create_test_bebop_state();
        state.used = true;
        let (token_limit, quote_limit) = state
            .get_limits(wbtc().address, usdc().address)
            .unwrap();
        assert_eq!(token_limit, BigUint::from(0u64));
        assert_eq!(quote_limit, BigUint::from(0u64));
    }

    #[test]
    fn test_get_limits_invalid_token_pair() {
        let state = create_test_bebop_state();
        let result = state.get_limits(wbtc().address, weth().address);
        assert!(
            matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("Invalid token addresses"))
        );
    }

    #[test]
    fn test_get_amount_out() {
        let state = create_test_bebop_state();

        // swap 3 WETH -> USDC: 6000 from level 1 + 2900 from level 2 = 8900 USDC
        let amount_out_result = state
            .get_amount_out(BigUint::from_str("3_000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        assert_eq!(amount_out_result.amount, BigUint::from_str("8900_000_000").unwrap());

        // swap 7000 USDC -> WETH: 1.5 from level 1 + 0.78333 from level 2 = 2.283333 WETH
        let amount_out_result = state
            .get_amount_out(BigUint::from_str("7000_000_000").unwrap(), &usdc(), &weth())
            .unwrap();
        assert_eq!(amount_out_result.amount, BigUint::from_str("2_283333333333333248").unwrap());
    }

    #[test]
    fn test_get_amount_out_once_per_venue() {
        let state = create_test_bebop_state();
        let first = state
            .get_amount_out(BigUint::from_str("1_000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        let after_first = first
            .new_state
            .as_any()
            .downcast_ref::<BebopState>()
            .unwrap();
        assert!(after_first.used);

        let second = after_first.get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc());
        assert!(
            matches!(second, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
        );
        assert!(after_first
            .spot_price(&wbtc(), &usdc())
            .is_err());
    }

    #[test]
    fn test_get_amount_out_without_rule() {
        let mut state = create_test_bebop_state();
        state.quote_rule = QuoteRule::None;
        let first = state
            .get_amount_out(BigUint::from_str("1_000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        let second = first
            .new_state
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .unwrap();
        assert_eq!(second.amount, BigUint::from(65_000_000_000u64));
    }

    #[test]
    fn test_eq_reads_used() {
        let state = create_test_bebop_state();
        let mut used = state.clone();
        used.used = true;
        assert!(state.eq(&state.clone()));
        assert!(!state.eq(&used));
    }
}
