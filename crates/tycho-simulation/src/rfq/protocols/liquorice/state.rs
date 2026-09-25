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
    protocols::liquorice::{client::LiquoriceClient, models::LiquoriceMakerLevels},
};

/// Liquorice's liquidity on one chain: every market maker's levels on every pair it quotes.
///
/// A swap takes its quote from one market maker and marks that maker used in the state it
/// returns. What a later swap on that state may still take is the venue's [`QuoteRule`] rule:
/// another maker, nothing, or anything.
#[derive(Clone, Serialize, Deserialize)]
pub struct LiquoriceState {
    /// One entry per market maker and directed pair. The levels sell the pair's base token for
    /// its quote token.
    pub books: Vec<LiquoriceMakerLevels>,
    /// Every token a book names, by address.
    pub tokens: HashMap<Bytes, Token>,
    pub quote_rule: QuoteRule,
    /// Market makers a swap on this state already took a quote from. Empty as streamed.
    pub used_market_makers: HashSet<String>,
    pub client: LiquoriceClient,
}

impl fmt::Debug for LiquoriceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiquoriceState")
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
    book: &'a LiquoriceMakerLevels,
    amount_out: f64,
    remaining_amount_in: f64,
}

impl LiquoriceState {
    pub fn new(
        books: Vec<LiquoriceMakerLevels>,
        tokens: HashMap<Bytes, Token>,
        quote_rule: QuoteRule,
        client: LiquoriceClient,
    ) -> Self {
        Self { books, tokens, quote_rule, used_market_makers: HashSet::new(), client }
    }

    fn token(&self, address: &Bytes) -> Result<&Token, SimulationError> {
        self.tokens.get(address).ok_or_else(|| {
            SimulationError::InvalidInput(format!("Liquorice does not quote token {address}"), None)
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
    ) -> Result<Vec<&LiquoriceMakerLevels>, SimulationError> {
        let mut quoted = false;
        let mut offers = Vec::new();
        for book in &self.books {
            if &book.price.base_token != token_in || &book.price.quote_token != token_out {
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
            if book.price.levels.is_empty() || withheld {
                continue;
            }
            offers.push(book);
        }
        if !quoted {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Invalid token addresses. Liquorice does not quote {token_in} -> {token_out}"
                ),
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
        for book in self.offers(token_in, token_out)? {
            let (amount_out, remaining_amount_in) = book
                .price
                .get_amount_out_from_levels(amount_in);
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
        best.ok_or_else(|| SimulationError::RecoverableError("No liquidity".into()))
    }

    fn to_whole_units(amount: &BigUint, token: &Token) -> Result<f64, SimulationError> {
        Ok(amount.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10f64.powi(token.decimals as i32))
    }
}

#[typetag::serde]
impl ProtocolSim for LiquoriceState {
    fn fee(&self) -> f64 {
        todo!()
    }

    /// The best price any unused market maker quotes on the pair.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.offers(&base.address, &quote.address)?
            .iter()
            .filter_map(|book| book.price.get_price())
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
            gas: BigUint::from(134_000u64),
            new_state: Box::new(new_state),
        };

        if fill.remaining_amount_in > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!("Pool has not enough liquidity to support complete swap. Input amount: {amount_in}, consumed amount: {}", amount_in - fill.remaining_amount_in),
                Some(res)));
        }
        Ok(res)
    }

    /// The limits of the single unused market maker that holds the most on the pair: a firm
    /// quote fills from one maker.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let offers = self.offers(&sell_token, &buy_token)?;
        let sell_decimals = self.token(&sell_token)?.decimals;
        let buy_decimals = self.token(&buy_token)?.decimals;

        let (total_sell_amount, total_buy_amount) = offers
            .iter()
            .map(|book| {
                book.price
                    .levels
                    .iter()
                    .fold((0.0, 0.0), |(sell_sum, buy_sum), level| {
                        (sell_sum + level.quantity, buy_sum + level.quantity * level.price)
                    })
            })
            .max_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))?;

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
            .downcast_ref::<LiquoriceState>()
        {
            self.books == other_state.books &&
                self.used_market_makers == other_state.used_market_makers
        } else {
            false
        }
    }
}

#[async_trait]
impl IndicativelyPriced for LiquoriceState {
    /// Takes the level of the market maker `get_amount_out` picks for the same amount, and no
    /// other: another maker's level could be one the route already fills against.
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
    use crate::rfq::protocols::liquorice::models::{LiquoricePriceLevel, LiquoriceTokenPairPrice};

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

