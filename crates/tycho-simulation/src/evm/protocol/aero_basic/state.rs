use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, Address, U256};
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

use crate::evm::{
    protocol::{
        aerodrome_v1::{
            solidly_stable::{StableMath, StablePool},
            state::volatile_get_amount_out,
        },
        cpmm::protocol::{cpmm_fee, cpmm_get_limits, cpmm_spot_price},
        safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        utils::add_fee_markup,
    },
    query_pool_swap::query_pool_swap,
};

const FEE_PRECISION_BPS: u32 = 10_000;
/// `CustomFeeModule` stores 420 to mean "this pool trades at 0 bps".
const ZERO_FEE_INDICATOR_BPS: u32 = 420;
/// The factory ignores fee-module results above this and falls back to its default fee.
const MAX_BASE_FEE_BPS: u32 = 1_000;
/// The `CustomFeeModule` deployments AERO basic factories use; identical on every chain.
const VOLATILE_CUSTOM_FEE_MODULE: Address = address!("7949DE8d67E4F6b8509Bdbb43FF0503aF1247a7A");
const STABLE_CUSTOM_FEE_MODULE: Address = address!("0195fC76Da986D66B2A468133a1be41E90893Da6");
/// Gas of `pool.swap`, from `eth_simulateV1` of a cold WETH/USDC swap on Base (231_751 including
/// the 21_000 intrinsic cost). The swap only checks the invariant, so stable pools cost the same.
const SWAP_GAS: u32 = 210_000;

/// State of an AERO v1.1 basic pool: a Solidly volatile (`x * y = k`) or stable
/// (`x^3 * y + y^3 * x = k`) pool whose fee comes from its factory's fee module.
///
/// Quotes assume the swap pays no MEV tax: the factory's MEV tax module only charges transactions
/// whose priority fee exceeds its threshold, which depends on the executing transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AeroBasicState {
    reserve0: U256,
    reserve1: U256,
    stable: bool,
    /// Raw `CustomFeeModule.customFee(pool)` in bps: 0 means unset, 420 means zero fee.
    custom_fee: u32,
    /// The factory's `defaultFee` in bps.
    default_fee: u32,
    /// The factory's fee module; fees are only modelled for the known `CustomFeeModule`s.
    fee_module: Bytes,
    paused: bool,
    decimals0: u8,
    decimals1: u8,
}

/// Inputs of [`AeroBasicState::new`] that come from the pool's factory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AeroBasicFees {
    pub custom_fee: u32,
    pub default_fee: u32,
    pub fee_module: Bytes,
    pub paused: bool,
}

impl AeroBasicState {
    pub fn new(
        reserves: (U256, U256),
        stable: bool,
        fees: AeroBasicFees,
        decimals: (u8, u8),
    ) -> Result<Self, String> {
        let state = Self {
            reserve0: reserves.0,
            reserve1: reserves.1,
            stable,
            custom_fee: fees.custom_fee,
            default_fee: fees.default_fee,
            fee_module: fees.fee_module,
            paused: fees.paused,
            decimals0: decimals.0,
            decimals1: decimals.1,
        };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), String> {
        if self.default_fee >= FEE_PRECISION_BPS {
            return Err(format!(
                "default fee {} bps must be below {FEE_PRECISION_BPS}",
                self.default_fee
            ));
        }
        Ok(())
    }

    fn supported_fee_module(&self) -> Address {
        if self.stable {
            STABLE_CUSTOM_FEE_MODULE
        } else {
            VOLATILE_CUSTOM_FEE_MODULE
        }
    }

    /// The swap fee in bps, as the factory's `getBaseFee`. Errors when the pool cannot be quoted:
    /// the factory is paused or uses a fee module whose fees are not modelled.
    fn fee_bps(&self) -> Result<u32, SimulationError> {
        if self.paused {
            return Err(SimulationError::RecoverableError("Pool factory is paused".to_string()));
        }
        if self
            .fee_module
            .iter()
            .all(|byte| *byte == 0)
        {
            return Ok(self.default_fee);
        }
        if self.fee_module.as_ref() != self.supported_fee_module().as_slice() {
            return Err(SimulationError::FatalError(format!(
                "Unsupported AERO fee module {}",
                self.fee_module
            )));
        }
        // `CustomFeeModule.getFee` reverts for an unset fee; the factory then uses its default,
        // as it does for any result above the maximum base fee.
        Ok(match self.custom_fee {
            ZERO_FEE_INDICATOR_BPS => 0,
            0 => self.default_fee,
            fee if fee > MAX_BASE_FEE_BPS => self.default_fee,
            fee => fee,
        })
    }

