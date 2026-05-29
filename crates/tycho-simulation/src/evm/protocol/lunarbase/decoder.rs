use std::collections::HashMap;

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, simulation::errors::SimulationError, Bytes};

use super::{
    attributes::{
        attrs, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64, require_bool,
        require_u128, require_u256, require_u32, require_u64, AttributeError, AttributeMap,
    },
    state::{Address, LunarBaseState, LunarBaseTychoState},
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDelta {
    pub updated_attributes: AttributeMap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateDecodeError {
    Attribute(AttributeError),
}

impl From<AttributeError> for StateDecodeError {
    fn from(value: AttributeError) -> Self {
        StateDecodeError::Attribute(value)
    }
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for LunarBaseTychoState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let state = decode_lunarbase_snapshot(&snapshot)?;
        Ok(Self { state, head_block: block.number })
    }
}

pub fn encode_state(state: &LunarBaseState) -> AttributeMap {
    let mut attrs = AttributeMap::new();
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

pub fn decode_state(
    static_attrs: StaticStateAttributes,
    attrs: &AttributeMap,
) -> Result<LunarBaseState, StateDecodeError> {
    Ok(LunarBaseState {
        pool: static_attrs.pool,
        token_x: static_attrs.token_x,
        token_y: static_attrs.token_y,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticStateAttributes {
    pub pool: [u8; 20],
    pub token_x: [u8; 20],
    pub token_y: [u8; 20],
}

pub fn apply_delta(state: &mut LunarBaseState, delta: &StateDelta) -> Result<(), StateDecodeError> {
    let mut attrs = encode_state(state);
    attrs.extend(delta.updated_attributes.clone());
    *state = decode_state(
        StaticStateAttributes { pool: state.pool, token_x: state.token_x, token_y: state.token_y },
        &attrs,
    )?;
    Ok(())
}

pub fn decode_lunarbase_snapshot(
    snapshot: &ComponentWithState,
) -> Result<LunarBaseState, InvalidSnapshotError> {
    let mut attributes = AttributeMap::new();
    for (name, value) in snapshot.state.attributes.iter() {
        attributes.insert(name.clone(), value.to_vec());
    }

    decode_state(
        StaticStateAttributes {
            pool: component_pool(snapshot)?,
            token_x: component_token(snapshot, 0)?,
            token_y: component_token(snapshot, 1)?,
        },
        &attributes,
    )
    .map_err(map_decode_error)
}

fn component_pool(snapshot: &ComponentWithState) -> Result<Address, InvalidSnapshotError> {
    snapshot
        .component
        .contract_addresses
        .first()
        .ok_or_else(|| {
            InvalidSnapshotError::ValueError("missing LunarBase pool contract".to_owned())
        })
        .and_then(|value| address_from_bytes(value.as_ref()).map_err(map_sim_error))
}

fn component_token(
    snapshot: &ComponentWithState,
    idx: usize,
) -> Result<Address, InvalidSnapshotError> {
    snapshot
        .component
        .tokens
        .get(idx)
        .map(|token| token.as_ref())
        .ok_or_else(|| InvalidSnapshotError::ValueError(format!("missing token index {idx}")))
        .and_then(|value| address_from_bytes(value).map_err(map_sim_error))
}

fn address_from_bytes(value: &[u8]) -> Result<Address, SimulationError> {
    value.try_into().map_err(|_| {
        SimulationError::InvalidInput(
            format!("expected 20-byte address, got {}", value.len()),
            None,
        )
    })
}

fn map_decode_error(err: StateDecodeError) -> InvalidSnapshotError {
    match err {
        StateDecodeError::Attribute(AttributeError::Missing(name)) => {
            InvalidSnapshotError::MissingAttribute(name.to_string())
        }
        other => InvalidSnapshotError::ValueError(format!("{other:?}")),
    }
}

fn map_sim_error(err: SimulationError) -> InvalidSnapshotError {
    InvalidSnapshotError::ValueError(err.to_string())
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
        let decoded = decode_state(
            StaticStateAttributes {
                pool: state.pool,
                token_x: state.token_x,
                token_y: state.token_y,
            },
            &encode_state(&state),
        )
        .unwrap();
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

        apply_delta(&mut state, &StateDelta { updated_attributes: updated }).unwrap();

        assert_eq!(state.anchor_price_x96, 2u128 << 96);
        assert_eq!(state.fee_ask_x24, 20);
        assert_eq!(state.fee_bid_x24, 21);
        assert_eq!(state.latest_update_block, 101);
        assert_eq!(state.reserve_x, 1_000_000);
    }
}
