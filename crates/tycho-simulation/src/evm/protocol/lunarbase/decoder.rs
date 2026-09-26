use std::collections::HashMap;

use lunarbase_pmm_math::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{Address, LunarBaseState};
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
    pub const MAX_PUNISHMENT_X24: &str = "max_punishment_x24";
    pub const BLACKLIST_FEE_MULTIPLIER: &str = "blacklist_fee_multiplier";
    pub const QUOTE_CALLER_WHITELISTED: &str = "quote_caller_whitelisted";
    pub const BLOCK_DELAY: &str = "block_delay";
    pub const PAUSED: &str = "paused";
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for LunarBaseState {
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
pub fn encode_state(state: &LunarBaseState) -> HashMap<String, Bytes> {
    HashMap::from([
        (
            attrs::ANCHOR_PRICE_X96.to_owned(),
            Bytes::from(
                state
                    .anchor_price_x96
                    .to_be_bytes::<32>()[12..]
                    .to_vec(),
            ),
        ),
        (attrs::FEE_ASK_X24.to_owned(), Bytes::from(state.fee_ask_x24)),
        (attrs::FEE_BID_X24.to_owned(), Bytes::from(state.fee_bid_x24)),
        (attrs::LATEST_UPDATE_BLOCK.to_owned(), Bytes::from(state.latest_update_block)),
        (attrs::RESERVE_X.to_owned(), Bytes::from(state.reserve_x)),
        (attrs::RESERVE_Y.to_owned(), Bytes::from(state.reserve_y)),
        (attrs::MAX_PUNISHMENT_X24.to_owned(), Bytes::from(state.max_punishment_x24)),
        (
            attrs::BLACKLIST_FEE_MULTIPLIER.to_owned(),
            Bytes::from(
                state
                    .blacklist_fee_multiplier
                    .to_be_bytes::<32>(),
            ),
        ),
        (
            attrs::QUOTE_CALLER_WHITELISTED.to_owned(),
            Bytes::from([u8::from(state.quote_caller_whitelisted)]),
        ),
        (attrs::BLOCK_DELAY.to_owned(), Bytes::from(state.block_delay)),
        (attrs::PAUSED.to_owned(), Bytes::from([u8::from(state.paused)])),
    ])
}

pub fn apply_delta(
    state: &mut LunarBaseState,
    updated_attributes: HashMap<String, Bytes>,
) -> Result<(), InvalidSnapshotError> {
    let mut next = state.clone();
    for (name, value) in updated_attributes {
        match name.as_str() {
            attrs::ANCHOR_PRICE_X96 => {
                next.anchor_price_x96 = decode_uint(&name, &value, 160, 20)?;
            }
            attrs::FEE_ASK_X24 => next.fee_ask_x24 = decode_uint(&name, &value, 24, 4)?.to(),
            attrs::FEE_BID_X24 => next.fee_bid_x24 = decode_uint(&name, &value, 24, 4)?.to(),
            attrs::LATEST_UPDATE_BLOCK => {
                next.latest_update_block = decode_uint(&name, &value, 48, 8)?.to();
            }
            attrs::RESERVE_X => next.reserve_x = decode_uint(&name, &value, 112, 16)?.to(),
            attrs::RESERVE_Y => next.reserve_y = decode_uint(&name, &value, 112, 16)?.to(),
            attrs::MAX_PUNISHMENT_X24 => {
                next.max_punishment_x24 = decode_uint(&name, &value, 24, 4)?.to();
            }
            attrs::BLACKLIST_FEE_MULTIPLIER => {
                next.blacklist_fee_multiplier = decode_uint(&name, &value, 256, 32)?;
            }
            attrs::QUOTE_CALLER_WHITELISTED => {
                next.quote_caller_whitelisted =
                    decode_bool(attrs::QUOTE_CALLER_WHITELISTED, &value)?;
            }
            attrs::BLOCK_DELAY => next.block_delay = decode_uint(&name, &value, 48, 8)?.to(),
            attrs::PAUSED => next.paused = decode_bool(attrs::PAUSED, &value)?,
            "block_number" => next.head_block = decode_uint(&name, &value, 64, 8)?.to(),
            _ => {}
        }
    }
    *state = next;
    Ok(())
}

pub fn decode_lunarbase_snapshot(
    snapshot: &ComponentWithState,
) -> Result<LunarBaseState, InvalidSnapshotError> {
    let attrs = &snapshot.state.attributes;

    Ok(LunarBaseState {
        pool: component_pool(snapshot)?,
        token_x: component_token(snapshot, 0)?,
        token_y: component_token(snapshot, 1)?,
        anchor_price_x96: decode_uint(
            attrs::ANCHOR_PRICE_X96,
            required_attr(attrs, attrs::ANCHOR_PRICE_X96)?,
            160,
            20,
        )?,
        fee_ask_x24: decode_uint(
            attrs::FEE_ASK_X24,
            required_attr(attrs, attrs::FEE_ASK_X24)?,
            24,
            4,
        )?
        .to(),
        fee_bid_x24: decode_uint(
            attrs::FEE_BID_X24,
            required_attr(attrs, attrs::FEE_BID_X24)?,
            24,
            4,
        )?
        .to(),
        latest_update_block: decode_uint(
            attrs::LATEST_UPDATE_BLOCK,
            required_attr(attrs, attrs::LATEST_UPDATE_BLOCK)?,
            48,
            8,
        )?
        .to(),
        reserve_x: decode_uint(attrs::RESERVE_X, required_attr(attrs, attrs::RESERVE_X)?, 112, 16)?
            .to(),
        reserve_y: decode_uint(attrs::RESERVE_Y, required_attr(attrs, attrs::RESERVE_Y)?, 112, 16)?
            .to(),
        max_punishment_x24: decode_uint(
            attrs::MAX_PUNISHMENT_X24,
            required_attr(attrs, attrs::MAX_PUNISHMENT_X24)?,
            24,
            4,
        )?
        .to(),
        blacklist_fee_multiplier: decode_uint(
            attrs::BLACKLIST_FEE_MULTIPLIER,
            required_attr(attrs, attrs::BLACKLIST_FEE_MULTIPLIER)?,
            256,
            32,
        )?,
        quote_caller_whitelisted: decode_bool(
            attrs::QUOTE_CALLER_WHITELISTED,
            required_attr(attrs, attrs::QUOTE_CALLER_WHITELISTED)?,
        )?,
        block_delay: decode_uint(
            attrs::BLOCK_DELAY,
            required_attr(attrs, attrs::BLOCK_DELAY)?,
            48,
            8,
        )?
        .to(),
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

// The extractor pads uint24/uint48/uint112 to Rust's primitive widths. Check
// both wire length and Solidity value width before converting to a primitive.
fn decode_uint(
    name: &str,
    value: &Bytes,
    bits: usize,
    max_bytes: usize,
) -> Result<U256, InvalidSnapshotError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(InvalidSnapshotError::ValueError(format!(
            "attribute {name} must contain 1..={max_bytes} bytes, got {}",
            value.len()
        )));
    }
    let decoded = U256::from_be_slice(value.as_ref());
    if decoded.bit_len() > bits {
        return Err(InvalidSnapshotError::ValueError(format!(
            "attribute {name} exceeds uint{bits}"
        )));
    }
    Ok(decoded)
}

fn decode_bool(name: &'static str, value: &Bytes) -> Result<bool, InvalidSnapshotError> {
    if value.len() != 1 {
        return Err(invalid_length(name, 1, value.len()));
    }
    match value[0] {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(InvalidSnapshotError::ValueError(format!(
            "attribute {name} must be 0 or 1, got {other}"
        ))),
    }
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

    fn state() -> LunarBaseState {
        LunarBaseState {
            pool: addr(9),
            token_x: addr(1),
            token_y: addr(2),
            anchor_price_x96: U256::from(1u128 << 96),
            fee_ask_x24: 10,
            fee_bid_x24: 11,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 2_000_000,
            max_punishment_x24: 4096,
            blacklist_fee_multiplier: U256::from(1u64),
            quote_caller_whitelisted: false,
            block_delay: 2,
            paused: false,
            head_block: 100,
        }
    }

    #[test]
    fn encodes_full_state_attributes() {
        let attrs = encode_state(&state());

        assert_eq!(
            U256::from_be_slice(attrs[attrs::ANCHOR_PRICE_X96].as_ref()),
            U256::from(1u128 << 96)
        );
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

        assert_eq!(state.anchor_price_x96, U256::from(2u128 << 96));
        assert_eq!(state.fee_ask_x24, 20);
        assert_eq!(state.fee_bid_x24, 21);
        assert_eq!(state.latest_update_block, 101);
        assert_eq!(state.reserve_x, 1_000_000);
    }

    #[test]
    fn accepts_full_uint160_anchor_and_legacy_sixteen_byte_encoding() {
        let mut state = state();
        let large = U256::from(1u64) << 159usize;
        apply_delta(
            &mut state,
            HashMap::from([(
                attrs::ANCHOR_PRICE_X96.to_owned(),
                Bytes::from(large.to_be_bytes::<32>()[12..].to_vec()),
            )]),
        )
        .unwrap();
        assert_eq!(state.anchor_price_x96, large);

        apply_delta(
            &mut state,
            HashMap::from([(attrs::ANCHOR_PRICE_X96.to_owned(), Bytes::from(1u128 << 96))]),
        )
        .unwrap();
        assert_eq!(state.anchor_price_x96, U256::from(1u128 << 96));
    }

    #[test]
    fn punishment_delta_updates_fees_without_refreshing_operator_block() {
        let mut state = state();
        apply_delta(
            &mut state,
            HashMap::from([
                (attrs::FEE_ASK_X24.to_owned(), Bytes::from(123u32)),
                (attrs::FEE_BID_X24.to_owned(), Bytes::from(456u32)),
                (attrs::MAX_PUNISHMENT_X24.to_owned(), Bytes::from(16_778u32)),
                ("block_number".to_owned(), Bytes::from(101u64)),
            ]),
        )
        .unwrap();
        assert_eq!((state.fee_ask_x24, state.fee_bid_x24), (123, 456));
        assert_eq!(state.max_punishment_x24, 16_778);
        assert_eq!(state.latest_update_block, 100);
        assert_eq!(state.head_block, 101);
    }

    #[test]
    fn rejects_malformed_and_out_of_range_attributes_without_partial_changes() {
        let cases = [
            (attrs::ANCHOR_PRICE_X96, Bytes::from(Vec::<u8>::new())),
            (attrs::ANCHOR_PRICE_X96, Bytes::from(vec![0u8; 21])),
            (attrs::FEE_ASK_X24, Bytes::from(1u32 << 24)),
            (attrs::FEE_BID_X24, Bytes::from(vec![0u8; 5])),
            (attrs::RESERVE_X, Bytes::from(1u128 << 112)),
            (attrs::RESERVE_Y, Bytes::from(vec![0u8; 17])),
            (attrs::MAX_PUNISHMENT_X24, Bytes::from(1u32 << 24)),
            (attrs::LATEST_UPDATE_BLOCK, Bytes::from(1u64 << 48)),
            (attrs::BLOCK_DELAY, Bytes::from(1u64 << 48)),
            (attrs::PAUSED, Bytes::from([2u8])),
            (attrs::BLACKLIST_FEE_MULTIPLIER, Bytes::from(vec![0u8; 33])),
            (attrs::QUOTE_CALLER_WHITELISTED, Bytes::from([2u8])),
            ("block_number", Bytes::from(vec![0u8; 9])),
        ];
        for (name, value) in cases {
            let mut state = state();
            let before = state.clone();
            let mut delta = HashMap::from([(attrs::RESERVE_Y.to_owned(), Bytes::from(123u128))]);
            delta.insert(name.to_owned(), value);
            assert!(apply_delta(&mut state, delta).is_err(), "accepted {name}");
            assert_eq!(state, before, "partially applied {name}");
        }
    }

    #[test]
    fn preserves_full_uint256_multiplier_and_caller_policy() {
        let mut state = state();
        apply_delta(
            &mut state,
            HashMap::from([
                (
                    attrs::BLACKLIST_FEE_MULTIPLIER.to_owned(),
                    Bytes::from(U256::MAX.to_be_bytes::<32>()),
                ),
                (attrs::QUOTE_CALLER_WHITELISTED.to_owned(), Bytes::from([1u8])),
            ]),
        )
        .unwrap();
        assert_eq!(state.blacklist_fee_multiplier, U256::MAX);
        assert!(state.quote_caller_whitelisted);
        assert_eq!(state.fee_multiplier(), U256::from(1u64));
    }
}
