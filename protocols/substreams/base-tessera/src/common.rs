//! Shared helpers and ABI-level constants for the Tessera modules.
//!
//! Deployment-specific values (addresses, storage slots) come from
//! [`crate::config::DeploymentConfig`]; the constants here are fixed by standards or by the
//! contract code itself.
use substreams::{
    hex,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use tycho_substreams::prelude::ProtocolComponent;

/// EIP-1967 implementation slot (`keccak256("eip1967.proxy.implementation") - 1`). Every
/// per-book price store is a proxy whose init writes this slot in its creation transaction.
pub const EIP1967_IMPL_SLOT: [u8; 32] =
    hex!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// Deterministic component id for a book: 32 bytes = `tesseraswap (20) ‖ base token low 12`,
/// hex-encoded.
///
/// The id must hex-decode to at most 32 bytes: `tycho-simulation` converts it into the swap
/// adapter's `bytes32 poolId` via `string_to_bytes32`. The adapter validates the sell/buy
/// tokens it is given against the low-12-byte token suffix; it never needs to reconstruct the
/// full token address from the id. Keyed by token (not by store address) so the id survives a
/// store re-deploy for the same book.
pub fn component_id(tesseraswap: &[u8], base_token: &[u8]) -> String {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(tesseraswap);
    id[20..].copy_from_slice(&base_token[8..20]);
    format!("0x{}", hex::encode(id))
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

/// Store key indexing a book component by its price-store address.
pub fn store_key(store_addr: &[u8]) -> String {
    format!("store:{}", hex::encode(store_addr))
}

/// Store key indexing a book component by a token address.
pub fn token_key(token: &[u8]) -> String {
    format!("token:{}", hex::encode(token))
}

/// All known book price-store addresses, from the append-only books store.
///
/// Entries are hex store addresses appended at component creation, `;`-separated by
/// `StoreAppend`. Duplicates cannot occur: a store address is appended exactly once, in the
/// transaction that creates its component.
pub fn all_book_stores(books_store: &StoreGetString) -> Vec<Vec<u8>> {
    books_store
        .get_last("books")
        .map(|joined| {
            joined
                .split(';')
                .filter(|s| !s.is_empty())
                .filter_map(|s| hex::decode(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// All known book components, resolved through the books list and the components store.
pub fn all_books(
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
) -> Vec<ProtocolComponent> {
    all_book_stores(books_store)
        .iter()
        .filter_map(|addr| components_store.get_last(store_key(addr)))
        .collect()
}

/// Component ids a token backs: USDC (the hub) backs every book; a base token backs exactly
/// its own book.
pub fn books_for_token(
    token: &[u8],
    usdc: &[u8],
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
) -> Vec<String> {
    if token == usdc {
        all_books(components_store, books_store)
            .into_iter()
            .map(|c| c.id)
            .collect()
    } else {
        components_store
            .get_last(token_key(token))
            .map(|c| vec![c.id])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TESSERASWAP: [u8; 20] = hex!("55555522005bcae1c2424d474bfd5ed477749e3e");

    #[test]
    fn component_id_is_tesseraswap_scoped_bytes32() {
        let weth = hex::decode("4200000000000000000000000000000000000006").unwrap();
        assert_eq!(
            component_id(&TESSERASWAP, &weth),
            // tesseraswap (20 bytes) ‖ WETH low 12 bytes
            "0x55555522005bcae1c2424d474bfd5ed477749e3e000000000000000000000006"
        );
        let cbbtc = hex::decode("cbb7c0000ab88b473b1f5afd9ef808440eed33bf").unwrap();
        assert_eq!(
            component_id(&TESSERASWAP, &cbbtc),
            "0x55555522005bcae1c2424d474bfd5ed477749e3e3b1f5afd9ef808440eed33bf"
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
