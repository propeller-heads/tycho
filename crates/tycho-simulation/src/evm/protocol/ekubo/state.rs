use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use evm_ekubo_sdk::{
    math::uint::U256,
    quoting::types::{NodeKey, TokenAmount},
};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
        swap::SimulationResult,
    },
    Bytes,
};

use super::pool::{
    base::BasePool, full_range::FullRangePool, oracle::OraclePool, twamm::TwammPool, EkuboPool,
};
use crate::evm::protocol::{
    ekubo::pool::mev_resist::MevResistPool,
    swap_quoter::{impl_native_swap_quoter, AttachedComponent, NativeQuote},
    u256_num::u256_to_f64,
    utils::add_fee_markup,
};

#[enum_delegate::implement(EkuboPool)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EkuboPoolState {
    Base(BasePool),
    FullRange(FullRangePool),
    Oracle(OraclePool),
    Twamm(TwammPool),
    MevResist(MevResistPool),
}

/// An Ekubo V2 pool and, once the decoder attached it, the pool's component.
///
/// The component is the same for every pool variant, so it lives here instead of in each of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EkuboState {
    pool: EkuboPoolState,
    #[serde(skip)]
    component: AttachedComponent,
}

impl From<EkuboPoolState> for EkuboState {
    fn from(pool: EkuboPoolState) -> Self {
        Self { pool, component: AttachedComponent::default() }
    }
}

impl EkuboState {
    /// This state with `component` attached, so it quotes through [`SwapQuoter`].
    pub(crate) fn with_component(mut self, component: AttachedComponent) -> Self {
        self.component = component;
        self
    }

    fn wrap(&self, pool: EkuboPoolState) -> Self {
        Self { pool, component: self.component.clone() }
    }
}

fn sqrt_price_q128_to_f64(
    x: U256,
    (token0_decimals, token1_decimals): (usize, usize),
) -> Result<f64, SimulationError> {
    let token_correction = 10f64.powi(token0_decimals as i32 - token1_decimals as i32);

    let price = u256_to_f64(alloy::primitives::U256::from_limbs(x.0))? / 2.0f64.powi(128);
    Ok(price.powi(2) * token_correction)
}

#[typetag::serde]
impl ProtocolSim for EkuboState {
    fn fee(&self) -> f64 {
        self.pool.key().config.fee as f64 / (2f64.powi(64))
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let sqrt_ratio = self.pool.sqrt_ratio();
        let (base_decimals, quote_decimals) = (base.decimals as usize, quote.decimals as usize);

        let price = if base < quote {
            sqrt_price_q128_to_f64(sqrt_ratio, (base_decimals, quote_decimals))?
        } else {
            1.0f64 / sqrt_price_q128_to_f64(sqrt_ratio, (quote_decimals, base_decimals))?
        };
        Ok(add_fee_markup(price, ProtocolSim::fee(self)))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let (amount_out, gas, new_state) =
            self.quote_exact_in(&amount_in, &token_in.address, &token_out.address, true)?;
        let new_state = new_state.expect("quote_exact_in builds the state it is asked for");
        Ok(GetAmountOutResult::new(amount_out, gas, Box::new(new_state)))
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
            self.pool
                .set_liquidity(liquidity.clone().into());
        }

        if let Some(sqrt_price) = delta
            .updated_attributes
            .get("sqrt_ratio")
        {
            self.pool
                .set_sqrt_ratio(U256::from_big_endian(sqrt_price));
        }

        self.pool
            .finish_transition(delta.updated_attributes, delta.deleted_attributes)
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
        let consumed_amount = self
            .pool
            .get_limit(U256::from_big_endian(&sell_token))?;

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

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }

    fn as_swap_quoter(&self) -> Option<&dyn tycho_common::simulation::swap::SwapQuoter> {
        self.component
            .is_attached()
            .then_some(self as &dyn tycho_common::simulation::swap::SwapQuoter)
    }
}

impl NativeQuote for EkuboState {
    fn attached_component(&self) -> &AttachedComponent {
        &self.component
    }

    fn swap_fee(&self, _zero_for_one: bool) -> f64 {
        ProtocolSim::fee(self)
    }

