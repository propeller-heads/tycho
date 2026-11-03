//! The simulation overrides attached to a pAMM state, and the buffer that holds them until the
//! price levels they belong to arrive.
//!
//! Titan publishes a venue's price levels and the account state it priced them on over two
//! sockets. A book names the quote sequence it was priced at (`calculated_from`). A venue entry of
//! the quote stream names, for each pair, the quote sequence of the state it carries
//! (`pairSequences`). Titan does not publish an entry for every quote sequence, so a book often
//! names a quote sequence that no entry carries. A book arrives about a second after the entries
//! Titan published before it, so the buffer already holds those entries when the book arrives, as
//! long as the consumer polls the stream again as soon as it has handled each update. The stream
//! reads both sockets only while polled, and then in no fixed order, so after a pause it can read
//! a book before entries that reached their socket earlier, and that book's state gets an earlier
//! entry or none. `OverrideBuffer::find` therefore returns the newest entry of the book's venue and
//! pair that comes from the book's slot and has a quote sequence at or below the book's: the latest
//! state Titan published before it priced the book.
//!
//! A venue can read the state of more than one quote address. An entry's `pricedAddresses` lists
//! every venue whose swaps read that entry's state, so the buffer stores the entry under each
//! listed venue the stream serves, and `find` merges the matching entries of the other quote
//! addresses into the venue's own entry. The venue's own entry wins wherever two of them set the
//! same value. Another quote address's entry matches by the same rule as the venue's own, so it
//! merges only when its quote sequence is at or below the book's. `find` compares the numbers
//! only, and cannot tell whether that quote address counts on the venue's counter: in the
//! 2026-10-21 recording, `0x6dca2500…` named Metric's router but numbered its quote sequences by
//! the millisecond, so none of its entries ever sat at or below a Metric book's quote sequence.
//! That cost nothing: on 2026-10-22, none of 16 traced Metric WETH/USDT swaps read the one storage
//! slot its entries set. `find` reports the quote addresses it skips this way.
//!
//! The buffer reads no clock. A quote sequence carries its beacon slot in its high 64 bits, and a
//! book only matches an entry of its own slot, so the buffer keeps only the slots a book can still
//! name: the slot of the latest price level message, the one before it, and the one after it.
//! Before the first price level message, the newest slot the entries name stands in for it.
//!
//! A match rests on what Titan publishes, not on anything this code can check: the messages of the
//! two sockets arrive separately, and nothing in either proves that one simulation produced both.
//! The live test `pamm_simulation_overrides` checks that claim against the chain.
//!
//! These types are serialized with the state that carries them, so no field is wider than `u64`:
//! a `serde_json::Value` holds no wider integer, and neither does typetag's `Content` enum, which
//! holds a state whose tag arrives after its content. Quote sequences do not fit, so these types
//! never carry one; the buffer uses them for matching only.

use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tycho_common::Bytes;

use super::{config::canonical_pamm_address, titan::VenueOverrides, NANOS_PER_SECOND, SLOT};

/// The simulation overrides of one account: the fields Titan overrode, and nothing else.
///
/// An absent field means the simulation read the chain's value, never that the value is zero, so
/// an account that overrode only storage carries no balance and no nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountOverride {
    /// The account's balance in wei: Titan's hex quantity decoded into minimal big-endian bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<Bytes>,
    /// The account's nonce, `None` when Titan did not override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<u64>,
    /// The overridden storage slots, keyed by slot. A slot the map does not name keeps its chain
    /// value: this is a `stateDiff`, never a full `state` replacement.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state_diff: BTreeMap<Bytes, Bytes>,
}

/// The account state Titan simulated a venue on to price one state's levels.
///
/// The fields map onto an `eth_call` State Override Set and block overrides, but this type is not
/// that JSON-RPC shape: a consumer converts it into its RPC client's override types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PammSimulationOverrides {
    /// The overridden accounts, keyed by address. An account the map does not name is simulated
    /// from chain state.
    pub accounts: BTreeMap<Bytes, AccountOverride>,
    /// The block Titan was building when it took this state, `None` when the venue entry named
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
    /// The Unix second that block is dated at, derived from the entry's beacon slot. `None` when
    /// the entry carried no slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_timestamp: Option<u64>,
    /// Unix nanoseconds at which Titan took this state. It orders two entries of one quote
    /// sequence, whose arrival order can differ.
    pub built_at_ns: u64,
}

impl PammSimulationOverrides {
    /// The Unix second one slot after [`Self::built_at_ns`], floored.
    ///
    /// Information only. Nothing in this crate acts on it: a state quotes for as long as its price
    /// levels are valid, whatever this says.
    pub fn expires_at(&self) -> u64 {
        // A slot is a whole number of seconds, so flooring first gives the same second.
        self.built_at_ns / NANOS_PER_SECOND + SLOT.as_secs()
    }

    /// How much of the buffer's budget one stored copy of these overrides takes: its accounts plus
    /// its storage words.
    fn size(&self) -> usize {
        self.accounts.len() +
            self.accounts
                .values()
                .map(|account| account.state_diff.len())
                .sum::<usize>()
    }

