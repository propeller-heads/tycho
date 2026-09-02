use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::Zero;
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

use crate::evm::protocol::{
    cpmm::protocol::{
        cpmm_fee, cpmm_get_amount_out, cpmm_get_limits, cpmm_spot_price, cpmm_swap_to_price,
        ProtocolFee,
    },
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint},
    utils::add_fee_markup,
};

// A mainnet-fork gas report measures RingSwapV2Executor.swap at ~146k. The solution-level gas
// estimator accounts for the ~60k input token transfer separately, leaving a 90k swap base.
const SWAP_BASE_GAS: u64 = 90_000;
const RING_SWAP_V2_FEE_BPS: u32 = 30;
const FEE_PRECISION: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const FEE_NUMERATOR: U256 = U256::from_limbs([9_970, 0, 0, 0]);

/// Ring Swap v2 uses normal CPMM reserve math, but its output FewToken must be unwrapped into
/// the solver-facing ERC-20. `backing{0,1}` is the indexed component balance
/// `min(pair reserve, wrapper backing)`. Since CPMM output is always below the output reserve,
/// this value is equivalent to the wrapper backing when enforcing executable output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingSwapV2State {
    pub component_id: String,
    pub reserve0: U256,
    pub reserve1: U256,
    pub backing0: U256,
    pub backing1: U256,
    pub token0: Bytes,
    pub token1: Bytes,
}

impl RingSwapV2State {
    pub fn new(
        component_id: String,
        reserve0: U256,
        reserve1: U256,
        backing0: U256,
        backing1: U256,
        token0: Bytes,
        token1: Bytes,
    ) -> Self {
        Self { component_id, reserve0, reserve1, backing0, backing1, token0, token1 }
    }

    fn zero_to_one(&self, token_in: &Token, token_out: &Token) -> bool {
        token_in.address < token_out.address
    }

    fn output_backing(&self, zero_to_one: bool) -> U256 {
        if zero_to_one {
            self.backing1
        } else {
            self.backing0
        }
    }

    fn protocol_fee() -> ProtocolFee {
        ProtocolFee::new(FEE_NUMERATOR, FEE_PRECISION)
    }

    fn max_input_for_backing(
        reserve_in: U256,
        reserve_out: U256,
        output_backing: U256,
    ) -> Result<U256, SimulationError> {
        if output_backing >= reserve_out {
            return Ok(U256::MAX);
        }
        let first_unexecutable_output = safe_add_u256(output_backing, U256::from(1))?;
        if first_unexecutable_output == reserve_out {
            return Ok(U256::MAX);
        }

        // floor(amount_out) <= backing iff:
        // amount_in * fee * (reserve_out - backing - 1)
        //     < (backing + 1) * reserve_in * fee_precision.
        let numerator =
            safe_mul_u256(safe_mul_u256(first_unexecutable_output, reserve_in)?, FEE_PRECISION)?;
        let denominator =
            safe_mul_u256(FEE_NUMERATOR, safe_sub_u256(reserve_out, first_unexecutable_output)?)?;

        safe_div_u256(safe_sub_u256(numerator, U256::from(1))?, denominator)
    }

    fn capped_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (soft_input, _) = cpmm_get_limits(
            sell_token.clone(),
            buy_token.clone(),
            self.reserve0,
            self.reserve1,
            RING_SWAP_V2_FEE_BPS,
        )?;
        let zero_to_one = sell_token < buy_token;
        let (reserve_in, reserve_out) = if zero_to_one {
            (self.reserve0, self.reserve1)
        } else {
            (self.reserve1, self.reserve0)
        };
        let soft_input_u256 = biguint_to_u256(&soft_input);
        let soft_output =
            cpmm_get_amount_out(soft_input_u256, reserve_in, reserve_out, Self::protocol_fee())?;
        let output_backing = self.output_backing(zero_to_one);
        if output_backing == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        if soft_output <= output_backing {
            return Ok((soft_input, u256_to_biguint(soft_output)));
        }

