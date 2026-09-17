use std::{any::Any, collections::HashMap, fmt};

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
    client::RFQClient,
    protocols::euclid::{client::EuclidClient, models::EuclidPriceData},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct EuclidState {
    pub base_token: Token,
    pub quote_token: Token,
    pub price_data: EuclidPriceData,
    pub client: EuclidClient,
}

impl fmt::Debug for EuclidState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EuclidState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .finish_non_exhaustive()
    }
}

impl EuclidState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        price_data: EuclidPriceData,
        client: EuclidClient,
    ) -> Self {
        EuclidState { base_token, quote_token, price_data, client }
    }
}

#[typetag::serde]
impl ProtocolSim for EuclidState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // Direction-agnostic: average the best bid and ask; fall back to
        // whichever side exists.
        let best_bid = self
            .price_data
            .bids
            .first()
            .map(|(price, _)| *price);
        let best_ask = self
            .price_data
            .asks
            .first()
            .map(|(price, _)| *price);

        let average_price = match (best_bid, best_ask) {
            (Some(best_bid), Some(best_ask)) => (best_bid + best_ask) / 2.0,
            (Some(best_bid), None) => best_bid,
            (None, Some(best_ask)) => best_ask,
            (None, None) => {
                return Err(SimulationError::RecoverableError("No liquidity available".to_string()))
            }
        };

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
        let sell_base = if token_in == &self.base_token && token_out == &self.quote_token {
            true
        } else if token_in == &self.quote_token && token_out == &self.base_token {
            false
        } else {
            return Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {}, {}",
                token_in.address, token_out.address
            )));
        };

        // Selling base walks the bids; selling quote walks inverted asks so
        // sizes are denominated in the input token.
        let price_levels = if sell_base {
            self.price_data.bids.clone()
        } else {
            EuclidPriceData::invert_price_levels(&self.price_data.asks)
        };

        if price_levels.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let amount_in_f = amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10f64.powi(token_in.decimals as i32);
        let (amount_out, remaining_amount_in) =
            EuclidPriceData::get_amount_out_from_levels(amount_in_f, &price_levels);
        let res = GetAmountOutResult {
            amount: BigUint::from_f64(amount_out * 10f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUInt".into())
                })?,
            // fillOrderRFQTo: order validation + two ERC20 transfers.
            gas: BigUint::from(120_000u64),
            new_state: self.clone_box(), // The state doesn't change after a swap
        };

        if remaining_amount_in > 0.0 {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Pool has not enough liquidity to support complete swap. input amount: {amount_in_f}, consumed amount: {}",
                    amount_in_f - remaining_amount_in
                ),
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
        // Selling BASE for QUOTE walks the bids; buying BASE with QUOTE walks the asks.
        let (sell_decimals, buy_decimals, price_levels) = if sell_token == self.base_token.address &&
            buy_token == self.quote_token.address
        {
            (self.base_token.decimals, self.quote_token.decimals, &self.price_data.bids)
        } else if buy_token == self.base_token.address && sell_token == self.quote_token.address {
            (self.quote_token.decimals, self.base_token.decimals, &self.price_data.asks)
        } else {
            return Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {sell_token}, {buy_token}"
            )));
        };

        if price_levels.is_empty() {
            return Ok((BigUint::from(0u64), BigUint::from(0u64)));
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
            if sell_token == self.base_token.address && buy_token == self.quote_token.address {
                (total_base_amount, total_quote_amount)
            } else {
                (total_quote_amount, total_base_amount)
            };

        let sell_limit =
            BigUint::from((total_sell_amount * 10_f64.pow(sell_decimals as f64)) as u128);
        let buy_limit = BigUint::from((total_buy_amount * 10_f64.pow(buy_decimals as f64)) as u128);

        Ok((sell_limit, buy_limit))
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
            .downcast_ref::<EuclidState>()
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

#[async_trait]
impl IndicativelyPriced for EuclidState {
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

    fn usdc() -> Token {
        Token::new(
            &hex::decode("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
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

    fn empty_client() -> EuclidClient {
        EuclidClient::new(
            Chain::Ethereum,
            "https://rfq.example.com/connectors/euclid/amm/tycho".to_string(),
            HashSet::new(),
            0.0,
            None,
            Duration::from_secs(3),
        )
        .unwrap()
    }

    fn test_state() -> EuclidState {
        let weth_token = weth();
        let usdc_token = usdc();
        EuclidState::new(
            weth_token.clone(),
            usdc_token.clone(),
            EuclidPriceData {
                base: weth_token.address.to_vec(),
                quote: usdc_token.address.to_vec(),
                last_update_ts: 1757500000,
                bids: vec![(3000.0, 2.0), (2900.0, 2.5)],
                asks: vec![(3100.0, 1.5), (3000.0, 3.0)],
            },
            empty_client(),
        )
    }

    #[test]
    fn test_spot_price() {
        let state = test_state();
        let price = state
            .spot_price(&weth(), &usdc())
            .unwrap();
        assert_eq!(price, 3050.0);
        let inverted = state
            .spot_price(&usdc(), &weth())
            .unwrap();
        assert!((inverted - 1.0 / 3050.0).abs() < 1e-12);
    }

    #[test]
    fn test_get_amount_out_sell_base() {
        let state = test_state();
        // 3 WETH -> 6000 (level 1) + 2900 (level 2) = 8900 USDC
        let result = state
            .get_amount_out(BigUint::from_str("3000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from_str("8900000000").unwrap());
    }

    #[test]
    fn test_get_amount_out_sell_quote() {
        let state = test_state();
        // 7000 USDC through inverted asks: 4650 fills level 1 (1.5 WETH),
        // remaining 2350 at 3000 -> 0.78333 WETH
        let result = state
            .get_amount_out(BigUint::from_str("7000000000").unwrap(), &usdc(), &weth())
            .unwrap();
        let expected = BigUint::from_str("2283333333333333248").unwrap();
        assert_eq!(result.amount, expected);
    }

    #[test]
    fn test_get_amount_out_insufficient_liquidity() {
        let state = test_state();
        let result = state.get_amount_out(
            BigUint::from_str("10000000000000000000").unwrap(),
            &weth(),
            &usdc(),
        );
        assert!(matches!(result, Err(SimulationError::InvalidInput(_, Some(_)))));
    }

    #[test]
    fn test_get_limits() {
        let state = test_state();
        let (sell_limit, buy_limit) = state
            .get_limits(weth().address.clone(), usdc().address.clone())
            .unwrap();
        // bids: 2.0 + 2.5 = 4.5 WETH; 3000*2 + 2900*2.5 = 13250 USDC
        assert_eq!(sell_limit, BigUint::from_str("4500000000000000000").unwrap());
        assert_eq!(buy_limit, BigUint::from_str("13250000000").unwrap());
    }

    #[test]
    fn test_get_amount_out_exact_boundary_consumes_all_liquidity() {
        let state = test_state();
        // Exactly 4.5 WETH = full bid depth → no error, full output.
        let result = state
            .get_amount_out(BigUint::from_str("4500000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        // 3000*2 + 2900*2.5 = 13250 USDC
        assert_eq!(result.amount, BigUint::from_str("13250000000").unwrap());
        assert_eq!(result.gas, BigUint::from(120_000u64));
    }

    #[test]
    fn test_get_amount_out_invalid_pair_rejected() {
        let state = test_state();
        let dai = Token::new(
            &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            "DAI",
            18,
            0,
            &[],
            Default::default(),
            100,
        );
        assert!(state
            .get_amount_out(BigUint::from(1u64), &dai, &usdc())
            .is_err());
    }

    #[test]
    fn test_eq_compares_tokens_and_levels() {
        let a = test_state();
        let b = test_state();
        assert!(ProtocolSim::eq(&a, &b));
        let mut c = test_state();
        c.price_data.bids[0].0 = 1.0;
        assert!(!ProtocolSim::eq(&a, &c));
    }

    #[test]
    fn test_spot_price_one_sided_book() {
        let mut state = test_state();
        state.price_data.asks = vec![];
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            3000.0
        );
        state.price_data.bids = vec![];
        assert!(state
            .spot_price(&weth(), &usdc())
            .is_err());
    }

    #[test]
    fn test_get_limits_empty_side() {
        let mut state = test_state();
        state.price_data.bids = vec![];
        let (sell_limit, buy_limit) = state
            .get_limits(weth().address.clone(), usdc().address.clone())
            .unwrap();
        assert_eq!(sell_limit, BigUint::from(0u64));
        assert_eq!(buy_limit, BigUint::from(0u64));
    }
}
