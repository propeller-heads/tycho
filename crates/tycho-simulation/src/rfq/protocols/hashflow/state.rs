use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt,
};

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
    models::QuoteRule,
    protocols::hashflow::{client::HashflowClient, models::HashflowMakerLevels},
};

/// Hashflow's liquidity on one chain: every market maker's levels on every pair it quotes.
///
/// A swap takes its quote from one market maker and marks that maker used in the state it
/// returns. What a later swap on that state may still take is the venue's [`QuoteRule`] rule:
/// another maker, nothing, or anything. Two firm quotes to one maker are not priced by each
/// other, so the default is another maker.
#[derive(Clone, Serialize, Deserialize)]
pub struct HashflowState {
    /// One entry per market maker and directed pair. The levels sell the pair's base token for
    /// its quote token.
    pub books: Vec<HashflowMakerLevels>,
    /// Every token a book names, by address.
    pub tokens: HashMap<Bytes, Token>,
    pub quote_rule: QuoteRule,
    /// Market makers a swap on this state already took a quote from. Empty as streamed.
    pub used_market_makers: HashSet<String>,
    pub client: HashflowClient,
}

impl fmt::Debug for HashflowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashflowState")
            .field("books", &self.books.len())
            .field("tokens", &self.tokens.len())
            .field("quote_rule", &self.quote_rule)
            .field("used_market_makers", &self.used_market_makers)
            .finish_non_exhaustive()
    }
}

/// What one market maker pays for an amount: the book, the output, and the input it could not
/// fill.
struct Fill<'a> {
    book: &'a HashflowMakerLevels,
    amount_out: f64,
    remaining_amount_in: f64,
}

impl HashflowState {
    pub fn new(
        books: Vec<HashflowMakerLevels>,
        tokens: HashMap<Bytes, Token>,
        quote_rule: QuoteRule,
        client: HashflowClient,
    ) -> Self {
        Self { books, tokens, quote_rule, used_market_makers: HashSet::new(), client }
    }

    fn token(&self, address: &Bytes) -> Result<&Token, SimulationError> {
        self.tokens.get(address).ok_or_else(|| {
            SimulationError::InvalidInput(format!("Hashflow does not quote token {address}"), None)
        })
    }

    /// The books a swap of `token_in` for `token_out` may take a quote from: the pair's books
    /// with levels, less what the reuse rule withholds after the makers this state has used.
    ///
    /// A pair no book names is an invalid input. A pair with no book left has no liquidity.
    fn offers(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Vec<&HashflowMakerLevels>, SimulationError> {
        let mut quoted = false;
        let mut offers = Vec::new();
        for book in &self.books {
            if &book.pair.base_token != token_in || &book.pair.quote_token != token_out {
                continue;
            }
            quoted = true;
            let withheld = match self.quote_rule {
                QuoteRule::OncePerMaker => self
                    .used_market_makers
                    .contains(&book.market_maker),
                QuoteRule::OncePerVenue => !self.used_market_makers.is_empty(),
                QuoteRule::None => false,
            };
            if book.levels.is_empty() || withheld {
                continue;
            }
            offers.push(book);
        }
        if !quoted {
            return Err(SimulationError::InvalidInput(
                format!("Hashflow does not quote {token_in} -> {token_out}"),
                None,
            ));
        }
        if offers.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(offers)
    }

    /// The market maker that pays most for `amount_in`, in whole token units. A maker that fills
    /// the whole amount beats one that fills part of it, whatever the two pay.
    fn best_fill(
        &self,
        amount_in: f64,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Fill<'_>, SimulationError> {
        let mut best: Option<Fill<'_>> = None;
        let mut min_amount = f64::MAX;
        for book in self.offers(token_in, token_out)? {
            // The first level is the smallest amount the maker quotes.
            let maker_min = book.levels[0].quantity;
            if amount_in < maker_min {
                min_amount = min_amount.min(maker_min);
                continue;
            }
            let (amount_out, remaining_amount_in) = book.get_amount_out_from_levels(amount_in);
            let fill = Fill { book, amount_out, remaining_amount_in };
            let better = match &best {
                None => true,
                Some(current) => {
                    (current.remaining_amount_in > 0.0 && fill.remaining_amount_in == 0.0) ||
                        (current.remaining_amount_in > 0.0) == (fill.remaining_amount_in > 0.0) &&
                            fill.amount_out > current.amount_out
                }
            };
            if better {
                best = Some(fill);
            }
        }
        best.ok_or_else(|| {
            SimulationError::RecoverableError(format!(
                "Amount below minimum. Input amount: {amount_in}, min amount: {min_amount}"
            ))
        })
    }

    fn to_whole_units(amount: &BigUint, token: &Token) -> Result<f64, SimulationError> {
        Ok(amount.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10f64.powi(token.decimals as i32))
    }
}

#[typetag::serde]
impl ProtocolSim for HashflowState {
    fn fee(&self) -> f64 {
        todo!()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // Levels are sorted by price, so a book's first level is its best price.
        self.offers(&base.address, &quote.address)?
            .iter()
            .map(|book| book.levels[0].price)
            .reduce(f64::max)
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = Self::to_whole_units(&amount_in, token_in)?;
        let fill = self.best_fill(amount_in, &token_in.address, &token_out.address)?;

        let mut new_state = self.clone();
        new_state
            .used_market_makers
            .insert(fill.book.market_maker.clone());
        let res = GetAmountOutResult {
            amount: BigUint::from_f64(fill.amount_out * 10f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUInt".into())
                })?,
            gas: BigUint::from(151_000u64), // Rough gas estimation
            new_state: Box::new(new_state),
        };