        let capped_input = Self::max_input_for_backing(reserve_in, reserve_out, output_backing)?
            .min(soft_input_u256);
        if capped_input == U256::ZERO {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let output =
            cpmm_get_amount_out(capped_input, reserve_in, reserve_out, Self::protocol_fee())?;
        Ok((u256_to_biguint(capped_input), u256_to_biguint(output)))
    }

    fn apply_component_balance_updates(&mut self, balances: &Balances) {
        let Some(component_balances) = balances
            .component_balances
            .get(&self.component_id)
        else {
            return;
        };
        if let Some(balance) = component_balances.get(&self.token0) {
            self.backing0 = U256::from_be_slice(balance);
        }
        if let Some(balance) = component_balances.get(&self.token1) {
            self.backing1 = U256::from_be_slice(balance);
        }
    }
}

#[typetag::serde]
impl ProtocolSim for RingSwapV2State {
    fn fee(&self) -> f64 {
        cpmm_fee(RING_SWAP_V2_FEE_BPS)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let price = cpmm_spot_price(base, quote, self.reserve0, self.reserve1)?;
        Ok(add_fee_markup(price, self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        let zero_to_one = self.zero_to_one(token_in, token_out);
        let (reserve_in, reserve_out) = if zero_to_one {
            (self.reserve0, self.reserve1)
        } else {
            (self.reserve1, self.reserve0)
        };
        let amount_out =
            cpmm_get_amount_out(amount_in, reserve_in, reserve_out, Self::protocol_fee())?;
        let output_backing = self.output_backing(zero_to_one);
        if output_backing == U256::ZERO || amount_out > output_backing {
            return Err(SimulationError::InvalidInput(
                "RingSwapV2 output exceeds FewToken underlying backing".to_string(),
                None,
            ));
        }

        let mut new_state = self.clone();
        if zero_to_one {
            new_state.reserve0 = safe_add_u256(self.reserve0, amount_in)?;
            new_state.reserve1 = safe_sub_u256(self.reserve1, amount_out)?;
            new_state.backing0 = safe_add_u256(self.backing0, amount_in)?;
            new_state.backing1 = safe_sub_u256(self.backing1, amount_out)?;
        } else {
            new_state.reserve0 = safe_sub_u256(self.reserve0, amount_out)?;
            new_state.reserve1 = safe_add_u256(self.reserve1, amount_in)?;
            new_state.backing0 = safe_sub_u256(self.backing0, amount_out)?;
            new_state.backing1 = safe_add_u256(self.backing1, amount_in)?;
        }

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(SWAP_BASE_GAS),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.capped_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        if delta
            .updated_attributes
            .contains_key("reserve0") ||
            delta
                .updated_attributes
                .contains_key("reserve1")
        {
            self.reserve0 = U256::from_be_slice(
                delta
                    .updated_attributes
                    .get("reserve0")
                    .ok_or_else(|| TransitionError::MissingAttribute("reserve0".to_string()))?,
            );
            self.reserve1 = U256::from_be_slice(
                delta
                    .updated_attributes
                    .get("reserve1")
                    .ok_or_else(|| TransitionError::MissingAttribute("reserve1".to_string()))?,
            );
        }
        self.apply_component_balance_updates(balances);
        Ok(())
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::PoolTargetPrice {
                target: price,
                tolerance: _,
                min_amount_in: _,
                max_amount_in: _,
            } => {
                let zero_to_one = self.zero_to_one(params.token_in(), params.token_out());
                let (reserve_in, reserve_out) = if zero_to_one {
                    (self.reserve0, self.reserve1)
                } else {
                    (self.reserve1, self.reserve0)
                };
                let (target_input, _) =
                    cpmm_swap_to_price(reserve_in, reserve_out, price, Self::protocol_fee())?;
                let (max_input, _) = self.get_limits(
                    params.token_in().address.clone(),
                    params.token_out().address.clone(),
                )?;
                let amount_in = target_input.min(max_input);
                if amount_in.is_zero() {
                    return Ok(PoolSwap::new(
                        BigUint::ZERO,
                        BigUint::ZERO,
                        Box::new(self.clone()),
                        None,
                    ));
                }

                let result =
                    self.get_amount_out(amount_in.clone(), params.token_in(), params.token_out())?;
                Ok(PoolSwap::new(amount_in, result.amount, result.new_state, None))
            }
            SwapConstraint::TradeLimitPrice { .. } => Err(SimulationError::InvalidInput(
                "RingSwapV2State does not support TradeLimitPrice constraint in query_pool_swap"
                    .to_string(),
                None,
            )),
        }
    }

    fn transitions_from_delta_alone(&self, _delta: &ProtocolStateDelta) -> bool {
        true
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
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{token::Token, Chain},
        simulation::{
            errors::SimulationError,
            protocol_sim::{Balances, ProtocolSim},
        },
        Bytes,
    };

    use super::*;

    fn address(value: u8) -> Bytes {
        Bytes::from(vec![value; 20])
    }

    fn token(value: u8) -> Token {
        Token::new(&address(value), "T", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
    }

    fn state_with_id(component_id: &str, backing1: u64) -> RingSwapV2State {
        RingSwapV2State::new(
            component_id.to_string(),
            U256::from(1_000),
            U256::from(1_000),
            U256::from(1_000),
            U256::from(backing1),
            address(1),
            address(2),
        )
    }

    fn state(backing1: u64) -> RingSwapV2State {
        state_with_id("ring-pool", backing1)
    }

    #[test]
    fn rejects_quotes_that_exceed_output_wrapper_backing() {
        let result = state(10).get_amount_out(BigUint::from(100_u64), &token(1), &token(2));

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }

    #[test]
    fn quotes_within_backing_update_reserves_and_wrapper_balances() {
        let result = state(10)
            .get_amount_out(BigUint::from(10_u64), &token(1), &token(2))
            .unwrap();
        let updated = result
            .new_state
            .as_any()
            .downcast_ref::<RingSwapV2State>()
            .unwrap();

        assert_eq!(result.amount, BigUint::from(9_u64));
        assert_eq!(updated.reserve0, U256::from(1_010));
        assert_eq!(updated.reserve1, U256::from(991));
        assert_eq!(updated.backing0, U256::from(1_010));
        assert_eq!(updated.backing1, U256::from(1));
    }

    #[test]
    fn limits_are_capped_by_output_wrapper_backing() {
        let state = state(10);
        let (max_input, max_output) = state
            .get_limits(address(1), address(2))
            .unwrap();

        assert_eq!(max_input, BigUint::from(11_u64));
        assert_eq!(max_output, BigUint::from(10_u64));
        assert!(state
            .get_amount_out(max_input.clone(), &token(1), &token(2))
            .is_ok());
        assert!(matches!(
            state.get_amount_out(max_input + BigUint::from(1_u64), &token(1), &token(2)),
            Err(SimulationError::InvalidInput(_, None))
        ));
    }

    #[test]
    fn zero_backing_has_no_executable_limits() {
        let state = state(0);
        let (max_input, max_output) = state
            .get_limits(address(1), address(2))
            .unwrap();

        assert_eq!(max_input, BigUint::ZERO);
        assert_eq!(max_output, BigUint::ZERO);
        assert!(matches!(
            state.get_amount_out(BigUint::from(1_u64), &token(1), &token(2)),
            Err(SimulationError::InvalidInput(_, None))
        ));
    }

    #[test]
    fn closed_form_backing_limit_is_exact_at_boundary() {
        for (reserve_in, reserve_out, backing) in [
            (1_000_u64, 1_000_u64, 0_u64),
            (1_000, 1_000, 10),
            (10_000, 25_000, 1_000),
            (25_000, 10_000, 9_000),
        ] {
            let max_input = RingSwapV2State::max_input_for_backing(
                U256::from(reserve_in),
                U256::from(reserve_out),
                U256::from(backing),
            )
            .unwrap();
            let amount_out = cpmm_get_amount_out(
                max_input,
                U256::from(reserve_in),
                U256::from(reserve_out),
                RingSwapV2State::protocol_fee(),
            )
            .unwrap();
            let next_amount_out = cpmm_get_amount_out(
                max_input + U256::from(1),
                U256::from(reserve_in),
                U256::from(reserve_out),
                RingSwapV2State::protocol_fee(),
            )
            .unwrap();

            assert!(amount_out <= U256::from(backing));
            assert!(next_amount_out > U256::from(backing));
        }
    }

    #[test]
    fn backing_at_or_above_reserve_is_uncapped_without_overflow() {
        assert_eq!(
            RingSwapV2State::max_input_for_backing(
                U256::from(1_000),
                U256::from(1_000),
                U256::from(1_000),
            )
            .unwrap(),
            U256::MAX
        );
        assert_eq!(
            RingSwapV2State::max_input_for_backing(
                U256::from(1_000),
                U256::from(1_000),
                U256::MAX,
            )
            .unwrap(),
            U256::MAX
        );
    }

    #[test]
    fn uncapped_limits_report_the_exact_cpmm_output() {
        let state = state(10_000);
        let (max_input, max_output) = state
            .get_limits(address(1), address(2))
            .unwrap();
        let expected_output = cpmm_get_amount_out(
            biguint_to_u256(&max_input),
            state.reserve0,
            state.reserve1,
            RingSwapV2State::protocol_fee(),
        )
        .unwrap();

        assert_eq!(max_output, u256_to_biguint(expected_output));
    }

    #[test]
    fn component_balance_only_delta_updates_without_reserve_attributes() {
        let mut state = state(10);
        let balances = Balances {
            component_balances: HashMap::from([(
                "ring-pool".to_string(),
                HashMap::from([(address(2), Bytes::from(vec![42]))]),
            )]),
            account_balances: HashMap::new(),
        };

        state
            .delta_transition(ProtocolStateDelta::default(), &HashMap::new(), &balances)
            .unwrap();

        assert_eq!(state.reserve0, U256::from(1_000));
        assert_eq!(state.reserve1, U256::from(1_000));
        assert_eq!(state.backing1, U256::from(42));
    }

    #[test]
    fn shared_wrapper_updates_each_pool_from_its_component_balance() {
        let mut first_pool = state_with_id("first-pool", 10);
        let mut second_pool = state_with_id("second-pool", 20);
        let balances = Balances {
            component_balances: HashMap::from([
                ("first-pool".to_string(), HashMap::from([(address(2), Bytes::from(vec![42]))])),
                ("second-pool".to_string(), HashMap::from([(address(2), Bytes::from(vec![21]))])),
            ]),
            account_balances: HashMap::new(),
        };

        first_pool
            .delta_transition(ProtocolStateDelta::default(), &HashMap::new(), &balances)
            .unwrap();
        second_pool
            .delta_transition(ProtocolStateDelta::default(), &HashMap::new(), &balances)
            .unwrap();

        assert_eq!(first_pool.backing1, U256::from(42));
        assert_eq!(second_pool.backing1, U256::from(21));
    }

    #[test]
    fn unrelated_component_balance_does_not_update_state() {
        let mut state = state(10);
        let balances = Balances {
            component_balances: HashMap::from([(
                "other-pool".to_string(),
                HashMap::from([(address(2), Bytes::from(vec![99]))]),
            )]),
            account_balances: HashMap::new(),
        };

        state
            .delta_transition(ProtocolStateDelta::default(), &HashMap::new(), &balances)
            .unwrap();

        assert_eq!(state.backing1, U256::from(10));
    }

    #[test]
    fn backing_cap_is_applied_in_both_directions() {
        let mut state = state(1_000);
        state.backing0 = U256::from(10);

        let result = state.get_amount_out(BigUint::from(100_u64), &token(2), &token(1));

        assert!(matches!(result, Err(SimulationError::InvalidInput(_, None))));
    }
}
