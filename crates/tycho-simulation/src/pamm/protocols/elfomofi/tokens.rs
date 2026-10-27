//! ERC20 metadata for pairs first observed in a complete helper snapshot.

use std::collections::{HashMap, HashSet};

use alloy::{sol, sol_types::SolCall};
use futures::{stream, StreamExt, TryStreamExt};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::{
    client::{ElfomofiClient, RpcHeader},
    models::Snapshot,
};
use crate::snapshot_feed::errors::FeedError;

sol! {
    function decimals() external view returns (uint8);
    function symbol() external view returns (string);
}

pub(super) async fn discover(
    client: &ElfomofiClient,
    chain: Chain,
    header: &RpcHeader,
    snapshots: &[Snapshot],
    known: &HashMap<Bytes, Token>,
) -> Result<HashMap<Bytes, Token>, FeedError> {
    let addresses: HashSet<_> = snapshots
        .iter()
        .flat_map(|snapshot| [&snapshot.book.base_token, &snapshot.book.quote_token])
        .filter(|address| !known.contains_key(*address))
        .cloned()
        .collect();
    stream::iter(
        addresses
            .into_iter()
            .map(|address| async move {
                let target = address.to_string();
                let decimals_data = decimalsCall {}.abi_encode();
                let symbol_data = symbolCall {}.abi_encode();
                let (decimals, symbol) = tokio::join!(
                    client.call_at(&target, &decimals_data, None, header),
                    client.call_at(&target, &symbol_data, None, header),
                );
                let decimals =
                    decimalsCall::abi_decode_returns_validate(&decimals?).map_err(|_| {
                        FeedError::Parsing(format!("Invalid ElFomoFi token decimals for {address}"))
                    })?;
                let symbol = symbol
                    .ok()
                    .and_then(|raw| decode_symbol(&raw))
                    .unwrap_or_else(|| target.clone());
                // Transfer gas uses a conservative ERC20 estimate.
                let token = Token::new(
                    &address,
                    &symbol,
                    u32::from(decimals),
                    0,
                    &[Some(65_000)],
                    chain,
                    100,
                );
                Ok((address, token))
            }),
    )
    .buffer_unordered(8)
    .try_collect()
    .await
}

fn decode_symbol(raw: &[u8]) -> Option<String> {
    let value = symbolCall::abi_decode_returns_validate(raw)
        .ok()
        .or_else(|| {
            (raw.len() == 32)
                .then(|| {
                    let end = raw
                        .iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(raw.len());
                    std::str::from_utf8(&raw[..end])
                        .ok()
                        .map(str::to_owned)
                })
                .flatten()
        })?;
    (!value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control))
        .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_symbols_accept_string_and_legacy_bytes32_and_reject_unusable_values() {
        assert_eq!(
            decode_symbol(&symbolCall::abi_encode_returns(&"USDC".to_owned())),
            Some("USDC".into())
        );
        let mut legacy = [0u8; 32];
        legacy[..4].copy_from_slice(b"WETH");
        assert_eq!(decode_symbol(&legacy), Some("WETH".into()));
        for value in ["".to_owned(), "\u{1b}[31m".to_owned(), "X".repeat(129)] {
            assert!(decode_symbol(&symbolCall::abi_encode_returns(&value)).is_none());
        }
        assert!(decode_symbol(&[0xff; 32]).is_none());
        assert!(decode_symbol(&[0x01]).is_none());
    }

    #[test]
    fn decimals_require_a_valid_uint8_abi_word() {
        for decimals in [0u8, 6, 18, 255] {
            assert_eq!(
                decimalsCall::abi_decode_returns_validate(&decimalsCall::abi_encode_returns(
                    &decimals
                ))
                .unwrap(),
                decimals
            );
        }
        assert!(decimalsCall::abi_decode_returns_validate(&[]).is_err());
        let mut overflow = [0u8; 32];
        overflow[30] = 1;
        assert!(decimalsCall::abi_decode_returns_validate(&overflow).is_err());
    }
}