    fn empty_liquorice_client() -> LiquoriceClient {
        LiquoriceClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(0),
            Duration::from_secs(30),
            300,
            QuoteRule::OncePerMaker,
        )
        .unwrap()
    }

    fn book(
        market_maker: &str,
        base: &Token,
        quote: &Token,
        levels: &[(f64, f64)],
    ) -> LiquoriceMakerLevels {
        LiquoriceMakerLevels {
            market_maker: market_maker.to_string(),
            price: LiquoriceTokenPairPrice {
                base_token: base.address.clone(),
                quote_token: quote.address.clone(),
                levels: levels
                    .iter()
                    .map(|&(quantity, price)| LiquoricePriceLevel { quantity, price })
                    .collect(),
                updated_at: None,
            },
        }
    }

    /// Two makers on WETH/USDC. `test_mm` holds 7 WETH, `test_mm_2` 1 WETH at a lower price.
    fn create_test_liquorice_state() -> LiquoriceState {
        LiquoriceState::new(
            vec![
                book("test_mm", &weth(), &usdc(), &[(0.5, 3000.0), (1.5, 3000.0), (5.0, 2999.0)]),
                book("test_mm_2", &weth(), &usdc(), &[(1.0, 2998.0)]),
            ],
            HashMap::from([(weth().address, weth()), (usdc().address, usdc())]),
            QuoteRule::OncePerMaker,
            empty_liquorice_client(),
        )
    }

    mod spot_price {
        use super::*;

        #[test]
        fn returns_best_price() {
            let state = create_test_liquorice_state();
            let price = state
                .spot_price(&weth(), &usdc())
                .unwrap();
            assert!((price - 20995.0 / 7.0).abs() < 1e-10);
        }

        #[test]
        fn returns_invalid_input_error() {
            let state = create_test_liquorice_state();
            let result = state.spot_price(&wbtc(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("Invalid token addresses"))
            );
        }

        #[test]
        fn returns_no_liquidity_error() {
            let mut state = create_test_liquorice_state();
            for book in &mut state.books {
                book.price.levels.clear();
            }
            let result = state.spot_price(&weth(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }
    }

    mod get_amount_out {
        use super::*;

        #[test]
        fn weth_to_usdc() {
            let state = create_test_liquorice_state();
            let amount_out_result = state
                .get_amount_out(BigUint::from_str("1500000000000000000").unwrap(), &weth(), &usdc())
                .unwrap();
            assert_eq!(amount_out_result.amount, BigUint::from_str("4500000000").unwrap());
            assert_eq!(amount_out_result.gas, BigUint::from(134_000u64));
        }

        #[test]
        fn new_state_excludes_the_maker() {
            let state = create_test_liquorice_state();
            let first = state
                .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &weth(), &usdc())
                .unwrap();
            let after_first = first
                .new_state
                .as_any()
                .downcast_ref::<LiquoriceState>()
                .unwrap();
            assert_eq!(after_first.used_market_makers, HashSet::from(["test_mm".to_string()]));

            let second = after_first
                .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &weth(), &usdc())
                .unwrap();
            assert_eq!(second.amount, BigUint::from_str("2998000000").unwrap(), "test_mm_2 quotes");
        }

        #[test]
        fn once_per_venue() {
            let mut state = create_test_liquorice_state();
            state.quote_rule = QuoteRule::OncePerVenue;
            let first = state
                .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &weth(), &usdc())
                .unwrap();
            let second = first.new_state.get_amount_out(
                BigUint::from_str("1000000000000000000").unwrap(),
                &weth(),
                &usdc(),
            );
            assert!(
                matches!(second, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
            );
        }

        #[test]
        fn usdc_to_weth() {
            let state = create_test_liquorice_state();
            let result =
                state.get_amount_out(BigUint::from_str("10000000000").unwrap(), &usdc(), &weth());
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("Invalid token addresses"))
            );
        }

        #[test]
        fn insufficient_liquidity() {
            let state = create_test_liquorice_state();
            let result = state.get_amount_out(
                BigUint::from_str("8000000000000000000").unwrap(),
                &weth(),
                &usdc(),
            );
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, Some(_))) if msg.contains("Pool has not enough liquidity"))
            );
        }

        #[test]
        fn invalid_token_pair() {
            let state = create_test_liquorice_state();
            let result =
                state.get_amount_out(BigUint::from_str("100000000").unwrap(), &wbtc(), &usdc());
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("Invalid token addresses"))
            );
        }
    }

    mod get_limits {
        use super::*;

        #[test]
        fn valid_limits() {
            let state = create_test_liquorice_state();
            let (sell_limit, buy_limit) = state
                .get_limits(weth().address, usdc().address)
                .unwrap();
            assert_eq!(sell_limit, BigUint::from((7.0 * 10f64.powi(18)) as u128));
            assert_eq!(buy_limit, BigUint::from((20995.0 * 10f64.powi(6)) as u128));
        }

        #[test]
        fn used_maker_is_skipped() {
            let mut state = create_test_liquorice_state();
            state
                .used_market_makers
                .insert("test_mm".to_string());
            let (sell_limit, buy_limit) = state
                .get_limits(weth().address, usdc().address)
                .unwrap();
            assert_eq!(sell_limit, BigUint::from((1.0 * 10f64.powi(18)) as u128));
            assert_eq!(buy_limit, BigUint::from((2998.0 * 10f64.powi(6)) as u128));
        }

        #[test]
        fn invalid_token_pair() {
            let state = create_test_liquorice_state();
            let result = state.get_limits(wbtc().address, usdc().address);
            assert!(
                matches!(result, Err(SimulationError::InvalidInput(msg, _)) if msg.contains("Invalid token addresses"))
            );
        }
    }
}
