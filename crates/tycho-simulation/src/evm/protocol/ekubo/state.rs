use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
};

use alloy::primitives::U256 as RuintU256;
use evm_ekubo_sdk::{
    math::uint::U256,
    quoting::types::{NodeKey, TokenAmount},
};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
        swap::{
            LimitsParams, MarginalPrice, MarginalPriceParams, QuerySwapParams, Quote, QuoteAmount,
            QuoteParams, Range, SimulationResult, Swap, SwapFee, SwapLimits, SwapQuoter,
            Transition, TransitionParams,
        },
    },
    Bytes,
};

use super::pool::{
    base::BasePool, full_range::FullRangePool, oracle::OraclePool, twamm::TwammPool, EkuboPool,
};
use crate::evm::protocol::{
    ekubo::pool::mev_resist::MevResistPool,
    u256_num::{biguint_to_u256 as biguint_to_ruint, u256_to_f64},
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

impl EkuboState {
    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        let slot = match &mut self {
            EkuboState::Base(pool) => &mut pool.component,
            EkuboState::FullRange(pool) => &mut pool.component,
            EkuboState::Oracle(pool) => &mut pool.component,
            EkuboState::Twamm(pool) => &mut pool.component,
            EkuboState::MevResist(pool) => &mut pool.component,
        };
        *slot = Some(component);
        self
    }

    fn component_ref(&self) -> Option<&Arc<ProtocolComponent<Arc<Token>>>> {
        match self {
            EkuboState::Base(pool) => pool.component.as_ref(),
            EkuboState::FullRange(pool) => pool.component.as_ref(),
            EkuboState::Oracle(pool) => pool.component.as_ref(),
            EkuboState::Twamm(pool) => pool.component.as_ref(),
            EkuboState::MevResist(pool) => pool.component.as_ref(),
        }
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
        Ok(add_fee_markup(price, ProtocolSim::fee(self)))
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

        let quote = EkuboPool::quote(self, token_amount)?;

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

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

#[typetag::serde]
impl SwapQuoter for EkuboState {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component_ref()
            .cloned()
            .ok_or_else(|| {
                SimulationError::FatalError(
                    "EkuboState: component not set (decode did not populate it)".to_string(),
                )
            })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Ok(SwapFee::new(self.key().config.fee as f64 / 2f64.powi(64)))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component_ref().ok_or_else(|| {
            SimulationError::FatalError("EkuboState: component not set".to_string())
        })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;

        let sqrt_ratio = self.sqrt_ratio();
        let (base_decimals, quote_decimals) =
            (base.decimals as usize, quote.decimals as usize);

        let price = if base.as_ref() < quote.as_ref() {
            sqrt_price_q128_to_f64(sqrt_ratio, (base_decimals, quote_decimals))?
        } else {
            1.0f64 / sqrt_price_q128_to_f64(sqrt_ratio, (quote_decimals, base_decimals))?
        };
        Ok(MarginalPrice::new(add_fee_markup(price, ProtocolSim::fee(self))))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "EkuboState does not yet support exact-out (FixedOut) quoting".to_string(),
                ))
            }
        };

        let token_amount = TokenAmount {
            token: U256::from_big_endian(params.token_in()),
            amount: i128::try_from(amount_in).map_err(|_| {
                SimulationError::InvalidInput("amount in must fit into a i128".to_string(), None)
            })?,
        };

        let quote = EkuboPool::quote(self, token_amount)?;

        if quote.calculated_amount > i128::MAX as u128 {
            return Err(SimulationError::RecoverableError(
                "calculated amount exceeds i128::MAX".to_string(),
            ));
        }
        if quote.consumed_amount != token_amount.amount {
            return Err(SimulationError::RecoverableError(format!(
                "pool does not have enough liquidity to support complete swap. input amount: {input_amount}, consumed amount: {consumed_amount}",
                input_amount = token_amount.amount,
                consumed_amount = quote.consumed_amount,
            )));
        }

        let new_state = if params.should_return_new_state() {
            Some(Arc::new(quote.new_state) as Arc<dyn SwapQuoter>)
        } else {
            None
        };
        Ok(Quote::new(RuintU256::from(quote.calculated_amount), quote.gas, new_state))
    }

    fn swap_limits(&self, params: LimitsParams) -> SimulationResult<SwapLimits> {
        let (max_in, max_out) =
            ProtocolSim::get_limits(self, params.token_in().clone(), params.token_out().clone())?;
        Ok(SwapLimits::new(
            Range::new(RuintU256::ZERO, biguint_to_ruint(&max_in))?,
            Range::new(RuintU256::ZERO, biguint_to_ruint(&max_out))?,
        ))
    }

    fn query_swap(&self, _params: QuerySwapParams) -> SimulationResult<Swap> {
        Err(SimulationError::FatalError(
            "EkuboState::query_swap is not yet wired (pending token plumbing)".to_string(),
        ))
    }

    fn delta_transition(
        &mut self,
        params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        ProtocolSim::delta_transition(
            self,
            params.delta().clone(),
            params.tokens(),
            params.balances(),
        )?;
        Ok(Transition::default())
    }

    fn clone_box(&self) -> Box<dyn SwapQuoter> {
        Box::new(self.clone())
    }

    #[allow(deprecated)]
    fn to_protocol_sim(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
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
    use crate::evm::protocol::ekubo::{pool::base::BasePool, test_cases::*};

    #[apply(all_cases)]
    fn test_delta_transition(case: TestCase) {
        let mut state = case.state_before_transition;

        ProtocolSim::delta_transition(
            &mut state,
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
    fn test_swap_quoter_matches_protocol_sim(case: TestCase) {
        use crate::evm::protocol::u256_num::u256_to_biguint;

        let (token0, token1) = (case.token0(), case.token1());
        let (amount_in, _) = case.swap_token0;
        let state = case.state_after_transition;

        let legacy = state
            .get_amount_out(amount_in.clone(), &token0, &token1)
            .expect("legacy quote");
        let quote = SwapQuoter::quote(
            &state,
            QuoteParams::fixed_in(&token0.address, &token1.address, biguint_to_ruint(&amount_in))
                .unwrap(),
        )
        .expect("swap quoter quote");

        assert_eq!(u256_to_biguint(quote.amount_out()), legacy.amount);
        assert_eq!(BigUint::from(quote.gas()), legacy.gas);
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

    #[test]
    fn test_marginal_price_matches_spot_price() {
        use std::str::FromStr;

        use approx::assert_ulps_eq;
        use tycho_common::models::Chain;

        const POOL_KEY: NodeKey = NodeKey {
            token0: U256([1, 0, 0, 0]),
            token1: U256([2, 0, 0, 0]),
            config: Config { fee: 0, tick_spacing: 10, extension: U256::zero() },
        };
        const LOWER_TICK: Tick = Tick { index: -10, liquidity_delta: 100_000_000 };
        const UPPER_TICK: Tick =
            Tick { index: -LOWER_TICK.index, liquidity_delta: -LOWER_TICK.liquidity_delta };
        const SQRT_RATIO: U256 = U256([0, 0, 1, 0]);

        let state = EkuboState::Base(
            BasePool::new(
                POOL_KEY,
                vec![LOWER_TICK, UPPER_TICK],
                SQRT_RATIO,
                LOWER_TICK.liquidity_delta as u128,
                0,
            )
            .unwrap(),
        );

        let token_a = Token::new(
            &Bytes::from_str("0xaaaa000000000000000000000000000000000000").unwrap(),
            "A",
            18,
            0,
            &[Some(0)],
            Chain::Ethereum,
            100,
        );
        let token_b = Token::new(
            &Bytes::from_str("0xbbbb000000000000000000000000000000000000").unwrap(),
            "B",
            6,
            0,
            &[Some(0)],
            Chain::Ethereum,
            100,
        );

        let mut dto = ProtocolComponent::default();
        dto.tokens = vec![token_a.address.clone(), token_b.address.clone()];
        let all_tokens = HashMap::from([
            (token_a.address.clone(), token_a.clone()),
            (token_b.address.clone(), token_b.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();

        let state = state.with_component(component);

        for (base, quote) in [(&token_a, &token_b), (&token_b, &token_a)] {
            let spot = state.spot_price(base, quote).unwrap();
            let marginal = state
                .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
                .unwrap();
            assert_ulps_eq!(marginal.price(), spot);
        }
    }
}
