use std::{any::Any, collections::HashMap, fmt};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive};
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

use crate::rfq::protocols::metric::{
    client::MetricClient,
    models::{MetricBidAskResponse, MetricMetadata},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct MetricState {
    pub base_token: Token,
    pub quote_token: Token,
    pub metadata: MetricMetadata,
    pub bid_ask: MetricBidAskResponse,
    pub client: MetricClient,
}

impl fmt::Debug for MetricState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .field("pool", &self.metadata.pool_address)
            .field("latest_block", &self.bid_ask.latest_block)
            .finish_non_exhaustive()
    }
}

impl MetricState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        metadata: MetricMetadata,
        bid_ask: MetricBidAskResponse,
        client: MetricClient,
    ) -> Self {
        Self { base_token, quote_token, metadata, bid_ask, client }
    }

    fn direction(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<MetricDirection, SimulationError> {
        if token_in == &self.base_token.address && token_out == &self.quote_token.address {
            Ok(MetricDirection::ZeroForOne)
        } else if token_in == &self.quote_token.address && token_out == &self.base_token.address {
            Ok(MetricDirection::OneForZero)
        } else {
            Err(SimulationError::InvalidInput(
                format!(
                    "Invalid token addresses. Got in={token_in}, out={token_out}, expected {} / {}",
                    self.base_token.address, self.quote_token.address
                ),
                None,
            ))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MetricDirection {
    ZeroForOne,
    OneForZero,
}

#[typetag::serde]
impl ProtocolSim for MetricState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let bid = self.bid_ask.bid_price()?;
        let ask = self.bid_ask.ask_price()?;
        let mid = (bid + ask) / 2.0;
        if base.address == self.base_token.address && quote.address == self.quote_token.address {
            Ok(mid)
        } else if base.address == self.quote_token.address &&
            quote.address == self.base_token.address
        {
            Ok(1.0 / mid)
        } else {
            Err(SimulationError::InvalidInput(
                format!(
                    "Invalid token addresses. Got base={}, quote={}, expected {} / {}",
                    base.address, quote.address, self.base_token.address, self.quote_token.address
                ),
                None,
            ))
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if !self.bid_ask.quote_available {
            return Err(SimulationError::RecoverableError(format!(
                "Metric quote unavailable for pool {} at block {}",
                self.metadata.pool_address, self.bid_ask.latest_block
            )));
        }

        let direction = self.direction(&token_in.address, &token_out.address)?;
        let amount_in_human = amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10_f64.powi(token_in.decimals as i32);

        // Stream prices are only indicative; execution asks Metric for a fresh quote later.
        let (amount_out_human, max_output) = match direction {
            MetricDirection::ZeroForOne => {
                let price = self.bid_ask.bid_price()?;
                (amount_in_human * price, self.bid_ask.total_token1_available()?)
            }
            MetricDirection::OneForZero => {
                let price = self.bid_ask.ask_price()?;
                (amount_in_human / price, self.bid_ask.total_token0_available()?)
            }
        };

        let amount_out =
            BigUint::from_f64(amount_out_human * 10_f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUint".into())
                })?;
        let capped_amount = amount_out
            .clone()
            .min(max_output.clone());
        let res = GetAmountOutResult {
            amount: capped_amount.clone(),
            gas: BigUint::from(170_000u64),
            new_state: self.clone_box(),
        };

        if amount_out > max_output {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Metric pool has not enough liquidity. Requested output {}, available {}",
                    amount_out, max_output
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
        let direction = self.direction(&sell_token, &buy_token)?;
        match direction {
            MetricDirection::ZeroForOne => {
                let price = self.bid_ask.bid_price()?;
                let buy_limit = self.bid_ask.total_token1_available()?;
                let buy_limit_human = buy_limit.to_f64().ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert buy limit to f64".into())
                })? / 10_f64.powi(self.quote_token.decimals as i32);
                let sell_limit = buy_limit_human / price;
                let sell_limit =
                    BigUint::from_f64(sell_limit * 10_f64.powi(self.base_token.decimals as i32))
                        .ok_or_else(|| {
                            SimulationError::RecoverableError(
                                "Can't convert sell limit to BigUint".into(),
                            )
                        })?;
                Ok((sell_limit, buy_limit))
            }
            MetricDirection::OneForZero => {
                let price = self.bid_ask.ask_price()?;
                let buy_limit = self.bid_ask.total_token0_available()?;
                let buy_limit_human = buy_limit.to_f64().ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert buy limit to f64".into())
                })? / 10_f64.powi(self.base_token.decimals as i32);
                let sell_limit = buy_limit_human * price;
                let sell_limit =
                    BigUint::from_f64(sell_limit * 10_f64.powi(self.quote_token.decimals as i32))
                        .ok_or_else(|| {
                        SimulationError::RecoverableError(
                            "Can't convert sell limit to BigUint".into(),
                        )
                    })?;
                Ok((sell_limit, buy_limit))
            }
        }
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
        // RFQ updates arrive as full API snapshots, not block deltas.
        Err(TransitionError::DecodeError(
            "Metric RFQ state is snapshot-based and does not support deltas".into(),
        ))
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
            .downcast_ref::<MetricState>()
        {
            self.base_token == other_state.base_token &&
                self.quote_token == other_state.quote_token &&
                self.metadata == other_state.metadata &&
                self.bid_ask == other_state.bid_ask
        } else {
            false
        }
    }
}

