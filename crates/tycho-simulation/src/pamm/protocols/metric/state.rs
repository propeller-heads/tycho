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

use super::fill::{exact_fill_inside_bin, Bin, BookSide};
use crate::{
    book::{component::PairState, sim},
    pamm::protocols::metric::models::{MetricBidAskResponse, MetricDepthBin, MetricMetadata},
    utils::saturating_sub,
};

/// Gas estimate for one MetricExecutor swap (pool swap + callback settlement).
const METRIC_SWAP_GAS: u64 = 235_000;

/// The error of a swap on the other direction once a swap filled this one.
const ALREADY_QUOTED: &str = "Metric pool already quoted in this route";

/// The error for a side Metric quotes no price on, or holds no depth on.
const NO_TRADABLE_SIDE: &str =
    "Metric pool has no tradable side in this direction (no price or no depth)";

/// The error for a direction an earlier fill has used up.
const USED_UP_BY_FILL: &str = "Metric side is used up by earlier fills in this route";

/// The error for a state that prices neither direction.
const NO_LIQUIDITY: &str = "No liquidity";

#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct MetricState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    #[debug("{{ pool_address: {:?}, .. }}", metadata.pool_address)]
    pub(super) metadata: MetricMetadata,
    /// The depth bins are too long for a log line; the timestamp says which quote this is.
    #[debug("{{ server_ts: {}, .. }}", bid_ask.server_ts)]
    pub(super) bid_ask: MetricBidAskResponse,
    /// What earlier swaps filled on this state. A state serialized without a fill deserializes
    /// with none, as `PriceLevelStreamState` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) fill: Option<MetricFill>,
}

/// The merged swap earlier swaps on one direction have made so far, in atomic units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct MetricFill {
    direction: MetricDirection,
    /// The input of the merged swap so far.
    amount_in: BigUint,
    /// The output the merged swap has paid so far.
    amount_out: BigUint,
}

impl MetricState {
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

