//! Attribute layout of a Biconomy venue component, shared with the `base-biconomy` substreams
//! package. Addresses are `0x`-prefixed lowercase hex; values are big-endian integers.
//!
//! | attribute                               | value                                         |
//! |-----------------------------------------|-----------------------------------------------|
//! | `fee_bps`                               | venue protocol fee                            |
//! | `makers`                                | registered makers, 20 bytes each, in order    |
//! | `board/{mm}/{tokenIn}/{tokenOut}/{slot}`| executor storage word `slot` (0..24) of board |
//! | `anchor/{mm}/{token0}/{token1}`         | executor anchor word for the sorted pair      |
//! | `paused/{mm}`                           | 1 when the maker paused itself                |
//! | `inventory/{provider}/{token}`          | `provider.available(token)`                   |

use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    math::{Address, BOARD_WORDS},
    state::{BiconomyState, BlockEnvState},
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

pub mod attrs {
    pub const FEE_BPS: &str = "fee_bps";
    pub const MAKERS: &str = "makers";
    pub const BOARD: &str = "board";
    pub const ANCHOR: &str = "anchor";
    pub const PAUSED: &str = "paused";
    pub const INVENTORY: &str = "inventory";
    pub const PAMM_ADDRESS: &str = "pamm_address";
}

/// Seconds between Base blocks, to project a snapshot's header to the block a quote lands in
/// until the decoder applies the real execution block.
const BLOCK_TIME_SECS: u64 = 2;

fn hex_address(address: &Address) -> String {
    format!("0x{}", alloy::hex::encode(address))
}

pub fn board_key(mm: &Address, a: &Address, b: &Address) -> String {
    format!("{}/{}/{}", hex_address(mm), hex_address(a), hex_address(b))
}

pub fn pair_key(a: &Address, b: &Address) -> String {
    format!("{}/{}", hex_address(a), hex_address(b))
}

fn parse_address(value: &str) -> Result<Address, InvalidSnapshotError> {
    let raw = alloy::hex::decode(value.trim_start_matches("0x"))
        .map_err(|_| InvalidSnapshotError::ValueError(format!("invalid address `{value}`")))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| InvalidSnapshotError::ValueError(format!("address `{value}` is not 20 bytes")))
}

fn word(value: &Bytes) -> Result<U256, InvalidSnapshotError> {
    if value.len() > 32 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "value of {} bytes does not fit a storage word",
            value.len()
        )));
    }
    Ok(U256::from_be_slice(value.as_ref()))
}

pub enum AttributeChange<'a> {
    Set(&'a String, &'a Bytes),
    Delete(&'a String),
}

/// Applies attribute writes and deletions to `state`; a deleted value reads as zero, as unset
/// storage does. Unknown attributes are ignored.
pub fn apply_attributes<'a>(
    state: &mut BiconomyState,
    changes: impl IntoIterator<Item = AttributeChange<'a>>,
) -> Result<(), InvalidSnapshotError> {
    for change in changes {
        let (name, value) = match change {
            AttributeChange::Set(name, value) => (name, value.clone()),
            AttributeChange::Delete(name) => (name, Bytes::new()),
        };
        let parts: Vec<&str> = name.split('/').collect();
        match parts.as_slice() {
            [attrs::FEE_BPS] => {
                let fee = word(&value)?;
                state.fee_bps = u16::try_from(fee)
                    .map_err(|_| InvalidSnapshotError::ValueError(format!("fee_bps {fee}")))?;
            }
            [attrs::MAKERS] => {
                if value.len() % 20 != 0 {
                    return Err(InvalidSnapshotError::ValueError(
                        "makers is not a list of 20-byte addresses".to_owned(),
                    ));
                }
                state.makers = value
                    .chunks(20)
                    .map(|chunk| chunk.try_into().expect("20-byte chunk"))
                    .collect();
            }
            [attrs::BOARD, mm, token_in, token_out, slot] => {
                let slot: u8 = slot
                    .parse()
                    .ok()
                    .filter(|slot| *slot < BOARD_WORDS)
                    .ok_or_else(|| {
                        InvalidSnapshotError::ValueError(format!("invalid board slot in `{name}`"))
                    })?;
                let key = board_key(
                    &parse_address(mm)?,
                    &parse_address(token_in)?,
                    &parse_address(token_out)?,
                );
                let words = state.boards.entry(key).or_default();
                let value = word(&value)?;
                if value.is_zero() {
                    words.remove(&slot);
                } else {
                    words.insert(slot, value);
                }
            }
            [attrs::ANCHOR, mm, token0, token1] => {
                let key = board_key(
                    &parse_address(mm)?,
                    &parse_address(token0)?,
                    &parse_address(token1)?,
                );
                state.anchors.insert(key, word(&value)?);
            }
            [attrs::PAUSED, mm] => {
                let mm = parse_address(mm)?;
                if word(&value)?.is_zero() {
                    state.paused.remove(&mm);
                } else {
                    state.paused.insert(mm);
                }
            }
            [attrs::INVENTORY, provider, token] => {
                let key = pair_key(&parse_address(provider)?, &parse_address(token)?);
                if value.is_empty() {
                    state.inventory.remove(&key);
                } else {
                    state
                        .inventory
                        .insert(key, word(&value)?);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn decode_biconomy_snapshot(
    snapshot: &ComponentWithState,
    header: &BlockHeader,
) -> Result<BiconomyState, InvalidSnapshotError> {
    let venue = snapshot
        .component
        .static_attributes
        .get(attrs::PAMM_ADDRESS)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(attrs::PAMM_ADDRESS.to_owned()))?;
    let mut state = BiconomyState {
        venue: venue.as_ref().try_into().map_err(|_| {
            InvalidSnapshotError::ValueError(format!("invalid {} {venue}", attrs::PAMM_ADDRESS))
        })?,
        block: BlockEnvState {
            number: header.number + 1,
            timestamp: header.timestamp + BLOCK_TIME_SECS,
        },
        ..Default::default()
    };
    for required in [attrs::FEE_BPS, attrs::MAKERS] {
        if !snapshot
            .state
            .attributes
            .contains_key(required)
        {
            return Err(InvalidSnapshotError::MissingAttribute(required.to_owned()));
        }
    }
    apply_attributes(
        &mut state,
        snapshot
            .state
            .attributes
            .iter()
            .map(|(name, value)| AttributeChange::Set(name, value)),
    )?;
    Ok(state)
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for BiconomyState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        decode_biconomy_snapshot(&snapshot, &block)
    }
}
