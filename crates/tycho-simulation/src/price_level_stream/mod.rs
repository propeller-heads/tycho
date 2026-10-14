//! Titan pAMM price level stream integration.
//!
//! Titan Builder exposes two pAMM WebSockets. One streams per-pair quote books (simulated price
//! levels) for the pAMMs it builds blocks with (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>), and this module turns those
//! frames directly into [`Update`](crate::protocol::models::Update)s ready for consumption. The
//! other streams the account state Titan simulated each venue on to produce those books, which
//! this module stamps onto the states whose ladders name it — see [Simulated
//! pre-state](#simulated-pre-state). A consumer that only quotes can turn that second socket off.
//!
//! # The stream and what it admits
//!
//! A frame carries a flat list of books, one per venue and pair. A book prices its pair in human
//! units — a decimal price and a cumulative volume per level — and names its two tokens by
//! **symbol**, so serving it takes two resolutions neither of which trusts a streamed name:
//!
//! - A symbol is priced only if [`SYMBOL_BINDINGS`](config::SYMBOL_BINDINGS), or a caller's
//!   [`with_symbol_bindings`](stream::PriceLevelStreamBuilder::with_symbol_bindings) override,
//!   binds it to an address **and** that address is present in the provided token metadata. A
//!   symbol the bindings do not name is never guessed at from the consumer's own token symbols, so
//!   a book naming an unbound symbol stays unserved until the symbol is bound.
//! - A venue is identified by the canonical public identity of its streamed `quoteAddress` (see
//!   [`PAMM_ADDRESS_ALIASES`](config::PAMM_ADDRESS_ALIASES)) — see [Identity and
//!   families](#identity-and-families).
//!
//! Levels are converted to the integer ladders the states serve by applying the resolved tokens'
//! decimals; a side whose levels do not state a usable price function is rejected whole, leaving
//! that one direction unquotable, never silently repaired.
//!
//! A book also names the version of its pair the venue priced both its sides at, which is the only
//! thing that joins a snapshot of the second socket to the ladders it belongs to. That decode is
//! lenient by design (`titan::Provenance`): a reference this parser cannot read costs a book its
//! stamp, never its quotes, and never the whole frame.
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
//! # Simulated pre-state
//!
//! Ladders price the block being built, so simulating a pAMM swap on mined state simulates a venue
//! Titan never quoted — the venue rejects such a fill as stale, or a wrapper pays a fallback price.
//! The second socket is what closes that gap: per venue entry it carries the accounts Titan
//! overrode to price that venue, and the pair versions the ladders it produced name. Entries are
//! validated whole (`titan::validate`), held in a bounded fail-closed buffer
//! (`overrides::OverrideBuffer`) for one slot of their build's lifetime — fixed at the build's
//! first admission, and never extended by a replay, a supersession or a contradiction, each of
//! which leaves the build barred rather than forgotten until a slot from its stamp has passed,
//! while pressure that would have to forget such a build refuses the arriving one instead — and
//! stamped onto a state at construction, which
//! [`simulation_overrides`](state::PriceLevelStreamState::simulation_overrides) reads back as
//! [`PammSimulationOverrides`](overrides::PammSimulationOverrides).
//!
//! - **The join is exact, on the full identity** — canonical venue, resolved pair, sequence. A
//!   sequence identifies nothing on its own (Titan stamps a batch of pairs with one), and an entry
//!   advertising the sequence under a different pair is no match either, so nothing substitutes a
//!   neighboring pair's snapshot for the one the ladders name.
//! - **Five outcomes per served pair, plus one case that is none of them:** `matched`, `missing`,
//!   `ambiguous` (the pair's books disagreed about their version), `expired`, `poisoned` (two
//!   payloads claimed one build) — and a pair whose books named no readable version, which reaches
//!   no lookup at all and so appears under no outcome.
//! - **The stamped copy carries the narrower deadline:** `min(ladder deadline, snapshot deadline)`
//!   in whole seconds, and off-chain guidance only. The narrowing goes on a clone, because the
//!   retained snapshot is shared by every pair of the entry that carried it.
//! - **Quoting never depends on any of this.** A refused, silent or malformed second socket, an
//!   entry the validation declines, a buffer that holds nothing, a reference this parser cannot
//!   read, and a consumer that opted out all end in the same place: a state that quotes its ladders
//!   and carries no snapshot. Nothing on this path removes a component, delays an update, or
//!   changes a quote.
//! - **A match is the producer's claim, taken on its word.** The two sockets arrive independently
//!   and nothing here can check that one simulation produced both; the [`overrides`] module docs
//!   say what rests on that assumption, and [`without_simulation_overrides`] with
//!   [`override_endpoint`] are the knobs.
//!
//! [`without_simulation_overrides`]: stream::PriceLevelStreamBuilder::without_simulation_overrides
//! [`override_endpoint`]: stream::PriceLevelStreamBuilder::override_endpoint
//!
//! # Observability
//!
//! The stream emits `price_level_stream_*` metrics through the `metrics` facade (frames
//! accepted and rejected by reason, the age of every accepted frame, last seen timestamp and
//! served components per registered venue, stale removals, serving state, reconnects, the
//! books and book sides it declined to serve, and — while stamping is on — what became of every
//! snapshot the second socket delivered); a consumer that installs a `metrics` recorder
//! receives them with no further setup. Per-venue series start at zero for every registered venue,
//! every `reason` of the two book counters starts at zero, every series of the override path
//! starts at zero while stamping is on (`invalid_provenance_total` alone starts at zero either
//! way — the provenance read is on the price-level path), and so does every socket series this
//! consumer can fill:
//! all six rejection reasons and all six reconnect reasons on `price_levels`, plus — only when
//! stamping is on, because only then is the socket connected — `parse_error` and all six reconnect
//! reasons on `quote_stream`. That is 19 socket series with stamping and 12 without; the five
//! rejection reasons `quote_stream` cannot carry are deliberately not registered, so a series that
//! exists is one something can move. No label ever carries a streamed value, except the venue
//! address itself when a pAMM is served under auto-detection. In particular a streamed
//! token symbol is never a label: an unserved symbol is named in the DEBUG log that accompanies
//! its counter, not in the series. Neither is a component id, a pair version, or any other
//! address: the join reports them in its DEBUG log, because one series per pair Titan mentions
//! is unbounded growth.
//!
//! Both sockets are maintained by one function, so the `socket` label of the reconnect and
//! rejection counters is what tells them apart: `price_levels` or `quote_stream`. An override
//! socket that keeps dropping is a stamping outage, where the same on the price level socket is a
//! quoting outage, and only the label separates the two. Adding that label **changes the identity
//! of two already-released series**: `price_level_stream_reconnects_total` and
//! `price_level_stream_frames_rejected_total` carried `reason` alone before, so on deploy each old
//! series goes stale and a labelled one starts from zero — benign for `rate` and `increase`, and
//! indistinguishable from a restart, but any recording rule, alert or `sum without(...)` keyed on
//! the exact label set sees both the stale and the new series for the length of the staleness
//! window.
//!
//! Label values and gauge encodings, for dashboards and alerts:
//! - `price_level_stream_frame_age_seconds`: a histogram of the wall-clock age of every accepted
//!   frame at acceptance, negative for a frame stamped ahead of the local clock. Titan's lag plus
//!   delivery delay and clock skew; above one slot the frame's states refuse to quote.
//! - `price_level_stream_frames_rejected_total{socket, reason}`: `parse_error`, `too_old`,
//!   `in_future`, `out_of_order`, `block_regression`, `block_jump`. Only `parse_error` ever carries
//!   `socket = quote_stream`; every other reason is a freshness check of a price level frame.
//! - `price_level_stream_skipped_books_total{reason}`: `unbound_symbol` (no binding names the
//!   symbol), `unadmitted_token` (the bound address is absent from the token metadata),
//!   `one_asset_pair` (both symbols resolved to one asset). One increment per book left unpriced; a
//!   steady rate means an asset the venue quotes is going unserved.
//! - `price_level_stream_rejected_sides_total{reason}`: `unsupported_level`, `non_finite_amount`,
//!   `zero_amount`, `conflicting_knots`. One increment per book side dropped whole, which leaves
//!   that one trade direction unquotable while the opposite one is served.
//! - `price_level_stream_reconnects_total{socket, reason}`: `idle_timeout`, `ended`, `closed`,
//!   `read_error`, `connect_failed`, `connect_timeout`, each on `price_levels` or `quote_stream`.
//! - `price_level_stream_override_match_total{outcome}`: `matched`, `missing`, `ambiguous`,
//!   `expired`, `poisoned`. One increment per pair whose ladders were joined to the snapshots held
//!   for its venue, so `matched` against the rest is the share of served pairs carrying the state
//!   they were priced on. A pair whose books named no build is no join and appears under no
//!   outcome.
//! - `price_level_stream_override_buffer_total{event}`: every event of an ingest of the override
//!   buffer — `admitted`, `restamped`, `replayed`, `older_build`, `conflict_poisoned`,
//!   `rejected_poisoned_build`, `rejected_older_than_poison`, `rejected_spent_replay`,
//!   `rejected_future_skew`, `rejected_expired`, `rejected_key_saturated`,
//!   `rejected_keys_exhausted`, `rejected_frame_entry_cap`, `rejected_entry_pair_cap`,
//!   `degenerate_pair`, `evicted_entry`, `evicted_key`, `swept_expired`. These are buffer events,
//!   not lookup misses: an eviction here says a held build was dropped, never that a ladder failed
//!   to find one.
//! - `price_level_stream_refused_override_entries_total{reason}`: `not_a_venue`, `not_an_account`,
//!   `malformed_pair`, `malformed_storage`, `malformed_quantity`, `unstamped`, `sequence_mismatch`,
//!   `no_data`, `unsupported_override`, `over_cap`. One increment per venue entry the validation
//!   refused whole, before the buffer saw it — a producer-side fault, where a buffer event is this
//!   process's own retention rule at work.
//! - `price_level_stream_invalid_provenance_total`: one increment per **served** book whose version
//!   reference this parser cannot read — a book whose symbols never resolved is counted on
//!   `price_level_stream_skipped_books_total` and never has its provenance read. An absent
//!   reference is not counted either, so a parse regression here cannot hide inside the ordinary
//!   rate of books that name nothing. Counted per book, where the match counter above counts per
//!   pair, so the two must not be ratioed against each other.
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

/// Both Titan sockets stamp in Unix nanoseconds, while every deadline the stream exports is a Unix
/// second, so each layer that crosses between the two divides by this.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Furthest ahead of the local wall clock a stamp from either Titan socket may lie: one slot.
/// Titan's frames were observed at least 120 ms behind the local clock; anything ahead of it by
/// more than clock skew is implausible. A frame's `timestamp` is judged against it in [`tracker`],
/// and a venue entry's build stamp in [`overrides`], on the same allowance: the two sockets
/// describe one build round, so admitting a stamp on one of them and refusing it on the other
/// would leave a ladder that cannot be attributed to the snapshot it was priced on. That the
/// allowance is shared is why it lives here rather than in either of them.
const MAX_FUTURE_SKEW_NANOS: u64 = SLOT.as_secs() * NANOS_PER_SECOND;
