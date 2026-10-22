//! Attribute layout of a Biconomy venue component, shared with the `base-biconomy` substreams
//! package. Addresses are `0x`-prefixed lowercase hex; values are big-endian integers. Nothing
//! is read over RPC: every value is a storage word, or a provider config set by the package.
//!
//! | attribute                                | value                                          |
//! |------------------------------------------|------------------------------------------------|
//! | `fee_bps`                                | venue storage slot 0 (`feeBps` is its low 16 bits) |
//! | `maker_count`                            | venue storage slot 1 (`makerList.length`)      |
//! | `maker/{i}`                              | `makerList[i]`                                 |
//! | `board/{mm}/{tokenIn}/{tokenOut}/{slot}` | executor storage word `slot` (0..24) of board  |
//! | `anchor/{mm}/{token0}/{token1}`          | executor anchor word for the sorted pair       |
//! | `paused/{mm}`                            | 1 when the maker paused itself                 |
//! | `provider/{provider}`                    | kind byte (0 balance, 1 vault, 2 unbounded), signer, vault |
//! | `balance/{token}/{holder}`               | the token's `balanceOf` storage word           |
//! | `allowance/{token}/{owner}/{spender}`    | the token's `allowance` storage word           |

use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    math::{Address, BOARD_WORDS},
    state::{BiconomyState, BlockEnvState, ProviderConfig, ProviderKind},
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

pub mod attrs {
    pub const FEE_BPS: &str = "fee_bps";
    pub const MAKER_COUNT: &str = "maker_count";
    pub const MAKER: &str = "maker";
    pub const BOARD: &str = "board";
    pub const ANCHOR: &str = "anchor";
    pub const PAUSED: &str = "paused";
    pub const PROVIDER: &str = "provider";
    pub const BALANCE: &str = "balance";
    pub const ALLOWANCE: &str = "allowance";
    pub const PAMM_ADDRESS: &str = "pamm_address";
}

/// Seconds between Base blocks, to project a snapshot's header to the block a quote lands in
/// until the decoder applies the real execution block.
const BLOCK_TIME_SECS: u64 = 2;

pub fn hex_address(address: &Address) -> String {
    format!("0x{}", alloy::hex::encode(address))
}

pub fn board_key(mm: &Address, a: &Address, b: &Address) -> String {
    format!("{}/{}/{}", hex_address(mm), hex_address(a), hex_address(b))
}

pub fn allowance_key(token: &Address, owner: &Address, spender: &Address) -> String {
    board_key(token, owner, spender)
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

fn word_address(word: U256) -> Address {
    word.to_be_bytes::<32>()[12..]
        .try_into()
        .expect("20 bytes")
}

/// A provider config: kind byte, signer, then the vault for a vault provider.
fn decode_provider(value: &Bytes) -> Result<ProviderConfig, InvalidSnapshotError> {
    if value.len() != 41 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "provider config of {} bytes",
            value.len()
        )));
    }
    let address = |range: std::ops::Range<usize>| -> Address {
        value[range]
            .try_into()
            .expect("20 bytes")
    };
    let kind = match value[0] {
        0 => ProviderKind::Balance,
        1 => ProviderKind::Vault(address(21..41)),
        2 => ProviderKind::Unbounded,
        other => {
            return Err(InvalidSnapshotError::ValueError(format!("provider kind {other}")));
        }
    };
    Ok(ProviderConfig { kind, signer: address(1..21) })
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
                // feeBps is the low 16 bits of the venue's first slot.
                state.fee_bps = (word(&value)? & U256::from(u16::MAX)).to::<u16>();
            }
            [attrs::MAKER_COUNT] => {
                let count = word(&value)?;
                state.maker_count = u64::try_from(count).map_err(|_| {
                    InvalidSnapshotError::ValueError(format!("maker_count {count}"))
                })?;
            }
            [attrs::MAKER, index] => {
                let index: u8 = index.parse().map_err(|_| {
                    InvalidSnapshotError::ValueError(format!("invalid maker index in `{name}`"))
                })?;
                state
                    .maker_slots
                    .insert(index, word_address(word(&value)?));
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
            [attrs::PROVIDER, provider] => {
                let provider = parse_address(provider)?;
                if value.is_empty() {
                    state
                        .providers
                        .remove(&hex_address(&provider));
                } else {
                    state
                        .providers
                        .insert(hex_address(&provider), decode_provider(&value)?);
                }
            }
            [attrs::BALANCE, token, holder] => {
                let key = pair_key(&parse_address(token)?, &parse_address(holder)?);
                state
                    .balances
                    .insert(key, word(&value)?);
            }
            [attrs::ALLOWANCE, token, owner, spender] => {
                let key = allowance_key(
                    &parse_address(token)?,
                    &parse_address(owner)?,
                    &parse_address(spender)?,
                );
                state
                    .allowances
                    .insert(key, word(&value)?);
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
