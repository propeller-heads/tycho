use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use evm_ekubo_sdk::{
    math::{
        tick::{MAX_SQRT_RATIO, MIN_SQRT_RATIO},
        uint::U256,
    },
    quoting::types::{NodeKey, TokenAmount},
};
use num_bigint::BigUint;
use num_traits::One;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, Price, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use super::pool::{
    base::BasePool, full_range::FullRangePool, oracle::OraclePool, twamm::TwammPool, EkuboPool,
};
use crate::evm::protocol::{
    ekubo::pool::mev_resist::MevResistPool, u256_num::u256_to_f64, utils::add_fee_markup,
};

#[enum_delegate::implement(EkuboPool)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EkuboState {
    Base(BasePool),
    FullRange(FullRangePool),
    Oracle(OraclePool),
    Twamm(TwammPool),
    MevResist(MevResistPool),
}

fn sqrt_price_q128_to_f64(
    x: U256,
    (token0_decimals, token1_decimals): (usize, usize),
) -> Result<f64, SimulationError> {
    let token_correction = 10f64.powi(token0_decimals as i32 - token1_decimals as i32);

    let price = u256_to_f64(alloy::primitives::U256::from_limbs(x.0))? / 2.0f64.powi(128);
    Ok(price.powi(2) * token_correction)
}

/// Converts a target pool price (`token_out`/`token_in` amounts in raw atomic units) into the
/// equivalent Q128.128 sqrt ratio, clamped to the valid sqrt ratio range.
///
/// [`ProtocolSim::spot_price`] applies a fee markup of `1 / (1 - fee)` (with `fee` in 0.64 fixed
/// point), so the raw pool price is derived from `target * (1 - fee)` to make the spot price of a
/// pool at the returned sqrt ratio match the target.
fn target_sqrt_ratio(target: &Price, zero_for_one: bool, fee: u64) -> U256 {
    let fee_factor = (BigUint::one() << 64usize) - fee;
    let (price1, price0) = if zero_for_one {
        (&target.numerator * fee_factor, &target.denominator << 64usize)
    } else {
        (&target.denominator << 64usize, &target.numerator * fee_factor)
    };

    // sqrt(price1 / price0) * 2^128 == floor(sqrt(price1 * 2^256 / price0))
    let sqrt_ratio = ((price1 << 256usize) / price0).sqrt();

    let bytes = sqrt_ratio.to_bytes_be();
    let sqrt_ratio = if bytes.len() > 32 { MAX_SQRT_RATIO } else { U256::from_big_endian(&bytes) };

    sqrt_ratio.clamp(MIN_SQRT_RATIO, MAX_SQRT_RATIO)
}

