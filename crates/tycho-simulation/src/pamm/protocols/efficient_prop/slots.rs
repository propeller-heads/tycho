//! The slot rule: where a listing's quote sits in its maker's desk contract, on a chain whose
//! builders land makers' updates (Ethereum).
//!
//! A maker has one desk contract, [`SlotRule::account`] of the desk id the listing is bound to.
//! Its storage is laid out as follows, and no other slot is ever written:
//! - slot 1, the desk word: [0-159] the pusher, [160-190] the desk epoch, [224] stopped, [225-255]
//!   the block of the last stop; slots 2 and 3 name the registry and the word key. Only the front's
//!   operations write them;
//! - slot `0x100 + i`, pair `i`'s quote word: [0-31] the stamp, the block that wrote it, [32-62]
//!   the desk epoch it was written under, [201] carry, [205] firm state;
//! - slot `0x200 + i`, pair `i`'s firm word, which counts only where the quote word's bit 205 is
//!   set.
//!
//! `i` is the listing's word index, from the front's `doorView`. A state map's answer covers a
//! listing only where it writes the listing's quote word with a stamp the front fills at the map's
//! block ([`fills_at`]), under the desk epoch the chain's head holds, on a desk the
//! head has not stopped, and writes no desk-level slot ([`ListingSlots::is_foreign`]). The desk
//! word is read from the chain's head, never from an answer: a pair's word shows neither a stop
//! nor the epoch of a later revive. Only the listing's own slots are taken from the answer
//! ([`super::overrides::NextBlock::take`]); several listings may share one desk contract, each
//! with its own pair of slots.

use std::collections::BTreeSet;

use alloy::{
    primitives::{Address, B256, U256},
    providers::bindings::IMulticall3::Call3,
    sol_types::SolCall,
};

use crate::pamm::protocols::efficient_prop::reader::{call, deskCellOfCall};

/// The rule the reader resolves every listing's desk contract and slots by.
pub(super) const SLOT_RULE: SlotRule = SlotRule::Desk;

/// The desk word's slot.
const DESK_WORD_SLOT: u64 = 1;
/// Pair `i`'s quote word is at this plus `i`.
const QUOTE_BASE: u64 = 0x100;
/// Pair `i`'s firm word is at this plus `i`.
const FIRM_BASE: u64 = 0x200;
/// The quote word's carry bit: the word may fill in the block after its landing.
const CARRY_BIT: usize = 201;
/// The quote word's firm state bit: the firm word counts.
const FIRM_BIT: usize = 205;
/// The desk word's stopped bit.
const STOPPED_BIT: usize = 224;
/// A desk contract's address is the CREATE2 child of the front at this flag or'd with the desk id.
const DESK_SALT_FLAG: usize = 252;

/// How the front places a listing's quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotRule {
    /// One desk contract per maker holding a pair of slots per listing, at fixed indices.
    Desk,
}

impl SlotRule {
    /// The front's call naming the desk contract of desk id `desk`: the check of
    /// [`Self::account`].
    pub(super) fn place_call(self, front: Address, desk: u32) -> Call3 {
        match self {
            Self::Desk => call(front, deskCellOfCall { desk }.abi_encode()),
        }
    }

    /// The desk contract of desk id `desk`: the CREATE2 child of `front` at salt
    /// `(1 << 252) | desk` under the desk contract's creation code hash `cell_codehash`.
    pub(super) fn account(self, front: Address, cell_codehash: B256, desk: u32) -> Address {
        match self {
            Self::Desk => front.create2(
                B256::from((U256::from(1u8) << DESK_SALT_FLAG) | U256::from(desk)),
                cell_codehash,
            ),
        }
    }

    /// The desk word's slot, read at the chain's head.
    pub(super) fn head_slot(self) -> U256 {
        match self {
            Self::Desk => U256::from(DESK_WORD_SLOT),
        }
    }

    /// The slots of pair `word_index` of a desk contract: its quote word's and its firm word's,
    /// each read at the chain's head too.
    pub(super) fn pair_slots(self, word_index: u8) -> (U256, U256) {
        match self {
            Self::Desk => (
                U256::from(QUOTE_BASE + u64::from(word_index)),
                U256::from(FIRM_BASE + u64::from(word_index)),
            ),
        }
    }

