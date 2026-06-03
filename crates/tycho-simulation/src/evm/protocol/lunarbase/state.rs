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

use super::decoder::{apply_delta, AttributeError, AttributeMap, StateDelta};

pub type Address = [u8; 20];
const DEFAULT_GAS: u64 = 180_000;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseState {
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
    pub blacklist_fee_multiplier: U256,
}

impl LunarBaseState {
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

    pub fn is_fresh(&self, block_number: u64) -> bool {
        block_number <
            self.latest_update_block
                .saturating_add(self.block_delay)
    }

    pub fn fee_multiplier(&self) -> U256 {
        U256::from(1u64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseTychoState {
    pub state: LunarBaseState,
    pub head_block: u64,
}

#[typetag::serde]
impl ProtocolSim for LunarBaseTychoState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let token_in = address_from_bytes(base.address.as_ref())?;
        let token_out = address_from_bytes(quote.address.as_ref())?;
        if token_in == self.state.token_x && token_out == self.state.token_y {
            return spot_from_reserves(self.state.reserve_x, self.state.reserve_y, base, quote);
        }
        if token_in == self.state.token_y && token_out == self.state.token_x {
            return spot_from_reserves(self.state.reserve_y, self.state.reserve_x, base, quote);
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let quote = quote_exact_in(
            &self.state,
            QuoteRequest {
                token_in: address_from_bytes(token_in.address.as_ref())?,
                token_out: address_from_bytes(token_out.address.as_ref())?,
                amount_in: biguint_to_u256(&amount_in)?,
                block_number: self.head_block,
            },
        )
        .map_err(map_quote_error)?;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(quote.amount_out),
            BigUint::from(DEFAULT_GAS),
            Box::new(Self { state: quote.next_state, head_block: self.head_block }),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell = address_from_bytes(sell_token.as_ref())?;
        let buy = address_from_bytes(buy_token.as_ref())?;
        if sell == self.state.token_x && buy == self.state.token_y {
            return quote_limit(&self.state, sell, buy, soft_limit(self.state.reserve_x));
        }
        if sell == self.state.token_y && buy == self.state.token_x {
            return quote_limit(&self.state, sell, buy, soft_limit(self.state.reserve_y));
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
            .map(|value| decode_block_number(value.as_ref()))
            .transpose()
            .map_err(|err| TransitionError::DecodeError(format!("{err:?}")))?;

        let state_delta = StateDelta {
            updated_attributes: delta
                .updated_attributes
                .into_iter()
                .filter(|(key, _)| key != "block_number" && key != "block_timestamp")
                .map(|(key, value)| (key, value.to_vec()))
                .collect::<AttributeMap>(),
        };
        apply_delta(&mut self.state, &state_delta)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    XToY,
    YToX,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QuoteRequest {
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    block_number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QuoteResult {
    amount_out: U256,
    next_state: LunarBaseState,
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

fn quote_exact_in(
    state: &LunarBaseState,
    request: QuoteRequest,
) -> Result<QuoteResult, QuoteError> {
    if state.paused {
        return Err(QuoteError::Paused);
    }

    if !state.is_fresh(request.block_number) {
        return Err(QuoteError::Stale {
            block_number: request.block_number,
            latest_update_block: state.latest_update_block,
            block_delay: state.block_delay,
        });
    }

    let direction = resolve_direction(state, &request)?;
    let params = state.pool_params();
    let math_result = match direction {
        Direction::XToY => {
            quote_x_to_y_with_multiplier(&params, request.amount_in, state.fee_multiplier())
        }
        Direction::YToX => {
            quote_y_to_x_with_multiplier(&params, request.amount_in, state.fee_multiplier())
        }
    };

    if math_result.amount_out.is_zero() {
        return Err(QuoteError::Rejected);
    }

    let next_state = transition_reserves(
        state,
        direction,
        request.amount_in,
        math_result.amount_out,
        math_result.fee,
    )?;

    Ok(QuoteResult { amount_out: math_result.amount_out, next_state })
}

fn resolve_direction(
    state: &LunarBaseState,
    request: &QuoteRequest,
) -> Result<Direction, QuoteError> {
    if request.token_in == state.token_x && request.token_out == state.token_y {
        return Ok(Direction::XToY);
    }
    if request.token_in == state.token_y && request.token_out == state.token_x {
        return Ok(Direction::YToX);
    }
    Err(QuoteError::InvalidTokenPair)
}

fn transition_reserves(
    state: &LunarBaseState,
    direction: Direction,
    amount_in: U256,
    amount_out: U256,
    fee: U256,
) -> Result<LunarBaseState, QuoteError> {
    let input = u256_to_u128(amount_in)?;
    let gross_output = u256_to_u128(
        amount_out
            .checked_add(fee)
            .ok_or(QuoteError::ReserveOverflow)?,
    )?;

    let mut next = state.clone();
    match direction {
        Direction::XToY => {
            next.reserve_x = next
                .reserve_x
                .checked_add(input)
                .ok_or(QuoteError::ReserveOverflow)?;
            next.reserve_y = next
                .reserve_y
                .checked_sub(gross_output)
                .ok_or(QuoteError::ReserveUnderflow)?;
        }
        Direction::YToX => {
            next.reserve_y = next
                .reserve_y
                .checked_add(input)
                .ok_or(QuoteError::ReserveOverflow)?;
            next.reserve_x = next
                .reserve_x
                .checked_sub(gross_output)
                .ok_or(QuoteError::ReserveUnderflow)?;
        }
    }
    Ok(next)
}

fn u256_to_u128(value: U256) -> Result<u128, QuoteError> {
    if value.bit_len() > 128 {
        return Err(QuoteError::ReserveOverflow);
    }
    let limbs = value.as_limbs();
    Ok(((limbs[1] as u128) << 64) | limbs[0] as u128)
}

fn decode_block_number(value: &[u8]) -> Result<u64, AttributeError> {
    if value.len() > 8 {
        return Err(AttributeError::IntegerOverflow("block_number"));
    }
    let mut out = [0u8; 8];
    out[8 - value.len()..].copy_from_slice(value);
    Ok(u64::from_be_bytes(out))
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

fn soft_limit(reserve_in: u128) -> BigUint {
    BigUint::from(reserve_in) * 2162u32 / 1000u32
}

fn quote_limit(
    state: &LunarBaseState,
    token_in: Address,
    token_out: Address,
    mut amount_in: BigUint,
) -> Result<(BigUint, BigUint), SimulationError> {
    if amount_in == BigUint::ZERO {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }

    loop {
        let quote = quote_exact_in(
            state,
            QuoteRequest {
                token_in,
                token_out,
                amount_in: biguint_to_u256(&amount_in)?,
                block_number: state.latest_update_block,
            },
        );
        match quote {
            Ok(quote) => return Ok((amount_in, u256_to_biguint(quote.amount_out))),
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

    fn state() -> LunarBaseState {
        LunarBaseState {
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
            blacklist_fee_multiplier: U256::from(1u64),
        }
    }

    #[test]
    fn uses_base_fee_by_default() {
        let mut state = state();
        state.blacklist_fee_multiplier = U256::from(100u64);

        assert_eq!(state.fee_multiplier(), U256::from(1u64));
    }

    #[test]
    fn quotes_x_to_y_and_transitions_reserves() {
        let state = state();
        let quote = quote_exact_in(
            &state,
            QuoteRequest {
                token_in: state.token_x,
                token_out: state.token_y,
                amount_in: U256::from(1_000u64),
                block_number: 100,
            },
        )
        .unwrap();

        assert_eq!(quote.amount_out, U256::from(1_000u64));
        assert_eq!(quote.next_state.reserve_x, 1_001_000);
        assert_eq!(quote.next_state.reserve_y, 999_000);
        assert_eq!(quote.next_state.anchor_price_x96, state.anchor_price_x96);
    }

    #[test]
    fn rejects_stale_state() {
        let state = state();
        let err = quote_exact_in(
            &state,
            QuoteRequest {
                token_in: state.token_x,
                token_out: state.token_y,
                amount_in: U256::from(1_000u64),
                block_number: 102,
            },
        )
        .unwrap_err();

        assert_eq!(
            err,
            QuoteError::Stale { block_number: 102, latest_update_block: 100, block_delay: 2 }
        );
    }
}