    /// `self` with the accounts of `others` added, `self`'s own values winning wherever both set
    /// one: a balance, a nonce or a storage slot. The block and timing fields stay `self`'s.
    fn merged<'a>(&'a self, others: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut accounts: BTreeMap<Bytes, AccountOverride> = BTreeMap::new();
        for source in others.into_iter().chain([self]) {
            for (address, account) in &source.accounts {
                match accounts.entry(address.clone()) {
                    Entry::Vacant(vacant) => {
                        vacant.insert(account.clone());
                    }
                    Entry::Occupied(mut occupied) => {
                        let held = occupied.get_mut();
                        if account.balance.is_some() {
                            held.balance = account.balance.clone();
                        }
                        if account.nonce.is_some() {
                            held.nonce = account.nonce;
                        }
                        held.state_diff
                            .extend(account.state_diff.clone());
                    }
                }
            }
        }
        Self {
            accounts,
            block_number: self.block_number,
            block_timestamp: self.block_timestamp,
            built_at_ns: self.built_at_ns,
        }
    }
}

/// The beacon slot a quote sequence was priced in: its high 64 bits.
pub(super) fn slot_of(sequence: u128) -> u64 {
    // A `u128` shifted right by 64 bits always fits a `u64`.
    (sequence >> 64) as u64
}

/// The most accounts and storage words the buffer holds across every venue and pair, counting an
/// entry once for each venue and pair it is stored under.
///
/// A memory ceiling against a misbehaving upstream, not a rule about Titan: it keeps the buffer
/// to a few hundred megabytes whatever Titan sends. Replaying a 13 minute recording (2026-10-21)
/// through these rules, with every streamed venue served, peaked at 148,444, held in 11,655 copies
/// of entries.
const MAX_HELD_SIZE: usize = 1_000_000;

/// What [`OverrideBuffer::find`] returns for a book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Found {
    /// The overrides to attach: the venue's own entry, with the other quote addresses' entries
    /// merged in.
    pub overrides: Arc<PammSimulationOverrides>,
    /// Whether the venue's own entry carried the book's quote sequence, rather than an earlier one
    /// of the same slot.
    pub same_sequence: bool,
    /// How many other quote addresses' entries were merged into the venue's own.
    pub merged_sources: usize,
    /// How many other quote addresses held entries for the pair in the book's slot, none of them
    /// at or below the book's quote sequence, so nothing of theirs was merged.
    pub skipped_sources: usize,
}

/// What storing entries, or moving the slot window, did to the buffer.
///
/// Each variant's name in snake case is the `event` label it is counted under.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum_macros::IntoStaticStr,
    strum_macros::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub(super) enum BufferEvent {
    /// The buffer now holds an entry, under one venue and pair, that it did not hold for that
    /// quote address and quote sequence.
    Added,
    /// A newer entry for the same quote address, venue, pair and quote sequence replaced the one
    /// held.
    ReplacedByNewer,
    /// An entry older than the one held for the same quote address, venue, pair and quote
    /// sequence arrived; the held one stays.
    OlderThanHeld,
    /// The same entry arrived again, identical. Nothing changed.
    ResentUnchanged,
    /// The same entry (same quote address and timestamp) arrived again with different state. The
    /// buffer keeps the copy it already holds.
    ///
    /// In a 13 minute recording (2026-10-21) Titan did this 268 times, and every resent copy
    /// carried a subset of the first copy's accounts and slots with the same values, so the first
    /// copy is the more complete one.
    ResentWithDifferentState,
    /// A pair of one token with itself, which quotes nothing. Counted once per pair.
    SameTokenPair,
    /// The pair's quote sequence names a slot more than one away from the window's slot, so the
    /// buffer did not store it. Counted once per pair. It moves while the price level socket is
    /// down, because only a price level message moves the window.
    ArrivedOutsideSlotWindow,
    /// A held entry was dropped because the window moved past its slot.
    LeftSlotWindow,
    /// Storing the entry, or a newer copy of one held, would take the buffer past
    /// [`MAX_HELD_SIZE`], so it was not stored.
    BufferFull,
}

/// How many times each [`BufferEvent`] happened.
pub(super) type BufferEvents = BTreeMap<BufferEvent, u64>;

/// Where the buffer holds an entry: a venue and a pair, the pair's tokens ordered so that the key
/// does not depend on the order Titan names them in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PairKey {
    venue: Bytes,
    low: Bytes,
    high: Bytes,
}

impl PairKey {
    fn new(venue: &Bytes, token_a: &Bytes, token_b: &Bytes) -> Self {
        let (low, high) = if token_a <= token_b { (token_a, token_b) } else { (token_b, token_a) };
        Self { venue: venue.clone(), low: low.clone(), high: high.clone() }
    }
}

/// One entry, as the buffer holds it under one venue and pair.
#[derive(Debug, Clone)]
struct Held {
    /// The quote address Titan streamed the entry under, after alias normalization: the venue
    /// itself, or another quote address whose `pricedAddresses` list the venue.
    source: Bytes,
    sequence: u128,
    overrides: Arc<PammSimulationOverrides>,
    /// [`PammSimulationOverrides::size`] of `overrides`, counted once per copy.
    size: usize,
}

/// The venue entries of the quote stream, held until the books that name them arrive.
pub(super) struct OverrideBuffer {
    held: BTreeMap<PairKey, Vec<Held>>,
    /// The sum of [`Held::size`] across every key.
    size: usize,
    max_size: usize,
    /// The slot of the latest price level message.
    message_slot: Option<u64>,
    /// The newest slot the entries named, which places the window until the first price level
    /// message.
    newest_entry_slot: Option<u64>,
}

impl Default for OverrideBuffer {
    fn default() -> Self {
        Self::with_capacity(MAX_HELD_SIZE)
    }
}

impl OverrideBuffer {
    fn with_capacity(max_size: usize) -> Self {
        Self {
            held: BTreeMap::new(),
            size: 0,
            max_size,
            message_slot: None,
            newest_entry_slot: None,
        }
    }

