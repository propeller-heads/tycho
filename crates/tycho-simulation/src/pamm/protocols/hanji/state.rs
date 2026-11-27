use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, Address, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
    },
    Bytes,
};

use crate::{
    book::{
        component::PairState,
        sim::{self, SwapDirection},
    },
    evm::protocol::{
        safe_math::{safe_add_u256, safe_mul_u256, safe_sub_u256},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    },
};

/// The denominator of Hanji's commission rates: `getConfig()` reports them per 1e18 of the
/// executed value.
const RATE_DENOMINATOR: u64 = 1_000_000_000_000_000_000;

/// Gas of the proxy orderbook's swap for a contract caller when it fills from the first price
/// level, per market, and what each further level filled adds. Measured with `debug_traceCall`
/// of both swap methods sent through a forwarding contract (the proxy call's own gas, the `gas` of
/// the recording's "contract" cases). In the recording kept in `testdata/` (Base block 61,693,481)
/// a first-level fill cost at most 1,046,021 on ETH/USDC, 1,150,851 on cbBTC/USDC and 1,381,075
/// on cbBTC/ETH, and a fill through more levels at most 266,798 more per further level, averaged
/// over the levels it touched. The constants were set over an earlier recording (block
/// 61,672,816, not kept) whose fills cost up to 7% more, so they sit 5% (ETH/USDC) to 14% over the
/// kept one's first levels and 12% over its further levels. Most of it is the quoter placing and
/// removing its virtual orders around the fill and re-reading the pools it prices from. A caller
/// the proxy serves from its FastQuoter (a whitelisted taker, as Tycho's router is since about
/// block 62,515,500) pays within 2% of that, measured on the same basis, so the model does not
/// tell the two apart. A market not listed here is charged the largest first-level cost.
/// Re-measure when the recorded gas stops fitting (`the_gas_covers_every_recorded_contract_fill`).
/// The router's own transfers and approval are added on top by tycho-execution's gas estimator.
const FIRST_LEVEL_GAS: [(Address, u64); 3] = [
    (address!("6a6951db3bb99f76a8ccb9f8a8399b613666298b"), 1_100_000),
    (address!("15c8cb4e336b1e478d155d813830129fb225eee3"), 1_290_000),
    (address!("f455b3c0756908c6a2f44a7f7390053cfe354eea"), 1_570_000),
];
/// The first-level gas of a market [`FIRST_LEVEL_GAS`] does not list: the largest it lists.
const UNMEASURED_MARKET_FIRST_LEVEL_GAS: u64 = {
    let mut largest = 0;
    let mut i = 0;
    while i < FIRST_LEVEL_GAS.len() {
        if FIRST_LEVEL_GAS[i].1 > largest {
            largest = FIRST_LEVEL_GAS[i].1;
        }
        i += 1;
    }
    largest
};
/// See [`FIRST_LEVEL_GAS`].
const GAS_PER_FURTHER_LEVEL: u64 = 300_000;

/// One price level of a Hanji book, in the market's scaled units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Level {
    /// Token Y units per share.
    pub price: U256,
    /// Shares of token X resting at the price.
    pub shares: U256,
}

/// One Hanji market as one of its quoters reported it at one block: both sides of the book the
/// proxy fills the feed's taker against, the market's scaling and its taker rate.
///
/// Prices exact-input swaps wei for wei as the proxy orderbook fills them at that block (see the
/// module docs for the rules), including the input left unused below one share or one share's
/// cost, which the quote does not count as spent and which stays with the caller. Every state,
/// deserialized ones included, passes the checks of [`HanjiState::validated`], and its arithmetic
/// is checked, so a book no contract would report fails its quotes instead of wrapping.
#[derive(Clone, derive_more::Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedHanjiState")]
pub struct HanjiState {
    /// The proxy orderbook the market is quoted and traded through.
    pub(super) orderbook: Address,
    /// Token X, or the native token where X is.
    pub(super) base_token: Token,
    /// Token Y, or the native token where Y is.
    pub(super) quote_token: Token,
    /// Atoms of token X per share.
    pub(super) scaling_x: U256,
    /// Atoms of token Y per unit of price and value.
    pub(super) scaling_y: U256,
    /// The taker's fee per 1e18 of the executed value: `totalAggressiveCommissionRate` plus
    /// `passiveOrderPayoutRate`.
    pub(super) fee_rate: u64,
    /// Best (lowest price) first.
    #[debug("{} levels", asks.len())]
    pub(super) asks: Vec<Level>,
    /// Best (highest price) first.
    #[debug("{} levels", bids.len())]
    pub(super) bids: Vec<Level>,
}

