use std::{any::Any, collections::HashMap, sync::Arc};

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

use crate::{
    book::{
        all_pairs::{all_pairs_state, RefusesQuote, SignedByMakers},
        component::PairState,
        sim::{self, SwapDirection},
    },
    rfq::protocols::liquorice::{client::LiquoriceClient, models::LiquoriceTokenPairPrice},
};

/// Rough gas estimate for one Liquorice settlement.
const LIQUORICE_SWAP_GAS: u64 = 134_000;

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct LiquoriceState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug(skip)]
    pub(super) book: LiquoriceTokenPairPrice,
    pub(super) market_maker: String,
    #[serde(skip)]
    #[debug(skip)]
    client: Option<Arc<LiquoriceClient>>,
}

impl LiquoriceState {
    /// A book this venue prices, with the client its binding quotes go to.
    pub(super) fn new(
        base_token: Token,
        quote_token: Token,
        book: LiquoriceTokenPairPrice,
        market_maker: String,
        client: Arc<LiquoriceClient>,
    ) -> Self {
        LiquoriceState { base_token, quote_token, book, market_maker, client: Some(client) }
    }

    /// The levels price one direction only: selling base for quote.
    fn valid_direction_guard(
        &self,
        token_address_in: &Bytes,
        token_address_out: &Bytes,
    ) -> Result<(), SimulationError> {
        SwapDirection::require_base_to_quote(
            &self.base_token,
            &self.quote_token,
            token_address_in,
            token_address_out,
        )
    }
}

impl PairState for LiquoriceState {
    fn base_token(&self) -> &Token {
        &self.base_token
    }

    fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    /// A maker's ladder prices the pair one way — a taker sells the base token it was published
    /// under — and the venue publishes the other way round as its own book.
    fn directions(&self) -> Vec<(Bytes, Bytes)> {
        sim::directions(&self.base_token, &self.quote_token, !self.book.levels.is_empty(), false)
    }
}

#[typetag::serde]
impl ProtocolSim for LiquoriceState {
    fn fee(&self) -> f64 {
        todo!()
    }

    /// What the maker pays on average over the whole of its ladder.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.valid_direction_guard(&base.address, &quote.address)?;
        self.book
            .average_price()
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // Liquorice refuses a zero amount with `invalid_request: Token amount is less than or
        // equal to zero`.
        sim::require_positive_amount(&amount_in)?;

        self.valid_direction_guard(&token_in.address, &token_out.address)?;
        let amount_in = sim::to_human(&amount_in, token_in.decimals);
        let fill = self.book.levels.fill(amount_in);
        // The state doesn't change after a swap.
        sim::fill_result(fill, amount_in, token_out.decimals, LIQUORICE_SWAP_GAS, self)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.valid_direction_guard(&sell_token, &buy_token)?;
        // Nothing on this side is no swap at any size, so there is no limit to report.
        if self.book.levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        sim::limits(&self.book.levels, self.base_token.decimals, self.quote_token.decimals)
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
            self.base_token == other_state.base_token &&
                self.quote_token == other_state.quote_token &&
                self.market_maker == other_state.market_maker &&
                self.book == other_state.book
        } else {
            false
        }
    }
}

#[async_trait]
impl IndicativelyPriced for LiquoriceState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        Ok(sim::quoting_client(self.client.as_ref(), "Liquorice")?
            .request_binding_quote(&params, &self.market_maker)
            .await?)
    }
}

all_pairs_state! {
    /// Liquorice's books on one chain behind one component: one book per market maker and pair.
    LiquoriceAllPairsState(LiquoriceState, SignedByMakers);
}