        if fill.remaining_amount_in > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!("Pool has not enough liquidity to support complete swap. Input amount: {amount_in}, consumed amount: {}", amount_in - fill.remaining_amount_in),
                Some(res)));
        }
        Ok(res)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let offers = self.offers(&sell_token, &buy_token)?;
        let sell_decimals = self.token(&sell_token)?.decimals;
        let buy_decimals = self.token(&buy_token)?.decimals;

        let mut total_sell_amount = 0.0;
        let mut total_buy_amount = 0.0;
        for book in offers {
            for level in &book.levels {
                total_sell_amount += level.quantity;
                total_buy_amount += level.quantity * level.price;
            }
        }
        let sell_limit =
            BigUint::from((total_sell_amount * 10_f64.pow(sell_decimals as f64)) as u128);
        let buy_limit = BigUint::from((total_buy_amount * 10_f64.pow(buy_decimals as f64)) as u128);
        Ok((sell_limit, buy_limit))
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        todo!()
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
            .downcast_ref::<HashflowState>()
        {
            self.books == other_state.books &&
                self.used_market_makers == other_state.used_market_makers
        } else {
            false
        }
    }
}

#[async_trait]
impl IndicativelyPriced for HashflowState {
    /// Asks the market maker `get_amount_out` picks for the same amount, and no other: a fallback
    /// maker could be one the route already fills against.
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        let token_in = self.token(&params.token_in)?;
        let amount_in = Self::to_whole_units(&params.amount_in, token_in)?;
        let market_maker = self
            .best_fill(amount_in, &params.token_in, &params.token_out)?
            .book
            .market_maker
            .clone();
        Ok(self
            .client
            .request_binding_quote_from(&params, Some(&market_maker))
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::hashflow::models::{HashflowPair, HashflowPriceLevel};

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

    fn empty_hashflow_client() -> HashflowClient {
        HashflowClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(0),
            Duration::from_secs(30),
            QuoteRule::OncePerMaker,
        )
        .unwrap()
    }

    fn book(
        market_maker: &str,
        base: &Token,
        quote: &Token,
        levels: &[(f64, f64)],
    ) -> HashflowMakerLevels {
        HashflowMakerLevels {
            market_maker: market_maker.to_string(),
            pair: HashflowPair {
                base_token: base.address.clone(),
                quote_token: quote.address.clone(),
            },
            levels: levels
                .iter()
                .map(|&(quantity, price)| HashflowPriceLevel { quantity, price })
                .collect(),
        }
    }

    /// Two makers on WETH/USDC and one on WBTC/USDC. `test_mm_2` pays more for a small amount and
    /// runs out at 2 WETH; `test_mm` holds 7 WETH.
    fn create_test_hashflow_state() -> HashflowState {
        HashflowState::new(
            vec![
                book("test_mm", &weth(), &usdc(), &[(0.5, 3000.0), (1.5, 3000.0), (5.0, 2999.0)]),
                book("test_mm_2", &weth(), &usdc(), &[(0.5, 3010.0), (1.5, 2990.0)]),
                book("test_mm", &wbtc(), &usdc(), &[(1.0, 65000.0)]),
            ],
            HashMap::from([
                (weth().address, weth()),
                (usdc().address, usdc()),
                (wbtc().address, wbtc()),
            ]),
            QuoteRule::OncePerMaker,
            empty_hashflow_client(),
        )
    }

    fn weth_amount(whole: f64) -> BigUint {
        BigUint::from((whole * 1e18) as u128)
    }

    fn usdc_amount(whole: f64) -> BigUint {
        BigUint::from((whole * 1e6) as u128)
    }

    mod spot_price {
        use super::*;

        #[test]
        fn best_first_level_across_makers() {
            let state = create_test_hashflow_state();
            assert_eq!(
                state
                    .spot_price(&weth(), &usdc())
                    .unwrap(),
                3010.0
            );
        }

        #[test]
        fn used_maker_is_skipped() {
            let mut state = create_test_hashflow_state();
            state
                .used_market_makers
                .insert("test_mm_2".to_string());
            assert_eq!(
                state
                    .spot_price(&weth(), &usdc())
                    .unwrap(),
                3000.0
            );
        }