#[async_trait]
impl IndicativelyPriced for MetricState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        Ok(self
            .client
            .request_binding_quote_for_pool(&self.metadata, &params)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::metric::client::MetricClient;

    fn weth() -> Token {
        Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(2300)],
            Chain::Ethereum,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(1)],
            Chain::Ethereum,
            100,
        )
    }

    fn base_weth() -> Token {
        Token::new(
            &Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap(),
            "WETH",
            18,
            0,
            &[Some(2300)],
            Chain::Base,
            100,
        )
    }

    fn base_usdc() -> Token {
        Token::new(
            &Bytes::from_str("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap(),
            "USDC",
            6,
            0,
            &[Some(1)],
            Chain::Base,
            100,
        )
    }

    fn state() -> MetricState {
        let weth = weth();
        let usdc = usdc();
        let metadata = MetricMetadata {
            pair: "ethusdc".to_string(),
            pool_address: Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
            price_provider_address: Bytes::from_str("0xbD321D18a7ce5fb91F8b16e026e3258f7b310598")
                .unwrap(),
            quoter_address: Bytes::from_str("0x58F9d1865d4Aeb59a9a7Dc68A3b4e0B42D9Ef5eD").unwrap(),
            token0: weth.address.clone(),
            token1: usdc.address.clone(),
            cex_step: Some(0.0002),
            dex_step: Some(0.0),
        };
        let bid_ask = MetricBidAskResponse {
            pair: "ethusdc".to_string(),
            // 3000 * 2^64
            bid_adj: "55340232221128654848000".to_string(),
            // 3010 * 2^64
            ask_adj: "55524699661865750400000".to_string(),
            quote_available: true,
            total_token0_available: "10000000000000000000".to_string(),
            total_token1_available: "30000000000".to_string(),
            latest_block: 100,
            block_ts: 1_700_000_000,
            server_ts: 1_700_000_001,
            quote_expiration: 1_700_000_005,
            depth: serde_json::json!({}),
        };
        let client = MetricClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
            HashSet::new(),
            "http://localhost:8080".to_string(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        MetricState::new(weth, usdc, metadata, bid_ask, client)
    }

    #[test]
    fn test_get_amount_out_zero_for_one() {
        let state = state();
        let result = state
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000u128),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap();

        assert_eq!(result.amount, BigUint::from(3_000_000_000u64));
    }

    #[test]
    fn test_get_amount_out_one_for_zero() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(3_010_000_000u64), &state.quote_token, &state.base_token)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(1_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_get_amount_out_caps_to_available_liquidity() {
        let mut state = state();
        state.bid_ask.total_token1_available = "1500000000".to_string();
        let err = state
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000u128),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap_err();

        assert!(matches!(err, SimulationError::InvalidInput(_, Some(_))));
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API"]
    async fn test_live_metric_api_state_get_amount_out_and_quote() {
        let weth = base_weth();
        let usdc = base_usdc();
        let base_url = std::env::var("METRIC_API_URL")
            .unwrap_or_else(|_| "http://54.199.103.16:8080".to_string())
            .trim_end_matches('/')
            .to_string();
        let client = MetricClient::new(
            Chain::Base,
            HashSet::from([weth.address.clone(), usdc.address.clone()]),
            0.0,
            HashSet::new(),
            base_url.clone(),
            None,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let http_client = reqwest::Client::new();
        let metadata: Vec<MetricMetadata> = http_client
            .get(format!("{base_url}/base/metadata"))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let metadata = metadata
            .into_iter()
            .find(|pool| pool.token0 == weth.address && pool.token1 == usdc.address)
            .expect("Metric live API returned no Base WETH/USDC pool");
        let bid_ask: MetricBidAskResponse = http_client
            .get(format!("{base_url}/base/{}/bid_ask", metadata.pool_address))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let state = MetricState::new(weth, usdc, metadata, bid_ask, client);
        assert!(state.bid_ask.quote_available);

        let amount_in = BigUint::from(1_000_000_000_000u64);
        let indicative_quote = state
            .get_amount_out(amount_in.clone(), &state.base_token, &state.quote_token)
            .unwrap();
        let trader = Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap();
        let signed_quote = state
            .request_signed_quote(GetAmountOutParams {
                amount_in,
                token_in: state.base_token.address.clone(),
                token_out: state.quote_token.address.clone(),
                sender: trader.clone(),
                receiver: trader,
            })
            .await
            .unwrap();

        assert!(indicative_quote.amount > BigUint::from(0u8));
        assert!(signed_quote.amount_out > BigUint::from(0u8));
        assert_eq!(signed_quote.base_token, state.base_token.address);
        assert_eq!(signed_quote.quote_token, state.quote_token.address);
    }
}