/// A [`HanjiState`] as serialized, before [`HanjiState::validated`] checks it.
#[derive(Deserialize)]
struct UncheckedHanjiState {
    orderbook: Address,
    base_token: Token,
    quote_token: Token,
    scaling_x: U256,
    scaling_y: U256,
    fee_rate: u64,
    asks: Vec<Level>,
    bids: Vec<Level>,
}

impl TryFrom<UncheckedHanjiState> for HanjiState {
    type Error = SimulationError;

    fn try_from(state: UncheckedHanjiState) -> Result<Self, SimulationError> {
        let UncheckedHanjiState {
            orderbook,
            base_token,
            quote_token,
            scaling_x,
            scaling_y,
            fee_rate,
            asks,
            bids,
        } = state;
        HanjiState {
            orderbook,
            base_token,
            quote_token,
            scaling_x,
            scaling_y,
            fee_rate,
            asks,
            bids,
        }
        .validated()
    }
}

/// What filling one side of the book against an input amounts to.
struct Fill {
    shares: U256,
    value: U256,
    /// Levels the fill touched, the last one possibly in part.
    levels: usize,
}

fn invalid(message: impl Into<String>) -> SimulationError {
    SimulationError::InvalidInput(message.into(), None)
}

/// The refusal of a swap in `direction` when the side it walks holds no level.
fn no_levels(direction: SwapDirection) -> SimulationError {
    invalid(match direction {
        SwapDirection::BaseToQuote => "the book has no bids",
        SwapDirection::QuoteToBase => "the book has no asks",
    })
}

impl HanjiState {
    /// The state, or an error for a market or book the contracts would never report: a zero
    /// scaling factor, a fee of the whole value or more, an empty level, or a side out of price
    /// order.
    pub(super) fn validated(self) -> Result<Self, SimulationError> {
        if self.scaling_x.is_zero() || self.scaling_y.is_zero() {
            return Err(invalid("market has a zero scaling factor"));
        }
        if self.fee_rate >= RATE_DENOMINATOR {
            return Err(invalid(format!("fee rate {} takes the whole value", self.fee_rate)));
        }
        for (name, side) in [("ask", &self.asks), ("bid", &self.bids)] {
            if side
                .iter()
                .any(|level| level.price.is_zero() || level.shares.is_zero())
            {
                return Err(invalid(format!("{name} side holds an empty level")));
            }
        }
        if self
            .asks
            .windows(2)
            .any(|pair| pair[1].price < pair[0].price)
        {
            return Err(invalid("ask side is not ordered lowest price first"));
        }
        if self
            .bids
            .windows(2)
            .any(|pair| pair[1].price > pair[0].price)
        {
            return Err(invalid("bid side is not ordered highest price first"));
        }
        Ok(self)
    }

    fn side(&self, direction: SwapDirection) -> &[Level] {
        match direction {
            SwapDirection::BaseToQuote => &self.bids,
            SwapDirection::QuoteToBase => &self.asks,
        }
    }

    /// The fee on `value`, which the proxy rounds up.
    fn fee_on(&self, value: U256) -> Result<U256, SimulationError> {
        Ok(safe_mul_u256(value, U256::from(self.fee_rate))?.div_ceil(U256::from(RATE_DENOMINATOR)))
    }

