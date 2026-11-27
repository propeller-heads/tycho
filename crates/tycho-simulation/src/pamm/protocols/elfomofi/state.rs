//! One ElFomoFi pair, priced by interpolating the helper's cumulative ladders.

use std::{any::Any, collections::HashMap};

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
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
    book::{
        component::PairState,
        sim::{self, SwapDirection},
    },
    pamm::protocols::elfomofi::models::ElfomofiSupportedChain,
    utils::decimals_scale,
};

/// One rung of a cumulative ladder: in total, `input` of the token sold buys `output` of the
/// token bought.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rung {
    input: BigUint,
    output: BigUint,
}

/// A cumulative ladder whose inputs and outputs both rise strictly from zero, so every input up
/// to the last rung's prices to one output. Empty when the side does not trade. Deserializing
/// runs the same check as [`Ladder::new`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Rung>", into = "Vec<Rung>")]
pub(crate) struct Ladder(Vec<Rung>);

impl TryFrom<Vec<Rung>> for Ladder {
    type Error = String;

    fn try_from(rungs: Vec<Rung>) -> Result<Self, Self::Error> {
        let mut previous = (BigUint::ZERO, BigUint::ZERO);
        for (index, rung) in rungs.iter().enumerate() {
            if rung.input <= previous.0 || rung.output <= previous.1 {
                return Err(format!(
                    "level {index} ({} -> {}) does not rise above ({} -> {})",
                    rung.input, rung.output, previous.0, previous.1
                ));
            }
            previous = (rung.input.clone(), rung.output.clone());
        }
        Ok(Ladder(rungs))
    }
}

impl From<Ladder> for Vec<Rung> {
    fn from(ladder: Ladder) -> Self {
        ladder.0
    }
}

impl Ladder {
    /// A ladder from `(cumulative input, cumulative output)` pairs; fails unless both rise
    /// strictly from zero.
    pub(crate) fn new(levels: Vec<(BigUint, BigUint)>) -> Result<Self, String> {
        Ladder::try_from(
            levels
                .into_iter()
                .map(|(input, output)| Rung { input, output })
                .collect::<Vec<_>>(),
        )
    }

    /// The segment `input` falls in, as its two ends; the first starts at zero. `None` past the
    /// last rung. An input on a rung belongs to the segment the rung closes.
    fn segment(&self, input: &BigUint) -> Option<(Rung, &Rung)> {
        let mut start = Rung { input: BigUint::ZERO, output: BigUint::ZERO };
        for rung in &self.0 {
            if *input <= rung.input {
                return Some((start, rung));
            }
            start = rung.clone();
        }
        None
    }

    /// The output `input` buys, interpolated linearly inside its segment and rounded down;
    /// `None` past the last rung.
    fn output_for(&self, input: &BigUint) -> Option<BigUint> {
        let (start, end) = self.segment(input)?;
        let progressed = input - &start.input;
        let span_in = &end.input - &start.input;
        let span_out = &end.output - &start.output;
        Some(start.output + progressed * span_out / span_in)
    }

    /// The largest input whose output does not exceed `cap`, and that output; zero for both when
    /// no input buys anything under the cap.
    fn max_input_within(&self, cap: &BigUint) -> (BigUint, BigUint) {
        let (input, output) = self.max_input_within_unchecked(cap);
        if output.is_zero() {
            return (BigUint::ZERO, BigUint::ZERO);
        }
        (input, output)
    }

    fn max_input_within_unchecked(&self, cap: &BigUint) -> (BigUint, BigUint) {
        let mut start = Rung { input: BigUint::ZERO, output: BigUint::ZERO };
        for rung in &self.0 {
            if rung.output > *cap {
                // Inside this segment, `output_for(start.input + d)` is
                // `start.output + floor(d * span_out / span_in)`, which stays at or below the cap
                // for every `d` with `d * span_out < (cap - start.output + 1) * span_in`.
                let span_in = &rung.input - &start.input;
                let span_out = &rung.output - &start.output;
                let headroom = cap - &start.output + 1u8;
                let progressed = (headroom * &span_in - 1u8) / &span_out;
                let input = &start.input + progressed;
                let output = self
                    .output_for(&input)
                    .unwrap_or_default();
                return (input, output);
            }
            start = rung.clone();
        }
        (start.input, start.output)
    }

