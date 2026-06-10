use std::{any::Any, collections::HashMap};

use lunarbase_pmm_math::{
    curve_pmm::{quote_x_to_y_with_multiplier, quote_y_to_x_with_multiplier},
    PoolParams, U256,
};
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
    },
    Bytes,
};

use super::decoder::apply_delta;

pub type Address = [u8; 20];
const DEFAULT_GAS: u64 = 180_000;
const LIMIT_PROBE_NUMERATOR: u32 = 2162;
const LIMIT_PROBE_DENOMINATOR: u32 = 1000;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseTychoState {
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub anchor_price_x96: u128,
    pub fee_ask_x24: u32,
    pub fee_bid_x24: u32,
    pub latest_update_block: u64,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub concentration_k: u32,
    pub block_delay: u64,
    pub paused: bool,
    pub head_block: u64,
}

impl LunarBaseTychoState {
    pub fn pool_params(&self) -> PoolParams {
        PoolParams {
            sqrt_price_x96: self.anchor_price_x96,
            fee_ask_x24: self.fee_ask_x24,
            fee_bid_x24: self.fee_bid_x24,
            reserve_x: self.reserve_x,
            reserve_y: self.reserve_y,
            concentration_k: self.concentration_k,
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.head_block
            < self
                .latest_update_block
                .saturating_add(self.block_delay)
    }

    fn quote_exact_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<(U256, Self), QuoteError> {
        if self.paused {
            return Err(QuoteError::Paused);
        }

        if !self.is_fresh() {
            return Err(QuoteError::Stale {
                block_number: self.head_block,
                latest_update_block: self.latest_update_block,
                block_delay: self.block_delay,
            });
        }

        let params = self.pool_params();
        if token_in == self.token_x && token_out == self.token_y {
            let math_result = quote_x_to_y_with_multiplier(&params, amount_in, U256::from(1u64));
            if math_result.amount_out.is_zero() {
                return Err(QuoteError::Rejected);
            }

            let input = u256_to_u128(amount_in)?;
            let gross_output = u256_to_u128(
                math_result
                    .amount_out
                    .checked_add(math_result.fee)
                    .ok_or(QuoteError::ReserveOverflow)?,
            )?;
            let mut next = self.clone();
            next.reserve_x = next
                .reserve_x
                .checked_add(input)
                .ok_or(QuoteError::ReserveOverflow)?;
            next.reserve_y = next
                .reserve_y
                .checked_sub(gross_output)
                .ok_or(QuoteError::ReserveUnderflow)?;
            return Ok((math_result.amount_out, next));
        }

        if token_in == self.token_y && token_out == self.token_x {
            let math_result = quote_y_to_x_with_multiplier(&params, amount_in, U256::from(1u64));
            if math_result.amount_out.is_zero() {
                return Err(QuoteError::Rejected);
            }

            let input = u256_to_u128(amount_in)?;
            let gross_output = u256_to_u128(
                math_result
                    .amount_out
                    .checked_add(math_result.fee)
                    .ok_or(QuoteError::ReserveOverflow)?,
            )?;
            let mut next = self.clone();
            next.reserve_y = next
                .reserve_y
                .checked_add(input)
                .ok_or(QuoteError::ReserveOverflow)?;
            next.reserve_x = next
                .reserve_x
                .checked_sub(gross_output)
                .ok_or(QuoteError::ReserveUnderflow)?;
            return Ok((math_result.amount_out, next));
        }

        Err(QuoteError::InvalidTokenPair)
    }
}

#[typetag::serde]
impl ProtocolSim for LunarBaseTychoState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let token_in = address_from_bytes(base.address.as_ref())?;
        let token_out = address_from_bytes(quote.address.as_ref())?;
        if token_in == self.token_x && token_out == self.token_y {
            return spot_from_reserves(self.reserve_x, self.reserve_y, base, quote);
        }
        if token_in == self.token_y && token_out == self.token_x {
            return spot_from_reserves(self.reserve_y, self.reserve_x, base, quote);
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let (amount_out, next_state) = self
            .quote_exact_in(
                address_from_bytes(token_in.address.as_ref())?,
                address_from_bytes(token_out.address.as_ref())?,
                biguint_to_u256(&amount_in)?,
            )
            .map_err(map_quote_error)?;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(DEFAULT_GAS),
            Box::new(next_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell = address_from_bytes(sell_token.as_ref())?;
        let buy = address_from_bytes(buy_token.as_ref())?;
        if sell == self.token_x && buy == self.token_y {
            return quote_limit(self, sell, buy, soft_limit(self.reserve_x));
        }
        if sell == self.token_y && buy == self.token_x {
            return quote_limit(self, sell, buy, soft_limit(self.reserve_y));
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(name) = delta.deleted_attributes.iter().next() {
            return Err(TransitionError::DecodeError(format!(
                "LunarBase does not support deleted attributes: {name}"
            )));
        }

        let head_block = delta
            .updated_attributes
            .get("block_number")
            .map(|value| u64::from(value.clone()));

        let updated_attributes = delta
            .updated_attributes
            .into_iter()
            .filter(|(key, _)| key != "block_number" && key != "block_timestamp")
            .collect();
        apply_delta(self, updated_attributes)
            .map_err(|err| TransitionError::DecodeError(format!("{err:?}")))?;
        if let Some(head_block) = head_block {
            self.head_block = head_block;
        }
        Ok(())
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum QuoteError {
    Paused,
    Stale { block_number: u64, latest_update_block: u64, block_delay: u64 },
    InvalidTokenPair,
    Rejected,
    ReserveOverflow,
    ReserveUnderflow,
}

fn u256_to_u128(value: U256) -> Result<u128, QuoteError> {
    if value.bit_len() > 128 {
        return Err(QuoteError::ReserveOverflow);
    }
    let limbs = value.as_limbs();
    Ok(((limbs[1] as u128) << 64) | limbs[0] as u128)
}

fn spot_from_reserves(
    reserve_in: u128,
    reserve_out: u128,
    token_in: &Token,
    token_out: &Token,
) -> Result<f64, SimulationError> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err(SimulationError::RecoverableError("zero LunarBase reserve".to_owned()));
    }
    let decimals_adjustment = 10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
    Ok((reserve_out as f64 / reserve_in as f64) * decimals_adjustment)
}

// This soft bound mirrors Tycho's CPMM `get_limits` convention:
// https://github.com/propeller-heads/tycho/blob/main/crates/tycho-simulation/src/evm/protocol/cpmm/protocol.rs/#L113
//
// CPMM uses `(sqrt(10) - 1) * reserve_in ~= 2.162 * reserve_in` as the
// amount-in that would produce roughly 90% price impact in a fee-less
// constant-product pool. LunarBase does not treat this as a protocol limit;
// it is only the initial probe for `quote_limit`, which halves the amount
// until the LunarBase quote math accepts it.
fn soft_limit(reserve_in: u128) -> BigUint {
    BigUint::from(reserve_in) * 2162u32 / 1000u32
}

fn quote_limit(
    state: &LunarBaseTychoState,
    token_in: Address,
    token_out: Address,
    mut amount_in: BigUint,
) -> Result<(BigUint, BigUint), SimulationError> {
    if amount_in == BigUint::ZERO {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }

    loop {
        match state.quote_exact_in(token_in, token_out, biguint_to_u256(&amount_in)?) {
            Ok((amount_out, _)) => return Ok((amount_in, u256_to_biguint(amount_out))),
            Err(
                QuoteError::Rejected | QuoteError::ReserveOverflow | QuoteError::ReserveUnderflow,
            ) => {
                amount_in >>= 1;
                if amount_in == BigUint::ZERO {
                    return Ok((BigUint::ZERO, BigUint::ZERO));
                }
            }
            Err(err) => return Err(map_quote_error(err)),
        }
    }
}

fn address_from_bytes(value: &[u8]) -> Result<Address, SimulationError> {
    value.try_into().map_err(|_| {
        SimulationError::InvalidInput(
            format!("expected 20-byte address, got {}", value.len()),
            None,
        )
    })
}

fn biguint_to_u256(value: &BigUint) -> Result<U256, SimulationError> {
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        return Err(SimulationError::InvalidInput("amount_in exceeds uint256".to_owned(), None));
    }
    Ok(U256::from_be_slice(&bytes))
}

fn u256_to_biguint(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

fn map_quote_error(err: QuoteError) -> SimulationError {
    SimulationError::InvalidInput(format!("LunarBase quote rejected: {err:?}"), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    fn state() -> LunarBaseTychoState {
        LunarBaseTychoState {
            pool: addr(9),
            token_x: addr(1),
            token_y: addr(2),
            anchor_price_x96: 1u128 << 96,
            fee_ask_x24: 0,
            fee_bid_x24: 0,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 1_000_000,
            concentration_k: 0,
            block_delay: 2,
            paused: false,
            head_block: 100,
        }
    }

    #[test]
    fn quotes_x_to_y_and_transitions_reserves() {
        let state = state();
        let (amount_out, next_state) = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1_000u64))
            .unwrap();

        assert_eq!(amount_out, U256::from(1_000u64));
        assert_eq!(next_state.reserve_x, 1_001_000);
        assert_eq!(next_state.reserve_y, 999_000);
        assert_eq!(next_state.anchor_price_x96, state.anchor_price_x96);
        assert_eq!(next_state.head_block, state.head_block);
    }

    #[test]
    fn rejects_stale_state() {
        let mut state = state();
        state.head_block = 102;

        let err = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1_000u64))
            .unwrap_err();

        assert_eq!(
            err,
            QuoteError::Stale { block_number: 102, latest_update_block: 100, block_delay: 2 }
        );
    }
}
