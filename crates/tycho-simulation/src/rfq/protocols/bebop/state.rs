use std::{any::Any, collections::HashMap, fmt, sync::Arc};

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
    protocols::bebop::{client::BebopClient, models::BebopPriceData},
};

/// Bebop's liquidity on one chain: one book per pair, bids and asks in one entry.
///
/// Bebop names no market maker and picks the makers behind a firm quote itself, so a swap marks
/// the whole venue used and a later swap on that state finds no liquidity.
#[derive(Clone, Serialize, Deserialize)]
pub struct BebopState {
    /// One entry per pair. A bid takes the pair's base token in; an ask takes its quote token in.
    pub books: Arc<Vec<BebopPriceData>>,
    pub tokens: Arc<HashMap<Bytes, Token>>,
    /// Whether a swap on this state already took Bebop's quote.
    pub used: bool,
    pub client: Arc<BebopClient>,
}

impl fmt::Debug for BebopState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BebopState")
            .field("books", &self.books.len())
            .field("tokens", &self.tokens.len())
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

impl BebopState {
    /// Fails when a book names a token `tokens` does not carry.
    pub fn new(
        books: Vec<BebopPriceData>,
        tokens: HashMap<Bytes, Token>,
        client: BebopClient,
    ) -> Result<Self, SimulationError> {
        for book in &books {
            for address in [&book.base, &book.quote] {
                if !tokens.contains_key(&Bytes::from(address.clone())) {
                    return Err(SimulationError::FatalError(format!(
                        "Bebop book names token 0x{}, which the state does not carry",
                        hex::encode(address)
                    )));
                }
            }
        }
        Ok(Self {
            books: Arc::new(books),
            tokens: Arc::new(tokens),
            used: false,
            client: Arc::new(client),
        })
    }

