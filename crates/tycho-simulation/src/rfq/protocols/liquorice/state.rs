use std::{any::Any, collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
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

use crate::rfq::protocols::{
    liquorice::client::LiquoriceClient,
    maker_books::{MakerBook, MakerBooks},
};

/// Liquorice's liquidity on one chain: every market maker's levels on every pair it quotes.
///
/// A swap takes its quote from one market maker and marks that maker used in the state it
/// returns. A maker's second quote does not account for its first fill, so by default a later
/// swap on that state goes to another maker.
#[derive(Clone, Serialize, Deserialize)]
pub struct LiquoriceState {
    pub books: MakerBooks,
    pub client: Arc<LiquoriceClient>,
}

impl fmt::Debug for LiquoriceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiquoriceState")
            .field("books", &self.books)
            .field("quote_rule", &self.client.quote_rule())
            .finish_non_exhaustive()
    }
}

impl LiquoriceState {
    pub fn new(
        books: Vec<MakerBook>,
        tokens: HashMap<Bytes, Token>,
        client: LiquoriceClient,
    ) -> Result<Self, SimulationError> {
        let books = MakerBooks::new(books, tokens, false)?;
        Ok(Self { books, client: Arc::new(client) })
    }
}

#[typetag::serde]
impl ProtocolSim for LiquoriceState {
    fn fee(&self) -> f64 {
        todo!()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.books
            .spot_price(self.client.quote_rule(), &base.address, &quote.address)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let fill = self.books.best_fill(
            self.client.quote_rule(),
            &amount_in,
            &token_in.address,
            &token_out.address,
        )?;
        let new_state = Self {
            books: self
                .books
                .with_used(&fill.book.market_maker),
            client: self.client.clone(),
        };
        fill.result(134_000, Box::new(new_state))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.books
            .get_limits(self.client.quote_rule(), &sell_token, &buy_token)
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
        let Some(other) = other
            .as_any()
            .downcast_ref::<LiquoriceState>()
        else {
            return false;
        };
        self.books.books == other.books.books &&
            self.books.used_market_makers == other.books.used_market_makers &&
            self.client.quote_rule() == other.client.quote_rule()
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
        let fill = self.books.best_fill(
            self.client.quote_rule(),
            &params.amount_in,
            &params.token_in,
            &params.token_out,
        )?;
        Ok(self
            .client
            .request_binding_quote_from_maker(&params, &fill.book.market_maker)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::{
        models::{PriceLevel, QuoteRule},
        protocols::{
            liquorice::client::tests::{create_test_quote_params, QUOTE_RESPONSE},
            test_utils::{mock_quote_server, usdc, wbtc, weth},
        },
    };

    fn client(quote_rule: QuoteRule, quote_endpoint: String) -> LiquoriceClient {
        LiquoriceClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(0),
            Duration::from_secs(1),
            300,
            quote_rule,
        )
        .unwrap()
        .with_quote_endpoint(quote_endpoint)
    }

    fn book(market_maker: &str, base: &Token, quote: &Token, levels: &[(f64, f64)]) -> MakerBook {
        MakerBook {
            market_maker: market_maker.to_string(),
            base_token: base.address.clone(),
            quote_token: quote.address.clone(),
            levels: levels
                .iter()
                .map(|&(quantity, price)| PriceLevel { quantity, price })
                .collect(),
        }
    }

    /// `test_mm` holds 7 WETH, `test_mm_2` 1 WETH at a lower price.
    fn test_state(quote_rule: QuoteRule) -> LiquoriceState {
        LiquoriceState::new(
            vec![
                book("test_mm", &weth(), &usdc(), &[(0.5, 3000.0), (1.5, 3000.0), (5.0, 2999.0)]),
                book("test_mm_2", &weth(), &usdc(), &[(1.0, 2998.0)]),
            ],
            HashMap::from([(weth().address, weth()), (usdc().address, usdc())]),
            client(quote_rule, String::new()),
        )
        .unwrap()
    }

    fn weth_amount(whole: f64) -> BigUint {
        BigUint::from((whole * 1e18) as u128)
    }

    #[test]
    fn get_amount_out_marks_the_maker_used() {
        let state = test_state(QuoteRule::OncePerMaker);
        let result = state
            .get_amount_out(weth_amount(1.0), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(3_000_000_000u64));
        assert_eq!(result.gas, BigUint::from(134_000u64));
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<LiquoriceState>()
            .unwrap();
        assert_eq!(new_state.books.used_market_makers, HashSet::from(["test_mm".to_string()]));
    }

    #[test]
    fn once_per_venue() {
        let state = test_state(QuoteRule::OncePerVenue);
        let first = state
            .get_amount_out(weth_amount(1.0), &weth(), &usdc())
            .unwrap();
        let second = first
            .new_state
            .get_amount_out(weth_amount(1.0), &weth(), &usdc());
        assert!(
            matches!(second, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity")
        );
    }

    #[test]
    fn eq_reads_used_makers_and_rule() {
        let state = test_state(QuoteRule::OncePerMaker);
        let used = LiquoriceState {
            books: state.books.with_used("test_mm"),
            client: state.client.clone(),
        };
        let other_rule = test_state(QuoteRule::OncePerVenue);
        assert!(state.eq(&state.clone()));
        assert!(!state.eq(&used));
        assert!(!state.eq(&other_rule));
    }

    #[tokio::test]
    async fn request_signed_quote_takes_the_picked_makers_level() {
        let (addr, _) = mock_quote_server(0, QUOTE_RESPONSE).await;
        // The mock quote holds one level, from `test-maker`, for 1 WETH -> WBTC.
        let state = LiquoriceState::new(
            vec![
                book("test-maker", &weth(), &wbtc(), &[(1.0, 0.051)]),
                book("other", &weth(), &wbtc(), &[(1.0, 0.05)]),
            ],
            HashMap::from([(weth().address, weth()), (wbtc().address, wbtc())]),
            client(QuoteRule::OncePerMaker, format!("http://127.0.0.1:{}/rfq", addr.port())),
        )
        .unwrap();
        let quote = state
            .request_signed_quote(create_test_quote_params())
            .await
            .unwrap();
        assert_eq!(quote.amount_out, BigUint::from(3329502u64));

        let after_first = LiquoriceState {
            books: state.books.with_used("test-maker"),
            client: state.client.clone(),
        };
        let missing = after_first
            .request_signed_quote(create_test_quote_params())
            .await
            .unwrap_err();
        assert!(
            matches!(missing, SimulationError::FatalError(msg) if msg.contains("quote not found"))
        );
    }
}
