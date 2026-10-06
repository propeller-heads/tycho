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
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use super::pool::{
    base::BasePool, full_range::FullRangePool, oracle::OraclePool, twamm::TwammPool, EkuboPool,
};
use crate::evm::protocol::{
    ekubo::pool::mev_resist::MevResistPool,
    ekubo_common::{swap_to_target_price, EkuboSwapToPrice},
    u256_num::u256_to_f64,
    utils::add_fee_markup,
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

impl EkuboSwapToPrice for EkuboState {
    type SqrtRatio = U256;

    fn sqrt_ratio_in_range(sqrt_ratio: &BigUint) -> Option<U256> {
        if sqrt_ratio.bits() > 256 {
            return None;
        }
        let sqrt_ratio = U256::from_big_endian(&sqrt_ratio.to_bytes_be());
        (MIN_SQRT_RATIO..=MAX_SQRT_RATIO)
            .contains(&sqrt_ratio)
            .then_some(sqrt_ratio)
    }

    fn current_sqrt_ratio(&self) -> U256 {
        self.sqrt_ratio()
    }

    fn quote_to_limit(
        &self,
        token_in: &Token,
        amount: i128,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<(i128, u128, Self), SimulationError> {
        let token_amount = TokenAmount { token: U256::from_big_endian(&token_in.address), amount };
        let quote = self.quote(token_amount, sqrt_ratio_limit)?;
        Ok((quote.consumed_amount, quote.calculated_amount, quote.new_state))
    }
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

    /// Solves [`SwapConstraint::PoolTargetPrice`] natively with a sqrt ratio limit. This path
    /// ignores `min_amount_in`, `max_amount_in` and `tolerance`, and returns no price points.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => {
                crate::evm::query_pool_swap::query_pool_swap(self, params)
            }
            SwapConstraint::PoolTargetPrice { target, .. } => {
                swap_to_target_price(self, params, target, self.key().config.fee)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use evm_ekubo_sdk::{
        math::{tick::MIN_SQRT_RATIO, uint::U256},
        quoting::types::{Config, NodeKey, Tick},
    };
    use rstest::*;
    use rstest_reuse::apply;

    use super::*;
    use crate::evm::protocol::{
        ekubo::{pool::base::BasePool, test_cases::*},
        ekubo_common::test_helpers::*,
    };

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
    #[case::full_range(full_range(), 0.95)]
    #[case::mev_resist_with_fee(mev_resist(), 0.999_995)]
    #[case::oracle(oracle(), 0.99)]
    #[case::twamm(twamm(), 0.99)]
    fn test_query_pool_swap_target_price_lands_in_band(
        #[case] case: TestCase,
        #[case] multiplier: f64,
    ) {
        assert_lands_in_band(
            &case.state_after_transition,
            &case.token0(),
            &case.token1(),
            multiplier,
        );
    }

    #[rstest]
    fn test_query_pool_swap_target_price_above_spot(full_range: TestCase) {
        let state = &full_range.state_after_transition;
        assert_target_above_spot_rejected(state, &full_range.token0(), &full_range.token1());
    }

    #[rstest]
    fn test_query_pool_swap_target_price_at_spot(full_range: TestCase) {
        let state = &full_range.state_after_transition;
        assert_target_at_spot_gives_zero_swap(state, &full_range.token0(), &full_range.token1());
    }

    #[rstest]
    fn test_query_pool_swap_target_price_out_of_range(full_range: TestCase) {
        let state = &full_range.state_after_transition;
        assert_out_of_range_falls_back(state, &full_range.token0(), &full_range.token1());
    }

    #[rstest]
    fn test_query_pool_swap_target_price_empty_pool(full_range: TestCase) {
        let state = empty_full_range_state();
        assert_missed_limit_falls_back(&state, &full_range.token0(), &full_range.token1());
    }

    #[test]
    fn test_query_pool_swap_target_price_virtual_orders_past_target() {
        let case = twamm();
        let state = &case.state_after_transition;
        assert_virtual_orders_applied_before_direction_check(state, &case.token0(), &case.token1());
    }
}