    /// The listing at word index `word_index` of the desk contract `account`, from the front's
    /// answer to [`Self::place_call`] and, read from `account` at the chain's head, the desk word
    /// `head` and the pair's own words `head_quote` and `head_firm`; `None` where the answer does
    /// not decode or names another contract than `account`, which the configured creation code
    /// hash then does not give.
    pub(super) fn slots(
        self,
        answer: &[u8],
        account: Address,
        word_index: u8,
        head: HeadWords,
    ) -> Option<ListingSlots> {
        match self {
            Self::Desk => {
                (deskCellOfCall::abi_decode_returns(answer).ok()? == account).then(|| {
                    ListingSlots {
                        account,
                        quote_slot: pair_slot(QUOTE_BASE, word_index),
                        firm_slot: pair_slot(FIRM_BASE, word_index),
                        head: DeskHead::of(head.desk),
                        head_quote: head.quote,
                        head_firm: head.firm,
                    }
                })
            }
        }
    }
}

/// What a listing's cover reads of its desk contract at the chain's head: the desk word and the
/// pair's quote and firm words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HeadWords {
    pub desk: U256,
    pub quote: B256,
    pub firm: B256,
}

fn pair_slot(base: u64, word_index: u8) -> B256 {
    B256::from(U256::from(base + u64::from(word_index)))
}

/// The head's desk word, as far as a listing's cover needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeskHead {
    /// The desk epoch, bits 160-190: all 31 bits.
    pub epoch: u32,
    /// Whether the desk is stopped, bit 224.
    pub stopped: bool,
}

impl DeskHead {
    pub(super) fn of(word: U256) -> Self {
        let epoch: U256 = (word >> 160usize) & U256::from(0x7fff_ffffu32);
        Self { epoch: epoch.to(), stopped: word.bit(STOPPED_BIT) }
    }
}

/// Where one listing's quote sits: its desk contract, its two slots there and the head's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListingSlots {
    /// The desk contract, the account its maker's updates write.
    pub account: Address,
    /// The quote word's slot, `0x100 + i`: the listing's stamp is in its low 32 bits.
    pub quote_slot: B256,
    /// The firm word's slot, `0x200 + i`.
    pub firm_slot: B256,
    /// The desk word at the chain's head.
    pub head: DeskHead,
    /// The pair's quote word and firm word at the chain's head: what a word stamped with the head
    /// block must equal to be carried into the next block.
    pub head_quote: B256,
    pub head_firm: B256,
}

impl ListingSlots {
    /// Whether the word was written under the desk epoch of the head: its bits 32-62 equal the
    /// desk word's bits 160-190, all 31 bits. A word of another epoch is empty.
    pub(super) fn under_head_epoch(&self, word: &B256) -> bool {
        let epoch: U256 = (U256::from_be_bytes(word.0) >> 32usize) & U256::from(0x7fff_ffffu32);
        epoch == U256::from(self.head.epoch)
    }

    /// Whether the word `word` the answer names for the block `number` is held: stamped that
    /// block, a landing of the answer's own; or, stamped the block before (a word carried into
    /// `number`), equal byte for byte to the head's quote word, and, where its firm bit is set,
    /// with `firm`, the answer's firm word where it writes one, equal to the head's too. No landing
    /// in `number` can write a stamp of the block before, so only the head can hold such a word:
    /// where it holds another, or none, the fill in `number` reads that.
    pub(super) fn holds(&self, word: &B256, firm: Option<&B256>, number: u64) -> bool {
        if u64::from(stamp(word)) == number & u64::from(u32::MAX) {
            return true;
        }
        *word == self.head_quote &&
            (!U256::from_be_bytes(word.0).bit(FIRM_BIT) ||
                firm.is_none_or(|firm| *firm == self.head_firm))
    }

    /// The slots the listing takes from the answer that covers it with quote word `word`: the
    /// quote word, and the firm word only where the quote word's bit 205 is set.
    pub(super) fn claims(&self, word: &B256) -> BTreeSet<B256> {
        let mut claims = BTreeSet::from([self.quote_slot]);
        if U256::from_be_bytes(word.0).bit(FIRM_BIT) {
            claims.insert(self.firm_slot);
        }
        claims
    }
}

/// Whether the front fills the quote word `word` at block `number`: stamped that block, or the
/// block before where the listing carries (`carries`: its carry life is set and its fence is not
/// NONE) and the word does (its bit 201). The stamp keeps a block number's low 32 bits, and the
/// front counts its age modulo 2^32.
pub(super) fn fills_at(word: &B256, number: u64, carries: bool) -> bool {
    let age = number.wrapping_sub(u64::from(stamp(word))) & u64::from(u32::MAX);
    age == 0 || (age == 1 && carries && U256::from_be_bytes(word.0).bit(CARRY_BIT))
}

/// The block stamp of a quote word, a block number's low 32 bits.
fn stamp(word: &B256) -> u32 {
    u32::from_be_bytes([word[28], word[29], word[30], word[31]])
}

