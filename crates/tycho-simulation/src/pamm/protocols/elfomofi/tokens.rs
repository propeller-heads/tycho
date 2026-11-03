//! ERC20 metadata for pairs first observed in a complete helper snapshot.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy::{sol, sol_types::SolCall};
use futures::{stream, StreamExt};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::client::{ElfomofiClient, RpcHeader};
use crate::snapshot_feed::errors::FeedError;

sol! {
    function decimals() external view returns (uint8);
    function symbol() external view returns (string);
}

const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(60);
const MAX_NEGATIVE_CACHE_ENTRIES: usize = 1024;
pub(super) const MAX_DISCOVERY_PER_FETCH: usize = 8;

#[derive(Debug, Default)]
pub(super) struct NegativeCache(HashMap<Bytes, Instant>);

impl NegativeCache {
    pub fn allows(&mut self, address: &Bytes, now: Instant) -> bool {
        match self.0.get(address) {
            Some(retry_at) if *retry_at > now => false,
            Some(_) => {
                self.0.remove(address);
                true
            }
            None => true,
        }
    }

    pub fn reject(&mut self, address: Bytes, now: Instant) {
        self.0
            .retain(|_, retry_at| *retry_at > now);
        if self.0.len() >= MAX_NEGATIVE_CACHE_ENTRIES {
            let oldest = self
                .0
                .iter()
                .min_by_key(|(_, retry_at)| **retry_at)
                .map(|(address, _)| address.clone());
            if let Some(oldest) = oldest {
                self.0.remove(&oldest);
            }
        }
        self.0
            .insert(address, now + NEGATIVE_CACHE_TTL);
    }
}

pub(super) async fn discover(
    client: &ElfomofiClient,
    chain: Chain,
    header: &RpcHeader,
    addresses: Vec<Bytes>,
    request_timeout: Duration,
) -> Vec<(Bytes, Result<Token, FeedError>)> {
    stream::iter(
        addresses
            .into_iter()
            .map(|address| async move {
                let result = tokio::time::timeout(request_timeout, async {
                    let target = address.to_string();
                    let decimals_data = decimalsCall {}.abi_encode();
                    let symbol_data = symbolCall {}.abi_encode();
                    let (decimals, symbol) = tokio::join!(
                        client.call_at(&target, &decimals_data, None, header),
                        client.call_at(&target, &symbol_data, None, header),
                    );
                    let decimals =
                        decimalsCall::abi_decode_returns_validate(&decimals?).map_err(|error| {
                            FeedError::Parsing(format!(
                                "Invalid ElFomoFi token decimals for {address}: {error}"
                            ))
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
                    Ok(token)
                })
                .await
                .unwrap_or_else(|_| {
                    Err(FeedError::Connection(format!(
                        "ElFomoFi metadata request timed out for {address}"
                    )))
                });
                (address, result)
            }),
    )
    .buffer_unordered(MAX_DISCOVERY_PER_FETCH)
    .collect()
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
    fn rejected_metadata_has_a_bounded_retry_window_and_cache_size() {
        let mut cache = NegativeCache::default();
        let now = Instant::now();
        let address = Bytes::from(vec![1; 20]);
        cache.reject(address.clone(), now);
        assert!(!cache.allows(&address, now + NEGATIVE_CACHE_TTL / 2));
        assert!(cache.allows(&address, now + NEGATIVE_CACHE_TTL));

        for index in 0..MAX_NEGATIVE_CACHE_ENTRIES * 2 {
            cache.reject(Bytes::from(index.to_be_bytes().to_vec()), now);
            assert!(cache.0.len() <= MAX_NEGATIVE_CACHE_ENTRIES);
        }
    }

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
