use std::collections::HashSet;

use super::{
    attributes::{
        attrs, insert_address, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64,
        require_address, require_bool, require_u128, require_u256, require_u32, require_u64,
        AttributeError, AttributeMap,
    },
    state::LunarBaseState,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub updated_attributes: AttributeMap,
    pub deleted_attributes: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateDecodeError {
    Attribute(AttributeError),
    DeletedRequiredAttribute(String),
}

impl From<AttributeError> for StateDecodeError {
    fn from(value: AttributeError) -> Self {
        StateDecodeError::Attribute(value)
    }
}

pub fn encode_state(state: &LunarBaseState) -> AttributeMap {
    let mut attrs = AttributeMap::new();
    insert_address(&mut attrs, attrs::POOL, state.pool);
    insert_address(&mut attrs, attrs::TOKEN_X, state.token_x);
    insert_address(&mut attrs, attrs::TOKEN_Y, state.token_y);
    insert_u128(&mut attrs, attrs::ANCHOR_PRICE_X96, state.anchor_price_x96);
    insert_u32(&mut attrs, attrs::FEE_ASK_X24, state.fee_ask_x24);
    insert_u32(&mut attrs, attrs::FEE_BID_X24, state.fee_bid_x24);
    insert_u64(&mut attrs, attrs::LATEST_UPDATE_BLOCK, state.latest_update_block);
    insert_u128(&mut attrs, attrs::RESERVE_X, state.reserve_x);
    insert_u128(&mut attrs, attrs::RESERVE_Y, state.reserve_y);
    insert_u32(&mut attrs, attrs::CONCENTRATION_K, state.concentration_k);
    insert_u64(&mut attrs, attrs::BLOCK_DELAY, state.block_delay);
    insert_bool(&mut attrs, attrs::PAUSED, state.paused);
    insert_u256(&mut attrs, attrs::BLACKLIST_FEE_MULTIPLIER, state.blacklist_fee_multiplier);
    insert_bool(&mut attrs, attrs::EXECUTOR_WHITELISTED, state.executor_whitelisted);
    attrs
}

pub fn decode_state(attrs: &AttributeMap) -> Result<LunarBaseState, StateDecodeError> {
    Ok(LunarBaseState {
        pool: require_address(attrs, attrs::POOL)?,
        token_x: require_address(attrs, attrs::TOKEN_X)?,
        token_y: require_address(attrs, attrs::TOKEN_Y)?,
        anchor_price_x96: require_u128(attrs, attrs::ANCHOR_PRICE_X96)?,
        fee_ask_x24: require_u32(attrs, attrs::FEE_ASK_X24)?,
        fee_bid_x24: require_u32(attrs, attrs::FEE_BID_X24)?,
        latest_update_block: require_u64(attrs, attrs::LATEST_UPDATE_BLOCK)?,
        reserve_x: require_u128(attrs, attrs::RESERVE_X)?,
        reserve_y: require_u128(attrs, attrs::RESERVE_Y)?,
        concentration_k: require_u32(attrs, attrs::CONCENTRATION_K)?,
        block_delay: require_u64(attrs, attrs::BLOCK_DELAY)?,
        paused: require_bool(attrs, attrs::PAUSED)?,
        blacklist_fee_multiplier: require_u256(attrs, attrs::BLACKLIST_FEE_MULTIPLIER)?,
        executor_whitelisted: require_bool(attrs, attrs::EXECUTOR_WHITELISTED)?,
    })
}

pub fn apply_delta(state: &mut LunarBaseState, delta: &StateDelta) -> Result<(), StateDecodeError> {
    if let Some(name) = delta.deleted_attributes.iter().next() {
        return Err(StateDecodeError::DeletedRequiredAttribute(name.clone()));
    }

    let mut attrs = encode_state(state);
    attrs.extend(delta.updated_attributes.clone());
    *state = decode_state(&attrs)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use lunarbase_pmm_math::U256;

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
            fee_ask_x24: 10,
            fee_bid_x24: 11,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 2_000_000,
            concentration_k: 4096,
            block_delay: 2,
            paused: false,
            blacklist_fee_multiplier: U256::from(1u64),
            executor_whitelisted: true,
        }
    }

    #[test]
    fn round_trips_full_state_attributes() {
        let state = state();
        let decoded = decode_state(&encode_state(&state)).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn applies_partial_state_updated_delta() {
        let mut state = state();
        let mut updated = AttributeMap::new();
        insert_u128(&mut updated, attrs::ANCHOR_PRICE_X96, 2u128 << 96);
        insert_u32(&mut updated, attrs::FEE_ASK_X24, 20);
        insert_u32(&mut updated, attrs::FEE_BID_X24, 21);
        insert_u64(&mut updated, attrs::LATEST_UPDATE_BLOCK, 101);

        apply_delta(
            &mut state,
            &StateDelta { updated_attributes: updated, deleted_attributes: HashSet::new() },
        )
        .unwrap();

        assert_eq!(state.anchor_price_x96, 2u128 << 96);
        assert_eq!(state.fee_ask_x24, 20);
        assert_eq!(state.fee_bid_x24, 21);
        assert_eq!(state.latest_update_block, 101);
        assert_eq!(state.reserve_x, 1_000_000);
    }
}
