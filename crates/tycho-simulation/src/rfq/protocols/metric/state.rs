use std::{any::Any, collections::HashMap, fmt};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{FromPrimitive, ToPrimitive, Zero};
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
    models::{MetricBidAskResponse, MetricDepthBin, MetricMetadata},
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

    fn quote_with_depth(
        &self,
        direction: MetricDirection,
        amount_in_human: f64,
        token_out_decimals: u32,
        max_output: &BigUint,
    ) -> Result<Option<DepthQuote>, SimulationError> {
        let (start_price, bins) = match direction {
            MetricDirection::ZeroForOne => (self.bid_ask.bid_price()?, &self.bid_ask.depth.bids),
            MetricDirection::OneForZero => (self.bid_ask.ask_price()?, &self.bid_ask.depth.asks),
        };

        // Some pools still return an empty depth object. In that case the top-of-book quote is
        // the best signal we have, so keep the old flat-price path.
        let Some(depth_max_output) = depth_max_output(bins)? else {
            return Ok(None);
        };

        let effective_max_output = depth_max_output.min(max_output.clone());
        if effective_max_output.is_zero() {
            return Ok(Some(DepthQuote {
                amount_out_human: 0.0,
                max_output: effective_max_output,
                exhausted: amount_in_human > 0.0,
            }));
        }

        let max_output_human =
            raw_to_human(&effective_max_output, token_out_decimals, "depth max output")?;
        let depth_fill = depth_output_for_input(
            direction,
            bins,
            start_price,
            amount_in_human,
            token_out_decimals,
            max_output_human,
        )?;

        Ok(Some(DepthQuote {
            amount_out_human: depth_fill.output_human,
            max_output: effective_max_output,
            exhausted: depth_fill.exhausted,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
enum MetricDirection {
    ZeroForOne,
    OneForZero,
}

struct DepthQuote {
    amount_out_human: f64,
    max_output: BigUint,
    exhausted: bool,
}

struct DepthFill {
    output_human: f64,
    exhausted: bool,
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

        let (flat_amount_out_human, max_output) = match direction {
            MetricDirection::ZeroForOne => {
                let price = self.bid_ask.bid_price()?;
                (amount_in_human * price, self.bid_ask.total_token1_available()?)
            }
            MetricDirection::OneForZero => {
                let price = self.bid_ask.ask_price()?;
                (amount_in_human / price, self.bid_ask.total_token0_available()?)
            }
        };
        // Prefer size-aware depth when Metric exposes it, otherwise use best bid/ask with only
        // the aggregate inventory cap.
        let depth_quote =
            self.quote_with_depth(direction, amount_in_human, token_out.decimals, &max_output)?;
        let (amount_out_human, effective_max_output, exhausted) = match depth_quote {
            Some(quote) => (quote.amount_out_human, quote.max_output, quote.exhausted),
            None => (flat_amount_out_human, max_output.clone(), false),
        };

        let amount_out =
            BigUint::from_f64(amount_out_human * 10_f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUint".into())
                })?;
        let capped_amount = amount_out
            .clone()
            .min(effective_max_output.clone());
        let res = GetAmountOutResult {
            amount: capped_amount.clone(),
            gas: BigUint::from(170_000u64),
            new_state: self.clone_box(),
        };

        if exhausted || amount_out > effective_max_output {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Metric pool has not enough liquidity. Requested output {}, available {}",
                    amount_out, effective_max_output
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

fn depth_max_output(bins: &[MetricDepthBin]) -> Result<Option<BigUint>, SimulationError> {
    let mut max_output = None;
    for bin in bins {
        max_output = Some(bin.cumulative_volume()?);
    }
    Ok(max_output)
}

fn depth_output_for_input(
    direction: MetricDirection,
    bins: &[MetricDepthBin],
    start_price: f64,
    input_human: f64,
    output_decimals: u32,
    max_output_human: f64,
) -> Result<DepthFill, SimulationError> {
    if input_human == 0.0 || max_output_human == 0.0 {
        return Ok(DepthFill { output_human: 0.0, exhausted: input_human > 0.0 });
    }

    let mut current_price = start_price;
    let mut previous_volume = 0.0;
    let mut remaining_input = input_human;
    let mut output_human = 0.0;

    for bin in bins {
        let bin_price = bin.price()?;
        // Metric reports cumulative depth in output-token units for the side being walked.
        // Adjacent differences give the volume available in each linear price segment.
        let cumulative =
            raw_to_human(&bin.cumulative_volume()?, output_decimals, "depth cumulative volume")?;
        let volume_in_bin = cumulative - previous_volume;
        previous_volume = cumulative;

        if volume_in_bin <= 0.0 {
            current_price = bin_price;
            continue;
        }

        let output_capacity = max_output_human - output_human;
        if output_capacity <= 0.0 {
            break;
        }
        let fillable_volume = volume_in_bin.min(output_capacity);
        // If the aggregate liquidity cap cuts this bin short, the segment can end before bin_price.
        let segment_exit_price =
            current_price + (bin_price - current_price) * fillable_volume / volume_in_bin;
        // First price this whole segment. If the remaining input can pay that cost, the quote
        // consumes fillable_volume completely and then continues into the next depth bin.
        let full_segment_input = depth_segment_input_required(
            direction,
            fillable_volume,
            current_price,
            segment_exit_price,
        )?;

        if remaining_input >= full_segment_input {
            output_human += fillable_volume;
            remaining_input -= full_segment_input;
            current_price = segment_exit_price;
            continue;
        }

        // The remaining input is not enough for the whole segment, so the trade stops between
        // current_price and segment_exit_price. Invert this segment's price formula to compute
        // the partial output bought by remaining_input.
        output_human += depth_segment_output_for_input(
            direction,
            remaining_input,
            fillable_volume,
            current_price,
            segment_exit_price,
        )?;
        remaining_input = 0.0;
        break;
    }

    if remaining_input > 1e-12 && output_human < max_output_human - 1e-12 {
        return Err(SimulationError::RecoverableError(
            "Metric depth has not enough cumulative volume".into(),
        ));
    }

    Ok(DepthFill {
        output_human: output_human.min(max_output_human),
        exhausted: remaining_input > 1e-12,
    })
}

fn depth_segment_input_required(
    direction: MetricDirection,
    output_human: f64,
    entry_price: f64,
    exit_price: f64,
) -> Result<f64, SimulationError> {
    let average_price = depth_segment_average_price(entry_price, exit_price)?;
    match direction {
        MetricDirection::ZeroForOne => Ok(output_human / average_price),
        MetricDirection::OneForZero => Ok(output_human * average_price),
    }
}

fn depth_segment_output_for_input(
    direction: MetricDirection,
    input_human: f64,
    segment_output_human: f64,
    entry_price: f64,
    exit_price: f64,
) -> Result<f64, SimulationError> {
    if segment_output_human <= 0.0 || entry_price <= 0.0 {
        return Err(SimulationError::RecoverableError(
            "Metric depth has invalid price curve".into(),
        ));
    }

    // Variables for this segment:
    //   V  = segment_output_human, the max output available in this segment.
    //   p0 = entry_price, the price at x = 0.
    //   p1 = exit_price, the price at x = V.
    //   x  = output filled inside this segment, where 0 <= x <= V.
    //   I  = input_human, the input available for this partial segment.
    //
    // Metric linearly interpolates the price reached after filling x output:
    //   exit_price_at_x = p0 + (p1 - p0) * x / V
    //
    // Metric prices the partial fill by averaging the start price and that exit price:
    //   avg_price(x) = (p0 + exit_price_at_x) / 2
    //
    // Substitute exit_price_at_x:
    //   avg_price(x) = (p0 + p0 + (p1 - p0) * x / V) / 2
    //                = p0 + (p1 - p0) * x / (2 * V)
    //
    // Store the x coefficient as average_slope:
    //   average_slope = (p1 - p0) / (2 * V)
    //   avg_price(x) = p0 + average_slope * x
    let average_slope = (exit_price - entry_price) / (2.0 * segment_output_human);
    let output = match direction {
        MetricDirection::ZeroForOne => {
            // ZeroForOne sells base for quote, so price is quote/base and:
            //
            //   I = x / avg_price(x)
            //
            // Substitute avg_price(x):
            //   I = x / (p0 + average_slope * x)
            //
            // Solve for x:
            //   x = I * p0 / (1 - I * average_slope)
            let denominator = 1.0 - input_human * average_slope;
            if denominator <= 0.0 {
                return Err(SimulationError::RecoverableError(
                    "Metric depth has invalid price curve".into(),
                ));
            }
            input_human * entry_price / denominator
        }
        MetricDirection::OneForZero => {
            // OneForZero sells quote for base, so price is quote/base and:
            //
            //   I = x * avg_price(x)
            //
            // Substitute avg_price(x):
            //   I = x * (p0 + average_slope * x)
            //   I = p0 * x + average_slope * x^2
            //
            // Rearrange into the standard quadratic form a*x^2 + b*x + c = 0:
            //   average_slope * x^2 + p0 * x - I = 0
            //
            // Here:
            //   a = average_slope
            //   b = p0
            //   c = -I
            //
            // The quadratic formula uses sqrt(b^2 - 4*a*c):
            //   b^2 - 4*a*c = p0^2 + 4 * average_slope * I
            //
            // The valid solution is the root inside [0, segment_output_human].
            if average_slope.abs() < 1e-18 {
                input_human / entry_price
            } else {
                let discriminant =
                    entry_price.mul_add(entry_price, 4.0 * average_slope * input_human);
                if discriminant < 0.0 {
                    return Err(SimulationError::RecoverableError(
                        "Metric depth has invalid price curve".into(),
                    ));
                }
                let root_a = (-entry_price + discriminant.sqrt()) / (2.0 * average_slope);
                let root_b = (-entry_price - discriminant.sqrt()) / (2.0 * average_slope);
                [root_a, root_b]
                    .into_iter()
                    .find(|root| {
                        root.is_finite() && *root >= -1e-12 && *root <= segment_output_human + 1e-12
                    })
                    .ok_or_else(|| {
                        SimulationError::RecoverableError(
                            "Metric depth has invalid price curve".into(),
                        )
                    })?
            }
        }
    };

    Ok(output.clamp(0.0, segment_output_human))
}

fn depth_segment_average_price(entry_price: f64, exit_price: f64) -> Result<f64, SimulationError> {
    let average_price = (entry_price + exit_price) / 2.0;
    if average_price <= 0.0 {
        return Err(SimulationError::RecoverableError(
            "Metric depth has non-positive average price".into(),
        ));
    }
    Ok(average_price)
}

fn raw_to_human(amount: &BigUint, decimals: u32, field: &str) -> Result<f64, SimulationError> {
    let amount = amount.to_f64().ok_or_else(|| {
        SimulationError::RecoverableError(format!("Can't convert {field} to f64"))
    })?;
    Ok(amount / 10_f64.powi(decimals as i32))
}

#[async_trait]
impl IndicativelyPriced for MetricState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        let direction = self.direction(&params.token_in, &params.token_out)?;
        let (token_in, token_out) = match direction {
            MetricDirection::ZeroForOne => (&self.base_token, &self.quote_token),
            MetricDirection::OneForZero => (&self.quote_token, &self.base_token),
        };
        let amount_out = self
            .get_amount_out(params.amount_in.clone(), token_in, token_out)?
            .amount;

        // Metric's swap path is independent from the quote API. Execution uses this hook to fetch
        // signed oracle-update calldata that can be submitted before the pool swap.
        Ok(self
            .client
            .request_oracle_update_for_pool(&self.metadata, &params, amount_out)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::metric::{client::MetricClient, models::MetricDepth};

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

    fn state() -> MetricState {
        let weth = weth();
        let usdc = usdc();
        let metadata = MetricMetadata {
            pool_address: Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
            token0: weth.address.clone(),
            token1: usdc.address.clone(),
        };
        let bid_ask = MetricBidAskResponse {
            // 3000 * 2^64
            bid_adj: "55340232221128654848000".to_string(),
            // 3010 * 2^64
            ask_adj: "55524699661865750400000".to_string(),
            quote_available: true,
            total_token0_available: "10000000000000000000".to_string(),
            total_token1_available: "30000000000".to_string(),
            latest_block: 100,
            depth: MetricDepth::default(),
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

    #[test]
    fn test_get_amount_out_walks_bid_depth() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: "53495557813757699686400".to_string(),
            cumulative_volume: "3000000000".to_string(),
        }];

        let result = state
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000u128),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap();

        assert!(result.amount < BigUint::from(3_000_000_000u64));
        assert!(result.amount > BigUint::from(2_950_000_000u64));
    }

    #[test]
    fn test_get_amount_out_walks_ask_depth() {
        let mut state = state();
        state.bid_ask.depth.asks = vec![MetricDepthBin {
            bin_idx: 0,
            // 3100 * 2^64
            price: "57184906628499610009600".to_string(),
            cumulative_volume: "1000000000000000000".to_string(),
        }];

        let result = state
            .get_amount_out(BigUint::from(3_000_000_000u64), &state.quote_token, &state.base_token)
            .unwrap();

        assert!(result.amount < BigUint::from(1_000_000_000_000_000_000u128));
        assert!(result.amount > BigUint::from(980_000_000_000_000_000u128));
    }

    #[test]
    fn test_depth_output_for_input_partially_fills_bid_bin() {
        let bins = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: "53495557813757699686400".to_string(),
            cumulative_volume: "3000000000".to_string(),
        }];

        let fill =
            depth_output_for_input(MetricDirection::ZeroForOne, &bins, 3000.0, 1.0, 6, 3000.0)
                .unwrap();

        assert!((fill.output_human - 2950.8196721311474).abs() < 1e-9);
        assert!(!fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_partially_fills_ask_bin() {
        let bins = vec![MetricDepthBin {
            bin_idx: 0,
            // 3100 * 2^64
            price: "57184906628499610009600".to_string(),
            cumulative_volume: "1000000000000000000".to_string(),
        }];

        let fill =
            depth_output_for_input(MetricDirection::OneForZero, &bins, 3010.0, 3000.0, 18, 1.0)
                .unwrap();

        assert!((fill.output_human - 0.9822534928279816).abs() < 1e-12);
        assert!(!fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_exhausts_available_depth() {
        let bins = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: "53495557813757699686400".to_string(),
            cumulative_volume: "3000000000".to_string(),
        }];

        let fill =
            depth_output_for_input(MetricDirection::ZeroForOne, &bins, 3000.0, 2.0, 6, 3000.0)
                .unwrap();

        assert_eq!(fill.output_human, 3000.0);
        assert!(fill.exhausted);
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API"]
    async fn test_live_metric_api_state_get_amount_out_and_oracle_update() {
        let weth = weth();
        let usdc = usdc();
        let base_url = std::env::var("METRIC_API_URL")
            .unwrap_or_else(|_| "http://54.199.103.16:8080".to_string())
            .trim_end_matches('/')
            .to_string();
        let client = MetricClient::new(
            Chain::Ethereum,
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
            .get(format!("{base_url}/ethereum/metadata"))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let mut selected = None;
        for pool in metadata
            .into_iter()
            .filter(|pool| pool.token0 == weth.address && pool.token1 == usdc.address)
        {
            let bid_ask: MetricBidAskResponse = http_client
                .get(format!("{base_url}/ethereum/{}/bid_ask", pool.pool_address))
                .header("accept", "application/json")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let has_enough_quote_liquidity = bid_ask
                .total_token1_available()
                .map(|available| available > BigUint::from(10u8))
                .unwrap_or(false);
            if bid_ask.quote_available && has_enough_quote_liquidity {
                selected = Some((pool, bid_ask));
                break;
            }
        }

        let Some((metadata, bid_ask)) = selected else {
            eprintln!("Metric live API returned no liquid Ethereum WETH/USDC pool; skipping");
            return;
        };

        let state = MetricState::new(weth, usdc, metadata, bid_ask, client);
        assert!(state.bid_ask.quote_available);

        let amount_in = BigUint::from(1_000_000_000u64);
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
        assert_eq!(
            &signed_quote.quote_attributes["oracle_update_0_calldata"][..4],
            &[0x78, 0xce, 0x3a, 0xe1]
        );
    }
}
