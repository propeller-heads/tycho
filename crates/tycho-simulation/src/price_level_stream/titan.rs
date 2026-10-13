//! Titan pAMM stream formats and connection handling.
//!
//! Titan serves two pAMM WebSockets, both read by [`messages`]:
//!
//! - `pamm_price_levels_v2` (see <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>)
//!   yields the books Titan simulated in one build round, targeting the block it is currently
//!   building. Consumers decide how fresh a component is from the frame `ts`, not from what a frame
//!   omits.
//! - `pamm_quote_stream_v2` yields, per venue, the account state Titan simulated that venue on to
//!   produce those books, together with the pair versions the books were priced at. A book names
//!   its version in `calculated_from`; a venue entry names the same number per pair, which is how a
//!   snapshot is matched to the ladder it belongs to.
//!
//! All Titan specifics (endpoints, JSON shapes, reconnect policy, and what a payload has to look
//! like to be used at all) live in this module; the rest of the price level stream machinery is
//! venue-agnostic.

use std::{collections::BTreeMap, time::Duration};

use async_stream::stream;
use futures::{Stream, StreamExt};
use serde::{
    de::{DeserializeOwned, IgnoredAny},
    Deserialize, Deserializer,
};
use serde_json::value::RawValue;
use tokio::time::{sleep, timeout, timeout_at, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use tycho_common::Bytes;

use super::{
    overrides::{AccountOverride, PammSimulationOverrides},
    telemetry::{self, ReconnectReason, RejectReason},
    NANOS_PER_SECOND, SLOT,
};

/// Default Titan pAMM price level WebSocket endpoint. Titan serves the same stream from other
/// regions as well; see <https://docs.titanbuilder.xyz/propamms/takers>.
pub(super) const TITAN_PRICE_LEVEL_URL: &str =
    "wss://eu.data.titanbuilder.xyz/ws/pamm_price_levels_v2";

/// Environment variable that replaces [`TITAN_PRICE_LEVEL_URL`] when the builder is given no
/// endpoint.
pub(super) const TITAN_PRICE_LEVEL_URL_ENV: &str = "TITAN_PAMM_PRICE_LEVEL_URL";

/// Default Titan pAMM quote stream WebSocket endpoint: the simulated pre-state behind the price
/// level books, from the same region.
pub(super) const TITAN_QUOTE_STREAM_URL: &str =
    "wss://eu.data.titanbuilder.xyz/ws/pamm_quote_stream_v2";

/// Environment variable that replaces [`TITAN_QUOTE_STREAM_URL`] when the builder is given no
/// override endpoint.
pub(super) const TITAN_QUOTE_STREAM_URL_ENV: &str = "TITAN_PAMM_QUOTE_STREAM_URL";

/// The Unix second beacon slot 0 of Ethereum mainnet began at. A slot's second is
/// `BEACON_GENESIS + slot · 12`.
const BEACON_GENESIS: u64 = 1_606_824_023;

/// A 20-byte address, the only width an identity has in either of Titan's messages.
const ADDRESS_BYTES: usize = 20;

/// A 32-byte EVM word, the width of every storage slot and stored value.
const WORD_BYTES: usize = 32;

/// How many accounts one venue entry may override before its snapshot is refused whole.
const MAX_ACCOUNTS: usize = 32;

/// How many storage words one venue entry may override across all its accounts.
const MAX_SLOTS: usize = 512;

/// How many bytes of storage slots and words one venue entry may carry.
const MAX_STORAGE_BYTES: usize = 64 * 1024;

/// Connection tuning for the Titan WebSocket, set through the
/// [`PriceLevelStreamBuilder`](super::stream::PriceLevelStreamBuilder).
#[derive(Clone, Copy, Debug)]
pub(super) struct ConnectionSettings {
    /// Longest a single connection attempt may take before it is aborted and retried, so a hung
    /// TCP/TLS handshake cannot block the stream forever.
    pub connect_timeout: Duration,
    /// Longest gap between *parsed* Titan frames tolerated before the socket is treated as dead
    /// and re-established. Titan pushes one frame per second and sends no keepalives, so a
    /// half-open socket is indistinguishable from silence; the 10 s default reconnects before
    /// the 24 s `stale_after` default removes any component. Pings, binary frames, and
    /// unparsable text do not reset the gap.
    pub read_idle_timeout: Duration,
    /// Cap on the exponential reconnect backoff (`2^attempt` seconds, at most this).
    pub max_backoff: Duration,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_idle_timeout: Duration::from_secs(10),
            max_backoff: Duration::from_secs(32),
        }
    }
}

/// The reconnect backoff after `attempt` consecutive failures: `2^attempt` seconds, capped at
/// `max_backoff`.
fn backoff(attempt: u32, max_backoff: Duration) -> Duration {
    2u64.checked_pow(attempt)
        .map_or(Duration::MAX, Duration::from_secs)
        .min(max_backoff)
}

/// The instant `idle_timeout` from now. The timeout is clamped to what the clock can represent,
/// so an absurd setting disables the watchdog instead of panicking on the addition.
fn deadline_after(idle_timeout: Duration) -> Instant {
    Instant::now() + idle_timeout.min(Duration::from_secs(u32::MAX.into()))
}

/// A frame of one of Titan's two pAMM sockets.
///
/// The socket label is a property of the frame type rather than of the caller, so the one reader
/// both sockets share cannot attribute one socket's traffic to the other. It is a fixed string,
/// never anything the stream carries.
pub(super) trait TitanFrame: DeserializeOwned + Send {
    const SOCKET: &'static str;
}

/// A parsed price level stream frame: every book Titan simulated in one build round, targeting
/// the block currently being built.
///
/// Books are quoted in human units, so a level is a decimal price and a cumulative volume rather
/// than a pair of token amounts, and tokens arrive as symbols, not addresses: a consumer has to
/// resolve them against its own token metadata before it can price anything.
///
/// Frames are best effort, not complete snapshots: a venue or pair can be absent from one frame
/// and present in the next (observed on 7.7% of frames in a 15 minute capture), so absence must
/// never be read as retirement.
#[derive(Debug, Deserialize)]
pub(super) struct TitanPriceLevelMessage {
    /// The L1 block number the quotes target (the block currently being built).
    pub block: u64,
    /// The beacon slot the frame was built for, when Titan reports one. No freshness decision
    /// rests on it, so a frame without it is still usable.
    pub slot: Option<u64>,
    /// When Titan built this frame, in nanoseconds since the Unix epoch. Frames re-emitted
    /// within one build round share a timestamp, so it is a freshness marker, not an identity.
    pub ts: u64,
    /// One book per venue and pair; a venue appears once for every pair it quotes.
    pub books: Vec<TitanBook>,
}

impl TitanFrame for TitanPriceLevelMessage {
    const SOCKET: &'static str = "price_levels";
}

/// Where a book's ladders came from: the version of the pair the venue priced them at, as the
/// stream's `calculated_from`.
///
/// The three states are told apart here, at the parse boundary, because nothing downstream can:
/// an absent reference and a present unreadable one are both "no sequence to match a snapshot
/// against", yet only the second says the producer sent something this parser does not
/// understand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Provenance {
    /// The book named no reference — the field was missing or null — so there is nothing to
    /// match a snapshot against.
    #[default]
    Absent,
    /// The book was priced at this version of its pair.
    Valid(u128),
    /// The book named something this parser cannot read as a version: a string, a boolean, a
    /// fraction, a negative, a number above `u128`, or a composite. Never trusted, and never a
    /// reason to drop the book — the ladders are what quote, and the reference is metadata about
    /// them.
    ///
    /// Nothing counts this yet: the tracker's `PairProvenance::of_book` is where this variant and
    /// [`Self::Absent`] collapse into one, and so the one place a counter can still tell a
    /// producer that sent an unreadable reference from one that sent none.
    Invalid,
}

/// Reads `calculated_from` as a [`Provenance`] without ever failing the frame.
///
/// A plain `Option<u128>` handles absence and then rejects the whole frame — every venue's
/// ladders in it — over one unreadable reference on one book, which turns optional provenance
/// metadata into a quoting liveness hazard. Leniency stops at the JSON itself: text that is not
/// JSON still rejects the message.
fn lenient_sequence<'de, D>(deserializer: D) -> Result<Provenance, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    let text = raw.get();
    if text.trim() == "null" {
        return Ok(Provenance::Absent);
    }
    Ok(serde_json::from_str::<u128>(text).map_or(Provenance::Invalid, Provenance::Valid))
}

