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
        protocols::bebop::{client::BebopClient, models::BebopPriceData},
    },
};

#[derive(Clone, Serialize, Deserialize)]
pub struct BebopState {
    pub base_token: Token,
    pub quote_token: Token,
    pub price_data: BebopPriceData,
    pub client: BebopClient,
    #[serde(skip)]
    component: Option<Arc<ProtocolComponent<Arc<Token>>>>,
}

impl fmt::Debug for BebopState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BebopState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .finish_non_exhaustive()
    }
}

impl BebopState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        price_data: BebopPriceData,
        client: BebopClient,
    ) -> Self {
        BebopState { base_token, quote_token, price_data, client, component: None }
    }

    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        self.component = Some(component);
        self
    }

    /// `U256`-native quote core shared by `get_amount_out` and `SwapQuoter::quote`. Returns
    /// `(amount_out, gas, incomplete)`.
    fn quote_core(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(U256, u64, bool), SimulationError> {
        let sell_base = if token_in == &self.base_token.address &&
            token_out == &self.quote_token.address
        {
            true
        } else if token_in == &self.quote_token.address && token_out == &self.base_token.address {
            false
        } else {
            return Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {token_in}, {token_out}"
            )));
        };

        let price_levels = if sell_base {
            self.price_data.get_bids()
        } else {
            self.price_data
                .get_asks()
                .iter()
                .map(|(price, size)| (1.0 / price, price * size))
                .collect()
        };
        if price_levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let (token_in_decimals, token_out_decimals) = if sell_base {
            (self.base_token.decimals, self.quote_token.decimals)
        } else {
            (self.quote_token.decimals, self.base_token.decimals)
        };

        let amount_in = u256_to_f64(amount_in)? / 10f64.powi(token_in_decimals as i32);
        let (amount_out, remaining_amount_in) = self
            .price_data
            .get_amount_out_from_levels(amount_in, price_levels);
        let amount_out = BigUint::from_f64(amount_out * 10f64.powi(token_out_decimals as i32))
            .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert amount out to BigUInt".into())
            })?;

        Ok((biguint_to_u256(&amount_out), 70_000, remaining_amount_in > 0.0))
    }

    /// `U256`-native limits core shared by `get_limits` and `SwapQuoter::swap_limits`.
    fn limits_core(
        &self,
        sell_token: &Bytes,
        buy_token: &Bytes,
    ) -> Result<(U256, U256), SimulationError> {
        let (sell_decimals, buy_decimals, price_levels) = if sell_token == &self.base_token.address &&
            buy_token == &self.quote_token.address
        {
            (self.base_token.decimals, self.quote_token.decimals, self.price_data.get_bids())
        } else if buy_token == &self.base_token.address && sell_token == &self.quote_token.address {
            (self.quote_token.decimals, self.base_token.decimals, self.price_data.get_asks())
        } else {
            return Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {sell_token}, {buy_token}"
            )));
        };

        if price_levels.is_empty() {
            return Ok((U256::ZERO, U256::ZERO));
        }

        let total_base_amount: f64 = price_levels
            .iter()
            .map(|(_, amount)| amount)
            .sum();
        let total_quote_amount: f64 = price_levels
            .iter()
            .map(|(price, amount)| price * amount)
            .sum();

        let (total_sell_amount, total_buy_amount) =
            if sell_token == &self.base_token.address && buy_token == &self.quote_token.address {
                (total_base_amount, total_quote_amount)
            } else {
                (total_quote_amount, total_base_amount)
            };

        let sell_limit = U256::from((total_sell_amount * 10_f64.pow(sell_decimals as f64)) as u128);
        let buy_limit = U256::from((total_buy_amount * 10_f64.pow(buy_decimals as f64)) as u128);
        Ok((sell_limit, buy_limit))
    }
}

