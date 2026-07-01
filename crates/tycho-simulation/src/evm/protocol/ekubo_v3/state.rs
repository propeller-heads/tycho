use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
};

use alloy::primitives::U256 as RuintU256;
use ekubo_sdk::{
    chain::evm::{EvmPoolKey, EvmTokenAmount},
    U256,
};
use num_bigint::BigUint;
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        swap::{
            LimitsParams, MarginalPrice, MarginalPriceParams, QuerySwapParams, Quote, QuoteAmount,
            QuoteParams, Range, SimulationResult, Swap, SwapFee, SwapLimits, SwapQuoter,
            Transition, TransitionParams,
        },
    },
    Bytes,
};

use super::pool::{
    concentrated::ConcentratedPool, full_range::FullRangePool, oracle::OraclePool,
    twamm::TwammPool, EkuboPool,
};
use crate::evm::protocol::{
    ekubo_v3::pool::{
        boosted_fees::BoostedFeesPool, mev_capture::MevCapturePool, stableswap::StableswapPool,
    },
    u256_num::{biguint_to_u256 as biguint_to_ruint, u256_to_f64},
};

#[enum_delegate::implement(EkuboPool)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EkuboV3State {
    Concentrated(ConcentratedPool),
    FullRange(FullRangePool),
    Stableswap(StableswapPool),
    Oracle(OraclePool),
    Twamm(TwammPool),
    MevCapture(MevCapturePool),
    BoostedFees(BoostedFeesPool),
}

impl EkuboV3State {
    fn component_ref(&self) -> &Option<Arc<ProtocolComponent<Arc<Token>>>> {
        match self {
            Self::Concentrated(pool) => &pool.component,
            Self::FullRange(pool) => &pool.component,
            Self::Stableswap(pool) => &pool.component,
            Self::Oracle(pool) => &pool.component,
            Self::Twamm(pool) => &pool.component,
            Self::MevCapture(pool) => &pool.component,
            Self::BoostedFees(pool) => &pool.component,
        }
    }

    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        let slot = match &mut self {
            Self::Concentrated(pool) => &mut pool.component,
            Self::FullRange(pool) => &mut pool.component,
            Self::Stableswap(pool) => &mut pool.component,
            Self::Oracle(pool) => &mut pool.component,
            Self::Twamm(pool) => &mut pool.component,
            Self::MevCapture(pool) => &mut pool.component,
            Self::BoostedFees(pool) => &mut pool.component,
        };
        *slot = Some(component);
        self
    }
}

fn sqrt_price_q128_to_f64(
    x: U256,
    (token0_decimals, token1_decimals): (usize, usize),
) -> Result<f64, SimulationError> {
    let token_correction = 10f64.powi(token0_decimals as i32 - token1_decimals as i32);

    let price = u256_to_f64(x)? / 2.0f64.powi(128);
    Ok(price.powi(2) * token_correction)
}

#[typetag::serde]
impl ProtocolSim for EkuboV3State {
    fn fee(&self) -> f64 {
        self.key().config.fee as f64 / (2f64.powi(64))
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let sqrt_ratio = self.sqrt_ratio();
        let (base_decimals, quote_decimals) = (base.decimals as usize, quote.decimals as usize);

        if base < quote {
            sqrt_price_q128_to_f64(sqrt_ratio, (base_decimals, quote_decimals))
        } else {
            sqrt_price_q128_to_f64(sqrt_ratio, (quote_decimals, base_decimals))
                .map(|price| 1.0f64 / price)
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        _token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let token_amount = EvmTokenAmount {
            token: Address::try_from(&token_in.address[..]).map_err(|err| {
                SimulationError::InvalidInput(format!("token_in invalid: {err}"), None)
            })?,
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
            self.set_sqrt_ratio(U256::try_from_be_slice(sqrt_price).ok_or_else(|| {
                TransitionError::DecodeError("failed to parse updated pool price".to_string())
            })?);
        }

        self.finish_transition(delta.updated_attributes, delta.deleted_attributes)
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
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
        other
            .as_any()
            .downcast_ref::<EkuboV3State>()
            .is_some_and(|other_state| self == other_state)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        _buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let consumed_amount =
            self.get_limit(Address::try_from(&sell_token[..]).map_err(|err| {
                SimulationError::InvalidInput(format!("sell_token invalid: {err}"), None)
            })?)?;

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
}

#[typetag::serde]
impl SwapQuoter for EkuboV3State {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component_ref().clone().ok_or_else(|| {
            SimulationError::FatalError(
                "EkuboV3State: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Ok(SwapFee::new(self.key().config.fee as f64 / 2f64.powi(64)))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self
            .component_ref()
            .as_ref()
            .ok_or_else(|| {
                SimulationError::FatalError("EkuboV3State: component not set".to_string())
            })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;

        let sqrt_ratio = self.sqrt_ratio();
        let (base_decimals, quote_decimals) = (base.decimals as usize, quote.decimals as usize);

        let price = if base.as_ref() < quote.as_ref() {
            sqrt_price_q128_to_f64(sqrt_ratio, (base_decimals, quote_decimals))?
        } else {
            1.0f64 / sqrt_price_q128_to_f64(sqrt_ratio, (quote_decimals, base_decimals))?
        };

        Ok(MarginalPrice::new(price))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "EkuboV3State does not yet support exact-out (FixedOut) quoting".to_string(),
                ))
            }
        };

        let token_amount = EvmTokenAmount {
            token: Address::try_from(&params.token_in()[..]).map_err(|err| {
                SimulationError::InvalidInput(format!("token_in invalid: {err}"), None)
            })?,
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
            "EkuboV3State::query_swap is not yet wired (pending token plumbing)".to_string(),
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
    use rstest::*;
    use rstest_reuse::apply;

    use super::*;
    use crate::evm::protocol::ekubo_v3::test_cases::*;

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
    fn test_marginal_price_matches_spot_price(case: TestCase) {
        let (token0, token1) = (case.token0(), case.token1());
        let state = case.state_after_transition;

        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![token0.address.clone(), token1.address.clone()];
        let all_tokens = HashMap::from([
            (token0.address.clone(), token0.clone()),
            (token1.address.clone(), token1.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = state.with_component(component);

        for (base, quote) in [(&token0, &token1), (&token1, &token0)] {
            let spot = ProtocolSim::spot_price(&state, base, quote).unwrap();
            let marginal = SwapQuoter::marginal_price(
                &state,
                MarginalPriceParams::new(&base.address, &quote.address),
            )
            .unwrap();
            approx::assert_ulps_eq!(marginal.price(), spot);
        }
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
}