/// One venue's two-sided quote ladder for one pair, priced in `token2` per `token1`.
///
/// Ask prices sit above bid prices, and either side can be empty when the venue quotes only one
/// direction of the pair.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanBook {
    /// Titan's display name for the venue (e.g. `Fermi`), which the stream sometimes leaves
    /// empty or omits. Venues are identified by `quote_address`, never by this label, and nothing
    /// reads it: it is typed here to document what the stream carries and to pin the shapes the
    /// parser accepts for it, not because any decision needs the value.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a display label, deliberately not part of any identity the stream records"
        )
    )]
    pub maker: Option<String>,
    /// The venue address Titan streams this book under — a join key across its streams, which is
    /// not necessarily the venue's executable address (see
    /// [`PAMM_ADDRESS_ALIASES`](super::config::PAMM_ADDRESS_ALIASES)).
    pub quote_address: Bytes,
    /// Base token symbol, the unit of `cum_volume`.
    pub token1: String,
    /// Quote token symbol, the unit of `price`.
    pub token2: String,
    /// The version of the pair this book was priced at, which the override stream repeats per
    /// pair. Read leniently, and read for nothing but matching a snapshot: see [`Provenance`].
    #[serde(rename = "calculated_from", default, deserialize_with = "lenient_sequence")]
    pub calculated_from: Provenance,
    pub asks: Vec<TitanLevel>,
    pub bids: Vec<TitanLevel>,
}

/// A single quote level: `cum_volume` of `token1` is available in total at `price` or better.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanLevel {
    /// `token2` per `token1`, in human units — both tokens' decimals are already applied.
    pub price: f64,
    /// Volume cumulated from the top of the book, in whole `token1`. Non-decreasing down a side,
    /// but consecutive levels can repeat a volume.
    pub cum_volume: f64,
}

/// A parsed quote stream frame: the simulated pre-state of every venue Titan priced in one build
/// round, one entry per venue.
///
/// The frame carries no stamp of its own — every stamp is per venue entry — and its entries are
/// best effort in exactly the way price level books are: a venue present in one frame can be
/// absent from the next without having retired.
#[derive(Debug, Deserialize)]
pub(super) struct TitanOverrideMessage {
    pub venues: Vec<TitanVenueOverride>,
}

impl TitanFrame for TitanOverrideMessage {
    const SOCKET: &'static str = "quote_stream";
}

/// One venue's entry: the accounts Titan overrode to price the venue, and the pair versions the
/// ladders it produced were priced at.
///
/// The stream also carries a `pricedAddresses` list, which nothing here reads: a snapshot is
/// matched to a book by `quote_address` and a pair version, not by the addresses a venue happens
/// to price through.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanVenueOverride {
    /// Titan's display name for the venue. Nothing reads it for identity: `quote_address` is the
    /// identity, exactly as on a book.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a display label, deliberately not part of any identity the stream records"
        )
    )]
    pub maker: Option<String>,
    /// The venue address this entry's snapshot belongs to — the same join key the price level
    /// stream's books carry, and not necessarily the venue's executable address.
    pub quote_address: Bytes,
    /// The L1 block the snapshot is to be simulated on, when Titan names one.
    pub block_number: Option<u64>,
    /// The beacon slot the entry was built for: the only source of a block timestamp.
    pub slot: Option<u64>,
    /// When Titan built the entry, in nanoseconds since the Unix epoch.
    pub timestamp: Option<u64>,
    /// The pairs this entry's sequences belong to, positionally.
    #[serde(default)]
    pub asset_pairs: Vec<TitanAssetPair>,
    /// The version each pair was priced at, one per [`Self::asset_pairs`] entry in the same
    /// order. Several pairs of one entry can share a version: Titan stamps a batch of pairs with
    /// one sequence.
    #[serde(default)]
    pub pair_sequences: Vec<u128>,
    /// The accounts the simulation overrode, keyed by address.
    #[serde(default)]
    pub state_override: BTreeMap<Bytes, TitanAccountOverride>,
}

/// One pair of a venue entry. Unlike the rest of the frame, the stream names these two fields in
/// snake case.
#[derive(Debug, Deserialize)]
pub(super) struct TitanAssetPair {
    pub token_1: Bytes,
    pub token_2: Bytes,
}

/// One account's overridden fields, as Titan sends them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanAccountOverride {
    /// The balance, as a JSON-RPC hex quantity: leading zero digits dropped, so an odd digit
    /// count is normal and zero is the single digit `0x0`.
    pub balance: Option<String>,
    /// The nonce, as a hex quantity.
    pub nonce: Option<String>,
    /// The overridden storage words, keyed by slot — a diff, never a full storage image.
    #[serde(default)]
    pub state_diff: BTreeMap<Bytes, Bytes>,
    /// Set when the entry replaced the account's code. Only its presence is read: see
    /// [`EntryRejection::UnsupportedOverride`].
    pub code: Option<IgnoredAny>,
    /// Set when the entry replaced the account's whole storage instead of diffing it. Read
    /// exactly like [`Self::code`].
    pub state: Option<IgnoredAny>,
}

/// The validated contents of one venue entry: a snapshot of the state Titan priced the venue on,
/// and the pair versions it priced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VenueOverrides {
    /// The venue address the entry was streamed under, exactly as Titan named it: the caller
    /// canonicalizes it, as it does a book's.
    pub quote_address: Bytes,
    /// The pair versions the entry advertised, in Titan's order and Titan's orientation.
    pub pairs: Vec<PairVersion>,
    /// The accounts the simulation overrode, keyed by address.
    pub accounts: BTreeMap<Bytes, AccountOverride>,
    /// The block the snapshot is to be simulated on, when the entry named one.
    pub block_number: Option<u64>,
    /// The second that block is to be simulated at.
    pub block_timestamp: BlockTimestamp,
    /// When Titan built the entry, in nanoseconds since the Unix epoch. Orders two builds of one
    /// venue's snapshot, which is not the order they were received in.
    pub built_at_ns: u64,
}

impl VenueOverrides {
    /// The snapshot as it is retained and serialized, given the `expires_at` second the admitting
    /// clock fixed for it.
    ///
    /// The deadline is the one thing validation cannot supply: it is a function of when the
    /// snapshot was admitted, not of anything Titan sent.
    pub fn snapshot(&self, expires_at: u64) -> PammSimulationOverrides {
        PammSimulationOverrides {
            accounts: self.accounts.clone(),
            block_number: self.block_number,
            block_timestamp: self.block_timestamp.seconds(),
            built_at_ns: self.built_at_ns,
            expires_at,
        }
    }
}

/// One version a venue entry advertised: the pair, and the sequence its ladders were priced at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PairVersion {
    pub token_1: Bytes,
    pub token_2: Bytes,
    /// The number a book of the price level stream repeats in its `calculated_from`.
    pub sequence: u128,
}

/// The Unix second a snapshot's block is to be simulated at, derived from the entry's beacon slot
/// and from nothing else: a local wall clock would date the simulation to when the frame was
/// read, not to the block Titan built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockTimestamp {
    /// Derived from a slot plausible for when the entry was built.
    Derived(u64),
    /// The entry named no slot, so a simulation uses the block's own timestamp.
    Unslotted,
    /// The entry's slot dates the block more than one slot away from when the entry was built —
    /// another chain's slot, another scale, or arithmetic that does not fit — so the derived
    /// second is discarded rather than trusted.
    Implausible,
}

impl BlockTimestamp {
    /// The derived second, or `None` when the entry named no usable slot.
    pub fn seconds(self) -> Option<u64> {
        match self {
            Self::Derived(second) => Some(second),
            Self::Unslotted | Self::Implausible => None,
        }
    }
}

/// What validation made of one venue entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EntryOutcome {
    /// The entry is usable.
    Accepted(VenueOverrides),
    /// The entry is refused whole, for this reason.
    Rejected(EntryRejection),
}

/// Why a venue entry was refused whole.
///
/// An entry is never partially used: a payload that contradicts the documented shape is evidence
/// about the whole payload, and half a snapshot simulates a state no venue was ever in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryRejection {
    /// `quote_address` is not an address, so the entry cannot be attributed to a venue at all.
    NotAVenue,
    /// An overridden account is not keyed by an address.
    NotAnAccount,
    /// A pair names something that is not an address.
    MalformedPair,
    /// A storage slot or a stored word is not a 32-byte EVM word.
    MalformedStorage,
    /// A balance or a nonce is not a hex quantity, or a nonce does not fit the field that carries
    /// it.
    MalformedQuantity,
    /// The entry named no `timestamp`, so nothing orders one build of its snapshot against
    /// another.
    Unstamped,
    /// The pairs and the sequences disagree in length, leaving it unknown which sequence belongs
    /// to which pair. Zipping the shorter of the two would advertise the snapshot under a pair
    /// version it was never priced at.
    SequenceMismatch,
    /// The entry carried no pair, or no account: nothing to match, or nothing to simulate.
    NoData,
    /// An account overrode its code, or replaced its whole storage instead of diffing it, so the
    /// snapshot no longer describes the venue the chain holds.
    UnsupportedOverride,
    /// The snapshot is past a per-entry cap ([`MAX_ACCOUNTS`] accounts, [`MAX_SLOTS`] storage
    /// words, [`MAX_STORAGE_BYTES`] of storage).
    OverCap,
}

