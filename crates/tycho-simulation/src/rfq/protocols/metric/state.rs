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

/// Gas estimate for one MetricExecutor swap (pool swap + callback settlement).
const METRIC_SWAP_GAS: u64 = 170_000;

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
            .field("server_ts", &self.bid_ask.server_ts)
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
        amount_in: &BigUint,
        max_output: &BigUint,
    ) -> Result<Option<DepthQuote>, SimulationError> {
        let bins = match direction {
            MetricDirection::ZeroForOne => &self.bid_ask.depth.bids,
            MetricDirection::OneForZero => &self.bid_ask.depth.asks,
        };

        // Some pools still return an empty depth object. In that case the top-of-book quote is
        // the best signal we have, so keep the flat-price path.
        let Some(depth_max_output) = depth_max_output(bins) else {
            return Ok(None);
        };

        let effective_max_output = depth_max_output.min(max_output.clone());
        if effective_max_output.is_zero() {
            return Ok(Some(DepthQuote {
                amount_out: BigUint::ZERO,
                max_output: effective_max_output,
                exhausted: !amount_in.is_zero(),
            }));
        }

        let depth_fill = depth_output_for_input(bins, amount_in, &effective_max_output)?;

        Ok(Some(DepthQuote {
            amount_out: depth_fill.output,
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
    amount_out: BigUint,
    max_output: BigUint,
    exhausted: bool,
}

#[derive(Debug)]
struct DepthFill {
    output: BigUint,
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
        let direction = self.direction(&token_in.address, &token_out.address)?;
        let max_output = match direction {
            MetricDirection::ZeroForOne => self.bid_ask.total_token1_available()?,
            MetricDirection::OneForZero => self.bid_ask.total_token0_available()?,
        };

        // Prefer size-aware depth when Metric exposes it. The depth walk runs entirely in raw
        // integer units on Metric's own per-bin accounting, so its result is exact — including
        // the cap returned when the depth is exhausted.
        if let Some(quote) = self.quote_with_depth(direction, &amount_in, &max_output)? {
            let res = GetAmountOutResult {
                amount: quote.amount_out,
                gas: BigUint::from(METRIC_SWAP_GAS),
                new_state: self.clone_box(),
            };
            if quote.exhausted {
                return Err(SimulationError::InvalidInput(
                    format!(
                        "Metric pool depth exhausted. Input {amount_in} cannot be fully filled; \
                         tradable depth caps output at {}",
                        quote.max_output
                    ),
                    Some(res),
                ));
            }
            return Ok(res);
        }

        // No depth bins: flat top-of-book quote, capped only by the aggregate inventory.
        let amount_in_human = amount_in.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert amount in to f64".into())
        })? / 10_f64.powi(token_in.decimals as i32);
        let flat_amount_out_human = match direction {
            MetricDirection::ZeroForOne => amount_in_human * self.bid_ask.bid_price()?,
            MetricDirection::OneForZero => amount_in_human / self.bid_ask.ask_price()?,
        };
        let amount_out =
            BigUint::from_f64(flat_amount_out_human * 10_f64.powi(token_out.decimals as i32))
                .ok_or_else(|| {
                    SimulationError::RecoverableError("Can't convert amount out to BigUint".into())
                })?;
        let res = GetAmountOutResult {
            amount: amount_out
                .clone()
                .min(max_output.clone()),
            gas: BigUint::from(METRIC_SWAP_GAS),
            new_state: self.clone_box(),
        };
        if amount_out > max_output {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Metric pool has not enough liquidity. Requested output {amount_out} exceeds \
                     available {max_output}"
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
        // Price of one buy-token unit in sell tokens, plus the per-direction inventory cap, depth
        // side, and token decimals.
        let (sell_per_buy, aggregate, bins, sell_decimals, buy_decimals) = match direction {
            MetricDirection::ZeroForOne => (
                1.0 / self.bid_ask.bid_price()?,
                self.bid_ask.total_token1_available()?,
                &self.bid_ask.depth.bids,
                self.base_token.decimals,
                self.quote_token.decimals,
            ),
            MetricDirection::OneForZero => (
                self.bid_ask.ask_price()?,
                self.bid_ask.total_token0_available()?,
                &self.bid_ask.depth.asks,
                self.quote_token.decimals,
                self.base_token.decimals,
            ),
        };

        // Metric's own accounting gives the exact input required to consume the whole book: the
        // last bin's cumulativeInputVolume. Prefer it over reconstructing the input from the
        // top-of-book price, which understates the limit by the cumulative price impact.
        if let Some(last_bin) = bins.last() {
            if last_bin.cumulative_volume <= aggregate {
                return Ok((
                    last_bin.cumulative_input_volume.clone(),
                    last_bin.cumulative_volume.clone(),
                ));
            }
        }

        // No depth bins, or the aggregate inventory truncates the walkable depth: cap the output
        // at the aggregate and estimate the matching input from the top-of-book price. Mirrors
        // get_amount_out, which rejects anything beyond this cap as depth-exhausted.
        let buy_limit = cap_to_depth(aggregate, bins);
        let buy_limit_human = buy_limit.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert buy limit to f64".into())
        })? / 10_f64.powi(buy_decimals as i32);
        let sell_limit =
            BigUint::from_f64(buy_limit_human * sell_per_buy * 10_f64.powi(sell_decimals as i32))
                .ok_or_else(|| {
                SimulationError::RecoverableError("Can't convert sell limit to BigUint".into())
            })?;
        Ok((sell_limit, buy_limit))
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

fn depth_max_output(bins: &[MetricDepthBin]) -> Option<BigUint> {
    bins.last()
        .map(|bin| bin.cumulative_volume.clone())
}

/// Caps an aggregate-inventory output limit by the published depth, when present.
///
/// Returns the smaller of `aggregate` and the cumulative depth volume so the reported limit
/// never exceeds what a depth walk can actually fill. Pools that expose no depth bins fall back
/// to `aggregate`.
fn cap_to_depth(aggregate: BigUint, bins: &[MetricDepthBin]) -> BigUint {
    match depth_max_output(bins) {
        Some(depth) => depth.min(aggregate),
        None => aggregate,
    }
}

/// Walks the depth bins and computes the output bought by `amount_in`, entirely in raw integer
/// units using Metric's own per-bin accounting.
///
/// Full bins cost exactly their `cumulativeInputVolume` difference. Partial fills are priced
/// pro-rata within the bin — live data shows a bin's full input over its full volume equals the
/// bin's boundary price (one price per bin), so pro-rata is exact at bin boundaries — and the
/// division rounds down so the quote never overstates the output.
fn depth_output_for_input(
    bins: &[MetricDepthBin],
    amount_in: &BigUint,
    max_output: &BigUint,
) -> Result<DepthFill, SimulationError> {
    if amount_in.is_zero() || max_output.is_zero() {
        return Ok(DepthFill { output: BigUint::ZERO, exhausted: !amount_in.is_zero() });
    }

    let mut previous_output = BigUint::ZERO;
    let mut previous_input = BigUint::ZERO;
    let mut remaining_input = amount_in.clone();
    let mut output = BigUint::ZERO;

    for bin in bins {
        // Metric reports both sides cumulatively to each boundary: output volume in output-token
        // raw units and the input required to reach it in input-token raw units. Adjacent
        // differences give the per-bin amounts.
        let cumulative_output = &bin.cumulative_volume;
        let cumulative_input = &bin.cumulative_input_volume;
        if cumulative_output < &previous_output || cumulative_input < &previous_input {
            return Err(SimulationError::RecoverableError(
                "Metric depth cumulative volumes are not monotonic".into(),
            ));
        }
        let volume_in_bin = cumulative_output - &previous_output;
        let input_in_bin = cumulative_input - &previous_input;
        previous_output = cumulative_output.clone();
        previous_input = cumulative_input.clone();

        // Price-grid bins without liquidity carry neither volume nor input.
        if volume_in_bin.is_zero() && input_in_bin.is_zero() {
            continue;
        }
        // A bin with volume but no input (or vice versa) would hand out output for free or charge
        // input for nothing; refuse to price against corrupt data.
        if volume_in_bin.is_zero() || input_in_bin.is_zero() {
            return Err(SimulationError::RecoverableError(
                "Metric depth bin has inconsistent volume and input".into(),
            ));
        }

        let output_capacity = max_output - &output;
        if output_capacity.is_zero() {
            break;
        }

        // The aggregate inventory cap can cut the bin short; charge the fillable slice pro-rata,
        // rounding the input up so the quote never undercharges.
        let (fillable_volume, fillable_input) = if volume_in_bin <= output_capacity {
            (volume_in_bin, input_in_bin)
        } else {
            let fillable_input = (&input_in_bin * &output_capacity + &volume_in_bin -
                BigUint::from(1u8)) /
                &volume_in_bin;
            (output_capacity, fillable_input)
        };

        if remaining_input >= fillable_input {
            output += &fillable_volume;
            remaining_input -= &fillable_input;
            continue;
        }

        // The input runs out inside this bin: pro-rata output, rounding down.
        output += &fillable_volume * &remaining_input / &fillable_input;
        remaining_input = BigUint::ZERO;
        break;
    }

    Ok(DepthFill { output, exhausted: !remaining_input.is_zero() })
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

        // The v1 heartbeat updates the oracle on-chain every block, so execution relays no signed
        // oracle-update args with the swap. The quote therefore carries no quote attributes.
        Ok(SignedQuote {
            base_token: params.token_in.clone(),
            quote_token: params.token_out.clone(),
            amount_in: params.amount_in.clone(),
            amount_out,
            quote_attributes: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use tokio::time::Duration;
    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::metric::{client::MetricClient, models::MetricDepth};

    fn big(value: &str) -> BigUint {
        value.parse().unwrap()
    }

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
            tvl_fiat: Some(3000.0),
        };
        let bid_ask = MetricBidAskResponse {
            // 3000 * 2^64
            bid_adj: big("55340232221128654848000"),
            // 3010 * 2^64
            ask_adj: big("55524699661865750400000"),
            total_token0_available: Some(big("10000000000000000000")),
            total_token1_available: Some(big("30000000000")),
            server_ts: 100,
            depth: MetricDepth::default(),
        };
        let client = MetricClient::new(
            Chain::Ethereum,
            HashSet::new(),
            0.0,
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
        state.bid_ask.total_token1_available = Some(big("1500000000"));
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
    fn test_get_amount_out_depth_exhausted_reports_depth_message() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: big("53495557813757699686400"),
            // Only 3000 USDC of depth, far less than the 30000 USDC aggregate inventory.
            cumulative_volume: big("3000000000"),
            // Full-bin input at avg price 2950 (3000 USDC / 2950) ≈ 1.0169 WETH.
            cumulative_input_volume: big("1016949152542372881"),
        }];

        let err = state
            .get_amount_out(
                // 2 WETH buys more output than the depth can fill.
                BigUint::from(2_000_000_000_000_000_000u128),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap_err();

        match err {
            SimulationError::InvalidInput(msg, Some(_)) => {
                assert!(msg.contains("depth exhausted"), "unexpected message: {msg}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn test_get_amount_out_exhausted_returns_exact_cap() {
        let mut state = state();
        // An 18-decimal cap above 2^53 that is NOT representable exactly as f64. This is the
        // value from the production log; the f64 round-trip would drift it to ...662912.
        let cap = "6575581573690662958";
        state.bid_ask.depth.asks = vec![MetricDepthBin {
            bin_idx: 0,
            // 3100 * 2^64
            price: big("57184906628499610009600"),
            cumulative_volume: big(cap),
            // Full-bin input at avg price 3055 (~6.5756 WETH * 3055) ≈ 20088 USDC, below the
            // 30000 USDC input so the whole bin is consumed and the trade is depth-exhausted.
            cumulative_input_volume: big("20088000000"),
        }];

        let err = state
            .get_amount_out(
                // 30000 USDC buys more WETH than the depth can fill.
                BigUint::from(30_000_000_000u64),
                &state.quote_token,
                &state.base_token,
            )
            .unwrap_err();

        match err {
            SimulationError::InvalidInput(msg, Some(res)) => {
                assert!(msg.contains("depth exhausted"), "unexpected message: {msg}");
                // Exact cap, not the f64-reconstructed 6575581573690662912.
                assert_eq!(res.amount, BigUint::from_str(cap).unwrap());
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn test_get_limits_caps_to_depth() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: big("53495557813757699686400"),
            // 1500 USDC of depth, below the 30000 USDC aggregate inventory.
            cumulative_volume: big("1500000000"),
            // 1500 USDC / avg price 2950 ≈ 0.508 WETH.
            cumulative_input_volume: big("508474576271186440"),
        }];

        let (sell_limit, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        // Output limit follows the depth, not the aggregate inventory.
        assert_eq!(buy_limit, BigUint::from(1_500_000_000u64));
        // Input limit is Metric's own accounting: the last bin's cumulativeInputVolume, not a
        // top-of-book reconstruction (which would understate it by the price impact).
        assert_eq!(sell_limit, BigUint::from(508_474_576_271_186_440u128));
    }

    #[test]
    fn test_get_limits_aggregate_truncates_depth() {
        let mut state = state();
        // Aggregate inventory (1000 USDC) is below the 3000 USDC depth total, so the exact
        // last-bin input no longer applies and the limit falls back to the top-of-book estimate.
        state.bid_ask.total_token1_available = Some(big("1000000000"));
        state.bid_ask.depth.bids = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: big("53495557813757699686400"),
            cumulative_volume: big("3000000000"),
            cumulative_input_volume: big("1016949152542372881"),
        }];

        let (sell_limit, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        // Output limit follows the aggregate inventory, not the deeper book.
        assert_eq!(buy_limit, BigUint::from(1_000_000_000u64));
        // Input limit estimated at the top-of-book bid (3000): 1000 USDC / 3000 = 1/3 WETH,
        // rounded through the f64 estimate path (this fallback is an estimate by design).
        assert_eq!(sell_limit, BigUint::from(333_333_333_333_333_312u128));
    }

    #[test]
    fn test_get_limits_uses_aggregate_without_depth() {
        let state = state();

        let (_, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        // No depth bins: fall back to aggregate inventory (30000 USDC).
        assert_eq!(buy_limit, BigUint::from(30_000_000_000u64));
    }

    #[test]
    fn test_get_amount_out_walks_bid_depth() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64
            price: big("53495557813757699686400"),
            cumulative_volume: big("3000000000"),
            // Full-bin input at avg price 2950 (3000 USDC / 2950) ≈ 1.0169 WETH.
            cumulative_input_volume: big("1016949152542372881"),
        }];

        let result = state
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000u128),
                &state.base_token,
                &state.quote_token,
            )
            .unwrap();

        // Pro-rata at the bin's own average price (3000 USDC over 1.016949... WETH = 2950):
        // 1 WETH buys exactly 2950 USDC.
        assert_eq!(result.amount, BigUint::from(2_950_000_000u64));
    }

    #[test]
    fn test_get_amount_out_walks_ask_depth() {
        let mut state = state();
        state.bid_ask.depth.asks = vec![MetricDepthBin {
            bin_idx: 0,
            // 3100 * 2^64
            price: big("57184906628499610009600"),
            cumulative_volume: big("1000000000000000000"),
            // Full-bin input at avg price 3055 (1 WETH * 3055) = 3055 USDC.
            cumulative_input_volume: big("3055000000"),
        }];

        let result = state
            .get_amount_out(BigUint::from(3_000_000_000u64), &state.quote_token, &state.base_token)
            .unwrap();

        assert!(result.amount < BigUint::from(1_000_000_000_000_000_000u128));
        assert!(result.amount > BigUint::from(980_000_000_000_000_000u128));
    }

    fn depth_bin(cumulative_volume: &str, cumulative_input_volume: &str) -> MetricDepthBin {
        MetricDepthBin {
            bin_idx: 0,
            // 2900 * 2^64; the integer walk prices from the volume/input columns, not this field.
            price: big("53495557813757699686400"),
            cumulative_volume: big(cumulative_volume),
            cumulative_input_volume: big(cumulative_input_volume),
        }
    }

    #[test]
    fn test_depth_output_for_input_partially_fills_bid_bin() {
        // Full-bin input at price 2950: 3000 USDC costs 3000/2950 ≈ 1.0169 WETH.
        let bins = vec![depth_bin("3000000000", "1016949152542372881")];

        let fill = depth_output_for_input(
            &bins,
            &BigUint::from(1_000_000_000_000_000_000u128),
            &BigUint::from(3_000_000_000u64),
        )
        .unwrap();

        // Pro-rata within the bin: 1 WETH buys exactly 2950 USDC.
        assert_eq!(fill.output, BigUint::from(2_950_000_000u64));
        assert!(!fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_partially_fills_ask_bin() {
        // Full-bin input at price 3055: 1 WETH costs 3055 USDC.
        let bins = vec![depth_bin("1000000000000000000", "3055000000")];

        let fill = depth_output_for_input(
            &bins,
            &BigUint::from(3_000_000_000u64),
            &BigUint::from(1_000_000_000_000_000_000u128),
        )
        .unwrap();

        // Pro-rata within the bin, rounded down: 1e18 * 3000 / 3055.
        let expected = BigUint::from(1_000_000_000_000_000_000u128) *
            BigUint::from(3_000_000_000u64) /
            BigUint::from(3_055_000_000u64);
        assert_eq!(fill.output, expected);
        assert!(!fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_exhausts_available_depth() {
        let bins = vec![depth_bin("3000000000", "1016949152542372881")];

        let fill = depth_output_for_input(
            &bins,
            &BigUint::from(2_000_000_000_000_000_000u128),
            &BigUint::from(3_000_000_000u64),
        )
        .unwrap();

        assert_eq!(fill.output, BigUint::from(3_000_000_000u64));
        assert!(fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_walks_multiple_bins() {
        // Bin 1 sells 3000 USDC for 1 WETH; bin 2 sells another 3000 USDC for 1.1 WETH.
        let bins = vec![
            depth_bin("3000000000", "1000000000000000000"),
            depth_bin("6000000000", "2100000000000000000"),
        ];

        let fill = depth_output_for_input(
            &bins,
            // 1.55 WETH: consumes bin 1 fully, then half of bin 2's 1.1 WETH.
            &BigUint::from(1_550_000_000_000_000_000u128),
            &BigUint::from(6_000_000_000u64),
        )
        .unwrap();

        // 3000 + 3000 * 0.55/1.1 = 4500 USDC.
        assert_eq!(fill.output, BigUint::from(4_500_000_000u64));
        assert!(!fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_caps_slice_to_aggregate_inventory() {
        let bins = vec![depth_bin("3000000000", "1000000000000000000")];

        // Aggregate inventory truncates the bin to 1500 USDC; the fillable slice costs a
        // pro-rata 0.5 WETH, so 1 WETH exhausts it.
        let fill = depth_output_for_input(
            &bins,
            &BigUint::from(1_000_000_000_000_000_000u128),
            &BigUint::from(1_500_000_000u64),
        )
        .unwrap();

        assert_eq!(fill.output, BigUint::from(1_500_000_000u64));
        assert!(fill.exhausted);
    }

    #[test]
    fn test_depth_output_for_input_rejects_inconsistent_bin() {
        // Volume without input would hand out output for free.
        let bins = vec![depth_bin("3000000000", "0")];

        let err = depth_output_for_input(
            &bins,
            &BigUint::from(1_000_000_000_000_000_000u128),
            &BigUint::from(3_000_000_000u64),
        )
        .unwrap_err();

        assert!(matches!(err, SimulationError::RecoverableError(_)));
    }

    #[test]
    fn test_depth_output_for_input_rejects_non_monotonic_bins() {
        let bins = vec![
            depth_bin("3000000000", "1000000000000000000"),
            // Cumulative volume goes backwards.
            depth_bin("2000000000", "2000000000000000000"),
        ];

        let err = depth_output_for_input(
            &bins,
            &BigUint::from(2_000_000_000_000_000_000u128),
            &BigUint::from(3_000_000_000u64),
        )
        .unwrap_err();

        assert!(matches!(err, SimulationError::RecoverableError(_)));
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API"]
    async fn test_live_metric_api_state_get_amount_out_and_signed_quote() {
        use crate::rfq::protocols::metric::models::PaginatedMetadataResponse;

        let weth = weth();
        let usdc = usdc();
        let config = crate::rfq::constants::get_metric_config();
        let base_url = config
            .base_url
            .trim_end_matches('/')
            .to_string();
        let client = MetricClient::new(
            Chain::Ethereum,
            HashSet::from([weth.address.clone(), usdc.address.clone()]),
            0.0,
            base_url.clone(),
            config.api_key.clone(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .unwrap();

        let http_client = reqwest::Client::new();
        let metadata: PaginatedMetadataResponse = http_client
            .get(format!("{base_url}/public/v1/evm/1/metadata"))
            .header("accept", "application/json")
            .query(&[("count", "500")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let mut selected = None;
        for pool in metadata
            .data
            .into_iter()
            .filter(|pool| pool.token0 == weth.address && pool.token1 == usdc.address)
        {
            let checksummed =
                alloy::primitives::Address::from_slice(&pool.pool_address).to_checksum(None);
            let mut request = http_client
                .get(format!("{base_url}/public/v1/evm/1/{checksummed}/bid_ask"))
                .header("accept", "application/json");
            if let Some(api_key) = &config.api_key {
                request = request.bearer_auth(api_key);
            }
            let bid_ask: MetricBidAskResponse = request
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
            if bid_ask.is_quotable() && has_enough_quote_liquidity {
                selected = Some((pool, bid_ask));
                break;
            }
        }

        let Some((metadata, bid_ask)) = selected else {
            eprintln!("Metric live API returned no liquid Ethereum WETH/USDC pool; skipping");
            return;
        };

        let state = MetricState::new(weth, usdc, metadata, bid_ask, client);
        assert!(state.bid_ask.is_quotable());

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
        // The heartbeat model relays no oracle-update args, so the quote carries no attributes.
        assert!(signed_quote.quote_attributes.is_empty());
    }
}
