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
        protocols::liquorice::{client::LiquoriceClient, models::LiquoriceTokenPairPrice},
    },
};

#[derive(Clone, Serialize, Deserialize)]
pub struct LiquoriceState {
    pub base_token: Token,
    pub quote_token: Token,
    pub prices_by_mm: HashMap<String, LiquoriceTokenPairPrice>,
    pub client: LiquoriceClient,
    #[serde(skip)]
    component: Option<Arc<ProtocolComponent<Arc<Token>>>>,
}

impl fmt::Debug for LiquoriceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mm_names: Vec<&String> = self.prices_by_mm.keys().collect();
        f.debug_struct("LiquoriceState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .field("market_makers", &mm_names)
            .finish_non_exhaustive()
    }
}

impl LiquoriceState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        prices_by_mm: HashMap<String, LiquoriceTokenPairPrice>,
        client: LiquoriceClient,
    ) -> Self {
        Self { base_token, quote_token, prices_by_mm, client, component: None }
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
        if self
            .prices_by_mm
            .values()
            .all(|price| price.levels.is_empty())
        {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        Ok(())
    }

    /// `U256`-native quote core shared by `get_amount_out` and `SwapQuoter::quote`. Returns
    /// `(amount_out, gas, incomplete)`.
    fn quote_core(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(U256, u64, bool), SimulationError> {
        self.valid_direction_guard(token_in, token_out)?;
        self.valid_levels_guard()?;

        let amount_in = u256_to_f64(amount_in)? / 10f64.powi(self.base_token.decimals as i32);

        let (amount_out, remaining_amount_in) = self
            .prices_by_mm
            .values()
            .filter(|price| !price.levels.is_empty())
            .map(|price| price.get_amount_out_from_levels(amount_in))
            .max_by(|a, b| {
                a.0.partial_cmp(&b.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))?;

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
        let (total_sell_amount, total_buy_amount) = self
            .prices_by_mm
            .values()
            .filter(|price| !price.levels.is_empty())
            .map(|price| {
                price
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

        let sell_limit = U256::from((total_sell_amount * 10_f64.pow(sell_decimals as f64)) as u128);
        let buy_limit = U256::from((total_buy_amount * 10_f64.pow(buy_decimals as f64)) as u128);
        Ok((sell_limit, buy_limit))
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
            .filter_map(|price| price.get_price())
            .reduce(f64::max)
            .ok_or(SimulationError::RecoverableError("No liquidity".into()))
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
            new_state: ProtocolSim::clone_box(self),
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

#[typetag::serde]
impl SwapQuoter for LiquoriceState {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component.clone().ok_or_else(|| {
            SimulationError::FatalError(
                "LiquoriceState: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Err(SimulationError::FatalError(
            "LiquoriceState::fee is not available for RFQ quotes".to_string(),
        ))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component.as_ref().ok_or_else(|| {
            SimulationError::FatalError("LiquoriceState: component not set".to_string())
        })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;
        Ok(MarginalPrice::new(self.spot_price(base.as_ref(), quote.as_ref())?))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "LiquoriceState does not yet support exact-out (FixedOut) quoting".to_string(),
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
            "LiquoriceState::query_swap is not supported for RFQ quotes".to_string(),
        ))
    }

    fn delta_transition(
        &mut self,
        _params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        Err(TransitionError::SimulationError(SimulationError::FatalError(
            "LiquoriceState is updated via the RFQ client, not protocol deltas".to_string(),
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
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::liquorice::models::LiquoricePriceLevel;

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
        )
        .unwrap()
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
                levels: vec![
                    LiquoricePriceLevel { quantity: 0.5, price: 3000.0 },
                    LiquoricePriceLevel { quantity: 1.5, price: 3000.0 },
                    LiquoricePriceLevel { quantity: 5.0, price: 2999.0 },
                ],
                updated_at: None,
            },
        );
        prices_by_mm.insert(
            "test_mm_2".to_string(),
            LiquoriceTokenPairPrice {
                base_token: base_addr.clone(),
                quote_token: quote_addr.clone(),
                levels: vec![LiquoricePriceLevel { quantity: 1.0, price: 2998.0 }],
                updated_at: None,
            },
        );
        LiquoriceState {
            base_token: weth(),
            quote_token: usdc(),
            prices_by_mm,
            client: empty_liquorice_client(),
            component: None,
        }
    }

    #[test]
    fn test_marginal_price_matches_spot_price() {
        let state = create_test_liquorice_state();
        let base = state.base_token.clone();
        let quote = state.quote_token.clone();

        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![base.address.clone(), quote.address.clone()];
        let all_tokens = HashMap::from([
            (base.address.clone(), base.clone()),
            (quote.address.clone(), quote.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = state.with_component(component);

        let spot = state.spot_price(&base, &quote).unwrap();
        let marginal = state
            .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
            .unwrap();
        approx::assert_ulps_eq!(marginal.price(), spot);
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
        fn returns_invalid_input_error() {
            let state = create_test_liquorice_state();
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
            let mut state = create_test_liquorice_state();
            state
                .prices_by_mm
                .values_mut()
                .for_each(|price| price.levels.clear());
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
        fn usdc_to_weth() {
            let state = create_test_liquorice_state();

            let result =
                state.get_amount_out(BigUint::from_str("10000000000").unwrap(), &usdc(), &weth());

            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, ..)) = result {
                assert!(msg.contains("Invalid token addresses"));
            } else {
                panic!("Expected InvalidInput");
            }
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

        #[test]
        fn invalid_token_pair() {
            let state = create_test_liquorice_state();

            let result =
                state.get_amount_out(BigUint::from_str("100000000").unwrap(), &wbtc(), &usdc());

            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, ..)) = result {
                assert!(msg.contains("Invalid token addresses"));
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

        #[test]
        fn invalid_token_pair() {
            let state = create_test_liquorice_state();
            let result =
                state.get_limits(wbtc().address.clone(), state.quote_token.address.clone());
            assert!(result.is_err());
            if let Err(SimulationError::InvalidInput(msg, _)) = result {
                assert!(msg.contains("Invalid token addresses"));
            } else {
                panic!("Expected InvalidInput");
            }
        }
    }
}
