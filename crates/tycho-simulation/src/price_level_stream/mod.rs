//! Titan pAMM price level stream integration.
//!
//! Titan Builder exposes two pAMM WebSockets. One streams per-pair quote books (simulated price
//! levels) for the pAMMs it builds blocks with (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>), and this module turns those
//! messages directly into [`Update`](crate::protocol::models::Update)s ready for consumption. The
//! other, the quote stream, carries the account state Titan simulated each venue on to price
//! those books. This module attaches the matching part of it to each state as simulation
//! overrides; see [Simulation overrides](#simulation-overrides). A consumer that never simulates a
//! swap with `eth_call` can turn the quote stream off with [`without_simulation_overrides`].
//!
//! # The stream and what it admits
//!
//! A message carries a flat list of books, one per venue and pair. A book prices its pair in human
//! units — a decimal price and a cumulative volume per level — and names both its assets and its
//! venue by address:
//!
//! - A pair is priced only if both of the book's token addresses are present in the provided token
//!   metadata, which supplies the decimals its price levels are converted with. The address is the
//!   identity: the book's symbols are read for the logs alone, so a token calling itself `WBTC`
//!   neither captures another asset's quotes nor keeps it from being served.
//! - A venue is identified by the canonical public identity of its streamed `quoteAddress` (see
//!   [`PAMM_ADDRESS_ALIASES`](config::PAMM_ADDRESS_ALIASES)) — see [Identity and
//!   families](#identity-and-families).
//!
//! Levels are converted to the integer quotes the states serve by applying the resolved tokens'
//! decimals; a side whose levels do not state a usable price function is rejected whole, leaving
//! that one direction unquotable, never silently repaired.
//!
//! A book also names the quote sequence it was priced at (`calculated_from`), and the tracker
//! matches the book to the quote stream's account state by it. The parser reads that field
//! leniently (`titan::QuoteSequence`): a value it cannot read costs the book its simulation
//! overrides, never its quotes, and never the whole message.
//!
//! # Freshness contract
//!
//! Messages are best effort, and none lists every venue: a venue or a pair can be absent from
//! one message and present in the next, so absence is never read as retirement. Instead the stream
//! serves a component only while an accepted message carried it within the last `stale_after`
//! (default 24 s, two slots, see [`stale_after`](stream::PriceLevelStreamBuilder::stale_after)):
//!
//! - A message is accepted only if its `ts` is younger than `stale_after`, not more than one slot
//!   in the future, not older than the newest accepted message (equal is fine: Titan can send
//!   several messages with one `ts`), and its block neither regresses nor jumps more than one block
//!   per elapsed slot plus 2. Rejected messages change nothing. The message's beacon `slot` carries
//!   no freshness decision, so a message without it is judged exactly like one carrying it.
//! - A component no accepted message has carried for `stale_after` turns stale and is emitted in
//!   `removed_pairs`, together with every other component that turned stale at the same instant.
//!   The next accepted message carrying it re-adds it in `new_pairs`. Silence, disconnects,
//!   keepalive-only or unparsable traffic, and replayed messages all end in this stale removal.
//! - Every emitted state also refuses to quote once its message's `ts` is one slot
//!   ([`QUOTE_TTL`](state::QUOTE_TTL)) old, so its quotes are not served past the block they priced
//!   even before the removal arrives. The deadline is an `Instant` of this process and is never
//!   serialized. A consumer that quotes a state more than one slot after it arrived by design can
//!   opt out with [`without_quote_guard`](stream::PriceLevelStreamBuilder::without_quote_guard);
//!   the removal after `stale_after` still applies.
//!
//! Recovery is per component: a message carrying a pair re-adds that pair, nothing more, and a
//! message carrying one direction re-adds the pair with the other direction unquotable.
//! Consumers cannot tell a stale removal from a retired venue; both mean the component must not
//! be routed until it reappears in `new_pairs`. Removal and re-add are always separate updates.
//! Each update carries the block its message targets, and a removal carries the newest accepted
//! block. The number never decreases while something is served. Once every component has turned
//! stale, the stream judges the next accepted message as a first message, which may carry a lower
//! block; that is how the stream recovers from a message with an implausible block.
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
//! # Simulation overrides
//!
//! Price levels price the block Titan is building, so a swap simulated on mined state simulates a
//! venue state Titan never priced: the venue rejects the swap as stale, or a wrapper pays a
//! fallback price. The quote stream carries the account state Titan simulated each venue on. Each
//! state carries the venue entry that matches its book, and
//! [`simulation_overrides`](state::PriceLevelStreamState::simulation_overrides) returns it as
//! [`PammSimulationOverrides`](overrides::PammSimulationOverrides), ready to convert and pass to
//! `eth_call`.
//!
//! - **Matching.** A book names the quote sequence it was priced at. Its state gets the newest
//!   entry for the book's venue and pair that comes from the book's slot and has a quote sequence
//!   at or below the book's. Most books name a quote sequence no entry carries, because Titan does
//!   not publish an entry for every quote sequence; see [`overrides`].
//! - **Venues that read several quote addresses.** The buffer also holds an entry under every
//!   served venue its `pricedAddresses` name, and merges it into that venue's own entry, which wins
//!   wherever both set a value. Another quote address's entry merges only when its quote sequence
//!   is at or below the book's. The buffer compares the numbers only, and cannot tell whether that
//!   quote address counts on the venue's counter; see [`overrides`].
//! - **What the buffer holds.** Entries of the latest price level message's slot and one slot
//!   either side, and only under the venues this consumer serves. Before the first price level
//!   message, the newest slot the entries name places that window.
//! - **Quoting never depends on any of this.** A refused, silent or malformed quote stream, a
//!   refused entry, a book with no match, and a consumer that opted out all end in a state that
//!   quotes its price levels and carries no overrides. Nothing on this path removes a component,
//!   changes a quote, or shortens how long a quote is valid, and the stream never waits on the
//!   quote stream socket to serve a price level message.
//!
//! [`without_simulation_overrides`] turns the quote stream socket off, and
//! [`quote_stream_endpoint`] points it at another region.
//!
//! [`without_simulation_overrides`]: stream::PriceLevelStreamBuilder::without_simulation_overrides
//! [`quote_stream_endpoint`]: stream::PriceLevelStreamBuilder::quote_stream_endpoint
//!
//! # Observability
//!
//! The stream emits `price_level_stream_*` metrics through the `metrics` facade (messages
//! accepted and rejected by reason, the age of every accepted message, last seen timestamp and
//! served components per registered venue, stale removals, serving state, reconnects, the
//! books and book sides it declined to serve, the quote calls states refused, and, while
//! simulation overrides are on, which quote stream entries the stream refused, stored or dropped,
//! and whether each served pair got overrides); a consumer that installs a `metrics` recorder
//! receives them with no further setup. Per-venue series start at zero for every registered venue,
//! every `reason` of the book counters starts at zero, and no label ever carries a value from the
//! stream, except the venue address itself when a pAMM is served under auto-detection. In
//! particular a token address, a component id and a quote sequence are never labels: they are named
//! in the DEBUG log that accompanies the counter, not in the series.
//!
//! Label values and gauge encodings, for dashboards and alerts:
//! - `price_level_stream_frame_age_seconds`: a histogram of the wall-clock age of every accepted
//!   message at acceptance, negative for a message whose `ts` is ahead of the local clock. Titan's
//!   lag plus delivery delay and clock skew; above one slot the message's states refuse to quote.
//! - `price_level_stream_frames_rejected_total{socket, reason}`: `parse_error`, `too_old`,
//!   `in_future`, `out_of_order`, `block_regression`, `block_jump`. Only `parse_error` is ever
//!   counted on `socket="quote_stream"`.
//! - `price_level_stream_skipped_books_total{reason}`: `malformed_address` (an address is not 20
//!   bytes), `unadmitted_token` (an address is absent from the token metadata), `one_asset_pair`
//!   (both addresses name one asset). One increment per book left unpriced; a steady rate means an
//!   asset the venue quotes is going unserved.
//! - `price_level_stream_rejected_sides_total{reason}`: `unsupported_level`, `non_finite_amount`,
//!   `zero_amount`, `conflicting_knots`. One increment per book side dropped whole, which leaves
//!   that one trade direction unquotable while the opposite one is served.
//! - `price_level_stream_reconnects_total{socket, reason}`: `idle_timeout`, `ended`, `closed`,
//!   `read_error`, `connect_failed`, `connect_timeout`, on `socket="price_levels"` or
//!   `socket="quote_stream"`.
//! - `price_level_stream_serving_state`: 0 = unserved, 1 = serving.
//! - `venue` on `price_level_stream_last_seen_timestamp_seconds`,
//!   `price_level_stream_served_components`, `price_level_stream_stale_removals_total`,
//!   `price_level_stream_override_match_total`, `price_level_stream_override_merged_sources_total`
//!   and `price_level_stream_override_skipped_sources_total`: the registered venue name, or the
//!   address of an auto-detected venue.
//! - The quote refusals and the simulation overrides series are documented with their labels and
//!   baselines in the solver docs' metrics section.
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
pub mod overrides;
pub mod state;
pub mod stream;
mod telemetry;
#[cfg(test)]
mod test_support;
mod titan;
mod tracker;

/// The post-merge Ethereum slot. Every freshness window of the stream is a multiple of it.
const SLOT: Duration = Duration::from_secs(12);

/// Nanoseconds per second. Both Titan sockets send Unix nanosecond timestamps, and the stream
/// exports Unix seconds.
const NANOS_PER_SECOND: u64 = 1_000_000_000;
