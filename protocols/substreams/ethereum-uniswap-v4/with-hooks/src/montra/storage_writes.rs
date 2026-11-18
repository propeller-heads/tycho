//! The storage writes a transaction made to the Montra hooks, netted per slot.
//!
//! [`storage_writes`] returns every slot a transaction wrote across a set of hooks, with the value
//! before and after. It orders writes by ordinal and keeps the first old value and the last new
//! value, as [`crate::storage::tx_storage_writes`] does. It differs from that function in five
//! ways:
//!
//! - It reads a set of contracts and lists every slot written, where `tx_storage_writes` reads one
//!   contract and looks slots up by key. The Montra modules match a hook write to its pool through
//!   `store_montra_slots` and cannot name the slots in advance.
//! - It returns the value before as well, which tells a changed word from a rewritten one.
//! - It returns a slot whose last write restores its first value, where `tx_storage_writes` drops
//!   it. The seed check (`PoolConfig` words 1 and 2 written) then holds whatever values `seed`
//!   writes.
//! - It fails on a hook change with a key or value wider than a word, where `tx_storage_writes`
//!   skips such a change.
//! - It does not check the transaction's status: its caller passes only successful transactions
//!   (`Block::transactions`), where `tx_storage_writes` returns nothing for a failed one itself.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Result};
use substreams_ethereum::pb::eth::v2::{StorageChange, TransactionTrace};

use crate::{
    address_list::Address,
    storage::{pad32, Word},
};

/// The net effect of one transaction on one storage slot of one contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotWrite {
    /// The contract whose storage the slot is.
    pub(crate) contract: Address,
    pub(crate) slot: Word,
    /// The value the slot held before the transaction's first write to it.
    pub(crate) old_value: Word,
    /// The value the transaction's last write left in the slot.
    pub(crate) new_value: Word,
}

/// The writes `tx` made to the storage of any of `contracts`, one per (contract, slot), ordered by
/// contract and slot.
///
/// `tx` is a successful transaction (`Block::transactions` yields only those); this function
/// ignores the writes of its calls whose state reverted. A slot written several times keeps the
/// `old_value` of its first write and the `new_value` of its last, by ordinal. A slot whose last
/// write restores its first `old_value` is returned with the two equal; callers compare them.
///
/// Fails when a change to one of `contracts` has a key or value longer than 32 bytes. The EVM
/// writes neither, so either means a malformed block source, and the malformed change may be a
/// `PoolKey` write that pauses the pool. A change whose address is not 20 bytes is no listed
/// contract's, so it is skipped like any other contract's.
pub(crate) fn storage_writes(
    tx: &TransactionTrace,
    contracts: &BTreeSet<Address>,
) -> Result<Vec<SlotWrite>> {
    let mut changes: Vec<(Address, &StorageChange)> = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter_map(|change| {
            Address::try_from(change.address.as_slice())
                .ok()
                .filter(|contract| contracts.contains(contract))
                .map(|contract| (contract, change))
        })
        .collect();
    // Calls are listed in call order, and a parent's writes after a nested call carry ordinals
    // above the nested call's, so only the ordinal gives execution order.
    changes.sort_unstable_by_key(|(_, change)| change.ordinal);

    let mut writes: BTreeMap<(Address, Word), SlotWrite> = BTreeMap::new();
    for (contract, change) in changes {
        let word = |field: &str, bytes: &[u8]| {
            pad32(bytes).ok_or_else(|| malformed(tx, change, field, bytes))
        };
        let slot = word("key", &change.key)?;
        let old_value = word("old value", &change.old_value)?;
        let new_value = word("new value", &change.new_value)?;
        writes
            .entry((contract, slot))
            .and_modify(|write| write.new_value = new_value)
            .or_insert(SlotWrite { contract, slot, old_value, new_value });
    }
    Ok(writes.into_values().collect())
}

