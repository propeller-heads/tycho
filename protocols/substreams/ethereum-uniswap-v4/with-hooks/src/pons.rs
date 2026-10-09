//! Decoding of the Pons V2 MemeHook fee terms. Registering a pool freezes them in the hook's
//! `launches` mapping; this turns the storage writes that made them into the static attributes a
//! simulation decoder needs to price a swap.

use substreams::scalar::BigInt;
use tycho_substreams::prelude::*;

use crate::storage::{mapping_slot, read_uint, word_at_offset, TxStorageWrites, Word};

/// Base storage slot of `mapping(PoolId => LaunchInfo) public launches` in the deployed V2MemeHook.
pub const PONS_LAUNCHES_SLOT: u64 = 10;

/// Value of the `hook_identifier` attribute that marks a component as a Pons V2 pool.
pub const PONS_HOOK_IDENTIFIER: &str = "pons_v2";

/// Ceiling `_registerPool` enforces on `hookFeeBps`, in basis points. It bounds that term alone;
/// `creatorTaxBps` is bounded only through [`MAX_TOTAL_TRADE_FEE_BPS`].
pub const MAX_HOOK_FEE_BPS: u64 = 1_000;

/// Ceiling `_registerPool` enforces on `creatorTaxBps + hookFeeBps`, in basis points. A pool
/// taking more than the whole unspecified leg of a swap would flip the swapper's output negative.
pub const MAX_TOTAL_TRADE_FEE_BPS: u64 = 2_000;

// `struct LaunchInfo` packs from the low end of each word and opens a new one whenever the next
// member would not fit. Word 0 holds `registered` (byte 0), `memecoinIsCurrency0` (byte 1) and
// `memecoin` (bytes 2..22), leaving no room for another address; words 1, 2 and 3 therefore hold
// `quoteToken`, `creator` and `buybackCreatorRecipient`, one 20-byte address each. Word 4 opens
// with `protocolFeeRecipient` (bytes 0..20) and then fits every remaining member: the five
// `uint16`s `creatorTaxBps` (20..22), `protocolFeeShareBps` (22..24), `buybackBurnBps` (24..26),
// `hookFeeBps` (26..28) and `maxInternalPriceImpactBps` (28..30), then `buybackEnabled` (byte 30).
const FEE_TERMS_WORD: u64 = 4;
const REGISTERED_OFFSET: usize = 0;
const REGISTERED_WIDTH: usize = 1;
const CREATOR_TAX_BPS_OFFSET: usize = 20;
const HOOK_FEE_BPS_OFFSET: usize = 26;
const BPS_WIDTH: usize = 2;

/// Decodes the fee terms `registerPool` froze for `pool_id` and returns them as the static
/// attributes `pons_hook_fee_bps` and `pons_creator_tax_bps`, in that order, each an unsigned
/// big-endian integer carrying [`ChangeType::Creation`].
///
/// Returns `None`, logging the pool id and the reason, when `writes` does not show the pool being
/// registered, when the word holding the fee terms went unwritten, or when the hook fee or the two
/// terms' sum exceeds the bounds `_registerPool` itself enforces, [`MAX_HOOK_FEE_BPS`] and
/// [`MAX_TOTAL_TRADE_FEE_BPS`]. Never substitutes a default for a term it could not read.
pub fn pons_static_attributes(pool_id: &Word, writes: &TxStorageWrites) -> Option<Vec<Attribute>> {
    let base = mapping_slot(pool_id, PONS_LAUNCHES_SLOT);

    let Some(registration_word) = writes.new_value(&base) else {
        substreams::log::info!(
            "pons: pool {} has no write to launches word 0, so it was not registered here",
            hex::encode(pool_id)
        );
        return None
    };
    // Every offset below is a compile-time constant window inside a 32-byte word, so `read_uint`
    // cannot reject it; `?` keeps the function total without a branch that can never be taken.
    let registered = read_uint(registration_word, REGISTERED_OFFSET, REGISTERED_WIDTH)?;
    if registered != 1 {
        substreams::log::info!(
            "pons: pool {} left launches.registered at {}, expected 1",
            hex::encode(pool_id),
            registered
        );
        return None
    }

    let Some(fee_terms_word) = writes.new_value(&word_at_offset(&base, FEE_TERMS_WORD)) else {
        substreams::log::info!(
            "pons: pool {} was registered without writing the fee terms in launches word 4",
            hex::encode(pool_id)
        );
        return None
    };
    let creator_tax_bps = read_uint(fee_terms_word, CREATOR_TAX_BPS_OFFSET, BPS_WIDTH)?;
    let hook_fee_bps = read_uint(fee_terms_word, HOOK_FEE_BPS_OFFSET, BPS_WIDTH)?;

    if hook_fee_bps > MAX_HOOK_FEE_BPS || hook_fee_bps + creator_tax_bps > MAX_TOTAL_TRADE_FEE_BPS {
        substreams::log::info!(
            "pons: pool {} has out-of-range fee terms: hookFeeBps {}, creatorTaxBps {}",
            hex::encode(pool_id),
            hook_fee_bps,
            creator_tax_bps
        );
        return None
    }

    Some(vec![
        uint_attribute("pons_hook_fee_bps", hook_fee_bps),
        uint_attribute("pons_creator_tax_bps", creator_tax_bps),
    ])
}

