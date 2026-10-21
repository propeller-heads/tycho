//! Concrete [`StateOverrideProvider`] backed by the Titan pAMM state-override stream.
//!
//! Connects to the Titan `pamm_quote_stream_v2` WebSocket and tracks the latest per-block VM
//! storage overrides published for the known pAMM venues (e.g. FermiSwap), exposing them through
//! the generic [`StateOverrideProvider`] trait so the decoder can inject them into the matching
//! `EVMPoolState` instances before simulation. See
//! <https://docs.titanbuilder.xyz/propamms/takers>.
//!
//! # Why the block timestamp must be overridden too
//!
//! Titan quotes target the *pending* L1 block. Registry-backed venues (FermiSwap, Kipseli)
//! receive their oracle prices through Flashbots'
//! [priority-update-registry](https://github.com/flashbots/priority-update-registry), whose lanes
//! are stamped with the pending block's timestamp and whose reads revert with `StaleUpdate()` when
//! the stored timestamp falls outside a freshness window derived from `block.timestamp`.
//! Simulating against the latest indexed block therefore reverts even with the storage overrides
//! applied; the simulated block timestamp must be bumped to the pending block's.
//!
//! The pending block timestamp is derived from the entry's beacon `slot` field as
//! `BEACON_GENESIS_TIMESTAMP + slot * SECONDS_PER_SLOT`. This mirrors Titan's reference taker
//! implementation ([gattaca-com/propamm](https://github.com/gattaca-com/propamm), `quoter.py`) and
//! the [LambdaClass propAMM SDK](https://github.com/lambdaclass/propamm-router-contracts), both of
//! which pass exactly this value as a block override alongside the state overrides. Should `slot`
//! ever be absent, the timestamp is recovered from the registry lanes themselves, then from the
//! entry's wall clock; see [`TitanProvider::pending_block_timestamp`].
//!
//! # Observed stream anatomy (2026-07)
//!
//! Facts established against the live stream and the deployed contracts, on which this module
//! relies:
//!
//! - Each WebSocket message is `{"venues": [...]}`, one entry per venue that published. An entry
//!   names the venue by `quoteAddress` and lists the contracts it prices in `pricedAddresses`, and
//!   carries its own `slot` / `blockNumber` / `timestamp` and its `stateOverride`. A channel claims
//!   an entry whose `quoteAddress` or any `pricedAddresses` entry is one of its aliases, so a venue
//!   that streams under a new key is still found while it prices a known contract (Kipseli streams
//!   under `0xc9a9…` and prices its oracle `0x5cdb…`). Entries without storage overrides (some
//!   carry a null `maker` and an empty `stateOverride`) are skipped, leaving the last snapshot to
//!   expire on its own.
//! - FermiSwap and Kipseli store their oracle prices in the shared priority-update-registry
//!   instance at `0xda7afeed01fe625cf15d187a19f94b45f00b8c5f`. A lane's base slot is
//!   `keccak256(abi.encode(venue, laneIndex))`; the base slot value packs the lane's
//!   `updateTimestamp` (uint32, top 4 bytes), the slot count (uint8, next byte) and the first
//!   27-byte value, with continuation slots stored verbatim at `base + 1..`. The deployed
//!   registry's `MAX_UPDATE_AGE`/`MAX_UPDATE_LEAD_TIME` are both zero, so on-chain writes require
//!   `updateTimestamp == block.timestamp` — which is why Titan stamps lanes with the pending
//!   block's timestamp and why simulations must run under that timestamp.
//! - Venue keys are unstable: makers stream under several alias addresses (oracle and router keys
//!   carry identical payloads) and deployments change every few weeks. Each protocol therefore
//!   subscribes to all of its known aliases, and an empty snapshot channel is the first sign that
//!   every alias left the stream (`scripts/titan-venue-census.py` shows the live venue set).
//!
//! All Titan-specific knowledge (venue addresses, timestamp resolution, endpoint URL) lives only
//! in this module; the rest of the override machinery is protocol-agnostic.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::primitives::{Address, U256};
use futures::StreamExt;
use serde::Deserialize;
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use tokio::{
    sync::watch,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use super::{FailurePolicy, OverrideSnapshot, StateOverrideProvider};

/// Storage overrides keyed by contract address, then by storage slot.
type VenueOverrides = HashMap<Address, HashMap<U256, U256>>;

/// A parsed Titan quote-stream message: the entries of the venues that published in it. Entries
/// stay raw JSON until [`TitanMessage::entries`] reads them, so an entry this module cannot read
/// costs that entry alone.
#[derive(Debug, Deserialize)]
struct TitanMessage {
    #[serde(default)]
    venues: Vec<serde_json::Value>,
}

impl TitanMessage {
    /// The entries whose keys and block fields parse; an unreadable one is logged and skipped.
    fn entries(self) -> Vec<VenueEntry> {
        self.venues
            .into_iter()
            .filter_map(|raw| match serde_json::from_value(raw) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    warn!(error = %e, "Skipping unreadable Titan venue entry");
                    None
                }
            })
            .collect()
    }
}