/// The error for `change`, a storage change of `tx` whose `field` holds `bytes`, a width no EVM
/// write has.
fn malformed(
    tx: &TransactionTrace,
    change: &StorageChange,
    field: &str,
    bytes: &[u8],
) -> anyhow::Error {
    anyhow!(
        "transaction 0x{} has a storage change (ordinal {}) whose {field} is {} bytes",
        hex::encode(&tx.hash),
        change.ordinal,
        bytes.len()
    )
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::Call;

    use rstest::rstest;

    use super::*;
    use crate::montra::test_fixtures::{HOOK, POOL_MANAGER as OTHER};

    fn word(hex_str: &str) -> Word {
        pad32(&hex::decode(hex_str).unwrap()).unwrap()
    }

    fn change(contract: &Address, slot: &str, old: &str, new: &str, ordinal: u64) -> StorageChange {
        StorageChange {
            address: contract.to_vec(),
            key: hex::decode(slot).unwrap(),
            old_value: hex::decode(old).unwrap(),
            new_value: hex::decode(new).unwrap(),
            ordinal,
        }
    }

    fn write(contract: Address, slot: &str, old_value: Word, new_value: Word) -> SlotWrite {
        SlotWrite { contract, slot: word(slot), old_value, new_value }
    }

    fn trace(calls: Vec<Call>) -> TransactionTrace {
        TransactionTrace { calls, ..Default::default() }
    }

    fn call(storage_changes: Vec<StorageChange>, state_reverted: bool) -> Call {
        Call { storage_changes, state_reverted, ..Default::default() }
    }

    #[test]
    fn test_writes_are_netted_per_slot_by_ordinal_across_calls() {
        // The outer call writes the slot after a nested call did: the nested call is listed second
        // but has the lower ordinal.
        let tx = trace(vec![
            call(vec![change(&HOOK, "01", "bb", "cc", 9)], false),
            call(vec![change(&HOOK, "01", "aa", "bb", 4)], false),
        ]);

        assert_eq!(
            storage_writes(&tx, &BTreeSet::from([HOOK])).unwrap(),
            vec![write(HOOK, "01", word("aa"), word("cc"))]
        );
    }

    #[test]
    fn test_slot_restored_to_its_old_value_is_returned_unchanged() {
        let tx = trace(vec![call(
            vec![change(&HOOK, "01", "aa", "bb", 1), change(&HOOK, "01", "bb", "aa", 2)],
            false,
        )]);

        assert_eq!(
            storage_writes(&tx, &BTreeSet::from([HOOK])).unwrap(),
            vec![write(HOOK, "01", word("aa"), word("aa"))]
        );
    }

    #[test]
    fn test_writes_of_reverted_calls_and_other_contracts_are_ignored() {
        let tx = trace(vec![
            call(vec![change(&HOOK, "01", "aa", "bb", 1)], false),
            call(vec![change(&HOOK, "01", "bb", "ff", 2)], true),
            call(vec![change(&OTHER, "01", "aa", "ee", 3)], false),
        ]);

        assert_eq!(
            storage_writes(&tx, &BTreeSet::from([HOOK])).unwrap(),
            vec![write(HOOK, "01", word("aa"), word("bb"))]
        );
    }

    #[test]
    fn test_same_slot_of_two_contracts_is_two_writes() {
        // HOOK sorts before OTHER but writes after it: the writes come back in contract order.
        let tx = trace(vec![call(
            vec![change(&OTHER, "01", "", "0b", 1), change(&HOOK, "01", "", "0a", 2)],
            false,
        )]);

        assert_eq!(
            storage_writes(&tx, &BTreeSet::from([HOOK, OTHER])).unwrap(),
            vec![write(HOOK, "01", [0; 32], word("0a")), write(OTHER, "01", [0; 32], word("0b")),]
        );
    }

    #[rstest]
    #[case::oversized_key(change(&HOOK, &"01".repeat(33), "aa", "bb", 7), "key is 33 bytes")]
    #[case::oversized_old_value(
        change(&HOOK, "02", &"01".repeat(33), "bb", 7),
        "old value is 33 bytes"
    )]
    #[case::oversized_new_value(
        change(&HOOK, "02", "aa", &"01".repeat(34), 7),
        "new value is 34 bytes"
    )]
    fn test_malformed_storage_change_fails(#[case] malformed: StorageChange, #[case] cause: &str) {
        let mut tx = trace(vec![call(vec![change(&HOOK, "03", "aa", "bb", 3), malformed], false)]);
        tx.hash = vec![0xab; 32];

        let error = storage_writes(&tx, &BTreeSet::from([HOOK]))
            .expect_err("a malformed storage change fails");

        assert_eq!(
            error.to_string(),
            format!(
                "transaction 0x{} has a storage change (ordinal 7) whose {cause}",
                "ab".repeat(32)
            )
        );
    }

    /// A change whose address is not 20 bytes is no hook's: it is skipped, so a defective change
    /// of another contract cannot fail the block, and the hook's own writes still come back.
    #[test]
    fn test_change_with_a_malformed_address_is_skipped() {
        let short =
            StorageChange { address: HOOK[..19].to_vec(), ..change(&HOOK, "02", "aa", "bb", 7) };
        let tx = trace(vec![call(vec![change(&HOOK, "03", "aa", "bb", 3), short], false)]);

        assert_eq!(
            storage_writes(&tx, &BTreeSet::from([HOOK])).unwrap(),
            vec![write(HOOK, "03", word("aa"), word("bb"))]
        );
    }
}
