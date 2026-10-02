use crate::pb::tessera::v1::TransactionStorageChanges;
use anyhow::{anyhow, Result};
use keccak_hash::keccak;
use substreams::store::{StoreGet, StoreGetString};

pub const IMPLEMENTATION_SLOT: [u8; 32] =
    substreams::hex!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
pub fn slot(n: u64) -> Vec<u8> {
    let mut word = vec![0; 32];
    word[24..].copy_from_slice(&n.to_be_bytes());
    word
}
pub fn address(word: &[u8]) -> Vec<u8> {
    let mut addr = vec![0; 20];
    let n = word.len().min(20);
    addr[20 - n..].copy_from_slice(&word[word.len() - n..]);
    addr
}
pub fn zero(word: &[u8]) -> bool {
    word.iter().all(|b| *b == 0)
}
pub fn id(addr: &[u8]) -> String {
    format!("0x{}", hex::encode(addr))
}
pub fn registry_slot(a: &[u8], b: &[u8], base: u64) -> Vec<u8> {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut pair = [0u8; 64];
    pair[12..32].copy_from_slice(lo);
    pair[44..].copy_from_slice(hi);
    let mut mapping = keccak(pair).as_bytes().to_vec();
    mapping.extend(slot(base));
    keccak(mapping).as_bytes().to_vec()
}
pub fn pairs(store: &StoreGetString, key: &str) -> Vec<String> {
    store
        .get_last(key)
        .unwrap_or_default()
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Storage key for the write helper's integrator fee A[0], used by empty swapData.
/// Solidity mapping layout: keccak256(uint256(0) ++ uint256(1)), with 32-byte words.
/// The mapping base slot (1) is a helper-layout assumption, not a deployment parameter.
pub fn fee_tag_zero_slot() -> Vec<u8> {
    let mut key = slot(0);
    key.extend(slot(1));
    keccak(key).as_bytes().to_vec()
}

/// The transaction a `map_storage_changes` group belongs to.
pub fn storage_transaction(
    group: &TransactionStorageChanges,
) -> Result<&tycho_substreams::prelude::Transaction> {
    group
        .tx
        .as_ref()
        .ok_or_else(|| anyhow!("map_storage_changes emitted writes without a transaction"))
}