/// One venue's entry in a message.
///
/// `slot` is the beacon slot the quote targets, `timestamp` the nanosecond wall-clock instant
/// Titan built it. The override set (eth_call State Override Set shape) stays raw JSON until a
/// channel claims the entry, so an override this module cannot read is reported against that
/// channel, as before, rather than dropping the entry for every channel. Fields this module does
/// not read (`maker`, `assetPairs`, `pairSequences`, ...) are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VenueEntry {
    #[serde(default)]
    quote_address: Option<Address>,
    #[serde(default)]
    priced_addresses: Vec<Address>,
    #[serde(default)]
    slot: Option<u64>,
    #[serde(default)]
    block_number: Option<u64>,
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(default)]
    state_override: serde_json::Value,
}

impl VenueEntry {
    /// Whether the entry belongs to a channel with these aliases: its `quoteAddress` or one of
    /// its `pricedAddresses` is among them.
    fn is_any_of(&self, venues: &[Address]) -> bool {
        self.quote_address
            .iter()
            .chain(&self.priced_addresses)
            .any(|address| venues.contains(address))
    }
}

/// A single account's override. Only `stateDiff` (storage slot -> value) is used, deserialized
/// directly from its `0x` hex string keys and values; balance / nonce / code are ignored (serde
/// drops unknown fields).
#[derive(Debug, Deserialize)]
struct AccountState {
    #[serde(rename = "stateDiff", default)]
    state_diff: HashMap<U256, U256>,
}

/// Default Titan pAMM quote-stream WebSocket endpoint serving all known pAMM venues.
const TITAN_URL: &str = "wss://eu.data.titanbuilder.xyz/ws/pamm_quote_stream_v2";
/// Environment variable that overrides the default endpoint when no custom URL is passed. Named
/// after the V2 socket, so a value still pointing at the retired V1 socket is not read.
const TITAN_URL_ENV: &str = "TITAN_PAMM_QUOTE_STREAM_URL";

/// A channel of Titan's quote stream: the overrides published under one venue's keys, served
/// under one key ([`key`](Self::key)) that a pool subscribes to.
///
/// Both [`key`](Self::key) and [`venue_aliases`](Self::venue_aliases) match every variant, so a new
/// channel does not compile until it has a key and its stream addresses, and
/// [`default_providers`] then serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter)]
pub enum TitanChannel {
    /// FermiSwap's oracle prices, written to the priority-update-registry.
    FermiSwap,
    /// Kipseli's oracle prices, written to the priority-update-registry.
    Kipseli,
    /// bopAMM (Bebop)'s signed maker quote, written to the venue's own storage.
    BopAmm,
    /// Metric's Pyth oracle. Metric's Ethereum pools read two oracles, published under two
    /// different stream keys, so a Metric swap needs this channel and
    /// [`MetricCompressedOracle`](Self::MetricCompressedOracle) together; neither is an alias of
    /// the other.
    MetricPythOracle,
    /// Metric's compressed oracle; see [`MetricPythOracle`](Self::MetricPythOracle).
    MetricCompressedOracle,
}

impl TitanChannel {
    /// The key the channel is registered and subscribed under. For a pAMM Tycho indexes it is the
    /// pAMM's `protocol_system`; Metric's oracles are no indexed protocol, so their keys are
    /// `metric:*` names of their own.
    pub const fn key(self) -> &'static str {
        match self {
            Self::FermiSwap => "vm:fermiswap",
            Self::Kipseli => "vm:kipseli",
            Self::BopAmm => "vm:bopamm",
            Self::MetricPythOracle => "metric:pyth_oracle",
            Self::MetricCompressedOracle => "metric:compressed_oracle",
        }
    }

    /// The channel registered under `key`, or `None` when Titan serves no such channel.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::iter().find(|channel| channel.key() == key)
    }

    /// The addresses that identify the channel's entries on Titan's quote stream, matched against
    /// an entry's `quoteAddress` and `pricedAddresses`.
    ///
    /// Every alias of a channel stands for the same maker feed (e.g. its oracle and router
    /// addresses); listing all of them keeps the channel served when one key disappears from the
    /// stream, which venue deployments have been observed doing within hours.
    const fn venue_aliases(self) -> &'static [&'static str] {
        match self {
            // The FermiSwap oracle and the FermiSwapper router. Both keys carry the same
            // registry diffs; either serves the protocol.
            Self::FermiSwap => &[
                "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e",
                "0x5979458912f80b96d30d4220af8e2e4925a33320",
            ],
            // The Kipseli oracle and the KipseliPropAMMWrapper router, both carrying the same
            // registry diffs.
            Self::Kipseli => &[
                "0x5cdbe59400cc2efdcc2b54acca4a99fe00dd588c",
                "0x71e790dd841c8a9061487cb3e78c288e75ce0b3d",
            ],
            // A self-storage pAMM whose frames write the maker's signed quote into the venue's
            // own storage.
            Self::BopAmm => &["0x160141a205f5ddcf096ba3f48b7ed21eb52c62ea"],
            // The oracle of every anchored Metric pool on Ethereum.
            Self::MetricPythOracle => &["0x30a88bf894d1b806125a23ebf23d715496fd7102"],
            // Carries the reference feeds.
            Self::MetricCompressedOracle => &["0xe844701934d9c6085b292e428f41b911cc0b3872"],
        }
    }

    /// [`venue_aliases`](Self::venue_aliases), parsed.
    fn venues(self) -> Vec<Address> {
        self.venue_aliases()
            .iter()
            .map(|venue| {
                venue
                    .parse()
                    .unwrap_or_else(|e| panic!("hardcoded venue address {venue} must parse: {e}"))
            })
            .collect()
    }
}

