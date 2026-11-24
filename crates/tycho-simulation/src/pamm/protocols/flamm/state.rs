//! One pair of one pool, priced from its two ladders of `previewSwap` answers.
use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
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
    evm::{
        protocol::u256_num::{biguint_to_u256, u256_to_biguint},
        query_pool_swap::query_pool_swap,
    },
    pamm::protocols::flamm::{gas::SideGas, ladder::Ladder},
};

/// The share of the largest quoted fill's input `get_limits` offers, in basis points: at a venue
/// that holds a minimum position, a quoted payout can revert by the slice the swap passes over
/// (`IFLAMM.previewSwap`), and the pool can move after the block the ladder was quoted at.
const LIMIT_KEEP_BPS: u64 = 9_900;

/// A pair of one Efficient pool: what selling either token fills, as the pool quoted it at
/// `block`, net of every fee its hooks take and of the feed's haircut.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlammPairState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    pub(super) sell_base: Ladder,
    pub(super) sell_quote: Ladder,
    /// The block the ladders were quoted at.
    pub(super) block: u64,
    /// What a swap selling each token is charged, the gas inside the pool's own `swap()` net of
    /// its refund, by how far its payout reaches into the pool's positions as the feed read them;
    /// and what it adds to a transaction's gas limit.
    pub(super) sell_base_gas: SideGas,
    pub(super) sell_quote_gas: SideGas,
    /// Whether a swap returned this state: the positions the feed read are then those before it,
    /// so a swap on it is charged its side's second-fill charge.
    pub(super) moved: bool,
}

fn invalid(reason: impl Into<String>) -> SimulationError {
    SimulationError::InvalidInput(reason.into(), None)
}

impl FlammPairState {
    /// The block the pool was quoted at.
    pub fn block(&self) -> u64 {
        self.block
    }

    fn ladder(&self, direction: SwapDirection) -> &Ladder {
        match direction {
            SwapDirection::BaseToQuote => &self.sell_base,
            SwapDirection::QuoteToBase => &self.sell_quote,
        }
    }

    fn side_gas(&self, direction: SwapDirection) -> &SideGas {
        match direction {
            SwapDirection::BaseToQuote => &self.sell_base_gas,
            SwapDirection::QuoteToBase => &self.sell_quote_gas,
        }
    }

    /// What a swap selling `sell_token` for `buy_token` adds to a transaction's gas limit: the
    /// dearest fill measured of its kind, in any state and to a fresh receiver, with the router's
    /// own work around the pool's call, before refunds, times 1.05 x 1.10. An addition, not a
    /// limit: a solver adds it to the limit of the rest of its route, as `get_amount_out`'s gas is
    /// a charge, not a limit. Zero where the state quotes nothing that way.
    pub fn gas_limit_addition(
        &self,
        sell_token: &Bytes,
        buy_token: &Bytes,
    ) -> Result<u64, SimulationError> {
        let direction =
            SwapDirection::require(&self.base_token, &self.quote_token, sell_token, buy_token)?;
        Ok(self
            .side_gas(direction)
            .limit_addition())
    }
}