/// Validates every venue entry of `frame`, in stream order: one outcome per entry.
pub(super) fn validate(frame: TitanOverrideMessage) -> Vec<EntryOutcome> {
    frame
        .venues
        .into_iter()
        .map(|entry| match validated(entry) {
            Ok(overrides) => EntryOutcome::Accepted(overrides),
            Err(rejection) => EntryOutcome::Rejected(rejection),
        })
        .collect()
}

/// The entry's contents, or the first reason it cannot be used.
fn validated(entry: TitanVenueOverride) -> Result<VenueOverrides, EntryRejection> {
    if entry.quote_address.len() != ADDRESS_BYTES {
        return Err(EntryRejection::NotAVenue);
    }
    let Some(built_at_ns) = entry.timestamp else {
        return Err(EntryRejection::Unstamped);
    };
    if entry.asset_pairs.len() != entry.pair_sequences.len() {
        return Err(EntryRejection::SequenceMismatch);
    }
    if entry.asset_pairs.is_empty() || entry.state_override.is_empty() {
        return Err(EntryRejection::NoData);
    }
    ensure_within_caps(&entry.state_override)?;

    Ok(VenueOverrides {
        quote_address: entry.quote_address,
        pairs: pair_versions(entry.asset_pairs, &entry.pair_sequences)?,
        accounts: validated_accounts(entry.state_override)?,
        block_number: entry.block_number,
        block_timestamp: block_timestamp(entry.slot, built_at_ns),
        built_at_ns,
    })
}

/// Errors unless the snapshot fits every per-entry cap.
///
/// The caps are checked on Titan's own values, before any width is, so they bound what one
/// frame can make this process retain even when the widths turn out wrong. At the 32-byte width a
/// word is validated to, the slot cap is the binding one; the byte cap bounds a payload that grows.
fn ensure_within_caps(
    accounts: &BTreeMap<Bytes, TitanAccountOverride>,
) -> Result<(), EntryRejection> {
    if accounts.len() > MAX_ACCOUNTS {
        return Err(EntryRejection::OverCap);
    }
    let mut slots: usize = 0;
    let mut bytes: usize = 0;
    for account in accounts.values() {
        slots = slots.saturating_add(account.state_diff.len());
        for (slot, word) in &account.state_diff {
            bytes = bytes.saturating_add(slot.len() + word.len());
        }
    }
    if slots > MAX_SLOTS || bytes > MAX_STORAGE_BYTES {
        return Err(EntryRejection::OverCap);
    }
    Ok(())
}

/// Pairs each asset pair with the sequence at its own position. The lengths are equal by the time
/// this runs, so no pair is left without a version and no version without a pair.
fn pair_versions(
    pairs: Vec<TitanAssetPair>,
    sequences: &[u128],
) -> Result<Vec<PairVersion>, EntryRejection> {
    let mut versions = Vec::with_capacity(pairs.len());
    for (pair, sequence) in pairs.into_iter().zip(sequences) {
        if pair.token_1.len() != ADDRESS_BYTES || pair.token_2.len() != ADDRESS_BYTES {
            return Err(EntryRejection::MalformedPair);
        }
        versions.push(PairVersion {
            token_1: pair.token_1,
            token_2: pair.token_2,
            sequence: *sequence,
        });
    }
    Ok(versions)
}

/// The overridden accounts as the snapshot retains them: quantities decoded, widths checked.
fn validated_accounts(
    streamed: BTreeMap<Bytes, TitanAccountOverride>,
) -> Result<BTreeMap<Bytes, AccountOverride>, EntryRejection> {
    let mut accounts = BTreeMap::new();
    for (address, account) in streamed {
        if address.len() != ADDRESS_BYTES {
            return Err(EntryRejection::NotAnAccount);
        }
        accounts.insert(address, account_override(account)?);
    }
    Ok(accounts)
}

fn account_override(account: TitanAccountOverride) -> Result<AccountOverride, EntryRejection> {
    if account.code.is_some() || account.state.is_some() {
        return Err(EntryRejection::UnsupportedOverride);
    }
    let balance = match account.balance.as_deref() {
        Some(quantity) => Some(quantity_bytes(quantity).ok_or(EntryRejection::MalformedQuantity)?),
        None => None,
    };
    let nonce = match account.nonce.as_deref() {
        Some(quantity) => Some(quantity_u64(quantity).ok_or(EntryRejection::MalformedQuantity)?),
        None => None,
    };
    for (slot, word) in &account.state_diff {
        if slot.len() != WORD_BYTES || word.len() != WORD_BYTES {
            return Err(EntryRejection::MalformedStorage);
        }
    }
    Ok(AccountOverride { balance, nonce, state_diff: account.state_diff })
}

/// The hex digits of a `0x`-prefixed quantity, or `None` for anything else.
fn quantity_digits(quantity: &str) -> Option<&str> {
    let digits = quantity.strip_prefix("0x")?;
    let readable = !digits.is_empty() &&
        digits
            .bytes()
            .all(|digit| digit.is_ascii_hexdigit());
    readable.then_some(digits)
}

/// A hex quantity as its minimal big-endian bytes, at most a `u256` wide.
///
/// A quantity drops leading zero digits, so it can carry an odd number of them —
/// `0x31a98d578d71685db` — which is exactly what fixed-width hex parsing rejects. A zero
/// quantity decodes to one zero byte, never to no bytes at all.
fn quantity_bytes(quantity: &str) -> Option<Bytes> {
    let digits = quantity_digits(quantity)?;
    if digits.len() > 2 * WORD_BYTES {
        return None;
    }
    let bytes =
        if digits.len() % 2 == 0 { hex::decode(digits) } else { hex::decode(format!("0{digits}")) };
    bytes.ok().map(Bytes::from)
}

/// A hex quantity as a `u64`, or `None` when it does not fit one.
fn quantity_u64(quantity: &str) -> Option<u64> {
    u64::from_str_radix(quantity_digits(quantity)?, 16).ok()
}

/// The Unix second the entry's block is to be simulated at, derived from its beacon slot.
///
/// The derived second is trusted only while it sits within one slot of when the entry was built:
/// Titan stamps an entry during the slot before the one it builds for, so the block's second runs
/// a little ahead of the stamp. Anything further apart is not this chain's slot, and is discarded
/// rather than replaced by a local clock.
fn block_timestamp(slot: Option<u64>, built_at_ns: u64) -> BlockTimestamp {
    let Some(slot) = slot else {
        return BlockTimestamp::Unslotted;
    };
    let Some(derived) = slot
        .checked_mul(SLOT.as_secs())
        .and_then(|elapsed| elapsed.checked_add(BEACON_GENESIS))
    else {
        return BlockTimestamp::Implausible;
    };
    let built_at = built_at_ns / NANOS_PER_SECOND;
    let plausible = derived <= built_at.saturating_add(SLOT.as_secs()) &&
        built_at <= derived.saturating_add(SLOT.as_secs());
    if plausible {
        BlockTimestamp::Derived(derived)
    } else {
        BlockTimestamp::Implausible
    }
}