/// Longest gap between Titan messages tolerated before the socket is treated as dead and
/// re-established. Titan pushes several updates per second, so a multi-second silence means a
/// stalled or half-open connection that [`StreamExt::next`] would otherwise wait on forever.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a single connection attempt may take before it is aborted and retried, so a hung TCP/TLS
/// handshake cannot block the background task forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff is `2^min(attempt, MAX_BACKOFF_EXP)` seconds, i.e. capped at 32s.
const MAX_BACKOFF_EXP: u32 = 5;

/// How long a Titan quote stays usable after it was built, in seconds. Titan targets the pending
/// L1 block, so one block time (12s) is the window in which the overridden prices are current; past
/// it the snapshot is stale and the pool reverts to Tycho's indexed state. Used to derive each
/// snapshot's [`OverrideSnapshot::expires_at`] from the entry's wall-clock `timestamp`.
const TITAN_QUOTE_TTL_SECS: u64 = 12;

/// Unix timestamp of the Ethereum beacon chain genesis (2020-12-01 12:00:23 UTC). A beacon slot's
/// timestamp is `BEACON_GENESIS_TIMESTAMP + slot * SECONDS_PER_SLOT`.
const BEACON_GENESIS_TIMESTAMP: u64 = 1_606_824_023;

/// Post-merge Ethereum slot duration in seconds.
const SECONDS_PER_SLOT: u64 = 12;

/// Longest a registry lane timestamp may lie ahead of the entry's wall-clock `timestamp` and still
/// be accepted by the lane-extraction fallback. A genuine lane timestamp is the pending block's
/// timestamp, i.e. at most one slot ahead of the instant the quote was built; two slots leaves
/// margin for clock skew. Anything further out is packed venue data that merely looks like a
/// timestamp.
const MAX_LANE_TIMESTAMP_LEAD_SECS: u64 = 2 * SECONDS_PER_SLOT;

/// Default override providers contributed by Titan, keyed by `protocol_system`.
///
/// Returns one shared [`TitanProvider`] mapped under every [`TitanChannel::key`] yielded by
/// `protocols` (the ones the caller wants a built-in default for). Takes an owned iterator so the
/// caller can forward its strings without cloning. The provider is spawned only when at least one
/// such venue needs serving — so an empty or fully-overridden `protocols` opens no connection. The
/// single provider is shared across its protocols via `Arc`, not duplicated.
pub fn default_providers(
    protocols: impl IntoIterator<Item = String>,
) -> HashMap<String, Arc<dyn StateOverrideProvider>> {
    let needed: Vec<TitanChannel> = protocols
        .into_iter()
        .filter_map(|protocol| TitanChannel::from_key(&protocol))
        .collect();
    if needed.is_empty() {
        return HashMap::new();
    }
    // Auto-registration endpoint: the `TITAN_PAMM_QUOTE_STREAM_URL` env override if set, else the
    // built-in default. Consumers needing a different endpoint can register their own
    // `TitanProvider::spawn(url, channels)` via `with_override_provider` instead.
    let url = std::env::var(TITAN_URL_ENV).unwrap_or_else(|_| TITAN_URL.to_string());
    let provider: Arc<dyn StateOverrideProvider> = Arc::new(TitanProvider::spawn(url, &needed));
    needed
        .into_iter()
        .map(|channel| (channel.key().to_string(), provider.clone()))
        .collect()
}

/// A [`StateOverrideProvider`] backed by a single Titan pAMM WebSocket connection.
///
/// One connection serves the `protocols` it was spawned for; per-protocol snapshots are exposed
/// through the `watch` receivers held here. Cheap to clone.
pub struct TitanProvider {
    /// Latest resolved snapshot per served `protocol_system`, kept fresh by the background task.
    receivers: HashMap<String, watch::Receiver<OverrideSnapshot>>,
}

impl TitanProvider {
    /// Opens ONE WebSocket connection to `url` serving the given `channels` and spawns the
    /// background reconnect/parse task that keeps their snapshots up to date. Each channel is
    /// subscribed under its [`TitanChannel::key`], and the task only parses their venues.
    ///
    /// Public so a consumer can point a provider at a custom endpoint and register it via
    /// [`ProtocolStreamBuilder::with_override_provider`](crate::evm::stream::ProtocolStreamBuilder::with_override_provider),
    /// which takes precedence over the default registry.
    ///
    /// Returns immediately; the connection is established and maintained in the background, so a
    /// transient WebSocket failure never blocks or crashes the caller.
    pub fn spawn(url: String, channels: &[TitanChannel]) -> Self {
        let mut receivers = HashMap::new();
        let mut senders = Vec::new();
        for &channel in channels {
            let (tx, rx) = watch::channel(OverrideSnapshot::default());
            receivers.insert(channel.key().to_string(), rx);
            senders.push((channel.venues(), tx));
        }
        tokio::spawn(Self::run(url, senders));
        Self { receivers }
    }