impl PairState for FlammPairState {
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
            !self.sell_base.is_empty(),
            !self.sell_quote.is_empty(),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for FlammPairState {
    /// The ladders are net of every fee the pool and its hooks take.
    fn fee(&self) -> f64 {
        0.0
    }

    /// What buying the smallest quoted fill of `base` costs in `quote`, per whole `base`: the
    /// first rung of the ladder that sells `quote` for `base`.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &quote.address,
            &base.address,
        )?;
        let first = self
            .ladder(direction)
            .first()
            .ok_or_else(|| {
                invalid(format!("the pool sells no {} for {}", base.symbol, quote.symbol))
            })?;
        let paid = sim::to_human(&u256_to_biguint(first.input), quote.decimals);
        let bought = sim::to_human(&u256_to_biguint(first.output), base.decimals);
        Ok(paid / bought)
    }

    /// The output of `amount_in` on its side's ladder: a quoted fill's own at its input, the fill
    /// below's inside a span that changes route, and the chord between the two fills around it
    /// elsewhere, rounded down. Refused under the smallest quoted fill and past the largest. The
    /// next state's ladder on this side is what the pool has left past the fill, and empty where
    /// the ladder has a span quoted at the fill below, as that fill no longer says what is left.
    ///
    /// The gas is a charge, never more than the swap costs inside the pool's `swap()` net of its
    /// refund, in the state the feed read: the least measured over the paths that state allows a
    /// fill of that output, and on a state a swap returned, the least measured of the second of
    /// two fills of one pool in one transaction.
    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        sim::require_positive_amount(&amount_in)?;
        if amount_in.bits() > 256 {
            return Err(invalid("amount exceeds uint256"));
        }
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &token_in.address,
            &token_out.address,
        )?;
        let input = biguint_to_u256(&amount_in);
        let ladder = self.ladder(direction);
        let Some(output) = ladder.output(input) else {
            let below = ladder
                .first()
                .is_some_and(|first| input < first.input);
            return Err(invalid(if below {
                "input below the smallest quoted fill"
            } else {
                "input past the largest quoted fill"
            }));
        };
        if output.is_zero() {
            return Err(invalid("input below the pool's precision"));
        }
        let gas = self
            .side_gas(direction)
            .charge(output, self.moved);
        let mut next = self.clone();
        match direction {
            SwapDirection::BaseToQuote => next.sell_base = ladder.after(input),
            SwapDirection::QuoteToBase => next.sell_quote = ladder.after(input),
        }
        next.moved = true;
        Ok(GetAmountOutResult::new(u256_to_biguint(output), BigUint::from(gas), Box::new(next)))
    }

    /// 99 % of the largest quoted fill's input, or the smallest fill's input where that is more,
    /// so a ladder of one rung still offers an input it prices; and that input's output.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction =
            SwapDirection::require(&self.base_token, &self.quote_token, &sell_token, &buy_token)?;
        let ladder = self.ladder(direction);
        let (Some(first), Some(last)) = (ladder.first(), ladder.last()) else {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        };
        let input =
            (last.input * U256::from(LIMIT_KEEP_BPS) / U256::from(10_000u64)).max(first.input);
        Ok(match ladder.output(input) {
            Some(output) if !output.is_zero() => (u256_to_biguint(input), u256_to_biguint(output)),
            _ => (BigUint::ZERO, BigUint::ZERO),
        })
    }

    /// Each poll publishes whole states; no delta reaches a book.
    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError(
            "an Efficient pool book is published whole, never as a delta".into(),
        ))
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        query_pool_swap(self, params)
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
            .downcast_ref::<Self>()
            .is_some_and(|o| o == self)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::{
        models::Chain,
        simulation::protocol_sim::{Price, SwapConstraint},
    };

    use super::*;
    use crate::pamm::protocols::flamm::{
        gas::{
            tests::{table, venue, CLASS},
            Legs, Venues,
        },
        ladder::{Rung, Segment},
    };

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, decimals, 0, &[], Chain::Base, 100)
    }

    fn btc() -> Token {
        token("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf", "cbBTC", 8)
    }

    fn usdc() -> Token {
        token("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", "USDC", 6)
    }

    /// A ladder of `(input, output, segment)` rungs.
    fn rungs(rungs: &[(u64, u64, Segment)]) -> Ladder {
        Ladder::from_rungs(
            rungs
                .iter()
                .map(|(input, output, segment)| Rung {
                    input: U256::from(*input),
                    output: U256::from(*output),
                    segment: *segment,
                })
                .collect(),
        )
    }

    /// A ladder of chords between `(input, output)` rungs.
    fn ladder(points: &[(u64, u64)]) -> Ladder {
        let chords: Vec<_> = points
            .iter()
            .map(|(input, output)| (*input, *output, Segment::Chord))
            .collect();
        rungs(&chords)
    }

    /// cbBTC/USDC: 0.0001 BTC sells for 7.99 USDC, 8.01 USDC buys 0.0001 BTC.
    fn state() -> FlammPairState {
        FlammPairState {
            base_token: btc(),
            quote_token: usdc(),
            sell_base: ladder(&[(10_000, 7_990_000), (20_000, 15_970_000)]),
            sell_quote: ladder(&[(8_010_000, 10_000), (16_030_000, 19_990)]),
            block: 7,
            sell_base_gas: SideGas::flat(600_000, 1_200_000),
            sell_quote_gas: SideGas::flat(500_000, 1_100_000),
            moved: false,
        }
    }

    /// The sale side of [`table`] over one Morpho Blue venue, its output holding 7,990,000, with
    /// nothing at the venue and nothing owed: 100,000 within custody, 250,000 for a first borrow.
    fn read_sale() -> SideGas {
        let venues = Venues { records: vec![venue(0)], unread: false };
        let mut legs: Legs = [U256::ZERO; 8];
        legs[4] = U256::from(7_990_000u64);
        table()
            .key(8453, CLASS, 0, 1, &venues)
            .unwrap()
            .side_gas(Some(&legs), true)
    }

    #[test]
    fn spot_price_is_what_buying_the_base_costs() {
        let price = state()
            .spot_price(&btc(), &usdc())
            .unwrap();
        assert!((price - 80_100.0).abs() < 1e-6, "{price}");
        // and the other way round: what buying USDC costs in BTC
        let price = state()
            .spot_price(&usdc(), &btc())
            .unwrap();
        assert!((price - 0.0001 / 7.99).abs() < 1e-12, "{price}");
    }

    #[test]
    fn a_swap_fills_its_rung_and_leaves_the_rest_of_the_ladder() {
        let state = FlammPairState {
            sell_base: ladder(&[(10_000, 7_990_000), (20_000, 15_970_000), (40_000, 31_800_000)]),
            ..state()
        };
        let result = state
            .get_amount_out(BigUint::from(10_000u64), &btc(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(7_990_000u64));
        assert_eq!(result.gas, BigUint::from(600_000u64));
        let selling_quote = state
            .get_amount_out(BigUint::from(8_010_000u64), &usdc(), &btc())
            .unwrap();
        assert_eq!(selling_quote.gas, BigUint::from(500_000u64));
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<FlammPairState>()
            .unwrap();
        assert_eq!(next.sell_base.rungs().len(), 1);
        assert_eq!(next.sell_quote, state.sell_quote);
        assert!(state
            .get_amount_out(BigUint::from(40_001u64), &btc(), &usdc())
            .is_err());
    }

    /// 99 % of the last rung's input, priced where the ladder prices it, and never under the
    /// first rung, where a ladder quotes nothing.
    #[rstest]
    // 7,990,000 + 7,980,000 * 9,800 / 10,000 on the chord
    #[case::two_rungs(
        ladder(&[(10_000, 7_990_000), (20_000, 15_970_000)]),
        19_800,
        15_810_400
    )]
    #[case::one_rung(ladder(&[(10_000, 7_990_000)]), 10_000, 7_990_000)]
    #[case::a_span_quoted_at_the_rung_below(
        rungs(&[(10_000, 7_990_000, Segment::Chord), (20_000, 15_970_000, Segment::Flat)]),
        19_800,
        7_990_000
    )]
    #[case::no_rung(Ladder::default(), 0, 0)]
    fn the_limit_is_99_percent_of_the_last_rung(
        #[case] sell_base: Ladder,
        #[case] input: u64,
        #[case] output: u64,
    ) {
        let state = FlammPairState { sell_base, ..state() };
        let limits = state
            .get_limits(btc().address, usdc().address)
            .unwrap();
        assert_eq!(limits, (BigUint::from(input), BigUint::from(output)));
    }

    #[test]
    fn an_input_off_the_ladder_is_refused_and_says_which_end() {
        let state = state();
        let error = |amount: u64| {
            state
                .get_amount_out(BigUint::from(amount), &btc(), &usdc())
                .unwrap_err()
                .to_string()
        };
        assert!(error(9_999).contains("below the smallest"), "{}", error(9_999));
        assert!(error(20_001).contains("past the largest"), "{}", error(20_001));
    }

    #[test]
    fn a_swap_inside_a_span_quoted_at_the_rung_below_leaves_no_second_fill() {
        let sell_base =
            rungs(&[(10_000, 7_990_000, Segment::Chord), (20_000, 15_970_000, Segment::Flat)]);
        let state = FlammPairState { sell_base, ..state() };
        let result = state
            .get_amount_out(BigUint::from(15_000u64), &btc(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(7_990_000u64));
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<FlammPairState>()
            .unwrap();
        assert!(next.sell_base.is_empty());
        assert_eq!(next.sell_quote, state.sell_quote);
    }

    /// The search asks a flat ladder for sizes it may not quote, and a next state that quotes
    /// nothing more on its side: whatever it finds is a ladder quote, or it fails.
    #[rstest]
    #[case::a_trade_limit(SwapConstraint::TradeLimitPrice {
        limit: Price::new(BigUint::from(780u64), BigUint::from(1u64)),
        tolerance: 0.0,
        min_amount_in: None,
        max_amount_in: None,
    })]
    #[case::a_pool_target(SwapConstraint::PoolTargetPrice {
        target: Price::new(BigUint::from(1u64), BigUint::from(1_000u64)),
        tolerance: 0.0,
        min_amount_in: None,
        max_amount_in: None,
    })]
    fn a_search_on_a_flat_ladder_ends_in_a_ladder_quote_or_an_error(
        #[case] constraint: SwapConstraint,
    ) {
        let flat = rungs(&[
            (10_000, 7_990_000, Segment::Chord),
            (20_000, 15_970_000, Segment::Flat),
            (40_000, 31_900_000, Segment::Flat),
        ]);
        let state = FlammPairState { sell_base: flat.clone(), sell_quote: flat, ..state() };
        let params = QueryPoolSwapParams::new(btc(), usdc(), constraint);
        let Ok(swap) = state.query_pool_swap(&params) else { return };
        if swap.amount_in() == &BigUint::ZERO {
            assert_eq!(swap.amount_out(), &BigUint::ZERO);
            return;
        }
        let quoted = state
            .get_amount_out(swap.amount_in().clone(), &btc(), &usdc())
            .unwrap();
        assert_eq!(swap.amount_out(), &quoted.amount);
    }

    #[test]
    fn a_one_sided_state_names_only_its_side() {
        let mut state = state();
        state.sell_quote = Ladder::default();
        assert_eq!(state.directions(), vec![(btc().address, usdc().address)]);
        assert!(state
            .spot_price(&btc(), &usdc())
            .is_err());
    }

    /// A payout within the output's custody is charged a custody fill, one past it a borrow; a
    /// state a swap returned is charged its side's second-fill charge, on either side, as the fill
    /// moved what the feed read and warmed what the next touches.
    #[test]
    fn a_swap_is_charged_by_its_payout_then_its_next_state_its_second_fill_charge() {
        let state = FlammPairState {
            sell_base: ladder(&[(10_000, 7_990_000), (20_000, 15_970_000), (40_000, 31_800_000)]),
            sell_base_gas: read_sale(),
            sell_quote_gas: read_sale(),
            ..state()
        };
        let charge = |state: &FlammPairState, amount: u64| {
            state
                .get_amount_out(BigUint::from(amount), &btc(), &usdc())
                .unwrap()
        };
        assert_eq!(charge(&state, 10_000).gas, BigUint::from(100_000u64));
        let past_custody = charge(&state, 20_000);
        assert_eq!(past_custody.amount, BigUint::from(15_970_000u64));
        assert_eq!(past_custody.gas, BigUint::from(250_000u64));

        let first = charge(&state, 10_000);
        let next = first
            .new_state
            .as_any()
            .downcast_ref::<FlammPairState>()
            .unwrap();
        assert!(next.moved);
        // what is left of the ladder: 30,000 more sells for 15,830,000, past custody
        assert_eq!(charge(next, 30_000).gas, BigUint::from(60_000u64));
        let other_side = next
            .get_amount_out(BigUint::from(8_010_000u64), &usdc(), &btc())
            .unwrap();
        assert_eq!(other_side.gas, BigUint::from(60_000u64));
    }

    #[test]
    fn the_limit_addition_is_the_sides_and_only_for_the_pairs_tokens() {
        let state = state();
        let (btc, usdc) = (btc().address, usdc().address);
        assert_eq!(
            state
                .gas_limit_addition(&btc, &usdc)
                .unwrap(),
            1_200_000
        );
        assert_eq!(
            state
                .gas_limit_addition(&usdc, &btc)
                .unwrap(),
            1_100_000
        );
        assert!(state
            .gas_limit_addition(&btc, &Bytes::from([0x01; 20]))
            .is_err());
    }

    /// A state saved before swaps were charged by their payout carries one figure and no reads: it
    /// fails to decode rather than being charged that figure.
    #[test]
    fn a_state_saved_with_one_gas_figure_does_not_decode() {
        let boxed: Box<dyn ProtocolSim> = Box::new(state());
        let mut json = serde_json::to_value(&boxed).unwrap();
        let fields = json["state"].as_object_mut().unwrap();
        for key in ["sell_base_gas", "sell_quote_gas", "moved"] {
            assert!(fields.remove(key).is_some(), "{key}");
        }
        fields.insert("gas".into(), serde_json::json!(600_000));
        assert!(serde_json::from_value::<Box<dyn ProtocolSim>>(json).is_err());
    }

    #[rstest]
    #[case::chords(state())]
    #[case::charged_by_payout(FlammPairState { sell_base_gas: read_sale(), ..state() })]
    #[case::moved(FlammPairState { moved: true, ..state() })]
    #[case::a_flat_span(FlammPairState {
        sell_quote: rungs(&[(8_010_000, 10_000, Segment::Chord), (16_030_000, 19_990, Segment::Flat)]),
        ..state()
    })]
    fn the_state_round_trips_through_serde(#[case] state: FlammPairState) {
        let boxed: Box<dyn ProtocolSim> = Box::new(state);
        let json = serde_json::to_string(&boxed).unwrap();
        let back: Box<dyn ProtocolSim> = serde_json::from_str(&json).unwrap();
        assert!(back.eq(boxed.as_ref()));
    }
}
