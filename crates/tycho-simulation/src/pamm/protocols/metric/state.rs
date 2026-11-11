use std::{any::Any, collections::HashMap};

use num_bigint::BigUint;
use num_traits::{CheckedSub, Zero};
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::{
    book::{component::PairState, sim},
    pamm::protocols::metric::models::{MetricBidAskResponse, MetricDepthBin, MetricMetadata},
};

/// Gas estimate for one MetricExecutor swap (pool swap + callback settlement).
const METRIC_SWAP_GAS: u64 = 235_000;

/// The error of a swap on the other direction once a swap filled this one.
const ALREADY_QUOTED: &str = "Metric pool already quoted in this route";

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct MetricState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug("{{ pool_address: {:?}, .. }}", metadata.pool_address)]
    pub(super) metadata: MetricMetadata,
    /// The depth bins are too long for a log line; the timestamp says which quote this is.
    #[debug("{{ server_ts: {}, .. }}", bid_ask.server_ts)]
    pub(super) bid_ask: MetricBidAskResponse,
    /// What earlier swaps filled on this state. Older serialized states carry none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fill: Option<MetricFill>,
}

/// The merged swap earlier swaps on one direction have made so far, in atomic units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MetricFill {
    direction: MetricDirection,
    /// The input of the merged swap so far.
    amount_in: BigUint,
    /// The output the merged swap has paid so far.
    amount_out: BigUint,
}