    /// The segment the next unit of input after `filled` is priced in, as `(input, output)`
    /// spans; `None` once the ladder is used up.
    fn marginal_segment(&self, filled: &BigUint) -> Option<(BigUint, BigUint)> {
        let mut start = Rung { input: BigUint::ZERO, output: BigUint::ZERO };
        for rung in &self.0 {
            if *filled < rung.input {
                return Some((&rung.input - &start.input, &rung.output - &start.output));
            }
            start = rung.clone();
        }
        None
    }

    /// The first rung as `(input, output)`; `None` for an empty ladder.
    pub(crate) fn first_rung(&self) -> Option<(&BigUint, &BigUint)> {
        self.0
            .first()
            .map(|rung| (&rung.input, &rung.output))
    }

    #[cfg(test)]
    pub(crate) fn last_rung(&self) -> Option<(&BigUint, &BigUint)> {
        self.0
            .last()
            .map(|rung| (&rung.input, &rung.output))
    }
}

/// Which way a state's fill ran through its pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum FillDirection {
    BaseToQuote,
    QuoteToBase,
}

impl From<SwapDirection> for FillDirection {
    fn from(direction: SwapDirection) -> Self {
        match direction {
            SwapDirection::BaseToQuote => FillDirection::BaseToQuote,
            SwapDirection::QuoteToBase => FillDirection::QuoteToBase,
        }
    }
}

/// What a route has already taken from this state's book: the total input and output of the
/// swaps it merged on one direction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fill {
    direction: FillDirection,
    input: BigUint,
    output: BigUint,
}

/// One pair's book as the helper read it at one block.
///
/// Selling base walks the bids, up to the vault's quote balance; selling quote walks the asks,
/// up to its base balance. A swap returns a state recording the route's fill, so the next swap
/// on the same direction is priced as its continuation (at zero gas: the caller merges swaps on
/// one pair and direction into one), and the opposite direction is refused, since how a fill
/// moves the other side of the book is unknown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElfomofiState {
    chain: ElfomofiSupportedChain,
    block_number: u64,
    block_timestamp: u64,
    base: Token,
    quote: Token,
    bids: Ladder,
    asks: Ladder,
    base_balance: BigUint,
    quote_balance: BigUint,
    fill: Option<Fill>,
}

/// The book half of an [`ElfomofiState`]: both ladders and both balances.
#[derive(Clone, Debug)]
pub(crate) struct BookSides {
    pub bids: Ladder,
    pub asks: Ladder,
    pub base_balance: BigUint,
    pub quote_balance: BigUint,
}

impl ElfomofiState {
    pub(crate) fn new(
        chain: ElfomofiSupportedChain,
        (block_number, block_timestamp): (u64, u64),
        (base, quote): (Token, Token),
        sides: BookSides,
    ) -> Self {
        let BookSides { bids, asks, base_balance, quote_balance } = sides;
        ElfomofiState {
            chain,
            block_number,
            block_timestamp,
            base,
            quote,
            bids,
            asks,
            base_balance,
            quote_balance,
            fill: None,
        }
    }

    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    pub fn block_timestamp(&self) -> u64 {
        self.block_timestamp
    }

