//! Titan pAMM stream formats and connection handling.
//!
//! Titan serves two pAMM WebSockets, both read by [`messages`]:
//!
//! - `pamm_price_levels_v2` (see <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>)
//!   yields the books Titan simulated, targeting the block it is currently building. Consumers
//!   decide how fresh a component is from the message's `ts`, not from what a message omits.
//! - `pamm_quote_stream_v2` yields, per venue, the account state Titan simulated that venue on to
//!   produce those books, together with the quote sequence of each pair that state carries. A book
//!   names its quote sequence in `calculated_from`, and the tracker matches each book to its
//!   account state by it.
//!
//! All Titan specifics (endpoints, JSON shapes, reconnect policy, and the checks a venue entry must
//! pass) live in this module; the rest of the price level stream machinery is venue-agnostic.

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
    SLOT,
};

/// Default Titan pAMM price level WebSocket endpoint. Titan serves the same stream from other
/// regions as well; see <https://docs.titanbuilder.xyz/propamms/takers>.
pub(super) const TITAN_PRICE_LEVEL_URL: &str =
    "wss://eu.data.titanbuilder.xyz/ws/pamm_price_levels_v2";

/// Environment variable that replaces [`TITAN_PRICE_LEVEL_URL`] when the builder is given no
/// endpoint.
pub(super) const TITAN_PRICE_LEVEL_URL_ENV: &str = "TITAN_PAMM_PRICE_LEVEL_URL";

/// Default Titan pAMM quote stream WebSocket endpoint, which carries the account state Titan priced
/// the books on; same region as [`TITAN_PRICE_LEVEL_URL`].
pub(super) const TITAN_QUOTE_STREAM_URL: &str =
    "wss://eu.data.titanbuilder.xyz/ws/pamm_quote_stream_v2";

/// Environment variable that replaces [`TITAN_QUOTE_STREAM_URL`] when the builder's
/// `quote_stream_endpoint` is not set.
pub(super) const TITAN_QUOTE_STREAM_URL_ENV: &str = "TITAN_PAMM_QUOTE_STREAM_URL";

/// The Unix second beacon slot 0 of Ethereum mainnet began at. A slot's second is
/// `BEACON_GENESIS + slot · 12`.
const BEACON_GENESIS: u64 = 1_606_824_023;

/// The width of an address in bytes; every address in both of Titan's messages has it.
const ADDRESS_BYTES: usize = 20;

/// A 32-byte EVM word, the width of every storage slot and stored value.
const WORD_BYTES: usize = 32;

/// How many accounts one venue entry may override before it is refused whole.
const MAX_ACCOUNTS: usize = 32;

/// How many storage words one venue entry may override across all its accounts.
const MAX_SLOTS: usize = 512;

/// Connection tuning for the Titan WebSocket, set through the
/// [`PriceLevelStreamBuilder`](super::stream::PriceLevelStreamBuilder).
#[derive(Clone, Copy, Debug)]
pub(super) struct ConnectionSettings {
    /// Longest a single connection attempt may take before it is aborted and retried, so a hung
    /// TCP/TLS handshake cannot block the stream forever.
    pub connect_timeout: Duration,
    /// Longest gap between *parsed* Titan messages tolerated before the socket is treated as dead
    /// and re-established. Titan pushes one message per second and sends no keepalives, so a
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

/// A message of one of Titan's two pAMM sockets.
///
/// The socket label is a property of the message type rather than of the caller, so the one reader
/// both sockets share cannot attribute one socket's traffic to the other. It is a fixed string,
/// never anything the stream carries.
pub(super) trait TitanMessage: DeserializeOwned + Send {
    const SOCKET: &'static str;
}

/// A parsed price level stream message: every book Titan simulated, targeting the block currently
/// being built.
///
/// Books are quoted in human units, so a level is a decimal price and a cumulative volume rather
/// than a pair of token amounts, and tokens arrive as addresses, with symbols alongside them as
/// display labels: a consumer resolves the addresses against its own token metadata before it can
/// price anything.
///
/// Messages are best effort, and none lists every venue: a venue or pair can be absent from one
/// message and present in the next (observed on 7.7% of messages in a 15 minute capture), so
/// absence must never be read as retirement.
#[derive(Debug, Deserialize)]
pub(super) struct TitanPriceLevelMessage {
    /// The L1 block number the quotes target (the block currently being built).
    pub block: u64,
    /// The beacon slot the message was built for, when Titan reports one. No freshness decision
    /// rests on it, so a message without it is still usable.
    pub slot: Option<u64>,
    /// When Titan built this message, in nanoseconds since the Unix epoch. Titan can send several
    /// messages with one `ts`, so it is a freshness marker, not an identity.
    pub ts: u64,
    /// One book per venue and pair; a venue appears once for every pair it quotes.
    pub books: Vec<TitanBook>,
}

impl TitanMessage for TitanPriceLevelMessage {
    const SOCKET: &'static str = "price_levels";
}

/// The quote sequence a book names in `calculated_from`: the one its price levels were priced at.
///
/// The parser tells the three cases apart because nothing downstream can: an absent quote sequence
/// and an unreadable one both leave nothing to match, but only an unreadable one means Titan sent
/// something this parser does not understand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum QuoteSequence {
    /// The book named none: the field was missing or null.
    #[default]
    Absent,
    /// The book was priced at this quote sequence.
    Valid(u128),
    /// The book named something this parser cannot read as a quote sequence: a string, a
    /// boolean, a fraction, a negative, a number above `u128::MAX`, or a composite. The book is
    /// still served: its price levels quote without it, and the quote sequence only finds their
    /// simulation overrides. Counted under `unreadable_quote_sequence_total`.
    Unreadable,
}

/// Reads `calculated_from` as a [`QuoteSequence`] without ever failing the message.
///
/// A plain `Option<u128>` would reject the whole message, and every venue's quotes in it, over
/// one unreadable value on one book. Leniency stops at the JSON itself: text that is not JSON
/// still rejects the message.
fn lenient_sequence<'de, D>(deserializer: D) -> Result<QuoteSequence, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    let text = raw.get();
    if text.trim() == "null" {
        return Ok(QuoteSequence::Absent);
    }
    Ok(serde_json::from_str::<u128>(text).map_or(QuoteSequence::Unreadable, QuoteSequence::Valid))
}

