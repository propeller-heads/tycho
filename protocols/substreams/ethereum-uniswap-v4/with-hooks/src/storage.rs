//! Solidity storage-slot arithmetic and word decoding over the storage writes that extended
//! Substreams blocks carry per call.
//!
//! Every reader here is total: malformed input yields `None` rather than a panic, because a panic
//! inside a Substreams module fails the block for every consumer of the stream.

use std::collections::HashMap;

use substreams_ethereum::pb::eth::v2::{StorageChange, TransactionTrace, TransactionTraceStatus};
use tiny_keccak::{Hasher, Keccak};

/// A 32-byte EVM storage word, used for both slot keys and slot values.
pub type Word = [u8; 32];

/// Left-pads `bytes` into a 32-byte word. Returns `None` when `bytes` is longer than 32 bytes; an
/// empty input yields a zero word.
pub fn pad32(bytes: &[u8]) -> Option<Word> {
    if bytes.len() > 32 {
        return None;
    }

    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(bytes);
    Some(padded)
}

/// Returns the storage slot of `mapping(bytes32 => T)[key]` for a mapping declared at `base_slot`,
/// which Solidity places at `keccak256(key ‖ uint256(base_slot))`.
pub fn mapping_slot(key: &Word, base_slot: u64) -> Word {
    let mut base = [0u8; 32];
    base[24..].copy_from_slice(&base_slot.to_be_bytes());

    let mut hasher = Keccak::v256();
    hasher.update(key);
    hasher.update(&base);

    let mut slot = [0u8; 32];
    hasher.finalize(&mut slot);
    slot
}

/// Returns `slot + offset` as a 256-bit big-endian addition, wrapping at the 256-bit maximum —
/// the slot of a struct member stored `offset` words after `slot`.
pub fn word_at_offset(slot: &Word, offset: u64) -> Word {
    let mut slot = *slot;
    let mut carry = offset;

    for byte in slot.iter_mut().rev() {
        if carry == 0 {
            break;
        }
        let sum = u64::from(*byte) + (carry & 0xff);
        *byte = (sum & 0xff) as u8;
        carry = (carry >> 8) + (sum >> 8);
    }

    slot
}

/// Whether [`read_uint`] reads the window `width` bytes wide starting `low_byte_offset` bytes from
/// the low end of a word: `width` is 1 to 8, so the value fits a `u64`, and the window ends within
/// the word.
pub const fn is_uint_window(low_byte_offset: usize, width: usize) -> bool {
    width >= 1 && width <= 8 && low_byte_offset <= 32 - width
}

/// Reads an unsigned big-endian integer packed into `word`, `width` bytes wide and starting
/// `low_byte_offset` bytes from the low end of the word — the order Solidity packs struct members
/// into a slot. Returns `None` unless [`is_uint_window`] holds for the window.
pub fn read_uint(word: &Word, low_byte_offset: usize, width: usize) -> Option<u64> {
    if !is_uint_window(low_byte_offset, width) {
        return None;
    }

    let start = 32 - low_byte_offset - width;
    let mut value = 0u64;
    for byte in &word[start..start + width] {
        value = (value << 8) | u64::from(*byte);
    }
    Some(value)
}

/// A field packed into a storage word: `width` bytes starting `low` bytes from the low end.
#[derive(Clone, Copy)]
pub(crate) struct Field {
    low: usize,
    width: usize,
}

impl Field {
    /// The field `width` bytes wide starting `low` bytes from the low end of a word. Panics unless
    /// [`is_uint_window`] holds for the window, the check `read_uint` makes. Every field is a
    /// constant, so the panic fails the build.
    pub(crate) const fn new(low: usize, width: usize) -> Self {
        assert!(is_uint_window(low, width), "a field is a window read_uint reads");
        Self { low, width }
    }

    /// The field read as an unsigned big-endian integer.
    pub(crate) fn unsigned(self, word: &Word) -> u64 {
        read_uint(word, self.low, self.width)
            .expect("Field::new admits only windows read_uint reads")
    }

