use std::{any::Any, collections::HashMap, sync::Arc};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::FromPrimitive;
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
    book::sim::{self, SwapDirection},
    rfq::protocols::hashflow::{client::HashflowClient, models::HashflowMarketMakerLevels},
};

/// Rough gas estimate for one Hashflow settlement.
const HASHFLOW_SWAP_GAS: u64 = 134_000;

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct HashflowState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug(skip)]
    pub(super) levels: HashflowMarketMakerLevels,
    pub(super) market_maker: String,
    #[debug(skip)]
    pub(super) client: Arc<HashflowClient>,
}

impl HashflowState {
    /// The levels price one direction only: selling base for quote.
    fn valid_direction_guard(
        &self,
        token_address_in: &Bytes,
        token_address_out: &Bytes,
    ) -> Result<(), SimulationError> {
        SwapDirection::require_base_to_quote(
            &self.base_token.address,
            &self.quote_token.address,
            token_address_in,
            token_address_out,
        )
    }

    fn valid_levels_guard(&self) -> Result<(), SimulationError> {
        if self.levels.levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(())
    }

    /// The pair's base token (the token whose amounts the price levels are quoted in).
    pub fn base_token(&self) -> &Token {
        &self.base_token
    }

    /// The pair's quote token.
    pub fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    /// Smallest `base_token` amount (in base units) Hashflow accepts for a firm quote: the first
    /// price level's quantity. `get_amount_out` fills smaller amounts partially against that
    /// level, but the quote API rejects them, so callers size requests at or above this.
    pub fn min_amount_in(&self) -> BigUint {
        let first_level_quantity = self
            .levels
            .levels
            .first()
            .map_or(0.0, |level| level.quantity);
        let scaled = first_level_quantity * 10f64.powi(self.base_token.decimals as i32);
        BigUint::from_f64(scaled.ceil()).unwrap_or_default()
    }
}

#[typetag::serde]
impl ProtocolSim for HashflowState {
    fn fee(&self) -> f64 {
        todo!()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.valid_direction_guard(&base.address, &quote.address)?;
        // Hashflow's levels are sorted by price, so the first level represents the best price.
        self.levels
            .levels
            .first()
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))
            .map(|level| level.price)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.valid_direction_guard(&token_in.address, &token_out.address)?;
        self.valid_levels_guard()?;
        let amount_in = sim::to_human(&amount_in, token_in.decimals)?;
        // First level represents the minimum amount that can be traded
        let min_amount = self.levels.levels[0].quantity;
        if amount_in < min_amount {
            return Err(SimulationError::RecoverableError(format!(
                "Amount below minimum. Input amount: {amount_in}, min amount: {min_amount}"
            )));
        }
        let fill = self.levels.levels.fill(amount_in);
        // The state doesn't change after a swap.
        sim::fill_result(fill, amount_in, token_out.decimals, HASHFLOW_SWAP_GAS, self.clone_box())
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.valid_direction_guard(&sell_token, &buy_token)?;
        self.valid_levels_guard()?;
        sim::limits(&self.levels.levels, self.base_token.decimals, self.quote_token.decimals)
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
            self.base_token == other_state.base_token &&
                self.quote_token == other_state.quote_token &&
                self.levels == other_state.levels
        } else {
            false
        }
    }
}