/// One venue's two-sided price levels for one pair, priced in `token2` per `token1`.
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
    /// Base token symbol, the unit of `cum_volume`. A display label: the pair is identified by
    /// [`token1_address`](Self::token1_address), never by this, so nothing resolves through it.
    /// It is read for the DEBUG logs that accompany a skipped book.
    pub token1: String,
    /// Quote token symbol, the unit of `price`. A display label, like
    /// [`token1`](Self::token1).
    pub token2: String,
    /// The address of the base token — the asset `cum_volume` is measured in.
    pub token1_address: Bytes,
    /// The address of the quote token — the asset `price` is measured in.
    pub token2_address: Bytes,
    /// The quote sequence this book was priced at, which the quote stream names per pair. Read
    /// leniently, and used for nothing but finding the book's simulation overrides: see
    /// [`QuoteSequence`].
    #[serde(rename = "calculated_from", default, deserialize_with = "lenient_sequence")]
    pub calculated_from: QuoteSequence,
    pub asks: Vec<TitanLevel>,
    pub bids: Vec<TitanLevel>,
}

/// A single quote level: selling `cum_volume` of `token1` in total executes at an average of
/// `price`.
///
/// An average, not a limit: `quotes_for_book` computes `amount_out = price · cum_volume`, which
/// holds only under that reading.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanLevel {
    /// `token2` per `token1`, in human units — both tokens' decimals are already applied.
    pub price: f64,
    /// Volume cumulated from the top of the book, in whole `token1`. Non-decreasing down a side,
    /// but consecutive levels can repeat a volume.
    pub cum_volume: f64,
}

/// A parsed quote stream message: the account state of every venue Titan priced, one
/// entry per venue.
///
/// Each entry is kept as raw JSON and decoded on its own by [`validate`], so an entry this parser
/// cannot read costs that entry and not the rest of the message. The message carries no timestamp
/// of its own, and like price level messages it is best effort: a venue present in one message can
/// be absent from the next without having retired.
#[derive(Debug, Deserialize)]
pub(super) struct TitanQuoteStreamMessage {
    pub venues: Vec<Box<RawValue>>,
}

impl TitanMessage for TitanQuoteStreamMessage {
    const SOCKET: &'static str = "quote_stream";
}

/// One venue's entry: the accounts Titan overrode to price the venue, and the quote sequence of
/// each pair that state carries.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanVenueOverride {
    /// The venue address this entry's state belongs to — the same address the price level
    /// stream's books carry, and not necessarily the venue's executable address.
    pub quote_address: Bytes,
    /// The quote address plus every venue whose swaps read this entry's state, per Titan's docs.
    #[serde(default)]
    pub priced_addresses: Vec<Bytes>,
    /// The L1 block Titan was building, when it names one.
    pub block_number: Option<u64>,
    /// The beacon slot of that block: the only source of its timestamp.
    pub slot: Option<u64>,
    /// When Titan took this state, in nanoseconds since the Unix epoch.
    pub timestamp: Option<u64>,
    /// The pairs the entry's quote sequences belong to, by position.
    #[serde(default)]
    pub asset_pairs: Vec<TitanAssetPair>,
    /// The quote sequence of each [`Self::asset_pairs`] entry, in the same order. Several pairs of
    /// one entry can share a quote sequence: Titan prices a batch of pairs at once.
    #[serde(default)]
    pub pair_sequences: Vec<u128>,
    /// The accounts the simulation overrode, keyed by address.
    #[serde(default)]
    pub state_override: BTreeMap<Bytes, TitanAccountOverride>,
}

/// One pair of a venue entry. Unlike the rest of the message, the stream names these two fields
/// in snake case.
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
    /// The overridden storage words, keyed by slot: a `stateDiff`, never a full `state`
    /// replacement.
    #[serde(default)]
    pub state_diff: BTreeMap<Bytes, Bytes>,
    /// Set when the entry replaced the account's code. Only its presence is read: see
    /// [`EntryRejection::UnsupportedOverride`].
    pub code: Option<IgnoredAny>,
    /// Set when the entry replaced the account's whole storage instead of diffing it. Read
    /// exactly like [`Self::code`].
    pub state: Option<IgnoredAny>,
}

/// The validated contents of one venue entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VenueOverrides {
    /// The venue address the entry was streamed under, exactly as Titan named it: the buffer
    /// normalizes it through the alias table, as the tracker does a book's.
    pub quote_address: Bytes,
    /// The quote address plus every venue whose swaps read this entry's state, exactly as Titan
    /// named them.
    pub priced_addresses: Vec<Bytes>,
    /// The quote sequence of each pair, in Titan's order and Titan's orientation.
    pub pairs: Vec<PairSequence>,
    /// The beacon slot of the block Titan was building, when the entry named one.
    pub slot: Option<u64>,
    /// The entry's simulation overrides, as a state carries them.
    pub overrides: PammSimulationOverrides,
}

/// One pair of a venue entry and the quote sequence its state carries for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PairSequence {
    pub token_1: Bytes,
    pub token_2: Bytes,
    /// The number a book of the price level stream names in its `calculated_from`.
    pub sequence: u128,
}