    /// Maintains the Titan WebSocket connection for the lifetime of the process.
    ///
    /// Connects, reads messages, and publishes each parsed [`OverrideSnapshot`] on the matching
    /// channel (an entry naming any of the channel's aliases publishes). Any
    /// disconnect, read error, server-side close, or idle timeout drops back to the reconnect
    /// loop, which retries forever with capped exponential backoff. The backoff is reset only
    /// after a message is actually received, so a socket that connects and immediately drops
    /// still backs off instead of busy-looping. This task never returns and never panics: every
    /// failure mode is logged and retried.
    async fn run(url: String, senders: Vec<(Vec<Address>, watch::Sender<OverrideSnapshot>)>) {
        let mut attempt: u32 = 0;
        loop {
            // Every receiver (the provider handle plus all pool clones) has been dropped, so
            // nobody consumes overrides anymore: stop the task and release the connection.
            // Because `subscribe` needs a live provider (which owns the master receivers), once
            // this is true no future pool can obtain the channel either.
            if senders
                .iter()
                .all(|(_, tx)| tx.is_closed())
            {
                warn!("All Titan override receivers dropped; stopping quote-stream task");
                return;
            }

            match timeout(CONNECT_TIMEOUT, connect_async(url.as_str())).await {
                Ok(Ok((mut ws_stream, _))) => {
                    info!(%url, "Connected to Titan pAMM quote stream");
                    loop {
                        // Bound the wait so a half-open connection (no data and no close frame) is
                        // detected and retried instead of blocking on `next()` indefinitely.
                        let message = match timeout(READ_IDLE_TIMEOUT, ws_stream.next()).await {
                            Ok(Some(message)) => message,
                            // Stream ended: the server hung up without sending a close frame.
                            Ok(None) => {
                                warn!("Titan quote stream ended; reconnecting");
                                break;
                            }
                            // No traffic within the idle window: assume a stalled socket.
                            Err(_elapsed) => {
                                warn!(
                                    idle_secs = READ_IDLE_TIMEOUT.as_secs(),
                                    "No Titan message within idle timeout; reconnecting"
                                );
                                break;
                            }
                        };

                        match message {
                            // Expected case: a JSON quote-stream frame. Deserialize it once, then
                            // extract each venue's snapshot from the parsed value. Receiving a
                            // frame proves the connection is healthy,
                            // so reset the reconnect backoff.
                            Ok(Message::Text(text)) => {
                                attempt = 0;
                                match serde_json::from_str::<TitanMessage>(text.as_str()) {
                                    Ok(message) => {
                                        let entries = message.entries();
                                        for (venues, sender) in &senders {
                                            // A send error means every receiver has been
                                            // dropped; the check after this loop stops the
                                            // task, so ignoring it here is safe.
                                            if let Some(snapshot) =
                                                Self::first_snapshot(&entries, venues)
                                            {
                                                let _ = sender.send(snapshot);
                                            }
                                        }
                                    }
                                    // Unparseable frame: log and keep the connection.
                                    Err(e) => warn!(error = %e, "Failed to parse Titan message"),
                                }
                                // Stop promptly if every consumer went away while connected.
                                if senders
                                    .iter()
                                    .all(|(_, tx)| tx.is_closed())
                                {
                                    warn!("All Titan override receivers dropped; stopping quote-stream task");
                                    return;
                                }
                            }
                            // Titan only sends JSON text; a binary frame is unexpected. Skip it and
                            // keep the (otherwise healthy) connection rather than reconnecting.
                            Ok(Message::Binary(bytes)) => {
                                warn!(len = bytes.len(), "Ignoring unexpected binary Titan frame");
                            }
                            // Keep-alive frames. tokio-tungstenite answers pings with pongs
                            // automatically while the stream is polled, so there is nothing to do.
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                            // `Frame` is only produced when *sending* raw frames; the read side
                            // never yields it, so this arm is unreachable in practice — kept only
                            // for match exhaustiveness.
                            Ok(Message::Frame(_)) => {}
                            // Server initiated a graceful close — reconnect.
                            Ok(Message::Close(frame)) => {
                                warn!(?frame, "Titan quote stream closed by server; reconnecting");
                                break;
                            }
                            // Transport/protocol error (broken pipe, invalid frame, ...) —
                            // reconnect.
                            Err(e) => {
                                warn!(error = %e, "Titan quote stream read error; reconnecting");
                                break;
                            }
                        }
                    }
                }
                // Connection refused / TLS error — fall through to backoff and retry.
                Ok(Err(e)) => {
                    warn!(error = %e, "Failed to connect to Titan quote stream; retrying");
                }
                // Handshake did not complete within the timeout — retry after backoff.
                Err(_elapsed) => {
                    warn!(
                        timeout_secs = CONNECT_TIMEOUT.as_secs(),
                        "Titan connect timed out; retrying"
                    );
                }
            }

            attempt = attempt.saturating_add(1);
            let backoff = Duration::from_secs(2u64.pow(attempt.min(MAX_BACKOFF_EXP)));
            warn!(seconds = backoff.as_secs(), attempt, "Backing off before reconnecting to Titan");
            sleep(backoff).await;
        }
    }

