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

pub fn fee_tag_zero_slot() -> Vec<u8> {
    let mut key = slot(0);
    key.extend(slot(1));
    keccak(key).as_bytes().to_vec()
}

/// Call vectors are ordered by call entry, not by the time each storage write occurred.
pub fn committed_writes(
    tx: &substreams_ethereum::pb::eth::v2::TransactionTrace,
) -> Vec<&substreams_ethereum::pb::eth::v2::StorageChange> {
    let mut writes: Vec<_> = tx
        .calls
        .iter()
        .filter(|c| !c.state_reverted)
        .flat_map(|c| &c.storage_changes)
        .collect();
    writes.sort_by_key(|w| w.ordinal);
    writes
}

#[cfg(test)]
mod tests {
    use super::*;
    use substreams_ethereum::pb::eth::v2::{Call, StorageChange, TransactionTrace};
    #[test]
    fn nested_call_writes_are_folded_in_execution_order() {
        let write = |ordinal, value| StorageChange {
            address: vec![1; 20],
            key: IMPLEMENTATION_SLOT.to_vec(),
            ordinal,
            new_value: vec![value; 32],
            ..Default::default()
        };
        let tx = TransactionTrace {
            calls: vec![
                Call { storage_changes: vec![write(10, 1), write(30, 3)], ..Default::default() },
                Call { storage_changes: vec![write(20, 2)], ..Default::default() },
            ],
            ..Default::default()
        };
        let writes = committed_writes(&tx);
        assert_eq!(
            writes
                .iter()
                .map(|w| w.ordinal)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert_eq!(writes.last().unwrap().new_value, vec![3; 32]);
    }
}