/// Why a venue entry was refused whole.
///
/// An entry is never partly used: the accounts of one entry were simulated together, so half of
/// them describe a state no venue was ever in. Each variant's name in snake case is the `reason`
/// label it is counted under.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum_macros::IntoStaticStr, strum_macros::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub(super) enum EntryRejection {
    /// The entry is not JSON of the documented shape: a field of the wrong type, or a value that
    /// does not fit its field.
    Undecodable,
    /// The entry's `quoteAddress`, or an address in its `pricedAddresses`, is not 20 bytes.
    NotAVenue,
    /// An overridden account is not keyed by a 20-byte address.
    NotAnAccount,
    /// A pair names something that is not a 20-byte address.
    MalformedPair,
    /// A storage slot or a stored word is not a 32-byte EVM word.
    MalformedStorage,
    /// A balance or a nonce is not a hex quantity, a balance is wider than 32 bytes, or a nonce
    /// does not fit the field that carries it.
    MalformedQuantity,
    /// The entry named no `timestamp`, so nothing orders it against another entry of the same
    /// quote sequence.
    NoTimestamp,
    /// `assetPairs` and `pairSequences` have different lengths, so which quote sequence belongs to
    /// which pair is unknown.
    PairsAndSequencesDiffer,
    /// The entry carried no pair, or no account: nothing to match, or nothing to simulate.
    ///
    /// **This refusal has a permanent baseline and is not a fault.** About 1.2% of Titan's entries
    /// name no pair and no account, roughly one a second, measured over 77,377 entries on
    /// 2026-10-21. Alerts on refused entries must exclude this reason.
    Empty,
    /// An account replaced its code, or its whole storage instead of diffing it, so the entry no
    /// longer describes the venue the chain holds.
    UnsupportedOverride,
    /// The entry overrides more than [`MAX_ACCOUNTS`] accounts or [`MAX_SLOTS`] storage words.
    OverCap,
}

/// Validates every venue entry of `message`, in stream order: one result per entry.
pub(super) fn validate(
    message: TitanQuoteStreamMessage,
) -> Vec<Result<VenueOverrides, EntryRejection>> {
    message
        .venues
        .into_iter()
        .map(|raw| {
            serde_json::from_str::<TitanVenueOverride>(raw.get())
                .map_err(|_| EntryRejection::Undecodable)
                .and_then(validated)
        })
        .collect()
}

/// The entry's contents, or the first reason it cannot be used.
fn validated(entry: TitanVenueOverride) -> Result<VenueOverrides, EntryRejection> {
    if entry.quote_address.len() != ADDRESS_BYTES ||
        entry
            .priced_addresses
            .iter()
            .any(|address| address.len() != ADDRESS_BYTES)
    {
        return Err(EntryRejection::NotAVenue);
    }
    let Some(built_at_ns) = entry.timestamp else {
        return Err(EntryRejection::NoTimestamp);
    };
    if entry.asset_pairs.len() != entry.pair_sequences.len() {
        return Err(EntryRejection::PairsAndSequencesDiffer);
    }
    if entry.asset_pairs.is_empty() || entry.state_override.is_empty() {
        return Err(EntryRejection::Empty);
    }
    ensure_within_caps(&entry.state_override)?;

    Ok(VenueOverrides {
        quote_address: entry.quote_address,
        priced_addresses: entry.priced_addresses,
        pairs: pair_sequences(entry.asset_pairs, &entry.pair_sequences)?,
        slot: entry.slot,
        overrides: PammSimulationOverrides {
            accounts: validated_accounts(entry.state_override)?,
            block_number: entry.block_number,
            block_timestamp: block_timestamp(entry.slot),
            built_at_ns,
        },
    })
}

/// Errors unless the entry fits every per-entry cap.
///
/// It counts accounts and storage words before any width check, so an entry with wrong widths still
/// cannot make this process hold more than the caps allow.
fn ensure_within_caps(
    accounts: &BTreeMap<Bytes, TitanAccountOverride>,
) -> Result<(), EntryRejection> {
    if accounts.len() > MAX_ACCOUNTS {
        return Err(EntryRejection::OverCap);
    }
    let mut slots: usize = 0;
    for account in accounts.values() {
        slots = slots.saturating_add(account.state_diff.len());
    }
    if slots > MAX_SLOTS {
        return Err(EntryRejection::OverCap);
    }
    Ok(())
}

/// Pairs each asset pair with the quote sequence at its own position. The lengths are equal by
/// the time this runs.
fn pair_sequences(
    pairs: Vec<TitanAssetPair>,
    sequences: &[u128],
) -> Result<Vec<PairSequence>, EntryRejection> {
    let mut paired = Vec::with_capacity(pairs.len());
    for (pair, sequence) in pairs.into_iter().zip(sequences) {
        if pair.token_1.len() != ADDRESS_BYTES || pair.token_2.len() != ADDRESS_BYTES {
            return Err(EntryRejection::MalformedPair);
        }
        paired.push(PairSequence {
            token_1: pair.token_1,
            token_2: pair.token_2,
            sequence: *sequence,
        });
    }
    Ok(paired)
}

/// The overridden accounts with their quantities decoded and their widths checked.
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
/// A quantity drops leading zero digits, so it can have an odd number of digits, as
/// `0x31a98d578d71685db` does, which fixed-width hex parsing rejects. A zero quantity decodes to
/// one zero byte, never to no bytes.
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
/// A local wall clock would date the simulation to when the message was read rather than to the
/// block Titan built, so the slot is the only source. `None` when the entry named no slot, or when
/// the arithmetic does not fit.
fn block_timestamp(slot: Option<u64>) -> Option<u64> {
    slot?
        .checked_mul(SLOT.as_secs())?
        .checked_add(BEACON_GENESIS)
}