    /// The field as the 8-byte big-endian `u64` the attributes carry.
    pub(crate) fn u64_bytes(self, word: &Word) -> Vec<u8> {
        self.unsigned(word)
            .to_be_bytes()
            .to_vec()
    }
}

/// A two's-complement field packed into a storage word, at most 4 bytes wide so that it fits an
/// `i32`.
#[derive(Clone, Copy)]
pub(crate) struct SignedField(Field);

impl SignedField {
    /// The signed field `width` bytes wide starting `low` bytes from the low end of a word. Panics
    /// unless the field is at most 4 bytes wide and [`Field::new`] accepts it.
    pub(crate) const fn new(low: usize, width: usize) -> Self {
        assert!(width <= 4, "a signed field fits an i32");
        Self(Field::new(low, width))
    }

    /// The field read as a two's-complement integer of its width, sign-extended.
    pub(crate) fn read(self, word: &Word) -> i32 {
        let shift = 64 - 8 * self.0.width as u32;
        ((self.0.unsigned(word) << shift) as i64 >> shift) as i32
    }

    /// The field as the 4-byte big-endian `i32` the attributes carry.
    pub(crate) fn i32_bytes(self, word: &Word) -> Vec<u8> {
        self.read(word).to_be_bytes().to_vec()
    }
}

/// A field wider than 8 bytes, which no [`Field`] reads, kept as its big-endian bytes.
#[derive(Clone, Copy)]
pub(crate) struct BytesField {
    low: usize,
    width: usize,
}

impl BytesField {
    /// The field `width` bytes wide starting `low` bytes from the low end of a word. Panics unless
    /// it ends within the word; every field is a constant, so the panic fails the build.
    pub(crate) const fn new(low: usize, width: usize) -> Self {
        assert!(width >= 1 && low + width <= 32, "a field lies within a word");
        Self { low, width }
    }

    pub(crate) fn bytes(self, word: &Word) -> Vec<u8> {
        word[32 - self.low - self.width..32 - self.low].to_vec()
    }
}

/// A watched word of a pool's hook storage and the state attributes it holds.
pub(crate) trait StateWord: Copy {
    /// The word's position among its struct's watched words.
    fn position(self) -> usize;

    /// The attributes the word holds, as (name, value) pairs, in the formats the simulation
    /// decoder reads. A byte the simulation does not read holds none.
    fn attributes(self, value: &Word) -> Vec<(&'static str, Vec<u8>)>;
}

/// The net storage writes one transaction made to one contract, keyed by slot: the value the slot
/// held before the transaction and the value it held after it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TxStorageWrites {
    slots: HashMap<Word, (Word, Word)>,
}

impl TxStorageWrites {
    /// The value `slot` held after the transaction. Returns `None` when the transaction left the
    /// slot unchanged.
    pub fn new_value(&self, slot: &Word) -> Option<&Word> {
        self.slots
            .get(slot)
            .map(|(_, new_value)| new_value)
    }

    /// Whether the transaction changed no slot of the contract.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How many slots of the contract the transaction changed.
    pub fn len(&self) -> usize {
        self.slots.len()
    }
}

