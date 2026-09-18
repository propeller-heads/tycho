//! Titan pAMM price level stream integration.
//!
//! Titan Builder exposes a WebSocket stream of per-pair quote ladders (simulated price levels)
//! for the pAMMs it builds blocks with (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>). This module turns those
//! frames directly into [`Update`](crate::protocol::models::Update)s ready for consumption.
//!
//! # Freshness contract
//!
//! Frames are best effort, not complete snapshots: a venue or a pair can be absent from one
//! frame and present in the next, so absence is never read as retirement. Instead the stream
//! serves a component only while an accepted frame carried it within the last `stale_after`
//! (default 24 s, two slots, see [`stale_after`](stream::PriceLevelStreamBuilder::stale_after)):
//!
//! - A frame is accepted only if its wire `timestamp` is younger than `stale_after`, not more than
//!   one slot in the future, not older than the newest accepted frame (equal is fine: Titan
//!   re-emits within a build round), and its block neither regresses nor jumps more than one block
//!   per elapsed slot plus 2. Rejected frames change nothing.
//! - A component no accepted frame has carried for `stale_after` turns stale and is emitted in
//!   `removed_pairs`, together with every other component that turned stale at the same instant.
//!   The next accepted frame carrying it re-adds it in `new_pairs`. Silence, disconnects,
//!   keepalive-only or unparsable traffic, and replayed frames all end in this stale removal.
//! - Every emitted state also refuses to quote once its frame's `timestamp` is one slot
//!   ([`QUOTE_TTL`](state::QUOTE_TTL)) old, so a ladder cannot be quoted past the block it targeted
//!   even before the removal arrives. The deadline is an `Instant` of this process and is never
//!   serialized. A consumer that quotes a state more than one slot after it arrived by design can
//!   opt out with [`without_quote_guard`](stream::PriceLevelStreamBuilder::without_quote_guard);
//!   the removal after `stale_after` still applies.
//!
//! Recovery is per component: a frame carrying a pair re-adds that pair, nothing more, and a
//! frame carrying one direction re-adds the pair with the other direction unquotable.
//! Consumers cannot tell a stale removal from a retired venue; both mean the component must not
//! be routed until it reappears in `new_pairs`. Removal and re-add are always separate updates.
//!
//! Quotes target the block currently being built, so every emitted update is marked as partial
//! and supersedes the previous one for the pairs it contains.
//!
//! # Identity and families
//!
//! Components are identified as `pricelevelstream:{pamm}`, where `{pamm}` is the configured
//! venue name (e.g. `pricelevelstream:fermiswap`) or, for auto-detected venues, the venue
//! address (e.g. `pricelevelstream:0x5979…`). The prefix keeps these components distinct from
//! those any other integration path may produce for the same venue (e.g. `vm:fermiswap`).
//!
//! Venues on Titan's PropAMMRouter whitelist are emitted under `propammfallback:{pamm}` instead:
//! tycho-execution routes their swaps through the router, which falls back to a single-hop
//! Uniswap V3 pool when the venue reverts. The whitelist is read through the node at
//! [`fallback_router_rpc_url`](stream::PriceLevelStreamBuilder::fallback_router_rpc_url) or
//! `RPC_URL`, each read bounded by a timeout, retried with backoff until it succeeds, and
//! re-read every
//! [`whitelist_refresh_interval`](stream::PriceLevelStreamBuilder::whitelist_refresh_interval).
//! Nothing is served until the first read succeeds, and
//! [`build`](stream::PriceLevelStreamBuilder::build) fails without a node URL or with one that
//! does not parse, so a misconfigured deployment never silently serves whitelisted venues under
//! the direct family, which has no Uniswap V3 fallback. A venue whose membership changes is
//! removed at once and re-added under its new family by the next frame carrying it.
//! [`without_fallback_router`](stream::PriceLevelStreamBuilder::without_fallback_router) skips
//! the read and keeps every venue on the direct path unconditionally.
//!
//! Distinct identifiers do not imply distinct liquidity, though: a venue served here may also be
//! integrated through another path, in which case the components of both paths price the same
//! underlying inventory. Consumers subscribing to multiple paths must expect such overlaps and
//! deduplicate by venue — e.g. via the
//! [`PAMM_ADDRESS_ATTRIBUTE`](stream::PAMM_ADDRESS_ATTRIBUTE) — wherever double-counting
//! matters, such as routing over the combined liquidity.
//!
//! # Observability
//!
//! The stream emits `price_level_stream_*` metrics through the `metrics` facade (frames
//! accepted and rejected by reason, the age of every accepted frame, last seen timestamp and
//! served components per registered venue, stale removals, serving state, reconnects, whitelist
//! reads); a consumer that installs a `metrics` recorder receives them with no further setup.
//! Per-venue series start at zero for every registered venue, and no label ever carries a value
//! from the wire, except the venue address itself when a pAMM is served under auto-detection.
//!
//! Label values and gauge encodings, for dashboards and alerts:
//! - `price_level_stream_frame_age_seconds`: a histogram of the wall-clock age of every accepted
//!   frame at acceptance. Titan's lag plus delivery delay; above one slot the frame's states
//!   refuse to quote.
//! - `price_level_stream_frames_rejected_total{reason}`: `parse_error`, `too_old`, `in_future`,
//!   `out_of_order`, `block_regression`, `block_jump`.
//! - `price_level_stream_reconnects_total{reason}`: `idle_timeout`, `ended`, `closed`,
//!   `read_error`, `connect_failed`, `connect_timeout`.
//! - `price_level_stream_whitelist_reads_total{outcome}`: `ok`, `error`.
//! - `price_level_stream_serving_state`: 0 = awaiting whitelist, 1 = unserved, 2 = serving.
//! - `venue` on `price_level_stream_last_seen_timestamp_seconds`,
//!   `price_level_stream_served_components` and `price_level_stream_stale_removals_total`: the
//!   registered venue name, or the address of an auto-detected venue.
//!
//! Entry point: [`PriceLevelStreamBuilder`](stream::PriceLevelStreamBuilder). Register the pAMMs
//! to serve — the known venues via
//! [`with_known_pamms`](stream::PriceLevelStreamBuilder::with_known_pamms), individual
//! [`PriceLevelStreamConfig`](config::PriceLevelStreamConfig)s via
//! [`add_pamm`](stream::PriceLevelStreamBuilder::add_pamm), or any streamed venue via
//! auto-detection — provide token metadata, and consume the resulting stream of
//! [`Update`](crate::protocol::models::Update)s.