    /// Stores `entry` under every venue whose swaps read its state and that `is_served` accepts:
    /// the quote address it was streamed under and every venue its `pricedAddresses` name, each
    /// normalized through the alias table as a book's venue is, so that the two sockets agree on
    /// what a venue is called.
    pub(super) fn store(
        &mut self,
        entry: VenueOverrides,
        is_served: impl Fn(&Bytes) -> bool,
        events: &mut BufferEvents,
    ) {
        let source = canonical_pamm_address(&entry.quote_address);
        let mut venues: Vec<Bytes> = Vec::new();
        for address in [&entry.quote_address]
            .into_iter()
            .chain(&entry.priced_addresses)
        {
            let venue = canonical_pamm_address(address);
            if is_served(&venue) && !venues.contains(&venue) {
                venues.push(venue);
            }
        }
        if venues.is_empty() {
            return;
        }
        if self.message_slot.is_none() {
            self.follow_newest_entry_slot(&entry, events);
        }
        let size = entry.overrides.size();
        let overrides = Arc::new(entry.overrides);
        for pair in &entry.pairs {
            if pair.token_1 == pair.token_2 {
                *events
                    .entry(BufferEvent::SameTokenPair)
                    .or_default() += 1;
                continue;
            }
            if !self.in_window(pair.sequence) {
                *events
                    .entry(BufferEvent::ArrivedOutsideSlotWindow)
                    .or_default() += 1;
                continue;
            }
            for venue in &venues {
                let held = Held {
                    source: source.clone(),
                    sequence: pair.sequence,
                    overrides: overrides.clone(),
                    size,
                };
                let event = self.hold(PairKey::new(venue, &pair.token_1, &pair.token_2), held);
                *events.entry(event).or_default() += 1;
            }
        }
    }

    /// Moves the slot window to `slot`, the slot of the latest price level message, and drops
    /// every entry outside it.
    pub(super) fn advance(&mut self, slot: u64, events: &mut BufferEvents) {
        self.message_slot = Some(slot);
        self.retain_window(slot, events);
    }

    /// The overrides to attach to a book of `venue` pricing `token_a`/`token_b` at `sequence`, or
    /// `None` when the venue's own entries carry nothing for that pair in the book's slot at or
    /// below `sequence`.
    ///
    /// From each quote address's entries it takes the one with the highest quote sequence at or
    /// below the book's. The buffer holds one entry per quote address and quote sequence, so
    /// there is never a tie.
    pub(super) fn find(
        &self,
        venue: &Bytes,
        token_a: &Bytes,
        token_b: &Bytes,
        sequence: u128,
    ) -> Option<Found> {
        let held = self
            .held
            .get(&PairKey::new(venue, token_a, token_b))?;
        let mut newest: BTreeMap<&Bytes, &Held> = BTreeMap::new();
        let mut in_slot: BTreeSet<&Bytes> = BTreeSet::new();
        for entry in held {
            if slot_of(entry.sequence) != slot_of(sequence) {
                continue;
            }
            in_slot.insert(&entry.source);
            if entry.sequence > sequence {
                continue;
            }
            newest
                .entry(&entry.source)
                .and_modify(|best| {
                    if entry.sequence > best.sequence {
                        *best = entry;
                    }
                })
                .or_insert(entry);
        }
        let own = newest.remove(venue)?;
        let skipped_sources = in_slot
            .iter()
            .filter(|source| **source != venue && !newest.contains_key(*source))
            .count();
        let overrides = if newest.is_empty() {
            own.overrides.clone()
        } else {
            let others = newest
                .values()
                .map(|other| other.overrides.as_ref());
            Arc::new(own.overrides.merged(others))
        };
        Some(Found {
            overrides,
            same_sequence: own.sequence == sequence,
            merged_sources: newest.len(),
            skipped_sources,
        })
    }

    /// Before the first price level message, moves the window to the newest slot `entry` names
    /// when it is newer than any seen, so the buffer still holds only three slots.
    fn follow_newest_entry_slot(&mut self, entry: &VenueOverrides, events: &mut BufferEvents) {
        let Some(newest) = entry
            .pairs
            .iter()
            .map(|pair| slot_of(pair.sequence))
            .max()
        else {
            return;
        };
        if self
            .newest_entry_slot
            .is_some_and(|held| held >= newest)
        {
            return;
        }
        self.newest_entry_slot = Some(newest);
        self.retain_window(newest, events);
    }

    /// Drops every entry whose slot is outside the window around `slot`.
    fn retain_window(&mut self, slot: u64, events: &mut BufferEvents) {
        let mut dropped = 0;
        self.held.retain(|_, held| {
            let before = held.len();
            held.retain(|entry| in_window(slot, entry.sequence));
            dropped += (before - held.len()) as u64;
            !held.is_empty()
        });
        self.size = self
            .held
            .values()
            .flatten()
            .map(|entry| entry.size)
            .sum();
        if dropped > 0 {
            *events
                .entry(BufferEvent::LeftSlotWindow)
                .or_default() += dropped;
        }
    }

    /// Whether a quote sequence's slot is one the buffer holds.
    fn in_window(&self, sequence: u128) -> bool {
        self.message_slot
            .or(self.newest_entry_slot)
            .is_none_or(|slot| in_window(slot, sequence))
    }

