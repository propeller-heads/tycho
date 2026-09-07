use std::{any::Any, borrow::Cow, collections::HashMap, sync::Arc};

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
        levels::Levels,
        sim::{self, SwapDirection},
    },
    rfq::protocols::bebop::{client::BebopClient, models::BebopBook},
};

/// Rough gas estimate for one Bebop settlement.
const BEBOP_SWAP_GAS: u64 = 70_000;

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct BebopState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug(skip)]
    pub(super) book: BebopBook,
    #[debug(skip)]
    pub(super) client: Arc<BebopClient>,
}

impl BebopState {
    /// The pair's base token (the token whose amounts the price levels are quoted in).
    pub fn base_token(&self) -> &Token {
        &self.base_token
    }

    /// The pair's quote token.
    pub fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    fn direction(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<SwapDirection, SimulationError> {
        SwapDirection::require(
            &self.base_token.address,
            &self.quote_token.address,
            token_in,
            token_out,
        )
    }

    /// The ladder a swap in `direction` consumes, in the units of the token sold: the bids as
    /// published for selling base, the asks re-expressed per quote unit for selling quote.
    fn ladder(&self, direction: SwapDirection) -> Cow<'_, Levels> {
        match direction {
            SwapDirection::BaseToQuote => Cow::Borrowed(&self.book.bids),
            SwapDirection::QuoteToBase => Cow::Owned(self.book.asks.invert()),
        }
    }

    fn decimals(&self, direction: SwapDirection) -> (u32, u32) {
        match direction {
            SwapDirection::BaseToQuote => (self.base_token.decimals, self.quote_token.decimals),
            SwapDirection::QuoteToBase => (self.quote_token.decimals, self.base_token.decimals),
        }
    }
}

#[typetag::serde]
impl ProtocolSim for BebopState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = self.direction(&base.address, &quote.address)?;
        // Since this method does not care about sell direction, we average the price of the best
        // bid and ask
        let best_bid = self
            .book
            .bids
            .first()
            .map(|level| level.price);
        let best_ask = self
            .book
            .asks
            .first()
            .map(|level| level.price);
        // If just one is available, only consider that one
        let average_price = match (best_bid, best_ask) {
            (Some(best_bid), Some(best_ask)) => (best_bid + best_ask) / 2.0,
            (Some(best_bid), None) => best_bid,
            (None, Some(best_ask)) => best_ask,
            (None, None) => {
                return Err(SimulationError::RecoverableError("No liquidity available".to_string()))
            }
        };
        match direction {
            SwapDirection::BaseToQuote => Ok(average_price),
            SwapDirection::QuoteToBase => Ok(1.0 / average_price),
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let direction = self.direction(&token_in.address, &token_out.address)?;
        let ladder = self.ladder(direction);
        if ladder.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }
        let amount_in = sim::to_human(&amount_in, token_in.decimals)?;
        let fill = ladder.fill(amount_in);
        // The state doesn't change after a swap.
        sim::fill_result(fill, amount_in, token_out.decimals, BEBOP_SWAP_GAS, self.clone_box())
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction = self.direction(&sell_token, &buy_token)?;
        let ladder = self.ladder(direction);
        // If there are no price levels, return 0 for both limits
        if ladder.is_empty() {
            return Ok((BigUint::from(0u64), BigUint::from(0u64)));
        }
        let (sell_decimals, buy_decimals) = self.decimals(direction);
        sim::limits(&ladder, sell_decimals, buy_decimals)
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
                self.book == other_state.book
        } else {
            false
        }
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
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
    use std::str::FromStr;

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::bebop::models::BebopPriceData;

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

    fn empty_client() -> Arc<BebopClient> {
        Arc::new(BebopClient::new(
            "https://api.bebop.xyz/pmm/ethereum/v3/quote".to_string(),
            "".to_string(),
            Duration::from_secs(30),
            None,
            None,
            None,
        ))
    }

    fn create_test_bebop_state() -> BebopState {
        let price_data = BebopPriceData {
            base: hex::decode("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(), // WBTC
            quote: hex::decode("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(), // USDC
            last_update_ts: 1703097600,
            bids: vec![65000.0f32, 1.5f32, 64950.0f32, 2.0f32, 64900.0f32, 0.5f32],
            asks: vec![65100.0f32, 1.0f32, 65150.0f32, 2.5f32, 65200.0f32, 1.5f32],
        };
        BebopState {
            base_token: wbtc(),
            quote_token: usdc(),
            book: BebopBook::try_from(price_data).unwrap(),
            client: empty_client(),
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
        state.book.asks = Levels::default();

        // Test WBTC/USDC with no asks - should use only best bid
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65000.0);
    }

    #[test]
    fn test_spot_price_empty_bids() {
        let mut state = create_test_bebop_state();
        state.book.bids = Levels::default();
        // Test WBTC/USDC with no bids - should use only best ask
        let price = state
            .spot_price(&wbtc(), &usdc())
            .unwrap();
        assert_eq!(price, 65100.0);
    }

    #[test]
    fn test_spot_price_no_liquidity() {
        let mut state = create_test_bebop_state();
        state.book.bids = Levels::default();
        state.book.asks = Levels::default();
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

        if let Err(SimulationError::InvalidInput(msg, None)) = result {
            assert!(msg.contains("Invalid token addresses"));
        } else {
            panic!("Expected InvalidInput with invalid token addresses message");
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
        let state = BebopState {
            base_token: weth.clone(),
            quote_token: usdc.clone(),
            book: BebopBook::try_from(price_data).unwrap(),
            client: empty_client(),
        };

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
