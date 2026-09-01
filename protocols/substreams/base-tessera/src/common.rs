//! Shared helpers and ABI-level constants for the Tessera modules.
//!
//! Deployment-specific values (addresses, storage slots) come from
//! [`crate::config::DeploymentConfig`]; the constants here are fixed by standards or by the
//! contract code itself.
use keccak_hash::keccak;
use substreams::{
    hex,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use tycho_substreams::prelude::ProtocolComponent;

/// EIP-1967 implementation slot (`keccak256("eip1967.proxy.implementation") - 1`). Every
/// Tessera pair contract is a proxy whose init writes this slot in its creation transaction.
pub const EIP1967_IMPL_SLOT: [u8; 32] =
    hex!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// Component id for a pair: its contract address, hex-encoded.
///
/// The id must hex-decode to at most 32 bytes: `tycho-simulation` left-pads it into the swap
/// adapter's `bytes32 poolId` via `string_to_bytes32`, so the adapter recovers the pair with
/// `address(bytes20(poolId))`. Using the contract address (rather than a token-derived key)
/// keeps ids unique when one base token trades against several quote tokens — WETH/USDC and
/// WETH/USDbC are distinct registered pairs — and makes a re-deployed pair a new component.
pub fn component_id(pair: &[u8]) -> String {
    format!("0x{}", hex::encode(pair))
}

/// The engine storage slot holding a pair's contract address:
/// `keccak256(abi.encode(pairKey, pair_map_slot))` with
/// `pairKey = keccak256(abi.encode(tokenLo, tokenHi))`, tokens sorted ascending.
///
/// Verified against all 15 registered pairs on Base (HANDOVER §2).
pub fn engine_pair_slot(token_a: &[u8], token_b: &[u8], pair_map_slot: u64) -> Vec<u8> {
    let (lo, hi) = if token_a < token_b { (token_a, token_b) } else { (token_b, token_a) };
    let mut encoded = [0u8; 64];
    encoded[12..32].copy_from_slice(lo);
    encoded[44..64].copy_from_slice(hi);
    let key = keccak(encoded.as_slice());
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(key.as_bytes());
    input[56..].copy_from_slice(&pair_map_slot.to_be_bytes());
    keccak(input.as_slice())
        .as_bytes()
        .to_vec()
}

/// A `u64` slot number as the 32-byte big-endian storage key used by firehose storage changes.
pub fn slot_key(slot: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[24..].copy_from_slice(&slot.to_be_bytes());
    key
}

pub fn is_zero(value: &[u8]) -> bool {
    value.iter().all(|&b| b == 0)
}

/// Extracts the 20-byte address packed in the low bytes of a 32-byte storage word.
///
/// Storage words shorter than 20 bytes are left-padded with zeros; the zero word yields the
/// zero address.
pub fn address_from_word(word: &[u8]) -> Vec<u8> {
    let mut address = [0u8; 20];
    let take = word.len().min(20);
    address[20 - take..].copy_from_slice(&word[word.len() - take..]);
    address.to_vec()
}

/// Store key indexing a pair component by its contract address.
pub fn pair_store_key(pair: &[u8]) -> String {
    format!("pair:{}", hex::encode(pair))
}

/// Store key indexing pair components by a token address (append store — one token can back
/// several pairs: USDC backs most of them, and WETH trades against both USDC and USDbC).
pub fn token_store_key(token: &[u8]) -> String {
    format!("token:{}", hex::encode(token))
}

/// All known pair-contract addresses, from the append-only pairs store.
///
/// Entries are hex pair addresses appended at component creation, `;`-separated by
/// `StoreAppend`. A pair address is appended exactly once, in the transaction that creates
/// its component.
pub fn all_pair_addresses(pairs_store: &StoreGetString) -> Vec<Vec<u8>> {
    pairs_store
        .get_last("pairs")
        .map(|joined| {
            joined
                .split(';')
                .filter(|s| !s.is_empty())
                .filter_map(|s| hex::decode(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// All known pair components, resolved through the pairs list and the components store.
pub fn all_pairs(
    components_store: &StoreGetProto<ProtocolComponent>,
    pairs_store: &StoreGetString,
) -> Vec<ProtocolComponent> {
    all_pair_addresses(pairs_store)
        .iter()
        .filter_map(|addr| components_store.get_last(pair_store_key(addr)))
        .collect()
}

/// Component ids a token backs: every pair whose token set contains it. Resolved through the
/// token index, which is append-valued — a quote token (USDC) backs many pairs and a base
/// token can trade against several quotes.
pub fn pairs_for_token(token: &[u8], token_index: &StoreGetString) -> Vec<String> {
    token_index
        .get_last(token_store_key(token))
        .map(|joined| {
            joined
                .split(';')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_id_is_the_pair_address() {
        let pair = hex::decode("f524c1bc1c64a2c99bc7eccf19ede9a1d89d5a7c").unwrap();
        assert_eq!(component_id(&pair), "0xf524c1bc1c64a2c99bc7eccf19ede9a1d89d5a7c");
    }

    #[test]
    fn engine_pair_slot_matches_onchain_values() {
        // WETH/USDC on Base: slot verified via `cast index bytes32 $(cast keccak $(cast
        // abi-encode "f(address,address)" WETH USDC)) 8` and eth_getStorageAt (HANDOVER §2).
        let weth = hex::decode("4200000000000000000000000000000000000006").unwrap();
        let usdc = hex::decode("833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap();
        let slot = engine_pair_slot(&weth, &usdc, 8);
        assert_eq!(
            hex::encode(&slot),
            "7de34ae1e8ba739875fb9abb94e6baebe5cefd45e23d860202def0971a284fe7"
        );
        // Order-independent.
        assert_eq!(engine_pair_slot(&usdc, &weth, 8), slot);

        // ZORA/USDT — a non-USDC-quoted pair resolves through the same formula.
        let zora = hex::decode("1111111111166b7fe7bd91427724b487980afc69").unwrap();
        let usdt = hex::decode("fde4c96c8593536e31f229ea8f37b2ada2699bb2").unwrap();
        assert_eq!(
            hex::encode(engine_pair_slot(&zora, &usdt, 8)),
            "8c09028ad8dbd6bcc77e9754c4adad03da90e13f78e0e2912906cf8ae58033fb"
        );
    }

    #[test]
    fn slot_key_is_left_padded_big_endian() {
        let key = slot_key(48);
        assert_eq!(key[31], 48);
        assert!(key[..31].iter().all(|&b| b == 0));
    }

    #[test]
    fn address_from_word_extracts_low_20_bytes() {
        let addr = hex::decode("f524c1bc1c64a2c99bc7eccf19ede9a1d89d5a7c").unwrap();
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(&addr);
        assert_eq!(address_from_word(&word), addr);
    }

    #[test]
    fn address_from_word_handles_short_and_zero_words() {
        assert_eq!(address_from_word(&[0xab]), {
            let mut a = vec![0u8; 20];
            a[19] = 0xab;
            a
        });
        let zero = address_from_word(&[0u8; 32]);
        assert!(zero.iter().all(|&b| b == 0));
    }
}