    /// Holds `arriving` under `key`. It replaces a held entry of the same quote address and quote
    /// sequence when `arriving` was built later, keeps the held one otherwise, and refuses
    /// `arriving` when holding it would take the buffer past its budget, replacing or not.
    fn hold(&mut self, key: PairKey, arriving: Held) -> BufferEvent {
        if let Some(held) = self.held.get_mut(&key) {
            let same = held.iter_mut().find(|entry| {
                entry.source == arriving.source && entry.sequence == arriving.sequence
            });
            if let Some(entry) = same {
                return match arriving
                    .overrides
                    .built_at_ns
                    .cmp(&entry.overrides.built_at_ns)
                {
                    Ordering::Greater => {
                        let size = self.size - entry.size + arriving.size;
                        if size > self.max_size {
                            return BufferEvent::BufferFull;
                        }
                        self.size = size;
                        *entry = arriving;
                        BufferEvent::ReplacedByNewer
                    }
                    Ordering::Less => BufferEvent::OlderThanHeld,
                    Ordering::Equal if entry.overrides == arriving.overrides => {
                        BufferEvent::ResentUnchanged
                    }
                    Ordering::Equal => BufferEvent::ResentWithDifferentState,
                };
            }
        }
        if self.size + arriving.size > self.max_size {
            return BufferEvent::BufferFull;
        }
        self.size += arriving.size;
        self.held
            .entry(key)
            .or_default()
            .push(arriving);
        BufferEvent::Added
    }
}

