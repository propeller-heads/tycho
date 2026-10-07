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
    book::sim::{self, SwapDirection},
    rfq::protocols::liquorice::{client::LiquoriceClient, models::LiquoriceTokenPairPrice},
};

/// Rough gas estimate for one Liquorice settlement.
const LIQUORICE_SWAP_GAS: u64 = 134_000;

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct LiquoriceState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    /// Printed as the makers' names only; the ladders are too long for a log line.
    #[debug("{:?}", prices_by_mm.keys().collect::<Vec<_>>())]
    pub(super) prices_by_mm: HashMap<String, LiquoriceTokenPairPrice>,
    #[debug(skip)]
    pub(super) client: Arc<LiquoriceClient>,
}

impl LiquoriceState {
    /// The pair's base token (the token whose amounts the price levels are quoted in).
    pub fn base_token(&self) -> &Token {
        &self.base_token
    }

    /// The pair's quote token.
    pub fn quote_token(&self) -> &Token {
        &self.quote_token
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

    fn valid_levels_guard(&self) -> Result<(), SimulationError> {
        if self
            .prices_by_mm
            .values()
            .all(|price| price.levels.is_empty())
        {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(())
    }

    /// The market makers' ladders that carry liquidity.
    fn populated_books(&self) -> impl Iterator<Item = &LiquoriceTokenPairPrice> {
        self.prices_by_mm
            .values()
            .filter(|price| !price.levels.is_empty())
    }
}

#[typetag::serde]
impl ProtocolSim for LiquoriceState {
    fn fee(&self) -> f64 {
        todo!()
    }

    /// Returns the best available price across all market makers
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.valid_direction_guard(&base.address, &quote.address)?;
        self.prices_by_mm
            .values()
            .filter_map(|price| price.average_price())
            .reduce(f64::max)
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.valid_direction_guard(&token_in.address, &token_out.address)?;
        self.valid_levels_guard()?;
        let amount_in = sim::to_human(&amount_in, token_in.decimals);
        // The market maker paying the most for amount_in fills the swap.
        let fill = self
            .populated_books()
            .map(|price| price.levels.fill(amount_in))
            .max_by(|a, b| {
                a.amount_out
                    .partial_cmp(&b.amount_out)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))?;
        sim::fill_result(fill, amount_in, token_out.decimals, LIQUORICE_SWAP_GAS, self.clone_box())
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.valid_direction_guard(&sell_token, &buy_token)?;
        self.valid_levels_guard()?;
        // The limits are those of the deepest market maker's ladder.
        let deepest = self
            .populated_books()
            .max_by(|a, b| {
                a.levels
                    .notional()
                    .partial_cmp(&b.levels.notional())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))?;
        sim::limits(&deepest.levels, self.base_token.decimals, self.quote_token.decimals)
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
                self.prices_by_mm == other_state.prices_by_mm
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
            Chain::Ethereum,
            "https://api.liquorice.tech/v1/solver/rfq".to_string(),
            "https://api.liquorice.tech/v1/solver/price-levels".to_string(),
            "".to_string(),
            "".to_string(),
            Duration::from_secs(30),
            300,
        ))
    }

    fn create_test_liquorice_state() -> LiquoriceState {
        let base_addr = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let quote_addr = Bytes::from_str("0xa0b86991c6218a76c1d19d4a2e9eb0ce3606eb48").unwrap();
        let mut prices_by_mm = HashMap::new();
        prices_by_mm.insert(
            "test_mm".to_string(),
            LiquoriceTokenPairPrice {
                base_token: base_addr.clone(),
                quote_token: quote_addr.clone(),
                levels: Levels::new(vec![
                    PriceLevel { quantity: 0.5, price: 3000.0 },
                    PriceLevel { quantity: 1.5, price: 3000.0 },
                    PriceLevel { quantity: 5.0, price: 2999.0 },
                ])
                .unwrap(),
                updated_at: None,
            },
        );
        prices_by_mm.insert(
            "test_mm_2".to_string(),
            LiquoriceTokenPairPrice {
                base_token: base_addr.clone(),
                quote_token: quote_addr.clone(),
                levels: Levels::new(vec![PriceLevel { quantity: 1.0, price: 2998.0 }]).unwrap(),
                updated_at: None,
            },
        );
        LiquoriceState {
            base_token: weth(),
            quote_token: usdc(),
            prices_by_mm,
            client: empty_client(),
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

        #[test]
        fn returns_best_price() {
            let state = create_test_liquorice_state();
            let price = state
                .spot_price(&state.base_token, &state.quote_token)
                .unwrap();
            assert!((price - 20995.0 / 7.0).abs() < 1e-10);
        }

        #[test]
        fn returns_no_liquidity_error() {
            let mut state = create_test_liquorice_state();
            state
                .prices_by_mm
                .values_mut()
                .for_each(|price| price.levels = Levels::default());
            let result = state.spot_price(&state.base_token, &state.quote_token);
            assert!(result.is_err());
            if let Err(SimulationError::RecoverableError(msg)) = result {
                assert_eq!(msg, "No liquidity");
            } else {
                panic!("Expected RecoverableError");
            }
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

            // Best single maker (test_mm) has 7.0 capacity, so 8 WETH exceeds it
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
}