    /// The snapshot of the first entry in `entries` that belongs to a channel with these aliases
    /// and carries storage overrides, or `None` when no entry does. An entry whose override set
    /// cannot be read is logged and passed over, keeping the channel's last good snapshot.
    fn first_snapshot(entries: &[VenueEntry], venues: &[Address]) -> Option<OverrideSnapshot> {
        entries
            .iter()
            .filter(|entry| entry.is_any_of(venues))
            .find_map(|entry| match Self::extract_entry(entry) {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    warn!(?venues, error = %e, "Failed to extract Titan venue snapshot");
                    None
                }
            })
    }

    /// Extracts an entry's snapshot (`stateDiff` overrides + resolved block environment).
    ///
    /// Returns `Ok(None)` when the entry overrides no storage, so there is nothing to apply. The
    /// block timestamp is resolved to the pending block's timestamp (see
    /// [`pending_block_timestamp`](Self::pending_block_timestamp)) so the registry's
    /// oracle-staleness guard sees the overridden prices as current, and `expires_at` is set to the
    /// entry's wall-clock `timestamp` plus [`TITAN_QUOTE_TTL_SECS`] so the pool stops using it once
    /// it goes stale.
    fn extract_entry(entry: &VenueEntry) -> Result<Option<OverrideSnapshot>, String> {
        let state_override: HashMap<Address, AccountState> = if entry.state_override.is_null() {
            HashMap::new()
        } else {
            serde_json::from_value(entry.state_override.clone())
                .map_err(|e| format!("invalid venue override: {e}"))?
        };

        // We only consume `stateDiff` storage; balance / nonce / code are ignored. Slots and values
        // were parsed into `U256` by serde, so we just drop accounts with no overrides.
        let mut storage: VenueOverrides = HashMap::new();
        for (account, account_override) in state_override {
            if !account_override.state_diff.is_empty() {
                storage.insert(account, account_override.state_diff);
            }
        }
        if storage.is_empty() {
            return Ok(None);
        }

        let block_timestamp = Self::pending_block_timestamp(entry.slot, entry.timestamp, &storage);

        // Expire the snapshot one block time after Titan built it, using the entry's nanosecond
        // wall-clock `timestamp`. Once past this the pool drops the override and reverts to Tycho's
        // indexed state, so a stalled stream can never keep serving prices indefinitely.
        let expires_at = entry
            .timestamp
            .map(|ns| ns / 1_000_000_000 + TITAN_QUOTE_TTL_SECS);

        Ok(Some(OverrideSnapshot {
            block_number: entry.block_number,
            block_timestamp,
            storage: Arc::new(storage),
            expires_at,
            // Titan snapshots target the pending block and a pair is only quotable under them
            // once its oracle lane is re-stamped for that block — briefly false for every pair
            // at block transitions, and for longer stretches for pairs the maker is not actively
            // quoting. The indexed state remains simulatable throughout, so pools should fall
            // back to it instead of surfacing the transient revert.
            failure_policy: FailurePolicy::FallbackToIndexedState,
        }))
    }

    /// Returns the unix-seconds timestamp of the pending block an entry's quote targets, or `None`
    /// if the entry carries no timestamp signal at all. `slot` and `timestamp` are the entry's.
    ///
    /// Resolution order:
    /// 1. The entry's beacon `slot` field (`BEACON_GENESIS_TIMESTAMP + slot * 12`) — the
    ///    authoritative source, and the one used by Titan's reference taker
    ///    (<https://github.com/gattaca-com/propamm>, `quoter.py`) and the LambdaClass propAMM SDK
    ///    (<https://github.com/lambdaclass/propamm-router-contracts>). Rejected (falling through
    ///    to the next sources) if the multiplication overflows or the derived value is implausible
    ///    against the entry's wall clock — see [`slot_timestamp`](Self::slot_timestamp).
    /// 2. The registry lane timestamps embedded in the storage overrides (see
    ///    [`max_lane_timestamp`](Self::max_lane_timestamp)): Titan stamps lanes with the pending
    ///    block's timestamp, so a genuine lane timestamp equals the slot-derived value. Only
    ///    accepted when it lies within [`MAX_LANE_TIMESTAMP_LEAD_SECS`] ahead of the entry's wall
    ///    clock, so packed venue data that merely looks like a timestamp (e.g. bopAMM's signed
    ///    quote encoding) can never be injected as the block timestamp. Without a wall clock to
    ///    validate against, a lane timestamp is never trusted.
    /// 3. The entry's nanosecond wall-clock `timestamp`. Up to one slot earlier than the pending
    ///    block, so registry-backed venues may still reject it as stale, but it is the best
    ///    remaining approximation.
    fn pending_block_timestamp(
        slot: Option<u64>,
        timestamp: Option<u64>,
        storage: &VenueOverrides,
    ) -> Option<u64> {
        let wall_clock = timestamp.map(|ns| ns / 1_000_000_000);
        if let Some(slot_ts) = Self::slot_timestamp(slot, wall_clock) {
            return Some(slot_ts);
        }
        match (Self::max_lane_timestamp(storage), wall_clock) {
            (Some(lane), Some(now))
                if (now..=now + MAX_LANE_TIMESTAMP_LEAD_SECS).contains(&lane) =>
            {
                Some(lane)
            }
            (Some(_), wall_clock) | (None, wall_clock) => wall_clock,
        }
    }

    /// Derives the timestamp of the beacon `slot`
    /// (`BEACON_GENESIS_TIMESTAMP + slot * SECONDS_PER_SLOT`).
    ///
    /// The slot comes verbatim from the stream, so it is not trusted blindly: returns `None` when
    /// `slot` is absent, when the arithmetic overflows, or when a wall clock is available and the
    /// derived value falls outside `wall_clock - SECONDS_PER_SLOT ..=
    /// wall_clock + MAX_LANE_TIMESTAMP_LEAD_SECS` (a genuine pending-block timestamp is at most
    /// one slot ahead of the instant the quote was built; the slack behind covers clock skew and
    /// late frames). Without a wall clock the arithmetic-checked value is accepted as is.
    fn slot_timestamp(slot: Option<u64>, wall_clock: Option<u64>) -> Option<u64> {
        let derived = slot?
            .checked_mul(SECONDS_PER_SLOT)
            .and_then(|offset| BEACON_GENESIS_TIMESTAMP.checked_add(offset))?;
        match wall_clock {
            Some(now)
                if !(now.saturating_sub(SECONDS_PER_SLOT)..=
                    now.saturating_add(MAX_LANE_TIMESTAMP_LEAD_SECS))
                    .contains(&derived) =>
            {
                None
            }
            Some(_) | None => Some(derived),
        }
    }

    /// Returns the newest lane update timestamp (in seconds) across all overridden slots, or
    /// `None` if no plausible one is present.
    ///
    /// Registry lane base slots pack a uint32 unix-seconds `updateTimestamp` in the top 4 bytes of
    /// the slot value (see the module docs and
    /// <https://github.com/flashbots/priority-update-registry>). This extracts that prefix from
    /// every overridden slot and keeps the maximum value inside a plausible unix-timestamp window.
    /// Heuristic by nature — continuation slots and non-registry venues store raw data whose top
    /// bytes can collide with the window — so callers must cross-check the result against the
    /// entry's wall clock before trusting it.
    fn max_lane_timestamp(overrides: &VenueOverrides) -> Option<u64> {
        const MIN_PLAUSIBLE: u64 = 1_000_000_000; // ~2001-09
        const MAX_PLAUSIBLE: u64 = u32::MAX as u64; // uint32 ceiling (~2106)
        overrides
            .values()
            .flat_map(|slots| slots.values())
            .filter_map(|value| {
                let candidate = u64::try_from(*value >> 224).ok()?;
                (MIN_PLAUSIBLE..=MAX_PLAUSIBLE)
                    .contains(&candidate)
                    .then_some(candidate)
            })
            .max()
    }
}