/// Whether a quote sequence's slot is within one of `slot`.
fn in_window(slot: u64, sequence: u128) -> bool {
    let sequence_slot = slot_of(sequence);
    sequence_slot.saturating_add(1) >= slot && sequence_slot <= slot.saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            test_support::{
                address, captured_quote_stream_message, captured_venue_overrides, test_overrides,
                CAPTURED_SLOT, SYNTHETIC_ACCOUNT, USDC, USDT, WBTC, WETH,
            },
            titan::PairSequence,
        },
        *,
    };

    /// The captured Goliath venue.
    const GOLIATH: &str = "0x4367985a63063fc970b009c6e599e9225181f551";
    /// The quote sequence the captured Goliath entry priced its one pair at.
    const GOLIATH_SEQUENCE: u128 = 284_836_027_668_196_596_826_243_132;

    /// Metric's public identity.
    const METRIC: &str = "0x99ab7c1c5eab47801f7937075998527a550fba8a";
    /// The second quote address whose `pricedAddresses` list Metric's router.
    const METRIC_SECOND_SOURCE: &str = "0x30a88bf894d1b806125a23ebf23d715496fd7102";
    /// The quote sequence the captured Metric entry priced all eight of its pairs at.
    const METRIC_SEQUENCE: u128 = 284_836_027_668_196_596_826_243_146;

    /// A synthetic venue.
    const VENUE: &str = "0x3ce2672aa806138585920421406bf0decb8130cb";
    /// A synthetic second quote address.
    const OTHER_SOURCE: &str = "0x00000000000000000000000000000000000000aa";
    /// The Unix nanosecond the synthetic entries are taken at.
    const BUILT_AT: u64 = 1_792_115_920_000_000_000;

    /// The quote sequence numbered `n` in `slot`: the slot in the high 64 bits, as Titan's are.
    fn sequence(slot: u64, n: u64) -> u128 {
        (u128::from(slot) << 64) | u128::from(n)
    }

    /// Every accepted entry of the captured message, stored as a tracker that serves every venue
    /// stores it.
    fn captured_buffer() -> OverrideBuffer {
        let mut buffer = OverrideBuffer::default();
        for entry in captured_venue_overrides(captured_quote_stream_message()) {
            buffer.store(entry, |_| true, &mut BufferEvents::new());
        }
        buffer
    }

    /// A synthetic entry streamed under `source`: it priced each of `pairs` at the given quote
    /// sequence, at `built_at_ns`, overriding the test account's storage slot 1 to `word` and its
    /// nonce to `nonce`.
    fn entry(
        source: &str,
        built_at_ns: u64,
        nonce: u64,
        word: u8,
        pairs: &[(&str, &str, u128)],
    ) -> VenueOverrides {
        VenueOverrides {
            quote_address: address(source),
            priced_addresses: Vec::new(),
            pairs: pairs
                .iter()
                .map(|(token_1, token_2, sequence)| PairSequence {
                    token_1: address(token_1),
                    token_2: address(token_2),
                    sequence: *sequence,
                })
                .collect(),
            slot: Some(CAPTURED_SLOT),
            overrides: PammSimulationOverrides {
                accounts: BTreeMap::from([(
                    address(SYNTHETIC_ACCOUNT),
                    AccountOverride {
                        balance: None,
                        nonce: Some(nonce),
                        state_diff: BTreeMap::from([(word_bytes(1), word_bytes(word))]),
                    },
                )]),
                block_number: Some(26_201_954),
                block_timestamp: Some(1_792_115_927),
                built_at_ns,
            },
        }
    }

    /// A 32-byte word whose last byte is `byte`.
    fn word_bytes(byte: u8) -> Bytes {
        let mut word = [0u8; 32];
        word[31] = byte;
        Bytes::from(word)
    }

    /// Stores `entry`, serving every venue, and returns what it did.
    fn store(buffer: &mut OverrideBuffer, entry: VenueOverrides) -> BufferEvents {
        let mut events = BufferEvents::new();
        buffer.store(entry, |_| true, &mut events);
        events
    }

    /// The events `list` names, as the buffer counts them.
    fn events(list: &[(BufferEvent, u64)]) -> BufferEvents {
        list.iter().copied().collect()
    }

    fn find(buffer: &OverrideBuffer, sequence: u128) -> Option<Found> {
        buffer.find(&address(VENUE), &address(WBTC), &address(USDC), sequence)
    }

    /// The nonce the found overrides set, which is what tells two synthetic entries apart.
    fn nonce_of(found: &Found) -> u64 {
        found.overrides.accounts[&address(SYNTHETIC_ACCOUNT)]
            .nonce
            .expect("a nonce")
    }

    /// The captured Goliath entry is found by the book that names its quote sequence, with the
    /// values transcribed by hand from the capture.
    #[test]
    fn a_captured_entry_is_found_by_venue_pair_and_quote_sequence() {
        let buffer = captured_buffer();

        let found = buffer
            .find(&address(GOLIATH), &address(WETH), &address(USDC), GOLIATH_SEQUENCE)
            .expect("the Goliath entry");

        assert!(found.same_sequence);
        assert_eq!(found.merged_sources, 0);
        assert_eq!(*found.overrides, test_overrides());
    }

    /// The pair's token order does not matter to `find`.
    #[test]
    fn find_ignores_the_order_of_the_pairs_tokens() {
        let mut buffer = OverrideBuffer::default();
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));

        let reversed = buffer.find(
            &address(VENUE),
            &address(USDC),
            &address(WBTC),
            sequence(CAPTURED_SLOT, 1),
        );

        assert!(reversed.is_some());
        assert_eq!(reversed, find(&buffer, sequence(CAPTURED_SLOT, 1)));
    }

    /// Most books name a quote sequence no entry carries, so `find` returns the newest entry of
    /// the same slot at or below the book's quote sequence, never a later one and never one of
    /// another slot.
    #[rstest::rstest]
    #[case::exact(8, Some((8, true)))]
    #[case::between_two_entries(10, Some((8, false)))]
    #[case::below_every_entry_of_the_slot(4, None)]
    fn the_newest_entry_at_or_below_the_quote_sequence_is_found(
        #[case] book: u64,
        #[case] expected: Option<(u64, bool)>,
    ) {
        let mut buffer = OverrideBuffer::default();
        for n in [5, 8, 12] {
            let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, n))];
            store(&mut buffer, entry(VENUE, BUILT_AT + n, n, 1, &pairs));
        }
        let previous_slot = [(WBTC, USDC, sequence(CAPTURED_SLOT - 1, 20))];
        store(&mut buffer, entry(VENUE, BUILT_AT, 99, 1, &previous_slot));

        let found = find(&buffer, sequence(CAPTURED_SLOT, book));

        assert_eq!(found.map(|found| (nonce_of(&found), found.same_sequence)), expected);
    }

    /// Of two entries carrying one quote sequence, the later-built one is held, whichever arrived
    /// first.
    #[test]
    fn of_two_entries_of_one_quote_sequence_the_later_built_is_held() {
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 7))];
        let mut buffer = OverrideBuffer::default();

        let first = store(&mut buffer, entry(VENUE, BUILT_AT + 1, 2, 1, &pairs));
        let older = store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));
        let newer = store(&mut buffer, entry(VENUE, BUILT_AT + 2, 3, 1, &pairs));

        assert_eq!(first, events(&[(BufferEvent::Added, 1)]));
        assert_eq!(older, events(&[(BufferEvent::OlderThanHeld, 1)]));
        assert_eq!(newer, events(&[(BufferEvent::ReplacedByNewer, 1)]));
        assert_eq!(nonce_of(&find(&buffer, sequence(CAPTURED_SLOT, 7)).expect("found")), 3);
    }

    /// The same entry sent twice changes nothing; sent again with different state, the copy
    /// already held is kept.
    #[test]
    fn a_resent_entry_keeps_the_copy_already_held() {
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 7))];
        let mut buffer = OverrideBuffer::default();
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));

        let unchanged = store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));
        let different = store(&mut buffer, entry(VENUE, BUILT_AT, 2, 1, &pairs));

        assert_eq!(unchanged, events(&[(BufferEvent::ResentUnchanged, 1)]));
        assert_eq!(different, events(&[(BufferEvent::ResentWithDifferentState, 1)]));
        assert_eq!(nonce_of(&find(&buffer, sequence(CAPTURED_SLOT, 7)).expect("found")), 1);
    }

    /// Another quote address's entry is merged into the venue's own. The venue's own entry wins
    /// where both set a value and keeps its block and timing fields; the other source fills in
    /// what the venue's own leaves unset; and the other source's entry above the book's quote
    /// sequence is left out.
    #[test]
    fn other_sources_merge_into_the_venues_own_entry() {
        let book = sequence(CAPTURED_SLOT, 7);
        let mut own = entry(VENUE, BUILT_AT, 1, 1, &[(WBTC, USDC, book)]);
        own.overrides
            .accounts
            .get_mut(&address(SYNTHETIC_ACCOUNT))
            .expect("the account")
            .nonce = None;
        let mut other = entry(OTHER_SOURCE, BUILT_AT + 5, 50, 50, &[(WBTC, USDC, book)]);
        other.priced_addresses = vec![address(VENUE)];
        other.overrides.block_number = Some(1);
        let account = other
            .overrides
            .accounts
            .get_mut(&address(SYNTHETIC_ACCOUNT))
            .expect("the account");
        account.balance = Some(Bytes::from(vec![7u8]));
        account
            .state_diff
            .insert(word_bytes(2), word_bytes(2));
        let mut later = entry(OTHER_SOURCE, BUILT_AT, 60, 60, &[(WBTC, USDC, book + 1)]);
        later.priced_addresses = vec![address(VENUE)];
        let mut buffer = OverrideBuffer::default();
        for entry in [own, other, later] {
            store(&mut buffer, entry);
        }

        let found = find(&buffer, book).expect("found");

        assert_eq!(found.merged_sources, 1);
        assert_eq!(found.overrides.block_number, Some(26_201_954), "the other block won");
        assert_eq!(found.overrides.built_at_ns, BUILT_AT, "the other timestamp won");
        let account = &found.overrides.accounts[&address(SYNTHETIC_ACCOUNT)];
        assert_eq!(account.nonce, Some(50), "the unset nonce was not filled in");
        assert_eq!(account.balance, Some(Bytes::from(vec![7u8])), "no balance was filled in");
        assert_eq!(account.state_diff[&word_bytes(1)], word_bytes(1), "the other word won");
        assert_eq!(account.state_diff[&word_bytes(2)], word_bytes(2), "the other slot is lost");
    }

    /// Another quote address's entry alone attaches nothing: the venue's own entry is required.
    #[test]
    fn another_sources_entry_alone_is_not_found() {
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 7))];
        let mut other = entry(OTHER_SOURCE, BUILT_AT, 50, 50, &pairs);
        other.priced_addresses = vec![address(VENUE)];
        let mut buffer = OverrideBuffer::default();
        store(&mut buffer, other);

        let found = find(&buffer, sequence(CAPTURED_SLOT, 7));

        assert_eq!(found, None);
    }

    /// Another quote address whose entries in the book's slot all carry a later quote sequence than
    /// the book's merges nothing, and is reported as skipped.
    #[test]
    fn another_source_with_only_later_entries_is_skipped() {
        let book = sequence(CAPTURED_SLOT, 7);
        let mut later = entry(OTHER_SOURCE, BUILT_AT, 50, 50, &[(WBTC, USDC, book + 1)]);
        later.priced_addresses = vec![address(VENUE)];
        let mut buffer = OverrideBuffer::default();
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &[(WBTC, USDC, book)]));
        store(&mut buffer, later);

        let found = find(&buffer, book).expect("found");

        assert_eq!(found.merged_sources, 0);
        assert_eq!(found.skipped_sources, 1);
        assert_eq!(nonce_of(&found), 1);
    }

    /// An entry is stored only under the venues `is_served` accepts.
    #[test]
    fn an_entry_is_stored_only_under_served_venues() {
        let mut buffer = OverrideBuffer::default();
        let mut events = BufferEvents::new();
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];

        buffer.store(entry(VENUE, BUILT_AT, 1, 1, &pairs), |_| false, &mut events);

        assert!(events.is_empty());
        assert_eq!(find(&buffer, sequence(CAPTURED_SLOT, 1)), None);
    }

    /// An entry no served venue reads leaves the buffer as it was: before the first price level
    /// message, its slot does not move the window, so it cannot drop a served venue's entries.
    #[test]
    fn an_unserved_entry_does_not_move_the_window() {
        let mut buffer = OverrideBuffer::default();
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));
        let mut events = BufferEvents::new();
        let far_ahead = [(WBTC, USDC, sequence(CAPTURED_SLOT + 5, 1))];

        buffer.store(entry(OTHER_SOURCE, BUILT_AT, 2, 2, &far_ahead), |_| false, &mut events);

        assert!(events.is_empty(), "{events:?}");
        assert!(find(&buffer, sequence(CAPTURED_SLOT, 1)).is_some());
    }

    /// The captured Metric book's pairs find Metric's main entry with its second quote address's
    /// entry merged in, for the pairs that second entry carries.
    #[test]
    fn the_captured_metric_entries_merge() {
        let buffer = captured_buffer();
        let metric = address(METRIC);

        let merged = buffer.find(&metric, &address(WETH), &address(USDC), METRIC_SEQUENCE);
        let alone = buffer.find(&metric, &address(USDC), &address(USDT), METRIC_SEQUENCE);

        let merged = merged.expect("Metric's WETH/USDC");
        assert_eq!(merged.merged_sources, 1);
        assert!(merged
            .overrides
            .accounts
            .contains_key(&address(METRIC_SECOND_SOURCE)));
        // The second source carries no USDC/USDT pair, so that pair is Metric's entry alone.
        assert_eq!(
            alone
                .expect("Metric's USDC/USDT")
                .merged_sources,
            0
        );
    }

    /// The window holds the latest message's slot and one slot either side: moving it drops the
    /// slots it leaves.
    #[test]
    fn moving_the_window_drops_the_slots_it_leaves() {
        let mut buffer = OverrideBuffer::default();
        buffer.advance(CAPTURED_SLOT - 1, &mut BufferEvents::new());
        for slot in [CAPTURED_SLOT - 2, CAPTURED_SLOT - 1, CAPTURED_SLOT] {
            store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &[(WBTC, USDC, sequence(slot, 1))]));
        }
        let mut moved = BufferEvents::new();

        buffer.advance(CAPTURED_SLOT, &mut moved);

        assert_eq!(moved, events(&[(BufferEvent::LeftSlotWindow, 1)]));
        assert_eq!(find(&buffer, sequence(CAPTURED_SLOT - 2, 1)), None);
        for slot in [CAPTURED_SLOT - 1, CAPTURED_SLOT] {
            assert!(find(&buffer, sequence(slot, 1)).is_some(), "slot {slot} was dropped");
        }
    }

    /// An entry for a slot more than one away from the window's is not stored.
    #[rstest::rstest]
    #[case::two_slots_back(CAPTURED_SLOT - 2)]
    #[case::two_slots_ahead(CAPTURED_SLOT + 2)]
    fn an_entry_outside_the_window_is_not_stored(#[case] slot: u64) {
        let mut buffer = OverrideBuffer::default();
        buffer.advance(CAPTURED_SLOT, &mut BufferEvents::new());

        let pairs = [(WBTC, USDC, sequence(slot, 1))];
        let stored = store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));

        assert_eq!(stored, events(&[(BufferEvent::ArrivedOutsideSlotWindow, 1)]));
        assert_eq!(find(&buffer, sequence(slot, 1)), None);
    }

    /// Before the first price level message the newest slot the entries name places the window,
    /// so the buffer still holds three slots however long the price level socket stays down.
    #[test]
    fn before_any_price_level_message_the_newest_entry_slot_places_the_window() {
        let mut buffer = OverrideBuffer::default();
        for slot in [CAPTURED_SLOT - 3, CAPTURED_SLOT - 2] {
            store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &[(WBTC, USDC, sequence(slot, 1))]));
        }

        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];
        let newest = store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));

        assert_eq!(newest, events(&[(BufferEvent::Added, 1), (BufferEvent::LeftSlotWindow, 2)]));
        assert!(find(&buffer, sequence(CAPTURED_SLOT, 1)).is_some());
    }

    /// Before the first price level message, an entry of an older slot arriving after a newer one
    /// does not pull the window back: it is not stored, and the newer entry stays.
    #[test]
    fn before_any_price_level_message_an_older_entry_does_not_move_the_window_back() {
        let mut buffer = OverrideBuffer::default();
        let newest = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &newest));

        let older = [(WBTC, USDC, sequence(CAPTURED_SLOT - 3, 1))];
        let stored = store(&mut buffer, entry(VENUE, BUILT_AT, 2, 2, &older));

        assert_eq!(stored, events(&[(BufferEvent::ArrivedOutsideSlotWindow, 1)]));
        assert!(find(&buffer, sequence(CAPTURED_SLOT, 1)).is_some());
    }

    /// A full buffer stores nothing new but keeps serving what it holds, and stores again once
    /// the window moves and frees room. Each synthetic entry overrides one account and one
    /// storage word, so it takes 2 of the budget: a budget of 4 holds two.
    #[test]
    fn a_full_buffer_stores_again_once_the_window_frees_room() {
        let mut buffer = OverrideBuffer::with_capacity(4);
        buffer.advance(CAPTURED_SLOT, &mut BufferEvents::new());
        for (slot, nonce) in [(CAPTURED_SLOT - 1, 1), (CAPTURED_SLOT, 2)] {
            let pairs = [(WBTC, USDC, sequence(slot, 1))];
            store(&mut buffer, entry(VENUE, BUILT_AT, nonce, 1, &pairs));
        }
        let next = || {
            let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT + 1, 1))];
            entry(VENUE, BUILT_AT, 3, 1, &pairs)
        };

        let full = store(&mut buffer, next());
        buffer.advance(CAPTURED_SLOT + 1, &mut BufferEvents::new());
        let freed = store(&mut buffer, next());

        assert_eq!(full, events(&[(BufferEvent::BufferFull, 1)]));
        assert_eq!(freed, events(&[(BufferEvent::Added, 1)]));
        assert!(find(&buffer, sequence(CAPTURED_SLOT, 1)).is_some());
    }

    /// A full buffer still replaces an entry it holds with a newer copy: the cap limits how much
    /// the buffer holds, not which copy of an entry.
    #[test]
    fn a_full_buffer_still_replaces_a_held_entry() {
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];
        let mut buffer = OverrideBuffer::with_capacity(2);
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));

        let newer = store(&mut buffer, entry(VENUE, BUILT_AT + 1, 2, 1, &pairs));

        assert_eq!(newer, events(&[(BufferEvent::ReplacedByNewer, 1)]));
        assert_eq!(nonce_of(&find(&buffer, sequence(CAPTURED_SLOT, 1)).expect("found")), 2);
    }

    /// A newer copy that would take the buffer past its budget is refused like any other entry, and
    /// the held copy stays.
    #[test]
    fn a_newer_copy_past_the_budget_is_refused() {
        let pairs = [(WBTC, USDC, sequence(CAPTURED_SLOT, 1))];
        let mut buffer = OverrideBuffer::with_capacity(2);
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));
        let mut larger = entry(VENUE, BUILT_AT + 1, 2, 1, &pairs);
        larger
            .overrides
            .accounts
            .get_mut(&address(SYNTHETIC_ACCOUNT))
            .expect("the account")
            .state_diff
            .insert(word_bytes(2), word_bytes(2));

        let refused = store(&mut buffer, larger);

        assert_eq!(refused, events(&[(BufferEvent::BufferFull, 1)]));
        assert_eq!(nonce_of(&find(&buffer, sequence(CAPTURED_SLOT, 1)).expect("found")), 1);
    }

    /// Another quote address's entries of another slot are neither merged nor counted as skipped.
    #[test]
    fn another_sources_entry_of_another_slot_is_not_skipped() {
        let book = sequence(CAPTURED_SLOT, 7);
        let mut previous_slot =
            entry(OTHER_SOURCE, BUILT_AT, 50, 50, &[(WBTC, USDC, sequence(CAPTURED_SLOT - 1, 7))]);
        previous_slot.priced_addresses = vec![address(VENUE)];
        let mut buffer = OverrideBuffer::default();
        store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &[(WBTC, USDC, book)]));
        store(&mut buffer, previous_slot);

        let found = find(&buffer, book).expect("found");

        assert_eq!((found.merged_sources, found.skipped_sources), (0, 0));
    }

    /// A pair of one token with itself quotes nothing, so it is not stored, and the entry's other
    /// pairs still are.
    #[test]
    fn a_same_token_pair_is_skipped() {
        let mut buffer = OverrideBuffer::default();
        let pairs =
            [(WBTC, WBTC, sequence(CAPTURED_SLOT, 1)), (WBTC, USDC, sequence(CAPTURED_SLOT, 1))];

        let stored = store(&mut buffer, entry(VENUE, BUILT_AT, 1, 1, &pairs));

        assert_eq!(stored, events(&[(BufferEvent::Added, 1), (BufferEvent::SameTokenPair, 1)]));
        assert!(find(&buffer, sequence(CAPTURED_SLOT, 1)).is_some());
    }

    /// `expires_at` is one slot after `built_at_ns`, floored, and does not overflow at
    /// `u64::MAX`.
    #[test]
    fn expires_at_is_one_slot_after_the_timestamp() {
        let at_the_limit = PammSimulationOverrides { built_at_ns: u64::MAX, ..test_overrides() };

        assert_eq!(test_overrides().expires_at(), 1_792_115_932);
        assert_eq!(at_the_limit.expires_at(), u64::MAX / 1_000_000_000 + 12);
    }

    /// An account that overrode only storage writes neither a balance nor a nonce, and one that
    /// overrode only its balance writes no storage map: an absent field means "take the chain's
    /// value", so writing it as a null or an empty map would claim an override Titan never sent.
    #[test]
    fn an_account_override_omits_the_fields_it_does_not_set() {
        let storage_only = AccountOverride {
            balance: None,
            nonce: None,
            state_diff: BTreeMap::from([(word_bytes(1), word_bytes(2))]),
        };
        let balance_only = AccountOverride {
            balance: Some(word_bytes(3)),
            nonce: None,
            state_diff: BTreeMap::new(),
        };

        let storage_json = serde_json::to_value(&storage_only).expect("serializes");
        let balance_json = serde_json::to_value(&balance_only).expect("serializes");

        let storage_object = storage_json
            .as_object()
            .expect("an object");
        assert!(storage_object.get("balance").is_none());
        assert!(storage_object.get("nonce").is_none());
        assert_eq!(storage_object.len(), 1);
        let balance_object = balance_json
            .as_object()
            .expect("an object");
        assert!(balance_object.get("balance").is_some());
        assert_eq!(balance_object.len(), 1);
    }

    /// The counterpart: an account entry naming no field at all reads back as an override of
    /// nothing, rather than as an account zeroed out.
    #[test]
    fn an_account_override_reads_an_absent_field_as_unset() {
        let read: AccountOverride = serde_json::from_str("{}").expect("reads back");

        assert_eq!(
            read,
            AccountOverride { balance: None, nonce: None, state_diff: BTreeMap::new() }
        );
    }

    /// Accounts and storage slots are `0x`-prefixed hex strings in a recording, as they are on
    /// the stream, not byte sequences: a consumer reading a recording finds the addresses it
    /// knows.
    #[test]
    fn accounts_and_slots_are_hex_string_keys() {
        let overrides = test_overrides();
        let (account, slot) = overrides
            .accounts
            .iter()
            .find_map(|(address, account)| {
                account
                    .state_diff
                    .keys()
                    .next()
                    .map(|slot| (address.to_string(), slot.to_string()))
            })
            .expect("an account that overrides storage");

        let json = serde_json::to_value(&overrides).expect("serializes");

        assert_eq!(json["accounts"][&account]["nonce"], serde_json::json!(1));
        assert!(json["accounts"][&account]["state_diff"][&slot].is_string());
    }

    /// Real overrides survive both serde_json shapes. Nothing in them is wider than `u64`,
    /// because a `serde_json::Value` cannot hold a wider integer.
    #[test]
    fn overrides_round_trip_through_text_and_value() {
        let overrides = test_overrides();

        let text = serde_json::to_string(&overrides).expect("serializes");
        let value = serde_json::to_value(&overrides).expect("serializes into a Value");

        let from_text: PammSimulationOverrides =
            serde_json::from_str(&text).expect("reads back from text");
        let from_value: PammSimulationOverrides =
            serde_json::from_value(value).expect("reads back from a Value");
        assert_eq!(from_text, overrides);
        assert_eq!(from_value, overrides);
    }

    /// The block fields are optional, and overrides without them round-trip to the same text.
    #[test]
    fn the_block_fields_are_optional() {
        let text = r#"{"accounts":{},"built_at_ns":1791475494775213874}"#;

        let read: PammSimulationOverrides = serde_json::from_str(text).expect("reads back");

        assert_eq!(read.block_number, None);
        assert_eq!(read.block_timestamp, None);
        assert_eq!(serde_json::to_string(&read).expect("serializes"), text);
    }

    /// The accounts and `built_at_ns` are required: overrides whose contents or timing are unknown
    /// are rejected, never defaulted to empty or to the epoch.
    #[rstest::rstest]
    #[case::no_accounts(r#"{"built_at_ns":1791475494775213874}"#)]
    #[case::no_built_at_ns(r#"{"accounts":{}}"#)]
    fn overrides_missing_a_required_field_are_rejected(#[case] text: &str) {
        let error = serde_json::from_str::<PammSimulationOverrides>(text)
            .expect_err("overrides missing a required field are rejected");

        assert!(
            error
                .to_string()
                .contains("missing field"),
            "unexpected error: {error}"
        );
    }
}