/// Yields parsed messages from `url` for as long as the returned stream is polled.
///
/// Maintains the connection in the background of the stream itself: any disconnect, read error,
/// server-side close, or idle timeout is retried forever with capped exponential backoff (reset
/// only once a message is actually parsed, so a socket that connects and immediately drops still
/// backs off instead of busy-looping). Malformed messages are logged and skipped; this stream never
/// terminates.
///
/// Both Titan sockets use this one function; the message type names the socket in its logs.
pub(super) fn messages<T: TitanMessage>(
    url: String,
    settings: ConnectionSettings,
) -> impl Stream<Item = T> + Send {
    stream! {
        let mut attempt: u32 = 0;
        loop {
            match timeout(settings.connect_timeout, connect_async(url.as_str())).await {
                Ok(Ok((mut ws_stream, _))) => {
                    info!(%url, socket = T::SOCKET, "Connected to Titan pAMM stream");
                    // The deadline moves only when a message parses, so control frames and
                    // unparsable text cannot keep a socket that sends no messages alive.
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
                            // No parsed message within the idle window: assume a stalled socket.
                            Err(_elapsed) => {
                                warn!(
                                    socket = T::SOCKET,
                                    idle_secs = settings.read_idle_timeout.as_secs(),
                                    "No parsed Titan message within idle timeout; reconnecting"
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
                                    // A parsed message proves the connection is healthy: reset the
                                    // reconnect backoff. The idle window restarts only once the
                                    // consumer resumes the stream after the yield, so it measures
                                    // the time spent waiting on the socket, not the time the
                                    // consumer spends between polls.
                                    Ok(message) => {
                                        attempt = 0;
                                        yield message;
                                        idle_deadline = deadline_after(settings.read_idle_timeout);
                                    }
                                    // Unparseable message: log and keep the connection.
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
                                    "Ignoring unexpected binary Titan message"
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
            telemetry::{
                recorded::{counter_value, record_async},
                FRAMES_REJECTED, RECONNECTS,
            },
            test_support::{
                address, captured_quote_stream_message, captured_quote_stream_text,
                captured_venue_overrides, message_text, message_then_repeat, test_overrides,
                wall_nanos_now, FakeTitan, PAMM, USDC, WBTC, WETH,
            },
        },
        *,
    };

    /// A message captured verbatim from the live stream (2026-10-16); the file name carries the
    /// message's `ts` field.
    const CAPTURED_MESSAGE: &str =
        include_str!("test_responses/pamm_price_levels_v2_1792115918290990905.json");

    /// The earlier quote stream message captured verbatim from the live `pamm_quote_stream_v2`
    /// socket (2026-10-16); [`captured_quote_stream_text`] is the later one. A quote stream message
    /// has no timestamp of its own, only one per venue entry, so each file name carries the
    /// nanosecond the capture received the message. Unlike the price level fixtures, nothing in
    /// the contents confirms that number.
    const EARLIER_QUOTE_STREAM_MESSAGE: &str =
        include_str!("test_responses/pamm_quote_stream_v2_1792115920051017671.json");

    /// The venue Metric streams under, whose entry prices all eight of its pairs at one quote
    /// sequence.
    const METRIC: &str = "0xe844701934d9c6085b292e428f41b911cc0b3872";
    /// The venue Goliath streams under, whose captured entry [`test_overrides`] transcribes by
    /// hand.
    const GOLIATH: &str = "0x4367985a63063fc970b009c6e599e9225181f551";
    /// An account whose balance and nonce the captured Metric entry overrides, and nothing else.
    const METRIC_ACCOUNT: &str = "0x629ef4396ec73973b5ea25c3f0cd1c45cd73948e";
    /// A token of the entry's fifth pair (`LINK`), which the defaults never bound.
    const LINK: &str = "0x514910771af9ca656af840dff83e8264ecf986ca";
    /// The Unix second the entry's beacon slot 15440992 begins at.
    const METRIC_BLOCK_SECOND: u64 = 1_792_115_927;

    /// Nanoseconds since the epoch for 2023-11 and 2033-05. A `ts` in seconds or milliseconds
    /// would fall far below the lower bound, so the window pins the scale rather than the date.
    const NANOS_LOWER_BOUND: u64 = 1_700_000_000_000_000_000;
    const NANOS_UPPER_BOUND: u64 = 2_000_000_000_000_000_000;

    #[test]
    fn parses_captured_live_message() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        assert_eq!(message.block, 26_201_953);
        assert_eq!(message.slot, Some(15_440_991));
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

        // A level without volume carries no depth, so a zero would mean the price levels lost their
        // quantities in parsing.
        for book in &message.books {
            for level in book.asks.iter().chain(&book.bids) {
                assert!(level.cum_volume > 0.0, "non-positive cumVolume: {level:?}");
                assert!(level.price > 0.0, "non-positive price: {level:?}");
            }
        }
    }

    /// `ts` is the only per-message freshness signal, so a message without it is unusable and must
    /// not parse. `slot` carries no freshness decision, so a message without it still parses.
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
    /// `calculated_from` is the quote sequence the book was priced at — a number that overflows
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
                "token1Address": "{WBTC}",
                "token2Address": "{USDC}",
                "calculated_from": 283851543383726791766048878,
                "asks": [{{"price": 80997.89405475458, "cumVolume": 0.00012346}}],
                "bids": []
            }}"#
        );

        let book: TitanBook = serde_json::from_str(&json).expect("valid book");
        assert_eq!(book.maker.as_deref(), expected);
        assert_eq!(book.quote_address, Bytes::from_str(PAMM).unwrap());
        assert_eq!(book.calculated_from, QuoteSequence::Valid(283_851_543_383_726_791_766_048_878));
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

    fn price_level_message() -> Message {
        Message::Text(message_text(100, wall_nanos_now()).into())
    }

    /// Polls `stream` until it yields a message, failing if none arrives within two seconds.
    async fn next_message(stream: &mut (impl Stream<Item = TitanPriceLevelMessage> + Unpin)) {
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("a message within two seconds")
            .expect("the stream never ends");
    }

    /// Traffic that is not a parsed message does not reset the idle timeout: the socket is
    /// dropped and reconnected even though it never went quiet.
    #[rstest]
    #[case::ping_only(Message::Ping(Vec::new().into()), false)]
    #[case::malformed_text(Message::Text("nonsense".into()), true)]
    fn traffic_that_is_not_a_message_does_not_count_as_liveness(
        #[case] filler: Message,
        #[case] rejected_as_parse_error: bool,
    ) {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(message_then_repeat(
                price_level_message(),
                filler,
                Duration::from_millis(10),
            ))
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_message(&mut stream).await;
            // The second message can only come from a new connection, which the idle timeout
            // opens once the filler fails to reset it.
            next_message(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert!(connections >= 2, "no reconnect on non-message traffic");
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
                if socket
                    .send(price_level_message())
                    .await
                    .is_err()
                {
                    return;
                }
                let _ = socket.send(Message::Close(None)).await;
            })
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_message(&mut stream).await;
            // The close frame follows the first message, so the second message can only come from a
            // new connection.
            next_message(&mut stream).await;
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
    /// that tells their failures apart: a quote stream socket that keeps closing must not read as
    /// the price level socket flapping, which is the signal that quoting liveness is at risk.
    #[test]
    fn a_reconnect_names_the_socket_it_happened_on() {
        let ((), snapshot) = record_async(async {
            let fake = FakeTitan::spawn(|_, mut socket| async move {
                let _ = socket.send(Message::Close(None)).await;
            })
            .await;
            let stream = messages::<TitanQuoteStreamMessage>(fake.url(), fast_settings());
            tokio::pin!(stream);
            // The fake sends no message, so the stream yields nothing; polling it drives the
            // connect, close and back-off loop.
            let _ = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        });

        assert!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "closed"), ("socket", "quote_stream")]
            ) >= 1,
            "the quote stream socket's reconnects are not counted under its own socket"
        );
        assert_eq!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "closed"), ("socket", "price_levels")]
            ),
            0,
            "the quote stream socket's reconnects were attributed to the price level socket"
        );
    }

    /// The idle timeout measures upstream silence, not the consumer's pace: a consumer that
    /// pauses for longer than the timeout finds the same connection when it resumes.
    #[test]
    fn slow_consumer_does_not_trigger_the_idle_timeout() {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(message_then_repeat(
                price_level_message(),
                price_level_message(),
                Duration::from_millis(20),
            ))
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_message(&mut stream).await;
            // Three idle timeouts pass without a poll.
            tokio::time::sleep(Duration::from_millis(300)).await;
            next_message(&mut stream).await;
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
    async fn parsed_messages_keep_the_connection_alive() {
        let fake = FakeTitan::spawn(message_then_repeat(
            price_level_message(),
            price_level_message(),
            Duration::from_millis(30),
        ))
        .await;
        let settings =
            ConnectionSettings { read_idle_timeout: Duration::from_millis(150), ..fast_settings() };
        let stream = messages(fake.url(), settings);
        tokio::pin!(stream);
        for _ in 0..10 {
            next_message(&mut stream).await;
        }
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    }

    /// The endpoint and the variable that replaces it are the deployment contract of the second
    /// socket, so they are spelled out rather than derived.
    #[test]
    fn the_quote_stream_endpoint_is_pinned() {
        assert_eq!(
            TITAN_QUOTE_STREAM_URL,
            "wss://eu.data.titanbuilder.xyz/ws/pamm_quote_stream_v2"
        );
        assert_eq!(TITAN_QUOTE_STREAM_URL_ENV, "TITAN_PAMM_QUOTE_STREAM_URL");
    }

    /// One reader serves both sockets: the quote stream socket is read, reconnected and parsed by
    /// exactly the machinery the price level socket is.
    #[tokio::test]
    async fn the_reader_serves_the_quote_stream_socket() {
        let fake = FakeTitan::spawn(message_then_repeat(
            Message::Text(captured_quote_stream_text().into()),
            Message::Ping(Vec::new().into()),
            Duration::from_millis(50),
        ))
        .await;
        let stream = messages::<TitanQuoteStreamMessage>(fake.url(), fast_settings());
        tokio::pin!(stream);
        let message = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("a message within two seconds")
            .expect("the stream never ends");
        assert_eq!(message.venues.len(), 9);
    }

    fn parsed(message: &str) -> TitanQuoteStreamMessage {
        serde_json::from_str(message).expect("a valid quote stream message")
    }

    /// Every venue entry of `message`, decoded on its own as [`validate`] decodes it.
    fn decoded(message: &str) -> Vec<TitanVenueOverride> {
        parsed(message)
            .venues
            .iter()
            .map(|raw| serde_json::from_str(raw.get()).expect("a decodable venue entry"))
            .collect()
    }

    #[test]
    fn parses_the_captured_quote_stream_messages() {
        let venues = decoded(captured_quote_stream_text());
        let earlier = decoded(EARLIER_QUOTE_STREAM_MESSAGE);

        assert_eq!(venues.len(), 9);
        assert_eq!(earlier.len(), 9);

        let metric = venues
            .iter()
            .find(|venue| venue.quote_address == address(METRIC))
            .expect("the Metric entry");
        // Metric names its router, among other venues, as reading this entry's state.
        assert!(metric
            .priced_addresses
            .contains(&address("0x99ab7c1c5eab47801f7937075998527a550fba8a")));
        assert_eq!(metric.block_number, Some(26_201_954));
        assert_eq!(metric.slot, Some(15_440_992));
        assert_eq!(metric.timestamp, Some(1_792_115_920_126_438_314));
        // The captured quote sequences verbatim: every one above `u64::MAX`, which a narrower
        // field could not hold, and all eight of them equal.
        assert_eq!(metric.pair_sequences, vec![284_836_027_668_196_596_826_243_146; 8]);
        assert!(metric
            .pair_sequences
            .iter()
            .all(|sequence| *sequence > u128::from(u64::MAX)));
        assert_eq!(metric.asset_pairs.len(), 8);
        assert_eq!(metric.asset_pairs[0].token_1, address(WETH));
        assert_eq!(metric.asset_pairs[0].token_2, address(USDC));
        // The fifth pair names LINK, an asset no symbol table ever bound.
        assert_eq!(metric.asset_pairs[4].token_1, address(LINK));
        assert_eq!(metric.state_override.len(), 2);
    }

    /// The captured Goliath entry validates into exactly the overrides `test_support` transcribes
    /// from the same message by hand: the hex quantities (the odd-digit and the zero one
    /// included), the 32-byte storage words, the block the entry names, the slot-derived block
    /// timestamp and the timestamp.
    #[test]
    fn a_captured_entry_validates_into_the_transcribed_overrides() {
        let entries = captured_venue_overrides(captured_quote_stream_message());

        let goliath = entries
            .into_iter()
            .find(|entry| entry.quote_address == address(GOLIATH))
            .expect("the Goliath entry");
        assert_eq!(goliath.overrides, test_overrides());
    }

    /// Live traffic must pass validation whole: every entry that carries data, in both captured
    /// messages, with its block dated from its beacon slot. One entry of the later message carries
    /// neither a pair nor an account, which is why the two counts differ by one.
    #[rstest]
    #[case::later(captured_quote_stream_text(), 8)]
    #[case::earlier(EARLIER_QUOTE_STREAM_MESSAGE, 9)]
    fn every_captured_entry_carrying_data_validates(
        #[case] message: &str,
        #[case] accepted_entries: usize,
    ) {
        let entries = captured_venue_overrides(parsed(message));

        assert_eq!(entries.len(), accepted_entries);
        for entry in entries {
            assert_eq!(entry.overrides.block_number, Some(26_201_954));
            assert_eq!(entry.overrides.block_timestamp, Some(METRIC_BLOCK_SECOND));
            assert_eq!(entry.slot, Some(15_440_992));
        }
    }

    /// Each pair keeps the quote sequence at its own position in `pairSequences`.
    #[test]
    fn each_pair_keeps_the_quote_sequence_at_its_position() {
        let entry = Entry {
            asset_pairs: format!(
                r#"[{{"token_1": "{LINK}", "token_2": "{USDC}"}},
                    {{"token_1": "{WETH}", "token_2": "{USDC}"}}]"#
            ),
            pair_sequences: "[284836027668196596826243146, 284836027668196596826243150]"
                .to_string(),
            ..Entry::default()
        };

        let pairs = entry.validated().pairs;

        assert_eq!(pairs[0].token_1, address(LINK));
        assert_eq!(pairs[0].sequence, 284_836_027_668_196_596_826_243_146);
        assert_eq!(pairs[1].token_1, address(WETH));
        assert_eq!(pairs[1].sequence, 284_836_027_668_196_596_826_243_150);
    }

    /// A synthetic entry shaped like Titan's, built from fields of the captured Metric entry, as
    /// raw JSON, so a case can replace exactly one field.
    struct Entry {
        quote_address: String,
        block_number: String,
        slot: String,
        timestamp: String,
        asset_pairs: String,
        pair_sequences: String,
        state_override: String,
    }

    impl Default for Entry {
        fn default() -> Self {
            Self {
                quote_address: format!(r#""{METRIC}""#),
                block_number: "26148760".to_string(),
                slot: "15440992".to_string(),
                timestamp: "1792115920126438314".to_string(),
                asset_pairs: format!(r#"[{{"token_1": "{LINK}", "token_2": "{USDC}"}}]"#),
                pair_sequences: "[284836027668196596826243146]".to_string(),
                state_override: format!(
                    r#"{{"{METRIC_ACCOUNT}": {{"balance": "0x2a60f7239c6aedc", "nonce": "0x1"}}}}"#
                ),
            }
        }
    }

    impl Entry {
        /// A one-entry quote stream message carrying these fields.
        fn message(&self) -> String {
            let Self {
                quote_address,
                block_number,
                slot,
                timestamp,
                asset_pairs,
                pair_sequences,
                state_override,
            } = self;
            format!(
                r#"{{"venues": [{{
                    "maker": "Metric",
                    "quoteAddress": {quote_address},
                    "pricedAddresses": ["{METRIC}"],
                    "blockNumber": {block_number},
                    "slot": {slot},
                    "timestamp": {timestamp},
                    "assetPairs": {asset_pairs},
                    "pairSequences": {pair_sequences},
                    "stateOverride": {state_override}
                }}]}}"#
            )
        }

        /// The result of validating this entry, which the message carries as its only one.
        fn outcome(&self) -> Result<VenueOverrides, EntryRejection> {
            let mut outcomes = validate(parsed(&self.message()));
            assert_eq!(outcomes.len(), 1, "expected exactly one venue entry");
            outcomes.remove(0)
        }

        fn rejection(&self) -> EntryRejection {
            self.outcome()
                .expect_err("the entry was accepted")
        }

        fn validated(&self) -> VenueOverrides {
            self.outcome()
                .expect("the entry was refused")
        }
    }

    /// `Entry::default()` is accepted, so each rejection below comes from the one field its case
    /// replaces.
    #[test]
    fn the_synthetic_entry_is_accepted_unmutated() {
        let entry = Entry::default().validated();

        assert_eq!(entry.quote_address, address(METRIC));
        assert_eq!(entry.pairs.len(), 1);
        assert_eq!(entry.pairs[0].sequence, 284_836_027_668_196_596_826_243_146);
        assert_eq!(entry.overrides.accounts.len(), 1);
        assert_eq!(entry.overrides.built_at_ns, 1_792_115_920_126_438_314);
    }

    /// An entry naming no block number, or no slot, is still accepted: the overrides then carry
    /// no block number, or no block timestamp.
    #[rstest]
    #[case::no_block_number(
        Entry { block_number: "null".to_string(), ..Entry::default() },
        None,
        Some(1_792_115_927)
    )]
    #[case::no_slot(
        Entry { slot: "null".to_string(), ..Entry::default() },
        Some(26_148_760),
        None
    )]
    fn an_entry_without_a_block_number_or_slot_is_accepted(
        #[case] entry: Entry,
        #[case] block_number: Option<u64>,
        #[case] block_timestamp: Option<u64>,
    ) {
        let entry = entry.validated();

        assert_eq!(entry.overrides.block_number, block_number);
        assert_eq!(entry.overrides.block_timestamp, block_timestamp);
    }

    /// Sequences that do not pair 1:1 with the asset pairs leave it unknown which pair each
    /// sequence belongs to; zipping the shorter of the two would attach the entry to a quote
    /// sequence it was never priced at.
    #[rstest]
    #[case::fewer_sequences("[283851543383726791766048793]")]
    #[case::more_sequences(
        "[283851543383726791766048793, 283851543383726791766048835, 283851543383726791766048836]"
    )]
    #[case::no_sequences("[]")]
    fn a_sequence_length_mismatch_rejects_the_entry(#[case] pair_sequences: &str) {
        let entry = Entry {
            asset_pairs: format!(
                r#"[{{"token_1": "{LINK}", "token_2": "{USDC}"}},
                    {{"token_1": "{WETH}", "token_2": "{USDC}"}}]"#
            ),
            pair_sequences: pair_sequences.to_string(),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::PairsAndSequencesDiffer);
    }

    /// An entry that overrides code, or replaces an account's whole storage instead of diffing
    /// it, no longer describes the venue the chain holds. It is refused whole rather than
    /// stripped of the offending account: the accounts of one entry were simulated together.
    #[rstest]
    #[case::code(r#""code": "0x6080604052","#)]
    #[case::full_state(
        r#""state": {"0x8dc4adfc36586ffb9fe5c2dba18756239463dda9032bbfa7fd13f3bb3b16358f":
                    "0x6ac7bf2b01000000000001a11c42ae030000007600000076000000000003969f"},"#
    )]
    fn code_or_a_full_state_rejects_the_entry(#[case] unsupported: &str) {
        let entry = Entry {
            state_override: format!(
                r#"{{"{METRIC_ACCOUNT}": {{"balance": "0xb0618b2431873a", "nonce": "0x1"}},
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
                r#"{{"{METRIC_ACCOUNT}": {{{null_field} "balance": "0xb0618b2431873a",
                     "nonce": "0x1"}}}}"#
            ),
            ..Entry::default()
        };

        assert_eq!(
            entry
                .validated()
                .overrides
                .accounts
                .len(),
            1
        );
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

    /// An entry past any one of the per-entry caps is refused whole: what one message can make
    /// this process retain has to be bounded before anything in it is trusted.
    #[rstest]
    #[case::too_many_accounts(33, 0)]
    #[case::too_many_slots(2, 257)]
    fn an_over_cap_entry_is_rejected(#[case] accounts: usize, #[case] slots: usize) {
        let entry = Entry {
            state_override: synthetic_state_override(accounts, slots, WORD_BYTES),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::OverCap);
    }

    /// The caps are inclusive: the largest entry that fits them is accepted.
    #[test]
    fn an_entry_at_the_caps_is_accepted() {
        let entry =
            Entry { state_override: synthetic_state_override(32, 16, 32), ..Entry::default() }
                .validated();

        assert_eq!(entry.overrides.accounts.len(), 32);
        let slots: usize = entry
            .overrides
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
                r#"{{"{METRIC_ACCOUNT}": {{"balance": "{quantity}", "nonce": "0x2b2d"}}}}"#
            ),
            ..Entry::default()
        };

        let account = entry
            .validated()
            .overrides
            .accounts
            .remove(&address(METRIC_ACCOUNT))
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
            state_override: format!(r#"{{"{METRIC_ACCOUNT}": {{{quantity}}}}}"#),
            ..Entry::default()
        };

        assert_eq!(entry.rejection(), EntryRejection::MalformedQuantity);
    }

    /// The widest nonce the field holds is still a nonce.
    #[test]
    fn a_nonce_at_the_width_of_its_field_is_accepted() {
        let entry = Entry {
            state_override: format!(r#"{{"{METRIC_ACCOUNT}": {{"nonce": "0xffffffffffffffff"}}}}"#),
            ..Entry::default()
        };

        let account = entry
            .validated()
            .overrides
            .accounts
            .remove(&address(METRIC_ACCOUNT))
            .expect("the overridden account");
        assert_eq!(account.nonce, Some(u64::MAX));
        assert_eq!(account.balance, None);
    }

    /// Addresses are 20 bytes and storage slots and words are 32: an entry that disagrees is not
    /// the shape Titan documents, and half-read, it would simulate a state no venue was ever in.
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
                    r#"{{"{METRIC_ACCOUNT}": {{"stateDiff": {{"0x8dc4adfc": "0x{}"}}}}}}"#,
                    "ab".repeat(32)),
                ..Entry::default() },
        EntryRejection::MalformedStorage
    )]
    #[case::stored_word(
        Entry { state_override: format!(
                    r#"{{"{METRIC_ACCOUNT}": {{"stateDiff": {{"0x{}": "0xabab"}}}}}}"#,
                    "ab".repeat(32)),
                ..Entry::default() },
        EntryRejection::MalformedStorage
    )]
    fn a_malformed_address_or_word_rejects_the_entry(
        #[case] entry: Entry,
        #[case] expected: EntryRejection,
    ) {
        assert_eq!(entry.rejection(), expected);
    }

    /// An entry with no pair to match, or no account to simulate, is refused as `Empty`.
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

        assert_eq!(entry.rejection(), EntryRejection::Empty);
    }

    /// An entry this parser cannot decode is refused on its own, and the other entries of the
    /// message are still validated: one field of the wrong type must not cost every venue its
    /// overrides.
    #[rstest]
    #[case::block_number_as_text(r#""blockNumber": "26148760","#)]
    #[case::sequence_as_text(r#""pairSequences": ["283851543383726791766048793"],"#)]
    #[case::balance_as_number(
        r#""stateOverride": {"0xba443ecec71dc7123bd68c2082d13e3299cde896": {"balance": 1}},"#
    )]
    fn an_undecodable_entry_costs_only_itself(#[case] wrong_field: &str) {
        let undecodable = format!(
            r#"{{"quoteAddress": "{METRIC}", {wrong_field} "timestamp": 1791475494775213874}}"#
        );
        let good = Entry::default().message();
        let good_entry = good
            .trim_start_matches(r#"{"venues": ["#)
            .trim_end_matches("]}");
        let message = format!(r#"{{"venues": [{undecodable}, {good_entry}]}}"#);

        let outcomes = validate(parsed(&message));

        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0], Err(EntryRejection::Undecodable));
        assert!(outcomes[1].is_ok(), "{:?}", outcomes[1]);
    }

    /// A `pricedAddresses` address that is not 20 bytes refuses the entry, as a malformed
    /// `quoteAddress` does: the entry would otherwise be held under a venue that does not exist.
    #[test]
    fn a_malformed_priced_address_rejects_the_entry() {
        let message = Entry::default().message().replace(
            &format!(r#""pricedAddresses": ["{METRIC}"]"#),
            r#""pricedAddresses": ["0x99ab7c1c"]"#,
        );

        assert_eq!(validate(parsed(&message)), vec![Err(EntryRejection::NotAVenue)]);
    }

    /// A quote stream message can carry no venue entry at all, which is not an error: Titan priced
    /// no venue.
    #[test]
    fn a_message_without_venues_yields_no_result() {
        assert!(validate(parsed(r#"{"venues": []}"#)).is_empty());
    }

    /// `timestamp` is the only field that orders two entries of one quote sequence, so an entry
    /// without it is refused: the local clock would date the entry to when the stream read it.
    #[rstest]
    #[case::null(Entry { timestamp: "null".to_string(), ..Entry::default() }.message())]
    #[case::absent(format!(
        r#"{{"venues": [{{
            "quoteAddress": "{METRIC}",
            "blockNumber": 26148760,
            "slot": 15387623,
            "assetPairs": [{{"token_1": "{LINK}", "token_2": "{USDC}"}}],
            "pairSequences": [283851543383726791766048793],
            "stateOverride": {{"{METRIC_ACCOUNT}": {{"nonce": "0x1"}}}}
        }}]}}"#
    ))]
    fn an_entry_without_a_timestamp_is_rejected(#[case] message: String) {
        assert_eq!(validate(parsed(&message)), vec![Err(EntryRejection::NoTimestamp)]);
    }

    /// Fields the stream carries that nothing here reads, and fields it may add later, are
    /// tolerated rather than rejected.
    #[test]
    fn unread_and_unknown_fields_are_tolerated() {
        let message = format!(
            r#"{{"venues": [{{
                "quoteAddress": "{METRIC}",
                "pricedAddresses": ["{METRIC}", "{USDC}"],
                "somethingNew": {{"nested": [1, 2, 3]}},
                "blockNumber": 26148760,
                "slot": 15387623,
                "timestamp": 1791475494775213874,
                "assetPairs": [{{"token_1": "{LINK}", "token_2": "{USDC}", "extra": true}}],
                "pairSequences": [283851543383726791766048793],
                "stateOverride": {{"{METRIC_ACCOUNT}": {{"nonce": "0x1", "extra": null}}}}
            }}]}}"#
        );

        let mut outcomes = validate(parsed(&message));

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes
                .remove(0)
                .expect("the entry was refused")
                .pairs
                .len(),
            1
        );
    }

    /// Every book of the captured price level message names the quote sequence it was priced at,
    /// and every one of those numbers overflows `u64`.
    #[test]
    fn decodes_the_captured_quote_sequences() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        assert_eq!(
            message.books[0].calculated_from,
            QuoteSequence::Valid(284_836_009_221_452_523_116_691_695)
        );
        for book in &message.books {
            match book.calculated_from {
                QuoteSequence::Valid(sequence) => {
                    assert!(sequence > u128::from(u64::MAX), "{sequence} fits a u64");
                }
                QuoteSequence::Absent | QuoteSequence::Unreadable => {
                    panic!("a captured book carried no readable quote sequence: {book:?}")
                }
            }
        }
    }

    #[test]
    fn a_book_without_a_quote_sequence_decodes_as_absent() {
        let json = format!(
            r#"{{"quoteAddress": "{PAMM}", "token1": "WBTC", "token2": "USDC",
                 "token1Address": "{WBTC}", "token2Address": "{USDC}",
                 "asks": [], "bids": []}}"#
        );

        let book: TitanBook = serde_json::from_str(&json).expect("valid book");
        assert_eq!(book.calculated_from, QuoteSequence::Absent);
    }

    /// The captured multi-book message with book `index`'s `calculated_from` value replaced by the
    /// raw `literal`; every other byte matches the capture. It edits the text because a
    /// `serde_json::Value` holds no integer wider than `u64`, so a round trip would turn every
    /// other book's quote sequence into an `f64`.
    fn message_with_mutated_quote_sequence(index: usize, literal: &str) -> String {
        const KEY: &str = r#""calculated_from""#;
        let keys: Vec<usize> = CAPTURED_MESSAGE
            .match_indices(KEY)
            .map(|(at, _)| at)
            .collect();
        let key_at = keys[index];
        let value_at = key_at +
            CAPTURED_MESSAGE[key_at..]
                .find(':')
                .expect("the quote sequence has a value") +
            1;
        let value_end = value_at +
            CAPTURED_MESSAGE[value_at..]
                .find(',')
                .expect("a field follows the quote sequence");
        format!("{}{literal}{}", &CAPTURED_MESSAGE[..value_at], &CAPTURED_MESSAGE[value_end..])
    }

    /// One book's unreadable `calculated_from` costs that book its quote sequence and nothing
    /// else: not the book, its price levels, another book or the message. A message carries every
    /// venue's price levels, so rejecting it over a value that only finds overrides would stop
    /// every venue quoting.
    #[rstest]
    #[case::null("null", QuoteSequence::Absent)]
    #[case::string(r#""283851543383726791766048878""#, QuoteSequence::Unreadable)]
    #[case::negative("-1", QuoteSequence::Unreadable)]
    #[case::fraction("1.5", QuoteSequence::Unreadable)]
    #[case::boolean("true", QuoteSequence::Unreadable)]
    #[case::object("{}", QuoteSequence::Unreadable)]
    #[case::above_u128("3402823669209384634633746074317682114560", QuoteSequence::Unreadable)]
    fn an_unreadable_quote_sequence_costs_only_itself(
        #[case] literal: &str,
        #[case] expected: QuoteSequence,
    ) {
        const MUTATED: usize = 3;
        let captured: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        let text = message_with_mutated_quote_sequence(MUTATED, literal);
        let message: TitanPriceLevelMessage =
            serde_json::from_str(&text).expect("the message still parses");

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

    /// Leniency stops at the JSON: the parser still rejects text that is not a price level
    /// message, and a message whose price levels it cannot read. In the last case the whole message
    /// fails, not one side: the parser reads a level's `price` as an `f64`, so a number JSON
    /// allows but `f64` cannot hold fails the message before any side exists to drop. Side
    /// isolation applies only to a message that parsed.
    #[rstest]
    #[case::truncated(r#"{"block": 1, "ts": 2, "books": ["#)]
    #[case::not_json("nonsense")]
    #[case::unreadable_price_levels(
        r#"{"block": 1, "ts": 2, "books": [{"quoteAddress": "0x01", "token1": "A",
             "token2": "B", "token1Address": "0x01", "token2Address": "0x02",
             "asks": "nope", "bids": []}]}"#
    )]
    #[case::price_beyond_f64(
        r#"{"block": 1, "ts": 2, "books": [{"quoteAddress": "0x01", "token1": "A",
             "token2": "B", "token1Address": "0x01", "token2Address": "0x02",
             "asks": [{"price": 1e400, "cumVolume": 1.0}], "bids": []}]}"#
    )]
    fn invalid_json_still_rejects_the_message(#[case] text: &str) {
        assert!(serde_json::from_str::<TitanPriceLevelMessage>(text).is_err());
    }
}
