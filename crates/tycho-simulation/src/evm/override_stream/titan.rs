//! Concrete [`StateOverrideProvider`] backed by the Titan pAMM state-override stream.
//!
//! Connects to the Titan `pamm_quote_stream` WebSocket and tracks the latest per-block VM storage
//! overrides published for the known pAMM venues (e.g. FermiSwap), exposing them through the
//! generic [`StateOverrideProvider`] trait so the decoder can inject them into the matching
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
//! The pending block timestamp is derived from the frame's beacon `slot` field as
//! `BEACON_GENESIS_TIMESTAMP + slot * SECONDS_PER_SLOT`. This mirrors Titan's reference taker
//! implementation ([gattaca-com/propamm](https://github.com/gattaca-com/propamm), `quoter.py`) and
//! the [LambdaClass propAMM SDK](https://github.com/lambdaclass/propamm-router-contracts), both of
//! which pass exactly this value as a block override alongside the state overrides. Should `slot`
//! ever be absent, the timestamp is recovered from the registry lanes themselves, then from the
//! frame's wall clock; see [`TitanProvider::pending_block_timestamp`].
//!
//! # Observed stream anatomy (2026-07)
//!
//! Facts established against the live stream and the deployed contracts, on which this module
//! relies:
//!
//! - Each WebSocket message carries `slot` / `blockNumber` / `timestamp` plus a single top-level
//!   venue key (one message per venue per update), unlike the docs' merged multi-venue sample. Both
//!   shapes are handled.
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
use tokio::{
    sync::watch,
    time::{sleep, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use super::{FailurePolicy, OverrideSnapshot, StateOverrideProvider};

/// Storage overrides keyed by contract address, then by storage slot.
type VenueOverrides = HashMap<Address, HashMap<U256, U256>>;

/// A parsed Titan quote-stream frame.
///
/// `slot` (the beacon slot the quote targets), `blockNumber` and `timestamp` (the nanosecond
/// wall-clock instant Titan built the quote) are consumed at this level; every other top-level key
/// (venue addresses and any future fields) lands in `overrides` as raw JSON. Only the venue keys
/// we look up are then deserialized into [`AddressEntry`], so an upstream schema change that adds
/// top-level fields never breaks parsing.
#[derive(Debug, Deserialize)]
struct TitanMessage {
    #[serde(default)]
    slot: Option<u64>,
    #[serde(rename = "blockNumber", default)]
    block_number: Option<u64>,
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(flatten)]
    overrides: HashMap<String, serde_json::Value>,
}

/// A single venue's override set, following the eth_call State Override Set shape. Only
/// `stateOverride` is consumed; account addresses are parsed straight from their `0x` hex keys.
#[derive(Debug, Deserialize)]
struct AddressEntry {
    #[serde(rename = "stateOverride", default)]
    state_override: HashMap<Address, AccountState>,
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
const TITAN_URL: &str = "wss://eu.rpc.titanbuilder.xyz/ws/pamm_quote_stream";
/// Environment variable that overrides the default endpoint when no custom URL is passed.
const TITAN_URL_ENV: &str = "TITAN_PAMM_STREAM_URL";

/// FermiSwap protocol system identifier as indexed by Tycho.
const FERMISWAP_PROTOCOL_SYSTEM: &str = "vm:fermiswap";
/// Kipseli protocol system identifier as indexed by Tycho.
const KIPSELI_PROTOCOL_SYSTEM: &str = "vm:kipseli";
/// bopAMM protocol system identifier as indexed by Tycho.
const BOPAMM_PROTOCOL_SYSTEM: &str = "vm:bopamm";
/// Tempest (Flowdesk) protocol system identifier as indexed by Tycho.
const TEMPEST_PROTOCOL_SYSTEM: &str = "vm:tempest";

/// FermiSwap venue aliases on Titan's quote stream: the FermiSwap oracle and the FermiSwapper
/// router. Both keys carry the same registry diffs; either serves the protocol.
const FERMISWAP_VENUES: &[&str] =
    &["0xb1076fe3ab5e28005c7c323bac5ac06a680d452e", "0x5979458912f80b96d30d4220af8e2e4925a33320"];
/// Kipseli venue aliases on Titan's quote stream: the Kipseli oracle and the KipseliPropAMMWrapper
/// router, both carrying the same registry diffs.
const KIPSELI_VENUES: &[&str] =
    &["0x5cdbe59400cc2efdcc2b54acca4a99fe00dd588c", "0x71e790dd841c8a9061487cb3e78c288e75ce0b3d"];
/// bopAMM (Bebop) venue on Titan's quote stream: a self-storage pAMM whose frames write the
/// maker's signed quote into the venue's own storage.
const BOPAMM_VENUES: &[&str] = &["0x160141a205f5ddcf096ba3f48b7ed21eb52c62ea"];
/// Tempest venue on Titan's quote stream: the `TempestEth` router, which is also the `target` the
/// maker writes its lanes against in the shared `PrioUpdateRegistry`. Confirmed against live
/// frames — the venue keys the stream publishes carry a `stateDiff` on the registry for this
/// address, which is what the override snapshot consumes.
const TEMPEST_VENUES: &[&str] = &["0x00000003f1ec2379e79f58e12ec6c4f51ee92149"];

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
/// snapshot's [`OverrideSnapshot::expires_at`] from the frame's wall-clock `timestamp`.
const TITAN_QUOTE_TTL_SECS: u64 = 12;

/// Unix timestamp of the Ethereum beacon chain genesis (2020-12-01 12:00:23 UTC). A beacon slot's
/// timestamp is `BEACON_GENESIS_TIMESTAMP + slot * SECONDS_PER_SLOT`.
const BEACON_GENESIS_TIMESTAMP: u64 = 1_606_824_023;

/// Post-merge Ethereum slot duration in seconds.
const SECONDS_PER_SLOT: u64 = 12;

/// Longest a registry lane timestamp may lie ahead of the frame's wall-clock `timestamp` and still
/// be accepted by the lane-extraction fallback. A genuine lane timestamp is the pending block's
/// timestamp, i.e. at most one slot ahead of the instant the quote was built; two slots leaves
/// margin for clock skew. Anything further out is packed venue data that merely looks like a
/// timestamp.
const MAX_LANE_TIMESTAMP_LEAD_SECS: u64 = 2 * SECONDS_PER_SLOT;

/// Returns the pAMM `protocol_system`s this provider knows how to serve.
fn known_pamm_protocols() -> &'static [&'static str] {
    &[
        FERMISWAP_PROTOCOL_SYSTEM,
        KIPSELI_PROTOCOL_SYSTEM,
        BOPAMM_PROTOCOL_SYSTEM,
        TEMPEST_PROTOCOL_SYSTEM,
    ]
}

/// Default override providers contributed by Titan, keyed by `protocol_system`.
///
/// Returns one shared [`TitanProvider`] mapped under every known pAMM `protocol_system` yielded by
/// `protocols` (the ones the caller wants a built-in default for). Takes an owned iterator so the
/// caller can forward its strings without cloning. The provider is spawned only when at least one
/// such venue needs serving — so an empty or fully-overridden `protocols` opens no connection. The
/// single provider is shared across its protocols via `Arc`, not duplicated.
pub fn default_providers(
    protocols: impl IntoIterator<Item = String>,
) -> HashMap<String, Arc<dyn StateOverrideProvider>> {
    let needed: Vec<&'static str> = protocols
        .into_iter()
        .filter_map(|protocol| {
            known_pamm_protocols()
                .iter()
                .copied()
                .find(|&known| known == protocol.as_str())
        })
        .collect();
    if needed.is_empty() {
        return HashMap::new();
    }
    // Auto-registration endpoint: the `TITAN_PAMM_STREAM_URL` env override if set, else the
    // built-in default. Consumers needing a different endpoint can register their own
    // `TitanProvider::spawn(url, protocols)` via `with_override_provider` instead.
    let url = std::env::var(TITAN_URL_ENV).unwrap_or_else(|_| TITAN_URL.to_string());
    let provider: Arc<dyn StateOverrideProvider> = Arc::new(TitanProvider::spawn(url, &needed));
    needed
        .into_iter()
        .map(|protocol| (protocol.to_string(), provider.clone()))
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
    /// Opens ONE WebSocket connection to `url` serving the given `protocols` and spawns the
    /// background reconnect/parse task that keeps their per-protocol snapshots up to date. Only the
    /// requested protocols get a channel, and the task only parses their venues.
    ///
    /// Public so a consumer can point a provider at a custom endpoint and register it via
    /// [`ProtocolStreamBuilder::with_override_provider`](crate::evm::stream::ProtocolStreamBuilder::with_override_provider),
    /// which takes precedence over the default registry.
    ///
    /// Returns immediately; the connection is established and maintained in the background, so a
    /// transient WebSocket failure never blocks or crashes the caller.
    pub fn spawn(url: String, protocols: &[&str]) -> Self {
        let mut receivers = HashMap::new();
        let mut senders = Vec::new();
        for &protocol in protocols {
            let Some(venues) = Self::venues_for_protocol(protocol) else {
                warn!("Unknown protocol: {protocol}");
                continue;
            };
            let (tx, rx) = watch::channel(OverrideSnapshot::default());
            receivers.insert(protocol.to_string(), rx);
            senders.push((venues, tx));
        }
        tokio::spawn(Self::run(url, senders));
        Self { receivers }
    }

    /// Maintains the Titan WebSocket connection for the lifetime of the process.
    ///
    /// Connects, reads messages, and publishes each parsed [`OverrideSnapshot`] on the matching
    /// protocol channel (a frame keyed by any of the protocol's venue aliases publishes). Any
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
                                        for (venues, sender) in &senders {
                                            match Self::extract_any_venue(&message, venues) {
                                                // A send error means every receiver has been
                                                // dropped; the check after this loop stops the
                                                // task, so ignoring it here is safe.
                                                Ok(Some(snapshot)) => {
                                                    let _ = sender.send(snapshot);
                                                }
                                                // No alias present in this frame — nothing to do.
                                                Ok(None) => {}
                                                // Malformed payload for this venue: keep the
                                                // connection and last good snapshot, but surface
                                                // it.
                                                Err(e) => {
                                                    warn!(?venues, error = %e, "Failed to extract Titan venue snapshot");
                                                }
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

    /// Maps a known pAMM `protocol_system` to its Titan venue aliases, or `None` if unknown.
    ///
    /// Every alias of a protocol streams the same maker feed (e.g. its oracle and router
    /// addresses); subscribing to all of them keeps the protocol served when one key disappears
    /// from the stream, which venue deployments have been observed doing within hours.
    /// Titan/venue specifics (the address mapping) live only here.
    fn venues_for_protocol(protocol_system: &str) -> Option<Vec<Address>> {
        let raw = match protocol_system {
            FERMISWAP_PROTOCOL_SYSTEM => FERMISWAP_VENUES,
            KIPSELI_PROTOCOL_SYSTEM => KIPSELI_VENUES,
            BOPAMM_PROTOCOL_SYSTEM => BOPAMM_VENUES,
            TEMPEST_PROTOCOL_SYSTEM => TEMPEST_VENUES,
            _ => return None,
        };
        let venues = raw
            .iter()
            .map(|venue| {
                venue
                    .parse()
                    .unwrap_or_else(|e| panic!("hardcoded venue address {venue} must parse: {e}"))
            })
            .collect();
        Some(venues)
    }

    /// Extracts the snapshot of the first alias in `venues` present in the frame, or `Ok(None)`
    /// if none is. A frame carries a single top-level venue key, so at most one alias matches.
    fn extract_any_venue(
        message: &TitanMessage,
        venues: &[Address],
    ) -> Result<Option<OverrideSnapshot>, String> {
        for venue in venues {
            if let Some(snapshot) = Self::extract_venue(message, venue)? {
                return Ok(Some(snapshot));
            }
        }
        Ok(None)
    }

    /// Extracts a single venue's snapshot (`stateDiff` overrides + resolved block environment)
    /// from an already-parsed Titan stream frame (deserialized once by the caller, then queried per
    /// venue).
    ///
    /// Returns `Ok(None)` if the venue is absent from the frame. The block timestamp is resolved
    /// to the pending block's timestamp (see
    /// [`pending_block_timestamp`](Self::pending_block_timestamp)) so the registry's
    /// oracle-staleness guard sees the overridden prices as current, and `expires_at` is set to the
    /// frame's wall-clock `timestamp` plus [`TITAN_QUOTE_TTL_SECS`] so the pool stops using it once
    /// it goes stale.
    fn extract_venue(
        message: &TitanMessage,
        venue: &Address,
    ) -> Result<Option<OverrideSnapshot>, String> {
        // Each frame carries one top-level entry per venue address. Absent => nothing to apply.
        let venue_key = format!("{venue:#x}");
        let Some(raw) = message.overrides.get(&venue_key) else {
            return Ok(None);
        };
        let entry: AddressEntry = serde_json::from_value(raw.clone())
            .map_err(|e| format!("invalid venue override: {e}"))?;

        let block_number = message.block_number;

        // We only consume `stateDiff` storage; balance / nonce / code are ignored. Slots and values
        // were parsed into `U256` by serde, so we just drop accounts with no overrides.
        let mut storage: VenueOverrides = HashMap::new();
        for (account, account_override) in entry.state_override {
            if !account_override.state_diff.is_empty() {
                storage.insert(account, account_override.state_diff);
            }
        }

        let block_timestamp = Self::pending_block_timestamp(message, &storage);

        // Expire the snapshot one block time after Titan built it, using the frame's nanosecond
        // wall-clock `timestamp`. Once past this the pool drops the override and reverts to Tycho's
        // indexed state, so a stalled stream can never keep serving prices indefinitely.
        let expires_at = message
            .timestamp
            .map(|ns| ns / 1_000_000_000 + TITAN_QUOTE_TTL_SECS);

        Ok(Some(OverrideSnapshot {
            block_number,
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

    /// Returns the unix-seconds timestamp of the pending block the frame's quote targets, or
    /// `None` if the frame carries no timestamp signal at all.
    ///
    /// Resolution order:
    /// 1. The frame's beacon `slot` field (`BEACON_GENESIS_TIMESTAMP + slot * 12`) — the
    ///    authoritative source, and the one used by Titan's reference taker
    ///    (<https://github.com/gattaca-com/propamm>, `quoter.py`) and the LambdaClass propAMM SDK
    ///    (<https://github.com/lambdaclass/propamm-router-contracts>). Rejected (falling through
    ///    to the next sources) if the multiplication overflows or the derived value is implausible
    ///    against the frame's wall clock — see [`slot_timestamp`](Self::slot_timestamp).
    /// 2. The registry lane timestamps embedded in the storage overrides (see
    ///    [`max_lane_timestamp`](Self::max_lane_timestamp)): Titan stamps lanes with the pending
    ///    block's timestamp, so a genuine lane timestamp equals the slot-derived value. Only
    ///    accepted when it lies within [`MAX_LANE_TIMESTAMP_LEAD_SECS`] ahead of the frame's wall
    ///    clock, so packed venue data that merely looks like a timestamp (e.g. bopAMM's signed
    ///    quote encoding) can never be injected as the block timestamp. Without a wall clock to
    ///    validate against, a lane timestamp is never trusted.
    /// 3. The frame's nanosecond wall-clock `timestamp`. Up to one slot earlier than the pending
    ///    block, so registry-backed venues may still reject it as stale, but it is the best
    ///    remaining approximation.
    fn pending_block_timestamp(message: &TitanMessage, storage: &VenueOverrides) -> Option<u64> {
        let wall_clock = message
            .timestamp
            .map(|ns| ns / 1_000_000_000);
        if let Some(slot_ts) = Self::slot_timestamp(message.slot, wall_clock) {
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
    /// frame's wall clock before trusting it.
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

    /// The known-protocols list must contain the FermiSwap protocol so the builder auto-registers
    /// the provider.
    #[test]
    fn known_pamm_protocols_contains_fermiswap() {
        assert!(known_pamm_protocols().contains(&FERMISWAP_PROTOCOL_SYSTEM));
    }

    #[test]
    fn venues_for_protocol_resolves_all_known_aliases() {
        let cases = [
            (FERMISWAP_PROTOCOL_SYSTEM, FERMISWAP_VENUES),
            (KIPSELI_PROTOCOL_SYSTEM, KIPSELI_VENUES),
            (BOPAMM_PROTOCOL_SYSTEM, BOPAMM_VENUES),
            (TEMPEST_PROTOCOL_SYSTEM, TEMPEST_VENUES),
        ];
        for (protocol, expected) in cases {
            let venues = TitanProvider::venues_for_protocol(protocol).expect("venues must resolve");
            let expected: Vec<Address> = expected
                .iter()
                .map(|raw| raw.parse().unwrap())
                .collect();
            assert_eq!(venues, expected, "venue mismatch for {protocol}");
            assert!(!venues.is_empty(), "every protocol needs at least one venue");
        }
    }

    #[test]
    fn venues_for_protocol_returns_none_for_unknown() {
        assert!(TitanProvider::venues_for_protocol("vm:unknown").is_none());
    }

    /// Sample message from the Titan docs (<https://docs.titanbuilder.xyz/propamms/takers>).
    const SAMPLE_MESSAGE: &str = r#"{
        "slot": 14285824,
        "blockNumber": 25051224,
        "timestamp": 1778253913749564761,
        "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e": {
            "stateOverride": {
                "0x1038c87766e36d1925889e6f26d10e0012d50fed": {
                    "balance": "0x0",
                    "nonce": "0x1",
                    "stateDiff": {
                        "0x156b1d71de08fed89d0fce38008e2b9d03a8998077e394b20597ef3d148f5ebc": "0x000000000000000000000000000000000000000000000000000000017e405801"
                    }
                }
            }
        }
    }"#;

    #[test]
    fn extract_venue_reads_state_diff() {
        let venue = FERMISWAP_VENUES[0]
            .parse::<Address>()
            .unwrap();
        let message = serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE).expect("valid JSON");
        let snapshot = TitanProvider::extract_venue(&message, &venue)
            .expect("extract must succeed")
            .expect("venue is present");

        assert_eq!(snapshot.block_number, Some(25051224));
        // Pending block timestamp derived from the beacon slot: 1606824023 + 14285824 * 12.
        assert_eq!(snapshot.block_timestamp, Some(1778253911));
        // 1778253913749564761 ns -> 1778253913 s, plus the 12s quote TTL.
        assert_eq!(snapshot.expires_at, Some(1778253913 + TITAN_QUOTE_TTL_SECS));
        // Titan overrides are only intermittently applicable, so pools must fall back.
        assert_eq!(snapshot.failure_policy, FailurePolicy::FallbackToIndexedState);

        let account = "0x1038c87766e36d1925889e6f26d10e0012d50fed"
            .parse::<Address>()
            .unwrap();
        let slot = "0x156b1d71de08fed89d0fce38008e2b9d03a8998077e394b20597ef3d148f5ebc"
            .parse::<U256>()
            .unwrap();
        let value = "0x000000000000000000000000000000000000000000000000000000017e405801"
            .parse::<U256>()
            .unwrap();
        assert_eq!(
            snapshot
                .storage
                .get(&account)
                .and_then(|s| s.get(&slot)),
            Some(&value)
        );
    }

    #[test]
    fn extract_venue_returns_none_for_absent_venue() {
        let venue = KIPSELI_VENUES[0]
            .parse::<Address>()
            .unwrap();
        let message = serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE).expect("valid JSON");
        assert!(TitanProvider::extract_venue(&message, &venue)
            .unwrap()
            .is_none());
    }

    /// The sample message is keyed by the FermiSwap oracle (`FERMISWAP_VENUES[0]`), so listing the
    /// router alias first must fall through to the oracle rather than give up.
    #[test]
    fn extract_any_venue_falls_back_to_a_later_alias() {
        let message = serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE).expect("valid JSON");
        let aliases = [
            FERMISWAP_VENUES[1]
                .parse::<Address>()
                .unwrap(),
            FERMISWAP_VENUES[0]
                .parse::<Address>()
                .unwrap(),
        ];
        let snapshot = TitanProvider::extract_any_venue(&message, &aliases)
            .expect("extract must succeed")
            .expect("oracle alias is present in the frame");
        assert_eq!(snapshot.block_number, Some(25051224));
    }

    #[test]
    fn extract_any_venue_returns_none_when_no_alias_matches() {
        let message = serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE).expect("valid JSON");
        let aliases: Vec<Address> = KIPSELI_VENUES
            .iter()
            .map(|raw| raw.parse().unwrap())
            .collect();
        assert!(TitanProvider::extract_any_venue(&message, &aliases)
            .unwrap()
            .is_none());
    }

    fn message(slot: Option<u64>, timestamp: Option<u64>) -> TitanMessage {
        TitanMessage { slot, block_number: Some(25445022), timestamp, overrides: HashMap::new() }
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
        let message = message(Some(14681135), Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(&message, &lane_overrides(1_800_000_000)),
            Some(PENDING_BLOCK_TS)
        );
    }

    #[test]
    fn pending_block_timestamp_rejects_implausible_slot() {
        // A slot decoding to a timestamp far from the frame's wall clock is corrupt stream data;
        // resolution must fall through to the remaining sources (here: the wall clock).
        let message = message(Some(1), Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(&message, &HashMap::new()),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_survives_overflowing_slot() {
        // `u64::MAX * SECONDS_PER_SLOT` must not panic or wrap; it falls through to the wall
        // clock.
        let message = message(Some(u64::MAX), Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(&message, &HashMap::new()),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_ignores_lane_without_wall_clock() {
        // Without a wall clock there is nothing to validate a lane timestamp against, so even a
        // plausible-looking one must not be trusted.
        let message = message(None, None);
        assert!(TitanProvider::pending_block_timestamp(
            &message,
            &lane_overrides(PENDING_BLOCK_TS)
        )
        .is_none());
    }

    #[test]
    fn pending_block_timestamp_backs_up_to_lane_timestamps() {
        let message = message(None, Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(&message, &lane_overrides(PENDING_BLOCK_TS)),
            Some(PENDING_BLOCK_TS)
        );
    }

    #[test]
    fn pending_block_timestamp_rejects_implausible_lane_timestamps() {
        // Top bytes of bopAMM's packed quote data decode to 1557751927 (2019) — far outside the
        // wall-clock window, so the fallback must skip it and use the wall clock.
        let message = message(None, Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(&message, &lane_overrides(1_557_751_927)),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_falls_back_to_wall_clock() {
        let message = message(None, Some(WALL_CLOCK_NS));
        assert_eq!(
            TitanProvider::pending_block_timestamp(&message, &HashMap::new()),
            Some(1782997641)
        );
    }

    #[test]
    fn pending_block_timestamp_none_without_any_signal() {
        let message = message(None, None);
        assert!(TitanProvider::pending_block_timestamp(&message, &HashMap::new()).is_none());
    }
}