fn uint_attribute(name: &str, value: u64) -> Attribute {
    Attribute {
        name: name.to_string(),
        value: BigInt::from(value).to_signed_bytes_be(),
        change: ChangeType::Creation.into(),
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{
        Call, StorageChange, TransactionTrace, TransactionTraceStatus,
    };

    use super::*;
    use crate::storage::{mapping_slot, pad32, tx_storage_writes, word_at_offset};

    const PONS_HOOK: [u8; 20] = hex_literal::hex!("e5e702641ea86f4ae6cc3cdaed2b886f976be044");

    // Two registered pools read off Robinhood (chain 4663); the raw `eth_getStorageAt` responses
    // behind all four words are in assets/pons-launches-storage.json.
    const POOL_100_100: &str = "0xc96847cc43f7595aafcbc1c99d335cb91be7ce1107524c87030f48a716a5f289";
    const WORD_0_100_100: &str =
        "0x00000000000000000000ab5983fe30f186055095305c862b0e097dab3b520101";
    const WORD_4_100_100: &str =
        "0x0000012c006413880bb80064263ed295dafae1d9aadd6e56c4b6f9f38ee019dd";
    const POOL_0_100: &str = "0x18c178f47be974b35b88b5f58a452d745b5410764c7fa114dd6cfebe86d3b46d";
    const WORD_0_0_100: &str = "0x00000000000000000000b7667b0d7f70002c5ba451a58b7e98ca8582e76d0001";
    const WORD_4_0_100: &str = "0x0000012c006413880bb80000263ed295dafae1d9aadd6e56c4b6f9f38ee019dd";

    fn word(hex_str: &str) -> Word {
        let bytes = hex::decode(hex_str.trim_start_matches("0x")).expect("test fixture is hex");
        pad32(&bytes).expect("test fixture fits a word")
    }

    /// Builds word 4 of a `LaunchInfo` holding only the two fee terms this decoder reads, placed
    /// at low-end byte offsets 20 and 26 — absolute indices 10..12 and 4..6.
    fn fee_terms_word(creator_tax_bps: u16, hook_fee_bps: u16) -> Word {
        let mut word = [0u8; 32];
        word[10..12].copy_from_slice(&creator_tax_bps.to_be_bytes());
        word[4..6].copy_from_slice(&hook_fee_bps.to_be_bytes());
        word
    }

    /// Builds word 0 of a `LaunchInfo` by setting `registered` over an observed word. Keeping the
    /// rest of the word is what makes a cleared flag a real write: a word holding nothing but a
    /// zero flag equals what an untouched slot already holds, and nets out to no write at all.
    fn registration_word(registered: u8) -> Word {
        let mut word = word(WORD_0_100_100);
        word[31] = registered;
        word
    }

    fn storage_change(slot: &Word, new_value: &Word, ordinal: u64) -> StorageChange {
        StorageChange {
            address: PONS_HOOK.to_vec(),
            key: slot.to_vec(),
            old_value: vec![0u8; 32],
            new_value: new_value.to_vec(),
            ordinal,
        }
    }

    /// The writes a transaction made to `launches[pool_id]`, one `(word offset, value)` per entry.
    fn launch_writes(pool_id: &Word, words: &[(u64, Word)]) -> TxStorageWrites {
        tx_storage_writes(&registration_tx(pool_id, words, false), &PONS_HOOK)
    }

    /// The same writes, made by a call whose state reverted.
    fn reverted_launch_writes(pool_id: &Word, words: &[(u64, Word)]) -> TxStorageWrites {
        tx_storage_writes(&registration_tx(pool_id, words, true), &PONS_HOOK)
    }

    fn registration_tx(pool_id: &Word, words: &[(u64, Word)], reverted: bool) -> TransactionTrace {
        let base = mapping_slot(pool_id, PONS_LAUNCHES_SLOT);
        let storage_changes = words
            .iter()
            .enumerate()
            .map(|(index, (offset, value))| {
                storage_change(&word_at_offset(&base, *offset), value, index as u64)
            })
            .collect();

        TransactionTrace {
            status: i32::from(TransactionTraceStatus::Succeeded),
            calls: vec![Call { storage_changes, state_reverted: reverted, ..Default::default() }],
            ..Default::default()
        }
    }

    fn attribute<'a>(attributes: &'a [Attribute], name: &str) -> &'a Attribute {
        attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
    }

    #[test]
    fn decodes_fee_terms_from_onchain_registration_writes() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, word(WORD_0_100_100)), (4, word(WORD_4_100_100))]);

        let attributes = pons_static_attributes(&pool_id, &writes).expect("pool is registered");

        assert_eq!(
            attributes
                .iter()
                .map(|attribute| attribute.name.as_str())
                .collect::<Vec<_>>(),
            ["pons_hook_fee_bps", "pons_creator_tax_bps"]
        );
        assert_eq!(attribute(&attributes, "pons_hook_fee_bps").value, vec![0x64]);
        assert_eq!(attribute(&attributes, "pons_creator_tax_bps").value, vec![0x64]);
        for attr in &attributes {
            assert_eq!(attr.change, i32::from(ChangeType::Creation), "{}", attr.name);
        }
    }

    #[test]
    fn decodes_a_zero_creator_tax_from_onchain_registration_writes() {
        let pool_id = word(POOL_0_100);
        let writes = launch_writes(&pool_id, &[(0, word(WORD_0_0_100)), (4, word(WORD_4_0_100))]);

        let attributes = pons_static_attributes(&pool_id, &writes).expect("pool is registered");

        assert_eq!(attribute(&attributes, "pons_hook_fee_bps").value, vec![0x64]);
        // `BigInt::from(0).to_signed_bytes_be()` yields a single zero byte, not an empty slice.
        assert_eq!(attribute(&attributes, "pons_creator_tax_bps").value, vec![0x00]);
    }

    #[test]
    fn a_missing_registration_word_yields_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes = launch_writes(&pool_id, &[(4, word(WORD_4_100_100))]);

        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn a_cleared_registered_flag_yields_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(0)), (4, word(WORD_4_100_100))]);

        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn a_missing_fee_terms_word_yields_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes = launch_writes(&pool_id, &[(0, word(WORD_0_100_100))]);

        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn a_hook_fee_above_the_contract_bound_yields_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(1)), (4, fee_terms_word(0, 1001))]);

        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn terms_of_one_thousand_each_are_accepted() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(1)), (4, fee_terms_word(1000, 1000))]);

        let attributes =
            pons_static_attributes(&pool_id, &writes).expect("1000 + 1000 is within both bounds");

        // 1000 = 0x03e8, whose high bit is clear, so it needs no sign byte.
        assert_eq!(attribute(&attributes, "pons_hook_fee_bps").value, vec![0x03, 0xe8]);
        assert_eq!(attribute(&attributes, "pons_creator_tax_bps").value, vec![0x03, 0xe8]);
    }

    #[test]
    fn a_creator_tax_taking_the_whole_total_is_accepted() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(1)), (4, fee_terms_word(2000, 0))]);

        let attributes = pons_static_attributes(&pool_id, &writes)
            .expect("the contract caps only the hook fee, not the creator tax");

        assert_eq!(attribute(&attributes, "pons_creator_tax_bps").value, vec![0x07, 0xd0]);
        assert_eq!(attribute(&attributes, "pons_hook_fee_bps").value, vec![0x00]);
    }

    #[test]
    fn a_creator_tax_above_the_total_bound_yields_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(1)), (4, fee_terms_word(2001, 0))]);

        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn terms_summing_past_the_total_bound_yield_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(1)), (4, fee_terms_word(2000, 1))]);

        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn writes_made_by_a_reverted_call_yield_no_attributes() {
        let pool_id = word(POOL_100_100);
        let writes = reverted_launch_writes(
            &pool_id,
            &[(0, word(WORD_0_100_100)), (4, word(WORD_4_100_100))],
        );

        assert!(writes.is_empty());
        assert_eq!(pons_static_attributes(&pool_id, &writes), None);
    }

    #[test]
    fn a_two_byte_term_is_encoded_with_a_sign_byte() {
        let pool_id = word(POOL_100_100);
        let writes =
            launch_writes(&pool_id, &[(0, registration_word(1)), (4, fee_terms_word(200, 200))]);

        let attributes = pons_static_attributes(&pool_id, &writes).expect("200 + 200 is in range");

        assert_eq!(BigInt::from(200u64).to_signed_bytes_be(), vec![0x00, 0xc8]);
        assert_eq!(attribute(&attributes, "pons_hook_fee_bps").value, vec![0x00, 0xc8]);
        assert_eq!(attribute(&attributes, "pons_creator_tax_bps").value, vec![0x00, 0xc8]);
    }
}