    fn direction(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<SwapDirection, SimulationError> {
        SwapDirection::require(&self.base, &self.quote, token_in, token_out)
    }

    fn ladder(&self, direction: SwapDirection) -> &Ladder {
        match direction {
            SwapDirection::BaseToQuote => &self.bids,
            SwapDirection::QuoteToBase => &self.asks,
        }
    }

    /// The whole book's bound in `direction`, before any fill: the most input whose output the
    /// vault's balance of the output token still covers, and that output.
    fn book_limit(&self, direction: SwapDirection) -> (BigUint, BigUint) {
        let balance = match direction {
            SwapDirection::BaseToQuote => &self.quote_balance,
            SwapDirection::QuoteToBase => &self.base_balance,
        };
        self.ladder(direction)
            .max_input_within(balance)
    }

    /// The input and output a route has already taken in `direction`; fails when it filled the
    /// opposite direction.
    fn filled(&self, direction: SwapDirection) -> Result<(BigUint, BigUint), SimulationError> {
        match &self.fill {
            None => Ok((BigUint::ZERO, BigUint::ZERO)),
            Some(fill) if fill.direction == FillDirection::from(direction) => {
                Ok((fill.input.clone(), fill.output.clone()))
            }
            Some(_) => Err(SimulationError::RecoverableError(
                "This ElFomoFi book was already quoted in the opposite direction on this route; \
                 its prices that way after that fill are unknown"
                    .to_string(),
            )),
        }
    }

    /// The input and output the book can still take in `direction` after the route's fill; zero
    /// for both once either is used up. A fill can pay out all of the vault's balance while its
    /// input stops short of the bound by less than one unit of output's worth, and no more input
    /// then buys anything.
    fn remaining(&self, direction: SwapDirection) -> Result<(BigUint, BigUint), SimulationError> {
        let (filled_in, filled_out) = self.filled(direction)?;
        let (max_in, max_out) = self.book_limit(direction);
        if max_in <= filled_in || max_out <= filled_out {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        Ok((max_in - filled_in, max_out - filled_out))
    }
}

impl PairState for ElfomofiState {
    fn base_token(&self) -> &Token {
        &self.base
    }

    fn quote_token(&self) -> &Token {
        &self.quote
    }

    fn directions(&self) -> Vec<(Bytes, Bytes)> {
        let tradable = |direction| match self.remaining(direction) {
            Ok((input, _)) => !input.is_zero(),
            Err(_) => false,
        };
        sim::directions(
            &self.base,
            &self.quote,
            tradable(SwapDirection::BaseToQuote),
            tradable(SwapDirection::QuoteToBase),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for ElfomofiState {
    /// The ladders are net of the venue's fee.
    fn fee(&self) -> f64 {
        0.0
    }

    /// The price of the next unit of `base` bought with `quote`, from the segment the route's
    /// fill has reached in that direction (the first one when it has none).
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = self.direction(&quote.address, &base.address)?;
        let (filled, _) = self.filled(direction)?;
        let no_liquidity = || {
            SimulationError::RecoverableError(format!(
                "ElFomoFi book has no liquidity selling {} for {}",
                quote.symbol, base.symbol
            ))
        };
        if self.remaining(direction)?.0.is_zero() {
            return Err(no_liquidity());
        }
        let (span_in, span_out) = self
            .ladder(direction)
            .marginal_segment(&filled)
            .ok_or_else(no_liquidity)?;
        let (in_decimals, out_decimals) = direction.decimals(&self.base, &self.quote);
        let human = |amount: &BigUint, decimals| {
            amount.to_f64().unwrap_or(f64::INFINITY) / decimals_scale(decimals)
        };
        Ok(human(&span_in, in_decimals) / human(&span_out, out_decimals))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        sim::require_positive_amount(&amount_in)?;
        let direction = self.direction(&token_in.address, &token_out.address)?;
        let (remaining_in, _) = self.remaining(direction)?;
        if amount_in > remaining_in {
            return Err(SimulationError::InvalidInput(
                format!(
                    "ElFomoFi book cannot fill {amount_in} {}: it takes at most {remaining_in} more",
                    token_in.symbol,
                ),
                None,
            ));
        }
        let (filled_in, filled_out) = self.filled(direction)?;
        let total_in = &filled_in + &amount_in;
        let total_out = self
            .ladder(direction)
            .output_for(&total_in)
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "ElFomoFi book limit {total_in} lies past its own ladder"
                ))
            })?;
        let amount = &total_out - &filled_out;
        if amount.is_zero() {
            return Err(SimulationError::InvalidInput(
                format!("ElFomoFi book pays nothing for {amount_in} {}", token_in.symbol),
                None,
            ));
        }
        // A continuation is merged into the swap that opened the fill, which already paid the gas.
        let gas = if self.fill.is_none() { self.chain.swap_gas() } else { 0 };
        let mut new_state = self.clone();
        new_state.fill =
            Some(Fill { direction: direction.into(), input: total_in, output: total_out });
        Ok(GetAmountOutResult { amount, gas: BigUint::from(gas), new_state: Box::new(new_state) })
    }

