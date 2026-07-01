use std::{any::Any, collections::HashMap, fmt, sync::Arc};

use alloy::primitives::U256;
use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{FromPrimitive, Pow};
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{
        protocol::{GetAmountOutParams, ProtocolComponent},
        token::Token,
    },
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        swap::{
            LimitsParams, MarginalPrice, MarginalPriceParams, QuerySwapParams, Quote, QuoteAmount,
            QuoteParams, Range, SimulationResult, Swap, SwapFee, SwapLimits, SwapQuoter,
            Transition, TransitionParams,
        },
    },
    Bytes,
};

use crate::{
    evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    rfq::{
        client::RFQClient,
        protocols::hashflow::{client::HashflowClient, models::HashflowMarketMakerLevels},
    },
};

#[derive(Clone, Serialize, Deserialize)]
pub struct HashflowState {
    pub base_token: Token,
    pub quote_token: Token,
    pub levels: HashflowMarketMakerLevels,
    pub market_maker: String,
    pub client: HashflowClient,
    #[serde(skip)]
    component: Option<Arc<ProtocolComponent<Arc<Token>>>>,
}

impl fmt::Debug for HashflowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashflowState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .field("market_maker", &self.market_maker)
            .finish_non_exhaustive()
    }
}

impl HashflowState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        levels: HashflowMarketMakerLevels,
        market_maker: String,
        client: HashflowClient,
    ) -> Self {
        Self { base_token, quote_token, levels, market_maker, client, component: None }
    }

    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        self.component = Some(component);
        self
    }

    fn valid_direction_guard(
        &self,
        token_address_in: &Bytes,
        token_address_out: &Bytes,
    ) -> Result<(), SimulationError> {
        // The current levels are only valid for the base/quote pair.
        if !(token_address_in == &self.base_token.address &&
            token_address_out == &self.quote_token.address)
        {
            Err(SimulationError::InvalidInput(
                format!("Invalid token addresses. Got in={token_address_in}, out={token_address_out}, expected in={}, out={}", self.base_token.address, self.quote_token.address),
                None,
            ))
        } else {
            Ok(())
        }
    }

    fn valid_levels_guard(&self) -> Result<(), SimulationError> {
        if self.levels.levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(())
    }

    /// `U256`-native quote core shared by `get_amount_out` and `SwapQuoter::quote`. Returns
    /// `(amount_out, gas, incomplete)` where `incomplete` is true when the levels could not absorb
    /// the full input amount.
    fn quote_core(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(U256, u64, bool), SimulationError> {
        self.valid_direction_guard(token_in, token_out)?;
        self.valid_levels_guard()?;

        let amount_in = u256_to_f64(amount_in)? / 10f64.powi(self.base_token.decimals as i32);

        let min_amount = self.levels.levels[0].quantity;
        if amount_in < min_amount {
            return Err(SimulationError::RecoverableError(format!(
                "Amount below minimum. Input amount: {amount_in}, min amount: {min_amount}"
            )));
        }

        let (amount_out, remaining_amount_in) = self
            .levels
            .get_amount_out_from_levels(amount_in);

        let amount_out =
            BigUint::from_f64(amount_out * 10f64.powi(self.quote_token.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUInt".into())
                })?;

        Ok((biguint_to_u256(&amount_out), 134_000, remaining_amount_in > 0.0))
    }

    /// `U256`-native limits core shared by `get_limits` and `SwapQuoter::swap_limits`.
    fn limits_core(
        &self,
        sell_token: &Bytes,
        buy_token: &Bytes,
    ) -> Result<(U256, U256), SimulationError> {
        self.valid_direction_guard(sell_token, buy_token)?;
        self.valid_levels_guard()?;

        let sell_decimals = self.base_token.decimals;
        let buy_decimals = self.quote_token.decimals;
        let (total_sell_amount, total_buy_amount) =
            self.levels
                .levels
                .iter()
                .fold((0.0, 0.0), |(sell_sum, buy_sum), level| {
                    (sell_sum + level.quantity, buy_sum + level.quantity * level.price)
                });

        let sell_limit = U256::from((total_sell_amount * 10_f64.pow(sell_decimals as f64)) as u128);
        let buy_limit = U256::from((total_buy_amount * 10_f64.pow(buy_decimals as f64)) as u128);
        Ok((sell_limit, buy_limit))
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
        let (amount_out, gas, incomplete) =
            self.quote_core(biguint_to_u256(&amount_in), &token_in.address, &token_out.address)?;
        let res = GetAmountOutResult {
            amount: u256_to_biguint(amount_out),
            gas: BigUint::from(gas),
            new_state: ProtocolSim::clone_box(self), // The state doesn't change after a swap
        };

        if incomplete {
            return Err(SimulationError::InvalidInput(
                "Pool has not enough liquidity to support complete swap".to_string(),
                Some(res),
            ));
        }

        Ok(res)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (sell_limit, buy_limit) = self.limits_core(&sell_token, &buy_token)?;
        Ok((u256_to_biguint(sell_limit), u256_to_biguint(buy_limit)))
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

#[typetag::serde]
impl SwapQuoter for HashflowState {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component.clone().ok_or_else(|| {
            SimulationError::FatalError(
                "HashflowState: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Err(SimulationError::FatalError(
            "HashflowState::fee is not available for RFQ quotes".to_string(),
        ))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component.as_ref().ok_or_else(|| {
            SimulationError::FatalError("HashflowState: component not set".to_string())
        })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;
        let price = self.spot_price(base.as_ref(), quote.as_ref())?;
        Ok(MarginalPrice::new(price))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "HashflowState does not yet support exact-out (FixedOut) quoting".to_string(),
                ))
            }
        };

        let (amount_out, gas, incomplete) =
            self.quote_core(amount_in, params.token_in(), params.token_out())?;
        if incomplete {
            return Err(SimulationError::RecoverableError(
                "Pool has not enough liquidity to support complete swap".to_string(),
            ));
        }
        let new_state = if params.should_return_new_state() {
            Some(Arc::new(self.clone()) as Arc<dyn SwapQuoter>)
        } else {
            None
        };
        Ok(Quote::new(amount_out, gas, new_state))
    }

    fn swap_limits(&self, params: LimitsParams) -> SimulationResult<SwapLimits> {
        let (max_in, max_out) = self.limits_core(params.token_in(), params.token_out())?;
        Ok(SwapLimits::new(Range::new(U256::ZERO, max_in)?, Range::new(U256::ZERO, max_out)?))
    }

    fn query_swap(&self, _params: QuerySwapParams) -> SimulationResult<Swap> {
        Err(SimulationError::FatalError(
            "HashflowState::query_swap is not supported for RFQ quotes".to_string(),
        ))
    }

    fn delta_transition(
        &mut self,
        _params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        Err(TransitionError::SimulationError(SimulationError::FatalError(
            "HashflowState is updated via the RFQ client, not protocol deltas".to_string(),
        )))
    }

    fn clone_box(&self) -> Box<dyn SwapQuoter> {
        Box::new(self.clone())
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }

    #[allow(deprecated)]
    fn to_protocol_sim(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
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
        )
        .unwrap()
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
                levels: vec![
                    HashflowPriceLevel { quantity: 0.5, price: 3000.0 },
                    HashflowPriceLevel { quantity: 1.5, price: 3000.0 },
                    HashflowPriceLevel { quantity: 5.0, price: 2999.0 },
                ],
            },
            market_maker: "test_mm".to_string(),
            client: empty_hashflow_client(),
            component: None,
        }
    }

    mod spot_price {
        use super::*;

        #[test]
        fn returns_best_price() {
            let state = create_test_hashflow_state();
            let price = state
                .spot_price(&state.base_token, &state.quote_token)
                .unwrap();
            // The best price is the first level's price (3000.0)
            assert_eq!(price, 3000.0);
        }

        #[test]
        fn returns_invalid_input_error() {
            let state = create_test_hashflow_state();
            let result = state.spot_price(&wbtc(), &usdc());
            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, _)) = result {
                assert!(msg.contains("Invalid token addresses"));
            } else {
                panic!("Expected InvalidInput");
            }
        }

        #[test]
        fn returns_no_liquidity_error() {
            let mut state = create_test_hashflow_state();
            state.levels.levels.clear();
            let result = state.spot_price(&state.base_token, &state.quote_token);
            assert!(result.is_err());
            if let Err(SimulationError::RecoverableError(msg)) = result {
                assert_eq!(msg, "No liquidity");
            } else {
                panic!("Expected RecoverableError");
            }
        }
    }

    #[test]
    fn test_marginal_price_matches_spot_price() {
        let base = weth();
        let quote = usdc();
        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![base.address.clone(), quote.address.clone()];
        let all_tokens = std::collections::HashMap::from([
            (base.address.clone(), base.clone()),
            (quote.address.clone(), quote.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = create_test_hashflow_state().with_component(component);

        let spot = state.spot_price(&base, &quote).unwrap();
        let marginal = state
            .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
            .unwrap();
        approx::assert_ulps_eq!(marginal.price(), spot);
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
        fn usdc_to_wbtc() {
            let state = create_test_hashflow_state();

            // Test swapping 10000 USDC -> WETH
            // The price levels returned by Hashflow are only valid for the requested pair,
            // and they can't be inverted to derive the reverse swap.
            // In that case, we should return an error.
            let result = state.get_amount_out(
                BigUint::from_str("10000000000").unwrap(), // 10000 USDC (6 decimals)
                &usdc(),
                &weth(),
            );

            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, ..)) = result {
                assert!(msg.contains("Invalid token addresses"));
            } else {
                panic!("Expected InvalidInput");
            }
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

        #[test]
        fn invalid_token_pair() {
            let state = create_test_hashflow_state();

            // Test with invalid token pair (WBTC not in WETH/USDC pool)
            let result = state.get_amount_out(
                BigUint::from_str("100000000").unwrap(), // 1 WBTC
                &wbtc(),
                &usdc(),
            );

            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, ..)) = result {
                assert!(msg.contains("Invalid token addresses"));
            } else {
                panic!("Expected InvalidInput");
            }
        }

        #[test]
        fn no_liquidity() {
            let mut state = create_test_hashflow_state();
            state.levels.levels = vec![]; // Remove all levels

            let result = state.get_amount_out(
                BigUint::from_str("1000000000000000000").unwrap(), // 1.0 WETH
                &weth(),
                &usdc(),
            );

            assert!(result.is_err());
            if let Err(SimulationError::RecoverableError(msg)) = result {
                assert_eq!(msg, "No liquidity");
            } else {
                panic!("Expected RecoverableError");
            }
        }
    }

    mod get_limits {
        use super::*;

        #[test]
        fn valid_limits() {
            let state = create_test_hashflow_state();
            let (sell_limit, buy_limit) = state
                .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
                .unwrap();

            // Total sell: 0.5 + 1.5 + 5.0 = 7.0 WETH (18 decimals)
            // Total buy: (0.5+1.5)*3000 + 5.0*2999 = 20995 USDC (6 decimals)
            assert_eq!(sell_limit, BigUint::from((7.0 * 10f64.powi(18)) as u128));
            assert_eq!(buy_limit, BigUint::from((20995.0 * 10f64.powi(6)) as u128));
        }

        #[test]
        fn invalid_token_pair() {
            let state = create_test_hashflow_state();
            let result =
                state.get_limits(wbtc().address.clone(), state.quote_token.address.clone());
            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, _)) = result {
                assert!(msg.contains("Invalid token addresses"));
            } else {
                panic!("Expected InvalidInput");
            }
        }

        #[test]
        fn no_liquidity() {
            let mut state = create_test_hashflow_state();
            state.levels.levels = vec![];
            let result = state
                .get_limits(state.base_token.address.clone(), state.quote_token.address.clone());
            assert!(result.is_err());
            if let Err(SimulationError::RecoverableError(msg)) = result {
                assert_eq!(msg, "No liquidity");
            } else {
                panic!("Expected RecoverableError");
            }
        }
    }
}