    /// The book that trades `token_in` for `token_out`, and whether that sells its base token.
    /// A book quoting the pair as given beats one quoting it the other way round.
    fn book(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(&BebopPriceData, bool), SimulationError> {
        let sells_base = |book: &&BebopPriceData| {
            book.base == token_in.as_ref() && book.quote == token_out.as_ref()
        };
        let sells_quote = |book: &&BebopPriceData| {
            book.base == token_out.as_ref() && book.quote == token_in.as_ref()
        };
        if let Some(book) = self.books.iter().find(sells_base) {
            return Ok((book, true));
        }
        if let Some(book) = self.books.iter().find(sells_quote) {
            return Ok((book, false));
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

    fn used_state(&self) -> Self {
        Self {
            books: self.books.clone(),
            tokens: self.tokens.clone(),
            used: true,
            client: self.client.clone(),
        }
    }
}

#[typetag::serde]
impl ProtocolSim for BebopState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if self.used {
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
        if self.used {
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

        let res = GetAmountOutResult {
            amount: BigUint::from_f64(amount_out * 10f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUInt".into())
                })?,
            gas: BigUint::from(70_000u64), // Rough gas estimation
            new_state: Box::new(self.used_state()),
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
        if self.used {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let sell_decimals = self.token(&sell_token)?.decimals;
        let buy_decimals = self.token(&buy_token)?.decimals;

        // If selling BASE for QUOTE, we need to look at [BASE/QUOTE].bids
        // If buying BASE with QUOTE, we need to look at [BASE/QUOTE].asks
        let price_levels = if sell_base { book.get_bids() } else { book.get_asks() };
        if price_levels.is_empty() {
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
    use crate::rfq::protocols::test_utils::{usdc, wbtc, weth};

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

    fn state(books: Vec<BebopPriceData>) -> BebopState {
        BebopState::new(
            books,
            HashMap::from([
                (wbtc().address, wbtc()),
                (usdc().address, usdc()),
                (weth().address, weth()),
            ]),
            empty_bebop_client(),
        )
        .unwrap()
    }

    /// WBTC/USDC and WETH/USDC books.
    fn create_test_bebop_state() -> BebopState {
        state(vec![
            book(
                &wbtc(),
                &usdc(),
                &[65000.0, 1.5, 64950.0, 2.0, 64900.0, 0.5],
                &[65100.0, 1.0, 65150.0, 2.5, 65200.0, 1.5],
            ),
            book(&weth(), &usdc(), &[3000.0, 2.0, 2900.0, 2.5], &[3100.0, 1.5, 3000.0, 3.0]),
        ])
    }

    fn edit_book(state: &mut BebopState, edit: impl FnOnce(&mut BebopPriceData)) {
        edit(&mut Arc::make_mut(&mut state.books)[0]);
    }

    #[test]
    fn test_new_rejects_book_naming_unknown_token() {
        let result = BebopState::new(
            vec![book(&weth(), &usdc(), &[3000.0, 2.0], &[])],
            HashMap::from([(weth().address, weth())]),
            empty_bebop_client(),
        );
        assert!(
            matches!(result, Err(SimulationError::FatalError(msg)) if msg.contains("does not carry"))
        );
    }

    #[test]
    fn test_spot_price_matching_base_and_quote() {
        let state = create_test_bebop_state();

        // Test WBTC/USDC (base/quote) - should use average of best bid and ask
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65050.0);
    }

    #[test]
    fn test_spot_price_inverted_base_and_quote() {
        let state = create_test_bebop_state();

        // Test USDC/WBTC (quote/base) - should use average of best bid and ask, then invert
        let price = state
            .spot_price(&usdc(), &wbtc())
            .unwrap();
        let expected = 0.00001537279;
        assert!((price - expected).abs() < 1e-10);
    }

    #[test]
    fn test_spot_price_empty_asks() {
        let mut state = create_test_bebop_state();
        edit_book(&mut state, |book| book.asks = vec![]);

        // Test WBTC/USDC with no asks - should use only best bid
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65000.0);
    }

    #[test]
    fn test_spot_price_empty_bids() {
        let mut state = create_test_bebop_state();
        edit_book(&mut state, |book| book.bids = vec![]);

        // Test WBTC/USDC with no bids - should use only best ask
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65100.0);
    }

    #[test]
    fn test_spot_price_no_liquidity() {
        let mut state = create_test_bebop_state();
        edit_book(&mut state, |book| {
            book.bids = vec![];
            book.asks = vec![];
        });
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
    fn test_book_quoting_the_direction_beats_the_inverted_one() {
        // The USDC/WETH book pays 1/2048 WETH per USDC; the WETH/USDC book's asks sell 1 WETH for
        // 3100 USDC.
        let state = state(vec![
            book(&weth(), &usdc(), &[3000.0, 2.0], &[3100.0, 1.5]),
            book(&usdc(), &weth(), &[0.00048828125, 2000.0], &[]),
        ]);
        let result = state
            .get_amount_out(BigUint::from(2_000_000_000u64), &usdc(), &weth())
            .unwrap();
        assert_eq!(result.amount, BigUint::from_str("0_976562500000000000").unwrap());
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
        edit_book(&mut state, |book| book.bids = vec![]);
        let (token_limit, quote_limit) = state
            .get_limits(wbtc().address, usdc().address)
            .unwrap();
        assert_eq!(token_limit, BigUint::from(0u64));
        assert_eq!(quote_limit, BigUint::from(0u64));
    }

    #[test]
    fn test_get_limits_used_venue() {
        let state = create_test_bebop_state().used_state();
        let result = state.get_limits(wbtc().address, usdc().address);
        assert!(
            matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
        );
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
        let spot_price = after_first.spot_price(&wbtc(), &usdc());
        assert!(
            matches!(spot_price, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
        );
    }

    #[test]
    fn test_eq_reads_used() {
        let state = create_test_bebop_state();
        assert!(state.eq(&state.clone()));
        assert!(!state.eq(&state.used_state()));
    }
}