    /// What the book can still take in this direction after the route's fill.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction = self.direction(&sell_token, &buy_token)?;
        self.remaining(direction)
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError(
            "ElFomoFi states are replaced by whole snapshots and take no deltas".into(),
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
        other
            .as_any()
            .downcast_ref::<ElfomofiState>()
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::pamm::protocols::elfomofi::test_support::{
        big, ladder, state_with_balances, usdc, weth, weth_usdc_state, BIDS,
    };

    const ONE_WETH: u128 = 1_000_000_000_000_000_000;
    const BASE_GAS: u64 = 452_000;

    fn sell_weth(
        state: &ElfomofiState,
        amount: u128,
    ) -> Result<GetAmountOutResult, SimulationError> {
        state.get_amount_out(big(amount), &weth(), &usdc())
    }

    fn filled(result: &GetAmountOutResult) -> &ElfomofiState {
        result
            .new_state
            .as_any()
            .downcast_ref::<ElfomofiState>()
            .unwrap()
    }

    /// The bids pay 3,000 USDC for the first WETH and 5,970 for the next two, so inside the
    /// second segment each WETH buys 2,985.
    #[rstest]
    #[case::inside_the_first_segment(ONE_WETH / 2, 1_500_000_000)]
    #[case::on_the_first_rung(ONE_WETH, 3_000_000_000)]
    #[case::inside_the_last_segment(2 * ONE_WETH, 5_985_000_000)]
    #[case::on_the_last_rung(3 * ONE_WETH, 8_970_000_000)]
    #[case::rounded_down(1, 0)]
    fn ladders_interpolate_inside_their_segments(#[case] input: u128, #[case] output: u128) {
        assert_eq!(ladder(&BIDS).output_for(&big(input)), Some(big(output)));
    }

    #[test]
    fn a_ladder_prices_nothing_past_its_last_rung() {
        assert_eq!(ladder(&BIDS).output_for(&big(3 * ONE_WETH + 1)), None);
    }

    #[rstest]
    #[case::input_falls(vec![(2, 10), (1, 20)])]
    #[case::input_repeats(vec![(1, 10), (1, 20)])]
    #[case::output_repeats(vec![(1, 10), (2, 10)])]
    #[case::starts_at_zero_input(vec![(0, 10)])]
    #[case::starts_at_zero_output(vec![(10, 0)])]
    fn a_ladder_that_does_not_rise_strictly_is_refused(#[case] levels: Vec<(u128, u128)>) {
        let levels = levels
            .into_iter()
            .map(|(input, output)| (big(input), big(output)))
            .collect();

        assert!(Ladder::new(levels).is_err());
    }

    #[test]
    fn a_ladder_that_does_not_rise_strictly_does_not_deserialize() {
        let json = r#"[{"input":[2],"output":[10]},{"input":[1],"output":[20]}]"#;

        assert!(serde_json::from_str::<Ladder>(json).is_err());
    }

    #[test]
    fn a_first_swap_pays_the_ladder_and_the_chains_gas() {
        let result = sell_weth(&weth_usdc_state(), ONE_WETH).unwrap();

        assert_eq!(result.amount, big(3_000_000_000));
        assert_eq!(result.gas, BigUint::from(BASE_GAS));
    }

    #[test]
    fn a_swap_past_the_ladder_is_refused() {
        let error = sell_weth(&weth_usdc_state(), 3 * ONE_WETH + 1).unwrap_err();

        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error}");
    }