/// Whether `slot` is a pair's quote or firm word, `0x100 + i` or `0x200 + i` for some `i` of 0 to
/// 255. Any other slot of a desk contract (slots 0 to 3 among them) is the front's.
pub(super) fn is_pair_slot(slot: &B256) -> bool {
    let slot = U256::from_be_bytes(slot.0);
    [QUOTE_BASE, FIRM_BASE]
        .into_iter()
        .any(|base| slot >= U256::from(base) && slot <= U256::from(base + 0xff))
}

#[cfg(test)]
mod tests {
    use alloy::sol_types::SolValue;
    use rstest::rstest;

    use super::*;
    use crate::pamm::protocols::efficient_prop::test_support::{
        quote_word, vector_desk_word, VECTOR_WORDS,
    };

    const FRONT: Address = Address::repeat_byte(0xf);
    const CODEHASH: B256 = B256::repeat_byte(0xcc);

    fn head_words(desk: U256) -> HeadWords {
        HeadWords { desk, quote: B256::ZERO, firm: B256::ZERO }
    }

    fn desk_word(epoch: u32) -> U256 {
        vector_desk_word(epoch)
    }

    fn word(hex: &str) -> B256 {
        hex.parse().unwrap()
    }

    fn listing(word_index: u8, head: U256) -> ListingSlots {
        let account = SLOT_RULE.account(FRONT, CODEHASH, 1);
        SLOT_RULE
            .slots(&account.abi_encode(), account, word_index, head_words(head))
            .unwrap()
    }

    /// The desk contract is the front's CREATE2 child at salt `(1 << 252) | desk`, and the front's
    /// `deskCellOf` of the desk id is the call that checks it.
    #[test]
    fn the_desk_contract_is_the_fronts_create2_child_at_the_desk_salt() {
        let salt = B256::from((U256::from(1u8) << 252usize) | U256::from(0x2au32));

        let account = SLOT_RULE.account(FRONT, CODEHASH, 0x2a);
        let place = SLOT_RULE.place_call(FRONT, 0x2a);

        assert_eq!(account, FRONT.create2(salt, CODEHASH));
        assert_ne!(account, SLOT_RULE.account(FRONT, CODEHASH, 0x2b));
        assert_eq!(place.target, FRONT);
        assert_eq!(place.callData.as_ref(), deskCellOfCall { desk: 0x2a }.abi_encode());
        assert_eq!(SLOT_RULE.head_slot(), U256::from(1u8));
    }

    /// The front's answer must name the derived contract, else the listing is not placed.
    #[test]
    fn a_front_naming_another_contract_places_nothing() {
        let account = SLOT_RULE.account(FRONT, CODEHASH, 1);

        assert!(SLOT_RULE
            .slots(&Address::repeat_byte(1).abi_encode(), account, 0, head_words(U256::ZERO))
            .is_none());
        assert!(SLOT_RULE
            .slots(&[1, 2], account, 0, head_words(U256::ZERO))
            .is_none());
    }

    /// A listing's slots are `0x100 + i` and `0x200 + i`, at the ends of the range too.
    #[rstest]
    #[case::slot_0(0, 0x100, 0x200)]
    #[case::slot_7(7, 0x107, 0x207)]
    #[case::slot_255(255, 0x1ff, 0x2ff)]
    fn a_listings_slots_are_its_pairs_quote_and_firm_words(
        #[case] i: u8,
        #[case] quote: u64,
        #[case] firm: u64,
    ) {
        let listing = listing(i, desk_word(0));

        assert_eq!(listing.quote_slot, B256::from(U256::from(quote)));
        assert_eq!(listing.firm_slot, B256::from(U256::from(firm)));
    }

    /// The desk word's epoch is all of bits 160-190, and its stop is bit 224, whatever its other
    /// bits hold.
    #[test]
    fn the_head_is_the_desk_words_epoch_and_stop() {
        let stopped = desk_word(3) | (U256::from(1u8) << 224usize) | (U256::from(99u8) << 225usize);
        // Bit 191 is no part of the epoch.
        let above = desk_word(0) | (U256::from(1u8) << 191usize);

        assert_eq!(
            DeskHead::of(desk_word(16_847_216)),
            DeskHead { epoch: 16_847_216, stopped: false }
        );
        assert_eq!(DeskHead::of(stopped), DeskHead { epoch: 3, stopped: true });
        assert_eq!(DeskHead::of(above), DeskHead { epoch: 0, stopped: false });
    }