impl StateOverrideProvider for TitanProvider {
    fn subscribe(&self, protocol_system: &str) -> Option<watch::Receiver<OverrideSnapshot>> {
        self.receivers
            .get(protocol_system)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builder registers the provider for an indexed pAMM by its `protocol_system`, so that is
    /// the key its channel must answer to.
    #[test]
    fn indexed_pamm_channels_are_keyed_by_protocol_system() {
        assert_eq!(TitanChannel::from_key("vm:fermiswap"), Some(TitanChannel::FermiSwap));
        assert_eq!(TitanChannel::from_key("vm:kipseli"), Some(TitanChannel::Kipseli));
        assert_eq!(TitanChannel::from_key("vm:bopamm"), Some(TitanChannel::BopAmm));
    }

    #[test]
    fn every_channel_has_a_distinct_key_and_parsable_venues() {
        let mut keys = std::collections::HashSet::new();
        for channel in TitanChannel::iter() {
            assert!(keys.insert(channel.key()), "duplicate key {}", channel.key());
            assert_eq!(TitanChannel::from_key(channel.key()), Some(channel));
            assert!(!channel.venues().is_empty(), "{channel:?} needs at least one venue");
        }
    }

    #[test]
    fn an_unknown_key_has_no_channel() {
        assert_eq!(TitanChannel::from_key("vm:unknown"), None);
    }

    /// A message captured from the live `pamm_quote_stream_v2` socket (2026-10-21), trimmed to two
    /// storage words per venue: Kipseli under its `0xc9a9…` key, which no channel lists, pricing
    /// its oracle `0x5cdb…`, which `Kipseli` does; FermiSwap under its router key; and an entry
    /// with a null `maker` and nothing to override.
    const SAMPLE_MESSAGE: &str = r#"{"venues": [
        {
            "maker": "Kipseli",
            "quoteAddress": "0xc9a956b5196dfe110debe8781857bc8b97d32091",
            "pricedAddresses": [
                "0xc9a956b5196dfe110debe8781857bc8b97d32091",
                "0x5cdbe59400cc2efdcc2b54acca4a99fe00dd588c"
            ],
            "assetPairs": [{"token_1": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                            "token_2": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"}],
            "pairSequences": {"0": 285546706930382120000000000},
            "slot": 15479518,
            "blockNumber": 26240354,
            "timestamp": 1792578232897607200,
            "stateOverride": {
                "0xda7afeed021eafc1c1af9c362de477dad0396b81": {
                    "balance": "0x0",
                    "nonce": "0x1",
                    "stateDiff": {
                        "0x25c009fc7d8173c87c44eba2110cf042b3bb2f5c82befc107f5eb2e25d05b64a": "0x0000000000000000000000000000000000540288a9d8f91e005000131cd3ae68",
                        "0x25c009fc7d8173c87c44eba2110cf042b3bb2f5c82befc107f5eb2e25d05b649": "0x6ad892bf020000000000000000000000000000000000000200000b6991ebf1ab"
                    }
                }
            }
        },
        {
            "maker": "Fermi",
            "quoteAddress": "0x5979458912f80b96d30d4220af8e2e4925a33320",
            "pricedAddresses": ["0x5979458912f80b96d30d4220af8e2e4925a33320"],
            "assetPairs": [{"token_1": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                            "token_2": "0xdac17f958d2ee523a2206206994597c13d831ec7"}],
            "pairSequences": {"0": 285546706930380330000000000},
            "slot": 15479518,
            "blockNumber": 26240354,
            "timestamp": 1792578232743987700,
            "stateOverride": {
                "0x0109aa912b58508886a2a707204b0f8c8b164ccc": {
                    "balance": "0x0",
                    "nonce": "0x1",
                    "stateDiff": {
                        "0xbb3ced3788a14d74a5ae4731b9cd3b1d86b85903bb5c31829df82a13ecb837f5": "0x0000000000001388000000000000000000000000000000000000000001906562",
                        "0x156b1d71de08fed89d0fce38008e2b9d03a8998077e394b20597ef3d148f5ebd": "0x000000000000000000000000000000000000000000000000000000390719ca1e"
                    }
                }
            }
        },
        {
            "maker": null,
            "quoteAddress": "0x77047af6cd8f7f84d96a020a2833d24916b75fde",
            "pricedAddresses": ["0x77047af6cd8f7f84d96a020a2833d24916b75fde"],
            "assetPairs": [],
            "pairSequences": [],
            "slot": 15479518,
            "blockNumber": 26240354,
            "timestamp": 1792578232859779000,
            "stateOverride": {}
        }
    ]}"#;

    fn sample_entries() -> Vec<VenueEntry> {
        serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE)
            .expect("valid JSON")
            .entries()
    }