    #[test]
    fn a_swap_of_nothing_is_refused() {
        let error = sell_weth(&weth_usdc_state(), 0).unwrap_err();

        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error}");
    }

    #[test]
    fn a_swap_too_small_to_buy_a_unit_is_refused() {
        let error = sell_weth(&weth_usdc_state(), 1).unwrap_err();

        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error}");
    }

    /// Two swaps on one direction pay what the one swap of their sum pays, and the second, merged
    /// into the first by the caller, costs no gas.
    #[test]
    fn a_continued_swap_pays_the_rest_of_one_merged_swap() {
        let state = weth_usdc_state();
        let first = sell_weth(&state, ONE_WETH).unwrap();
        let second = sell_weth(filled(&first), ONE_WETH).unwrap();
        let merged = sell_weth(&state, 2 * ONE_WETH).unwrap();

        assert_eq!(&first.amount + &second.amount, merged.amount);
        assert_eq!(second.gas, BigUint::ZERO);
        assert_eq!(
            filled(&second)
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (big(ONE_WETH), big(2_985_000_000))
        );
    }

    #[test]
    fn a_filled_book_refuses_the_opposite_direction() {
        let result = sell_weth(&weth_usdc_state(), ONE_WETH).unwrap();
        let state = filled(&result);

        let quote = state.get_amount_out(big(1_000_000), &usdc(), &weth());
        let limits = state.get_limits(usdc().address, weth().address);
        let price = state.spot_price(&weth(), &usdc());

        for error in [quote.unwrap_err(), limits.unwrap_err(), price.unwrap_err()] {
            assert!(matches!(error, SimulationError::RecoverableError(_)), "{error}");
        }
        assert_eq!(state.directions(), vec![(weth().address, usdc().address)]);
    }

    #[test]
    fn a_fill_moves_the_spot_price_to_the_segment_it_reached() {
        let state = weth_usdc_state();
        let before = state
            .spot_price(&usdc(), &weth())
            .unwrap();

        let result = sell_weth(&state, ONE_WETH).unwrap();
        let after = filled(&result)
            .spot_price(&usdc(), &weth())
            .unwrap();

        assert!((before - 1.0 / 3_000.0).abs() < 1e-15, "{before}");
        assert!((after - 1.0 / 2_985.0).abs() < 1e-15, "{after}");
    }

    /// Buying WETH walks the asks, whose first rung pays 0.99 WETH for 3,000 USDC.
    #[test]
    fn the_spot_price_of_base_is_the_first_ask() {
        let price = weth_usdc_state()
            .spot_price(&weth(), &usdc())
            .unwrap();

        assert!((price - 3_000.0 / 0.99).abs() < 1e-9, "{price}");
    }

    #[test]
    fn a_book_trades_both_ways_until_one_side_runs_dry() {
        assert_eq!(
            weth_usdc_state().directions(),
            vec![(weth().address, usdc().address), (usdc().address, weth().address)]
        );
        let no_usdc = state_with_balances(big(100 * ONE_WETH), BigUint::ZERO);

        assert_eq!(no_usdc.directions(), vec![(usdc().address, weth().address)]);
        let error = no_usdc
            .spot_price(&usdc(), &weth())
            .unwrap_err();
        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error}");
    }

    #[test]
    fn an_empty_side_neither_trades_nor_prices() {
        let mut state = weth_usdc_state();
        state.bids = Ladder::default();

        assert_eq!(state.directions(), vec![(usdc().address, weth().address)]);
        assert!(state
            .spot_price(&usdc(), &weth())
            .is_err());
        assert!(sell_weth(&state, ONE_WETH).is_err());
        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    /// With 5,985 USDC in the vault, the bids stop inside their second segment, where each WETH
    /// buys 2,985: 1 WETH plus 2,985 / 5,970 of 2 WETH pays 5,985 exactly, and the rounding down
    /// of the interpolation lets 335,008,375 more wei through before a unit more is paid.
    #[test]
    fn the_limit_stops_where_the_vault_balance_runs_out() {
        let state = state_with_balances(big(100 * ONE_WETH), big(5_985_000_000));
        let limit = big(2 * ONE_WETH + 335_008_375);

        let limits = state
            .get_limits(weth().address, usdc().address)
            .unwrap();

        assert_eq!(limits, (limit.clone(), big(5_985_000_000)));
        assert_eq!(
            state
                .get_amount_out(limit.clone(), &weth(), &usdc())
                .unwrap()
                .amount,
            limits.1
        );
        assert!(state
            .get_amount_out(limit + 1u8, &weth(), &usdc())
            .is_err());
    }

    /// Selling 2 WETH + 1 wei into the 5,985 USDC vault pays all of it, while 335,008,374 wei of
    /// the limit's input are left: too little to buy another unit, so the direction is used up.
    #[test]
    fn a_fill_that_pays_out_the_vault_uses_the_direction_up() {
        let state = state_with_balances(big(100 * ONE_WETH), big(5_985_000_000));
        let result = sell_weth(&state, 2 * ONE_WETH + 1).unwrap();
        let state = filled(&result);

        assert_eq!(result.amount, big(5_985_000_000));
        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
        assert_eq!(state.directions(), Vec::<(Bytes, Bytes)>::new());
        assert!(state
            .spot_price(&usdc(), &weth())
            .is_err());
        let error = sell_weth(state, 1).unwrap_err();
        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error}");
    }

    #[test]
    fn a_state_survives_serialization_with_its_fill() {
        let result = sell_weth(&weth_usdc_state(), ONE_WETH).unwrap();
        let state = filled(&result);

        let json = serde_json::to_string(state).unwrap();
        let restored: ElfomofiState = serde_json::from_str(&json).unwrap();

        assert_eq!(&restored, state);
        assert!(weth_usdc_state().fill.is_none());
    }

    #[test]
    fn deltas_are_refused() {
        let mut state = weth_usdc_state();

        let result = state.delta_transition(
            ProtocolStateDelta::default(),
            &HashMap::new(),
            &Balances::default(),
        );

        assert!(result.is_err());
    }

    /// A small xorshift, so the sweep below is the same on every run.
    struct Random(u64);

    impl Random {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % bound
        }
    }

    /// Over random ladders and balances, in both directions and before and after a fill, the
    /// limit is exactly the edge of what the state quotes: the limit itself quotes its own output,
    /// and one unit more is refused.
    #[test]
    fn limits_are_the_edge_of_the_quotes_on_random_books() {
        let mut random = Random(0x5eed_e1f0);
        for _ in 0..500 {
            let mut side = || {
                let mut levels = Vec::new();
                let (mut input, mut output) = (0u128, 0u128);
                for _ in 0..random.below(5) {
                    input += u128::from(random.below(1_000_000) + 1);
                    output += u128::from(random.below(1_000_000) + 1);
                    levels.push((input, output));
                }
                ladder(&levels)
            };
            let (bids, asks) = (side(), side());
            let mut state = state_with_balances(
                big(u128::from(random.below(5_000_000))),
                big(u128::from(random.below(5_000_000))),
            );
            state.bids = bids;
            state.asks = asks;
            for (token_in, token_out) in [(weth(), usdc()), (usdc(), weth())] {
                assert_limit_is_the_edge(&state, &token_in, &token_out);
                let (max_in, _) = state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .unwrap();
                let part = big(u128::from(random.below(1_000_000))) % (&max_in + 1u8);
                if let Ok(result) = state.get_amount_out(part, &token_in, &token_out) {
                    assert_limit_is_the_edge(filled(&result), &token_in, &token_out);
                }
            }
        }
    }

    fn assert_limit_is_the_edge(state: &ElfomofiState, token_in: &Token, token_out: &Token) {
        let (max_in, max_out) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();
        if max_in.is_zero() {
            assert!(max_out.is_zero());
            assert!(state
                .get_amount_out(big(1), token_in, token_out)
                .is_err());
            return;
        }
        let at_limit = state
            .get_amount_out(max_in.clone(), token_in, token_out)
            .unwrap();
        assert_eq!(at_limit.amount, max_out, "{state:?}");
        assert!(state
            .get_amount_out(max_in + 1u8, token_in, token_out)
            .is_err());
    }
}