impl RefusesQuote for LiquoriceAllPairsState {}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::book::levels::{Levels, PriceLevel};

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

    fn empty_client() -> Arc<LiquoriceClient> {
        Arc::new(LiquoriceClient::new(
            None,
            Chain::Ethereum.id(),
            String::new(),
            String::new(),
            Duration::from_secs(30),
            300,
        ))
    }

    fn create_test_liquorice_state() -> LiquoriceState {
        let base_addr = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let quote_addr = Bytes::from_str("0xa0b86991c6218a76c1d19d4a2e9eb0ce3606eb48").unwrap();
        LiquoriceState {
            base_token: weth(),
            quote_token: usdc(),
            book: LiquoriceTokenPairPrice {
                base_token: base_addr,
                quote_token: quote_addr,
                levels: Levels::new(vec![
                    PriceLevel { quantity: 0.5, price: 3000.0 },
                    PriceLevel { quantity: 1.5, price: 3000.0 },
                    PriceLevel { quantity: 5.0, price: 2999.0 },
                ])
                .unwrap(),
                updated_at: None,
            },
            market_maker: "test_mm".to_string(),
            client: Some(empty_client()),
        }
    }

    /// A Liquorice book prices one direction, so the reverse direction is rejected like a token
    /// outside the pair, by every method.
    #[test]
    fn rejects_tokens_outside_the_pair_and_the_reverse_direction() {
        let state = create_test_liquorice_state();
        let amount = BigUint::from_str("100000000").unwrap();
        let is_invalid_pair = |result: Result<_, SimulationError>| {
            matches!(
                result,
                Err(SimulationError::InvalidInput(msg, _)) if msg.contains("Invalid token addresses")
            )
        };

        assert!(is_invalid_pair(
            state
                .spot_price(&wbtc(), &usdc())
                .map(|_| ())
        ));
        assert!(is_invalid_pair(
            state
                .get_amount_out(amount.clone(), &wbtc(), &usdc())
                .map(|_| ())
        ));
        assert!(is_invalid_pair(
            state
                .get_amount_out(amount, &usdc(), &weth())
                .map(|_| ())
        ));
        assert!(is_invalid_pair(
            state
                .get_limits(wbtc().address.clone(), state.quote_token.address.clone())
                .map(|_| ())
        ));
    }

    mod spot_price {
        use super::*;

        /// 0.5 and 1.5 WETH at 3000 USDC and 5 more at 2999 is 20 995 USDC for the 7 WETH the
        /// ladder holds.
        #[test]
        fn is_what_the_whole_ladder_pays_per_token() {
            let state = create_test_liquorice_state();
            let price = state
                .spot_price(&state.base_token, &state.quote_token)
                .unwrap();
            assert!((price - 20995.0 / 7.0).abs() < 1e-10);
        }

        /// A maker quoting nothing leaves no price to report, and no depth either.
        #[test]
        fn an_empty_book_has_nothing_to_offer() {
            let mut state = create_test_liquorice_state();
            state.book.levels = Levels::default();

            let result = state.spot_price(&state.base_token, &state.quote_token);
            assert!(
                matches!(&result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity"),
                "{result:?}"
            );
            assert!(matches!(
                state.get_limits(
                    state.base_token.address.clone(),
                    state.quote_token.address.clone()
                ),
                Err(SimulationError::RecoverableError(_))
            ));
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
        fn insufficient_liquidity() {
            let state = create_test_liquorice_state();

            // The maker's ladder holds 7 WETH, so 8 exceeds it.
            let result = state.get_amount_out(
                BigUint::from_str("8000000000000000000").unwrap(),
                &weth(),
                &usdc(),
            );

            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, _)) = result {
                assert!(msg.contains("Pool has not enough liquidity"));
            } else {
                panic!("Expected InvalidInput");
            }
        }
    }

    mod get_limits {
        use super::*;

        #[test]
        fn valid_limits() {
            let state = create_test_liquorice_state();
            let (sell_limit, buy_limit) = state
                .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
                .unwrap();

            assert_eq!(sell_limit, BigUint::from((7.0 * 10f64.powi(18)) as u128));
            assert_eq!(buy_limit, BigUint::from((20995.0 * 10f64.powi(6)) as u128));
        }
    }

    /// The venue refuses a swap of nothing, so simulating one must not report a fill.
    #[test]
    fn rejects_a_zero_amount() {
        let state = create_test_liquorice_state();

        let result = state.get_amount_out(
            BigUint::ZERO,
            &state.base_token.clone(),
            &state.quote_token.clone(),
        );

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))), "{result:?}");
    }
}
