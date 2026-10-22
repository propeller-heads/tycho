use crate::pb::tessera::v1::TransactionStorageChanges;
use anyhow::{anyhow, Result};
use keccak_hash::keccak;

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

#[cfg(test)]
pub mod testing {
    use crate::pb::tessera::v1::{BlockStorageChanges, StorageChange, TransactionStorageChanges};
    use substreams_ethereum::pb::eth;

    /// Every write in `block`, grouped by transaction in call order, without the filtering
    /// `map_storage_changes` applies.
    pub fn storage_changes(block: &eth::v2::Block) -> BlockStorageChanges {
        let mut transactions = vec![];
        for tx in &block.transaction_traces {
            let writes: Vec<_> = tx
                .calls
                .iter()
                .flat_map(|call| &call.storage_changes)
                .map(|w| StorageChange {
                    address: w.address.clone(),
                    key: w.key.clone(),
                    old_value: w.old_value.clone(),
                    new_value: w.new_value.clone(),
                    ordinal: w.ordinal,
                })
                .collect();
            if !writes.is_empty() {
                transactions.push(TransactionStorageChanges { tx: Some(tx.into()), writes });
            }
        }
        BlockStorageChanges { transactions }
    }
}