#[async_trait]
impl IndicativelyPriced for HashflowState {
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
    use std::str::FromStr;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        book::levels::{Levels, PriceLevel},
        rfq::protocols::hashflow::models::HashflowPair,
    };

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

    fn empty_client() -> Arc<HashflowClient> {
        Arc::new(HashflowClient::new(
            Chain::Ethereum,
            "https://api.hashflow.com/taker/v3/rfq".to_string(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(30),
        ))
    }

    fn create_test_hashflow_state() -> HashflowState {
        HashflowState {
            base_token: weth(),
            quote_token: usdc(),
            levels: HashflowMarketMakerLevels {
                pair: HashflowPair {
                    base_token: Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
                        .unwrap(),
                    quote_token: Bytes::from_str("0xa0b86991c6218a76c1d19d4a2e9eb0ce3606eb48")
                        .unwrap(),
                },
                levels: Levels::new(vec![
                    PriceLevel { quantity: 0.5, price: 3000.0 },
                    PriceLevel { quantity: 1.5, price: 3000.0 },
                    PriceLevel { quantity: 5.0, price: 2999.0 },
                ])
                .unwrap(),
            },
            market_maker: "test_mm".to_string(),
            client: empty_client(),
        }
    }

    #[test]
    fn rejects_tokens_outside_the_pair_and_the_reverse_direction() {
        // Hashflow's levels price one direction only and cannot be inverted, so the reverse
        // pair is as foreign to the state as a token outside it.
        let state = create_test_hashflow_state();
        let amount = BigUint::from_str("1000000000000000000").unwrap();
        let results = [
            state
                .spot_price(&wbtc(), &usdc())
                .map(|_| ()),
            state
                .get_amount_out(amount.clone(), &usdc(), &weth())
                .map(|_| ()),
            state
                .get_amount_out(amount, &wbtc(), &usdc())
                .map(|_| ()),
            state
                .get_limits(wbtc().address.clone(), usdc().address.clone())
                .map(|_| ()),
        ];

        for result in results {
            assert!(
                matches!(&result, Err(SimulationError::InvalidInput(msg, None)) if msg.contains("Invalid token addresses")),
                "{result:?}"
            );
        }
    }

    #[test]
    fn reports_no_liquidity_for_an_empty_ladder() {
        let mut state = create_test_hashflow_state();
        state.levels.levels = Levels::default();
        let results = [
            state
                .spot_price(&weth(), &usdc())
                .map(|_| ()),
            state
                .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &weth(), &usdc())
                .map(|_| ()),
            state
                .get_limits(weth().address.clone(), usdc().address.clone())
                .map(|_| ()),
        ];

        for result in results {
            assert!(
                matches!(&result, Err(SimulationError::RecoverableError(msg)) if msg == "No liquidity"),
                "{result:?}"
            );
        }
    }

    #[test]
    fn spot_price_is_the_first_level() {
        let state = create_test_hashflow_state();
        let price = state
            .spot_price(&state.base_token, &state.quote_token)
            .unwrap();
        assert_eq!(price, 3000.0);
    }

    mod get_amount_out {
        use super::*;

        #[test]
        fn wbtc_to_usdc() {
            let state = create_test_hashflow_state();

            // Test swapping 1.5 WETH -> USDC
            // Should consume first level (0.5 WETH at 3000) + partial second level (1.0 WETH at
            // 3000)
            let amount_out_result = state
                .get_amount_out(
                    BigUint::from_str("1500000000000000000").unwrap(), // 1.5 WETH (18 decimals)
                    &weth(),
                    &usdc(),
                )
                .unwrap();

            // Expected: (0.5 * 3000) + (1.0 * 3000) = 1500 + 3000 = 4500 USDC
            assert_eq!(amount_out_result.amount, BigUint::from_str("4500000000").unwrap()); // 6 decimals
            assert_eq!(amount_out_result.gas, BigUint::from(134_000u64));
        }

        #[test]
        fn below_minimum() {
            let state = create_test_hashflow_state();

            // Test with amount below minimum (first level quantity is 0.5 WETH)
            let result = state.get_amount_out(
                BigUint::from_str("250000000000000000").unwrap(), // 0.25 WETH (18 decimals)
                &weth(),
                &usdc(),
            );

            assert!(result.is_err());
            if let Err(SimulationError::RecoverableError(msg)) = result {
                assert!(msg.contains("Amount below minimum"));
            } else {
                panic!("Expected RecoverableError");
            }
        }

        #[test]
        fn insufficient_liquidity() {
            let state = create_test_hashflow_state();

            // Test with amount exceeding total liquidity (total is 7.0 WETH)
            let result = state.get_amount_out(
                BigUint::from_str("8000000000000000000").unwrap(), // 8.0 WETH (18 decimals)
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

    #[test]
    fn limits_are_the_ladder_totals() {
        let state = create_test_hashflow_state();
        let (sell_limit, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        // Total sell: 0.5 + 1.5 + 5.0 = 7.0 WETH (18 decimals)
        // Total buy: (0.5+1.5)*3000 + 5.0*2999 = 20995 USDC (6 decimals)
        assert_eq!(sell_limit, BigUint::from((7.0 * 10f64.powi(18)) as u128));
        assert_eq!(buy_limit, BigUint::from((20995.0 * 10f64.powi(6)) as u128));
    }
}