    /// `value` plus its fee: what buying shares worth `value` costs.
    fn cost(&self, value: U256) -> Result<U256, SimulationError> {
        safe_add_u256(value, self.fee_on(value)?)
    }

    /// The most value a buy with `target` can pay for, fee included: the largest `v` with
    /// `cost(v) <= target`, which is `floor(target * D / (D + rate))` exactly. `t - v` is an
    /// integer, so `v + ceil(v * rate / D) <= t` holds just when `v * rate / D <= t - v`, that is
    /// when `v * (D + rate) <= t * D`.
    fn affordable_value(&self, target: U256) -> Result<U256, SimulationError> {
        let denominator = U256::from(RATE_DENOMINATOR);
        Ok(safe_mul_u256(target, denominator)? / (denominator + U256::from(self.fee_rate)))
    }

    /// Sells `shares` into the bids, best first; the bids must hold at least that many.
    fn sell(&self, shares: U256) -> Result<Fill, SimulationError> {
        let mut fill = Fill { shares: U256::ZERO, value: U256::ZERO, levels: 0 };
        let mut left = shares;
        for level in &self.bids {
            if left.is_zero() {
                break;
            }
            let take = left.min(level.shares);
            fill.shares += take;
            fill.value = safe_add_u256(fill.value, safe_mul_u256(take, level.price)?)?;
            fill.levels += 1;
            left -= take;
        }
        Ok(fill)
    }

    /// Buys from the asks, best first, the most shares whose value and fee fit in `target`, which
    /// must not exceed what the whole side costs.
    fn buy(&self, target: U256) -> Result<Fill, SimulationError> {
        let budget = self.affordable_value(target)?;
        let mut fill = Fill { shares: U256::ZERO, value: U256::ZERO, levels: 0 };
        for level in &self.asks {
            let take = ((budget - fill.value) / level.price).min(level.shares);
            if take.is_zero() {
                break;
            }
            fill.shares += take;
            fill.value = safe_add_u256(fill.value, safe_mul_u256(take, level.price)?)?;
            fill.levels += 1;
        }
        Ok(fill)
    }

    /// What selling for `value` pays out: the value less its fee, in atoms of token Y.
    fn sale_proceeds(&self, value: U256) -> Result<U256, SimulationError> {
        safe_mul_u256(safe_sub_u256(value, self.fee_on(value)?)?, self.scaling_y)
    }

    /// The proxy's gas for a fill that touches `levels` levels.
    fn gas(&self, levels: usize) -> u64 {
        let first_level = FIRST_LEVEL_GAS
            .iter()
            .find(|(orderbook, _)| *orderbook == self.orderbook)
            .map_or(UNMEASURED_MARKET_FIRST_LEVEL_GAS, |(_, gas)| *gas);
        let further = u64::try_from(levels.saturating_sub(1)).unwrap_or(u64::MAX);
        first_level.saturating_add(GAS_PER_FURTHER_LEVEL.saturating_mul(further))
    }

    /// The input that fills the whole side a swap in `direction` walks, and what it pays out;
    /// zero both ways for a side with no levels or whose whole fill pays out nothing. Every input
    /// up to it of at least one share fills, and none past it.
    fn limits(&self, direction: SwapDirection) -> Result<(U256, U256), SimulationError> {
        let mut value = U256::ZERO;
        let mut shares = U256::ZERO;
        for level in self.side(direction) {
            value = safe_add_u256(value, safe_mul_u256(level.price, level.shares)?)?;
            shares = safe_add_u256(shares, level.shares)?;
        }
        let (input, output) = match direction {
            SwapDirection::BaseToQuote => {
                (safe_mul_u256(shares, self.scaling_x)?, self.sale_proceeds(value)?)
            }
            SwapDirection::QuoteToBase => (
                safe_mul_u256(self.cost(value)?, self.scaling_y)?,
                safe_mul_u256(shares, self.scaling_x)?,
            ),
        };
        if output.is_zero() {
            return Ok((U256::ZERO, U256::ZERO));
        }
        Ok((input, output))
    }
}

