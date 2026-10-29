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
    evm::protocol::{
        u256_num::{biguint_to_u256, u256_to_biguint},
        utils::solidity_math::mul_div,
    },
    pamm::protocols::baibai::math::{invalid, Curve, Side},
};

/// Gas of the executor's swap: a base cost plus a cost per knot the walk passes. Measured cold on a
/// Base fork at block 53,234,728 against WETH's five-knot sides: 110,963 to 122,575 buying base
/// and 111,330 to 117,438 selling it, rising by 2,908 and 1,527 per knot passed. Curves with more
/// knots were not available to measure. The router's own transfers and approval are added on top
/// by tycho-execution's gas estimator.
const BASE_SWAP_GAS: u64 = 112_000;
const GAS_PER_KNOT_PASSED: u64 = 3_000;

/// The entrypoint's fee denominator: fees are set in millionths of the gross output.
const FEE_DENOMINATOR: u32 = 1_000_000;

/// One BaiBai pair as the chain held it at one block: the curve's two sides, the custody each
/// payout draws on, and the fee the entrypoint charges the taker the feed was built for.
///
/// Prices exact-input swaps wei for wei as `Entrypoint.quoteFor(base, tokenIn, amountIn, taker)`
/// does at that block. It does not price the entrypoint's priority surcharge, 10 bps withheld from
/// a swap sent above a gas price threshold (0.008 gwei when measured): see the module docs.
#[derive(Clone, derive_more::Debug, PartialEq, Serialize, Deserialize)]
pub struct BaibaiState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    /// Quote atoms per 10^18 base atoms, before each side's spread.
    pub(super) mid: U256,
    pub(super) q_unit: U256,
    pub(super) c_unit: U256,
    #[debug("{{ filled: {}, {} knots }}", ask.filled, ask.knots.len())]
    pub(super) ask: Side,
    #[debug("{{ filled: {}, {} knots }}", bid.filled, bid.knots.len())]
    pub(super) bid: Side,
    /// What the custodian can pay out, base then quote: its balance less pending withdrawals.
    pub(super) available: [U256; 2],
    pub(super) fee_pips: u32,
}

impl BaibaiState {
    /// Fails for a curve the contract would never store or a fee it would never charge.
    pub(super) fn validated(self) -> Result<Self, SimulationError> {
        if self.mid == U256::ZERO || self.q_unit == U256::ZERO || self.c_unit == U256::ZERO {
            return Err(invalid("curve has no mid price or unit"));
        }
        if self.fee_pips >= FEE_DENOMINATOR {
            return Err(invalid(format!("fee of {} pips takes the whole output", self.fee_pips)));
        }
        self.curve(SwapDirection::BaseToQuote)?;
        self.curve(SwapDirection::QuoteToBase)?;
        Ok(self)
    }

    /// The side a swap in `direction` walks: selling base fills the bid, buying it the ask.
    fn curve(&self, direction: SwapDirection) -> Result<Curve, SimulationError> {
        let (side, asks) = match direction {
            SwapDirection::BaseToQuote => (&self.bid, false),
            SwapDirection::QuoteToBase => (&self.ask, true),
        };
        Curve::new(side, self.mid, self.q_unit, self.c_unit, asks)
    }

    /// Indices into `available` of the token a swap in `direction` pays in and the one it pays
    /// out.
    fn custody(direction: SwapDirection) -> (usize, usize) {
        match direction {
            SwapDirection::BaseToQuote => (0, 1),
            SwapDirection::QuoteToBase => (1, 0),
        }
    }

    /// The gross output less the entrypoint's fee, which the contract rounds up.
    fn after_fee(&self, gross: U256) -> Result<U256, SimulationError> {
        mul_div(gross, U256::from(FEE_DENOMINATOR - self.fee_pips), U256::from(FEE_DENOMINATOR))
    }

    fn sells(&self, direction: SwapDirection) -> bool {
        let (_, out) = Self::custody(direction);
        self.available[out] > U256::ZERO &&
            self.curve(direction)
                .and_then(|curve| curve.capacity())
                .is_ok_and(|capacity| capacity > U256::ZERO)
    }
}