    fn stable_pool(&self) -> StablePool {
        StablePool {
            reserve0: self.reserve0,
            reserve1: self.reserve1,
            decimals0: self.decimals0,
            decimals1: self.decimals1,
            math: StableMath::FullPrecision,
        }
    }

    /// Marginal price of token1 in token0 on the stable curve, in whole-token units:
    /// `dy/dx = (3x²y + y³) / (x³ + 3xy²)`.
    fn stable_spot_price(&self, zero2one: bool) -> Result<f64, SimulationError> {
        let x = u256_to_f64(self.reserve0)? / 10f64.powi(self.decimals0 as i32);
        let y = u256_to_f64(self.reserve1)? / 10f64.powi(self.decimals1 as i32);
        if x == 0.0 || y == 0.0 {
            return Err(SimulationError::RecoverableError("No liquidity".to_string()));
        }
        let quote_per_base = (3.0 * x * x * y + y * y * y) / (x * x * x + 3.0 * x * y * y);
        Ok(if zero2one { quote_per_base } else { 1.0 / quote_per_base })
    }
}

#[typetag::serde]
impl ProtocolSim for AeroBasicState {
    fn fee(&self) -> f64 {
        cpmm_fee(
            self.fee_bps()
                .unwrap_or(self.default_fee),
        )
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let fee = cpmm_fee(self.fee_bps()?);
        let price = if self.stable {
            self.stable_spot_price(base.address < quote.address)?
        } else {
            cpmm_spot_price(base, quote, self.reserve0, self.reserve1)?
        };
        Ok(add_fee_markup(price, fee))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let fee_bps = self.fee_bps()?;
        let amount_in = biguint_to_u256(&amount_in);
        let zero2one = token_in.address < token_out.address;
        let (reserve_in, reserve_out) =
            if zero2one { (self.reserve0, self.reserve1) } else { (self.reserve1, self.reserve0) };

        let amount_out = if self.stable {
            self.stable_pool()
                .get_amount_out(amount_in, zero2one, fee_bps)?
        } else {
            volatile_get_amount_out(amount_in, reserve_in, reserve_out, fee_bps)?
        };
        if amount_out.is_zero() {
            return Err(SimulationError::InvalidInput(
                "Amount in is too small to buy any output".to_string(),
                None,
            ));
        }

        // The fee leaves the pool: the swap sends it to the pool's fee contract before `Sync`.
        let fee_amount = safe_div_u256(
            safe_mul_u256(amount_in, U256::from(fee_bps))?,
            U256::from(FEE_PRECISION_BPS),
        )?;
        let new_reserve_in = safe_add_u256(reserve_in, safe_sub_u256(amount_in, fee_amount)?)?;
        let new_reserve_out = safe_sub_u256(reserve_out, amount_out)?;
        let mut new_state = self.clone();
        if zero2one {
            new_state.reserve0 = new_reserve_in;
            new_state.reserve1 = new_reserve_out;
        } else {
            new_state.reserve0 = new_reserve_out;
            new_state.reserve1 = new_reserve_in;
        }

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(SWAP_GAS),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let fee_bps = self.fee_bps()?;
        if self.stable {
            self.stable_pool()
                .get_limits(sell_token, buy_token)
        } else {
            cpmm_get_limits(sell_token, buy_token, self.reserve0, self.reserve1, fee_bps)
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attributes.get("reserve0") {
            self.reserve0 = U256::from_be_slice(value);
        }
        if let Some(value) = attributes.get("reserve1") {
            self.reserve1 = U256::from_be_slice(value);
        }
        if let Some(value) = attributes.get("custom_fee") {
            self.custom_fee = u32::from(value.clone());
        }
        if let Some(value) = attributes.get("default_fee") {
            self.default_fee = u32::from(value.clone());
        }
        if let Some(value) = attributes.get("fee_module") {
            self.fee_module = value.clone();
        }
        if let Some(value) = attributes.get("paused") {
            self.paused = value.iter().any(|byte| *byte != 0);
        }
        self.validate()
            .map_err(TransitionError::DecodeError)
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
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