    /// The vectors' words are covered under their own desk epoch at their own block, and at no
    /// block before or two after.
    #[rstest]
    #[case::slot_0_epoch_0(0)]
    #[case::slot_7_epoch_3(1)]
    #[case::slot_255_epoch_16_847_216(2)]
    fn the_vectors_words_are_under_their_epoch_and_fill_at_their_block(#[case] index: usize) {
        let (i, epoch, hex, number) = VECTOR_WORDS[index];
        let listing = listing(i, desk_word(epoch));
        let word = word(hex);

        assert!(fills_at(&word, number, false));
        assert!(listing.under_head_epoch(&word));
        assert!(!fills_at(&word, number - 1, true));
        assert!(!fills_at(&word, number + 2, true));
    }

    /// The encoder the fakes write quote words with agrees with the builder's reference encoder on
    /// every field a reader reads: stamp, epoch, carry and the firm bit.
    #[test]
    fn the_fakes_quote_words_agree_with_the_vectors_on_the_bits_a_reader_reads() {
        let mask = ((U256::from(1u8) << 63usize) - U256::from(1u8)) |
            (U256::from(1u8) << 201usize) |
            (U256::from(1u8) << 205usize);
        for (_, epoch, hex, number) in VECTOR_WORDS {
            let vector = U256::from_be_bytes(word(hex).0);
            let carry = vector.bit(201);
            let firm = vector.bit(205);

            let built = U256::from_be_bytes(quote_word(number, epoch, carry, firm).0);

            assert_eq!(built & mask, vector & mask, "epoch {epoch}");
        }
    }

    /// One block after its stamp a word fills only where the listing carries and the word does:
    /// slot 7's word carries, slot 0's does not.
    #[test]
    fn a_word_fills_the_block_after_only_where_listing_and_word_both_carry() {
        let carrying = word(VECTOR_WORDS[1].2);
        let not_carrying = word(VECTOR_WORDS[0].2);

        assert!(fills_at(&carrying, 26_460_202, true));
        assert!(!fills_at(&carrying, 26_460_202, false));
        assert!(!fills_at(&not_carrying, 26_460_201, true));
    }

    /// The 31 bits all count: the epoch 16,847,216 (0x1011170) and 4,464 (0x1170) agree in their
    /// low 16 bits, and a word of one is empty under the other.
    #[test]
    fn epochs_agreeing_in_the_low_16_bits_are_not_the_same_epoch() {
        let word_of_big = word(VECTOR_WORDS[2].2);

        assert_eq!(16_847_216u32 & 0xffff, 4_464 & 0xffff);
        assert!(listing(255, desk_word(16_847_216)).under_head_epoch(&word_of_big));
        assert!(!listing(255, desk_word(4_464)).under_head_epoch(&word_of_big));
        assert!(!listing(255, desk_word(0)).under_head_epoch(&word_of_big));
    }

    /// A quote word's bits 63 and above its epoch never stand in for the epoch: bit 62 is the top
    /// of it, bit 63 is not read.
    #[test]
    fn the_words_epoch_is_bits_32_to_62() {
        let listing = listing(0, desk_word(1));
        let one = B256::from(U256::from(1u8) << 32usize);

        assert!(listing.under_head_epoch(&one));
        assert!(listing.under_head_epoch(&B256::from(
            (U256::from(1u8) << 32usize) | (U256::from(1u8) << 63usize)
        )));
        assert!(!listing.under_head_epoch(&B256::from(U256::from(1u8) << 62usize)));
    }

    /// The firm word counts only where the quote word's bit 205 is set: slot 7's word has it.
    #[test]
    fn the_firm_word_is_claimed_only_under_bit_205() {
        let seven = listing(7, desk_word(3));

        let with_firm = seven.claims(&word(VECTOR_WORDS[1].2));
        let without = seven.claims(&word(VECTOR_WORDS[0].2));

        assert_eq!(with_firm, BTreeSet::from([seven.quote_slot, seven.firm_slot]));
        assert_eq!(without, BTreeSet::from([seven.quote_slot]));
    }

    /// Only the pairs' words are not the front's: slots 0 to 3 and every other slot are foreign.
    #[rstest]
    #[case::slot_0(0, true)]
    #[case::the_desk_word(1, true)]
    #[case::the_registry(2, true)]
    #[case::the_word_key(3, true)]
    #[case::below_the_quote_words(0xff, true)]
    #[case::the_first_quote_word(0x100, false)]
    #[case::the_last_quote_word(0x1ff, false)]
    #[case::the_first_firm_word(0x200, false)]
    #[case::the_last_firm_word(0x2ff, false)]
    #[case::past_the_firm_words(0x300, true)]
    fn only_the_pair_words_of_a_desk_contract_are_not_the_fronts(
        #[case] slot: u64,
        #[case] foreign: bool,
    ) {
        assert_eq!(!is_pair_slot(&B256::from(U256::from(slot))), foreign);
    }
}