impl PairState for BaibaiState {
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
            self.sells(SwapDirection::BaseToQuote),
            self.sells(SwapDirection::QuoteToBase),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for BaibaiState {
    fn fee(&self) -> f64 {
        f64::from(self.fee_pips) / f64::from(FEE_DENOMINATOR)
    }

    /// The marginal rate of selling `base` for `quote`, fee included.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &base.address,
            &quote.address,
        )?;
        let (_, out) = Self::custody(direction);
        if self.available[out] == U256::ZERO {
            return Err(invalid("no custody to pay out"));
        }
        let atomic = self
            .curve(direction)?
            .marginal_price()?;
        Ok(atomic * 10f64.powi(base.decimals as i32 - quote.decimals as i32) * (1.0 - self.fee()))
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
        let input = biguint_to_u256(&amount_in);
        let curve = self.curve(direction)?;
        let (gross, cursor) = curve.quote(input)?;
        let output = self.after_fee(gross)?;
        if output == U256::ZERO {
            return Err(invalid("input below the curve's atomic precision"));
        }
        let (paid_in, paid_out) = Self::custody(direction);
        if output > self.available[paid_out] {
            return Err(invalid(format!(
                "output {output} exceeds the custodian's available {}",
                self.available[paid_out]
            )));
        }
        let mut next = self.clone();
        match direction {
            SwapDirection::BaseToQuote => next.bid.filled = cursor,
            SwapDirection::QuoteToBase => next.ask.filled = cursor,
        }
        next.available[paid_in] = next.available[paid_in].saturating_add(input);
        next.available[paid_out] -= output;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(output),
            BigUint::from(BASE_SWAP_GAS + GAS_PER_KNOT_PASSED * curve.knots_below(cursor)),
            Box::new(next),
        ))
    }

    /// The largest input the curve fills and custody pays out, and its output. Zero both ways
    /// when nothing can be swapped in that direction.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction =
            SwapDirection::require(&self.base_token, &self.quote_token, &sell_token, &buy_token)?;
        let (paid_in, paid_out) = Self::custody(direction);
        let available = self.available[paid_out];
        if available == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let curve = self.curve(direction)?;
        let mut high = curve
            .capacity()?
            .min(U256::MAX - self.available[paid_in]);
        let full_output = self.after_fee(curve.quote(high)?.0)?;
        if full_output == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        if self.after_fee(curve.output_bound(high)?)? <= available {
            return Ok((u256_to_biguint(high), u256_to_biguint(full_output)));
        }
        // Custody binds before the curve does: find the largest input whose output it covers.
        let mut low = U256::ZERO;
        while low < high {
            let mid = low + (high - low).div_ceil(U256::from(2));
            if self.after_fee(curve.output_bound(mid)?)? <= available {
                low = mid;
            } else {
                high = mid - U256::from(1);
            }
        }
        let output = self.after_fee(curve.quote(low)?.0)?;
        if output == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        Ok((u256_to_biguint(low), u256_to_biguint(output)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError(
            "BaiBai state is read whole every poll and does not take deltas".into(),
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
    use rstest::rstest;

    use super::*;
    use crate::pamm::protocols::baibai::{
        math::Knot,
        test_support::{fixtures, state, token, weth_state},
        BASE_DEPLOYMENT,
    };

    fn quote() -> Token {
        token(BASE_DEPLOYMENT.quote)
    }

    /// Every recorded `quoteFor` for Tycho's router, both pairs and both directions, at blocks
    /// with every cursor at zero, the ask partly filled and the bid partly filled, priced wei for
    /// wei, including the sizes the entrypoint refused (recorded as zero output).
    #[test]
    fn quotes_match_the_entrypoint_wei_for_wei() {
        let mut priced = 0;
        for fixture in fixtures() {
            for pair in &fixture.pairs {
                let state = state(&fixture, pair);
                for case in &pair.cases {
                    let (token_in, token_out) = if case.sell_base {
                        (state.base_token.clone(), quote())
                    } else {
                        (quote(), state.base_token.clone())
                    };
                    let amount_in = u256_to_biguint(case.amount_in);
                    match state.get_amount_out(amount_in, &token_in, &token_out) {
                        Ok(result) => {
                            assert_eq!(result.amount, u256_to_biguint(case.amount_out), "{case:?}");
                            priced += 1;
                        }
                        Err(error) => {
                            assert_eq!(case.amount_out, U256::ZERO, "{case:?}: {error}")
                        }
                    }
                }
            }
        }
        assert_eq!(priced, 87, "the recordings' fillable cases");
    }

    /// The curve itself, without the fee, matches `CurveBook.quote` at every recorded block.
    #[test]
    fn a_feeless_state_matches_the_curve_book() {
        for fixture in fixtures() {
            for pair in &fixture.pairs {
                let mut state = state(&fixture, pair);
                state.fee_pips = 0;
                for case in pair
                    .cases
                    .iter()
                    .filter(|case| case.curve_out != U256::ZERO)
                {
                    let (token_in, token_out) = if case.sell_base {
                        (state.base_token.clone(), quote())
                    } else {
                        (quote(), state.base_token.clone())
                    };
                    let result = state
                        .get_amount_out(u256_to_biguint(case.amount_in), &token_in, &token_out)
                        .unwrap();
                    assert_eq!(result.amount, u256_to_biguint(case.curve_out), "{case:?}");
                }
            }
        }
    }

    /// The recording's router fee, 50 millionths.
    #[test]
    fn the_fee_is_in_millionths_of_the_output() {
        assert_eq!(weth_state().fee(), 0.00005);
    }

    #[test]
    fn a_swap_advances_the_cursor_and_moves_custody() {
        let state = weth_state();
        let weth = state.base_token.clone();
        let amount = BigUint::from(10u64).pow(17);

        let result = state
            .get_amount_out(amount.clone(), &weth, &quote())
            .unwrap();

        let next = result
            .new_state
            .as_any()
            .downcast_ref::<BaibaiState>()
            .unwrap();
        assert_eq!(next.bid.filled, U256::from(10u64).pow(U256::from(17)));
        assert_eq!(next.ask.filled, state.ask.filled);
        assert_eq!(
            u256_to_biguint(next.available[0]),
            u256_to_biguint(state.available[0]) + amount
        );
        assert_eq!(
            u256_to_biguint(next.available[1]),
            u256_to_biguint(state.available[1]) - &result.amount
        );
        // The same size again walks further along the bid and pays no more.
        let again = next
            .get_amount_out(BigUint::from(10u64).pow(17), &weth, &quote())
            .unwrap();
        assert!(again.amount <= result.amount);
    }

    /// A swap costs a base amount plus a share per knot it walks past. The recording's WETH bid
    /// has knots at about 0.43, 1.08, 3.25, 5.42 and 12.24 WETH; the fork measured 111,330 gas
    /// selling 0.2 WETH and 114,384 selling 2 WETH.
    #[rstest]
    #[case::within_the_first_segment(2u64 * 10u64.pow(17), 112_000)]
    #[case::past_two_knots(2u64 * 10u64.pow(18), 118_000)]
    fn gas_grows_with_the_knots_a_swap_passes(#[case] amount: u64, #[case] expected: u64) {
        let state = weth_state();
        let weth = state.base_token.clone();

        let gas = state
            .get_amount_out(BigUint::from(amount), &weth, &quote())
            .unwrap()
            .gas;

        assert_eq!(gas, BigUint::from(expected));
    }

    #[test]
    fn output_beyond_custody_is_refused() {
        let mut state = weth_state();
        state.available[1] = U256::from(1_000_000);

        let error = state
            .get_amount_out(BigUint::from(10u64).pow(17), &state.base_token.clone(), &quote())
            .unwrap_err();

        assert!(error.to_string().contains("available"), "{error}");
    }

    #[rstest]
    #[case::zero_amount(0u64, true)]
    #[case::unrelated_tokens(1_000u64, false)]
    fn invalid_requests_are_refused(#[case] amount: u64, #[case] own_tokens: bool) {
        let state = weth_state();
        let token_in = if own_tokens {
            state.base_token.clone()
        } else {
            token(alloy::primitives::Address::repeat_byte(0x11))
        };

        let error = state
            .get_amount_out(BigUint::from(amount), &token_in, &quote())
            .unwrap_err();

        assert!(matches!(error, SimulationError::InvalidInput(..)), "{error}");
    }

    /// The limits are a swap the state fills, and the next atom past a curve-bound limit is not.
    #[rstest]
    #[case::sell_base(true)]
    #[case::buy_base(false)]
    fn the_limits_are_fillable_and_tight(#[case] sell_base: bool) {
        let state = weth_state();
        let (token_in, token_out) = if sell_base {
            (state.base_token.clone(), quote())
        } else {
            (quote(), state.base_token.clone())
        };

        let (max_in, max_out) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();

        let at_limit = state
            .get_amount_out(max_in.clone(), &token_in, &token_out)
            .unwrap();
        assert_eq!(at_limit.amount, max_out);
        assert!(state
            .get_amount_out(max_in + 1u32, &token_in, &token_out)
            .is_err());
    }

    #[test]
    fn custody_binds_the_limits_when_it_runs_out_first() {
        let mut state = weth_state();
        state.available[1] = U256::from(500_000_000u64);
        let weth = state.base_token.clone();

        let (max_in, max_out) = state
            .get_limits(weth.address.clone(), quote().address)
            .unwrap();

        assert!(max_out <= BigUint::from(500_000_000u64));
        assert!(state
            .get_amount_out(max_in.clone(), &weth, &quote())
            .is_ok());
        assert!(state
            .get_amount_out(max_in + 10u64.pow(12), &weth, &quote())
            .is_err());
    }

    #[test]
    fn an_empty_custody_trades_neither_way() {
        let mut state = weth_state();
        state.available = [U256::ZERO, U256::ZERO];

        assert!(state.directions().is_empty());
        let limits = state
            .get_limits(state.base_token.address.clone(), quote().address)
            .unwrap();
        assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO));
        assert!(state
            .spot_price(&state.base_token.clone(), &quote())
            .is_err());
    }

    /// Selling base pays the bid, buying it costs the ask: the two spot prices bracket the mid.
    #[test]
    fn spot_prices_bracket_the_mid() {
        let state = weth_state();
        let weth = state.base_token.clone();
        let mid = 2_307.8;

        let bid = state
            .spot_price(&weth, &quote())
            .unwrap();
        let ask = 1.0 /
            state
                .spot_price(&quote(), &weth)
                .unwrap();

        assert!(bid < ask, "bid {bid} ask {ask}");
        assert!((bid - mid).abs() / mid < 0.01, "bid {bid}");
        assert!((ask - mid).abs() / mid < 0.01, "ask {ask}");
    }

    #[rstest]
    #[case::fee_takes_everything(|s: &mut BaibaiState| s.fee_pips = 1_000_000)]
    #[case::no_mid(|s: &mut BaibaiState| s.mid = U256::ZERO)]
    #[case::too_many_knots(|s: &mut BaibaiState| s.ask.knots = vec![Knot { q: 1, extra: 0 }; 37])]
    #[case::falling_cost(|s: &mut BaibaiState| s.bid.knots = vec![Knot { q: 1, extra: 5 }, Knot { q: 2, extra: 4 }])]
    #[case::repeated_quantity(|s: &mut BaibaiState| s.bid.knots = vec![Knot { q: 1, extra: 0 }, Knot { q: 1, extra: 1 }])]
    #[case::depth_over_100_percent(|s: &mut BaibaiState| s.ask.depth_bps = 10_001)]
    fn malformed_states_are_rejected(#[case] corrupt: fn(&mut BaibaiState)) {
        let mut state = weth_state();
        corrupt(&mut state);

        assert!(state.validated().is_err());
    }

    #[test]
    fn a_state_round_trips_through_serde() {
        let state: Box<dyn ProtocolSim> = Box::new(weth_state());

        let json = serde_json::to_string(&state).unwrap();
        let back: Box<dyn ProtocolSim> = serde_json::from_str(&json).unwrap();

        assert!(back.eq(state.as_ref()));
    }
}