impl PairState for HanjiState {
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
            !self.bids.is_empty(),
            !self.asks.is_empty(),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for HanjiState {
    fn fee(&self) -> f64 {
        self.fee_rate as f64 / RATE_DENOMINATOR as f64
    }

    /// The best level's rate of selling `base` for `quote`, fee included.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &base.address,
            &quote.address,
        )?;
        let best = self
            .side(direction)
            .first()
            .ok_or_else(|| no_levels(direction))?;
        // Quote atoms per base atom at the best level, before the fee.
        let atomic =
            u256_to_f64(best.price)? * u256_to_f64(self.scaling_y)? / u256_to_f64(self.scaling_x)?;
        let rate = ProtocolSim::fee(self);
        let decimals = 10f64.powf(f64::from(base.decimals) - f64::from(quote.decimals));
        Ok(match direction {
            SwapDirection::BaseToQuote => atomic * decimals * (1.0 - rate),
            SwapDirection::QuoteToBase => decimals / (atomic * (1.0 + rate)),
        })
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        sim::require_positive_amount(&amount_in)?;
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &token_in.address,
            &token_out.address,
        )?;
        if amount_in.bits() > 256 {
            return Err(invalid("amount exceeds uint256"));
        }
        if self.side(direction).is_empty() {
            return Err(no_levels(direction));
        }
        let input = biguint_to_u256(&amount_in);
        let (limit, _) = self.limits(direction)?;
        if input > limit {
            return Err(invalid(format!("input {input} exceeds the book's {limit}")));
        }
        let (fill, output) = match direction {
            SwapDirection::BaseToQuote => {
                let shares = input / self.scaling_x;
                if shares.is_zero() {
                    return Err(invalid("input is less than one share of the base token"));
                }
                let fill = self.sell(shares)?;
                let output = self.sale_proceeds(fill.value)?;
                (fill, output)
            }
            SwapDirection::QuoteToBase => {
                let fill = self.buy(input / self.scaling_y)?;
                let output = safe_mul_u256(fill.shares, self.scaling_x)?;
                (fill, output)
            }
        };
        if output.is_zero() {
            return Err(invalid("input buys less than one unit of output"));
        }
        let gas = self.gas(fill.levels);
        // The quoter places a new, worse book after every fill, which no state here knows, so the
        // side the swap walked is left empty: a second swap the same way is refused, not
        // over-quoted from the levels the first left (see the module docs).
        let mut next = self.clone();
        match direction {
            SwapDirection::BaseToQuote => next.bids.clear(),
            SwapDirection::QuoteToBase => next.asks.clear(),
        }
        Ok(GetAmountOutResult::new(u256_to_biguint(output), BigUint::from(gas), Box::new(next)))
    }

    /// The input that fills the whole side the swap walks and what it pays out. Zero both ways
    /// for a side with no levels.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction =
            SwapDirection::require(&self.base_token, &self.quote_token, &sell_token, &buy_token)?;
        let (input, output) = self.limits(direction)?;
        Ok((u256_to_biguint(input), u256_to_biguint(output)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError(
            "Hanji state is read whole every poll and does not take deltas".into(),
        ))
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
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
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;
    use crate::pamm::protocols::hanji::test_support::{
        cbbtc, eth, fixture, state, usdc, Caller, CBBTC_ETH, CBBTC_USDC, ETH_USDC,
    };

    /// The state's tokens a recorded case sells and buys.
    fn tokens(state: &HanjiState, sell_base: bool) -> (Token, Token) {
        if sell_base {
            (state.base_token.clone(), state.quote_token.clone())
        } else {
            (state.quote_token.clone(), state.base_token.clone())
        }
    }

    fn quote(
        state: &HanjiState,
        amount_in: U256,
        sell_base: bool,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let (token_in, token_out) = tokens(state, sell_base);
        state.get_amount_out(u256_to_biguint(amount_in), &token_in, &token_out)
    }

    fn limits(state: &HanjiState, sell_base: bool) -> (U256, U256) {
        let (token_in, token_out) = tokens(state, sell_base);
        let (input, output) = state
            .get_limits(token_in.address, token_out.address)
            .unwrap();
        (biguint_to_u256(&input), biguint_to_u256(&output))
    }

    /// Every recorded fill on the three markets, both directions, for both kinds of caller,
    /// priced wei for wei from the book that caller gets against what the caller's balance
    /// gained; the recorded sizes past a side's depth, which the proxy fills in part, are refused,
    /// and what the proxy paid for them is the limit's output.
    #[test]
    fn quotes_match_the_proxy_wei_for_wei() {
        let fixture = fixture();
        let (mut priced, mut refused) = (0, 0);
        for orderbook in [ETH_USDC, CBBTC_USDC, CBBTC_ETH] {
            for case in &fixture.market(orderbook).cases {
                let state = state(orderbook, case.caller);
                let amount_in = case.amount_in;
                let (limit, limit_out) = limits(&state, case.sell_base);
                let result = quote(&state, amount_in, case.sell_base);
                if amount_in > limit {
                    // The proxy fills such an input only up to its book, which pays out exactly
                    // what the limit says the whole side does.
                    assert!(result.is_err(), "{case:?} is past the book");
                    assert_eq!(limit_out, case.amount_out, "{case:?}");
                    refused += 1;
                    continue;
                }
                assert_eq!(biguint_to_u256(&result.unwrap().amount), case.amount_out, "{case:?}");
                priced += 1;
            }
        }
        assert_eq!((priced, refused), (60, 12));
    }

    /// The gas model covers every recorded fill of a contract caller within the book, the path
    /// Tycho's router takes, and overstates none by more than a quarter: the constants carry the
    /// largest figures of two recordings 7% apart.
    #[test]
    fn the_gas_covers_every_recorded_contract_fill() {
        let fixture = fixture();
        let mut checked = 0;
        for orderbook in [ETH_USDC, CBBTC_USDC, CBBTC_ETH] {
            let state = state(orderbook, Caller::Contract);
            for case in fixture
                .market(orderbook)
                .cases
                .iter()
                .filter(|case| case.caller == Caller::Contract)
            {
                let (limit, _) = limits(&state, case.sell_base);
                if case.amount_in > limit {
                    continue;
                }
                let result = quote(&state, case.amount_in, case.sell_base).unwrap();
                let gas = u64::try_from(result.gas).unwrap();
                assert!(gas >= case.gas, "{case:?}: modelled {gas}");
                assert!(gas * 4 <= case.gas * 5, "{case:?}: modelled {gas}");
                checked += 1;
            }
        }
        assert_eq!(checked, 30, "the recording's contract fills within the book");
    }

    /// The limit is exactly what get_amount_out fills: the whole side at the limit, and an
    /// error one wei past it.
    #[rstest]
    fn the_limit_is_the_largest_input_that_fills(
        #[values(ETH_USDC, CBBTC_USDC, CBBTC_ETH)] orderbook: Address,
        #[values(true, false)] sell_base: bool,
    ) {
        let state = state(orderbook, Caller::Contract);
        let (input, output) = limits(&state, sell_base);

        let at_limit = quote(&state, input, sell_base).unwrap();
        let past_limit = quote(&state, input + U256::from(1), sell_base);

        assert_eq!(biguint_to_u256(&at_limit.amount), output);
        let next = at_limit
            .new_state
            .as_any()
            .downcast_ref::<HanjiState>()
            .unwrap();
        assert!(next
            .side(direction(sell_base))
            .is_empty());
        assert!(matches!(past_limit, Err(SimulationError::InvalidInput(..))));
    }

    fn direction(sell_base: bool) -> SwapDirection {
        if sell_base {
            SwapDirection::BaseToQuote
        } else {
            SwapDirection::QuoteToBase
        }
    }

    #[test]
    fn input_below_one_share_is_refused() {
        let state = state(ETH_USDC, Caller::Contract);

        let selling = quote(&state, state.scaling_x - U256::from(1), true);
        let buying = quote(&state, U256::from(1000), false);

        assert!(matches!(selling, Err(SimulationError::InvalidInput(..))));
        assert!(matches!(buying, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn zero_and_foreign_tokens_are_refused() {
        let state = state(CBBTC_USDC, Caller::Contract);

        let zero = state.get_amount_out(BigUint::ZERO, &cbbtc(), &usdc());
        let foreign = state.get_amount_out(BigUint::from(1_000_000u64), &eth(), &usdc());

        assert!(matches!(zero, Err(SimulationError::InvalidInput(..))));
        assert!(matches!(foreign, Err(SimulationError::InvalidInput(..))));
    }

    /// A swap that fills part of the bids leaves none in the next state, which then refuses a
    /// second sell, and keeps the asks.
    #[test]
    fn a_swap_empties_the_side_it_walked() {
        let state = state(CBBTC_USDC, Caller::Contract);
        let amount = U256::from(10) * state.scaling_x;

        let result = quote(&state, amount, true).unwrap();
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<HanjiState>()
            .unwrap();

        assert!(next.bids.is_empty());
        assert_eq!(next.asks, state.asks);
        assert!(matches!(quote(next, amount, true), Err(SimulationError::InvalidInput(..))));
    }

    /// The fee on a value rounds up, as the proxy charges it: 1 bp of 12,345 is 1.2345, charged
    /// as 2.
    #[test]
    fn the_fee_rounds_up() {
        let state = state(CBBTC_USDC, Caller::Contract);

        assert_eq!(state.fee_rate, 100_000_000_000_000);
        assert_eq!(
            state
                .fee_on(U256::from(12_345))
                .unwrap(),
            U256::from(2)
        );
        assert_eq!(
            state
                .fee_on(U256::from(10_000))
                .unwrap(),
            U256::from(1)
        );
        assert_eq!(state.fee_on(U256::ZERO).unwrap(), U256::ZERO);
    }

    /// Buying spends the most value whose cost fits the target: at 1 bp, 10,000 costs 10,001 and
    /// 10,001 costs 10,003, so a target of 10,002 still buys 10,000.
    #[rstest]
    #[case::one_fee_step_short(10_000, 9_999)]
    #[case::exactly_one_fee_step(10_001, 10_000)]
    #[case::between_fee_steps(10_002, 10_000)]
    #[case::exactly_two_fee_steps(20_002, 20_000)]
    #[case::two_fee_steps_and_one_over(20_004, 20_001)]
    fn the_affordable_value_is_the_largest_that_fits(#[case] target: u64, #[case] value: u64) {
        let state = state(CBBTC_USDC, Caller::Contract);

        let affordable = state
            .affordable_value(U256::from(target))
            .unwrap();

        assert_eq!(affordable, U256::from(value));
        assert!(state.cost(affordable).unwrap() <= U256::from(target));
        assert!(
            state
                .cost(affordable + U256::from(1))
                .unwrap() >
                U256::from(target)
        );
    }

    /// The best bid of ETH/USDC, less the fee, in USDC per ETH, and its inverse on the ask
    /// side in ETH per USDC.
    #[test]
    fn the_spot_price_is_the_best_level_net_of_the_fee() {
        let state = state(ETH_USDC, Caller::Contract);
        let best_bid = f64::from(u32::try_from(state.bids[0].price).unwrap()) / 100.0;
        let best_ask = f64::from(u32::try_from(state.asks[0].price).unwrap()) / 100.0;

        let selling = state
            .spot_price(&eth(), &usdc())
            .unwrap();
        let buying = state
            .spot_price(&usdc(), &eth())
            .unwrap();

        assert!((selling - best_bid * 0.9999).abs() < 1e-9 * best_bid);
        assert!((buying - 1.0 / (best_ask * 1.0001)).abs() < 1e-12);
    }

    #[test]
    fn a_one_sided_book_trades_one_way() {
        let mut state = state(CBBTC_USDC, Caller::Contract);
        state.asks.clear();

        let directions = state.directions();
        let (input, output) = limits(&state, false);
        let buying = quote(&state, U256::from(1_000_000), false);

        assert_eq!(directions, vec![(cbbtc().address, usdc().address)]);
        assert_eq!((input, output), (U256::ZERO, U256::ZERO));
        assert!(matches!(buying, Err(SimulationError::InvalidInput(..))));
        assert!(state
            .spot_price(&usdc(), &cbbtc())
            .is_err());
    }

    #[rstest]
    #[case::zero_scaling(|state: &mut HanjiState| state.scaling_x = U256::ZERO)]
    #[case::whole_value_fee(|state: &mut HanjiState| state.fee_rate = RATE_DENOMINATOR)]
    #[case::empty_level(|state: &mut HanjiState| state.bids[1].shares = U256::ZERO)]
    #[case::asks_out_of_order(|state: &mut HanjiState| state.asks.swap(0, 1))]
    #[case::bids_out_of_order(|state: &mut HanjiState| state.bids.swap(0, 1))]
    fn a_malformed_book_is_refused(#[case] break_it: fn(&mut HanjiState)) {
        let mut state = state(CBBTC_USDC, Caller::Contract);
        break_it(&mut state);

        assert!(state.validated().is_err());
    }

    /// Deserializing runs the same checks as building a state, so a recorded state no contract
    /// would report is refused instead of failing later inside a quote.
    #[test]
    fn a_malformed_serialized_state_is_refused() {
        let mut state = state(CBBTC_ETH, Caller::Contract);
        state.asks[0].price = U256::ZERO;
        let json = serde_json::to_string(&state).unwrap();

        let restored = serde_json::from_str::<HanjiState>(&json);

        assert!(restored.is_err());
    }

    /// A book whose values overflow 256 bits fails its quotes and limits instead of wrapping.
    #[test]
    fn an_overflowing_book_fails_instead_of_wrapping() {
        let mut state = state(CBBTC_USDC, Caller::Contract);
        state.scaling_y = U256::MAX;

        let selling = quote(&state, U256::from(1_000_000), true);
        let limits = state.get_limits(cbbtc().address, usdc().address);

        assert!(selling.is_err());
        assert!(limits.is_err());
    }

    /// A one-share bid at price 1 is worth one unit, all of it fee (the fee rounds up), so selling
    /// into it pays nothing: the side offers no limit, as get_amount_out refuses it.
    #[test]
    fn a_side_whose_fee_takes_its_whole_value_has_no_limit() {
        let mut state = state(CBBTC_USDC, Caller::Contract);
        state.bids = vec![Level { price: U256::from(1), shares: U256::from(1) }];

        let (input, output) = limits(&state, true);
        let selling = quote(&state, state.scaling_x, true);

        assert_eq!((input, output), (U256::ZERO, U256::ZERO));
        assert!(matches!(selling, Err(SimulationError::InvalidInput(..))));
    }

    #[test]
    fn a_state_survives_serialization() {
        let state = state(CBBTC_ETH, Caller::Contract);

        let json = serde_json::to_string(&state).unwrap();
        let restored: HanjiState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored, state);
        assert_eq!(ProtocolSim::fee(&restored), f64::from_str("0.0001").unwrap());
    }
}