#[typetag::serde]
impl ProtocolSim for EkuboState {
    fn fee(&self) -> f64 {
        self.key().config.fee as f64 / (2f64.powi(64))
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let sqrt_ratio = self.sqrt_ratio();
        let (base_decimals, quote_decimals) = (base.decimals as usize, quote.decimals as usize);

        let price = if base < quote {
            sqrt_price_q128_to_f64(sqrt_ratio, (base_decimals, quote_decimals))?
        } else {
            1.0f64 / sqrt_price_q128_to_f64(sqrt_ratio, (quote_decimals, base_decimals))?
        };
        Ok(add_fee_markup(price, self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        _token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let token_amount = TokenAmount {
            token: U256::from_big_endian(&token_in.address),
            amount: amount_in.try_into().map_err(|_| {
                SimulationError::InvalidInput("amount in must fit into a i128".to_string(), None)
            })?,
        };

        let quote = self.quote(token_amount, None)?;

        if quote.calculated_amount > i128::MAX as u128 {
            return Err(SimulationError::RecoverableError(
                "calculated amount exceeds i128::MAX".to_string(),
            ));
        }

        let res = GetAmountOutResult {
            amount: BigUint::from(quote.calculated_amount),
            gas: quote.gas.into(),
            new_state: Box::new(quote.new_state),
        };

        if quote.consumed_amount != token_amount.amount {
            return Err(SimulationError::InvalidInput(
                format!("pool does not have enough liquidity to support complete swap. input amount: {input_amount}, consumed amount: {consumed_amount}", input_amount = token_amount.amount, consumed_amount = quote.consumed_amount),
                Some(res),
            ));
        }

        Ok(res)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(liquidity) = delta
            .updated_attributes
            .get("liquidity")
        {
            self.set_liquidity(liquidity.clone().into());
        }

        if let Some(sqrt_price) = delta
            .updated_attributes
            .get("sqrt_ratio")
        {
            self.set_sqrt_ratio(U256::from_big_endian(sqrt_price));
        }

        self.finish_transition(delta.updated_attributes, delta.deleted_attributes)
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
            .downcast_ref::<EkuboState>()
            .is_some_and(|other_state| self == other_state)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        _buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let consumed_amount = self.get_limit(U256::from_big_endian(&sell_token))?;

        // TODO Update once exact out is supported
        Ok((
            BigUint::try_from(consumed_amount).map_err(|_| {
                SimulationError::FatalError(format!(
                    "Failed to convert consumed amount `{consumed_amount}` into BigUint"
                ))
            })?,
            BigUint::ZERO,
        ))
    }

    /// See [`ProtocolSim::query_pool_swap`] for the trait documentation.
    ///
    /// For [`SwapConstraint::PoolTargetPrice`] the target price is converted into a sqrt ratio
    /// limit and an "infinite" amount is swapped with that limit, so the swap stops exactly at
    /// the target price. [`SwapConstraint::TradeLimitPrice`] falls back to the numerical search.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
            SwapConstraint::PoolTargetPrice {
                target,
                tolerance: _,
                min_amount_in: _,
                max_amount_in: _,
            } => {
                let token_in = U256::from_big_endian(&params.token_in().address);
                let zero_for_one = token_in == self.key().token0;

                let sqrt_ratio_limit =
                    target_sqrt_ratio(target, zero_for_one, self.key().config.fee);
                let sqrt_ratio = self.sqrt_ratio();

                let target_unreachable = if zero_for_one {
                    sqrt_ratio_limit >= sqrt_ratio
                } else {
                    sqrt_ratio_limit <= sqrt_ratio
                };
                if target_unreachable {
                    return Err(SimulationError::InvalidInput(
                        "Target price is unreachable (already below current spot price)"
                            .to_string(),
                        None,
                    ));
                }

                let quote = match self.quote(
                    TokenAmount { token: token_in, amount: i128::MAX },
                    Some(sqrt_ratio_limit),
                ) {
                    Ok(quote) => quote,
                    // Time-dependent pools (e.g. TWAMM) execute virtual orders inside the quote
                    // using an estimated block timestamp, which can move the price past the
                    // target before any input is consumed. The SDK reports this as an invalid
                    // sqrt ratio limit (surfaced here as a stringified error), so treat it as a
                    // zero-amount swap.
                    Err(SimulationError::RecoverableError(msg))
                        if msg.contains("InvalidSqrtRatioLimit") =>
                    {
                        return Ok(PoolSwap::new(
                            BigUint::ZERO,
                            BigUint::ZERO,
                            self.clone_box(),
                            None,
                        ));
                    }
                    Err(err) => return Err(err),
                };

                if quote.consumed_amount == i128::MAX {
                    return Err(SimulationError::InvalidInput(
                        "Amount required to reach the target price exceeds i128::MAX".to_string(),
                        None,
                    ));
                }

                let amount_in = BigUint::try_from(quote.consumed_amount).map_err(|_| {
                    SimulationError::FatalError(format!(
                        "Negative consumed amount `{}` in target price quote",
                        quote.consumed_amount
                    ))
                })?;
                if amount_in == BigUint::ZERO {
                    return Ok(PoolSwap::new(BigUint::ZERO, BigUint::ZERO, self.clone_box(), None));
                }

                Ok(PoolSwap::new(
                    amount_in,
                    BigUint::from(quote.calculated_amount),
                    Box::new(quote.new_state),
                    None,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use evm_ekubo_sdk::{
        math::{tick::MIN_SQRT_RATIO, uint::U256},
        quoting::{
            full_range_pool::FullRangePoolState,
            types::{Config, NodeKey, Tick},
        },
    };
    use num_traits::ToPrimitive as _;
    use rstest::*;
    use rstest_reuse::apply;
    use tycho_common::models::Chain;

    use super::*;
    use crate::evm::protocol::ekubo::{pool::base::BasePool, test_cases::*};

    const TOKEN0_ADDRESS: U256 = U256([1, 0, 0, 0]);
    const TOKEN1_ADDRESS: U256 = U256([2, 0, 0, 0]);

    // Wide enough (~±e^1 in price) to allow multi-percent price moves in the comparison tests
    const WIDE_TICK: i32 = 1_000_000;
    const WIDE_LIQUIDITY: u128 = 10_000_000_000;

    // Price 1.0 between token0 and token1
    const SQRT_RATIO_ONE: U256 = U256([0, 0, 1, 0]);

    fn test_token(address: U256, symbol: &str) -> Token {
        Token {
            address: address.to_big_endian().into(),
            decimals: 18,
            symbol: symbol.to_string(),
            gas: vec![Some(0)],
            chain: Chain::Ethereum,
            tax: 0,
            quality: 100,
        }
    }

    fn tokens() -> (Token, Token) {
        (test_token(TOKEN0_ADDRESS, "TOKEN0"), test_token(TOKEN1_ADDRESS, "TOKEN1"))
    }

    /// Tokens matching the pool's SDK key. Some SDK pools rewrite the key (e.g. oracle pools
    /// force `token0` to the native token), so tests must use the key's addresses.
    fn pool_tokens(state: &EkuboState) -> (Token, Token) {
        let key = state.key();
        (test_token(key.token0, "TOKEN0"), test_token(key.token1, "TOKEN1"))
    }

    /// Spot price after a 1 wei probe swap, so that time-dependent pools (TWAMM) report the
    /// price after virtual order execution, which is the price a target price swap acts on.
    fn probe_spot(state: &EkuboState, token_in: &Token, token_out: &Token) -> f64 {
        state
            .get_amount_out(BigUint::from(1u8), token_in, token_out)
            .expect("probing spot price")
            .new_state
            .spot_price(token_in, token_out)
            .expect("computing spot price")
    }

    fn to_price(price_f64: f64, token_in: &Token, token_out: &Token) -> Price {
        let decimal_adj = 10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
        let price_no_decimals = price_f64 / decimal_adj;
        Price::new(BigUint::from((price_no_decimals * 1e18) as u128), BigUint::from(10u128.pow(18)))
    }

    fn target_price_params(
        token_in: &Token,
        token_out: &Token,
        target_f64: f64,
        tolerance: f64,
    ) -> QueryPoolSwapParams {
        QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target: to_price(target_f64, token_in, token_out),
                tolerance,
                min_amount_in: None,
                max_amount_in: None,
            },
        )
    }

    fn wide_ticks() -> Vec<Tick> {
        vec![
            Tick { index: -WIDE_TICK, liquidity_delta: WIDE_LIQUIDITY as i128 },
            Tick { index: WIDE_TICK, liquidity_delta: -(WIDE_LIQUIDITY as i128) },
        ]
    }

    fn wide_base(fee: u64) -> EkuboState {
        let key = NodeKey {
            token0: TOKEN0_ADDRESS,
            token1: TOKEN1_ADDRESS,
            config: Config { fee, tick_spacing: 10, extension: U256::zero() },
        };

        EkuboState::Base(
            BasePool::new(key, wide_ticks(), SQRT_RATIO_ONE, WIDE_LIQUIDITY, 0).unwrap(),
        )
    }

    fn wide_mev_resist(fee: u64) -> EkuboState {
        let key = NodeKey {
            token0: TOKEN0_ADDRESS,
            token1: TOKEN1_ADDRESS,
            config: Config { fee, tick_spacing: 10, extension: U256::one() },
        };

        EkuboState::MevResist(
            MevResistPool::new(key, wide_ticks(), SQRT_RATIO_ONE, WIDE_LIQUIDITY, 0).unwrap(),
        )
    }

    #[apply(all_cases)]
    fn test_delta_transition(case: TestCase) {
        let mut state = case.state_before_transition;

        state
            .delta_transition(
                ProtocolStateDelta {
                    updated_attributes: case.transition_attributes,
                    ..Default::default()
                },
                &HashMap::default(),
                &Balances::default(),
            )
            .expect("executing transition");

        assert_eq!(state, case.state_after_transition);
    }

    #[apply(all_cases)]
    fn test_get_amount_out(case: TestCase) {
        let (token0, token1) = (case.token0(), case.token1());
        let (amount_in, expected_out) = case.swap_token0;

        let res = case
            .state_after_transition
            .get_amount_out(amount_in, &token0, &token1)
            .expect("computing quote");

        assert_eq!(res.amount, expected_out);
    }

    #[apply(all_cases)]
    fn test_get_limits(case: TestCase) {
        use std::ops::Deref;

        let (token0, token1) = (case.token0(), case.token1());
        let state = case.state_after_transition;

        let max_amount_in = state
            .get_limits(token0.address.deref().into(), token1.address.deref().into())
            .expect("computing limits for token0")
            .0;

        assert_eq!(max_amount_in, case.expected_limit_token0);

        state
            .get_amount_out(max_amount_in, &token0, &token1)
            .expect("quoting with limit");
    }

    #[test]
    fn test_get_limits_negative_consumed_amount() {
        // Reproduces an issue where get_limit was returning a negative value which then failed
        // when converting to BigUint in get_limits. This happened for pools with depleted liquidity
        // for the current price.
        let eth_address = U256::zero();
        let usdt_address_bytes =
            hex::decode("dac17f958d2ee523a2206206994597c13d831ec7").expect("valid hex");
        let usdt_address = U256::from_big_endian(&usdt_address_bytes);

        let pool_key = NodeKey {
            token0: eth_address,
            token1: usdt_address,
            config: Config { fee: 0, tick_spacing: 1000, extension: U256::zero() },
        };

        // Create a pool with single tick of minimal liquidity
        // positioned such that one direction has effectively no liquidity
        let state = EkuboState::Base(
            BasePool::new(
                pool_key,
                vec![
                    Tick { index: 1000, liquidity_delta: 1 },
                    Tick { index: 2000, liquidity_delta: -1 },
                ],
                MIN_SQRT_RATIO, // Minimum valid price (all liquidity is above current price)
                0,              // No liquidity at current price.
                -887272,        // MIN_TICK (corresponding to MIN_SQRT_RATIO)
            )
            .unwrap(),
        );

        let (limit, _) = state
            .get_limits(
                pool_key.token0.to_big_endian().into(),
                pool_key.token1.to_big_endian().into(),
            )
            .unwrap();

        // Limit should be 0 for pool with no liquidity at current price
        assert_eq!(limit, BigUint::ZERO);
    }

    #[rstest]
    #[case::zero_fee_sell_token0(0, 0.987, true)]
    #[case::zero_fee_sell_token1(0, 1.31, false)]
    #[case::with_fee_sell_token0(u64::MAX / 100, 0.987, true)]
    #[case::with_fee_sell_token1(u64::MAX / 100, 1.31, false)]
    fn test_target_sqrt_ratio_roundtrips_through_spot_price(
        #[case] fee: u64,
        #[case] price_f64: f64,
        #[case] zero_for_one: bool,
    ) {
        let (token0, token1) = tokens();
        let (token_in, token_out) =
            if zero_for_one { (&token0, &token1) } else { (&token1, &token0) };

        let target = to_price(price_f64, token_in, token_out);
        let sqrt_ratio = target_sqrt_ratio(&target, zero_for_one, fee);

        let key = NodeKey {
            token0: TOKEN0_ADDRESS,
            token1: TOKEN1_ADDRESS,
            config: Config { fee, tick_spacing: 0, extension: U256::zero() },
        };
        let state = EkuboState::FullRange(
            FullRangePool::new(key, FullRangePoolState { sqrt_ratio, liquidity: 1 }).unwrap(),
        );

        let spot = state
            .spot_price(token_in, token_out)
            .expect("computing spot price");
        let rel_err = ((spot - price_f64) / price_f64).abs();
        assert!(rel_err < 1e-9, "spot {spot} deviates from target {price_f64} by {rel_err}");
    }

    #[rstest]
    #[case::base(wide_base(0), 0.999)]
    #[case::base_deep(wide_base(0), 0.95)]
    #[case::base_with_fee(wide_base(u64::MAX / 100), 0.99)]
    #[case::mev_resist(wide_mev_resist(u64::MAX / 10), 0.99)]
    fn test_query_pool_swap_target_price_matches_numerical(
        #[case] state: EkuboState,
        #[case] multiplier: f64,
    ) {
        let (token0, token1) = pool_tokens(&state);

        for (token_in, token_out) in [(&token0, &token1), (&token1, &token0)] {
            let ref_spot = probe_spot(&state, token_in, token_out);

            let tolerance = (1.0 - multiplier) / 1e3;
            let target_f64 = ref_spot * multiplier;
            let params = target_price_params(token_in, token_out, target_f64, tolerance);

            let native = state
                .query_pool_swap(&params)
                .expect("native query_pool_swap");
            let numerical = crate::evm::query_pool_swap::query_pool_swap(&state, &params)
                .expect("numerical query_pool_swap");

            let native_spot = native
                .new_state()
                .spot_price(token_in, token_out)
                .expect("native spot price");
            let native_err = ((native_spot - target_f64) / target_f64).abs();
            assert!(
                native_err <= tolerance,
                "native spot {native_spot} deviates from target {target_f64} by {native_err}"
            );

            let numerical_spot = numerical
                .new_state()
                .spot_price(token_in, token_out)
                .expect("numerical spot price");
            let numerical_err = ((numerical_spot - target_f64) / target_f64).abs();
            assert!(
                numerical_err <= tolerance,
                "numerical spot {numerical_spot} deviates from target {target_f64} by {numerical_err}"
            );

            let native_in = native.amount_in().to_f64().unwrap();
            let numerical_in = numerical.amount_in().to_f64().unwrap();
            // The numerical result is itself an approximation: a spot price within `tolerance`
            // of the target maps to an amount error of roughly `tolerance / (1 - multiplier)`.
            let amount_tolerance = 0.001f64.max(2.0 * tolerance / (1.0 - multiplier));
            let amount_err = ((native_in - numerical_in) / numerical_in).abs();
            assert!(
                amount_err <= amount_tolerance,
                "native amount_in {native_in} deviates from numerical {numerical_in} by {amount_err}"
            );
        }
    }

    // The numerical search cannot converge on these pools: their swap limit (the search's upper
    // bracket) is ~1e27 while the searched amounts are ~1e5, which 30 bisection iterations
    // cannot close. Instead, verify the native result directly: the new state must be at the
    // target price, requoting the amount must reproduce the swap, and a slightly smaller amount
    // must not yet reach the target (minimality).
    #[rstest]
    #[case::full_range(full_range().state_after_transition, 0.999)]
    #[case::full_range_deep(full_range().state_after_transition, 0.95)]
    #[case::oracle(oracle().state_after_transition, 0.99)]
    #[case::twamm(twamm().state_after_transition, 0.99)]
    fn test_query_pool_swap_target_price_reaches_target(
        #[case] state: EkuboState,
        #[case] multiplier: f64,
    ) {
        let (token0, token1) = pool_tokens(&state);

        for (token_in, token_out) in [(&token0, &token1), (&token1, &token0)] {
            let ref_spot = probe_spot(&state, token_in, token_out);

            let tolerance = (1.0 - multiplier) / 1e3;
            let target_f64 = ref_spot * multiplier;
            let params = target_price_params(token_in, token_out, target_f64, tolerance);

            let native = state
                .query_pool_swap(&params)
                .expect("native query_pool_swap");
            assert!(native.amount_in() > &BigUint::ZERO);

            let native_spot = native
                .new_state()
                .spot_price(token_in, token_out)
                .expect("native spot price");
            let native_err = ((native_spot - target_f64) / target_f64).abs();
            assert!(
                native_err <= tolerance,
                "native spot {native_spot} deviates from target {target_f64} by {native_err}"
            );

            let requote = state
                .get_amount_out(native.amount_in().clone(), token_in, token_out)
                .expect("requoting native amount_in");
            // The final partial step rounds differently when swapping to a price limit than
            // when consuming an exact input amount, so allow a rounding-level difference.
            let out_diff = requote.amount.to_f64().unwrap() - native.amount_out().to_f64().unwrap();
            assert!(
                out_diff.abs() <= 2.0,
                "requoted amount_out {} deviates from native amount_out {}",
                requote.amount,
                native.amount_out()
            );

            let smaller_amount = native.amount_in() * 995u32 / 1000u32;
            let smaller_spot = state
                .get_amount_out(smaller_amount, token_in, token_out)
                .expect("quoting smaller amount")
                .new_state
                .spot_price(token_in, token_out)
                .expect("smaller amount spot price");
            assert!(
                smaller_spot > target_f64,
                "swapping 0.5% less input should not reach the target yet: {smaller_spot} vs {target_f64}"
            );
        }
    }

    #[apply(all_cases)]
    fn test_query_pool_swap_target_price_unreachable(case: TestCase) {
        let (token0, token1) = (case.token0(), case.token1());
        let state = case.state_after_transition;

        for (token_in, token_out) in [(&token0, &token1), (&token1, &token0)] {
            let spot = state
                .spot_price(token_in, token_out)
                .expect("computing spot price");
            let params = target_price_params(token_in, token_out, spot * 1.01, 1e-4);

            let res = state.query_pool_swap(&params);
            assert!(
                matches!(res, Err(SimulationError::InvalidInput(_, _))),
                "target above spot should be rejected, got {res:?}"
            );
        }
    }

    #[test]
    fn test_query_pool_swap_target_price_twamm_virtual_orders_past_target() {
        let case = twamm();
        let (token0, token1) = (case.token0(), case.token1());
        let state = case.state_after_transition;

        let pre_spot = state
            .spot_price(&token0, &token1)
            .expect("computing spot price");
        let post_spot = state
            .get_amount_out(BigUint::from(1u8), &token0, &token1)
            .expect("probing spot price")
            .new_state
            .spot_price(&token0, &token1)
            .expect("computing post-virtual-orders spot price");
        assert!(post_spot < pre_spot, "virtual orders should move the price down");

        // Reachable at query time, but crossed by virtual order execution alone
        let target_f64 = (pre_spot * post_spot).sqrt();
        let params = target_price_params(&token0, &token1, target_f64, 1e-4);

        let swap = state
            .query_pool_swap(&params)
            .expect("native query_pool_swap");
        assert_eq!(swap.amount_in(), &BigUint::ZERO);
        assert_eq!(swap.amount_out(), &BigUint::ZERO);
    }
}
