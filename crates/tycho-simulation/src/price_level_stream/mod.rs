//! Titan pAMM price level stream integration.
//!
//! Titan Builder exposes a WebSocket stream of per-pair quote books (simulated price levels) for
//! the pAMMs it builds blocks with (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>). This module turns those
//! frames directly into [`Update`](crate::protocol::models::Update)s ready for consumption.
//!
//! # The stream and what it admits
//!
//! A frame carries a flat list of books, one per venue and pair. A book prices its pair in human
//! units — a decimal price and a cumulative volume per level — and names both its assets and its
//! venue by address:
//!
//! - A pair is priced only if both of the book's token addresses are present in the provided token
//!   metadata, which supplies the decimals its ladders are converted with. The address is the
//!   identity: the book's symbols are read for the logs alone, so a token calling itself `WBTC`
//!   neither captures another asset's quotes nor keeps it from being served.
//! - A venue is identified by the canonical public identity of its streamed `quoteAddress` (see
//!   [`PAMM_ADDRESS_ALIASES`](config::PAMM_ADDRESS_ALIASES)) — see [Identity and
//!   families](#identity-and-families).
//!
//! Levels are converted to the integer ladders the states serve by applying the resolved tokens'
//! decimals; a side whose levels do not state a usable price function is rejected whole, leaving
//! that one direction unquotable, never silently repaired.
//!
//! # Freshness contract
//!
//! Frames are best effort, not complete snapshots: a venue or a pair can be absent from one
//! frame and present in the next, so absence is never read as retirement. Instead the stream
//! serves a component only while an accepted frame carried it within the last `stale_after`
//! (default 24 s, two slots, see [`stale_after`](stream::PriceLevelStreamBuilder::stale_after)):
//!
//! - A frame is accepted only if its `ts` is younger than `stale_after`, not more than one slot in
//!   the future, not older than the newest accepted frame (equal is fine: Titan re-emits within a
//!   build round), and its block neither regresses nor jumps more than one block per elapsed slot
//!   plus 2. Rejected frames change nothing. The frame's beacon `slot` carries no freshness
//!   decision, so a frame without it is judged exactly like one carrying it.
//! - A component no accepted frame has carried for `stale_after` turns stale and is emitted in
//!   `removed_pairs`, together with every other component that turned stale at the same instant.
//!   The next accepted frame carrying it re-adds it in `new_pairs`. Silence, disconnects,
//!   keepalive-only or unparsable traffic, and replayed frames all end in this stale removal.
//! - Every emitted state also refuses to quote once its frame's `ts` is one slot
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
//! Updates are stamped with the block their frame targets (a removal with the newest accepted
//! block); the number never decreases while something is served, but once every component has
//! turned stale the next accepted frame is judged as a first frame and may carry a lower block,
//! which is how the stream recovers from a frame with an implausible block.
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
//! A venue's identity is its **canonical public identity**: the address a swap is sent to. Titan
//! streams some venues under a different `quoteAddress`, so every boundary normalizes through
//! [`PAMM_ADDRESS_ALIASES`](config::PAMM_ADDRESS_ALIASES) — incoming books as well as the
//! addresses passed to [`add_pamm`](stream::PriceLevelStreamBuilder::add_pamm) and
//! [`deny_pamm`](stream::PriceLevelStreamBuilder::deny_pamm), either of which may name either
//! address. Component ids, venue names, custom gas costs, denials and the
//! [`PAMM_ADDRESS_ATTRIBUTE`](stream::PAMM_ADDRESS_ATTRIBUTE) therefore always carry the public
//! identity, never the streamed one. A venue the alias table does not name streams under its
//! public identity already, so for it the two coincide.
//!
//! By default components are emitted under `fallback:{pamm}` instead: tycho-execution routes
//! their swaps through `TychoFallbackRouter`, which retries a reverted pAMM swap on the fallback
//! pool the solver names.
//! [`without_fallback_router`](stream::PriceLevelStreamBuilder::without_fallback_router) keeps
//! every venue on the direct `pricelevelstream:` path.
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
//! served components per registered venue, stale removals, serving state, reconnects, and the
//! books and book sides it declined to serve); a consumer that installs a `metrics` recorder
//! receives them with no further setup. Per-venue series start at zero for every registered
//! venue, every `reason` of the two book counters starts at zero, and no label ever carries a
//! value from the stream, except the venue address itself when a pAMM is served under
//! auto-detection. In particular a token address is never a label: an unserved asset is
//! named in the DEBUG log that accompanies its counter, not in the series.
//!
//! Label values and gauge encodings, for dashboards and alerts:
//! - `price_level_stream_frame_age_seconds`: a histogram of the wall-clock age of every accepted
//!   frame at acceptance, negative for a frame stamped ahead of the local clock. Titan's lag plus
//!   delivery delay and clock skew; above one slot the frame's states refuse to quote.
//! - `price_level_stream_frames_rejected_total{reason}`: `parse_error`, `too_old`, `in_future`,
//!   `out_of_order`, `block_regression`, `block_jump`.
//! - `price_level_stream_skipped_books_total{reason}`: `malformed_address` (an address is not 20
//!   bytes), `unadmitted_token` (an address is absent from the token metadata), `one_asset_pair`
//!   (both addresses name one asset). One increment per book left unpriced; a steady rate means an
//!   asset the venue quotes is going unserved.
//! - `price_level_stream_rejected_sides_total{reason}`: `unsupported_level`, `non_finite_amount`,
//!   `zero_amount`, `conflicting_knots`. One increment per book side dropped whole, which leaves
//!   that one trade direction unquotable while the opposite one is served.
//! - `price_level_stream_reconnects_total{reason}`: `idle_timeout`, `ended`, `closed`,
//!   `read_error`, `connect_failed`, `connect_timeout`.
//! - `price_level_stream_serving_state`: 0 = unserved, 1 = serving.
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
mod convert;
pub mod state;
pub mod stream;
mod telemetry;
#[cfg(test)]
mod test_support;
mod titan;
mod tracker;

/// The post-merge Ethereum slot. Every freshness window of the stream is a multiple of it.
const SLOT: Duration = Duration::from_secs(12);
