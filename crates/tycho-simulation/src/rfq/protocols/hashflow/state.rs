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
    hashflow::client::HashflowClient,
    maker_books::{MakerBook, MakerBooks},
};

/// Hashflow's liquidity on one chain: every market maker's levels on every pair it quotes.
///
/// A swap takes its quote from one market maker and marks that maker used in the state it
/// returns. A maker's second quote does not account for its first fill, so by default a later
/// swap on that state goes to another maker.
#[derive(Clone, Serialize, Deserialize)]
pub struct HashflowState {
    pub books: MakerBooks,
    pub client: Arc<HashflowClient>,
}

impl fmt::Debug for HashflowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashflowState")
            .field("books", &self.books)
            .field("quote_rule", &self.client.quote_rule())
            .finish_non_exhaustive()
    }
}

impl HashflowState {
    pub fn new(
        books: Vec<MakerBook>,
        tokens: HashMap<Bytes, Token>,
        client: HashflowClient,
    ) -> Result<Self, SimulationError> {
        // A Hashflow maker declines an amount below its first level's quantity.
        let books = MakerBooks::new(books, tokens, true)?;
        Ok(Self { books, client: Arc::new(client) })
    }
}

#[typetag::serde]
impl ProtocolSim for HashflowState {
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
        fill.result(151_000, Box::new(new_state))
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
            .downcast_ref::<HashflowState>()
        else {
            return false;
        };
        self.books.books == other.books.books &&
            self.books.used_market_makers == other.books.used_market_makers &&
            self.client.quote_rule() == other.client.quote_rule()
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
            hashflow::client::tests::{create_test_quote_params, QUOTE_RESPONSE},
            test_utils::{mock_quote_server, usdc, wbtc, weth},
        },
    };

    fn client(quote_rule: QuoteRule, quote_endpoint: String) -> HashflowClient {
        HashflowClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(0),
            Duration::from_secs(1),
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

    /// `test_mm_2` pays more for 0.5 WETH; `test_mm` also quotes WBTC.
    fn test_state(quote_rule: QuoteRule, quote_endpoint: String) -> HashflowState {
        HashflowState::new(
            vec![
                book("test_mm", &weth(), &usdc(), &[(0.5, 3000.0), (1.5, 3000.0)]),
                book("test_mm_2", &weth(), &usdc(), &[(0.5, 3010.0)]),
                book("test_mm", &wbtc(), &usdc(), &[(1.0, 65000.0)]),
            ],
            HashMap::from([
                (weth().address, weth()),
                (usdc().address, usdc()),
                (wbtc().address, wbtc()),
            ]),
            client(quote_rule, quote_endpoint),
        )
        .unwrap()
    }

    fn weth_amount(whole: f64) -> BigUint {
        BigUint::from((whole * 1e18) as u128)
    }

    #[test]
    fn get_amount_out_marks_the_maker_used() {
        let state = test_state(QuoteRule::OncePerMaker, String::new());
        let result = state
            .get_amount_out(weth_amount(0.5), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(1_505_000_000u64));
        assert_eq!(result.gas, BigUint::from(151_000u64));
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<HashflowState>()
            .unwrap();
        assert_eq!(new_state.books.used_market_makers, HashSet::from(["test_mm_2".to_string()]));
    }

    #[test]
    fn once_per_venue() {
        let state = test_state(QuoteRule::OncePerVenue, String::new());
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
    fn eq_reads_used_makers_and_rule() {
        let state = test_state(QuoteRule::OncePerMaker, String::new());
        let used =
            HashflowState { books: state.books.with_used("test_mm"), client: state.client.clone() };
        let other_rule = test_state(QuoteRule::OncePerVenue, String::new());
        assert!(state.eq(&state.clone()));
        assert!(!state.eq(&used));
        assert!(!state.eq(&other_rule));
    }

    #[tokio::test]
    async fn request_signed_quote_names_the_picked_maker() {
        let (addr, request_log) = mock_quote_server(0, QUOTE_RESPONSE).await;
        // The mock quote is for 1 WETH -> WBTC, as `create_test_quote_params` asks.
        let state = HashflowState::new(
            vec![
                book("test_mm", &weth(), &wbtc(), &[(1.0, 0.05)]),
                book("test_mm_2", &weth(), &wbtc(), &[(1.0, 0.051)]),
            ],
            HashMap::from([(weth().address, weth()), (wbtc().address, wbtc())]),
            client(QuoteRule::OncePerMaker, format!("http://127.0.0.1:{}/rfq", addr.port())),
        )
        .unwrap();
        state
            .request_signed_quote(create_test_quote_params())
            .await
            .unwrap();
        let after_first = HashflowState {
            books: state.books.with_used("test_mm_2"),
            client: state.client.clone(),
        };
        after_first
            .request_signed_quote(create_test_quote_params())
            .await
            .unwrap();

        let requests = request_log.lock().unwrap();
        assert!(requests[0].contains("\"marketMakers\":[\"test_mm_2\"]"), "{}", requests[0]);
        assert!(requests[1].contains("\"marketMakers\":[\"test_mm\"]"), "{}", requests[1]);
    }
}
