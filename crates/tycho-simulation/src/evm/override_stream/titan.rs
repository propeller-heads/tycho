//! Concrete [`StateOverrideProvider`] backed by the Titan pAMM state-override stream.
//!
//! Connects to the Titan `pamm_quote_stream` WebSocket and tracks the latest per-block VM storage
//! overrides published for the known pAMM venues (e.g. FermiSwap), exposing them through the
//! generic [`StateOverrideProvider`] trait so the decoder can inject them into the matching
//! `EVMPoolState` instances before simulation. See
//! <https://docs.titanbuilder.xyz/propamms/takers>.
//!
//! All Titan/Fermi-specific knowledge (venue addresses, lane-timestamp resolution, endpoint URL)
//! lives only in this module; the rest of the override machinery is protocol-agnostic.

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

use super::{OverrideSnapshot, StateOverrideProvider};

/// Storage overrides keyed by contract address, then by storage slot.
type VenueOverrides = HashMap<Address, HashMap<U256, U256>>;

/// A parsed Titan quote-stream frame.
///
/// `blockNumber` and `timestamp` (the nanosecond wall-clock instant Titan built the quote) are
/// consumed at this level; every other top-level key (venue addresses, `slot`, and any future
/// fields) lands in `overrides` as raw JSON. Only the venue keys we look up are then deserialized
/// into [`AddressEntry`], so an upstream schema change that adds top-level fields never breaks
/// parsing.
#[derive(Debug, Deserialize)]
struct TitanMessage {
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

/// FermiSwap pAMM venue address on Titan's quote stream.
const FERMISWAP_VENUE: &str = "0xb1076fe3ab5e28005c7c323bac5ac06a680d452e";
/// Kipseli pAMM venue address on Titan's quote stream.
const KIPSELI_VENUE: &str = "0x5cdbe59400cc2efdcc2b54acca4a99fe00dd588c";
/// bopAMM pAMM venue address on Titan's quote stream.
const BOPAMM_VENUE: &str = "0x160141a205f5ddcf096ba3f48b7ed21eb52c62ea";

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

/// Returns the pAMM `protocol_system`s this provider knows how to serve.
fn known_pamm_protocols() -> &'static [&'static str] {
    &[FERMISWAP_PROTOCOL_SYSTEM, KIPSELI_PROTOCOL_SYSTEM, BOPAMM_PROTOCOL_SYSTEM]
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
            let Some(venue) = Self::venue_for_protocol(protocol) else {
                warn!("Unknown protocol: {protocol}");
                continue;
            };
            let (tx, rx) = watch::channel(OverrideSnapshot::default());
            receivers.insert(protocol.to_string(), rx);
            senders.push((venue, tx));
        }
        tokio::spawn(Self::run(url, senders));
        Self { receivers }
    }

    /// Maintains the Titan WebSocket connection for the lifetime of the process.
    ///
    /// Connects, reads messages, and publishes each parsed [`OverrideSnapshot`] on the matching
    /// venue channel. Any disconnect, read error, server-side close, or idle timeout drops back to
    /// the reconnect loop, which retries forever with capped exponential backoff. The backoff is
    /// reset only after a message is actually received, so a socket that connects and immediately
    /// drops still backs off instead of busy-looping. This task never returns and never panics:
    /// every failure mode is logged and retried.
    async fn run(url: String, senders: Vec<(Address, watch::Sender<OverrideSnapshot>)>) {
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
                                        for (venue, sender) in &senders {
                                            match Self::extract_venue(&message, venue) {
                                                // A send error means every receiver has been
                                                // dropped; the check after this loop stops the
                                                // task, so ignoring it here is safe.
                                                Ok(Some(snapshot)) => {
                                                    let _ = sender.send(snapshot);
                                                }
                                                // Venue not present in this frame — nothing to do.
                                                Ok(None) => {}
                                                // Malformed payload for this venue: keep the
                                                // connection and last good snapshot, but surface
                                                // it.
                                                Err(e) => {
                                                    warn!(%venue, error = %e, "Failed to extract Titan venue snapshot");
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

    /// Maps a known pAMM `protocol_system` to its Titan venue address, or `None` if unknown.
    ///
    /// Titan/Fermi specifics (the venue address mapping) live only here.
    fn venue_for_protocol(protocol_system: &str) -> Option<Address> {
        let raw = match protocol_system {
            FERMISWAP_PROTOCOL_SYSTEM => FERMISWAP_VENUE,
            KIPSELI_PROTOCOL_SYSTEM => KIPSELI_VENUE,
            BOPAMM_PROTOCOL_SYSTEM => BOPAMM_VENUE,
            _ => return None,
        };
        raw.parse().ok()
    }

    /// Extracts a single venue's snapshot (`stateDiff` overrides + resolved block environment)
    /// from an already-parsed Titan stream frame (deserialized once by the caller, then queried per
    /// venue).
    ///
    /// Returns `Ok(None)` if the venue is absent from the frame. The block timestamp is resolved
    /// from the freshest lane update timestamp (see
    /// [`max_lane_timestamp`](Self::max_lane_timestamp)) so the pool's oracle-staleness guard
    /// sees the overridden prices as current, and `expires_at` is set to the frame's wall-clock
    /// `timestamp` plus [`TITAN_QUOTE_TTL_SECS`] so the pool stops using it once it goes stale.
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

        // Resolve the block timestamp from the freshest lane update, not the wall-clock
        // `timestamp` field, so the registry's oracle-staleness guard treats the streamed prices
        // as current.
        let block_timestamp = Self::max_lane_timestamp(&storage);

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
        }))
    }

    /// Returns the newest lane update timestamp (in seconds) across all overridden slots.
    ///
    /// FermiSwap registry lanes pack a uint32 update timestamp in the first 4 bytes of the slot
    /// value. This extracts that prefix from every overridden slot and returns the maximum value
    /// that looks like a plausible unix timestamp, or `None` if there are none. The result is used
    /// as the resolved `block_timestamp` so the registry's staleness guard treats the streamed
    /// oracle prices as current.
    fn max_lane_timestamp(overrides: &VenueOverrides) -> Option<u64> {
        // Lane slots pack a uint32 unix-seconds timestamp in the top 4 bytes (`value >> 224`).
        // Non-lane slots have other (usually zero) top bytes, so we keep only values inside a
        // plausible window to discard them.
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
    fn venue_for_protocol_resolves_all_known_venues() {
        let cases = [
            (FERMISWAP_PROTOCOL_SYSTEM, FERMISWAP_VENUE),
            (KIPSELI_PROTOCOL_SYSTEM, KIPSELI_VENUE),
            (BOPAMM_PROTOCOL_SYSTEM, BOPAMM_VENUE),
        ];
        for (protocol, expected) in cases {
            assert_eq!(
                TitanProvider::venue_for_protocol(protocol).expect("venue must resolve"),
                expected.parse::<Address>().unwrap(),
                "venue mismatch for {protocol}",
            );
        }
    }

    #[test]
    fn venue_for_protocol_returns_none_for_unknown() {
        assert!(TitanProvider::venue_for_protocol("vm:unknown").is_none());
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
        let venue = FERMISWAP_VENUE
            .parse::<Address>()
            .unwrap();
        let message = serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE).expect("valid JSON");
        let snapshot = TitanProvider::extract_venue(&message, &venue)
            .expect("extract must succeed")
            .expect("venue is present");

        assert_eq!(snapshot.block_number, Some(25051224));
        // 1778253913749564761 ns -> 1778253913 s, plus the 12s quote TTL.
        assert_eq!(snapshot.expires_at, Some(1778253913 + TITAN_QUOTE_TTL_SECS));

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
        let venue = KIPSELI_VENUE
            .parse::<Address>()
            .unwrap();
        let message = serde_json::from_str::<TitanMessage>(SAMPLE_MESSAGE).expect("valid JSON");
        assert!(TitanProvider::extract_venue(&message, &venue)
            .unwrap()
            .is_none());
    }

    #[test]
    fn max_lane_timestamp_picks_newest_plausible_top_word() {
        let addr = FERMISWAP_VENUE
            .parse::<Address>()
            .unwrap();
        // Two lane slots with the timestamp packed in the top 4 bytes, plus a non-lane slot
        // whose top bytes are zero (must be ignored).
        let older = U256::from(1_700_000_000u64) << 224;
        let newer = U256::from(1_800_000_000u64) << 224;
        let non_lane = U256::from(42u64);
        let overrides = HashMap::from([(
            addr,
            HashMap::from([
                (U256::from(0u64), older),
                (U256::from(1u64), newer),
                (U256::from(2u64), non_lane),
            ]),
        )]);
        assert_eq!(TitanProvider::max_lane_timestamp(&overrides), Some(1_800_000_000));
    }

    #[test]
    fn max_lane_timestamp_none_when_no_plausible_values() {
        let addr = FERMISWAP_VENUE
            .parse::<Address>()
            .unwrap();
        let overrides =
            HashMap::from([(addr, HashMap::from([(U256::from(0u64), U256::from(7u64))]))]);
        assert!(TitanProvider::max_lane_timestamp(&overrides).is_none());
    }
}
