use lunarbase_pmm_math::{
    curve_pmm::{quote_x_to_y_with_multiplier, quote_y_to_x_with_multiplier},
    U256,
};

use super::state::{Address, LunarBaseState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    XToY,
    YToX,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuoteRequest {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub block_number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuoteResult {
    pub direction: Direction,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee: U256,
    pub sqrt_price_next_x96: u128,
    pub next_state: LunarBaseState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuoteError {
    Paused,
    Stale { block_number: u64, latest_update_block: u64, block_delay: u64 },
    InvalidTokenPair,
    Rejected,
    ReserveOverflow,
    ReserveUnderflow,
}

pub fn quote_exact_in(
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

    Ok(QuoteResult {
        direction,
        amount_in: request.amount_in,
        amount_out: math_result.amount_out,
        fee: math_result.fee,
        sqrt_price_next_x96: math_result.sqrt_price_next,
        next_state,
    })
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
            executor_whitelisted: true,
        }
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

        assert_eq!(quote.direction, Direction::XToY);
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