use std::time::Duration;

pub mod config;
pub mod fallback_router;
pub mod state;
pub mod stream;
mod telemetry;
#[cfg(test)]
mod test_support;
mod titan;
mod tracker;

/// The post-merge Ethereum slot. Every freshness window of the stream is a multiple of it.
const SLOT: Duration = Duration::from_secs(12);

/// The delay before the retry after `attempt` consecutive failures: `2^attempt` seconds, capped
/// at `max_backoff`. Shared by the Titan reconnect and the whitelist read retry.
fn backoff(attempt: u32, max_backoff: Duration) -> Duration {
    let exponential = 2u64
        .checked_pow(attempt)
        .map(Duration::from_secs)
        .unwrap_or(Duration::MAX);
    exponential.min(max_backoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_up_to_the_cap() {
        let max_backoff = Duration::from_secs(32);
        assert_eq!(backoff(1, max_backoff), Duration::from_secs(2));
        assert_eq!(backoff(4, max_backoff), Duration::from_secs(16));
        assert_eq!(backoff(5, max_backoff), max_backoff);
        assert_eq!(backoff(100, max_backoff), max_backoff);
        // Exponent overflow must saturate to the cap rather than panic.
        assert_eq!(backoff(u32::MAX, max_backoff), max_backoff);
    }
}