    fn address(hex: &str) -> Address {
        hex.parse().unwrap()
    }

    #[test]
    fn a_channel_takes_the_entry_quoted_under_one_of_its_aliases() {
        let snapshot =
            TitanProvider::first_snapshot(&sample_entries(), &TitanChannel::FermiSwap.venues())
                .expect("FermiSwap's router key is in the message");

        assert_eq!(snapshot.block_number, Some(26240354));
        // Pending block timestamp derived from the beacon slot: 1606824023 + 15479518 * 12.
        assert_eq!(snapshot.block_timestamp, Some(1792578239));
        // 1792578232743987700 ns -> 1792578232 s, plus the 12s quote TTL.
        assert_eq!(snapshot.expires_at, Some(1792578232 + TITAN_QUOTE_TTL_SECS));
        // Titan overrides are only intermittently applicable, so pools must fall back.
        assert_eq!(snapshot.failure_policy, FailurePolicy::FallbackToIndexedState);

        let account = address("0x0109aa912b58508886a2a707204b0f8c8b164ccc");
        let slot = "0x156b1d71de08fed89d0fce38008e2b9d03a8998077e394b20597ef3d148f5ebd"
            .parse::<U256>()
            .unwrap();
        let value = "0x000000000000000000000000000000000000000000000000000000390719ca1e"
            .parse::<U256>()
            .unwrap();
        assert_eq!(
            snapshot
                .storage
                .get(&account)
                .and_then(|s| s.get(&slot)),
            Some(&value)
        );
        assert_eq!(snapshot.storage.len(), 1, "only accounts with a stateDiff are kept");
    }

    /// Kipseli streams under a key no channel lists, but prices its oracle, which `Kipseli` does.
    #[test]
    fn a_channel_takes_an_entry_that_prices_one_of_its_aliases() {
        let snapshot =
            TitanProvider::first_snapshot(&sample_entries(), &TitanChannel::Kipseli.venues())
                .expect("Kipseli prices its oracle in the message");

        let registry = address("0xda7afeed021eafc1c1af9c362de477dad0396b81");
        assert_eq!(snapshot.storage[&registry].len(), 2);
        // 1792578232897607200 ns -> 1792578232 s.
        assert_eq!(snapshot.expires_at, Some(1792578232 + TITAN_QUOTE_TTL_SECS));
    }