#[typetag::serde]
impl ProtocolSim for BebopState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // Since this method does not care about sell direction, we average the price of the best
        // bid and ask
        let best_bid = self
            .price_data
            .get_bids()
            .first()
            .map(|(price, _)| *price);
        let best_ask = self
            .price_data
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

        // If the base/quote token addresses are the opposite of the pool tokens, we need to invert
        // the price
        if base.address == self.quote_token.address && quote.address == self.base_token.address {
            Ok(1.0 / average_price)
        } else if quote.address == self.quote_token.address &&
            base.address == self.base_token.address
        {
            Ok(average_price)
        } else {
            Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {}, {}",
                base.address, quote.address
            )))
        }
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
            self.base_token == other_state.base_token &&
                self.quote_token == other_state.quote_token &&
                self.price_data == other_state.price_data
        } else {
            false
        }
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[typetag::serde]
impl SwapQuoter for BebopState {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component.clone().ok_or_else(|| {
            SimulationError::FatalError(
                "BebopState: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Ok(SwapFee::new(0.0))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component.as_ref().ok_or_else(|| {
            SimulationError::FatalError("BebopState: component not set".to_string())
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
                    "BebopState does not yet support exact-out (FixedOut) quoting".to_string(),
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
            "BebopState::query_swap is not supported for RFQ quotes".to_string(),
        ))
    }

    fn delta_transition(
        &mut self,
        _params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        Err(TransitionError::DecodeError("Not implemented".into()))
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

    fn empty_bebop_client() -> BebopClient {
        BebopClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            "".to_string(),
            "".to_string(),
            HashSet::new(),
            Duration::from_secs(30),
        )
        .unwrap()
    }