    fn quote_exact_in(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        _token_out: &Bytes,
        with_state: bool,
    ) -> SimulationResult<(BigUint, BigUint, Option<Self>)> {
        let token_amount = TokenAmount {
            token: U256::from_big_endian(token_in),
            amount: amount_in
                .clone()
                .try_into()
                .map_err(|_| {
                    SimulationError::InvalidInput(
                        "amount in must fit into a i128".to_string(),
                        None,
                    )
                })?,
        };

        let quote = self.pool.quote(token_amount)?;

        if quote.calculated_amount > i128::MAX as u128 {
            return Err(SimulationError::RecoverableError(
                "calculated amount exceeds i128::MAX".to_string(),
            ));
        }

        let new_state = self.wrap(quote.new_state);

        if quote.consumed_amount != token_amount.amount {
            let partial = GetAmountOutResult {
                amount: BigUint::from(quote.calculated_amount),
                gas: quote.gas.into(),
                new_state: Box::new(new_state),
            };
            return Err(SimulationError::InvalidInput(
                format!("pool does not have enough liquidity to support complete swap. input amount: {input_amount}, consumed amount: {consumed_amount}", input_amount = token_amount.amount, consumed_amount = quote.consumed_amount),
                Some(partial),
            ));
        }

        Ok((
            BigUint::from(quote.calculated_amount),
            quote.gas.into(),
            with_state.then_some(new_state),
        ))
    }
}

impl_native_swap_quoter!(EkuboState);

#[cfg(test)]
mod tests {
    use evm_ekubo_sdk::{
        math::{tick::MIN_SQRT_RATIO, uint::U256},
        quoting::types::{Config, NodeKey, Tick},
    };
    use rstest::*;
    use rstest_reuse::apply;
    use tycho_common::simulation::swap::QuoteParams;

    use super::*;
    use crate::evm::protocol::{
        ekubo::{pool::base::BasePool, test_cases::*},
        swap_quoter::tests::{self, assert_delta_transition_matches, assert_quoter_matches},
    };

    #[apply(all_cases)]
    fn test_delta_transition(case: TestCase) {
        let mut state = EkuboState::from(case.state_before_transition);

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

        assert_eq!(state, EkuboState::from(case.state_after_transition));
    }

    #[apply(all_cases)]
    fn test_get_amount_out(case: TestCase) {
        let (token0, token1) = (case.token0(), case.token1());
        let (amount_in, expected_out) = case.swap_token0;

        let res = EkuboState::from(case.state_after_transition)
            .get_amount_out(amount_in, &token0, &token1)
            .expect("computing quote");

        assert_eq!(res.amount, expected_out);
    }

    #[apply(all_cases)]
    fn test_get_limits(case: TestCase) {
        use std::ops::Deref;

        let (token0, token1) = (case.token0(), case.token1());
        let state = EkuboState::from(case.state_after_transition);

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
        let state = EkuboState::from(EkuboPoolState::Base(
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
        ));

        let (limit, _) = state
            .get_limits(
                pool_key.token0.to_big_endian().into(),
                pool_key.token1.to_big_endian().into(),
            )
            .unwrap();

        // Limit should be 0 for pool with no liquidity at current price
        assert_eq!(limit, BigUint::ZERO);
    }

    #[apply(all_cases)]
    fn test_swap_quoter_matches_protocol_sim(case: TestCase) {
        let (token0, token1) = (case.token0(), case.token1());
        let component = tests::component(&token0, &token1);
        let limit = case.expected_limit_token0.clone();
        let amounts = [
            BigUint::from(1u8),
            case.swap_token0.0.clone(),
            limit.clone(),
            limit * 2u8 + 1u8,
            BigUint::from(10u128.pow(30)),
        ];
        let state = EkuboState::from(case.state_after_transition).with_component(component);

        assert_quoter_matches(&state, &token0, &token1, &amounts);
        let fee = state
            .as_swap_quoter()
            .expect("component is attached")
            .fee(QuoteParams::fixed_in(&token0.address, &token1.address, BigUint::ZERO).unwrap())
            .unwrap();
        assert_eq!(fee.fee(), ProtocolSim::fee(&state));
    }

    #[apply(all_cases)]
    fn test_swap_quoter_delta_transition_matches_protocol_sim(case: TestCase) {
        let (token0, token1) = (case.token0(), case.token1());
        let state = EkuboState::from(case.state_before_transition)
            .with_component(tests::component(&token0, &token1));
        let delta = ProtocolStateDelta {
            updated_attributes: case.transition_attributes,
            ..Default::default()
        };

        assert_delta_transition_matches(&state, delta);
    }

    #[test]
    fn test_swap_quoter_partial_fill_is_reported_like_protocol_sim() {
        let case = base();
        let (token0, token1) = (case.token0(), case.token1());
        let state = EkuboState::from(case.state_after_transition)
            .with_component(tests::component(&token0, &token1));
        let amount = case.expected_limit_token0 * 2u8 + 1u8;

        let quoter = state
            .as_swap_quoter()
            .expect("component is attached");
        let Err(error) = quoter.quote(
            QuoteParams::fixed_in(&token0.address, &token1.address, amount.clone())
                .expect("valid params"),
        ) else {
            panic!("amount exceeds the pool limit");
        };

        assert!(matches!(error, SimulationError::InvalidInput(_, Some(_))));
        let Err(expected) = state.get_amount_out(amount, &token0, &token1) else {
            panic!("amount exceeds the pool limit");
        };
        assert_eq!(error.to_string(), expected.to_string());
    }
}
