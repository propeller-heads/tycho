use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use ekubo_sdk::{
    chain::evm::{EvmPoolKey, EvmTokenAmount},
    U256,
};
use num_bigint::BigUint;
use revm::primitives::Address;
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

use super::pool::{
    concentrated::ConcentratedPool, full_range::FullRangePool, oracle::OraclePool,
    twamm::TwammPool, EkuboPool,
};
use crate::evm::protocol::{
    ekubo_v3::{
        addresses::SIGNED_EXCLUSIVE_SWAP_ADDRESS,
        pool::{
            boosted_fees::BoostedFeesPool, mev_capture::MevCapturePool, stableswap::StableswapPool,
        },
    },
    u256_num::u256_to_f64,
};

/// Gas cost of `Core.forward`, the signature check and the signed-fee accounting, on top of the
/// swap itself.
///
/// Measured against a plain-swap baseline in Ekubo's `SignedExclusiveSwap.t.sol`: `forward` plus
/// the signature check costs 37,852 and charging the signed fee costs another 24,521. Fynd signs a
/// fee above zero, so the constant is the sum of both.
const SIGNED_EXCLUSIVE_SWAP_GAS: u64 = 62_373;

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

fn sqrt_price_q128_to_f64(
    x: U256,
    (token0_decimals, token1_decimals): (usize, usize),
) -> Result<f64, SimulationError> {
    let token_correction = 10f64.powi(token0_decimals as i32 - token1_decimals as i32);

    let price = u256_to_f64(x)? / 2.0f64.powi(128);
    Ok(price.powi(2) * token_correction)
}

impl EkuboV3State {
    /// Zero unless the extension forces the swap through `Core.forward`.
    fn forward_overhead_gas(&self) -> u64 {
        if self.key().config.extension == SIGNED_EXCLUSIVE_SWAP_ADDRESS {
            SIGNED_EXCLUSIVE_SWAP_GAS
        } else {
            0
        }
    }
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

        let quote = self.quote(token_amount)?;

        if quote.calculated_amount > i128::MAX as u128 {
            return Err(SimulationError::RecoverableError(
                "calculated amount exceeds i128::MAX".to_string(),
            ));
        }

        let res = GetAmountOutResult {
            amount: BigUint::from(quote.calculated_amount),
            gas: BigUint::from(quote.gas) + BigUint::from(self.forward_overhead_gas()),
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

#[cfg(test)]
mod tests {
    use rstest::*;
    use rstest_reuse::apply;

    use super::*;
    use crate::evm::protocol::ekubo_v3::test_cases::*;

    /// Both pools price identically, so the gas gap is exactly the forward overhead.
    #[rstest]
    fn test_signed_exclusive_swap_gas_includes_the_forward_overhead() {
        let signed = signed_exclusive_swap();
        let (token0, token1) = (signed.token0(), signed.token1());
        let (amount_in, _) = signed.swap_token0.clone();

        let signed_gas = signed
            .state_after_transition
            .get_amount_out(amount_in.clone(), &token0, &token1)
            .expect("signed pool quotes")
            .gas;

        let plain = concentrated();
        let plain_gas = plain
            .state_after_transition
            .get_amount_out(amount_in, &plain.token0(), &plain.token1())
            .expect("plain pool quotes")
            .gas;

        assert_eq!(
            signed_gas - plain_gas,
            BigUint::from(SIGNED_EXCLUSIVE_SWAP_GAS),
            "the signed pool must carry exactly the forward overhead over an equivalent plain pool"
        );
    }

    /// Only a pool that cannot be swapped without `Core.forward` is surcharged.
    #[rstest]
    fn test_other_pools_carry_no_forward_overhead() {
        for case in [concentrated(), full_range(), mev_capture()] {
            assert_eq!(
                case.state_after_transition
                    .forward_overhead_gas(),
                0,
                "only a signed-exclusive pool is surcharged"
            );
        }
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
}