/// Collects the storage writes `contract` received in `tx`, netted per slot: the first `old_value`
/// and the last `new_value` in ascending ordinal order. Returns an empty set when `tx` did not
/// succeed, and drops slots whose final value equals the value they started with. Writes made by
/// calls whose state reverted are ignored.
///
/// Mirrors `tycho_substreams::block_storage::get_block_storage_changes` on transaction status,
/// `state_reverted`, ordinal ordering and unchanged slots, with two deliberate differences:
/// that helper reaches successful transactions through `Block::transactions()`, whereas the status
/// check here is inline so a single trace can be passed; and it keeps keys and values as the
/// variable-length bytes Firehose emitted, whereas they are left-padded to a full word here so a
/// slot compares equal to one computed with [`mapping_slot`] no matter how the chain trimmed
/// leading zeros. Keys or values longer than a word cannot describe a storage word, so such
/// changes are dropped rather than truncated.
pub fn tx_storage_writes(tx: &TransactionTrace, contract: &[u8]) -> TxStorageWrites {
    if tx.status != i32::from(TransactionTraceStatus::Succeeded) {
        return TxStorageWrites::default();
    }

    let mut changes: Vec<&StorageChange> = Vec::new();
    for call in &tx.calls {
        if call.state_reverted {
            continue;
        }
        for change in &call.storage_changes {
            if change.address == contract {
                changes.push(change);
            }
        }
    }
    changes.sort_unstable_by_key(|change| change.ordinal);

    let mut slots: HashMap<Word, (Word, Word)> = HashMap::new();
    for change in changes {
        let (Some(slot), Some(old_value), Some(new_value)) =
            (pad32(&change.key), pad32(&change.old_value), pad32(&change.new_value))
        else {
            continue;
        };
        slots
            .entry(slot)
            .and_modify(|(_, last_value)| *last_value = new_value)
            .or_insert((old_value, new_value));
    }
    slots.retain(|_, (old_value, new_value)| old_value != new_value);

    TxStorageWrites { slots }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{
        Call, StorageChange, TransactionTrace, TransactionTraceStatus,
    };

    use super::*;

    const HOOK: [u8; 20] = hex_literal::hex!("e5e702641ea86f4ae6cc3cdaed2b886f976be044");
    const OTHER: [u8; 20] = hex_literal::hex!("8366a39cc670b4001a1121b8f6a443a643e40951");

    // Word 4 of a registered Pons launch (plan-task-1.md, pool 0xc96847cc…): creatorTaxBps 100 at
    // low byte 20, hookFeeBps 100 at low byte 26.
    const LAUNCH_WORD_4: &str =
        "0x0000012c006413880bb80064263ed295dafae1d9aadd6e56c4b6f9f38ee019dd";

    /// Left-pads a hex literal into a word, mirroring how storage values arrive on the wire.
    fn word(hex_str: &str) -> Word {
        let bytes = hex::decode(hex_str.trim_start_matches("0x")).expect("test fixture is hex");
        pad32(&bytes).expect("test fixture fits a word")
    }

    fn change(address: &[u8], key: &[u8], old: &[u8], new: &[u8], ordinal: u64) -> StorageChange {
        StorageChange {
            address: address.to_vec(),
            key: key.to_vec(),
            old_value: old.to_vec(),
            new_value: new.to_vec(),
            ordinal,
        }
    }

    fn succeeded_tx(calls: Vec<Call>) -> TransactionTrace {
        TransactionTrace {
            status: i32::from(TransactionTraceStatus::Succeeded),
            calls,
            ..Default::default()
        }
    }

    fn call(storage_changes: Vec<StorageChange>) -> Call {
        Call { storage_changes, ..Default::default() }
    }

    fn reverted_call(storage_changes: Vec<StorageChange>) -> Call {
        Call { storage_changes, state_reverted: true, ..Default::default() }
    }

    #[test]
    fn reads_packed_launch_info_fields() {
        let w = word(LAUNCH_WORD_4);

        assert_eq!(read_uint(&w, 20, 2), Some(100), "creatorTaxBps");
        assert_eq!(read_uint(&w, 26, 2), Some(100), "hookFeeBps");
        assert_eq!(read_uint(&w, 28, 2), Some(300));
    }

    #[test]
    fn reads_registered_flag_from_low_byte() {
        // Word 0 of launches[0x18c178f4…]; see assets/pons-launches-storage.json.
        let w = word("0x00000000000000000000b7667b0d7f70002c5ba451a58b7e98ca8582e76d0001");

        assert_eq!(read_uint(&w, 0, 1), Some(1));
    }

    #[test]
    fn reads_a_zero_valued_field_from_an_onchain_launch_word() {
        // Word 4 of launches[0x18c178f4…]; see assets/pons-launches-storage.json.
        let w = word("0x0000012c006413880bb80000263ed295dafae1d9aadd6e56c4b6f9f38ee019dd");

        assert_eq!(read_uint(&w, 20, 2), Some(0), "creatorTaxBps");
        assert_eq!(read_uint(&w, 26, 2), Some(100), "hookFeeBps");
    }

    #[test]
    fn read_uint_rejects_out_of_range_windows() {
        let w = word(LAUNCH_WORD_4);

        assert_eq!(read_uint(&w, 0, 0), None, "zero width");
        assert_eq!(read_uint(&w, 0, 9), None, "wider than u64");
        assert_eq!(read_uint(&w, 31, 2), None, "offset + width past the word");
        assert_eq!(read_uint(&w, 32, 1), None, "offset past the word");
        assert_eq!(read_uint(&w, usize::MAX, 1), None, "offset + width overflows");
        assert_eq!(read_uint(&w, 24, 8), Some(0x0000_012c_0064_1388), "widest window");
    }

    #[test]
    fn mapping_slot_matches_observed_pons_base_slot() {
        let pool_id = word("0x18c178f47be974b35b88b5f58a452d745b5410764c7fa114dd6cfebe86d3b46d");

        assert_eq!(
            hex::encode(mapping_slot(&pool_id, 10)),
            "e37c20a04046455d80c1bb1695b0374864ee27081b38911bef0ef8155a911965"
        );
    }

    #[test]
    fn mapping_slot_matches_known_keccak_vector() {
        // keccak256 of 64 zero bytes.
        assert_eq!(
            hex::encode(mapping_slot(&[0u8; 32], 0)),
            "ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"
        );
    }

    #[test]
    fn word_at_offset_adds_at_the_low_end() {
        let base = word("0xe37c20a04046455d80c1bb1695b0374864ee27081b38911bef0ef8155a911965");

        assert_eq!(
            hex::encode(word_at_offset(&base, 4)),
            "e37c20a04046455d80c1bb1695b0374864ee27081b38911bef0ef8155a911969"
        );
        assert_eq!(word_at_offset(&base, 0), base);
    }

    #[test]
    fn word_at_offset_carries_across_byte_boundaries() {
        let mut base = [0u8; 32];
        base[30] = 0x01;
        base[31] = 0xff;

        let mut expected = [0u8; 32];
        expected[30] = 0x02;
        expected[31] = 0x00;

        assert_eq!(word_at_offset(&base, 1), expected);
    }

    #[test]
    fn word_at_offset_wraps_at_the_word_maximum() {
        assert_eq!(word_at_offset(&[0xffu8; 32], 1), [0u8; 32]);
    }

    #[test]
    fn pad32_left_pads_short_input() {
        let padded = pad32(&[0x01, 0x02]).expect("two bytes fit");

        assert_eq!(padded[..30], [0u8; 30]);
        assert_eq!(padded[30..], [0x01, 0x02]);
    }

    #[test]
    fn pad32_keeps_a_full_word_unchanged() {
        let full = [0x7au8; 32];

        assert_eq!(pad32(&full), Some(full));
    }

    #[test]
    fn pad32_rejects_oversized_input() {
        assert_eq!(pad32(&[0u8; 33]), None);
    }

    #[test]
    fn pad32_accepts_empty_input() {
        assert_eq!(pad32(&[]), Some([0u8; 32]));
    }

    #[test]
    fn highest_ordinal_wins_and_first_old_value_is_kept() {
        let slot = word("0x01");
        let tx = succeeded_tx(vec![call(vec![
            change(&HOOK, &slot, &word("0xbb"), &word("0xcc"), 7),
            change(&HOOK, &slot, &word("0xaa"), &word("0xbb"), 3),
        ])]);

        let writes = tx_storage_writes(&tx, &HOOK);

        assert_eq!(writes.len(), 1);
        assert_eq!(writes.slots.get(&slot), Some(&(word("0xaa"), word("0xcc"))));
        assert_eq!(writes.new_value(&slot), Some(&word("0xcc")));
    }

    #[test]
    fn reverted_calls_are_ignored() {
        let slot = word("0x01");
        let tx = succeeded_tx(vec![reverted_call(vec![change(
            &HOOK,
            &slot,
            &word("0xaa"),
            &word("0xbb"),
            1,
        )])]);

        let writes = tx_storage_writes(&tx, &HOOK);

        assert!(writes.is_empty());
        assert_eq!(writes.new_value(&slot), None);
    }

    #[test]
    fn a_reverted_call_does_not_hide_a_committed_one() {
        let slot = word("0x01");
        let tx = succeeded_tx(vec![
            call(vec![change(&HOOK, &slot, &word("0xaa"), &word("0xbb"), 1)]),
            reverted_call(vec![change(&HOOK, &slot, &word("0xbb"), &word("0xff"), 2)]),
        ]);

        let writes = tx_storage_writes(&tx, &HOOK);

        assert_eq!(writes.new_value(&slot), Some(&word("0xbb")));
    }

    #[test]
    fn writes_of_other_contracts_are_ignored() {
        let hook_slot = word("0x01");
        let other_slot = word("0x02");
        let tx = succeeded_tx(vec![call(vec![
            change(&OTHER, &other_slot, &word("0xaa"), &word("0xbb"), 1),
            change(&HOOK, &hook_slot, &word("0xcc"), &word("0xdd"), 2),
        ])]);

        let writes = tx_storage_writes(&tx, &HOOK);

        assert_eq!(writes.len(), 1);
        assert_eq!(writes.new_value(&hook_slot), Some(&word("0xdd")));
        assert_eq!(writes.new_value(&other_slot), None);
    }

    #[test]
    fn contract_is_matched_on_raw_bytes_not_hex_case() {
        let slot = word("0x01");
        let tx =
            succeeded_tx(vec![call(vec![change(&HOOK, &slot, &word("0xaa"), &word("0xbb"), 1)])]);

        let uppercase_hex =
            hex::decode("E5E702641EA86F4AE6CC3CDAED2B886F976BE044").expect("hook address is hex");

        assert_eq!(tx_storage_writes(&tx, &uppercase_hex).len(), 1);
    }

    #[test]
    fn slots_restored_to_their_original_value_are_dropped() {
        let slot = word("0x01");
        let tx = succeeded_tx(vec![call(vec![
            change(&HOOK, &slot, &word("0xaa"), &word("0xbb"), 1),
            change(&HOOK, &slot, &word("0xbb"), &word("0xaa"), 2),
        ])]);

        let writes = tx_storage_writes(&tx, &HOOK);

        assert!(writes.is_empty());
    }

    #[test]
    fn unsuccessful_transactions_yield_no_writes() {
        let slot = word("0x01");
        let changes = vec![change(&HOOK, &slot, &word("0xaa"), &word("0xbb"), 1)];

        for status in [
            TransactionTraceStatus::Unknown,
            TransactionTraceStatus::Failed,
            TransactionTraceStatus::Reverted,
        ] {
            let tx = TransactionTrace {
                status: i32::from(status),
                calls: vec![call(changes.clone())],
                ..Default::default()
            };

            assert!(tx_storage_writes(&tx, &HOOK).is_empty(), "{status:?}");
        }
    }

    #[test]
    fn short_keys_and_values_are_left_padded() {
        let tx = succeeded_tx(vec![call(vec![change(&HOOK, &[0x01], &[], &[0x02], 1)])]);

        let writes = tx_storage_writes(&tx, &HOOK);

        assert_eq!(writes.new_value(&word("0x01")), Some(&word("0x02")));
    }

    #[test]
    fn oversized_keys_and_values_are_ignored() {
        let tx = succeeded_tx(vec![call(vec![change(
            &HOOK,
            &[0x01; 33],
            &word("0xaa"),
            &word("0xbb"),
            1,
        )])]);

        assert!(tx_storage_writes(&tx, &HOOK).is_empty());
    }

    #[test]
    fn a_transaction_without_calls_yields_no_writes() {
        assert!(tx_storage_writes(&succeeded_tx(vec![]), &HOOK).is_empty());
    }
}
