use std::collections::HashMap;

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{Address, LunarBaseTychoState};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

mod attrs {
    pub const ANCHOR_PRICE_X96: &str = "anchor_price_x96";
    pub const FEE_ASK_X24: &str = "fee_ask_x24";
    pub const FEE_BID_X24: &str = "fee_bid_x24";
    pub const LATEST_UPDATE_BLOCK: &str = "latest_update_block";
    pub const RESERVE_X: &str = "reserve_x";
    pub const RESERVE_Y: &str = "reserve_y";
    pub const CONCENTRATION_K: &str = "concentration_k";
    pub const BLOCK_DELAY: &str = "block_delay";
    pub const PAUSED: &str = "paused";
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
        let mut state = decode_lunarbase_snapshot(&snapshot)?;
        state.head_block = block.number;
        Ok(state)
    }
}

#[cfg(test)]
pub fn encode_state(state: &LunarBaseTychoState) -> HashMap<String, Bytes> {
    HashMap::from([
        (attrs::ANCHOR_PRICE_X96.to_owned(), Bytes::from(state.anchor_price_x96)),
        (attrs::FEE_ASK_X24.to_owned(), Bytes::from(state.fee_ask_x24)),
        (attrs::FEE_BID_X24.to_owned(), Bytes::from(state.fee_bid_x24)),
        (attrs::LATEST_UPDATE_BLOCK.to_owned(), Bytes::from(state.latest_update_block)),
        (attrs::RESERVE_X.to_owned(), Bytes::from(state.reserve_x)),
        (attrs::RESERVE_Y.to_owned(), Bytes::from(state.reserve_y)),
        (attrs::CONCENTRATION_K.to_owned(), Bytes::from(state.concentration_k)),
        (attrs::BLOCK_DELAY.to_owned(), Bytes::from(state.block_delay)),
        (attrs::PAUSED.to_owned(), Bytes::from([u8::from(state.paused)])),
    ])
}

pub fn apply_delta(
    state: &mut LunarBaseTychoState,
    updated_attributes: HashMap<String, Bytes>,
) -> Result<(), InvalidSnapshotError> {
    for (name, value) in updated_attributes {
        match name.as_str() {
            attrs::ANCHOR_PRICE_X96 => state.anchor_price_x96 = u128::from(value),
            attrs::FEE_ASK_X24 => state.fee_ask_x24 = u32::from(value),
            attrs::FEE_BID_X24 => state.fee_bid_x24 = u32::from(value),
            attrs::LATEST_UPDATE_BLOCK => state.latest_update_block = u64::from(value),
            attrs::RESERVE_X => state.reserve_x = u128::from(value),
            attrs::RESERVE_Y => state.reserve_y = u128::from(value),
            attrs::CONCENTRATION_K => state.concentration_k = u32::from(value),
            attrs::BLOCK_DELAY => state.block_delay = u64::from(value),
            attrs::PAUSED => state.paused = decode_bool(attrs::PAUSED, &value)?,
            _ => {}
        }
    }
    Ok(())
}

pub fn decode_lunarbase_snapshot(
    snapshot: &ComponentWithState,
) -> Result<LunarBaseTychoState, InvalidSnapshotError> {
    let attrs = &snapshot.state.attributes;

    Ok(LunarBaseTychoState {
        pool: component_pool(snapshot)?,
        token_x: component_token(snapshot, 0)?,
        token_y: component_token(snapshot, 1)?,
        anchor_price_x96: u128::from(required_attr(attrs, attrs::ANCHOR_PRICE_X96)?.clone()),
        fee_ask_x24: u32::from(required_attr(attrs, attrs::FEE_ASK_X24)?.clone()),
        fee_bid_x24: u32::from(required_attr(attrs, attrs::FEE_BID_X24)?.clone()),
        latest_update_block: u64::from(required_attr(attrs, attrs::LATEST_UPDATE_BLOCK)?.clone()),
        reserve_x: u128::from(required_attr(attrs, attrs::RESERVE_X)?.clone()),
        reserve_y: u128::from(required_attr(attrs, attrs::RESERVE_Y)?.clone()),
        concentration_k: u32::from(required_attr(attrs, attrs::CONCENTRATION_K)?.clone()),
        block_delay: u64::from(required_attr(attrs, attrs::BLOCK_DELAY)?.clone()),
        paused: decode_bool(attrs::PAUSED, required_attr(attrs, attrs::PAUSED)?)?,
        head_block: 0,
    })
}

