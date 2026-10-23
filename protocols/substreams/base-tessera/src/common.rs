use crate::pb::tessera::v1::TransactionStorageChanges;
use alloy_primitives::{b256, keccak256, Address, B256};
use anyhow::{anyhow, Result};

pub const IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// Storage key of the write helper's integrator fee A[0], used by empty swapData:
/// keccak256(uint256(0) ++ uint256(1)), as `cast index uint256 0 1` computes it.
/// The mapping base slot (1) is a helper-layout assumption, not a deployment parameter.
pub const FEE_TAG_ZERO_SLOT: B256 =
    b256!("a6eef7e35abe7026729641147f7915573c7e97b47efa546f5f6e3230263bcb49");

pub fn slot(n: u64) -> B256 {
    B256::left_padding_from(&n.to_be_bytes())
}

/// The address in the low 20 bytes of a storage word.
///
/// Panics unless `word` is 32 bytes long, as every storage value is.
pub fn address(word: &[u8]) -> Address {
    Address::from_word(B256::from_slice(word))
}

pub fn zero(word: &[u8]) -> bool {
    word.iter().all(|b| *b == 0)
}

/// Lowercase `0x`-prefixed hex, the form of component ids and store keys.
pub fn id(addr: impl AsRef<[u8]>) -> String {
    format!("0x{}", hex::encode(addr))
}

/// Engine registry slot of the token pair `a`/`b` in the mapping at `base`, keyed by the sorted
/// pair: keccak256(keccak256(abi.encode(lo, hi)) ++ uint256(base)).
pub fn registry_slot(a: Address, b: Address, base: u64) -> B256 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let pair = keccak256([lo.into_word(), hi.into_word()].concat());
    keccak256([pair, slot(base)].concat())
}

impl TransactionStorageChanges {
    /// The transaction these writes belong to; `map_storage_changes` always sets it.
    pub fn tx(&self) -> Result<&tycho_substreams::prelude::Transaction> {
        self.tx
            .as_ref()
            .ok_or_else(|| anyhow!("map_storage_changes emitted writes without a transaction"))
    }
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