/// Yields parsed frames from `url` for as long as the returned stream is polled.
///
/// Maintains the connection in the background of the stream itself: any disconnect, read error,
/// server-side close, or idle timeout is retried forever with capped exponential backoff (reset
/// only once a frame is actually parsed, so a socket that connects and immediately drops still
/// backs off instead of busy-looping). Malformed frames are logged and skipped; this stream never
/// terminates.
///
/// Both Titan sockets are read through this one function; the frame type names which socket its
/// logs describe.
pub(super) fn messages<T: TitanFrame>(
    url: String,
    settings: ConnectionSettings,
) -> impl Stream<Item = T> + Send {
    stream! {
        let mut attempt: u32 = 0;
        loop {
            match timeout(settings.connect_timeout, connect_async(url.as_str())).await {
                Ok(Ok((mut ws_stream, _))) => {
                    info!(%url, socket = T::SOCKET, "Connected to Titan pAMM stream");
                    // The deadline moves only when a frame parses, so control frames and
                    // unparsable text cannot keep a socket that sends no frames alive.
                    let mut idle_deadline = deadline_after(settings.read_idle_timeout);
                    loop {
                        let message = match timeout_at(idle_deadline, ws_stream.next()).await {
                            Ok(Some(message)) => message,
                            // Stream ended: the server hung up without sending a close frame.
                            Ok(None) => {
                                warn!(socket = T::SOCKET, "Titan stream ended; reconnecting");
                                telemetry::record_reconnect(T::SOCKET, ReconnectReason::Ended);
                                break;
                            }
                            // No parsed frame within the idle window: assume a stalled socket.
                            Err(_elapsed) => {
                                warn!(
                                    socket = T::SOCKET,
                                    idle_secs = settings.read_idle_timeout.as_secs(),
                                    "No parsed Titan frame within idle timeout; reconnecting"
                                );
                                let reason = ReconnectReason::IdleTimeout;
                                telemetry::record_reconnect(T::SOCKET, reason);
                                break;
                            }
                        };

                        match message {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<T>(text.as_str())
                                {
                                    // A parsed frame proves the connection is healthy: reset the
                                    // reconnect backoff. The idle window restarts only once the
                                    // consumer resumes the stream after the yield, so it measures
                                    // the time spent waiting on the socket, not the time the
                                    // consumer spends between polls.
                                    Ok(message) => {
                                        attempt = 0;
                                        yield message;
                                        idle_deadline = deadline_after(settings.read_idle_timeout);
                                    }
                                    // Unparseable frame: log and keep the connection.
                                    Err(e) => {
                                        warn!(
                                            socket = T::SOCKET,
                                            error = %e,
                                            "Failed to parse Titan message"
                                        );
                                        telemetry::record_frame_rejected(
                                            T::SOCKET,
                                            RejectReason::ParseError,
                                        );
                                    }
                                }
                            }
                            // Titan only sends JSON text; a binary frame is unexpected. Skip it
                            // and keep the (otherwise healthy) connection.
                            Ok(Message::Binary(bytes)) => {
                                warn!(
                                    socket = T::SOCKET,
                                    len = bytes.len(),
                                    "Ignoring unexpected binary Titan frame"
                                );
                            }
                            // Keep-alive frames. tokio-tungstenite answers pings with pongs
                            // automatically while the stream is polled, so there is nothing to
                            // do.
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                            // `Frame` is only produced when *sending* raw frames; the read side
                            // never yields it, so this arm is unreachable in practice — kept only
                            // for match exhaustiveness.
                            Ok(Message::Frame(_)) => {}
                            // Server initiated a graceful close — reconnect.
                            Ok(Message::Close(frame)) => {
                                warn!(
                                    socket = T::SOCKET,
                                    ?frame,
                                    "Titan stream closed by server; reconnecting"
                                );
                                telemetry::record_reconnect(T::SOCKET, ReconnectReason::Closed);
                                break;
                            }
                            // Transport/protocol error (broken pipe, invalid frame, ...) —
                            // reconnect.
                            Err(e) => {
                                warn!(
                                    socket = T::SOCKET,
                                    error = %e,
                                    "Titan stream read error; reconnecting"
                                );
                                telemetry::record_reconnect(T::SOCKET, ReconnectReason::ReadError);
                                break;
                            }
                        }
                    }
                }
                // Connection refused / TLS error — fall through to backoff and retry.
                Ok(Err(e)) => {
                    warn!(
                        socket = T::SOCKET,
                        error = %e,
                        "Failed to connect to Titan stream; retrying"
                    );
                    telemetry::record_reconnect(T::SOCKET, ReconnectReason::ConnectFailed);
                }
                // Handshake did not complete within the timeout — retry after backoff.
                Err(_elapsed) => {
                    warn!(
                        socket = T::SOCKET,
                        timeout_secs = settings.connect_timeout.as_secs(),
                        "Titan connect timed out; retrying"
                    );
                    telemetry::record_reconnect(T::SOCKET, ReconnectReason::ConnectTimeout);
                }
            }

            attempt = attempt.saturating_add(1);
            let backoff = backoff(attempt, settings.max_backoff);
            warn!(
                socket = T::SOCKET,
                seconds = backoff.as_secs(),
                attempt,
                "Backing off before reconnecting to Titan"
            );
            sleep(backoff).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr, sync::atomic::Ordering};

    use futures::SinkExt;
    use rstest::rstest;

    use super::{
        super::{
            overrides::test_overrides,
            telemetry::{
                recorded::{counter_value, record_async},
                FRAMES_REJECTED, RECONNECTS,
            },
            test_support::{
                address, frame_text, frame_then_repeat, wall_nanos_now, FakeTitan, PAMM, USDC, WETH,
            },
        },
        *,
    };

    /// A frame captured verbatim from the live stream (2026-10-08); the file name carries the
    /// frame's `ts` field.
    const CAPTURED_MESSAGE: &str =
        include_str!("test_responses/pamm_price_levels_v2_1791475493806326998.json");

    /// Override frames captured verbatim from the live `pamm_quote_stream_v2` socket
    /// (2026-10-08). The frames carry no timestamp of their own — every stamp is per venue entry —
    /// so the file names carry the nanosecond the capture received each frame at. That stamp lives
    /// in the capture's envelope, not in the frame, so unlike the price level fixtures above these
    /// two names cannot be checked against their own contents.
    const CAPTURED_OVERRIDES: &str =
        include_str!("test_responses/pamm_quote_stream_v2_1791475494921154915.json");
    const EARLIER_CAPTURED_OVERRIDES: &str =
        include_str!("test_responses/pamm_quote_stream_v2_1791475494521439938.json");

    /// The venue TaurusFi streams under, whose entry of [`CAPTURED_OVERRIDES`]
    /// [`test_overrides`] transcribes by hand.
    const TAURUSFI: &str = "0x3ce2672aa806138585920421406bf0decb8130cb";
    /// One of the two accounts that entry overrides a balance and a nonce of.
    const TAURUSFI_ACCOUNT: &str = "0xba443ecec71dc7123bd68c2082d13e3299cde896";
    /// A token of the entry's first pair (`NVDAx`).
    const NVDAX: &str = "0xc845b2894dbddd03858fd2d643b4ef725fe0849d";
    /// The second [`test_overrides`] records the entry's snapshot as expiring at: one slot after
    /// the second it was built in, as admission fixes it.
    const TAURUSFI_EXPIRES_AT: u64 = 1_791_475_506;
    /// The Unix second the entry's beacon slot 15387623 begins at.
    const TAURUSFI_BLOCK_SECOND: u64 = 1_791_475_499;

    /// Nanoseconds since the epoch for 2023-11 and 2033-05. A `ts` in seconds or milliseconds
    /// would fall far below the lower bound, so the window pins the scale rather than the date.
    const NANOS_LOWER_BOUND: u64 = 1_700_000_000_000_000_000;
    const NANOS_UPPER_BOUND: u64 = 2_000_000_000_000_000_000;

    #[test]
    fn parses_captured_live_message() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        assert_eq!(message.block, 26148760);
        assert_eq!(message.slot, Some(15387623));
        assert!(
            (NANOS_LOWER_BOUND..NANOS_UPPER_BOUND).contains(&message.ts),
            "ts is not nanosecond-scale: {}",
            message.ts
        );

        // Every venue Titan builds with arrives in one flat book list.
        let venues: HashSet<&Bytes> = message
            .books
            .iter()
            .map(|book| &book.quote_address)
            .collect();
        assert!(venues.len() >= 2, "expected several venues, got {}", venues.len());

        let fermi = message
            .books
            .iter()
            .find(|book| {
                book.quote_address == Bytes::from_str(PAMM).unwrap() &&
                    book.token1 == "WBTC" &&
                    book.token2 == "USDC"
            })
            .expect("Fermi WBTC/USDC book");
        assert_eq!(fermi.maker.as_deref(), Some("Fermi"));
        assert!(!fermi.asks.is_empty());
        assert!(!fermi.bids.is_empty());

        // A level without volume carries no depth, so a zero would mean the ladder lost its
        // quantities in parsing.
        for book in &message.books {
            for level in book.asks.iter().chain(&book.bids) {
                assert!(level.cum_volume > 0.0, "non-positive cumVolume: {level:?}");
                assert!(level.price > 0.0, "non-positive price: {level:?}");
            }
        }
    }

    /// `ts` is the only per-frame freshness signal, so a frame without it is unusable and must
    /// not parse. `slot` carries no freshness decision, so a frame without it still parses.
    #[test]
    fn requires_ts_but_not_slot() {
        let without_ts = r#"{"slot": 1, "block": 2, "books": []}"#;
        assert!(serde_json::from_str::<TitanPriceLevelMessage>(without_ts).is_err());

        let without_slot = r#"{"block": 2, "ts": 3, "books": []}"#;
        let message: TitanPriceLevelMessage =
            serde_json::from_str(without_slot).expect("valid JSON");
        assert_eq!(message.slot, None);
    }

    /// `maker` is a display label the stream sometimes leaves empty or omits, and
    /// `calculated_from` is the pair version the book was priced at — a number that overflows
    /// `u64`, which is why it is read as a `u128`.
    #[rstest]
    #[case::named_maker(r#""maker": "Fermi","#, Some("Fermi"))]
    #[case::empty_maker(r#""maker": "","#, Some(""))]
    #[case::null_maker(r#""maker": null,"#, None)]
    #[case::absent_maker("", None)]
    fn tolerates_internal_fields_and_unlabelled_makers(
        #[case] maker: &str,
        #[case] expected: Option<&str>,
    ) {
        let json = format!(
            r#"{{
                {maker}
                "quoteAddress": "{PAMM}",
                "token1": "WBTC",
                "token2": "USDC",
                "calculated_from": 283851543383726791766048878,
                "asks": [{{"price": 80997.89405475458, "cumVolume": 0.00012346}}],
                "bids": []
            }}"#
        );

        let book: TitanBook = serde_json::from_str(&json).expect("valid book");
        assert_eq!(book.maker.as_deref(), expected);
        assert_eq!(book.quote_address, Bytes::from_str(PAMM).unwrap());
        assert_eq!(book.calculated_from, Provenance::Valid(283_851_543_383_726_791_766_048_878));
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.asks[0].price, 80997.89405475458);
        assert_eq!(book.asks[0].cum_volume, 0.00012346);
        assert!(book.bids.is_empty());
    }

    #[test]
    fn backoff_grows_exponentially_up_to_the_cap() {
        let max_backoff = ConnectionSettings::default().max_backoff;
        assert_eq!(backoff(1, max_backoff), Duration::from_secs(2));
        assert_eq!(backoff(4, max_backoff), Duration::from_secs(16));
        assert_eq!(backoff(5, max_backoff), max_backoff);
        assert_eq!(backoff(100, max_backoff), max_backoff);
        // Exponent overflow must saturate to the cap rather than panic.
        assert_eq!(backoff(u32::MAX, max_backoff), max_backoff);
    }

    /// Settings that reconnect quickly enough for a test: a 100 ms idle timeout and a 10 ms
    /// backoff.
    fn fast_settings() -> ConnectionSettings {
        ConnectionSettings {
            connect_timeout: Duration::from_secs(1),
            read_idle_timeout: Duration::from_millis(100),
            max_backoff: Duration::from_millis(10),
        }
    }

    fn frame() -> Message {
        Message::Text(frame_text(100, wall_nanos_now()).into())
    }

    /// Polls `stream` until it yields a frame, failing if none arrives within two seconds.
    async fn next_frame(stream: &mut (impl Stream<Item = TitanPriceLevelMessage> + Unpin)) {
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("a frame within two seconds")
            .expect("the stream never ends");
    }

    /// Traffic that is not a parsed frame does not reset the idle timeout: the socket is
    /// dropped and reconnected even though it never went quiet.
    #[rstest]
    #[case::ping_only(Message::Ping(Vec::new().into()), false)]
    #[case::malformed_text(Message::Text("nonsense".into()), true)]
    fn non_frame_traffic_does_not_count_as_liveness(
        #[case] filler: Message,
        #[case] rejected_as_parse_error: bool,
    ) {
        let (connections, snapshot) = record_async(async {
            let fake =
                FakeTitan::spawn(frame_then_repeat(frame(), filler, Duration::from_millis(10)))
                    .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // The second frame can only come from a new connection, which the idle timeout
            // opens once the filler fails to reset it.
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert!(connections >= 2, "no reconnect on non-frame traffic");
        assert!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "idle_timeout"), ("socket", "price_levels")]
            ) >= 1
        );
        let parse_errors = counter_value(
            &snapshot,
            FRAMES_REJECTED,
            &[("reason", "parse_error"), ("socket", "price_levels")],
        );
        assert_eq!(parse_errors > 0, rejected_as_parse_error);
    }

    #[test]
    fn server_close_frame_reconnects() {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(|_, mut socket| async move {
                if socket.send(frame()).await.is_err() {
                    return;
                }
                let _ = socket.send(Message::Close(None)).await;
            })
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // The close frame follows the first frame, so the second frame can only come from a
            // new connection.
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert_eq!(connections, 2);
        assert_eq!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "closed"), ("socket", "price_levels")]
            ),
            1
        );
    }

    /// Both sockets are read through this one function, so the `socket` label is the only thing
    /// that tells their failures apart: an override socket that keeps closing must not read as the
    /// price level socket flapping, which is the signal that quoting liveness is at risk.
    #[test]
    fn a_reconnect_names_the_socket_it_happened_on() {
        let ((), snapshot) = record_async(async {
            let fake = FakeTitan::spawn(|_, mut socket| async move {
                let _ = socket.send(Message::Close(None)).await;
            })
            .await;
            let stream = messages::<TitanOverrideMessage>(fake.url(), fast_settings());
            tokio::pin!(stream);
            // The fake sends no frame, so the stream yields nothing: polling it is what drives the
            // connect, close and back off loop at all.
            let _ = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        });

        assert!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "closed"), ("socket", "quote_stream")]
            ) >= 1,
            "the override socket's reconnects are not counted under its own socket"
        );
        assert_eq!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "closed"), ("socket", "price_levels")]
            ),
            0,
            "the override socket's reconnects were attributed to the price level socket"
        );
    }

    /// The idle timeout measures upstream silence, not the consumer's pace: a consumer that
    /// pauses for longer than the timeout finds the same connection when it resumes.
    #[test]
    fn slow_consumer_does_not_trigger_the_idle_timeout() {
        let (connections, snapshot) = record_async(async {
            let fake =
                FakeTitan::spawn(frame_then_repeat(frame(), frame(), Duration::from_millis(20)))
                    .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // Three idle timeouts pass without a poll.
            tokio::time::sleep(Duration::from_millis(300)).await;
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert_eq!(connections, 1, "reconnected while the consumer was not polling");
        assert_eq!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "idle_timeout"), ("socket", "price_levels")]
            ),
            0
        );
    }

    #[tokio::test]
    async fn parsed_frames_keep_the_connection_alive() {
        let fake =
            FakeTitan::spawn(frame_then_repeat(frame(), frame(), Duration::from_millis(30))).await;
        let settings =
            ConnectionSettings { read_idle_timeout: Duration::from_millis(150), ..fast_settings() };
        let stream = messages(fake.url(), settings);
        tokio::pin!(stream);
        for _ in 0..10 {
            next_frame(&mut stream).await;
        }
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    }

    /// The endpoint and the variable that replaces it are the deployment contract of the second
    /// socket, so they are spelled out rather than derived.
    #[test]
    fn the_override_endpoint_is_pinned() {
        assert_eq!(
            TITAN_QUOTE_STREAM_URL,
            "wss://eu.data.titanbuilder.xyz/ws/pamm_quote_stream_v2"
        );
        assert_eq!(TITAN_QUOTE_STREAM_URL_ENV, "TITAN_PAMM_QUOTE_STREAM_URL");
    }

    /// One reader serves both sockets: the override socket is read, reconnected and parsed by
    /// exactly the machinery the price level socket is.
    #[tokio::test]
    async fn the_reader_serves_the_override_socket() {
        let fake = FakeTitan::spawn(frame_then_repeat(
            Message::Text(CAPTURED_OVERRIDES.into()),
            Message::Ping(Vec::new().into()),
            Duration::from_millis(50),
        ))
        .await;
        let stream = messages::<TitanOverrideMessage>(fake.url(), fast_settings());
        tokio::pin!(stream);
        let message = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("a frame within two seconds")
            .expect("the stream never ends");
        assert_eq!(message.venues.len(), 9);
    }

    fn parsed(frame: &str) -> TitanOverrideMessage {
        serde_json::from_str(frame).expect("a valid override frame")
    }

    /// The validated entries of `frame`, which must all have been accepted.
    fn accepted(frame: &str) -> Vec<VenueOverrides> {
        validate(parsed(frame))
            .into_iter()
            .map(|outcome| match outcome {
                EntryOutcome::Accepted(overrides) => overrides,
                EntryOutcome::Rejected(reason) => panic!("rejected a captured entry: {reason:?}"),
            })
            .collect()
    }

    #[test]
    fn parses_the_captured_override_frames() {
        let message = parsed(CAPTURED_OVERRIDES);
        assert_eq!(message.venues.len(), 9);
        assert_eq!(
            parsed(EARLIER_CAPTURED_OVERRIDES)
                .venues
                .len(),
            8
        );

        let taurusfi = message
            .venues
            .iter()
            .find(|venue| venue.quote_address == address(TAURUSFI))
            .expect("the TaurusFi entry");
        assert_eq!(taurusfi.maker.as_deref(), Some("TaurusFi"));
        assert_eq!(taurusfi.block_number, Some(26_148_760));
        assert_eq!(taurusfi.slot, Some(15_387_623));
        assert_eq!(taurusfi.timestamp, Some(1_791_475_494_775_213_874));
        // The captured sequences verbatim: both above `u64::MAX`, which a narrower field would
        // have failed the whole frame on.
        assert_eq!(
            taurusfi.pair_sequences,
            vec![283_851_543_383_726_791_766_048_793, 283_851_543_383_726_791_766_048_835]
        );
        assert!(taurusfi
            .pair_sequences
            .iter()
            .all(|sequence| *sequence > u128::from(u64::MAX)));
        assert_eq!(taurusfi.asset_pairs.len(), 2);
        assert_eq!(taurusfi.asset_pairs[0].token_1, address(NVDAX));
        assert_eq!(taurusfi.asset_pairs[0].token_2, address(USDC));
        assert_eq!(taurusfi.state_override.len(), 3);
    }

    /// The captured TaurusFi entry validates into exactly the snapshot `overrides.rs` transcribes
    /// from the same frame by hand: the hex quantities (the odd-digit and the zero one included),
    /// the 32-byte storage words, the block the entry names, the slot-derived block timestamp and
    /// the build stamp.
    #[test]
    fn a_captured_entry_validates_into_the_transcribed_snapshot() {
        let taurusfi = accepted(CAPTURED_OVERRIDES)
            .into_iter()
            .find(|entry| entry.quote_address == address(TAURUSFI))
            .expect("the TaurusFi entry");

        assert_eq!(taurusfi.snapshot(TAURUSFI_EXPIRES_AT), test_overrides());
    }

    /// Live traffic must pass validation whole: every entry of both captured frames, each with
    /// its sequences paired 1:1 to its pairs and its block dated from its beacon slot.
    #[test]
    fn every_captured_entry_validates() {
        for frame in [CAPTURED_OVERRIDES, EARLIER_CAPTURED_OVERRIDES] {
            let entries = accepted(frame);
            assert_eq!(entries.len(), parsed(frame).venues.len());
            for entry in entries {
                assert_eq!(entry.quote_address.len(), 20);
                assert!(!entry.pairs.is_empty(), "an entry carried no pair version");
                assert!(!entry.accounts.is_empty(), "an entry carried no account");
                assert_eq!(entry.block_number, Some(26_148_760));
                assert_eq!(entry.block_timestamp, BlockTimestamp::Derived(TAURUSFI_BLOCK_SECOND));
                assert_eq!(entry.snapshot(0).block_timestamp, Some(TAURUSFI_BLOCK_SECOND));
            }
        }
    }

    /// Titan stamps a batch of pairs with one sequence, so a sequence is not an identity on its
    /// own: the captured Metric entry repeats one across all eleven of its pairs.
    #[test]
    fn one_sequence_can_cover_several_pairs_of_an_entry() {
        let batched = accepted(CAPTURED_OVERRIDES)
            .into_iter()
            .find(|entry| entry.pairs.len() == 11)
            .expect("the eleven-pair entry");

        let sequences: HashSet<u128> = batched
            .pairs
            .iter()
            .map(|pair| pair.sequence)
            .collect();
        assert_eq!(sequences.len(), 1, "expected one sequence across the batch");
    }

    /// The captured TaurusFi entry's fields as raw JSON, so a case can replace exactly one and
    /// stay anchored to the shape Titan sends. Every replacement makes the frame synthetic.
    struct Entry {
        quote_address: String,
        slot: String,
        timestamp: String,
        asset_pairs: String,
        pair_sequences: String,
        state_override: String,
    }

    impl Default for Entry {
        fn default() -> Self {
            Self {
                quote_address: format!(r#""{TAURUSFI}""#),
                slot: "15387623".to_string(),
                timestamp: "1791475494775213874".to_string(),
                asset_pairs: format!(r#"[{{"token_1": "{NVDAX}", "token_2": "{USDC}"}}]"#),
                pair_sequences: "[283851543383726791766048793]".to_string(),
                state_override: format!(
                    r#"{{"{TAURUSFI_ACCOUNT}": {{"balance": "0xb0618b2431873a", "nonce": "0x1"}}}}"#
                ),
            }
        }
    }

    impl Entry {
        /// A one-entry override frame carrying these fields.
        fn frame(&self) -> String {
            let Self {
                quote_address,
                slot,
                timestamp,
                asset_pairs,
                pair_sequences,
                state_override,
            } = self;
            format!(
                r#"{{"venues": [{{
                    "maker": "TaurusFi",
                    "quoteAddress": {quote_address},
                    "pricedAddresses": ["{TAURUSFI}"],
                    "blockNumber": 26148760,
                    "slot": {slot},
                    "timestamp": {timestamp},
                    "assetPairs": {asset_pairs},
                    "pairSequences": {pair_sequences},
                    "stateOverride": {state_override}
                }}]}}"#
            )
        }

        /// The outcome of validating this entry, which the frame carries as its only one.
        fn outcome(&self) -> EntryOutcome {
            let mut outcomes = validate(parsed(&self.frame()));
            assert_eq!(outcomes.len(), 1, "expected exactly one venue entry");
            outcomes.remove(0)
        }

        fn rejection(&self) -> EntryRejection {
            match self.outcome() {
                EntryOutcome::Rejected(reason) => reason,
                EntryOutcome::Accepted(overrides) => panic!("accepted {overrides:?}"),
            }
        }

        fn validated(&self) -> VenueOverrides {
            match self.outcome() {
                EntryOutcome::Accepted(overrides) => overrides,
                EntryOutcome::Rejected(reason) => panic!("rejected: {reason:?}"),
            }
        }
    }

    /// The unmutated builder reproduces an accepted entry, so every rejection below is caused by
    /// the one field its case replaces.
    #[test]
    fn the_synthetic_entry_is_accepted_unmutated() {
        let entry = Entry::default().validated();
        assert_eq!(entry.quote_address, address(TAURUSFI));
        assert_eq!(entry.pairs.len(), 1);
        assert_eq!(entry.pairs[0].sequence, 283_851_543_383_726_791_766_048_793);
        assert_eq!(entry.accounts.len(), 1);
        assert_eq!(entry.built_at_ns, 1_791_475_494_775_213_874);
    }

    /// Sequences that do not pair 1:1 with the asset pairs leave it unknown which pair each
    /// sequence belongs to; zipping the shorter of the two would advertise the snapshot under a
    /// pair version it was never priced at.
    #[rstest]
    #[case::fewer_sequences("[283851543383726791766048793]")]
    #[case::more_sequences(
        "[283851543383726791766048793, 283851543383726791766048835, 283851543383726791766048836]"
    )]
    #[case::no_sequences("[]")]
    fn a_sequence_length_mismatch_rejects_the_entry(#[case] pair_sequences: &str) {
        let entry = Entry {
            asset_pairs: format!(
                r#"[{{"token_1": "{NVDAX}", "token_2": "{USDC}"}},
                    {{"token_1": "{WETH}", "token_2": "{USDC}"}}]"#
            ),
            pair_sequences: pair_sequences.to_string(),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::SequenceMismatch);
    }

    /// An entry that overrides code, or replaces an account's whole storage instead of diffing
    /// it, no longer describes the venue the chain holds. It is refused whole rather than
    /// stripped of the offending account: the accounts of one snapshot were simulated together.
    #[rstest]
    #[case::code(r#""code": "0x6080604052","#)]
    #[case::full_state(
        r#""state": {"0x8dc4adfc36586ffb9fe5c2dba18756239463dda9032bbfa7fd13f3bb3b16358f":
                    "0x6ac7bf2b01000000000001a11c42ae030000007600000076000000000003969f"},"#
    )]
    fn code_or_a_full_state_rejects_the_entry(#[case] unsupported: &str) {
        let entry = Entry {
            state_override: format!(
                r#"{{"{TAURUSFI_ACCOUNT}": {{"balance": "0xb0618b2431873a", "nonce": "0x1"}},
                     "0x27df873222a07fa78d0e52797380dbbfa0e98d43": {{{unsupported}
                        "balance": "0x9ae4c04827bc33", "nonce": "0x2b2d"}}}}"#
            ),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::UnsupportedOverride);
    }

    /// A null `code` or `state` overrides nothing, so it is read as the absence it is.
    #[rstest]
    #[case::null_code(r#""code": null,"#)]
    #[case::null_state(r#""state": null,"#)]
    fn a_null_code_or_state_is_not_an_override(#[case] null_field: &str) {
        let entry = Entry {
            state_override: format!(
                r#"{{"{TAURUSFI_ACCOUNT}": {{{null_field} "balance": "0xb0618b2431873a",
                     "nonce": "0x1"}}}}"#
            ),
            ..Entry::default()
        };

        assert_eq!(entry.validated().accounts.len(), 1);
    }

    /// `accounts` synthetic accounts, each overriding `slots` storage words of `word_bytes`.
    fn synthetic_state_override(accounts: usize, slots: usize, word_bytes: usize) -> String {
        let word = format!("0x{}", "ab".repeat(word_bytes));
        let accounts: Vec<String> = (0..accounts)
            .map(|account| {
                let diff: Vec<String> = (0..slots)
                    .map(|slot| format!(r#""0x{:064x}": "{word}""#, account * slots + slot))
                    .collect();
                format!(r#""0x{:040x}": {{"stateDiff": {{{}}}}}"#, account + 1, diff.join(", "))
            })
            .collect();
        format!("{{{}}}", accounts.join(", "))
    }

    /// A snapshot past any one of the per-entry caps is refused whole: what one frame can make
    /// this process retain has to be bounded before anything in it is trusted.
    #[rstest]
    #[case::too_many_accounts(33, 0, 32)]
    #[case::too_many_slots(2, 257, 32)]
    #[case::too_much_storage(1, 200, 400)]
    fn an_over_cap_snapshot_rejects_the_entry(
        #[case] accounts: usize,
        #[case] slots: usize,
        #[case] word_bytes: usize,
    ) {
        let entry = Entry {
            state_override: synthetic_state_override(accounts, slots, word_bytes),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::OverCap);
    }

    /// The caps are inclusive: the largest snapshot that fits them is admitted.
    #[test]
    fn a_snapshot_at_the_caps_is_admitted() {
        let entry =
            Entry { state_override: synthetic_state_override(32, 16, 32), ..Entry::default() }
                .validated();

        assert_eq!(entry.accounts.len(), 32);
        let slots: usize = entry
            .accounts
            .values()
            .map(|account| account.state_diff.len())
            .sum();
        assert_eq!(slots, 512);
    }

    /// Titan writes balances and nonces as JSON-RPC quantities: leading zero digits dropped, so
    /// an odd digit count is normal and a zero is a single digit. A balance keeps its minimal
    /// big-endian bytes, never a fixed width Titan did not send.
    #[rstest]
    #[case::odd_digits("0x31a98d578d71685db", "0x031a98d578d71685db")]
    #[case::zero("0x0", "0x00")]
    #[case::even_digits("0xb0618b2431873a", "0xb0618b2431873a")]
    #[case::one_odd_digit("0xf", "0x0f")]
    fn a_balance_quantity_decodes_to_its_minimal_bytes(
        #[case] quantity: &str,
        #[case] expected: &str,
    ) {
        let entry = Entry {
            state_override: format!(
                r#"{{"{TAURUSFI_ACCOUNT}": {{"balance": "{quantity}", "nonce": "0x2b2d"}}}}"#
            ),
            ..Entry::default()
        };

        let account = entry
            .validated()
            .accounts
            .remove(&address(TAURUSFI_ACCOUNT))
            .expect("the overridden account");
        assert_eq!(account.balance, Some(Bytes::from_str(expected).expect("hex literal")));
        assert_eq!(account.nonce, Some(0x2b2d));
    }

    /// A quantity this parser cannot read, or a nonce that does not fit the field that carries
    /// it, refuses the entry instead of substituting a value the simulation would price on.
    #[rstest]
    #[case::not_hex(r#""balance": "0xzz""#)]
    #[case::unprefixed(r#""balance": "b0618b2431873a""#)]
    #[case::empty(r#""balance": "0x""#)]
    #[case::decimal(r#""balance": "12345""#)]
    #[case::wider_than_a_u256(
        r#""balance":
           "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff""#
    )]
    #[case::nonce_overflow(r#""nonce": "0x10000000000000000""#)]
    #[case::nonce_not_hex(r#""nonce": "0x1g""#)]
    #[case::nonce_signed(r#""nonce": "0x+1""#)]
    fn a_malformed_quantity_rejects_the_entry(#[case] quantity: &str) {
        let entry = Entry {
            state_override: format!(r#"{{"{TAURUSFI_ACCOUNT}": {{{quantity}}}}}"#),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::MalformedQuantity);
    }

    /// The widest nonce the field holds is still a nonce.
    #[test]
    fn a_nonce_at_the_width_of_its_field_is_accepted() {
        let entry = Entry {
            state_override: format!(
                r#"{{"{TAURUSFI_ACCOUNT}": {{"nonce": "0xffffffffffffffff"}}}}"#
            ),
            ..Entry::default()
        };

        let account = entry
            .validated()
            .accounts
            .remove(&address(TAURUSFI_ACCOUNT))
            .expect("the overridden account");
        assert_eq!(account.nonce, Some(u64::MAX));
        assert_eq!(account.balance, None);
    }

    /// Addresses are 20 bytes and storage slots and words are 32: a payload that disagrees is not
    /// the shape Titan documents, and a snapshot half-read from it simulates a state no venue was
    /// ever in.
    #[rstest]
    #[case::short_venue(
        Entry { quote_address: r#""0x3ce2672aa806138585920421406bf0decb81""#.to_string(),
                ..Entry::default() },
        EntryRejection::NotAVenue
    )]
    #[case::long_venue(
        Entry { quote_address: r#""0x3ce2672aa806138585920421406bf0decb8130cbcb""#.to_string(),
                ..Entry::default() },
        EntryRejection::NotAVenue
    )]
    #[case::pair_token(
        Entry { asset_pairs: r#"[{"token_1": "0xc845b2894dbddd03858fd2d643b4ef725fe0849d0a0a",
                                 "token_2": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"}]"#
                    .to_string(),
                ..Entry::default() },
        EntryRejection::MalformedPair
    )]
    #[case::account_address(
        Entry { state_override:
                    r#"{"0xba443ecec71dc7123bd68c2082d13e3299cd": {"nonce": "0x1"}}"#.to_string(),
                ..Entry::default() },
        EntryRejection::NotAnAccount
    )]
    #[case::storage_slot(
        Entry { state_override: format!(
                    r#"{{"{TAURUSFI_ACCOUNT}": {{"stateDiff": {{"0x8dc4adfc": "0x{}"}}}}}}"#,
                    "ab".repeat(32)),
                ..Entry::default() },
        EntryRejection::MalformedStorage
    )]
    #[case::stored_word(
        Entry { state_override: format!(
                    r#"{{"{TAURUSFI_ACCOUNT}": {{"stateDiff": {{"0x{}": "0xabab"}}}}}}"#,
                    "ab".repeat(32)),
                ..Entry::default() },
        EntryRejection::MalformedStorage
    )]
    fn a_malformed_identity_rejects_the_entry(
        #[case] entry: Entry,
        #[case] expected: EntryRejection,
    ) {
        assert_eq!(entry.rejection(), expected);
    }

    /// An entry with nothing to match, or nothing to simulate, is not a usable snapshot.
    #[rstest]
    #[case::no_pairs(
        "[]",
        "[]",
        r#"{"0xba443ecec71dc7123bd68c2082d13e3299cde896": {"nonce": "0x1"}}"#
    )]
    #[case::no_accounts(
        r#"[{"token_1": "0xc845b2894dbddd03858fd2d643b4ef725fe0849d",
             "token_2": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"}]"#,
        "[283851543383726791766048793]",
        "{}"
    )]
    fn an_entry_without_data_is_rejected(
        #[case] asset_pairs: &str,
        #[case] pair_sequences: &str,
        #[case] state_override: &str,
    ) {
        let entry = Entry {
            asset_pairs: asset_pairs.to_string(),
            pair_sequences: pair_sequences.to_string(),
            state_override: state_override.to_string(),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::NoData);
    }

    /// A frame can carry no venue at all, which is not an error: the socket simply priced nothing
    /// this round.
    #[test]
    fn a_frame_without_venues_yields_no_outcome() {
        assert!(validate(parsed(r#"{"venues": []}"#)).is_empty());
    }

    /// `timestamp` is the only thing that orders one build of a venue's snapshot against another,
    /// so an entry without it cannot be used at all — the local clock would date the snapshot to
    /// when the frame was read instead.
    #[test]
    fn an_unstamped_entry_is_rejected() {
        let null_stamp = Entry { timestamp: "null".to_string(), ..Entry::default() };
        assert_eq!(null_stamp.rejection(), EntryRejection::Unstamped);

        let no_stamp = format!(
            r#"{{"venues": [{{
                "quoteAddress": "{TAURUSFI}",
                "blockNumber": 26148760,
                "slot": 15387623,
                "assetPairs": [{{"token_1": "{NVDAX}", "token_2": "{USDC}"}}],
                "pairSequences": [283851543383726791766048793],
                "stateOverride": {{"{TAURUSFI_ACCOUNT}": {{"nonce": "0x1"}}}}
            }}]}}"#
        );
        assert_eq!(
            validate(parsed(&no_stamp)),
            vec![EntryOutcome::Rejected(EntryRejection::Unstamped)]
        );
    }

    /// The block timestamp is derived from the beacon slot and from nothing else. An entry
    /// without a slot leaves the block's own timestamp in place, and a slot that dates the block
    /// more than a slot away from when the entry was built is discarded rather than trusted —
    /// never replaced by a local wall clock.
    #[rstest]
    #[case::captured_slot("15387623", BlockTimestamp::Derived(TAURUSFI_BLOCK_SECOND))]
    #[case::no_slot("null", BlockTimestamp::Unslotted)]
    #[case::slot_being_built_in("15387622", BlockTimestamp::Derived(1_791_475_487))]
    #[case::two_slots_ahead("15387624", BlockTimestamp::Implausible)]
    #[case::another_chains_slot("1", BlockTimestamp::Implausible)]
    #[case::overflowing("18446744073709551615", BlockTimestamp::Implausible)]
    fn the_block_timestamp_comes_only_from_the_slot(
        #[case] slot: &str,
        #[case] expected: BlockTimestamp,
    ) {
        let entry = Entry { slot: slot.to_string(), ..Entry::default() }.validated();

        assert_eq!(entry.block_timestamp, expected);
        assert_eq!(
            entry
                .snapshot(TAURUSFI_EXPIRES_AT)
                .block_timestamp,
            expected.seconds()
        );
    }

    /// Fields the stream carries that nothing here reads, and fields it may add later, are
    /// tolerated rather than rejected.
    #[test]
    fn unread_and_unknown_fields_are_tolerated() {
        let frame = format!(
            r#"{{"venues": [{{
                "quoteAddress": "{TAURUSFI}",
                "pricedAddresses": ["{TAURUSFI}", "{USDC}"],
                "somethingNew": {{"nested": [1, 2, 3]}},
                "blockNumber": 26148760,
                "slot": 15387623,
                "timestamp": 1791475494775213874,
                "assetPairs": [{{"token_1": "{NVDAX}", "token_2": "{USDC}", "extra": true}}],
                "pairSequences": [283851543383726791766048793],
                "stateOverride": {{"{TAURUSFI_ACCOUNT}": {{"nonce": "0x1", "extra": null}}}}
            }}]}}"#
        );

        let mut outcomes = validate(parsed(&frame));
        assert_eq!(outcomes.len(), 1);
        match outcomes.remove(0) {
            EntryOutcome::Accepted(entry) => assert_eq!(entry.pairs.len(), 1),
            EntryOutcome::Rejected(reason) => panic!("rejected: {reason:?}"),
        }
    }

    /// Every book of the captured price frame names the pair version it was priced at, and every
    /// one of those numbers overflows `u64`.
    #[test]
    fn decodes_the_captured_pair_versions() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        assert_eq!(
            message.books[0].calculated_from,
            Provenance::Valid(283_851_543_383_726_791_766_048_823)
        );
        for book in &message.books {
            match book.calculated_from {
                Provenance::Valid(sequence) => {
                    assert!(sequence > u128::from(u64::MAX), "{sequence} fits a u64");
                }
                Provenance::Absent | Provenance::Invalid => {
                    panic!("a captured book carried no readable reference: {book:?}")
                }
            }
        }
    }

    #[test]
    fn a_book_without_a_reference_decodes_as_absent() {
        let json = format!(
            r#"{{"quoteAddress": "{PAMM}", "token1": "WBTC", "token2": "USDC",
                 "asks": [], "bids": []}}"#
        );

        let book: TitanBook = serde_json::from_str(&json).expect("valid book");
        assert_eq!(book.calculated_from, Provenance::Absent);
    }

    /// The captured multi-book frame with book `index`'s `calculated_from` value replaced by the
    /// raw `literal` — synthetic from that one value on, every other book byte-identical to the
    /// capture.
    ///
    /// The splice is textual on purpose: routing the frame through a `serde_json::Value` would
    /// quantize every other book's reference to an `f64`, since a `Value` holds no integer wider
    /// than `u64`.
    fn frame_with_mutated_reference(index: usize, literal: &str) -> String {
        const KEY: &str = r#""calculated_from""#;
        let references: Vec<usize> = CAPTURED_MESSAGE
            .match_indices(KEY)
            .map(|(at, _)| at)
            .collect();
        let key_at = references[index];
        let value_at = key_at +
            CAPTURED_MESSAGE[key_at..]
                .find(':')
                .expect("the reference has a value") +
            1;
        let value_end = value_at +
            CAPTURED_MESSAGE[value_at..]
                .find(',')
                .expect("a field follows the reference");
        format!("{}{literal}{}", &CAPTURED_MESSAGE[..value_at], &CAPTURED_MESSAGE[value_end..])
    }

    /// One book's unreadable reference costs that one reference and nothing else: not the book,
    /// not its ladders, not a sibling book, not the frame. A frame carries every venue's
    /// ladders, so rejecting it over optional provenance metadata would be a quoting liveness
    /// hazard.
    #[rstest]
    #[case::null("null", Provenance::Absent)]
    #[case::string(r#""283851543383726791766048878""#, Provenance::Invalid)]
    #[case::negative("-1", Provenance::Invalid)]
    #[case::fraction("1.5", Provenance::Invalid)]
    #[case::boolean("true", Provenance::Invalid)]
    #[case::object("{}", Provenance::Invalid)]
    #[case::above_u128("3402823669209384634633746074317682114560", Provenance::Invalid)]
    fn a_malformed_reference_costs_only_that_reference(
        #[case] literal: &str,
        #[case] expected: Provenance,
    ) {
        const MUTATED: usize = 3;
        let captured: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        let text = frame_with_mutated_reference(MUTATED, literal);
        let message: TitanPriceLevelMessage =
            serde_json::from_str(&text).expect("the frame still parses");

        assert_eq!(message.books.len(), captured.books.len());
        assert_eq!(message.ts, captured.ts);
        for (index, (book, original)) in message
            .books
            .iter()
            .zip(&captured.books)
            .enumerate()
        {
            assert_eq!(book.quote_address, original.quote_address);
            assert_eq!(book.token1, original.token1);
            assert_eq!(book.asks.len(), original.asks.len());
            assert_eq!(book.bids.len(), original.bids.len());
            let expected = if index == MUTATED { expected } else { original.calculated_from };
            assert_eq!(book.calculated_from, expected, "book {index}");
        }
    }

    /// Leniency stops at the JSON: text that is not a frame is still rejected, as is a frame
    /// whose ladders are unreadable.
    ///
    /// The last case is where the frame rather than the side is the unit: a level's `price` is
    /// read as an `f64`, so a number the stream may legally write but that type cannot hold fails
    /// the whole frame, before any side exists to drop on its own. The side isolation the solver
    /// page promises begins once a frame has typed, which is what that page now says.
    #[rstest]
    #[case::truncated(r#"{"block": 1, "ts": 2, "books": ["#)]
    #[case::not_json("nonsense")]
    #[case::unreadable_ladder(
        r#"{"block": 1, "ts": 2, "books": [{"quoteAddress": "0x01", "token1": "A",
             "token2": "B", "asks": "nope", "bids": []}]}"#
    )]
    #[case::price_beyond_f64(
        r#"{"block": 1, "ts": 2, "books": [{"quoteAddress": "0x01", "token1": "A",
             "token2": "B", "asks": [{"price": 1e400, "cumVolume": 1.0}], "bids": []}]}"#
    )]
    fn invalid_json_still_rejects_the_message(#[case] text: &str) {
        assert!(serde_json::from_str::<TitanPriceLevelMessage>(text).is_err());
    }
}