    /// The book side `direction` trades against — the bid when selling token0, the ask when
    /// buying it — or `None` when that side cannot be traded: Metric quotes no price on it or it
    /// has no depth.
    fn tradable_side(
        &self,
        direction: MetricDirection,
    ) -> Result<Option<TradableSide<'_>>, SimulationError> {
        let bins = match direction {
            MetricDirection::ZeroForOne => self.bid_ask.tradable_bids(),
            MetricDirection::OneForZero => self.bid_ask.tradable_asks(),
        };
        let Some(bins) = bins else {
            return Ok(None);
        };
        let Some((last_bin, _)) = bins.split_last() else {
            return Ok(None);
        };
        let price = match direction {
            MetricDirection::ZeroForOne => self.bid_ask.bid_price()?,
            MetricDirection::OneForZero => self.bid_ask.ask_price()?,
        };
        let Some(price) = price else {
            return Ok(None);
        };
        let side = match direction {
            MetricDirection::ZeroForOne => BookSide::Bids,
            MetricDirection::OneForZero => BookSide::Asks,
        };
        let decimals = (self.base_token.decimals, self.quote_token.decimals);
        Ok(Some(TradableSide { price, bins, last_bin, side, decimals }))
    }

    /// The fill an earlier swap made on `direction`, if any. Errors when the state was filled on
    /// the other direction.
    fn recorded_fill(
        &self,
        direction: MetricDirection,
    ) -> Result<Option<&MetricFill>, SimulationError> {
        match &self.fill {
            Some(fill) if fill.direction == direction => Ok(Some(fill)),
            Some(_) => Err(SimulationError::InvalidInput(ALREADY_QUOTED.to_string(), None)),
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

    /// Whether `direction` can be traded now. After a fill only its own direction can continue,
    /// and only while its limits still leave output to buy. A direction whose state cannot be read
    /// is left out and logged, since `directions` returns no error.
    fn is_priced(&self, direction: MetricDirection) -> bool {
        let pool = &self.metadata.pool_address;
        match self.recorded_fill(direction) {
            Ok(Some(_)) => {
                let (sell, buy) = match direction {
                    MetricDirection::ZeroForOne => (&self.base_token, &self.quote_token),
                    MetricDirection::OneForZero => (&self.quote_token, &self.base_token),
                };
                match self.get_limits(sell.address.clone(), buy.address.clone()) {
                    Ok((_, buy_limit)) => !buy_limit.is_zero(),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            %pool,
                            ?direction,
                            "Metric filled direction left out: limits unreadable"
                        );
                        false
                    }
                }
            }
            Ok(None) => match self.tradable_side(direction) {
                Ok(side) => side.is_some_and(|side| side.price > 0.0),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        %pool,
                        ?direction,
                        "Metric direction left out: price unreadable"
                    );
                    false
                }
            },
            // The other direction after a fill is the expected refusal, so it is not logged.
            Err(_) => false,
        }
    }

    /// The average price of the whole bin the fill stopped in, in quote tokens per base token.
    /// An invalid-input error once the fill used up the side.
    fn price_after_fill(&self, fill: &MetricFill) -> Result<f64, SimulationError> {
        let Some(side) = self.tradable_side(fill.direction)? else {
            return Err(SimulationError::InvalidInput(NO_TRADABLE_SIDE.into(), None));
        };
        if fill.amount_out >= self.max_output(fill.direction)? {
            return Err(SimulationError::InvalidInput(USED_UP_BY_FILL.into(), None));
        }
        let (in_decimals, out_decimals) = match fill.direction {
            MetricDirection::ZeroForOne => (side.decimals.0, side.decimals.1),
            MetricDirection::OneForZero => (side.decimals.1, side.decimals.0),
        };
        let mut previous_output = BigUint::ZERO;
        let mut previous_input = BigUint::ZERO;
        for bin in side.bins {
            let (Some(input), Some(output)) = (
                bin.cumulative_input_volume
                    .checked_sub(&previous_input),
                bin.cumulative_volume
                    .checked_sub(&previous_output),
            ) else {
                return Err(non_monotonic_bin(bin));
            };
            // Price-grid bins without liquidity carry neither volume nor input.
            if input.is_zero() && output.is_zero() {
                continue;
            }
            if input.is_zero() || output.is_zero() {
                return Err(SimulationError::RecoverableError(
                    "Metric depth bin has inconsistent volume and input".into(),
                ));
            }
            if bin.cumulative_input_volume > fill.amount_in {
                let input = sim::to_human(&input, in_decimals);
                let output = sim::to_human(&output, out_decimals);
                return Ok(match fill.direction {
                    MetricDirection::ZeroForOne => output / input,
                    MetricDirection::OneForZero => input / output,
                });
            }
            previous_output = bin.cumulative_volume.clone();
            previous_input = bin.cumulative_input_volume.clone();
        }
        Err(SimulationError::InvalidInput("Metric depth holds no bin past this fill".into(), None))
    }

    /// The limits of a swap in `direction` on the quoted book, before any fill.
    fn unfilled_limits(
        &self,
        direction: MetricDirection,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let Some(side) = self.tradable_side(direction)? else {
            // An untradable side is no swap at any size, so there is no limit to report.
            return Err(SimulationError::InvalidInput(NO_TRADABLE_SIDE.into(), None));
        };
        let aggregate = self.max_output(direction)?;

        // Metric's own accounting gives the exact input required to consume the whole book: the
        // last bin's cumulativeInputVolume. Prefer it over reconstructing the input from the
        // top-of-book price, which understates the limit by the cumulative price impact.
        if side.last_bin.cumulative_volume <= aggregate {
            return Ok((
                side.last_bin
                    .cumulative_input_volume
                    .clone(),
                side.last_bin.cumulative_volume.clone(),
            ));
        }

        // The aggregate inventory cuts the walkable depth short, so the aggregate is the binding
        // output cap. The input the walk takes to reach it is the sell limit, so a swap of that
        // input pays exactly the aggregate, and a swap after a partial fill sees what is left.
        let walk = depth_output_for_input(
            side.bins,
            side.side,
            side.decimals,
            &side.last_bin.cumulative_input_volume,
            &aggregate,
        )?;
        Ok((walk.consumed_input, aggregate))
    }
}

/// The refusal for a swap of `amount_in` that fills less than its whole input. `max_output` is
/// the merged swap's bound and `taken_out` the output earlier swaps on the direction already
/// paid, which comes off the bound so the message gives this swap's share.
fn depth_shortfall_message(
    amount_in: &BigUint,
    max_output: &BigUint,
    taken_out: &BigUint,
) -> String {
    format!(
        "Metric pool depth exhausted. Input {amount_in} cannot be fully filled; tradable depth \
         caps output at {}",
        saturating_sub(max_output, taken_out)
    )
}

/// The refusal for a bin whose cumulative volume or input falls below the bin before it.
fn non_monotonic_bin(bin: &MetricDepthBin) -> SimulationError {
    SimulationError::RecoverableError(format!(
        "Metric depth bin {} is below the bin before it: cumulative volume {}, cumulative input {}",
        bin.bin_idx, bin.cumulative_volume, bin.cumulative_input_volume
    ))
}

/// One side of the book that can be traded: Metric quotes its price and it has at least one
/// bin.
struct TradableSide<'a> {
    /// Top-of-book price of token0 in token1.
    price: f64,
    /// The side's bins, nearest the top of the book first.
    bins: &'a [MetricDepthBin],
    /// The deepest bin, which carries the side's cumulative totals.
    last_bin: &'a MetricDepthBin,
    /// Which side of the book an order walks, and the token0 and token1 decimals.
    side: BookSide,
    decimals: (u32, u32),
}

