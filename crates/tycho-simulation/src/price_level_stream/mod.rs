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
//! frame and present in the next, so absence is never read as retirement. Instead every served
//! component is vouched for only as long as an accepted frame carried it within `stale_after`
//! (default 24 s, two block times, see
//! [`stale_after`](stream::PriceLevelStreamBuilder::stale_after)):
//!
//! - A frame is accepted only if its wire `timestamp` is younger than `stale_after`, not more than
//!   one slot in the future, not older than the newest accepted frame (equal is fine: Titan
//!   re-emits within a build round), and its block neither regresses nor jumps further than elapsed
//!   time allows. Rejected frames change nothing.
//! - A component no accepted frame has carried for `stale_after` is emitted in `removed_pairs`,
//!   together with every other component that expired at the same instant. The next accepted frame
//!   carrying it re-adds it in `new_pairs`. Silence, disconnects, keepalive-only or unparsable
//!   traffic, and replayed frames all end in this removal.
//! - Every emitted state also carries a one-block-time quote guard
//!   ([`QUOTE_TTL`](state::QUOTE_TTL)) anchored on its frame's `timestamp`, so a ladder cannot be
//!   quoted past the block it targeted even before the removal arrives. The guard is a live
//!   property and is never serialized.
//!
//! Recovery is per component: a frame carrying a pair re-adds that pair, nothing more, and a
//! frame carrying one direction re-adds the pair with the other direction unquotable.
//! Consumers cannot tell an expiry from a retired venue; both mean the component must not be
//! routed until it reappears in `new_pairs`. Removal and re-add are always separate updates.
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
//! Nothing is served until the first read succeeds, and without a node URL nothing is ever
//! served, so a misconfigured deployment never silently lands on the unfloored direct family.
//! A venue whose membership changes is removed at once and re-added under its new family by
//! the next frame carrying it.
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
//! accepted and rejected by reason, last seen timestamp and served components per registered
//! venue, stale removals, source state, reconnects, whitelist reads); a consumer with a
//! recorder installed sees them without wiring. Per-venue series start at zero for every
//! registered venue, and no label ever carries a value from the wire, except the venue address
//! itself when a pAMM is served under auto-detection.
//!
//! Entry point: [`PriceLevelStreamBuilder`](stream::PriceLevelStreamBuilder). Register the pAMMs
//! to serve — the known venues via
//! [`with_known_pamms`](stream::PriceLevelStreamBuilder::with_known_pamms), individual
//! [`PriceLevelStreamConfig`](config::PriceLevelStreamConfig)s via
//! [`add_pamm`](stream::PriceLevelStreamBuilder::add_pamm), or any streamed venue via
//! auto-detection — provide token metadata, and consume the resulting stream of
//! [`Update`](crate::protocol::models::Update)s.

pub mod config;
pub mod fallback_router;
pub mod state;
pub mod stream;
mod telemetry;
#[cfg(test)]
mod test_support;
mod titan;
mod tracker;