    fn create_test_bebop_state() -> BebopState {
        BebopState {
            base_token: wbtc(),
            quote_token: usdc(),
            price_data: BebopPriceData {
                base: hex::decode("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(), // WBTC
                quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
                last_update_ts: 1703097600,
                bids: vec![65000.0f32, 1.5f32, 64950.0f32, 2.0f32, 64900.0f32, 0.5f32],
                asks: vec![65100.0f32, 1.0f32, 65150.0f32, 2.5f32, 65200.0f32, 1.5f32],
            },
            client: empty_bebop_client(),
            component: None,
        }
    }

    #[test]
    fn test_marginal_price_matches_spot_price() {
        let wbtc = wbtc();
        let usdc = usdc();
        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![wbtc.address.clone(), usdc.address.clone()];
        let all_tokens = std::collections::HashMap::from([
            (wbtc.address.clone(), wbtc.clone()),
            (usdc.address.clone(), usdc.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = create_test_bebop_state().with_component(component);

        for (base, quote) in [(&wbtc, &usdc), (&usdc, &wbtc)] {
            let spot = state.spot_price(base, quote).unwrap();
            let marginal = state
                .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
                .unwrap();
            approx::assert_ulps_eq!(marginal.price(), spot);
        }
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
        state.price_data.asks = vec![]; // Remove all asks

        // Test WBTC/USDC with no asks - should use only best bid
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65000.0);
    }

    #[test]
    fn test_spot_price_empty_bids() {
        let mut state = create_test_bebop_state();
        state.price_data.bids = vec![]; // Remove all bids
                                        // Test WBTC/USDC with no bids - should use only best ask
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65100.0);
    }

    #[test]
    fn test_spot_price_no_liquidity() {
        let mut state = create_test_bebop_state();
        state.price_data.bids = vec![]; // Remove all bids
        state.price_data.asks = vec![]; // Remove all asks
                                        // Test with no liquidity at all - should return error
        let result = state.spot_price(&wbtc(), &usdc());
        assert!(result.is_err());
    }

    #[test]
    fn test_get_limits_sell_base_for_quote() {
        let state = create_test_bebop_state();

        // Test selling WBTC for USDC (should use bids)
        let (wbtc_limit, usdc_limit) = state
            .get_limits(wbtc().address.clone(), usdc().address.clone())
            .unwrap();

        // Use bids: vec![(65000.0, 1.5), (64950.0, 2.0), (64900.0, 0.5)]

        // Total WBTC available: 1.5 + 2.0 + 0.5 = 4.0 WBTC
        let expected_wbtc_limit = BigUint::from(4u64) * BigUint::from(10u64).pow(8u32);

        // Total USDC value: (65000*1.5) + (64950*2.0) + (64900*0.5) = 97500 + 129900 + 32450 =
        // 259850
        let expected_usdc_limit = BigUint::from(259850u64) * BigUint::from(10u64).pow(6u32);

        assert_eq!(wbtc_limit, expected_wbtc_limit);
        assert_eq!(usdc_limit, expected_usdc_limit);
    }

    #[test]
    fn test_get_limits_buy_base_with_quote() {
        let state = create_test_bebop_state();

        // Test buying WBTC with USDC (should use asks)
        let (usdc_limit, wbtc_limit) = state
            .get_limits(usdc().address.clone(), wbtc().address.clone())
            .unwrap();

        // Use asks: vec![(65100.0, 1.0), (65150.0, 2.5), (65200.0, 1.5)]

        // Total USDC needed: (65100*1.0) + (65150*2.5) + (65200*1.5) = 65100 + 162875 + 97800 =
        // 325775
        let expected_usdc_limit = BigUint::from(325775u64) * BigUint::from(10u64).pow(6u32);

        // Total WBTC available: 1.0 + 2.5 + 1.5 = 5.0 WBTC
        let expected_wbtc_limit = BigUint::from(5u64) * BigUint::from(10u64).pow(8u32);

        assert_eq!(usdc_limit, expected_usdc_limit);
        assert_eq!(wbtc_limit, expected_wbtc_limit);
    }

    #[test]
    fn test_get_limits_no_bids() {
        let mut state = create_test_bebop_state();
        state.price_data.bids = vec![]; // Remove all bids

        // Test selling WBTC for USDC with no bids - should return 0
        let (token_limit, quote_limit) = state
            .get_limits(wbtc().address.clone(), usdc().address.clone())
            .unwrap();

        assert_eq!(token_limit, BigUint::from(0u64));
        assert_eq!(quote_limit, BigUint::from(0u64));
    }

    #[test]
    fn test_get_limits_no_asks() {
        let mut state = create_test_bebop_state();
        state.price_data.asks = vec![]; // Remove all asks

        // Test buying WBTC with USDC with no asks - should return 0
        let (token_limit, quote_limit) = state
            .get_limits(usdc().address.clone(), wbtc().address.clone())
            .unwrap();

        assert_eq!(token_limit, BigUint::from(0u64));
        assert_eq!(quote_limit, BigUint::from(0u64));
    }

    #[test]
    fn test_get_limits_invalid_token_pair() {
        let state = create_test_bebop_state();

        // Create a different token (not WBTC or USDC)
        let eth = Token::new(
            &hex::decode("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
                .unwrap()
                .into(),
            "ETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        // Test with invalid token pair (ETH not in WBTC/USDC pool) - should return error
        let result = state.get_limits(eth.address.clone(), usdc().address.clone());
        assert!(result.is_err());

        if let Err(SimulationError::RecoverableError(msg)) = result {
            assert!(msg.contains("Invalid token addresses"));
        } else {
            panic!("Expected RecoverableError with invalid token addresses message");
        }
    }

    #[test]
    fn test_get_amount_out() {
        // WETH/USDC
        let price_data = BebopPriceData {
            base: hex::decode("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(), // WETH
            quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            last_update_ts: 1234567890,
            bids: vec![3000.0f32, 2.0f32, 2900.0f32, 2.5f32],
            asks: vec![3100.0f32, 1.5f32, 3000.0f32, 3.0f32],
        };

        let weth = weth();
        let usdc = usdc();
        let state = BebopState::new(weth.clone(), usdc.clone(), price_data, empty_bebop_client());

        // swap 3 WETH -> USDC
        let amount_out_result = state
            .get_amount_out(BigUint::from_str("3_000000000000000000").unwrap(), &weth, &usdc)
            .unwrap();

        // 6000 from level 1 + 2900 from level 2 = 8900 USDC
        assert_eq!(amount_out_result.amount, BigUint::from_str("8900_000_000").unwrap());

        // swap 7000 USDC -> WETH
        let amount_out_result = state
            .get_amount_out(BigUint::from_str("7000_000_000").unwrap(), &usdc, &weth)
            .unwrap();

        // 1.5 from level 1 + 0.78333 from level 2 = 2.283333 WETH
        assert_eq!(amount_out_result.amount, BigUint::from_str("2_283333333333333248").unwrap());
    }
}