impl TradableSide<'_> {
    /// The output `amount_in` buys walking this side's bins, capped by `max_output`.
    fn quote(
        &self,
        amount_in: &BigUint,
        max_output: &BigUint,
    ) -> Result<DepthQuote, SimulationError> {
        let effective_max_output = self
            .last_bin
            .cumulative_volume
            .clone()
            .min(max_output.clone());
        let depth_fill = depth_output_for_input(
            self.bins,
            self.side,
            self.decimals,
            amount_in,
            &effective_max_output,
        )?;
        Ok(DepthQuote {
            amount_out: depth_fill.output,
            max_output: effective_max_output,
            exhausted: depth_fill.exhausted,
        })
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
    /// The input the walk took; the rest of the input it was given is unused.
    consumed_input: BigUint,
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
        sim::directions(
            &self.base_token,
            &self.quote_token,
            self.is_priced(MetricDirection::ZeroForOne),
            self.is_priced(MetricDirection::OneForZero),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for MetricState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = self.direction(&base.address, &quote.address)?;
        let price = match self.recorded_fill(direction)? {
            Some(fill) => self.price_after_fill(fill)?,
            None => {
                let side_price = |direction| -> Result<Option<f64>, SimulationError> {
                    Ok(self
                        .tradable_side(direction)?
                        .map(|side| side.price))
                };
                let bid = side_price(MetricDirection::ZeroForOne)?;
                let ask = side_price(MetricDirection::OneForZero)?;
                match (bid, ask) {
                    (Some(bid), Some(ask)) => (bid + ask) / 2.0,
                    (Some(bid), None) => bid,
                    (None, Some(ask)) => ask,
                    (None, None) => {
                        return Err(SimulationError::InvalidInput(NO_LIQUIDITY.into(), None))
                    }
                }
            }
        };
        Ok(match direction {
            MetricDirection::ZeroForOne => price,
            MetricDirection::OneForZero => 1.0 / price,
        })
    }

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
        let previous = self.recorded_fill(direction)?;
        let Some(side) = self.tradable_side(direction)? else {
            return Err(SimulationError::InvalidInput(
                format!("{NO_TRADABLE_SIDE}. Input {amount_in} cannot be filled"),
                None,
            ));
        };
        let max_output = self.max_output(direction)?;
        let merged_in = match previous {
            Some(previous) => &previous.amount_in + &amount_in,
            None => amount_in.clone(),
        };

        // The depth walk runs entirely in raw integer units on Metric's own per-bin accounting,
        // so the cap it reports when the depth is exhausted is exact (no f64 round-trip); see
        // `depth_output_for_input` for the intra-bin rounding.
        let quote = side.quote(&merged_in, &max_output)?;
        let (amount, gas) = match previous {
            Some(previous) => {
                (saturating_sub(&quote.amount_out, &previous.amount_out), BigUint::ZERO)
            }
            None => (quote.amount_out.clone(), BigUint::from(METRIC_SWAP_GAS)),
        };
        // A partly filled swap records the merged input it was given, not the input the walk
        // took. The filled direction drops once its buy side reaches the cap, so the unused input
        // is never read again.
        let new_state = Self {
            fill: Some(MetricFill {
                direction,
                amount_in: merged_in,
                amount_out: quote.amount_out.clone(),
            }),
            ..self.clone()
        };
        let result = GetAmountOutResult { amount, gas, new_state: Box::new(new_state) };
        if quote.exhausted {
            let taken_out = previous.map_or(BigUint::ZERO, |fill| fill.amount_out.clone());
            let message = depth_shortfall_message(&amount_in, &quote.max_output, &taken_out);
            return Err(SimulationError::InvalidInput(message, Some(result)));
        }
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction = self.direction(&sell_token, &buy_token)?;
        let fill = self.recorded_fill(direction)?;
        let (sell_limit, buy_limit) = self.unfilled_limits(direction)?;
        Ok(match fill {
            Some(fill) => (
                saturating_sub(&sell_limit, &fill.amount_in),
                saturating_sub(&buy_limit, &fill.amount_out),
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

/// Walks the depth bins and computes the output bought by `amount_in`, entirely in raw integer
/// units using Metric's own per-bin accounting.
///
/// Full bins cost exactly their `cumulativeInputVolume` difference — Metric's authoritative,
/// fee-adjusted accounting. The bin an order ends in is priced along its own straight-line curve
/// (see `fill::exact_fill_inside_bin`). A bin the inventory cap cuts short is charged pro-rata.
fn depth_output_for_input(
    bins: &[MetricDepthBin],
    side: BookSide,
    decimals: (u32, u32),
    amount_in: &BigUint,
    max_output: &BigUint,
) -> Result<DepthFill, SimulationError> {
    if amount_in.is_zero() || max_output.is_zero() {
        return Ok(DepthFill {
            output: BigUint::ZERO,
            consumed_input: BigUint::ZERO,
            exhausted: !amount_in.is_zero(),
        });
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
            return Err(non_monotonic_bin(bin));
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

        if volume_in_bin > output_capacity {
            // The aggregate inventory cap cuts the bin short: charge the fillable slice pro-rata,
            // rounding the input up so the quote never undercharges.
            let fillable_input = (&input_in_bin * &output_capacity + &volume_in_bin -
                BigUint::from(1u8)) /
                &volume_in_bin;
            if remaining_input >= fillable_input {
                output += &output_capacity;
                remaining_input -= &fillable_input;
                continue;
            }
            output += &output_capacity * &remaining_input / &fillable_input;
            remaining_input = BigUint::ZERO;
            break;
        }

        if remaining_input >= input_in_bin {
            output += &volume_in_bin;
            remaining_input -= &input_in_bin;
            continue;
        }

        // The input runs out inside this bin: the exact fill along the bin's own curve.
        let bin_curve = Bin { input: &input_in_bin, output: &volume_in_bin, price: &bin.price };
        output += exact_fill_inside_bin(bin_curve, side, &remaining_input, decimals)
            .unwrap_or_else(|| &volume_in_bin * &remaining_input / &input_in_bin);
        remaining_input = BigUint::ZERO;
        break;
    }

    Ok(DepthFill {
        output,
        consumed_input: amount_in - &remaining_input,
        exhausted: !remaining_input.is_zero(),
    })
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
            // One flat bin per side at the quoted prices: the bid buys 10 WETH for 30000 USDC
            // (3000), the ask sells 10 WETH for 30100 USDC (3010). Closing at the
            // quoted price keeps each bin flat, so the walk takes the proportional
            // share.
            depth: MetricDepth {
                bids: vec![MetricDepthBin {
                    price: big("55340232221128654848000"),
                    ..depth_bin("30000000000", "10000000000000000000")
                }],
                asks: vec![MetricDepthBin {
                    price: big("55524699661865750400000"),
                    ..depth_bin("10000000000000000000", "30100000000")
                }],
            },
        };
        MetricState { base_token: weth, quote_token: usdc, metadata, bid_ask, fill: None }
    }

    /// A book whose bid side Metric closed, as it answers one: the bid sentinel and no bid bins.
    /// The ask side is one bin selling 1 WETH for 3011 USDC.
    fn state_without_bid() -> MetricState {
        let mut state = state();
        state.bid_ask.bid_adj = BigUint::zero();
        state.bid_ask.depth.bids = vec![];
        state.bid_ask.depth.asks = vec![depth_bin("1000000000000000000", "3011000000")];
        state
    }

    /// A book whose ask side Metric closed: the ask sentinel and no ask bins. The bid side is one
    /// bin buying 1 WETH for 2999 USDC.
    fn state_without_ask() -> MetricState {
        let mut state = state();
        state.bid_ask.ask_adj = BigUint::from(u128::MAX);
        state.bid_ask.depth.asks = vec![];
        state.bid_ask.depth.bids = vec![depth_bin("2999000000", "1000000000000000000")];
        state
    }

    /// A side whose price Metric quotes but whose depth holds nothing is not traded at the top of
    /// the book: there is no size the quote is good for. Metric sends such a side either with no
    /// bins or with bins whose volumes are all zero.
    #[rstest::rstest]
    #[case::no_bins(vec![])]
    #[case::bins_without_volume(vec![depth_bin("0", "0"), depth_bin("0", "0")])]
    fn a_priced_side_without_depth_is_not_tradable(#[case] bids: Vec<MetricDepthBin>) {
        let mut state = state();
        state.bid_ask.depth.bids = bids;
        let ask = state
            .bid_ask
            .ask_price()
            .unwrap()
            .unwrap();

        assert_eq!(state.directions(), vec![(usdc().address, weth().address)]);
        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            ask
        );
        assert!(matches!(
            state.get_limits(weth().address, usdc().address),
            Err(SimulationError::InvalidInput(_, None))
        ));
        let Err(SimulationError::InvalidInput(msg, None)) =
            state.get_amount_out(big("100000000000000000"), &weth(), &usdc())
        else {
            panic!("selling WETH into a bid without depth must fail");
        };
        assert!(msg.contains("no tradable side"), "{msg}");
    }

    /// A side Metric closed by its sentinel price is not traded even when bins are left on it.
    #[test]
    fn a_side_with_bins_but_no_price_is_not_tradable() {
        let mut state = state();
        state.bid_ask.ask_adj = BigUint::from(u128::MAX);

        assert_eq!(state.directions(), vec![(weth().address, usdc().address)]);
        assert!(state
            .get_amount_out(big("3010000000"), &usdc(), &weth())
            .is_err());
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
            Err(SimulationError::InvalidInput(_, None))
        ));
        let Err(SimulationError::InvalidInput(msg, None)) =
            state.get_amount_out(big("100000000000000000"), &weth(), &usdc())
        else {
            panic!("selling WETH into a book without a bid must fail");
        };
        assert!(msg.contains("no tradable side"), "{msg}");
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
            Err(SimulationError::InvalidInput(_, None))
        ));
        let Err(SimulationError::InvalidInput(msg, None)) =
            state.get_amount_out(big("300000000"), &usdc(), &weth())
        else {
            panic!("buying WETH from a book without an ask must fail");
        };
        assert!(msg.contains("no tradable side"), "{msg}");
        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (big("1000000000000000000"), big("2999000000"))
        );
    }

    /// A zero ask price is no quote, so the ask side is not traded even with bins on it:
    /// `directions`, `get_limits` and `get_amount_out` all leave it out, and the bid side still
    /// trades.
    #[test]
    fn zero_ask_price_is_not_quoted() {
        let mut state = state();
        state.bid_ask.ask_adj = BigUint::zero();

        assert!(state.bid_ask.is_quotable());
        assert_eq!(state.directions(), vec![(weth().address, usdc().address)]);
        assert!(matches!(
            state.get_limits(usdc().address, weth().address),
            Err(SimulationError::InvalidInput(_, None))
        ));
        let Err(SimulationError::InvalidInput(msg, None)) =
            state.get_amount_out(big("300000000"), &usdc(), &weth())
        else {
            panic!("buying WETH from a zero ask price must fail");
        };
        assert!(msg.contains("no tradable side"), "{msg}");
    }

    #[test]
    fn book_without_quotes_has_no_spot_price() {
        let mut state = state();
        state.bid_ask.bid_adj = BigUint::zero();
        state.bid_ask.ask_adj = BigUint::from(u128::MAX);

        let Err(SimulationError::InvalidInput(msg, None)) = state.spot_price(&weth(), &usdc())
        else {
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
        // last-bin input does not apply and the limit is the input the depth walk takes for that
        // inventory.
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
        // Input the walk takes for 1000 USDC of the 3000 USDC bin: 1.0169 WETH × 1000 / 3000.
        assert_eq!(sell_limit, BigUint::from(338_983_050_847_457_627u128));
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
            // 2900 * 2^64. A bin's closing price is read only when the walk ends inside it, and
            // this one is not the average price of its own volume and input, so the tests that
            // end inside a bin built with this helper set their own price.
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
            BookSide::Bids,
            (18, 6),
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
            BookSide::Asks,
            (18, 6),
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
            BookSide::Bids,
            (18, 6),
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
            BookSide::Bids,
            (18, 6),
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
            BookSide::Bids,
            (18, 6),
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
            BookSide::Bids,
            (18, 6),
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
            BookSide::Bids,
            (18, 6),
            &BigUint::from(2_000_000_000_000_000_000u128),
            &BigUint::from(3_000_000_000u64),
        )
        .unwrap_err();

        assert!(matches!(err, SimulationError::RecoverableError(_)));
    }

    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_state_get_amount_out() {
        let Some(state) = live_base_weth_usdc_state().await else {
            return;
        };

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

    /// The WETH/USDC example of Metric's "Estimate a fill from depth" page: 5 WETH sold into two
    /// bid bins. The second bin is only partly filled, and the page's exact fill inside it gives
    /// 13535754492 raw USDC in total.
    #[test]
    fn get_amount_out_fills_the_bin_on_the_pool_curve() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![
            MetricDepthBin {
                bin_idx: -1,
                price: big("49937913286041055521250"),
                cumulative_volume: big("7430706558"),
                cumulative_input_volume: big("2744786401520641763"),
            },
            MetricDepthBin {
                bin_idx: -2,
                price: big("49932918995283375647711"),
                cumulative_volume: big("21731301391"),
                cumulative_input_volume: big("8027598362225637937"),
            },
        ];

        let result = state
            .get_amount_out(big("5000000000000000000"), &state.base_token, &state.quote_token)
            .unwrap();

        assert_eq!(result.amount, big("13535754492"));
    }

    fn sell_weth(state: &MetricState, amount: u128) -> Result<GetAmountOutResult, SimulationError> {
        state.get_amount_out(BigUint::from(amount), &state.base_token, &state.quote_token)
    }

    fn buy_weth(state: &MetricState, amount: u128) -> Result<GetAmountOutResult, SimulationError> {
        state.get_amount_out(BigUint::from(amount), &state.quote_token, &state.base_token)
    }

    /// The partial fill a refused swap still reports.
    fn partial(result: Result<GetAmountOutResult, SimulationError>) -> GetAmountOutResult {
        match result {
            Err(SimulationError::InvalidInput(_, Some(partial))) => partial,
            other => panic!("expected a partial fill, got {other:?}"),
        }
    }

    fn metric_state(result: &GetAmountOutResult) -> &MetricState {
        result
            .new_state
            .as_any()
            .downcast_ref::<MetricState>()
            .unwrap()
    }

    /// Bin 1 buys 1 WETH for 3000 USDC, bin 2 another 1 WETH for 2000 USDC. Each bin closes at
    /// its own average price, so both are flat.
    fn two_bin_bid_state() -> MetricState {
        let mut state = state();
        state.bid_ask.depth.bids = vec![
            MetricDepthBin {
                price: big("55340232221128654848000"),
                ..depth_bin("3000000000", "1000000000000000000")
            },
            MetricDepthBin {
                // 2000 * 2^64
                price: big("36893488147419103232000"),
                ..depth_bin("5000000000", "2000000000000000000")
            },
        ];
        state
    }

    /// The first quotable Base WETH/USDC pool the live API serves with liquid quote tokens.
    async fn live_base_weth_usdc_state() -> Option<MetricState> {
        use crate::pamm::protocols::metric::models::PaginatedMetadataResponse;

        let api_key = std::env::var("METRIC_API_KEY").expect("METRIC_API_KEY not set");

        // Base: the only supported chain with pools published on the live API so far.
        let weth = base_weth();
        let usdc = base_usdc();
        let base_url = "https://api.metric.xyz";

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
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
            let pool_address = alloy::primitives::Address::try_from(pool.pool_address.as_ref())
                .expect("Metric returned a pool address that is not 20 bytes");
            let checksummed = pool_address.to_checksum(None);
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
            return None;
        };

        let state =
            MetricState { base_token: weth, quote_token: usdc, metadata, bid_ask, fill: None };
        assert!(state.bid_ask.is_quotable());
        Some(state)
    }

    /// A continuation that runs past the depth names the input of that swap, not the merged input.
    #[test]
    fn continuation_refusal_names_the_swaps_own_input() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![depth_bin("3000000000", "1000000000000000000")];
        let first = sell_weth(&state, 500_000_000_000_000_000).unwrap();

        let Err(SimulationError::InvalidInput(message, _)) =
            sell_weth(metric_state(&first), 700_000_000_000_000_000)
        else {
            panic!("a continuation past the depth must be refused");
        };
        assert!(message.contains("Input 700000000000000000 cannot be fully filled"), "{message}");
    }

    /// Once the one bin is used up, the filled direction is no longer advertised.
    #[test]
    fn directions_drop_the_filled_direction_once_its_depth_is_used_up() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![depth_bin("3000000000", "1000000000000000000")];

        let half = sell_weth(&state, 500_000_000_000_000_000).unwrap();
        let used_up = sell_weth(&state, 1_000_000_000_000_000_000).unwrap();

        assert_eq!(metric_state(&half).directions(), vec![(weth().address, usdc().address)]);
        assert!(metric_state(&used_up)
            .directions()
            .is_empty());
    }

    #[test]
    fn fill_refuses_the_other_direction() {
        let state = two_bin_bid_state();

        let first = sell_weth(&state, 500_000_000_000_000_000).unwrap();
        let after = metric_state(&first);

        assert!(buy_weth(&state, 1_000_000_000).is_ok());
        let Err(SimulationError::InvalidInput(message, None)) = buy_weth(after, 1_000_000_000)
        else {
            panic!("the other direction is refused after a fill");
        };
        assert_eq!(message, "Metric pool already quoted in this route");
        assert!(matches!(
            after.get_limits(usdc().address, weth().address),
            Err(SimulationError::InvalidInput(_, None))
        ));
        assert_eq!(after.directions(), vec![(weth().address, usdc().address)]);
    }

    #[test]
    fn fill_survives_serialization() {
        let first = sell_weth(&two_bin_bid_state(), 500_000_000_000_000_000).unwrap();
        let after = metric_state(&first);

        let json = serde_json::to_string(after).unwrap();
        let restored: MetricState = serde_json::from_str(&json).unwrap();

        assert!(ProtocolSim::eq(&restored, after));
        assert_eq!(
            restored
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            after
                .get_limits(weth().address, usdc().address)
                .unwrap()
        );
        assert!(!ProtocolSim::eq(&restored, &two_bin_bid_state()));
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

    /// Two ask-side swaps against one ask bin of 2 WETH for 6000 USDC: the merged buy of 3000 USDC
    /// buys 1 WETH, so the second swap pays the second half.
    #[test]
    fn ask_side_fills_in_sequence_sum_to_one_fill() {
        let mut state = state();
        state.bid_ask.depth.asks = vec![depth_bin("2000000000000000000", "6000000000")];

        let first = buy_weth(&state, 1_500_000_000).unwrap();
        let second = buy_weth(metric_state(&first), 1_500_000_000).unwrap();
        let whole = buy_weth(&state, 3_000_000_000).unwrap();

        assert_eq!(whole.amount, BigUint::from(1_000_000_000_000_000_000u128));
        assert_eq!(first.amount, BigUint::from(500_000_000_000_000_000u128));
        assert_eq!(&first.amount + &second.amount, whole.amount);
        assert_eq!(second.gas, BigUint::ZERO);
    }

    #[test]
    fn bid_fills_in_sequence_sum_to_one_fill() {
        let mut state = state();
        // Bin 1 buys 1 WETH for 3000 USDC and closes at 3000, so it is flat; bin 2 buys another
        // 1.1 WETH for 3000 USDC and closes at its average, 3000 / 1.1.
        state.bid_ask.depth.bids = vec![
            MetricDepthBin {
                price: big("55340232221128654848000"),
                ..depth_bin("3000000000", "1000000000000000000")
            },
            MetricDepthBin {
                // 30000 * 2^64 / 11, rounded down
                price: big("50309302019207868043636"),
                ..depth_bin("6000000000", "2100000000000000000")
            },
        ];

        // The first fill stops inside bin 1, the second one inside bin 2.
        let first = sell_weth(&state, 500_000_000_000_000_000).unwrap();
        let second = sell_weth(metric_state(&first), 1_050_000_000_000_000_000).unwrap();
        let whole = sell_weth(&state, 1_550_000_000_000_000_000).unwrap();

        assert_eq!(first.amount, BigUint::from(1_500_000_000u64));
        assert_eq!(&first.amount + &second.amount, whole.amount);
        assert_eq!(first.gas, BigUint::from(METRIC_SWAP_GAS));
        assert_eq!(second.gas, BigUint::ZERO);
    }

    /// On a flat bin a continuation is priced at the quoted price, and pays the merged flat quote
    /// less what the first swap paid.
    #[test]
    fn flat_continuation_is_priced_at_the_top_of_book() {
        let first = sell_weth(&state(), 5_000_000_000_000_000_000).unwrap();
        let after = metric_state(&first);

        let price = after
            .spot_price(&weth(), &usdc())
            .unwrap();
        assert!((price - 3000.0).abs() < 1e-9, "{price}");
        let next = after
            .get_amount_out(big("1000000000000000000"), &weth(), &usdc())
            .unwrap();
        assert_eq!(next.amount, BigUint::from(3_000_000_000u64));
    }

    #[test]
    fn flat_fill_counts_towards_the_inventory() {
        // One flat bin at 3000 USDC per WETH, with 30000 USDC of inventory.
        let state = state();

        let first = sell_weth(&state, 5_000_000_000_000_000_000).unwrap();
        let second = partial(sell_weth(metric_state(&first), 6_000_000_000_000_000_000));

        assert_eq!(first.amount, BigUint::from(15_000_000_000u64));
        assert_eq!(second.amount, BigUint::from(15_000_000_000u64));
        assert_eq!(
            sell_weth(&state, 6_000_000_000_000_000_000)
                .unwrap()
                .amount,
            BigUint::from(18_000_000_000u64)
        );
    }

    #[test]
    fn get_limits_is_the_flat_bin_when_inventory_covers_it() {
        let state = state();

        let (sell_limit, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();

        // The inventory (30000 USDC) covers the one flat bin, so the limit is that bin's total.
        assert_eq!(buy_limit, BigUint::from(30_000_000_000u64));
        assert_eq!(sell_limit, BigUint::from(10_000_000_000_000_000_000u128));
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

    /// Bids of 1 WETH for 3000 USDC and 1 WETH for 2000 USDC, with 4000 USDC of inventory, so the
    /// inventory cuts the book short. The limit's input must pay its output, and a swap after a
    /// partial fill must still see the input it has left.
    #[test]
    fn limits_agree_with_the_fills_on_a_truncated_book() {
        let mut state = state();
        state.bid_ask.total_token1_available = Some(big("4000000000"));
        state.bid_ask.depth.bids = vec![
            MetricDepthBin {
                bin_idx: 0,
                // 3000 * 2^64
                price: big("55340232221128654848000"),
                cumulative_volume: big("3000000000"),
                cumulative_input_volume: big("1000000000000000000"),
            },
            MetricDepthBin {
                bin_idx: 1,
                // 2000 * 2^64
                price: big("36893488147419103232000"),
                cumulative_volume: big("5000000000"),
                cumulative_input_volume: big("2000000000000000000"),
            },
        ];
        let (sell_limit, buy_limit) = state
            .get_limits(state.base_token.address.clone(), state.quote_token.address.clone())
            .unwrap();
        assert_eq!(buy_limit, big("4000000000"));
        let whole = state
            .get_amount_out(sell_limit.clone(), &weth(), &usdc())
            .unwrap();
        assert_eq!(whole.amount, buy_limit);
        assert!(state
            .get_amount_out(sell_limit + BigUint::from(1u8), &weth(), &usdc())
            .is_err());

        let filled = state
            .get_amount_out(big("1400000000000000000"), &weth(), &usdc())
            .unwrap();
        let (rest_sell, rest_buy) = filled
            .new_state
            .get_limits(weth().address, usdc().address)
            .unwrap();
        assert_eq!(rest_buy, big("200000000"));
        let rest = filled
            .new_state
            .get_amount_out(rest_sell, &weth(), &usdc())
            .unwrap();
        assert_eq!(rest.amount, rest_buy);
    }

    /// After a buy of WETH on the ask side, the other direction's spot price is the inverse of
    /// the bin's average price: 30100 USDC for 10 WETH, so 3010 USDC per WETH.
    #[test]
    fn spot_price_after_an_ask_side_fill_is_the_bins_average_price() {
        let bought = buy_weth(&state(), 3_010_000_000).unwrap();

        let spot = metric_state(&bought)
            .spot_price(&usdc(), &weth())
            .unwrap();

        assert!((spot - 1.0 / 3010.0).abs() < 1e-12, "{spot}");
    }

    /// The second bin's cumulative volume falls below the first's, so the price after a fill inside
    /// it is refused, as the depth walk refuses the same bins.
    #[test]
    fn spot_price_after_a_fill_refuses_non_monotonic_bins() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![
            depth_bin("3000000000", "1000000000000000000"),
            depth_bin("2000000000", "2000000000000000000"),
        ];
        state.fill = Some(MetricFill {
            direction: MetricDirection::ZeroForOne,
            amount_in: big("1500000000000000000"),
            amount_out: big("1500000000"),
        });

        let Err(SimulationError::RecoverableError(msg)) = state.spot_price(&weth(), &usdc()) else {
            panic!("a bin below the bin before it must be refused");
        };
        assert!(msg.contains("below the bin before it"), "{msg}");
    }

    /// A bin whose volume is zero while its input grows is refused, as the depth walk refuses it.
    #[test]
    fn spot_price_after_a_fill_refuses_a_bin_with_input_and_no_volume() {
        let mut state = state();
        state.bid_ask.depth.bids = vec![
            depth_bin("0", "1000000000000000000"),
            depth_bin("3000000000", "2000000000000000000"),
        ];
        state.fill = Some(MetricFill {
            direction: MetricDirection::ZeroForOne,
            amount_in: big("500000000000000000"),
            amount_out: big("1500000000"),
        });

        let Err(SimulationError::RecoverableError(msg)) = state.spot_price(&weth(), &usdc()) else {
            panic!("a bin with input and no volume must be refused");
        };
        assert_eq!(msg, "Metric depth bin has inconsistent volume and input");
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
        let Err(SimulationError::InvalidInput(message, None)) =
            metric_state(&into_bin_2).spot_price(&usdc(), &weth())
        else {
            panic!("the other direction has no spot price after a fill");
        };
        assert_eq!(message, "Metric pool already quoted in this route");
        assert!(matches!(price(&all), Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn spot_price_refuses_the_other_direction_after_a_fill() {
        let first = sell_weth(&two_bin_bid_state(), 500_000_000_000_000_000).unwrap();
        let after = metric_state(&first);

        assert!(after
            .spot_price(&weth(), &usdc())
            .is_ok());
        let Err(SimulationError::InvalidInput(message, None)) = after.spot_price(&usdc(), &weth())
        else {
            panic!("the other direction has no spot price after a fill");
        };
        assert_eq!(message, "Metric pool already quoted in this route");
    }

    /// Without a fill the state carries no `fill` key, and a state without one restores unfilled.
    #[test]
    fn unfilled_state_serializes_without_a_fill_key() {
        let json = serde_json::to_string(&state()).unwrap();
        assert!(!json.contains("\"fill\""), "{json}");

        let restored: MetricState = serde_json::from_str(&json).unwrap();
        assert!(restored.fill.is_none());
    }

    /// Two swaps of one live snapshot, sold one after the other, pay what one swap of their total
    /// pays, and the second one costs no gas.
    #[tokio::test]
    #[ignore = "hits Metric's public API; requires METRIC_API_KEY"]
    async fn live_metric_api_continuation_sums_to_one_swap() {
        let Some(state) = live_base_weth_usdc_state().await else {
            return;
        };

        let half = BigUint::from(500_000_000_000_000u64);
        let whole = &half + &half;
        let first = state
            .get_amount_out(half.clone(), &state.base_token, &state.quote_token)
            .unwrap();
        let after_first = first
            .new_state
            .as_any()
            .downcast_ref::<MetricState>()
            .unwrap();
        let second = after_first
            .get_amount_out(half, &state.base_token, &state.quote_token)
            .unwrap();
        let one = state
            .get_amount_out(whole, &state.base_token, &state.quote_token)
            .unwrap();

        assert_eq!(&first.amount + &second.amount, one.amount);
        assert_eq!(second.gas, BigUint::ZERO);
    }
}