    #[test]
    fn a_channel_absent_from_the_message_gets_nothing() {
        assert!(TitanProvider::first_snapshot(&sample_entries(), &TitanChannel::BopAmm.venues())
            .is_none());
    }

    /// An entry with nothing to override must not replace a channel's last snapshot with an empty
    /// one: Titan sends them with a null `maker`, and the last snapshot expires on its own.
    #[test]
    fn an_entry_without_storage_overrides_publishes_nothing() {
        let stelaxis = [address("0x77047af6cd8f7f84d96a020a2833d24916b75fde")];
        assert!(TitanProvider::first_snapshot(&sample_entries(), &stelaxis).is_none());
    }

    /// One unreadable override set costs only that entry: a later entry of the same channel still
    /// serves it, and the other channels in the message are unaffected.
    #[test]
    fn an_unreadable_override_falls_through_to_the_next_entry() {
        let mut message: serde_json::Value = serde_json::from_str(SAMPLE_MESSAGE).unwrap();
        let venues = message["venues"]
            .as_array_mut()
            .unwrap();
        let mut broken = venues[1].clone();
        broken["stateOverride"] = serde_json::json!({ "not-an-address": {} });
        venues.insert(1, broken);
        let entries = serde_json::from_value::<TitanMessage>(message)
            .unwrap()
            .entries();

        let fermi = TitanProvider::first_snapshot(&entries, &TitanChannel::FermiSwap.venues())
            .expect("the readable FermiSwap entry still serves");
        assert_eq!(fermi.block_number, Some(26240354));
        assert!(TitanProvider::first_snapshot(&entries, &TitanChannel::Kipseli.venues()).is_some());
    }

    /// An entry whose keys do not parse is skipped without taking the message's other entries with
    /// it.
    #[test]
    fn an_unreadable_entry_is_skipped() {
        let mut message: serde_json::Value = serde_json::from_str(SAMPLE_MESSAGE).unwrap();
        message["venues"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "quoteAddress": 7 }));
        let entries = serde_json::from_value::<TitanMessage>(message)
            .unwrap()
            .entries();

        assert_eq!(entries.len(), 3);
    }

    fn lane_overrides(lane_ts: u64) -> VenueOverrides {
        let account = "0xda7afeed01fe625cf15d187a19f94b45f00b8c5f"
            .parse::<Address>()
            .unwrap();
        HashMap::from([(account, HashMap::from([(U256::from(1u64), U256::from(lane_ts) << 224)]))])
    }

    /// Wall clock 1782997641s, one slot before the pending block's timestamp 1782997643 (both
    /// observed live for block 25445022 / slot 14681135).
    const WALL_CLOCK_NS: u64 = 1782997641189454379;
    const PENDING_BLOCK_TS: u64 = 1782997643;

    #[test]
    fn pending_block_timestamp_derives_from_beacon_slot() {
        // The slot field wins even when lane timestamps are present.
        let (slot, timestamp) = (Some(14681135), Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(slot, timestamp, &lane_overrides(1_800_000_000)),
            Some(PENDING_BLOCK_TS)
        );
    }

    #[test]
    fn pending_block_timestamp_rejects_implausible_slot() {
        // A slot decoding to a timestamp far from the entry's wall clock is corrupt stream data;
        // resolution must fall through to the remaining sources (here: the wall clock).
        let (slot, timestamp) = (Some(1), Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(slot, timestamp, &HashMap::new()),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_survives_overflowing_slot() {
        // `u64::MAX * SECONDS_PER_SLOT` must not panic or wrap; it falls through to the wall
        // clock.
        let (slot, timestamp) = (Some(u64::MAX), Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(slot, timestamp, &HashMap::new()),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_ignores_lane_without_wall_clock() {
        // Without a wall clock there is nothing to validate a lane timestamp against, so even a
        // plausible-looking one must not be trusted.
        let (slot, timestamp) = (None, None);
        assert!(TitanProvider::pending_block_timestamp(
            slot,
            timestamp,
            &lane_overrides(PENDING_BLOCK_TS)
        )
        .is_none());
    }

    #[test]
    fn pending_block_timestamp_backs_up_to_lane_timestamps() {
        let (slot, timestamp) = (None, Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(
                slot,
                timestamp,
                &lane_overrides(PENDING_BLOCK_TS)
            ),
            Some(PENDING_BLOCK_TS)
        );
    }

    #[test]
    fn pending_block_timestamp_rejects_implausible_lane_timestamps() {
        // Top bytes of bopAMM's packed quote data decode to 1557751927 (2019) — far outside the
        // wall-clock window, so the fallback must skip it and use the wall clock.
        let (slot, timestamp) = (None, Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(slot, timestamp, &lane_overrides(1_557_751_927)),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_falls_back_to_wall_clock() {
        let (slot, timestamp) = (None, Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(slot, timestamp, &HashMap::new()),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_none_without_any_signal() {
        let (slot, timestamp) = (None, None);
        assert!(TitanProvider::pending_block_timestamp(slot, timestamp, &HashMap::new()).is_none());
    }
}