        #[test]
        fn unquoted_pair() {
            let state = create_test_hashflow_state();
            let result = state.spot_price(&usdc(), &weth());
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("does not quote"))
            );
        }

        #[test]
        fn every_maker_used() {
            let mut state = create_test_hashflow_state();
            state.used_market_makers =
                HashSet::from(["test_mm".to_string(), "test_mm_2".to_string()]);
            let result = state.spot_price(&weth(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }
    }

    mod get_amount_out {
        use super::*;

        #[test]
        fn best_paying_maker() {
            let state = create_test_hashflow_state();
            // test_mm_2: 0.5 * 3010 = 1505. test_mm: 0.5 * 3000 = 1500.
            let result = state
                .get_amount_out(weth_amount(0.5), &weth(), &usdc())
                .unwrap();
            assert_eq!(result.amount, usdc_amount(1505.0));
            assert_eq!(result.gas, BigUint::from(151_000u64));
        }

        #[test]
        fn new_state_excludes_the_maker() {
            let state = create_test_hashflow_state();
            let first = state
                .get_amount_out(weth_amount(0.5), &weth(), &usdc())
                .unwrap();
            let after_first = first
                .new_state
                .as_any()
                .downcast_ref::<HashflowState>()
                .unwrap();
            assert_eq!(after_first.used_market_makers, HashSet::from(["test_mm_2".to_string()]));

            let second = after_first
                .get_amount_out(weth_amount(0.5), &weth(), &usdc())
                .unwrap();
            assert_eq!(second.amount, usdc_amount(1500.0), "the second swap goes to test_mm");
            let after_second = second
                .new_state
                .as_any()
                .downcast_ref::<HashflowState>()
                .unwrap();
            assert_eq!(
                after_second.used_market_makers,
                HashSet::from(["test_mm".to_string(), "test_mm_2".to_string()])
            );
        }

        #[test]
        fn used_maker_on_another_pair() {
            let mut state = create_test_hashflow_state();
            state
                .used_market_makers
                .insert("test_mm".to_string());
            let result = state.get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }

        #[test]
        fn once_per_venue() {
            let mut state = create_test_hashflow_state();
            state.quote_rule = QuoteRule::OncePerVenue;
            let first = state
                .get_amount_out(weth_amount(0.5), &weth(), &usdc())
                .unwrap();
            let second =
                first
                    .new_state
                    .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc());
            assert!(
                matches!(second, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }

        #[test]
        fn without_rule() {
            let mut state = create_test_hashflow_state();
            state.quote_rule = QuoteRule::None;
            let first = state
                .get_amount_out(weth_amount(0.5), &weth(), &usdc())
                .unwrap();
            let second = first
                .new_state
                .get_amount_out(weth_amount(0.5), &weth(), &usdc())
                .unwrap();
            assert_eq!(second.amount, usdc_amount(1505.0), "test_mm_2 quotes again");
        }

        #[test]
        fn full_fill_beats_partial_fill() {
            let state = create_test_hashflow_state();
            // test_mm_2 fills 2 of 3 WETH at better prices; test_mm fills all 3:
            // 0.5 * 3000 + 1.5 * 3000 + 1.0 * 2999 = 8999.
            let result = state
                .get_amount_out(weth_amount(3.0), &weth(), &usdc())
                .unwrap();
            assert_eq!(result.amount, usdc_amount(8999.0));
        }

        #[test]
        fn below_every_minimum() {
            let state = create_test_hashflow_state();
            let result = state.get_amount_out(weth_amount(0.25), &weth(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg.contains("Amount below minimum"))
            );
        }

        #[test]
        fn insufficient_liquidity() {
            let state = create_test_hashflow_state();
            let result = state.get_amount_out(weth_amount(8.0), &weth(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, Some(_))) if msg.contains("Pool has not enough liquidity"))
            );
        }

        #[test]
        fn reverse_direction() {
            let state = create_test_hashflow_state();
            let result = state.get_amount_out(usdc_amount(10_000.0), &usdc(), &weth());
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("does not quote"))
            );
        }
    }

    mod get_limits {
        use super::*;

        #[test]
        fn sums_unused_makers() {
            let state = create_test_hashflow_state();
            let (sell_limit, buy_limit) = state
                .get_limits(weth().address, usdc().address)
                .unwrap();
            // test_mm: 7 WETH for 20995 USDC. test_mm_2: 2 WETH for 1505 + 4485 = 5990 USDC.
            assert_eq!(sell_limit, weth_amount(9.0));
            assert_eq!(buy_limit, usdc_amount(26985.0));
        }

        #[test]
        fn used_maker_is_skipped() {
            let mut state = create_test_hashflow_state();
            state
                .used_market_makers
                .insert("test_mm_2".to_string());
            let (sell_limit, buy_limit) = state
                .get_limits(weth().address, usdc().address)
                .unwrap();
            assert_eq!(sell_limit, weth_amount(7.0));
            assert_eq!(buy_limit, usdc_amount(20995.0));
        }

        #[test]
        fn unquoted_pair() {
            let state = create_test_hashflow_state();
            let result = state.get_limits(wbtc().address, weth().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("does not quote"))
            );
        }
    }

    #[test]
    fn test_eq_reads_used_makers() {
        let state = create_test_hashflow_state();
        let mut used = state.clone();
        used.used_market_makers
            .insert("test_mm".to_string());
        assert!(state.eq(&state.clone()));
        assert!(!state.eq(&used));
    }
}