impl MetricState {
    /// The state of a pool quoted at `bid_ask`, before any swap.
    pub(super) fn new(
        base_token: Token,
        quote_token: Token,
        metadata: MetricMetadata,
        bid_ask: MetricBidAskResponse,
    ) -> Self {
        Self { base_token, quote_token, metadata, bid_ask, fill: None }
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

    /// The quoted price of the book side `direction` trades against: the bid when selling
    /// token0, the ask when buying it. `None` when the pool quotes no price on that side.
    fn quoted_price(&self, direction: MetricDirection) -> Result<Option<f64>, SimulationError> {
        let price = match direction {
            MetricDirection::ZeroForOne => self.bid_ask.bid_price()?,
            MetricDirection::OneForZero => self.bid_ask.ask_price()?,
        };
        Ok(price)
    }

    /// The fill an earlier swap made on `direction`, if any. Errors when the state was filled on
    /// the other direction.
    fn fill_on(&self, direction: MetricDirection) -> Result<Option<&MetricFill>, SimulationError> {
        match &self.fill {
            Some(fill) if fill.direction == direction => Ok(Some(fill)),
            Some(_) => Err(SimulationError::RecoverableError(ALREADY_QUOTED.to_string())),
            None => Ok(None),
        }
    }

    /// The output inventory a swap in `direction` draws on.
    fn max_output(&self, direction: MetricDirection) -> Result<BigUint, SimulationError> {
        Ok(match direction {
            MetricDirection::ZeroForOne => self.bid_ask.total_token1_available()?,
            MetricDirection::OneForZero => self.bid_ask.total_token0_available()?,
        })
    }

    /// The output of one swap of `amount_in` against the quoted book, and the refusal message
    /// when the book cannot fill all of it. The output is then what the book can fill.
    fn price_input(
        &self,
        direction: MetricDirection,
        amount_in: &BigUint,
        price: f64,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<(BigUint, Option<String>), SimulationError> {
        let max_output = self.max_output(direction)?;

        // Prefer size-aware depth when Metric exposes it. The depth walk runs entirely in raw
        // integer units on Metric's own per-bin accounting, so the cap it reports when the depth
        // is exhausted is exact (no f64 round-trip); see `depth_output_for_input` for the
        // intra-bin rounding.
        if let Some(quote) = self.quote_with_depth(direction, amount_in, &max_output)? {
            let refusal = quote.exhausted.then(|| {
                format!(
                    "Metric pool depth exhausted. Input {amount_in} cannot be fully filled; \
                     tradable depth caps output at {}",
                    quote.max_output
                )
            });
            return Ok((quote.amount_out, refusal));
        }

        // No depth bins: flat top-of-book quote, capped only by the aggregate inventory.
        let amount_in_human = sim::to_human(amount_in, token_in.decimals);
        let flat_amount_out_human = match direction {
            MetricDirection::ZeroForOne => amount_in_human * price,
            MetricDirection::OneForZero => amount_in_human / price,
        };
        let amount_out = sim::to_atomic(flat_amount_out_human, token_out.decimals)?;
        if amount_out > max_output {
            let refusal = format!(
                "Metric pool has not enough liquidity. Requested output {amount_out} exceeds \
                 available {max_output}"
            );
            return Ok((max_output, Some(refusal)));
        }
        Ok((amount_out, None))
    }

    /// The price of the next input unit after `fill`, in quote tokens per base token, as the
    /// depth walk prices it: the average price of the bin the fill stopped in. Without depth bins
    /// it is the flat top-of-book price. A recoverable error once the fill used up the side.
    fn price_after_fill(&self, fill: &MetricFill) -> Result<f64, SimulationError> {
        let no_liquidity = || SimulationError::RecoverableError("No liquidity".into());
        let (bins, in_decimals, out_decimals) = match fill.direction {
            MetricDirection::ZeroForOne => {
                (&self.bid_ask.depth.bids, self.base_token.decimals, self.quote_token.decimals)
            }
            MetricDirection::OneForZero => {
                (&self.bid_ask.depth.asks, self.quote_token.decimals, self.base_token.decimals)
            }
        };
        if fill.amount_out >= self.max_output(fill.direction)? {
            return Err(no_liquidity());
        }
        if bins.is_empty() {
            return self
                .quoted_price(fill.direction)?
                .ok_or_else(no_liquidity);
        }
        let mut previous_output = &BigUint::ZERO;
        let mut previous_input = &BigUint::ZERO;
        for bin in bins {
            if bin.cumulative_input_volume > fill.amount_in {
                let (Some(input), Some(output)) = (
                    bin.cumulative_input_volume
                        .checked_sub(previous_input),
                    bin.cumulative_volume
                        .checked_sub(previous_output),
                ) else {
                    return Err(SimulationError::RecoverableError(
                        "Metric depth cumulative volumes are not monotonic".into(),
                    ));
                };
                if output.is_zero() {
                    return Err(SimulationError::RecoverableError(
                        "Metric depth bin has inconsistent volume and input".into(),
                    ));
                }
                let input = sim::to_human(&input, in_decimals);
                let output = sim::to_human(&output, out_decimals);
                return Ok(match fill.direction {
                    MetricDirection::ZeroForOne => output / input,
                    MetricDirection::OneForZero => input / output,
                });
            }
            previous_output = &bin.cumulative_volume;
            previous_input = &bin.cumulative_input_volume;
        }
        Err(no_liquidity())
    }

    /// The limits of a swap in `direction` on the quoted book, before any fill.
    fn unfilled_limits(
        &self,
        direction: MetricDirection,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let Some(price) = self.quoted_price(direction)? else {
            // Nothing on this side is no swap at any size, so there is no limit to report.
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        };
        // Price of one buy-token unit in sell tokens, plus the per-direction inventory cap, depth
        // side, and token decimals.
        let (sell_per_buy, aggregate, bins, sell_decimals, buy_decimals) = match direction {
            MetricDirection::ZeroForOne => (
                1.0 / price,
                self.bid_ask.total_token1_available()?,
                &self.bid_ask.depth.bids,
                self.base_token.decimals,
                self.quote_token.decimals,
            ),
            MetricDirection::OneForZero => (
                price,
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

        // No depth bins, or the aggregate inventory truncates the walkable depth: either way the
        // aggregate is the binding output cap, and get_amount_out rejects anything beyond it
        // (as insufficient liquidity or depth-exhausted respectively). Estimate the matching
        // input from the top-of-book price.
        let buy_limit = aggregate;
        let buy_limit_human = sim::to_human(&buy_limit, buy_decimals);
        let sell_limit = sim::to_atomic(buy_limit_human * sell_per_buy, sell_decimals)?;
        Ok((sell_limit, buy_limit))
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

        // `is_quotable` only requires one side of the book to be populated, so the traded side
        // may still have no bins when the depth is one-sided. The top-of-book quote is then the
        // best signal we have.
        let Some(depth_max_output) = depth_max_output(bins) else {
            return Ok(None);
        };

        let effective_max_output = depth_max_output.min(max_output.clone());
        let depth_fill = depth_output_for_input(bins, amount_in, &effective_max_output)?;

        Ok(Some(DepthQuote {
            amount_out: depth_fill.output,
            max_output: effective_max_output,
            exhausted: depth_fill.exhausted,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

impl PairState for MetricState {
    fn base_token(&self) -> &Token {
        &self.base_token
    }

    fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    fn directions(&self) -> Vec<(Bytes, Bytes)> {
        // After a fill only its own direction can continue; the other one is refused.
        let priced = |direction| {
            self.fill_on(direction).is_ok() &&
                matches!(self.quoted_price(direction), Ok(Some(price)) if price > 0.0)
        };
        sim::directions(
            &self.base_token,
            &self.quote_token,
            priced(MetricDirection::ZeroForOne),
            priced(MetricDirection::OneForZero),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for MetricState {
    fn fee(&self) -> f64 {
        0.0
    }

    /// Before a fill, the mid of the bid and ask. After a fill, the price of the next unit on
    /// the filled side, since the other side is refused.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let price = if let Some(fill) = &self.fill {
            self.price_after_fill(fill)?
        } else {
            match (self.bid_ask.bid_price()?, self.bid_ask.ask_price()?) {
                (Some(bid), Some(ask)) => (bid + ask) / 2.0,
                (Some(bid), None) => bid,
                (None, Some(ask)) => ask,
                (None, None) => {
                    return Err(SimulationError::RecoverableError("No liquidity".into()))
                }
            }
        };
        if base.address == self.base_token.address && quote.address == self.quote_token.address {
            Ok(price)
        } else if base.address == self.quote_token.address &&
            quote.address == self.base_token.address
        {
            Ok(1.0 / price)
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

    /// Records the fill in the returned state, also when the book fills only part of the input.
    /// After a swap on the same direction it pays the output of the merged input less the output
    /// so far, at zero gas, so the book is priced from the position the earlier swaps left it at.
    /// A swap on the other direction fails: the quote says nothing about the pool's price on that
    /// side once it filled this one. The caller merges all swaps on one direction into one swap.
    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // The pool a Metric swap settles against reverts `InvalidAmount()` on a zero input, and
        // the venue's routing API answers one with `400 amountIn must be positive`.
        sim::require_positive_amount(&amount_in)?;

        let direction = self.direction(&token_in.address, &token_out.address)?;
        let Some(price) = self.quoted_price(direction)? else {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Metric pool quotes no price in this direction. Input {amount_in} cannot be \
                     filled"
                ),
                None,
            ));
        };
        let previous = self.fill_on(direction)?;
        let merged_in = match previous {
            Some(previous) => &previous.amount_in + &amount_in,
            None => amount_in,
        };
        let (merged_out, refusal) =
            self.price_input(direction, &merged_in, price, token_in, token_out)?;
        let (amount, gas) = match previous {
            Some(previous) => {
                (sim::saturating_sub(&merged_out, &previous.amount_out), BigUint::ZERO)
            }
            None => (merged_out.clone(), BigUint::from(METRIC_SWAP_GAS)),
        };
        let new_state = Self {
            fill: Some(MetricFill { direction, amount_in: merged_in, amount_out: merged_out }),
            ..self.clone()
        };
        let result = GetAmountOutResult { amount, gas, new_state: Box::new(new_state) };
        match refusal {
            Some(message) => Err(SimulationError::InvalidInput(message, Some(result))),
            None => Ok(result),
        }
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction = self.direction(&sell_token, &buy_token)?;
        let fill = self.fill_on(direction)?;
        let (sell_limit, buy_limit) = self.unfilled_limits(direction)?;
        Ok(match fill {
            Some(fill) => (
                sim::saturating_sub(&sell_limit, &fill.amount_in),
                sim::saturating_sub(&buy_limit, &fill.amount_out),
            ),
            None => (sell_limit, buy_limit),
        })
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
                self.bid_ask == other_state.bid_ask &&
                self.fill == other_state.fill
        } else {
            false
        }
    }
}

fn depth_max_output(bins: &[MetricDepthBin]) -> Option<BigUint> {
    bins.last()
        .map(|bin| bin.cumulative_volume.clone())
}

/// Walks the depth bins and computes the output bought by `amount_in`, entirely in raw integer
/// units using Metric's own per-bin accounting.
///
/// Full bins cost exactly their `cumulativeInputVolume` difference — Metric's authoritative,
/// fee-adjusted accounting. Partial fills are priced pro-rata at the bin's average cost: under
/// the documented linear intra-bin model this is exact at bin boundaries and conservative inside
/// the bin (off by at most half the bin's price width, understating the output; the division
/// also rounds down).
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::Chain;

    use super::*;
    use crate::pamm::protocols::metric::models::MetricDepth;

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
            pool_address: Bytes::from_str("0xbF48bCf474d57fF82A3215319229e0DE1476A557").unwrap(),
            token0: weth.address.clone(),
            token1: usdc.address.clone(),
            tvl_fiat: Some(3000.0),
            pause_level: Some(0),
        };
        let bid_ask = MetricBidAskResponse {
            // 3000 * 2^64
            bid_adj: big("55340232221128654848000"),
            // 3010 * 2^64
            ask_adj: big("55524699661865750400000"),
            total_token0_available: Some(big("10000000000000000000")),
            total_token1_available: Some(big("30000000000")),
            server_ts: 100,
            price_provider_status: Some("healthy".to_string()),
            depth: MetricDepth::default(),
        };
        MetricState::new(weth, usdc, metadata, bid_ask)
    }

    /// A book with an ask side only: one bin selling 1 WETH for 3011 USDC.
    fn state_without_bid() -> MetricState {
        let mut state = state();
        state.bid_ask.bid_adj = BigUint::zero();
        state.bid_ask.depth.asks = vec![depth_bin("1000000000000000000", "3011000000")];
        state
    }

    /// A book with a bid side only: one bin buying 1 WETH for 2999 USDC.
    fn state_without_ask() -> MetricState {
        let mut state = state();
        state.bid_ask.ask_adj = BigUint::from(u128::MAX);
        state.bid_ask.depth.bids = vec![depth_bin("2999000000", "1000000000000000000")];
        state
    }

    #[test]
    fn book_without_bid_trades_only_the_ask_side() {
        let state = state_without_bid();
        let ask = state
            .bid_ask
            .ask_price()
            .unwrap()
            .unwrap();

        assert!(state.bid_ask.is_quotable());
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            ask
        );
        assert!(matches!(
            state.get_limits(weth().address, usdc().address),
            Err(SimulationError::RecoverableError(_))
        ));
        let Err(SimulationError::InvalidInput(msg, None)) =
            state.get_amount_out(big("100000000000000000"), &weth(), &usdc())
        else {
            panic!("selling WETH into a book without a bid must fail");
        };
        assert!(msg.contains("quotes no price in this direction"), "{msg}");
        assert_eq!(
            state
                .get_limits(usdc().address, weth().address)
                .unwrap(),
            (big("3011000000"), big("1000000000000000000"))
        );
    }

    #[test]
    fn book_without_ask_trades_only_the_bid_side() {
        let state = state_without_ask();
        let bid = state
            .bid_ask
            .bid_price()
            .unwrap()
            .unwrap();

        assert!(state.bid_ask.is_quotable());
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            bid
        );
        assert!(matches!(
            state.get_limits(usdc().address, weth().address),
            Err(SimulationError::RecoverableError(_))
        ));
        let Err(SimulationError::InvalidInput(msg, None)) =
            state.get_amount_out(big("300000000"), &usdc(), &weth())
        else {
            panic!("buying WETH from a book without an ask must fail");
        };
        assert!(msg.contains("quotes no price in this direction"), "{msg}");
        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (big("1000000000000000000"), big("2999000000"))
        );
    }

    #[test]
    fn book_without_quotes_has_no_spot_price() {
        let mut state = state();
        state.bid_ask.bid_adj = BigUint::zero();
        state.bid_ask.ask_adj = BigUint::from(u128::MAX);

        let Err(SimulationError::RecoverableError(msg)) = state.spot_price(&weth(), &usdc()) else {
            panic!("a book without quotes has no spot price");
        };
        assert_eq!(msg, "No liquidity");
    }

    #[test]
    fn get_amount_out_zero_for_one() {
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
    fn get_amount_out_one_for_zero() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(3_010_000_000u64), &state.quote_token, &state.base_token)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(1_000_000_000_000_000_000u128));
    }

    #[test]
    fn get_amount_out_caps_to_available_liquidity() {
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
    fn get_amount_out_depth_exhausted_reports_depth_message() {
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
            other @ (SimulationError::FatalError(_) |
            SimulationError::InvalidInput(..) |
            SimulationError::RecoverableError(_)) => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn get_amount_out_exhausted_returns_exact_cap() {
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
            other @ (SimulationError::FatalError(_) |
            SimulationError::InvalidInput(..) |
            SimulationError::RecoverableError(_)) => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn get_limits_caps_to_depth() {
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
    fn get_limits_aggregate_truncates_depth() {
        let mut state = state();
        // Aggregate inventory (1000 USDC) is below the 3000 USDC depth total, so the exact
        // last-bin input does not apply and the limit falls back to the top-of-book estimate.
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
    fn get_limits_uses_aggregate_without_depth() {
        let state = state();

        let (_, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        // No depth bins: fall back to aggregate inventory (30000 USDC).
        assert_eq!(buy_limit, BigUint::from(30_000_000_000u64));
    }

    #[test]
    fn get_amount_out_walks_bid_depth() {
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
    fn get_amount_out_walks_ask_depth() {
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
    fn depth_output_for_input_partially_fills_bid_bin() {
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
    fn depth_output_for_input_partially_fills_ask_bin() {
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
    fn depth_output_for_input_exhausts_available_depth() {
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
    fn depth_output_for_input_walks_multiple_bins() {
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
    fn depth_output_for_input_caps_slice_to_aggregate_inventory() {
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
    fn depth_output_for_input_rejects_inconsistent_bin() {
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
    fn depth_output_for_input_rejects_non_monotonic_bins() {
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

    fn sell_weth(state: &MetricState, amount: u128) -> Result<GetAmountOutResult, SimulationError> {
        state.get_amount_out(BigUint::from(amount), &state.base_token, &state.quote_token)
    }

    fn buy_weth(state: &MetricState, amount: u128) -> Result<GetAmountOutResult, SimulationError> {
        state.get_amount_out(BigUint::from(amount), &state.quote_token, &state.base_token)
    }

    fn metric_state(result: &GetAmountOutResult) -> &MetricState {
        result
            .new_state
            .as_any()
            .downcast_ref::<MetricState>()
            .unwrap()
    }

    /// The partial fill a refused swap still reports.
    fn partial(result: Result<GetAmountOutResult, SimulationError>) -> GetAmountOutResult {
        match result {
            Err(SimulationError::InvalidInput(_, Some(partial))) => partial,
            other => panic!("expected a partial fill, got {other:?}"),
        }
    }

    /// Bin 1 buys 1 WETH for 3000 USDC, bin 2 another 1 WETH for 2000 USDC.
    fn two_bin_bid_state() -> MetricState {
        let mut state = state();
        state.bid_ask.depth.bids = vec![
            depth_bin("3000000000", "1000000000000000000"),
            depth_bin("5000000000", "2000000000000000000"),
        ];
        state
    }

    #[test]
    fn fills_in_sequence_pay_what_one_fill_of_their_total_pays() {
        let mut state = state();
        // Bin 1 buys 1 WETH for 3000 USDC; bin 2 buys another 1.1 WETH for 3000 USDC.
        state.bid_ask.depth.bids = vec![
            depth_bin("3000000000", "1000000000000000000"),
            depth_bin("6000000000", "2100000000000000000"),
        ];

        // The first fill stops inside bin 1, the second one inside bin 2.
        let first = sell_weth(&state, 500_000_000_000_000_000).unwrap();
        let second = sell_weth(metric_state(&first), 1_050_000_000_000_000_000).unwrap();
        let whole = sell_weth(&state, 1_550_000_000_000_000_000).unwrap();

        assert_eq!(first.amount, BigUint::from(1_500_000_000u64));
        assert_eq!(second.amount, BigUint::from(3_000_000_000u64));
        assert_eq!(&first.amount + &second.amount, whole.amount);
        assert_eq!(first.gas, BigUint::from(METRIC_SWAP_GAS));
        assert_eq!(second.gas, BigUint::ZERO);
    }

    #[test]
    fn fill_that_uses_up_the_depth_refuses_the_next_one() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![depth_bin("3000000000", "1000000000000000000")];

        let first = sell_weth(&state, 1_000_000_000_000_000_000).unwrap();
        let next = partial(sell_weth(metric_state(&first), 100_000_000_000_000_000));

        assert_eq!(first.amount, BigUint::from(3_000_000_000u64));
        assert_eq!(next.amount, BigUint::ZERO);
    }

    #[test]
    fn partial_fill_counts_towards_the_next_one() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![depth_bin("3000000000", "1000000000000000000")];

        let first = partial(sell_weth(&state, 2_000_000_000_000_000_000));
        let next = partial(sell_weth(metric_state(&first), 100_000_000_000_000_000));

        assert_eq!(first.amount, BigUint::from(3_000_000_000u64));
        assert_eq!(next.amount, BigUint::ZERO);
    }

    #[test]
    fn inventory_cap_inside_a_bin_binds_across_fills() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![depth_bin("3000000000", "1000000000000000000")];
        state.bid_ask.total_token1_available = Some(big("2000000000"));

        let first = sell_weth(&state, 400_000_000_000_000_000).unwrap();
        let second = partial(sell_weth(metric_state(&first), 400_000_000_000_000_000));

        // The capped bin charges its input rounded up, so the first fill rounds a unit down and
        // the second one makes up for it.
        assert_eq!(first.amount, BigUint::from(1_199_999_999u64));
        assert_eq!(second.amount, BigUint::from(800_000_001u64));
    }

    #[test]
    fn flat_fill_counts_towards_the_inventory() {
        // No depth bins: 3000 USDC per WETH up to the 30000 USDC inventory.
        let state = state();

        let first = sell_weth(&state, 5_000_000_000_000_000_000).unwrap();
        let second = partial(sell_weth(metric_state(&first), 6_000_000_000_000_000_000));

        assert_eq!(first.amount, BigUint::from(15_000_000_000u64));
        assert_eq!(second.amount, BigUint::from(15_000_000_000u64));
        assert!(sell_weth(&state, 6_000_000_000_000_000_000).is_ok());
    }

    #[test]
    fn fill_refuses_the_other_direction() {
        let state = two_bin_bid_state();

        let first = sell_weth(&state, 500_000_000_000_000_000).unwrap();
        let after = metric_state(&first);

        assert!(buy_weth(&state, 1_000_000_000).is_ok());
        assert!(matches!(
            buy_weth(after, 1_000_000_000),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(
            after.get_limits(usdc().address, weth().address),
            Err(SimulationError::RecoverableError(_))
        ));
        assert_eq!(after.directions(), vec![(weth().address, usdc().address)]);
    }

    #[test]
    fn spot_price_after_a_fill_is_the_next_units_price() {
        let state = two_bin_bid_state();

        let inside_bin_1 = sell_weth(&state, 500_000_000_000_000_000).unwrap();
        let into_bin_2 = sell_weth(&state, 1_000_000_000_000_000_000).unwrap();
        let all = sell_weth(&state, 2_000_000_000_000_000_000).unwrap();
        let price = |result: &GetAmountOutResult| metric_state(result).spot_price(&weth(), &usdc());

        assert_eq!(price(&inside_bin_1).unwrap(), 3000.0);
        assert_eq!(price(&into_bin_2).unwrap(), 2000.0);
        assert_eq!(
            metric_state(&into_bin_2)
                .spot_price(&usdc(), &weth())
                .unwrap(),
            1.0 / 2000.0
        );
        assert!(matches!(price(&all), Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn limits_after_a_fill_are_what_is_left() {
        let state = two_bin_bid_state();

        let first = sell_weth(&state, 500_000_000_000_000_000).unwrap();

        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (big("2000000000000000000"), big("5000000000"))
        );
        assert_eq!(
            metric_state(&first)
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (big("1500000000000000000"), big("3500000000"))
        );
    }

    #[test]
    fn fill_survives_serialization() {
        let first = sell_weth(&two_bin_bid_state(), 500_000_000_000_000_000).unwrap();
        let after = metric_state(&first);

        let json = serde_json::to_string(after).unwrap();
        let restored: MetricState = serde_json::from_str(&json).unwrap();

        assert!(ProtocolSim::eq(&restored, after));
        assert!(!ProtocolSim::eq(&restored, &two_bin_bid_state()));
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_state_get_amount_out() {
        use crate::pamm::protocols::metric::models::PaginatedMetadataResponse;

        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");

        // Base: the only supported chain with pools published on the live API so far.
        let weth = base_weth();
        let usdc = base_usdc();
        let base_url = "https://api.metric.xyz";

        let http_client = reqwest::Client::new();
        let metadata: PaginatedMetadataResponse = http_client
            .get(format!("{base_url}/public/v1/evm/8453/metadata"))
            .header("accept", "application/json")
            .query(&[("count", "500")])
            .bearer_auth(&api_key)
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
            let bid_ask: MetricBidAskResponse = http_client
                .get(format!("{base_url}/public/v1/evm/8453/{checksummed}/bid_ask"))
                .header("accept", "application/json")
                .bearer_auth(&api_key)
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
            eprintln!("Metric live API returned no liquid Base WETH/USDC pool; skipping");
            return;
        };

        let state = MetricState::new(weth, usdc, metadata, bid_ask);
        assert!(state.bid_ask.is_quotable());

        let amount_in = BigUint::from(1_000_000_000u64);
        let quote = state
            .get_amount_out(amount_in, &state.base_token, &state.quote_token)
            .unwrap();

        assert!(quote.amount > BigUint::from(0u8));
    }

    /// The pool a Metric swap settles against reverts on a zero input, so simulating one must
    /// not report a fill.
    #[test]
    fn rejects_a_zero_amount() {
        let state = state();

        let result = state.get_amount_out(BigUint::ZERO, &weth(), &usdc());

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))), "{result:?}");
    }
}