fn component_pool(snapshot: &ComponentWithState) -> Result<Address, InvalidSnapshotError> {
    address_from_component_id(&snapshot.component.id)
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
        .and_then(address_from_bytes)
}

fn required_attr<'a>(
    attrs: &'a HashMap<String, Bytes>,
    name: &'static str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    attrs
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_owned()))
}

fn decode_bool(name: &'static str, value: &Bytes) -> Result<bool, InvalidSnapshotError> {
    if value.len() != 1 {
        return Err(invalid_length(name, 1, value.len()));
    }
    Ok(value[0] != 0)
}

fn address_from_bytes(value: &[u8]) -> Result<Address, InvalidSnapshotError> {
    value.try_into().map_err(|_| {
        InvalidSnapshotError::ValueError(format!("expected 20-byte address, got {}", value.len()))
    })
}

fn address_from_component_id(value: &str) -> Result<Address, InvalidSnapshotError> {
    let value = value
        .strip_prefix("0x")
        .unwrap_or(value);
    if value.len() != 40 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "expected 20-byte hex address component id, got {value}"
        )));
    }

    let mut out = [0u8; 20];
    for (idx, byte) in out.iter_mut().enumerate() {
        let start = idx * 2;
        *byte = u8::from_str_radix(&value[start..start + 2], 16).map_err(|err| {
            InvalidSnapshotError::ValueError(format!("invalid LunarBase component id hex: {err}"))
        })?;
    }
    Ok(out)
}

fn invalid_length(name: &'static str, expected: usize, actual: usize) -> InvalidSnapshotError {
    InvalidSnapshotError::ValueError(format!(
        "attribute {name} has invalid length: expected {expected}, got {actual}"
    ))
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
            fee_ask_x24: 10,
            fee_bid_x24: 11,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 2_000_000,
            concentration_k: 4096,
            block_delay: 2,
            paused: false,
            head_block: 100,
        }
    }

    #[test]
    fn encodes_full_state_attributes() {
        let attrs = encode_state(&state());

        assert_eq!(u128::from(attrs[attrs::ANCHOR_PRICE_X96].clone()), 1u128 << 96);
        assert_eq!(u32::from(attrs[attrs::FEE_ASK_X24].clone()), 10);
        assert_eq!(u64::from(attrs[attrs::LATEST_UPDATE_BLOCK].clone()), 100);
        assert!(!decode_bool(attrs::PAUSED, &attrs[attrs::PAUSED]).unwrap());
    }

    #[test]
    fn applies_partial_state_updated_delta() {
        let mut state = state();
        let updated = HashMap::from([
            (attrs::ANCHOR_PRICE_X96.to_owned(), Bytes::from(2u128 << 96)),
            (attrs::FEE_ASK_X24.to_owned(), Bytes::from(20u32)),
            (attrs::FEE_BID_X24.to_owned(), Bytes::from(21u32)),
            (attrs::LATEST_UPDATE_BLOCK.to_owned(), Bytes::from(101u64)),
        ]);

        apply_delta(&mut state, updated).unwrap();

        assert_eq!(state.anchor_price_x96, 2u128 << 96);
        assert_eq!(state.fee_ask_x24, 20);
        assert_eq!(state.fee_bid_x24, 21);
        assert_eq!(state.latest_update_block, 101);
        assert_eq!(state.reserve_x, 1_000_000);
    }
}
