//! Turns Titan frames into [`Update`]s.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    time::{Duration, Instant},
};

use chrono::Utc;
use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use super::{
    config::{canonical_pamm_address, PriceLevelStreamConfig, SymbolBindings},
    convert::{ladders_for_book, resolve_book_tokens, Ladder, SideRejection, UnresolvedSymbol},
    overrides::{IngestEvent, MatchResult, OverrideBuffer, PammSimulationOverrides},
    state::{PriceLevelStreamQuote, PriceLevelStreamState, QUOTE_TTL},
    stream::PAMM_ADDRESS_ATTRIBUTE,
    telemetry::{
        self, EntryRefusalReason, MatchOutcome, RejectReason, SideRejectReason, SkipReason,
    },
    titan::{
        self, EntryOutcome, EntryRejection, Provenance, TitanBook, TitanFrame,
        TitanOverrideMessage, TitanPriceLevelMessage,
    },
    MAX_FUTURE_SKEW_NANOS, NANOS_PER_SECOND, SLOT,
};
use crate::protocol::models::{ProtocolComponent, Update};

/// How long a component stays served after the last accepted frame that carried it: two slots.
/// Titan streams at 1 Hz and its frames arrive within about 3 s of being built, so 24 s absorbs
/// jitter and still removes a silent component within two blocks.
pub(super) const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(2 * SLOT.as_secs());

/// Extra blocks a frame may jump beyond one block per elapsed slot. Titan builds at chain head + 1
/// and sometimes + 2, so a frame right after a block boundary may jump by two.
const BLOCK_JUMP_SLACK: u64 = 2;

/// How many distinct unregistered venue addresses the tracker logs at INFO, once each. The
/// addresses come from the stream, so the set is capped to keep a misbehaving upstream from growing
/// memory, and no address becomes a metric label.
const MAX_UNREGISTERED_LOGGED: usize = 64;

/// How many venues auto-detection may add to the registry per process. Titan streamed seven
/// venues in the 2026-09 capture; the cap keeps a misbehaving upstream from growing the registry,
/// the served set and the per-venue metric series without bound. Venues past the cap are skipped
/// like unregistered ones.
const MAX_AUTO_DETECTED: usize = 64;

/// The wall-clock and monotonic time at which the tracker handles a frame. The tracker never
/// reads a clock itself: the caller supplies both, so every decision is a function of its inputs.
#[derive(Clone, Copy, Debug)]
pub(super) struct Now {
    /// Wall clock, nanoseconds since the Unix epoch. Only ever compared against Titan's
    /// `timestamp`.
    pub wall_nanos: u64,
    /// Monotonic clock. Every deadline is computed from it, so a wall-clock jump cannot remove a
    /// component early or keep it served late.
    pub monotonic: Instant,
}

/// The timing one accepted frame's output is judged by, all of it derived from the frame's age at
/// acceptance.
#[derive(Clone, Copy)]
struct FrameTiming {
    /// The reading the frame was accepted at, which a snapshot lookup is judged against.
    now: Now,
    /// The instant the components the frame carries turn stale.
    stale_at: Instant,
    /// The instant their states stop quoting, or `None` while the quote guard is off.
    quotable_until: Option<Instant>,
    /// The Unix second their ladders stop quoting at: the deadline of `quotable_until` floored to
    /// the second, which a stamped snapshot's own deadline is narrowed to.
    ///
    /// Computed whether or not the guard is on, because unlike `quotable_until` it travels with
    /// the state: a consumer that opted out of the guard by design still learns when the
    /// pre-state it would simulate on went stale.
    ladder_deadline: u64,
}

impl FrameTiming {
    fn new(now: Now, frame_age: Duration, stale_after: Duration, quote_guard: bool) -> Self {
        // `QUOTE_TTL` counts from the frame's `timestamp` and is enforced on the monotonic
        // clock; it does not depend on whether the ladder's content changed, because quiet
        // venues repeat a ladder for minutes. A frame stamped ahead of the local clock has age
        // zero, so it gets one slot of quotability and never more.
        let quotable_for = QUOTE_TTL.saturating_sub(frame_age);
        Self {
            now,
            stale_at: now.monotonic + stale_after.saturating_sub(frame_age),
            quotable_until: quote_guard.then(|| now.monotonic + quotable_for),
            ladder_deadline: (now.wall_nanos / NANOS_PER_SECOND)
                .saturating_add(quotable_for.as_secs()),
        }
    }
}

/// A component the stream currently serves.
struct ServedComponent {
    component: ProtocolComponent,
    /// The venue name, for logs and metric labels.
    venue_name: String,
    /// The instant this component's data turns `stale_after` old.
    stale_at: Instant,
}

/// The newest accepted frame while something is served. A later frame may not target a block
/// below `block`, nor one above it by more than one block per slot elapsed since `accepted_at`
/// plus [`BLOCK_JUMP_SLACK`].
#[derive(Clone, Copy, Debug)]
struct Frontier {
    block: u64,
    accepted_at: Instant,
}

/// The stream's serving state.
#[derive(Clone, Copy)]
enum ServingState {
    /// Nothing served: at start, and whenever the last served component turned stale.
    Unserved,
    /// At least one component is served. `deadline` is the earliest `stale_at` among them and
    /// `frontier` the newest accepted frame. Both exist only in this state, so a frame accepted
    /// while nothing is served never binds the block checks of a later frame.
    Serving { deadline: Instant, frontier: Frontier },
}

impl ServingState {
    fn telemetry_state(self) -> telemetry::ServingState {
        match self {
            ServingState::Unserved => telemetry::ServingState::Unserved,
            ServingState::Serving { deadline: _, frontier: _ } => telemetry::ServingState::Serving,
        }
    }
}

/// What a [`FreshnessTracker`] is built from.
pub(super) struct TrackerSettings {
    /// The venues to serve, keyed by their canonical public identity (see
    /// [`canonical_pamm_address`]).
    pub registry: HashMap<Bytes, PriceLevelStreamConfig>,
    /// Venues excluded from auto-detection, by canonical public identity. Disjoint from
    /// `registry`: the builder keeps denying and registering the same venue from overlapping.
    pub denied: HashSet<Bytes>,
    pub tokens: HashMap<Bytes, Token>,
    /// The bindings a book's token symbols are resolved through.
    pub bindings: SymbolBindings,
    /// Whether frames from pAMMs absent from the registry get an address-named configuration
    /// synthesized (and cached in the registry) instead of being skipped.
    pub auto_detect: bool,
    /// The per-swap gas cost synthesized auto-detected configurations are served with.
    pub auto_detected_gas_cost: BigUint,
    /// How long a component stays served after the last accepted frame that carried it; see
    /// [`DEFAULT_STALE_AFTER`].
    pub stale_after: Duration,
    /// Whether components are emitted under the `fallback:` family, so their swaps execute
    /// through `TychoFallbackRouter` instead of the venue directly.
    pub via_fallback_router: bool,
    /// Whether every emitted state refuses to quote once its frame is [`QUOTE_TTL`] old.
    pub quote_guard: bool,
    /// Whether a state carries the simulated pre-state its ladders were priced on. Off means no
    /// override socket feeds this tracker, so no snapshot is held, looked up or stamped, and no
    /// join outcome is reported.
    pub simulation_overrides: bool,
}

/// Turns Titan frames into [`Update`]s. A frame only adds or refreshes components: a component (or
/// a whole venue) that a frame omits stays served until its own deadline passes in
/// [`on_stale_deadline`](Self::on_stale_deadline).
pub(super) struct FreshnessTracker {
    registry: HashMap<Bytes, PriceLevelStreamConfig>,
    denied: HashSet<Bytes>,
    tokens: HashMap<Bytes, Token>,
    bindings: SymbolBindings,
    auto_detect: bool,
    auto_detected_gas_cost: BigUint,
    stale_after: Duration,
    via_fallback_router: bool,
    quote_guard: bool,
    /// The components currently served, keyed by component id.
    served: HashMap<String, ServedComponent>,
    /// The stream's serving state; holds the block frontier and drives
    /// [`Self::stale_deadline`] and the per-venue gauges.
    serving_state: ServingState,
    /// The `timestamp` of the newest accepted frame. Frames older than it are out of order;
    /// equal ones are re-emissions within a build round and accepted. Never reset.
    newest_timestamp_nanos: u64,
    /// Whether the last frame was rejected. The first rejection of a streak logs at WARN and the
    /// rest at DEBUG; `price_level_stream_frames_rejected_total` counts every rejection.
    rejecting: bool,
    /// Unregistered venue addresses already logged, at most [`MAX_UNREGISTERED_LOGGED`].
    logged_unregistered: HashSet<Bytes>,
    /// Venues auto-detection has added to the registry, at most [`MAX_AUTO_DETECTED`].
    auto_detected: usize,
    /// The snapshots the override socket has delivered, until a ladder names one; `None` when the
    /// consumer opted out of stamping, in which case no socket fills it.
    overrides: Option<OverrideBuffer>,
}

impl FreshnessTracker {
    pub(super) fn new(settings: TrackerSettings) -> Self {
        let TrackerSettings {
            registry,
            denied,
            tokens,
            bindings,
            auto_detect,
            auto_detected_gas_cost,
            stale_after,
            via_fallback_router,
            quote_guard,
            simulation_overrides,
        } = settings;
        telemetry::record_serving_state(telemetry::ServingState::Unserved);
        // Pre-initialise every per-venue series so a venue that never appears is a visible
        // zero, not a missing series.
        for config in registry.values() {
            telemetry::record_served_components(&config.protocol, 0);
            telemetry::record_last_seen(&config.protocol, 0);
            telemetry::init_stale_removals(&config.protocol);
        }
        telemetry::init_book_counters();
        // One tracker is built per stream, so this is also where the series the socket layer fills
        // are registered — for the second socket only when it will exist. Only a consumer that
        // stamps can ever move the override series, so only it advertises them.
        telemetry::init_socket_counters(simulation_overrides);
        if simulation_overrides {
            telemetry::init_override_counters();
        }
        Self {
            registry,
            denied,
            tokens,
            bindings,
            auto_detect,
            auto_detected_gas_cost,
            stale_after,
            via_fallback_router,
            quote_guard,
            served: HashMap::new(),
            serving_state: ServingState::Unserved,
            newest_timestamp_nanos: 0,
            rejecting: false,
            logged_unregistered: HashSet::new(),
            auto_detected: 0,
            overrides: simulation_overrides.then(OverrideBuffer::default),
        }
    }

    /// Returns the frame's age if the frame passes the timestamp and block checks, or the
    /// [`RejectReason`] that names the first failed check. The block checks bind only while
    /// something is served.
    fn check_frame(
        &self,
        frame: &TitanPriceLevelMessage,
        now: Now,
    ) -> Result<Duration, RejectReason> {
        let frame_age = Duration::from_nanos(now.wall_nanos.saturating_sub(frame.ts));
        if frame_age >= self.stale_after {
            return Err(RejectReason::TooOld);
        }
        if frame.ts >
            now.wall_nanos
                .saturating_add(MAX_FUTURE_SKEW_NANOS)
        {
            return Err(RejectReason::InFuture);
        }
        if frame.ts < self.newest_timestamp_nanos {
            return Err(RejectReason::OutOfOrder);
        }
        let ServingState::Serving { deadline: _, frontier } = self.serving_state else {
            return Ok(frame_age);
        };
        if frame.block < frontier.block {
            return Err(RejectReason::BlockRegression);
        }
        let elapsed_slots = now
            .monotonic
            .saturating_duration_since(frontier.accepted_at)
            .as_secs() /
            SLOT.as_secs();
        let allowed = frontier
            .block
            .saturating_add(elapsed_slots)
            .saturating_add(BLOCK_JUMP_SLACK);
        if frame.block > allowed {
            return Err(RejectReason::BlockJump);
        }
        Ok(frame_age)
    }

    /// Processes one frame into an [`Update`], or `None` if the frame is rejected (see
    /// [`RejectReason`]) or carries no book of a served pAMM that prices an admitted pair.
    ///
    /// One book carries both directions of its pair, so a venue needs only one book per pair. If
    /// a frame carries several for one pair anyway — a stream anomaly, never observed — the last in
    /// stream order is the one served, and the pair is stamped with a snapshot only if every book
    /// of it named the same build (see [`pair_entries`]).
    pub(super) fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update> {
        let frame_age = match self.check_frame(&frame, now) {
            Ok(frame_age) => frame_age,
            Err(reason) => {
                self.log_rejection(reason, &frame);
                // Every check here is one this tracker makes of a price level frame; the override
                // socket's own rejections are counted under its name where it is parsed.
                telemetry::record_frame_rejected(TitanPriceLevelMessage::SOCKET, reason);
                return None;
            }
        };
        self.rejecting = false;
        telemetry::record_frame_accepted();
        // Signed, unlike `frame_age`: a frame stamped ahead of the local clock records a negative
        // age, so a clock skew between Titan and this host shows on the histogram instead of
        // looking like a perfect feed.
        let age_nanos = i128::from(now.wall_nanos) - i128::from(frame.ts);
        telemetry::record_frame_age(age_nanos as f64 / NANOS_PER_SECOND as f64);
        self.newest_timestamp_nanos = frame.ts;

        let timing = FrameTiming::new(now, frame_age, self.stale_after, self.quote_guard);
        let (states, new_pairs) = self.serve_books(frame.books, frame.ts, timing);

        self.settle(Frontier { block: frame.block, accepted_at: now.monotonic });
        if states.is_empty() {
            return None;
        }
        Some(Update::new(frame.block, states, new_pairs).set_is_partial(true))
    }

    /// Serves every pair of every served venue `books` carries: the state of each, and the
    /// component of each pair that was not served before.
    fn serve_books(
        &mut self,
        books: Vec<TitanBook>,
        frame_ts: u64,
        timing: FrameTiming,
    ) -> (HashMap<String, Box<dyn ProtocolSim>>, HashMap<String, ProtocolComponent>) {
        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();
        for (pamm, books) in group_by_venue(books) {
            self.admit(&pamm);
            let Some(config) = self.registry.get(&pamm) else {
                continue;
            };
            telemetry::record_last_seen(&config.protocol, frame_ts / NANOS_PER_SECOND);

            for entry in pair_entries(&books, &self.bindings, &self.tokens) {
                let id = component_id(&config.address, &entry.token0, &entry.token1);
                let id_string = id.to_string();
                match self.served.get_mut(&id_string) {
                    Some(served) => served.stale_at = timing.stale_at,
                    None => {
                        let component = build_component(
                            &self.tokens,
                            config,
                            id,
                            &entry.token0,
                            &entry.token1,
                            self.via_fallback_router,
                        );
                        new_pairs.insert(id_string.clone(), component.clone());
                        self.served.insert(
                            id_string.clone(),
                            ServedComponent {
                                component,
                                venue_name: config.protocol.clone(),
                                stale_at: timing.stale_at,
                            },
                        );
                    }
                }
                let joined = join(self.overrides.as_ref(), &pamm, &entry, timing.now);
                let mut state = PriceLevelStreamState::new(
                    entry.token0,
                    entry.token1,
                    entry.quotes_0_to_1,
                    entry.quotes_1_to_0,
                    config.gas_cost.clone(),
                );
                if let Some(until) = timing.quotable_until {
                    state = state.with_quotable_until(until);
                }
                if let Some(outcome) = joined {
                    record_stamping(&config.protocol, &id_string, &outcome);
                    state = stamped(state, &outcome, timing.ladder_deadline);
                }
                states.insert(id_string, Box::new(state));
            }
        }
        (states, new_pairs)
    }

    /// Registers an auto-detected venue on first sight, up to [`MAX_AUTO_DETECTED`]. A denied
    /// venue, an unregistered one while auto-detection is off, and any venue past the cap stay
    /// out of the registry.
    ///
    /// `pamm` is a canonical public identity, so a venue is admitted, denied and named by the
    /// address its swaps are sent to, whichever address Titan streams its books under.
    fn admit(&mut self, pamm: &Bytes) {
        if self.registry.contains_key(pamm) {
            return;
        }
        if self.denied.contains(pamm) {
            tracing::debug!(%pamm, "Skipping denied pAMM");
            return;
        }
        if self.auto_detect && self.auto_detected < MAX_AUTO_DETECTED {
            tracing::info!(%pamm, "Serving auto-detected pAMM");
            let config = PriceLevelStreamConfig::auto_detected(
                pamm.clone(),
                self.auto_detected_gas_cost.clone(),
            );
            telemetry::record_served_components(&config.protocol, 0);
            telemetry::record_last_seen(&config.protocol, 0);
            telemetry::init_stale_removals(&config.protocol);
            self.registry
                .insert(pamm.clone(), config);
            self.auto_detected += 1;
            return;
        }
        telemetry::record_unregistered_pamm();
        if self.logged_unregistered.len() < MAX_UNREGISTERED_LOGGED &&
            self.logged_unregistered
                .insert(pamm.clone())
        {
            if self.auto_detect {
                tracing::warn!(
                    %pamm,
                    cap = MAX_AUTO_DETECTED,
                    "Skipping unknown pAMM: the auto-detection cap is reached; register it via \
                     add_pamm to serve it"
                );
            } else {
                tracing::info!(
                    %pamm,
                    "Skipping unregistered pAMM; register it via add_pamm to serve it"
                );
            }
        }
    }

    /// Admits the venue entries of one override frame into the buffer.
    ///
    /// Yields nothing, because a snapshot changes neither what is served nor how it quotes: it is
    /// held until a book of the price level socket names the build it belongs to and is stamped
    /// onto that book's state then. A snapshot whose ladders were served before it arrived
    /// therefore changes nothing — the state already handed to the consumer is never revisited.
    ///
    /// The venue is canonicalized here exactly as a book's is: both sockets name a venue by the
    /// same streamed address, so a snapshot indexed under the streamed address would never be found
    /// by ladders indexed under the public one.
    pub(super) fn on_override_frame(&mut self, frame: TitanOverrideMessage, now: Now) {
        // Unreachable while stamping is off: the builder connects no override socket then, so
        // there is no frame to admit.
        let Some(buffer) = self.overrides.as_mut() else {
            return;
        };
        let mut entries = Vec::new();
        for outcome in titan::validate(frame) {
            match outcome {
                EntryOutcome::Accepted(entry) => {
                    entries.push((canonical_pamm_address(&entry.quote_address), entry));
                }
                EntryOutcome::Rejected(reason) => record_refused_entry(reason),
            }
        }
        record_override_ingest(&buffer.ingest(entries, now));
    }

    /// Removes every served component whose deadline has passed, as one [`Update`].
    pub(super) fn on_stale_deadline(&mut self, now: Instant) -> Option<Update> {
        let ServingState::Serving { deadline: _, frontier } = self.serving_state else {
            return None;
        };
        let mut removed = HashMap::new();
        let mut venues = BTreeSet::new();
        for (id, served) in self
            .served
            .extract_if(|_, served| served.stale_at <= now)
        {
            telemetry::record_stale_removal(&served.venue_name);
            venues.insert(served.venue_name);
            removed.insert(id, served.component);
        }
        if removed.is_empty() {
            return None;
        }
        let component_ids: Vec<&String> = removed.keys().collect();
        tracing::warn!(
            removed = removed.len(),
            venues = ?venues,
            components = ?component_ids,
            stale_after_secs = self.stale_after.as_secs(),
            "Removing price level components: no accepted frame carried them within stale_after"
        );
        // Stamped with the newest accepted block, so the removal orders after every update the
        // removed components were served in.
        let update = Update::new(frontier.block, HashMap::new(), HashMap::new())
            .set_is_partial(true)
            .set_removed_pairs(removed);
        self.settle(frontier);
        Some(update)
    }

    /// The instant the earliest served component turns stale, if anything is served.
    pub(super) fn stale_deadline(&self) -> Option<Instant> {
        match self.serving_state {
            ServingState::Serving { deadline, frontier: _ } => Some(deadline),
            ServingState::Unserved => None,
        }
    }

    /// Recomputes the serving state from the served set, with `frontier` as the newest accepted
    /// frame, and records the serving-state and per-venue gauges. Nothing served means
    /// `Unserved`, which forgets the frontier: the next frame is judged as a first frame, so a
    /// frame with an implausible block cannot cause rejections for longer than `stale_after`.
    fn settle(&mut self, frontier: Frontier) {
        let deadline = self
            .served
            .values()
            .map(|served| served.stale_at)
            .min();
        self.serving_state = match deadline {
            Some(deadline) => ServingState::Serving { deadline, frontier },
            None => ServingState::Unserved,
        };
        telemetry::record_serving_state(self.serving_state.telemetry_state());
        let mut per_venue: HashMap<&str, usize> = self
            .registry
            .values()
            .map(|config| (config.protocol.as_str(), 0))
            .collect();
        for served in self.served.values() {
            *per_venue
                .entry(served.venue_name.as_str())
                .or_default() += 1;
        }
        for (venue, count) in per_venue {
            telemetry::record_served_components(venue, count);
        }
    }

    /// Logs the first rejected frame of a streak at WARN and the rest at DEBUG.
    fn log_rejection(&mut self, reason: RejectReason, frame: &TitanPriceLevelMessage) {
        let reason: &'static str = reason.into();
        if self.rejecting {
            tracing::debug!(
                reason,
                block = frame.block,
                slot = ?frame.slot,
                ts = frame.ts,
                "Rejecting price level frame"
            );
            return;
        }
        self.rejecting = true;
        let newest_block = match self.serving_state {
            ServingState::Serving { deadline: _, frontier } => Some(frontier.block),
            ServingState::Unserved => None,
        };
        tracing::warn!(
            reason,
            block = frame.block,
            slot = ?frame.slot,
            ts = frame.ts,
            newest_block = ?newest_block,
            newest_timestamp_nanos = self.newest_timestamp_nanos,
            "Rejecting price level frame; further rejections logged at debug until a frame is \
             accepted"
        );
    }
}

/// Groups a frame's books by the canonical public identity of the venue that quoted them,
/// keeping the venues and each venue's books in stream order.
///
/// Only the grouping key is canonicalized: every book keeps the `quote_address` Titan streamed it
/// under, so the streamed identity stays available beside the book even though no component ever
/// carries it.
fn group_by_venue(books: Vec<TitanBook>) -> Vec<(Bytes, Vec<TitanBook>)> {
    let mut venues: Vec<(Bytes, Vec<TitanBook>)> = Vec::new();
    let mut positions: HashMap<Bytes, usize> = HashMap::new();
    for book in books {
        let venue = canonical_pamm_address(&book.quote_address);
        match positions.get(&venue) {
            Some(&position) => venues[position].1.push(book),
            None => {
                positions.insert(venue.clone(), venues.len());
                venues.push((venue, vec![book]));
            }
        }
    }
    venues
}

/// One pair of one venue, as a frame serves it: the ladders quoted for it, and what the books
/// that quoted them said about the snapshot they were priced on.
struct PairEntry {
    token0: Bytes,
    token1: Bytes,
    quotes_0_to_1: Vec<PriceLevelStreamQuote>,
    quotes_1_to_0: Vec<PriceLevelStreamQuote>,
    provenance: PairProvenance,
}

/// What the books one venue quoted a pair with said about the snapshot their ladders were priced
/// on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairProvenance {
    /// Every book of the pair named this one version of it.
    Version(u128),
    /// No book of the pair named a version to match a snapshot against.
    Unreferenced,
    /// The pair's books did not agree on one version, so nothing attributes the ladders served
    /// for it to a single snapshot.
    Conflicting,
}

impl PairProvenance {
    /// What one book says: the version it was priced at, or nothing — a book that named no
    /// reference and one whose reference this parser cannot read both leave nothing to match a
    /// snapshot against.
    ///
    /// The two are distinct where the message is parsed ([`Provenance`]), because only the second
    /// says the producer sent something this parser does not understand, and this is the one
    /// place they collapse: the counter that tells them apart is therefore here, not further
    /// down, where the distinction is gone. Only the unreadable reference is counted — counting
    /// absence too would hide a parse regression inside the ordinary rate of books that name
    /// nothing.
    fn of_book(book: &TitanBook) -> Self {
        match book.calculated_from {
            Provenance::Valid(sequence) => Self::Version(sequence),
            Provenance::Invalid => {
                telemetry::record_invalid_provenance();
                // The reference itself is not retained: the lenient decode reads it for a version
                // and keeps nothing else, so the log names the book rather than what it said.
                tracing::debug!(
                    quote_address = %book.quote_address,
                    token1 = book.token1,
                    token2 = book.token2,
                    "Reading a book whose version reference this parser cannot read"
                );
                Self::Unreferenced
            }
            Provenance::Absent => Self::Unreferenced,
        }
    }

    /// What two books of one pair jointly say.
    ///
    /// Only agreement on one readable version survives: two different versions, a version against
    /// a book that named none, and a pair already in disagreement all leave the pair
    /// unattributable. Failing closed is the point — stamping a snapshot the served ladders may
    /// not have been priced on would have a consumer simulate the venue on a state it was never
    /// in.
    fn merged(self, other: Self) -> Self {
        match (self, other) {
            (Self::Version(held), Self::Version(arriving)) if held == arriving => {
                Self::Version(held)
            }
            (Self::Unreferenced, Self::Unreferenced) => Self::Unreferenced,
            (Self::Version(_), Self::Version(_)) |
            (Self::Version(_), Self::Unreferenced) |
            (Self::Version(_), Self::Conflicting) |
            (Self::Unreferenced, Self::Version(_)) |
            (Self::Unreferenced, Self::Conflicting) |
            (Self::Conflicting, Self::Version(_)) |
            (Self::Conflicting, Self::Unreferenced) |
            (Self::Conflicting, Self::Conflicting) => Self::Conflicting,
        }
    }
}

/// The pairs one venue's books price, in the stream order of each pair's first book.
///
/// One book carries both directions of its pair, so a venue needs only one book per pair, and a
/// frame carrying several for one pair — a stream anomaly, never observed — is where the two halves
/// of an entry part ways: the ladders are the last book's, which is the winner stage 1 served,
/// while the version is carried only if every book of the pair named the same one. Provenance
/// therefore never decides which quotes win, and a disagreement about what priced them costs the
/// pair its stamp rather than its quotes.
fn pair_entries(
    books: &[TitanBook],
    bindings: &SymbolBindings,
    tokens: &HashMap<Bytes, Token>,
) -> Vec<PairEntry> {
    let mut entries: Vec<PairEntry> = Vec::new();
    let mut positions: HashMap<(Bytes, Bytes), usize> = HashMap::new();
    for book in books {
        let Some(PairLadders { token0, token1, quotes_0_to_1, quotes_1_to_0 }) =
            ladders_for_pair(book, bindings, tokens)
        else {
            continue;
        };
        let provenance = PairProvenance::of_book(book);
        match positions.get(&(token0.clone(), token1.clone())) {
            Some(&position) => {
                let served = &mut entries[position];
                served.quotes_0_to_1 = quotes_0_to_1;
                served.quotes_1_to_0 = quotes_1_to_0;
                served.provenance = served.provenance.merged(provenance);
            }
            None => {
                positions.insert((token0.clone(), token1.clone()), entries.len());
                entries.push(PairEntry {
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    provenance,
                });
            }
        }
    }
    entries
}

/// What the buffer makes of `entry`'s provenance: the snapshot of the exact build its books named,
/// or why there is none.
///
/// **A match is the producer's claim, not a fact established here.** Titan says the ladders of this
/// book were priced at this version of this pair, and a venue entry says this snapshot is the state
/// it priced that version on; that the two statements describe one simulation is an assumption
/// about the producer, and nothing Titan sends lets this code check it. Validating it is the
/// release gate's full-coverage simulation — re-simulating stamped states and holding the result
/// against the ladders they came with — not anything on this path. A consumer that simulates on a
/// stamped snapshot relies on that assumption, which is why a pair whose books disagree is stamped
/// from nothing rather than from the likelier of two candidates: refusing to guess is the only
/// safeguard this layer has.
///
/// `None` is not an outcome but the absence of a join: the pair's books named no version, so
/// nothing was looked up. A producer that states nothing about what it priced on is a different
/// fact from a lookup that found nothing, and reporting it as one would hide a parse regression
/// inside the ordinary miss rate — see [`PairProvenance::of_book`] for where it is told apart.
fn join(
    buffer: Option<&OverrideBuffer>,
    venue: &Bytes,
    entry: &PairEntry,
    now: Now,
) -> Option<MatchResult> {
    let buffer = buffer?;
    match entry.provenance {
        PairProvenance::Version(sequence) => {
            Some(buffer.matched(venue, &entry.token0, &entry.token1, sequence, now))
        }
        PairProvenance::Unreferenced => None,
        // Which of the pair's references the served ladders belong to cannot be decided here,
        // which is what `Ambiguous` says; the buffer, whose key is exact, never produces it.
        PairProvenance::Conflicting => Some(MatchResult::Ambiguous),
    }
}

/// `state` with the simulated pre-state its ladders were priced on, when the join found one.
fn stamped(
    state: PriceLevelStreamState,
    outcome: &MatchResult,
    ladder_deadline: u64,
) -> PriceLevelStreamState {
    match outcome {
        MatchResult::Matched(snapshot) => {
            // The retained snapshot is shared by every pair of the entry that carried it and by
            // every state stamped from it, so the narrowing goes on a copy: one pair's ladder
            // deadline must shorten neither another pair's nor the lifetime the buffer holds the
            // build for.
            let mut narrowed = PammSimulationOverrides::clone(snapshot);
            narrowed.expires_at = narrowed.expires_at.min(ladder_deadline);
            state.with_simulation_overrides(narrowed)
        }
        MatchResult::Missing |
        MatchResult::Ambiguous |
        MatchResult::Expired |
        MatchResult::Poisoned => state,
    }
}

/// One pair's quote ladders, in the token order [`PriceLevelStreamState`] and [`component_id`]
/// use: `token0` is the lower address.
struct PairLadders {
    token0: Bytes,
    token1: Bytes,
    quotes_0_to_1: Vec<PriceLevelStreamQuote>,
    quotes_1_to_0: Vec<PriceLevelStreamQuote>,
}

/// The quote ladders of `book`, or `None` if it prices no pair this consumer can serve: a symbol
/// that does not resolve through `bindings` into `tokens`, or two symbols naming one asset.
///
/// A side the conversion rejects yields an empty ladder, so that one direction is unquotable
/// while the opposite direction keeps being served — as for a side the venue quoted empty.
fn ladders_for_pair(
    book: &TitanBook,
    bindings: &SymbolBindings,
    tokens: &HashMap<Bytes, Token>,
) -> Option<PairLadders> {
    let (base, quote) = match resolve_book_tokens(book, bindings, tokens) {
        Ok(resolved) => resolved,
        Err(unresolved) => {
            let (reason, symbol) = match unresolved {
                UnresolvedSymbol::Unbound(symbol) => (SkipReason::UnboundSymbol, symbol),
                UnresolvedSymbol::Unadmitted(symbol) => (SkipReason::UnadmittedToken, symbol),
            };
            telemetry::record_skipped_book(reason);
            // The symbol comes from Titan, so it goes in the log and never in a metric label.
            tracing::debug!(
                quote_address = %book.quote_address,
                token1 = book.token1,
                token2 = book.token2,
                reason = <&'static str>::from(reason),
                symbol,
                "Skipping book: its symbols name no pair of admitted tokens"
            );
            return None;
        }
    };
    if base.address == quote.address {
        telemetry::record_skipped_book(SkipReason::OneAssetPair);
        tracing::debug!(
            quote_address = %book.quote_address,
            token1 = book.token1,
            token2 = book.token2,
            address = %base.address,
            "Skipping book: both of its symbols name one asset"
        );
        return None;
    }

    let (sell_base, sell_quote) = ladders_for_book(book, base, quote);
    let sell_base = accepted_side(sell_base, book, "bids");
    let sell_quote = accepted_side(sell_quote, book, "asks");
    if base.address < quote.address {
        Some(PairLadders {
            token0: base.address.clone(),
            token1: quote.address.clone(),
            quotes_0_to_1: sell_base,
            quotes_1_to_0: sell_quote,
        })
    } else {
        Some(PairLadders {
            token0: quote.address.clone(),
            token1: base.address.clone(),
            quotes_0_to_1: sell_quote,
            quotes_1_to_0: sell_base,
        })
    }
}

/// The side's ladder, or an empty one if the conversion rejected the side.
fn accepted_side(
    ladder: Ladder,
    book: &TitanBook,
    side: &'static str,
) -> Vec<PriceLevelStreamQuote> {
    ladder.unwrap_or_else(|rejection| {
        let reason = side_reject_reason(rejection);
        telemetry::record_rejected_side(reason);
        tracing::debug!(
            quote_address = %book.quote_address,
            token1 = book.token1,
            token2 = book.token2,
            side,
            reason = <&'static str>::from(reason),
            "Dropping book side: its levels are not a usable price function"
        );
        Vec::new()
    })
}

/// Routes a venue entry the validation refused whole.
///
/// An entry Titan sent and this process could not use must not be silent: the pairs it carried go
/// unstamped for the rest of their build, and the reason says whether the payload contradicted the
/// documented shape or merely outgrew a cap.
fn record_refused_entry(reason: EntryRejection) {
    let reason = entry_refusal_reason(reason);
    telemetry::record_refused_override_entry(reason);
    tracing::debug!(reason = <&'static str>::from(reason), "Refusing a pAMM override entry whole");
}

/// The telemetry reason an entry the validation refused is counted under. Spelled out rather than
/// derived, so a new [`EntryRejection`] variant has to be given a label here.
fn entry_refusal_reason(rejection: EntryRejection) -> EntryRefusalReason {
    match rejection {
        EntryRejection::NotAVenue => EntryRefusalReason::NotAVenue,
        EntryRejection::NotAnAccount => EntryRefusalReason::NotAnAccount,
        EntryRejection::MalformedPair => EntryRefusalReason::MalformedPair,
        EntryRejection::MalformedStorage => EntryRefusalReason::MalformedStorage,
        EntryRejection::MalformedQuantity => EntryRefusalReason::MalformedQuantity,
        EntryRejection::Unstamped => EntryRefusalReason::Unstamped,
        EntryRejection::SequenceMismatch => EntryRefusalReason::SequenceMismatch,
        EntryRejection::NoData => EntryRefusalReason::NoData,
        EntryRejection::UnsupportedOverride => EntryRefusalReason::UnsupportedOverride,
        EntryRejection::OverCap => EntryRefusalReason::OverCap,
    }
}

/// Routes what one ingest of the override buffer did: one event per admission it attempted, in the
/// order the frame named its entries, plus what its rotating sweep dropped.
///
/// The counter loses that order, which is the whole content of a sequence like "swept, evicted,
/// admitted", so the log keeps the events as one list beside it.
fn record_override_ingest(events: &[IngestEvent]) {
    for event in events {
        telemetry::record_override_buffer_event(*event);
    }
    tracing::debug!(?events, "Admitted a pAMM override frame");
}

/// Routes the outcome of attributing one pair's ladders to a snapshot.
///
/// Spelled out rather than derived, so a new [`MatchResult`] variant has to be classified here.
/// Every outcome is counted, the healthy one included: `matched` against the rest is the share of
/// served pairs that carry what priced them, which is the signal that the join is working at all.
/// Only the outcomes that leave a state unstamped are logged — a matched one happens for most
/// books of every frame.
///
/// `outcome` and `venue` are the parts fit to label a metric — `venue` exactly as the per-venue
/// series already carry it, an address only when the venue was auto-detected. **`component` is
/// log-only, and must never become a label:** it is a venue address plus both token addresses, so
/// labelling it would open one series per pair Titan mentions, which is the unbounded growth
/// [`MAX_UNREGISTERED_LOGGED`] and the no-address-as-label rule exist to prevent. The same holds
/// for every other address, venue and token value on this path.
fn record_stamping(venue: &str, component: &str, outcome: &MatchResult) {
    let counted = match outcome {
        MatchResult::Matched(_) => MatchOutcome::Matched,
        MatchResult::Missing => MatchOutcome::Missing,
        MatchResult::Ambiguous => MatchOutcome::Ambiguous,
        MatchResult::Expired => MatchOutcome::Expired,
        MatchResult::Poisoned => MatchOutcome::Poisoned,
    };
    telemetry::record_override_match(counted);
    match counted {
        MatchOutcome::Matched => {}
        MatchOutcome::Missing |
        MatchOutcome::Ambiguous |
        MatchOutcome::Expired |
        MatchOutcome::Poisoned => tracing::debug!(
            venue,
            component,
            // Keyed as the metric labels it, so one query term reaches both.
            outcome = <&'static str>::from(counted),
            "Serving pAMM ladders without the simulated pre-state they were priced on"
        ),
    }
}

/// The telemetry reason a side rejected by the conversion is counted under. Spelled out rather
/// than derived, so a new [`SideRejection`] variant has to be given a label here.
fn side_reject_reason(rejection: SideRejection) -> SideRejectReason {
    match rejection {
        SideRejection::UnsupportedLevel => SideRejectReason::UnsupportedLevel,
        SideRejection::NonFiniteAmount => SideRejectReason::NonFiniteAmount,
        SideRejection::ZeroAmount => SideRejectReason::ZeroAmount,
        SideRejection::ConflictingKnots => SideRejectReason::ConflictingKnots,
    }
}

fn build_component(
    tokens: &HashMap<Bytes, Token>,
    config: &PriceLevelStreamConfig,
    id: Bytes,
    token0: &Bytes,
    token1: &Bytes,
    via_router: bool,
) -> ProtocolComponent {
    let protocol_system =
        if via_router { config.fallback_protocol_system() } else { config.protocol_system() };
    ProtocolComponent::new(
        id,
        protocol_system.clone(),
        protocol_system,
        // Titan builds Ethereum L1 blocks; the stream carries no other chains.
        Chain::Ethereum,
        vec![tokens[token0].clone(), tokens[token1].clone()],
        vec![config.address.clone()],
        HashMap::from([(PAMM_ADDRESS_ATTRIBUTE.to_string(), config.address.clone())]),
        Bytes::default(),
        Utc::now().naive_utc(),
    )
}

/// The component identity of a (pAMM, pair) combination: `pamm ++ token0 ++ token1`.
fn component_id(pamm: &Bytes, token0: &Bytes, token1: &Bytes) -> Bytes {
    Bytes::from([pamm.as_ref(), token0.as_ref(), token1.as_ref()].concat())
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        time::{Duration, Instant},
    };

    use rstest::rstest;

    use super::{
        super::{
            config::DEFAULT_AUTO_DETECTED_GAS_COST,
            overrides::test_overrides,
            state::QUOTE_TTL,
            telemetry::{
                recorded::{
                    all_label_values, counter_registered, counter_value, gauge_value,
                    histogram_values, record_async, registered_series,
                },
                FRAMES_ACCEPTED, FRAMES_REJECTED, FRAME_AGE, LAST_SEEN, RECONNECTS, REJECTED_SIDES,
                SERVED_COMPONENTS, SERVING_STATE, SKIPPED_BOOKS, STALE_REMOVALS,
                UNREGISTERED_PAMM_ENTRIES,
            },
            test_support::*,
            titan::TitanLevel,
        },
        *,
    };

    /// 2026-09-05 16:09:18 UTC, the first frame of the live capture.
    const BASE_WALL_NANOS: u64 = 1_788_624_558_000_000_000;

    /// A test clock whose wall and monotonic time advance together: `at(s)` is `s` seconds after
    /// its base. Tests must pass non-decreasing readings, as a monotonic clock would give.
    struct Clock {
        base: u64,
        start: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self::based_at(BASE_WALL_NANOS)
        }

        /// A clock based at `base` wall nanoseconds, for the 2026-10-08 captures, whose stamps lie
        /// a month past [`BASE_WALL_NANOS`].
        fn based_at(base: u64) -> Self {
            Self { base, start: Instant::now() }
        }

        fn at(&self, seconds: u64) -> Now {
            self.after(Duration::from_secs(seconds))
        }

        /// The reading `elapsed` after the base. Sub-second offsets are what the stamps of the
        /// two 2026-10-08 captures lie apart by.
        fn after(&self, elapsed: Duration) -> Now {
            Now {
                wall_nanos: self.base +
                    u64::try_from(elapsed.as_nanos())
                        .expect("a test duration fits a u64 of nanoseconds"),
                monotonic: self.start + elapsed,
            }
        }
    }

    /// Settings serving the registered `configs` with the defaults, auto-detection off and the
    /// direct family.
    fn settings(configs: Vec<PriceLevelStreamConfig>) -> TrackerSettings {
        TrackerSettings {
            registry: configs
                .into_iter()
                .map(|config| (config.address.clone(), config))
                .collect(),
            denied: HashSet::new(),
            tokens: tokens(),
            bindings: SymbolBindings::default(),
            auto_detect: false,
            auto_detected_gas_cost: BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            stale_after: DEFAULT_STALE_AFTER,
            via_fallback_router: false,
            quote_guard: true,
            simulation_overrides: true,
        }
    }

    /// A tracker serving the registered `configs` with auto-detection off.
    fn tracker_serving(
        configs: Vec<PriceLevelStreamConfig>,
        via_fallback_router: bool,
    ) -> FreshnessTracker {
        FreshnessTracker::new(TrackerSettings { via_fallback_router, ..settings(configs) })
    }

    fn tracker() -> FreshnessTracker {
        tracker_serving(vec![fermiswap()], false)
    }

    /// Kipseli under a custom registration on its canonical public identity, which is not the
    /// address its books are streamed under.
    fn kipseli_custom() -> PriceLevelStreamConfig {
        PriceLevelStreamConfig::new("custom-kipseli", address(KIPSELI), BigUint::from(42_000u64))
    }

    fn level(price: f64, cum_volume: f64) -> TitanLevel {
        TitanLevel { price, cum_volume }
    }

    fn quote(amount_in: u64, amount_out: u64) -> PriceLevelStreamQuote {
        PriceLevelStreamQuote::new(BigUint::from(amount_in), BigUint::from(amount_out))
    }

    /// A book of the venue at `quote_address` pricing `token1`/`token2` with the given sides.
    fn book(
        quote_address: &str,
        token1: &str,
        token2: &str,
        bids: Vec<TitanLevel>,
        asks: Vec<TitanLevel>,
    ) -> TitanBook {
        TitanBook {
            maker: Some("test".to_string()),
            quote_address: address(quote_address),
            token1: token1.to_string(),
            token2: token2.to_string(),
            calculated_from: Provenance::Absent,
            asks,
            bids,
        }
    }

    /// A two-sided book of the venue at `quote_address`: one whole `token1` sells for `price`
    /// `token2`, and `price` `token2` buys one whole `token1`.
    fn two_sided(quote_address: &str, token1: &str, token2: &str, price: f64) -> TitanBook {
        book(quote_address, token1, token2, vec![level(price, 1.0)], vec![level(price, 1.0)])
    }

    /// A frame built `seconds` after the base instant.
    fn message_at(block: u64, seconds: u64, books: Vec<TitanBook>) -> TitanPriceLevelMessage {
        TitanPriceLevelMessage {
            block,
            slot: Some(15_387_623),
            ts: BASE_WALL_NANOS + seconds * NANOS_PER_SECOND,
            books,
        }
    }

    fn message(block: u64, books: Vec<TitanBook>) -> TitanPriceLevelMessage {
        message_at(block, 0, books)
    }

    /// The FermiSwap WBTC/USDC book: 1 WBTC ⇄ 100,000 USDC, both directions quoted.
    fn wbtc_usdc_books() -> Vec<TitanBook> {
        vec![two_sided(PAMM, "WBTC", "USDC", 100_000.0)]
    }

    /// The FermiSwap WETH/USDC book: 1 WETH ⇄ 3,000 USDC, both directions quoted.
    fn weth_usdc_books() -> Vec<TitanBook> {
        vec![two_sided(PAMM, "WETH", "USDC", 3_000.0)]
    }

    /// Both FermiSwap books in one frame, as two components of one venue.
    fn wbtc_and_weth_books() -> Vec<TitanBook> {
        vec![two_sided(PAMM, "WBTC", "USDC", 100_000.0), two_sided(PAMM, "WETH", "USDC", 3_000.0)]
    }

    /// The component id of the WBTC/USDC pair of the venue at `pamm`: WBTC sorts below USDC.
    fn wbtc_usdc_id(pamm: &str) -> String {
        format!("{pamm}{}{}", &WBTC[2..], &USDC[2..])
    }

    /// The component id of the WETH/USDC pair of the venue at `pamm`: USDC sorts below WETH.
    fn weth_usdc_id(pamm: &str) -> String {
        format!("{pamm}{}{}", &USDC[2..], &WETH[2..])
    }

    fn expected_id() -> String {
        wbtc_usdc_id(PAMM)
    }

    fn state_of(update: &Update) -> &PriceLevelStreamState {
        update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state")
    }

    fn quotable_until(update: &Update) -> Instant {
        state_of(update)
            .quotable_until()
            .expect("quotable_until set")
    }

    #[test]
    fn first_frame_emits_new_pair_with_both_directions() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let Update {
            block_number_or_timestamp,
            is_partial,
            sync_states,
            states,
            new_pairs,
            removed_pairs,
        } = tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        assert_eq!(block_number_or_timestamp, 100);
        assert!(is_partial);
        assert!(sync_states.is_empty());
        assert!(removed_pairs.is_empty());

        let id = expected_id();
        let component = &new_pairs[&id];
        assert_eq!(component.protocol_system, "pricelevelstream:fermiswap");
        assert_eq!(
            component.static_attributes[PAMM_ADDRESS_ATTRIBUTE],
            Bytes::from_str(PAMM).unwrap()
        );

        let state = states[&id]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.token0, Bytes::from_str(WBTC).unwrap());
        assert_eq!(state.token1, Bytes::from_str(USDC).unwrap());
        assert_eq!(state.quotes_0_to_1.len(), 1);
        assert_eq!(state.quotes_1_to_0.len(), 1);
        assert_eq!(state.quotes_0_to_1[0].amount_in, BigUint::from(100_000_000u64));
        assert_eq!(state.gas_cost, BigUint::from(120_000u64));
        assert!(state.quotable_until().is_some());
    }

    #[test]
    fn repeated_frame_is_not_a_new_pair() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        let update = tracker
            .on_frame(message(101, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        assert!(update.new_pairs.is_empty());
        assert!(update.removed_pairs.is_empty());
        assert!(update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn block_regression_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(101, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        assert!(tracker
            .on_frame(message_at(100, 1, weth_usdc_books()), clock.at(1))
            .is_none());

        let update = tracker
            .on_frame(message_at(102, 2, wbtc_usdc_books()), clock.at(2))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    /// Judged at t=24, a frame built at t=0 is exactly `stale_after` old and already stale; one
    /// built a second later is accepted.
    #[rstest]
    #[case::at_the_limit(0, false)]
    #[case::inside_the_limit(1, true)]
    fn frame_age_at_the_stale_after_limit(#[case] built_at: u64, #[case] accepted: bool) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let update = tracker.on_frame(message_at(100, built_at, wbtc_usdc_books()), clock.at(24));
        assert_eq!(update.is_some(), accepted);
    }

    /// Judged at t=0, a frame stamped 13 s ahead lies beyond the one-slot skew; 12 s ahead is
    /// accepted.
    #[rstest]
    #[case::beyond_the_limit(13, false)]
    #[case::at_the_limit(12, true)]
    fn frame_timestamp_at_the_future_skew_limit(#[case] built_at: u64, #[case] accepted: bool) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let update = tracker.on_frame(message_at(100, built_at, wbtc_usdc_books()), clock.at(0));
        assert_eq!(update.is_some(), accepted);
    }

    #[test]
    fn older_timestamp_frame_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 5, wbtc_usdc_books()), clock.at(5))
            .expect("update expected");
        assert!(tracker
            .on_frame(message_at(100, 4, wbtc_usdc_books()), clock.at(5))
            .is_none());
    }

    #[test]
    fn equal_timestamp_frame_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 5, wbtc_usdc_books()), clock.at(5))
            .expect("update expected");
        // Titan re-emits within a build round with the same `ts` and different content.
        let changed = vec![two_sided(PAMM, "WBTC", "USDC", 101_000.0)];
        let update = tracker
            .on_frame(message_at(100, 5, changed), clock.at(5))
            .expect("update expected");
        assert!(update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn block_jump_beyond_the_elapsed_bound_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // One second later the chain cannot have advanced by more than 0 + 2 blocks.
        assert!(tracker
            .on_frame(message_at(103, 1, wbtc_usdc_books()), clock.at(1))
            .is_none());
        assert!(tracker
            .on_frame(message_at(u64::MAX / 2, 1, wbtc_usdc_books()), clock.at(1))
            .is_none());
        // A plausible block is still accepted afterwards.
        assert!(tracker
            .on_frame(message_at(101, 2, wbtc_usdc_books()), clock.at(2))
            .is_some());
    }

    #[test]
    fn block_jump_within_the_elapsed_bound_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // 60 s later: 60 / 12 + 2 = 7 blocks allowed.
        assert!(tracker
            .on_frame(message_at(107, 60, wbtc_usdc_books()), clock.at(60))
            .is_some());
    }

    #[test]
    fn rejections_are_counted_by_reason() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 5, wbtc_usdc_books()), clock.at(5));
            // Still fresh (16 s old) but stamped before the accepted frame: out_of_order.
            tracker.on_frame(message_at(100, 4, wbtc_usdc_books()), clock.at(20));
            tracker.on_frame(message_at(100, 5, wbtc_usdc_books()), clock.at(29)); // too_old
            tracker.on_frame(message_at(100, 45, wbtc_usdc_books()), clock.at(29)); // in_future
            tracker.on_frame(message_at(99, 6, wbtc_usdc_books()), clock.at(29)); // block_regression
            tracker.on_frame(message_at(200, 6, wbtc_usdc_books()), clock.at(29)); // block_jump
        });
        assert_eq!(counter_value(&snapshot, FRAMES_ACCEPTED, &[]), 1);
        for reason in ["too_old", "in_future", "out_of_order", "block_regression", "block_jump"] {
            assert_eq!(
                counter_value(
                    &snapshot,
                    FRAMES_REJECTED,
                    &[("reason", reason), ("socket", "price_levels")]
                ),
                1,
                "{reason}"
            );
        }
    }

    #[test]
    fn accepted_frame_records_its_signed_age() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            // Built at t=7, accepted at t=9: 2 s old.
            tracker.on_frame(message_at(100, 7, wbtc_usdc_books()), clock.at(9));
            // Stamped 3 s ahead of the local clock: a negative age, so a skew is visible.
            tracker.on_frame(message_at(100, 12, wbtc_usdc_books()), clock.at(9));
            // Rejected frames record no age.
            tracker.on_frame(message_at(100, 6, wbtc_usdc_books()), clock.at(9));
        });
        assert_eq!(histogram_values(&snapshot, FRAME_AGE, &[]), vec![2.0, -3.0]);
    }

    #[test]
    fn auto_detection_stops_at_its_cap() {
        let (registered, snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker =
                FreshnessTracker::new(TrackerSettings { auto_detect: true, ..settings(vec![]) });
            for index in 0..(MAX_AUTO_DETECTED + 2) as u64 {
                let venue = format!("0x{index:040x}");
                let frame = TitanPriceLevelMessage {
                    block: 100,
                    slot: None,
                    ts: BASE_WALL_NANOS + index * 1_000,
                    books: vec![two_sided(&venue, "WBTC", "USDC", 100_000.0)],
                };
                let served = tracker
                    .on_frame(frame, clock.at(1))
                    .is_some();
                assert_eq!(served, (index as usize) < MAX_AUTO_DETECTED, "venue {index}");
            }
            tracker.registry.len()
        });
        assert_eq!(registered, MAX_AUTO_DETECTED);
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_ENTRIES, &[]), 2);
    }

    #[test]
    fn quote_guard_off_emits_states_that_never_expire() {
        let clock = Clock::new();
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            quote_guard: false,
            ..settings(vec![fermiswap()])
        });
        // 15 s old: past `QUOTE_TTL`, so the guard alone would refuse every quote.
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(15))
            .expect("update expected");
        assert!(state_of(&update)
            .quotable_until()
            .is_none());
        // The component still turns stale on its own deadline.
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).monotonic
        );
    }

    #[test]
    fn quotable_until_is_one_block_after_the_frame_was_built() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=7, accepted at t=9: the data is 2 s old, so 10 s of quotability remain.
        let update = tracker
            .on_frame(message_at(100, 7, wbtc_usdc_books()), clock.at(9))
            .expect("update expected");
        assert_eq!(
            quotable_until(&update),
            clock.at(9).monotonic + (QUOTE_TTL - Duration::from_secs(2))
        );
    }

    #[test]
    fn quotable_until_never_exceeds_one_block_for_a_future_stamped_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Maximum accepted skew: stamped 12 s ahead of the local clock.
        let update = tracker
            .on_frame(message_at(100, 12, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(0).monotonic + QUOTE_TTL);
    }

    #[test]
    fn frame_older_than_one_block_yields_an_unquotable_state() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // 15 s old: inside the 24 s window, past `QUOTE_TTL`.
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(15))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(15).monotonic);
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).monotonic
        );
    }

    #[test]
    fn omitted_pair_is_not_removed_by_the_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let update = tracker
            .on_frame(message_at(101, 1, weth_usdc_books()), clock.at(1))
            .expect("update expected");
        assert!(update.removed_pairs.is_empty());
        assert_eq!(update.new_pairs.len(), 1);
        assert_eq!(update.states.len(), 1);
        assert!(!update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn omitted_venue_is_not_removed_by_the_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert!(tracker
            .on_frame(message_at(101, 1, Vec::new()), clock.at(1))
            .is_none());
        assert!(tracker.stale_deadline().is_some());
    }

    #[test]
    fn stale_deadline_fired_a_second_early_removes_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        assert!(tracker
            .on_stale_deadline(clock.at(23).monotonic)
            .is_none());
        assert!(tracker.stale_deadline().is_some());
    }

    #[test]
    fn component_turns_stale_at_its_deadline() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let update = tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");

        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert_eq!(update.removed_pairs.len(), 1);
        assert!(update
            .removed_pairs
            .contains_key(&expected_id()));
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update.is_partial);
        assert!(tracker.stale_deadline().is_none());
    }

    #[test]
    fn stale_deadline_fired_with_nothing_served_removes_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");

        assert!(tracker
            .on_stale_deadline(clock.at(25).monotonic)
            .is_none());
    }

    #[test]
    fn fresh_frame_after_the_stale_removal_re_adds_the_component() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");

        let update = tracker
            .on_frame(message_at(102, 26, wbtc_usdc_books()), clock.at(26))
            .expect("update expected");

        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn stale_removal_is_counted_per_venue() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker
                .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
                .expect("update expected");
            tracker
                .on_stale_deadline(clock.at(24).monotonic)
                .expect("removal expected");
        });
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("venue", "fermiswap")]), 1);
    }

    #[test]
    fn deadline_is_shortened_by_the_frame_age_at_acceptance() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=0, accepted at t=3: the data is already 3 s old.
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(3))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).monotonic
        );
    }

    #[test]
    fn replayed_frame_does_not_extend_the_deadline() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // The same frame arriving again 10 s later carries the same timestamp.
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(10))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).monotonic
        );
    }

    #[test]
    fn omitted_pair_turns_stale_alone_while_the_rest_stays_served() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = wbtc_and_weth_books();
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        // Only WETH/USDC keeps being carried, up to t=23; the clock never goes backwards.
        for second in 1..=23 {
            tracker.on_frame(
                message_at(100 + second / 12, second, weth_usdc_books()),
                clock.at(second),
            );
        }
        let update = tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");
        assert_eq!(update.removed_pairs.len(), 1);
        assert!(update
            .removed_pairs
            .contains_key(&expected_id()));
        // WETH/USDC is still served, with a deadline 24 s after its last frame at t=23.
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(47).monotonic
        );
    }

    #[test]
    fn components_from_one_frame_turn_stale_together() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = wbtc_and_weth_books();
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        let update = tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");
        assert_eq!(update.removed_pairs.len(), 2);
    }

    #[test]
    fn pair_set_oscillation_emits_no_removal() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let wide = || wbtc_and_weth_books();
        let narrow = weth_usdc_books;
        tracker
            .on_frame(message_at(100, 0, wide()), clock.at(0))
            .expect("update expected");
        for second in 1..=10u64 {
            let pairs = if second % 2 == 0 { wide() } else { narrow() };
            let update = tracker
                .on_frame(message_at(100, second, pairs), clock.at(second))
                .expect("update expected");
            assert!(update.removed_pairs.is_empty(), "second {second}");
            assert!(update.new_pairs.is_empty(), "second {second}");
        }
    }

    #[test]
    fn implausible_first_block_is_forgotten_at_stale_removal() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // A fresh first frame skips the block checks, so an implausible block becomes
        // `newest_block`.
        tracker
            .on_frame(message_at(u64::MAX / 2, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // Plausible frames regress below it and are rejected for the whole window.
        for second in 1..=23 {
            assert!(tracker
                .on_frame(message_at(100, second, wbtc_usdc_books()), clock.at(second))
                .is_none());
        }
        // The stale removal clears the served set and resets `newest_block`.
        let removal = tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");
        assert_eq!(removal.removed_pairs.len(), 1);
        assert_eq!(removal.block_number_or_timestamp, u64::MAX / 2);
        // The next plausible frame is accepted as a first frame again, and nothing from the
        // implausible frame survives: the pair comes back as new.
        let update = tracker
            .on_frame(message_at(100, 25, wbtc_usdc_books()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn implausible_block_on_a_frame_serving_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Fresh enough to be accepted, but its only book prices an unbound symbol, so the frame
        // serves nothing and its implausible block must not survive as `newest_block`.
        let unbound = vec![two_sided(PAMM, "NVDAx", "USDC", 180.0)];
        assert!(tracker
            .on_frame(message_at(u64::MAX / 2, 0, unbound), clock.at(0))
            .is_none());

        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_books()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn recovery_re_adds_only_the_pairs_the_frame_carries() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = wbtc_and_weth_books();
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");
        // After the outage only WETH/USDC comes back: WBTC/USDC stays absent.
        let update = tracker
            .on_frame(message_at(102, 25, weth_usdc_books()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.new_pairs.len(), 1);
        assert!(!update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    /// A book whose base symbol resolves to an address above its quote token's sorts the pair the
    /// other way round, so the bids become the `token1 → token0` ladder and the asks the
    /// `token0 → token1` one. Both sides carry distinct amounts, so a ladder landing in the wrong
    /// slot cannot pass: WETH sorts above USDC, the bids sell 1 WETH for 3,000 USDC, and the asks
    /// sell 6,200 USDC for 2 WETH.
    #[test]
    fn a_reversed_order_pair_keeps_each_direction_on_its_own_ladder() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let reversed =
            vec![book(PAMM, "WETH", "USDC", vec![level(3_000.0, 1.0)], vec![level(3_100.0, 2.0)])];

        let update = tracker
            .on_frame(message(100, reversed), clock.at(0))
            .expect("update expected");

        let state = update.states[&weth_usdc_id(PAMM)]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.token0, address(USDC));
        assert_eq!(state.token1, address(WETH));
        assert_eq!(state.quotes_0_to_1, vec![quote(6_200_000_000, 2_000_000_000_000_000_000)]);
        assert_eq!(state.quotes_1_to_0, vec![quote(1_000_000_000_000_000_000, 3_000_000_000)]);

        // The same, as the quoting behavior a consumer sees: each direction prices its own side.
        let usdc = token(USDC, "USDC", 6);
        let weth = token(WETH, "WETH", 18);
        let sold_weth = state
            .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &weth, &usdc)
            .expect("selling one WETH quotes");
        assert_eq!(sold_weth.amount, BigUint::from(3_000_000_000u64));
        let sold_usdc = state
            .get_amount_out(BigUint::from(6_200_000_000u64), &usdc, &weth)
            .expect("selling 6,200 USDC quotes");
        assert_eq!(sold_usdc.amount, BigUint::from(2_000_000_000_000_000_000u64));
    }

    #[test]
    fn one_direction_frame_re_adds_the_pair_with_the_other_direction_unquotable() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // A book quoted on the bid side only: USDC buys nothing.
        let one_way = vec![book(PAMM, "WBTC", "USDC", vec![level(100_000.0, 1.0)], Vec::new())];
        let update = tracker
            .on_frame(message_at(100, 0, one_way), clock.at(0))
            .expect("update expected");
        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.quotes_0_to_1.len(), 1);
        assert!(state.quotes_1_to_0.is_empty());
        let usdc = token(USDC, "USDC", 6);
        let wbtc = token(WBTC, "WBTC", 8);
        assert!(state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc, &wbtc)
            .is_err());
    }

    /// Every per-venue series of a registered venue exists from construction, so a venue that
    /// never appears in a message reads as a zero rather than a gap. `stale_removals_total` is a
    /// counter, where a zero value and an absent series are indistinguishable by value alone, so
    /// its presence is asserted directly.
    #[test]
    fn new_tracker_starts_every_registered_venue_series_at_zero() {
        let ((), snapshot) = record_async(async {
            let tracker = tracker();
            drop(tracker);
        });
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("venue", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, LAST_SEEN, &[("venue", "fermiswap")]), 0.0);
        assert!(
            counter_registered(&snapshot, STALE_REMOVALS, &[("venue", "fermiswap")]),
            "stale_removals_total is a missing series for a registered venue"
        );
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("venue", "fermiswap")]), 0);
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 0.0);
    }

    /// An auto-detected venue joins the registry mid-stream, so its per-venue series are
    /// registered then rather than at construction — including the removals counter, which would
    /// otherwise stay absent until the venue first lost a component.
    #[test]
    fn an_auto_detected_venue_starts_its_series_at_zero() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker =
                FreshnessTracker::new(TrackerSettings { auto_detect: true, ..settings(vec![]) });
            tracker
                .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
                .expect("update expected");
        });

        let venue = [("venue", PAMM)];
        assert!(
            counter_registered(&snapshot, STALE_REMOVALS, &venue),
            "stale_removals_total is a missing series for an auto-detected venue"
        );
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &venue), 0);
        // The venue is serving the book it was detected from, so this one is no longer zero.
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &venue), 1.0);
    }

    #[test]
    fn accepted_frame_gauges_its_venue_as_served() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0));
        });
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("venue", "fermiswap")]), 1.0);
        assert_eq!(
            gauge_value(&snapshot, LAST_SEEN, &[("venue", "fermiswap")]),
            (BASE_WALL_NANOS / NANOS_PER_SECOND) as f64
        );
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 1.0);
    }

    #[test]
    fn stale_removal_gauges_its_venue_back_to_zero() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0));
            tracker.on_stale_deadline(clock.at(24).monotonic);
        });
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("venue", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 0.0);
    }

    #[test]
    fn unregistered_pamm_produces_no_update_without_auto_detection() {
        let clock = Clock::new();
        let mut tracker = tracker_serving(vec![], false);
        assert!(tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .is_none());
    }

    #[test]
    fn denied_pamm_is_not_auto_detected() {
        let clock = Clock::new();
        let denied = HashSet::from([Bytes::from_str(PAMM).unwrap()]);
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            denied,
            auto_detect: true,
            ..settings(vec![])
        });
        assert!(tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .is_none());
    }

    #[test]
    fn auto_detected_pamm_is_served_under_its_address() {
        let clock = Clock::new();
        let mut tracker =
            FreshnessTracker::new(TrackerSettings { auto_detect: true, ..settings(vec![]) });
        let update = tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let component = &update.new_pairs[&expected_id()];
        assert_eq!(component.protocol_system, format!("pricelevelstream:{PAMM}"));
        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.gas_cost, BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));

        // The synthesized config is cached: the next frame is not a new pair again.
        let update = tracker
            .on_frame(message(101, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    #[test]
    fn auto_detected_gas_cost_override_applies() {
        let clock = Clock::new();
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            auto_detect: true,
            auto_detected_gas_cost: BigUint::from(42_000u64),
            ..settings(vec![])
        });
        let update = tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.gas_cost, BigUint::from(42_000u64));
    }

    #[test]
    fn unregistered_venues_counter_and_log_cap() {
        let (logged, snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker_serving(vec![], false);
            // 70 distinct unregistered addresses, each seen twice.
            for round in 0..2u64 {
                for index in 0..70u64 {
                    let venue = format!("0x{index:040x}");
                    let frame = TitanPriceLevelMessage {
                        block: 100,
                        slot: None,
                        ts: BASE_WALL_NANOS + (round * 70 + index) * 1_000,
                        books: vec![two_sided(&venue, "WBTC", "USDC", 100_000.0)],
                    };
                    tracker.on_frame(frame, clock.at(1));
                }
            }
            tracker.logged_unregistered.len()
        });
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_ENTRIES, &[]), 140);
        assert_eq!(logged, MAX_UNREGISTERED_LOGGED);
    }

    /// A symbol is served only if a binding names it and the consumer admits the bound asset.
    /// `NVDAx` is bound by nothing; `cbBTC` is bound but absent from the consumer's tokens.
    /// Either way the book is skipped and the frame serves nothing.
    #[rstest]
    #[case::unbound_symbol("NVDAx")]
    #[case::unadmitted_token("cbBTC")]
    fn books_naming_an_unserved_symbol_are_skipped(#[case] token1: &str) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let unserved = vec![two_sided(PAMM, token1, "USDC", 180.0)];
        assert!(tracker
            .on_frame(message(100, unserved), clock.at(0))
            .is_none());
    }

    /// Every book left unpriced is counted under the reason it failed on, so an asset a venue
    /// quotes but this consumer never serves is visible rather than silent.
    #[rstest]
    #[case::unbound_symbol("NVDAx", "unbound_symbol")]
    #[case::unadmitted_token("cbBTC", "unadmitted_token")]
    fn skipped_books_are_counted_by_reason(#[case] token1: &str, #[case] expected_reason: &str) {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker
                .on_frame(message(100, vec![two_sided(PAMM, token1, "USDC", 180.0)]), clock.at(0));
        });

        for reason in ["unbound_symbol", "unadmitted_token", "one_asset_pair"] {
            let expected = u64::from(reason == expected_reason);
            assert_eq!(
                counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", reason)]),
                expected,
                "{reason}"
            );
        }
    }

    /// Every dropped book side is counted under the conversion's own reason for rejecting it.
    /// Synthetic levels, one per reason, all on the bids of an otherwise valid WBTC/USDC book.
    #[rstest]
    #[case::unsupported_level(vec![level(0.0, 1.0)], "unsupported_level")]
    #[case::non_finite_amount(vec![level(f64::MAX, 1.0)], "non_finite_amount")]
    #[case::zero_amount(vec![level(100_000.0, 0.000_000_001)], "zero_amount")]
    #[case::conflicting_knots(
        vec![level(100_000.0, 1.0), level(101_000.0, 1.0)],
        "conflicting_knots"
    )]
    fn rejected_sides_are_counted_by_reason(
        #[case] bids: Vec<TitanLevel>,
        #[case] expected_reason: &str,
    ) {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            let corrupt = vec![book(PAMM, "WBTC", "USDC", bids, vec![level(100_000.0, 1.0)])];
            tracker
                .on_frame(message(100, corrupt), clock.at(0))
                .expect("the valid side is still served");
        });

        assert_eq!(counter_value(&snapshot, REJECTED_SIDES, &[("reason", expected_reason)]), 1);
    }

    /// A token symbol comes from Titan, so it must never become a metric label: the
    /// counters carry fixed reasons and the symbol appears only in the accompanying log. The
    /// frame here carries an unbound symbol, an unadmitted one and a corrupt side, so every
    /// symbol-driven counter fires.
    #[test]
    fn no_streamed_symbol_becomes_a_metric_label() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            let books = vec![
                two_sided(PAMM, "NVDAx", "USDC", 180.0),
                two_sided(PAMM, "cbBTC", "USDC", 80_000.0),
                book(PAMM, "WBTC", "USDC", vec![level(0.0, 1.0)], vec![level(100_000.0, 1.0)]),
            ];
            tracker
                .on_frame(message(100, books), clock.at(0))
                .expect("the WBTC/USDC asks are served");
        });

        // The counters did fire, so the assertion below is about a populated snapshot.
        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "unbound_symbol")]), 1);
        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "unadmitted_token")]), 1);
        assert_eq!(counter_value(&snapshot, REJECTED_SIDES, &[("reason", "unsupported_level")]), 1);

        let labels = all_label_values(&snapshot);
        for symbol in ["NVDAx", "cbBTC", "WBTC", "USDC"] {
            assert!(!labels.contains(&symbol), "{symbol} became a metric label: {labels:?}");
        }
    }

    /// Two symbols naming one asset state no pair, so the book is skipped — and counted — rather
    /// than served as a component that would quote an asset against itself.
    #[test]
    fn a_book_whose_symbols_name_one_asset_is_skipped() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = FreshnessTracker::new(TrackerSettings {
                bindings: SymbolBindings::with_overrides(HashMap::from([(
                    "USDbC".to_string(),
                    address(USDC),
                )])),
                ..settings(vec![fermiswap()])
            });
            let self_pair = vec![two_sided(PAMM, "USDC", "USDbC", 1.0)];
            assert!(tracker
                .on_frame(message(100, self_pair), clock.at(0))
                .is_none());
        });

        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "one_asset_pair")]), 1);
        // The pair failed on neither symbol: both resolved, to the same asset.
        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "unbound_symbol")]), 0);
        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "unadmitted_token")]), 0);
        let labels = all_label_values(&snapshot);
        for symbol in ["USDC", "USDbC"] {
            assert!(!labels.contains(&symbol), "{symbol} became a metric label: {labels:?}");
        }
    }

    /// An id is never in both `removed_pairs` and `new_pairs` of one update, and a removal
    /// carries no states.
    #[test]
    fn no_update_both_removes_and_adds_an_id() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let mut updates = Vec::new();
        updates.extend(tracker.on_frame(message_at(100, 0, wbtc_usdc_books()), clock.at(0)));
        updates.extend(tracker.on_stale_deadline(clock.at(24).monotonic));
        updates.extend(tracker.on_frame(message_at(102, 25, wbtc_usdc_books()), clock.at(25)));
        assert_eq!(updates.len(), 3);
        for update in &updates {
            for id in update.removed_pairs.keys() {
                assert!(!update.new_pairs.contains_key(id), "{id} removed and added");
                assert!(!update.states.contains_key(id), "{id} removed with a state");
            }
        }
    }

    /// Components are emitted under `fallback:{name}`, so their swaps execute through
    /// `TychoFallbackRouter`; identity and attributes are the same as on the direct path.
    #[test]
    fn venues_are_served_under_the_fallback_family() {
        let clock = Clock::new();
        let mut tracker = tracker_serving(vec![fermiswap()], true);
        let update = tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let component = &update.new_pairs[&expected_id()];
        assert_eq!(component.protocol_system, "fallback:fermiswap");
        assert_eq!(
            component.static_attributes[PAMM_ADDRESS_ATTRIBUTE],
            Bytes::from_str(PAMM).unwrap()
        );
    }

    /// Kipseli's captured books arrive under the address Titan streams them by, but the venue is
    /// served under its canonical public identity however it was configured: the component ids,
    /// the contract ids and the executable `pamm_address` all carry the public identity and never
    /// the streamed one. Only the venue name and the gas cost follow the configuration.
    #[rstest]
    #[case::registered_on_the_public_identity(
        vec![kipseli_custom()],
        false,
        "custom-kipseli",
        42_000
    )]
    #[case::auto_detected(vec![], true, KIPSELI, DEFAULT_AUTO_DETECTED_GAS_COST)]
    fn an_aliased_venue_is_served_under_its_public_identity(
        #[case] registry: Vec<PriceLevelStreamConfig>,
        #[case] auto_detect: bool,
        #[case] expected_name: &str,
        #[case] expected_gas: u64,
    ) {
        let clock = Clock::new();
        let mut tracker =
            FreshnessTracker::new(TrackerSettings { auto_detect, ..settings(registry) });

        let update = tracker
            .on_frame(message(100, captured_books_of("Kipseli")), clock.at(0))
            .expect("update expected");

        // Kipseli quoted WBTC/USDC and WETH/USDC in the capture.
        let public = address(KIPSELI);
        let mut ids: Vec<&String> = update.new_pairs.keys().collect();
        ids.sort();
        assert_eq!(ids, [&wbtc_usdc_id(KIPSELI), &weth_usdc_id(KIPSELI)]);
        for (id, component) in &update.new_pairs {
            assert!(!id.contains(&KIPSELI_STREAMED[2..]), "{id} carries the streamed address");
            assert_eq!(component.protocol_system, format!("pricelevelstream:{expected_name}"));
            assert_eq!(component.static_attributes[PAMM_ADDRESS_ATTRIBUTE], public);
            assert_eq!(component.contract_ids, vec![public.clone()]);
        }
        for state in update.states.values() {
            let state = state
                .as_any()
                .downcast_ref::<PriceLevelStreamState>()
                .expect("price level state");
            assert_eq!(state.gas_cost, BigUint::from(expected_gas));
        }
    }

    /// Denial is on the public identity, so a venue denied by the address its swaps go to stays
    /// unserved even though its books arrive under another address.
    #[test]
    fn denying_the_public_identity_denies_the_books_of_its_alias() {
        let clock = Clock::new();
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            denied: HashSet::from([address(KIPSELI)]),
            auto_detect: true,
            ..settings(vec![])
        });

        assert!(tracker
            .on_frame(message(100, captured_books_of("Kipseli")), clock.at(0))
            .is_none());
    }

    /// A side the conversion rejects costs that one direction, not the component: the opposite
    /// direction is served and still quotes. Synthetic: a bid at a zero price states no price.
    #[test]
    fn a_rejected_side_leaves_the_opposite_direction_served() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let corrupt_bids =
            vec![book(PAMM, "WBTC", "USDC", vec![level(0.0, 1.0)], vec![level(100_000.0, 1.0)])];

        let update = tracker
            .on_frame(message(100, corrupt_bids), clock.at(0))
            .expect("update expected");

        let state = state_of(&update);
        assert!(state.quotes_0_to_1.is_empty(), "the rejected side was served");
        assert_eq!(state.quotes_1_to_0.len(), 1);
        let usdc = token(USDC, "USDC", 6);
        let wbtc = token(WBTC, "WBTC", 8);
        // Selling USDC still quotes; selling WBTC cannot.
        assert!(state
            .get_amount_out(BigUint::from(100_000_000_000u64), &usdc, &wbtc)
            .is_ok());
        assert!(state
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc, &usdc)
            .is_err());
    }

    /// Auto-detected, address-named venues take the fallback family too.
    #[test]
    fn auto_detected_venue_is_served_under_the_fallback_family() {
        let clock = Clock::new();
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            auto_detect: true,
            via_fallback_router: true,
            ..settings(vec![])
        });
        let update = tracker
            .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, format!("fallback:{PAMM}"));
    }

    /// The nanosecond Titan built the TaurusFi entry of the captured override frame, which
    /// [`test_overrides`] transcribes. The capture clock is based here, so that entry is admitted
    /// at the instant it was stamped and its snapshot carries the `expires_at` the transcription
    /// records.
    const TAURUSFI_BUILT_AT_NANOS: u64 = 1_791_475_494_775_213_874;

    /// The nanosecond Titan stamped the price level frame whose books that override frame priced:
    /// 32.6 ms after the TaurusFi entry. Every entry of the frame is stamped within 42 ms of the
    /// base, so one clock reading admits all nine.
    const JOINED_FRAME_NANOS: u64 = 1_791_475_494_807_834_809;

    /// The block and beacon slot both captures were taken on.
    const CAPTURED_BLOCK: u64 = 26_148_760;
    const CAPTURED_SLOT: u64 = 15_387_623;

    /// The venues of the four captured entries whose snapshots the joining price level frame
    /// names, as Titan streams them. None is aliased, so each is also the venue's public identity.
    const TAURUSFI_STREAMED: &str = "0x3ce2672aa806138585920421406bf0decb8130cb";
    const GOLIATH: &str = "0x4367985a63063fc970b009c6e599e9225181f551";
    const BEBOP: &str = "0xb09aaa8933626d7e4c48d65dad2d77021cfbca9a";

    /// A tracker serving every venue of the 2026-10-08 captures through auto-detection, with the
    /// token metadata their books need.
    fn capture_tracker() -> FreshnessTracker {
        FreshnessTracker::new(TrackerSettings {
            auto_detect: true,
            tokens: captured_tokens(),
            ..settings(vec![])
        })
    }

    /// The joining price level frame, stamped as the capture holds it.
    fn joined_frame() -> TitanPriceLevelMessage {
        TitanPriceLevelMessage {
            block: CAPTURED_BLOCK,
            slot: Some(CAPTURED_SLOT),
            ts: JOINED_FRAME_NANOS,
            books: joined_books(),
        }
    }

    /// The component id of `venue`'s pair, in the token order the stream serves it under.
    fn pair_id(venue: &str, token_a: &str, token_b: &str) -> String {
        let (first, second) = (address(token_a), address(token_b));
        let (token0, token1) = if first <= second { (first, second) } else { (second, first) };
        component_id(&address(venue), &token0, &token1).to_string()
    }

    fn state_at<'a>(update: &'a Update, id: &str) -> &'a PriceLevelStreamState {
        update.states[id]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state")
    }

    /// The ids of `update`'s states that carry a snapshot, sorted.
    fn stamped_ids(update: &Update) -> Vec<String> {
        let mut ids: Vec<String> = update
            .states
            .keys()
            .filter(|id| {
                state_at(update, id)
                    .simulation_overrides()
                    .is_some()
            })
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    /// The two captures of 2026-10-08 join on six books across four venues — two of them pairs of
    /// one venue that share a sequence — and each of the six states carries the snapshot of the
    /// entry that priced it. The other thirteen served pairs carry none: the override frame
    /// advertised no entry for the pair version their books named.
    ///
    /// The TaurusFi state is asserted against the hand transcription of that entry, so the join
    /// is pinned to a snapshot read off the capture by eye rather than to the parser's own output.
    /// Its `expires_at` is the one difference: the stamped copy is narrowed to the second the
    /// ladders stop quoting at, which falls a second before the snapshot's own deadline here.
    #[test]
    fn the_captured_frames_stamp_every_book_they_join() {
        let clock = Clock::based_at(TAURUSFI_BUILT_AT_NANOS);
        let mut tracker = capture_tracker();
        tracker.on_override_frame(captured_override_frame(), clock.after(Duration::ZERO));

        let update = tracker
            .on_frame(joined_frame(), clock.after(Duration::from_millis(100)))
            .expect("update expected");

        assert_eq!(update.states.len(), 19, "a captured book went unserved");
        let mut joined = vec![
            pair_id(TAURUSFI_STREAMED, WETH, USDC),
            pair_id(GOLIATH, WETH, USDC),
            pair_id(PAMM, WBTC, USDC),
            pair_id(PAMM, WETH, USDC),
            pair_id(PAMM, CBBTC, USDC),
            pair_id(BEBOP, WETH, USDC),
        ];
        joined.sort();
        assert_eq!(stamped_ids(&update), joined);

        let transcribed = PammSimulationOverrides { expires_at: 1_791_475_505, ..test_overrides() };
        assert_eq!(
            state_at(&update, &pair_id(TAURUSFI_STREAMED, WETH, USDC)).simulation_overrides(),
            Some(&transcribed)
        );
    }

    /// The captured second, with no sub-second part, so the deadline arithmetic of a case is the
    /// one its comments state rather than one its base's fraction shifted.
    const WHOLE_SECOND_NANOS: u64 = 1_791_475_494_000_000_000;
    const WHOLE_SECOND: u64 = 1_791_475_494;

    /// A pair version above `u64::MAX`, as Titan's are, and the account a synthetic entry
    /// overrides.
    const SEQUENCE: u128 = 283_851_543_383_726_791_766_048_901;
    const SYNTHETIC_ACCOUNT: &str = "0xba443ecec71dc7123bd68c2082d13e3299cde896";

    /// A synthetic override entry, as the raw JSON of one venue entry: `venue` priced each of
    /// `pairs` — a `(token_1, token_2, sequence)` each — at `built_at_ns`, having overridden one
    /// account's `nonce`. The nonce is what tells two builds apart.
    fn override_entry(
        venue: &str,
        built_at_ns: u64,
        nonce: u64,
        pairs: &[(&str, &str, u128)],
    ) -> String {
        let asset_pairs: Vec<String> = pairs
            .iter()
            .map(|(token_1, token_2, _)| {
                format!(r#"{{"token_1": "{token_1}", "token_2": "{token_2}"}}"#)
            })
            .collect();
        let sequences: Vec<String> = pairs
            .iter()
            .map(|(_, _, sequence)| sequence.to_string())
            .collect();
        format!(
            r#"{{
                "maker": "Synthetic",
                "quoteAddress": "{venue}",
                "blockNumber": {CAPTURED_BLOCK},
                "slot": {CAPTURED_SLOT},
                "timestamp": {built_at_ns},
                "assetPairs": [{}],
                "pairSequences": [{}],
                "stateOverride": {{
                    "{SYNTHETIC_ACCOUNT}": {{"balance": "0x1", "nonce": "{nonce:#x}"}}
                }}
            }}"#,
            asset_pairs.join(", "),
            sequences.join(", ")
        )
    }

    /// An override frame carrying `entries`, parsed from the raw JSON they form.
    fn override_frame(entries: &[String]) -> TitanOverrideMessage {
        serde_json::from_str(&format!(r#"{{"venues": [{}]}}"#, entries.join(", ")))
            .expect("the override frame parses")
    }

    /// A one-entry override frame: `venue` priced `pairs` at `built_at_ns`, with nonce 1.
    fn one_entry(
        venue: &str,
        built_at_ns: u64,
        pairs: &[(&str, &str, u128)],
    ) -> TitanOverrideMessage {
        override_frame(&[override_entry(venue, built_at_ns, 1, pairs)])
    }

    /// A frame of `books` stamped `ts`, on the block and slot both captures were taken on.
    fn frame_at(ts: u64, books: Vec<TitanBook>) -> TitanPriceLevelMessage {
        TitanPriceLevelMessage { block: CAPTURED_BLOCK, slot: Some(CAPTURED_SLOT), ts, books }
    }

    /// A two-sided book of the venue at `quote_address`, priced at `sequence`: the shape of every
    /// book Titan sends, whose reference is what a snapshot is matched against.
    fn versioned(
        quote_address: &str,
        token1: &str,
        token2: &str,
        price: f64,
        sequence: u128,
    ) -> TitanBook {
        TitanBook {
            calculated_from: Provenance::Valid(sequence),
            ..two_sided(quote_address, token1, token2, price)
        }
    }

    /// The deadline of the snapshot `update` stamped onto the FermiSwap WBTC/USDC pair.
    fn stamped_deadline(update: &Update) -> u64 {
        state_of(update)
            .simulation_overrides()
            .expect("a stamped snapshot")
            .expires_at
    }

    /// A tracker serving FermiSwap and auto-detecting whatever else a frame carries.
    fn detecting_tracker() -> FreshnessTracker {
        FreshnessTracker::new(TrackerSettings { auto_detect: true, ..settings(vec![fermiswap()]) })
    }

    /// A book whose `calculated_from` this parser cannot read still quotes. The reference is
    /// metadata about the ladders, so an unreadable one costs the state its snapshot and nothing
    /// else — asserted on the raw JSON the tracker is fed, because a decode that failed the frame
    /// would cost every venue in it its quotes, which no parser-level test would notice.
    ///
    /// Synthetic references, each replacing the number Titan sends: a string, a negative, a
    /// fraction, a composite, and a number above `u128`; `null` is the control, the one shape Titan
    /// does send.
    /// A one-book FermiSwap frame whose `calculated_from` is the raw JSON `reference`, parsed as
    /// the stream parses it: the field is read leniently, so only a frame built from text covers
    /// what an unreadable reference does.
    fn referencing(reference: &str) -> TitanPriceLevelMessage {
        let text = format!(
            r#"{{"slot": 1, "block": 100, "ts": {BASE_WALL_NANOS}, "books": [
                {{"maker": "Fermi", "quoteAddress": "{PAMM}", "token1": "WBTC", "token2": "USDC",
                  "calculated_from": {reference},
                  "bids": [{{"price": 100000.0, "cumVolume": 1.0}}],
                  "asks": [{{"price": 100000.0, "cumVolume": 1.0}}]}}
            ]}}"#
        );
        serde_json::from_str(&text).expect("the frame parses")
    }

    #[rstest]
    #[case::text(r#""283851543383726791766048901""#)]
    #[case::negative("-1")]
    #[case::fraction("1.5")]
    #[case::composite("[283851543383726791766048901]")]
    #[case::above_u128("340282366920938463463374607431768211456")]
    #[case::absent("null")]
    fn a_book_with_an_unreadable_version_still_quotes(#[case] reference: &str) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let frame = referencing(reference);

        let update = tracker
            .on_frame(frame, clock.at(0))
            .expect("update expected");

        let state = state_of(&update);
        assert_eq!(state.simulation_overrides(), None);
        assert!(state
            .get_amount_out(
                BigUint::from(100_000_000u64),
                &token(WBTC, "WBTC", 8),
                &token(USDC, "USDC", 6)
            )
            .is_ok());
    }

    /// Two books of one venue for one pair in one frame that do not agree on one version of it:
    /// nothing attributes the ladders that end up served to one snapshot, so the pair is stamped
    /// from neither even though the buffer holds both builds. The ladders are the last book's,
    /// exactly as they are without any of this: a disagreement about what priced a pair never
    /// changes which quotes win.
    ///
    /// Both forms of disagreement are covered, because a careless reading of "conflicting
    /// references" collapses the second into "keep the version we have": two books naming
    /// different versions, and one naming a version against one naming none.
    ///
    /// Synthetic on two counts, neither observed in the captures: one venue quoting a pair twice
    /// in one frame, and the second book naming the pair the other way round — the two sockets
    /// name a pair in whichever orientation they please, so the reversal is the shape a join must
    /// survive.
    ///
    /// `alone_stamps` is whether the second book on its own would have been stamped, which is what
    /// makes the unstamped pair above a statement about the disagreement in the first case; in the
    /// second the book names nothing, so the control is the first book instead.
    #[rstest]
    #[case::different_versions(Provenance::Valid(SEQUENCE + 1), true)]
    #[case::one_book_named_none(Provenance::Absent, false)]
    fn duplicate_books_disagreeing_on_their_version_are_stamped_from_neither(
        #[case] second: Provenance,
        #[case] alone_stamps: bool,
    ) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let now = clock.after(Duration::ZERO);
        let first = || versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE);
        let reversed =
            || TitanBook { calculated_from: second, ..two_sided(PAMM, "USDC", "WBTC", 0.000_02) };
        let entry = |sequence| one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, sequence)]);

        let mut both = tracker();
        both.on_override_frame(entry(SEQUENCE), now);
        both.on_override_frame(entry(SEQUENCE + 1), now);
        let update = both
            .on_frame(frame_at(WHOLE_SECOND_NANOS, vec![first(), reversed()]), now)
            .expect("update expected");

        assert_eq!(state_of(&update).simulation_overrides(), None);

        // The ladders are the ones the second book alone would have served.
        let mut alone = tracker();
        alone.on_override_frame(entry(SEQUENCE), now);
        alone.on_override_frame(entry(SEQUENCE + 1), now);
        let single = alone
            .on_frame(frame_at(WHOLE_SECOND_NANOS, vec![reversed()]), now)
            .expect("update expected");
        assert_eq!(state_of(&update).quotes_0_to_1, state_of(&single).quotes_0_to_1);
        assert_eq!(state_of(&update).quotes_1_to_0, state_of(&single).quotes_1_to_0);
        assert_eq!(
            state_of(&single)
                .simulation_overrides()
                .is_some(),
            alone_stamps
        );

        // The build the surviving pair's other book named is held and does stamp on its own, so
        // the absence above is the disagreement and not a buffer that holds nothing.
        let mut uncontested = tracker();
        uncontested.on_override_frame(entry(SEQUENCE), now);
        let only_first = uncontested
            .on_frame(frame_at(WHOLE_SECOND_NANOS, vec![first()]), now)
            .expect("update expected");
        assert!(state_of(&only_first)
            .simulation_overrides()
            .is_some());
    }

    /// Both sockets name a venue by the address Titan streams it under and both canonicalize it,
    /// so an aliased venue's snapshot reaches its ladders whichever of the venue's two addresses
    /// the override entry names, and the stamp lands on the component built from the public
    /// identity rather than the streamed one.
    ///
    /// Kipseli is the one aliased venue and no venue of either capture is aliased, so this is the
    /// only cover for a regression that canonicalized one socket and not the other — which would
    /// silently stop stamping every aliased venue while every other test still passed.
    #[rstest]
    #[case::entry_under_the_streamed_address(KIPSELI_STREAMED)]
    #[case::entry_under_the_public_identity(KIPSELI)]
    fn an_aliased_venue_is_stamped_under_its_public_identity(#[case] entry_venue: &str) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let now = clock.after(Duration::ZERO);
        let mut tracker = detecting_tracker();
        tracker.on_override_frame(
            one_entry(entry_venue, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]),
            now,
        );

        // The book arrives under the streamed address, as Titan sends Kipseli's.
        let update = tracker
            .on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS,
                    vec![versioned(KIPSELI_STREAMED, "WBTC", "USDC", 100_000.0, SEQUENCE)],
                ),
                now,
            )
            .expect("update expected");

        assert_eq!(stamped_ids(&update), vec![pair_id(KIPSELI, WBTC, USDC)]);
        for id in update.states.keys() {
            assert!(!id.contains(&KIPSELI_STREAMED[2..]), "{id} carries the streamed address");
        }
    }

    /// The stamped deadline is derived from the frame's age, not from whether the quote guard is
    /// on. With the guard off the state never refuses to quote, and the snapshot it carries is
    /// still narrowed to the second the ladders stop quoting at — the same second the guard-on
    /// case of the same frame lands on — because that deadline is serialized and travels with the
    /// state, where `quotable_until` is a process-local instant that does not.
    #[test]
    fn stamping_narrows_the_deadline_with_the_quote_guard_off() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            quote_guard: false,
            ..settings(vec![fermiswap()])
        });
        tracker.on_override_frame(
            one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]),
            clock.after(Duration::ZERO),
        );

        // The 3.5 s old frame of the guard-on case whose ladders expire first.
        let update = tracker
            .on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS,
                    vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)],
                ),
                clock.after(Duration::from_millis(3_500)),
            )
            .expect("update expected");

        assert!(
            state_of(&update)
                .quotable_until()
                .is_none(),
            "the guard is off"
        );
        assert_eq!(stamped_deadline(&update), WHOLE_SECOND + 11);
    }

    /// Titan stamps a batch of pairs with one sequence, so a sequence identifies a build only
    /// together with the venue and the pair: the two pairs the entry advertised are stamped from
    /// its one snapshot, while a third pair of the same venue and the same pair of another venue
    /// are stamped from nothing, however exactly their books name that sequence.
    #[test]
    fn a_sequence_reaches_only_the_pairs_its_entry_advertised() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let now = clock.after(Duration::ZERO);
        let other_venue = "0x00000000000000000000000000000000deadbeef";
        let mut tracker = detecting_tracker();
        tracker.on_override_frame(
            one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE), (WETH, USDC, SEQUENCE)]),
            now,
        );

        let update = tracker
            .on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS,
                    vec![
                        versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE),
                        versioned(PAMM, "WETH", "USDC", 3_000.0, SEQUENCE),
                        versioned(PAMM, "WBTC", "WETH", 33.0, SEQUENCE),
                        versioned(other_venue, "WBTC", "USDC", 100_000.0, SEQUENCE),
                    ],
                ),
                now,
            )
            .expect("update expected");

        assert_eq!(update.states.len(), 4);
        let mut advertised = vec![pair_id(PAMM, WBTC, USDC), pair_id(PAMM, WETH, USDC)];
        advertised.sort();
        assert_eq!(stamped_ids(&update), advertised);
        // One build, shared by both pairs it was advertised for.
        assert_eq!(
            state_at(&update, &pair_id(PAMM, WBTC, USDC)).simulation_overrides(),
            state_at(&update, &pair_id(PAMM, WETH, USDC)).simulation_overrides()
        );
    }

    /// The deadline a stamped snapshot carries is the earlier of the two the state is bound by:
    /// the second its ladders stop quoting, and the second the snapshot itself must no longer be
    /// simulated. Either can be the earlier one — the ladders' when the frame arrived late in its
    /// slot, the snapshot's when the snapshot did.
    ///
    /// `admitted`, `built` and `accepted` are milliseconds from the base second, which carries no
    /// fraction, so each case's arithmetic is the one it states. The snapshot is always built at
    /// the base second: Titan repeats a ladder for a pair whose version has not changed, so a
    /// frame naming a build several seconds older than itself is ordinary traffic.
    #[rstest]
    // The snapshot is admitted at once and keeps a full slot, to second 12. The frame is 3.5 s
    // old, so its ladders quote 8 more seconds from second 3 — second 11, and the earlier one.
    #[case::the_ladders_expire_first(0, 0, 3_500, WHOLE_SECOND + 11)]
    // The snapshot is admitted 5.5 s into its slot and keeps the rest of it: its deadline is one
    // slot from its stamp — second 12 — whenever inside that slot it was admitted. The frame is
    // fresh, so its ladders quote a full slot from second 5: second 17, and the snapshot's is the
    // earlier one.
    #[case::the_snapshot_expires_first(5_500, 5_500, 5_500, WHOLE_SECOND + 12)]
    fn a_stamped_snapshot_expires_with_whichever_deadline_comes_first(
        #[case] admitted: u64,
        #[case] built: u64,
        #[case] accepted: u64,
        #[case] expected: u64,
    ) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        tracker.on_override_frame(
            one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]),
            clock.after(Duration::from_millis(admitted)),
        );

        let update = tracker
            .on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS + built * 1_000_000,
                    vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)],
                ),
                clock.after(Duration::from_millis(accepted)),
            )
            .expect("update expected");

        assert_eq!(stamped_deadline(&update), expected);
    }

    /// Narrowing goes on a copy: the build the buffer holds keeps its own deadline, so the next
    /// frame that names it is bound by its own ladders and not by an earlier frame's.
    #[test]
    fn narrowing_a_stamped_deadline_leaves_the_retained_build_alone() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        tracker.on_override_frame(
            one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]),
            clock.after(Duration::ZERO),
        );
        let book = || vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)];

        // A 3.5 s old frame narrows what it is stamped with to second 11.
        let narrowed = tracker
            .on_frame(
                frame_at(WHOLE_SECOND_NANOS, book()),
                clock.after(Duration::from_millis(3_500)),
            )
            .expect("update expected");
        assert_eq!(stamped_deadline(&narrowed), WHOLE_SECOND + 11);

        // A fresh frame of the same ladders, naming the same build: bound by the snapshot's own
        // deadline, which the earlier narrowing did not touch.
        let fresh = tracker
            .on_frame(
                frame_at(WHOLE_SECOND_NANOS + 4_000_000_000, book()),
                clock.after(Duration::from_millis(4_000)),
            )
            .expect("update expected");
        assert_eq!(stamped_deadline(&fresh), WHOLE_SECOND + 12);
    }

    /// A build whose slot has passed stamps nothing, and a build two payloads contradicted each
    /// other over stamps nothing for the rest of its lifetime. Either way the ladders are served
    /// and quote: a state without its pre-state is the normal case, not a broken one.
    #[rstest]
    #[case::expired(false)]
    #[case::poisoned(true)]
    fn an_unusable_build_leaves_the_ladders_served_and_unstamped(#[case] poisoned: bool) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        let pairs = [(WBTC, USDC, SEQUENCE)];
        tracker.on_override_frame(
            one_entry(PAMM, WHOLE_SECOND_NANOS, &pairs),
            clock.after(Duration::ZERO),
        );
        // A second payload of the same build, overriding a different nonce: which state the venue
        // was priced on is unknowable from the stream, so the build is refused.
        if poisoned {
            tracker.on_override_frame(
                override_frame(&[override_entry(PAMM, WHOLE_SECOND_NANOS, 2, &pairs)]),
                clock.after(Duration::ZERO),
            );
        }
        // One slot on, the snapshot's lifetime has passed; the frame itself is fresh either way.
        let accepted = if poisoned { Duration::ZERO } else { SLOT };
        let frame_ts = WHOLE_SECOND_NANOS +
            u64::try_from(accepted.as_nanos()).expect("a test duration fits a u64");

        let update = tracker
            .on_frame(
                frame_at(frame_ts, vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)]),
                clock.after(accepted),
            )
            .expect("update expected");

        let state = state_of(&update);
        assert_eq!(state.simulation_overrides(), None);
        assert!(state
            .get_amount_out(
                BigUint::from(100_000_000u64),
                &token(WBTC, "WBTC", 8),
                &token(USDC, "USDC", 6)
            )
            .is_ok());
    }

    /// A venue entry the validation refuses whole costs that venue its stamp and nothing else: the
    /// rest of the frame is admitted and stamps what it priced. Synthetic: an entry that names no
    /// `timestamp`, so nothing orders one build of its snapshot against another.
    #[test]
    fn a_refused_entry_does_not_cost_the_rest_of_its_frame() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let now = clock.after(Duration::ZERO);
        let unstamped = override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &[(WETH, USDC, SEQUENCE)])
            .replace(&WHOLE_SECOND_NANOS.to_string(), "null");
        let mut tracker = tracker();

        tracker.on_override_frame(
            override_frame(&[
                unstamped,
                override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &[(WBTC, USDC, SEQUENCE)]),
            ]),
            now,
        );
        let update = tracker
            .on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS,
                    vec![
                        versioned(PAMM, "WETH", "USDC", 3_000.0, SEQUENCE),
                        versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE),
                    ],
                ),
                now,
            )
            .expect("update expected");

        assert_eq!(stamped_ids(&update), vec![pair_id(PAMM, WBTC, USDC)]);
    }

    /// With stamping off no snapshot is held, so no state carries one — and an override frame,
    /// which no socket delivers then, changes nothing if one arrives anyway.
    #[test]
    fn stamping_off_leaves_every_state_unstamped() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let now = clock.after(Duration::ZERO);
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            simulation_overrides: false,
            ..settings(vec![fermiswap()])
        });

        tracker
            .on_override_frame(one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]), now);
        let update = tracker
            .on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS,
                    vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)],
                ),
                now,
            )
            .expect("update expected");

        assert_eq!(state_of(&update).simulation_overrides(), None);
    }

    /// The metric names and label values below are the contract with dashboards and alerts, so
    /// they are spelled out rather than read off the constants and enums the counters are emitted
    /// under — as [`telemetry`](super::super::telemetry)'s own tests do.
    const OVERRIDE_MATCH_TOTAL: &str = "price_level_stream_override_match_total";
    const OVERRIDE_BUFFER_TOTAL: &str = "price_level_stream_override_buffer_total";
    const REFUSED_ENTRIES_TOTAL: &str = "price_level_stream_refused_override_entries_total";
    const INVALID_PROVENANCE_TOTAL: &str = "price_level_stream_invalid_provenance_total";

    /// Every join of a ladder to a snapshot is counted under the outcome it reached, so a feed
    /// that stops stamping says which of the five ways it stopped — a producer that never
    /// advertised the build (`missing`), one whose builds arrive too late to be used (`expired`),
    /// one contradicting itself (`poisoned`), and ladders this consumer cannot attribute to one
    /// build (`ambiguous`) all call for different fixes.
    ///
    /// One tracker per outcome, each driven into exactly one of them, so a count of one is the
    /// outcome named and not a frame that reached several.
    #[test]
    fn override_joins_are_counted_by_outcome() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::based_at(WHOLE_SECOND_NANOS);
            let now = clock.after(Duration::ZERO);
            let pairs = [(WBTC, USDC, SEQUENCE)];
            let book = || versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE);
            let entry = || one_entry(PAMM, WHOLE_SECOND_NANOS, &pairs);

            let mut matched = tracker();
            matched.on_override_frame(entry(), now);
            matched.on_frame(frame_at(WHOLE_SECOND_NANOS, vec![book()]), now);

            // Nothing was ever admitted for the version the book names.
            let mut missing = tracker();
            missing.on_frame(frame_at(WHOLE_SECOND_NANOS, vec![book()]), now);

            // Two books of one pair, the second naming no version: nothing attributes the ladders
            // that end up served to one snapshot.
            let mut ambiguous = tracker();
            ambiguous.on_override_frame(entry(), now);
            ambiguous.on_frame(
                frame_at(
                    WHOLE_SECOND_NANOS,
                    vec![book(), two_sided(PAMM, "USDC", "WBTC", 0.000_02)],
                ),
                now,
            );

            // The build's slot has passed by the time its ladders arrive.
            let mut expired = tracker();
            expired.on_override_frame(entry(), now);
            expired.on_frame(
                frame_at(WHOLE_SECOND_NANOS + SLOT.as_secs() * NANOS_PER_SECOND, vec![book()]),
                clock.after(SLOT),
            );

            // Two payloads claimed to be the one build, so what the venue was priced on is
            // unknowable.
            let mut poisoned = tracker();
            poisoned.on_override_frame(entry(), now);
            poisoned.on_override_frame(
                override_frame(&[override_entry(PAMM, WHOLE_SECOND_NANOS, 2, &pairs)]),
                now,
            );
            poisoned.on_frame(frame_at(WHOLE_SECOND_NANOS, vec![book()]), now);
        });

        for outcome in ["matched", "missing", "ambiguous", "expired", "poisoned"] {
            assert_eq!(
                counter_value(&snapshot, OVERRIDE_MATCH_TOTAL, &[("outcome", outcome)]),
                1,
                "{outcome}"
            );
        }
    }

    /// A build Titan stamped ahead of this host's clock is served for one slot from its
    /// admission, and repeating its envelope past that does not buy it a second slot: the pair
    /// goes unstamped, its ladders quote as they always do, and the next build stamps.
    ///
    /// That shape — a stamp ahead of the admitting clock — is the only one in which a replay
    /// could gain anything, and the [`overrides`](super::super::overrides) cases cover why. This
    /// is the path a solver sees it on: two sockets, one repeated frame, and a state that either
    /// carries the pre-state behind its price or carries nothing. Re-admitting the build would
    /// stamp states with a snapshot half a slot past the deadline its first admission fixed,
    /// which is what the `expired` count here is asserted against.
    #[test]
    fn a_replayed_expired_build_leaves_the_pair_unstamped() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::based_at(WHOLE_SECOND_NANOS);
            let pairs = [(WBTC, USDC, SEQUENCE)];
            let mut tracker = tracker();
            let built_ahead = WHOLE_SECOND_NANOS + 6 * NANOS_PER_SECOND;

            tracker.on_override_frame(
                one_entry(PAMM, built_ahead, &pairs),
                clock.after(Duration::ZERO),
            );
            // The identical envelope, repeated once the build's slot of life has passed.
            let expired_at = clock.after(SLOT);
            tracker.on_override_frame(one_entry(PAMM, built_ahead, &pairs), expired_at);

            let unstamped = tracker
                .on_frame(
                    frame_at(
                        WHOLE_SECOND_NANOS + SLOT.as_secs() * NANOS_PER_SECOND,
                        vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)],
                    ),
                    expired_at,
                )
                .expect("update expected");
            assert!(stamped_ids(&unstamped).is_empty(), "the replayed build was stamped");
            assert!(
                state_of(&unstamped)
                    .quotable_until()
                    .is_some(),
                "the ladders stopped quoting"
            );

            // The next build stamps as it always did: refusing a replay wedges nothing.
            let fresh = WHOLE_SECOND_NANOS + 13 * NANOS_PER_SECOND;
            let recovered_at = clock.after(Duration::from_secs(13));
            tracker.on_override_frame(
                one_entry(PAMM, fresh, &[(WBTC, USDC, SEQUENCE + 1)]),
                recovered_at,
            );
            let stamped = tracker
                .on_frame(
                    frame_at(fresh, vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE + 1)]),
                    recovered_at,
                )
                .expect("update expected");
            assert_eq!(stamped_ids(&stamped), vec![pair_id(PAMM, WBTC, USDC)]);
        });

        assert_eq!(
            counter_value(&snapshot, OVERRIDE_BUFFER_TOTAL, &[("event", "rejected_spent_replay")]),
            1
        );
        for (outcome, counted) in [("expired", 1), ("matched", 1), ("missing", 0)] {
            assert_eq!(
                counter_value(&snapshot, OVERRIDE_MATCH_TOTAL, &[("outcome", outcome)]),
                counted,
                "{outcome}"
            );
        }
    }

    /// An unreadable `calculated_from` is counted apart from an absent one. Both leave the ladders
    /// unstamped, but only the first says the producer sent something this parser does not
    /// understand, and the tracker's read of the field is the last place the two are still
    /// distinct — so a parse regression there cannot hide inside the ordinary miss rate.
    ///
    /// The third case is why the series is registered unconditionally: the reference is read off a
    /// served book, which this consumer reads whether or not it stamps anything, so with stamping
    /// off the counter must still *move*. Reading provenance only when a buffer exists would leave
    /// a permanently flat registered series advertising a path that cannot fire.
    #[rstest]
    #[case::unreadable(r#""283851543383726791766048901""#, true, 1)]
    #[case::absent("null", true, 0)]
    #[case::unreadable_without_stamping(r#""283851543383726791766048901""#, false, 1)]
    fn an_unreadable_version_is_counted_apart_from_an_absent_one(
        #[case] reference: &str,
        #[case] stamping: bool,
        #[case] counted: u64,
    ) {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = FreshnessTracker::new(TrackerSettings {
                simulation_overrides: stamping,
                ..settings(vec![fermiswap()])
            });
            tracker
                .on_frame(referencing(reference), clock.at(0))
                .expect("update expected");
        });

        assert!(
            counter_registered(&snapshot, INVALID_PROVENANCE_TOTAL, &[]),
            "invalid_provenance_total is a missing series"
        );
        assert_eq!(counter_value(&snapshot, INVALID_PROVENANCE_TOTAL, &[]), counted);
    }

    /// What one ingest did is counted event by event, and an entry the validation refused whole is
    /// counted on its own counter: a payload that never reached the buffer is a different failure,
    /// with a different fix, from one the buffer's retention rules turned away.
    #[test]
    fn override_ingests_are_counted_by_event() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::based_at(WHOLE_SECOND_NANOS);
            let now = clock.after(Duration::ZERO);
            let pairs = [(WBTC, USDC, SEQUENCE)];
            let mut tracker = tracker();

            tracker.on_override_frame(one_entry(PAMM, WHOLE_SECOND_NANOS, &pairs), now);
            tracker.on_override_frame(one_entry(PAMM, WHOLE_SECOND_NANOS, &pairs), now);
            tracker.on_override_frame(one_entry(PAMM, WHOLE_SECOND_NANOS + 1, &pairs), now);
            tracker.on_override_frame(
                override_frame(&[override_entry(PAMM, WHOLE_SECOND_NANOS + 1, 2, &pairs)]),
                now,
            );
            // An entry naming no `timestamp` is refused by the validation, so the buffer never
            // sees it.
            tracker.on_override_frame(
                override_frame(&[override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &pairs)
                    .replace(&WHOLE_SECOND_NANOS.to_string(), "null")]),
                now,
            );
        });

        for event in ["admitted", "replayed", "restamped", "conflict_poisoned"] {
            assert_eq!(
                counter_value(&snapshot, OVERRIDE_BUFFER_TOTAL, &[("event", event)]),
                1,
                "{event}"
            );
        }
        assert_eq!(counter_value(&snapshot, REFUSED_ENTRIES_TOTAL, &[("reason", "unstamped")]), 1);
        assert_eq!(
            counter_value(&snapshot, OVERRIDE_BUFFER_TOTAL, &[("event", "rejected_expired")]),
            0,
            "a refused entry was counted as an ingest event"
        );
    }

    /// A new tracker registers exactly the series this consumer can fill, and no others, so a
    /// dashboard tells something that never happened from a consumer that was never scraped —
    /// while a series nothing can ever move is never advertised at all.
    ///
    /// Three asymmetries are pinned here. The override path's counters exist only while stamping
    /// is on, because no socket fills the buffer otherwise. The override socket's own
    /// freshness-rejection series is `parse_error` and nothing else: every other reason is a check
    /// the tracker makes of a price level frame, so registering them under `quote_stream` would
    /// advertise five series no code path can reach. And the provenance counter is registered
    /// either way, because a book's reference is read whether or not anything is stamped.
    ///
    /// The full label vocabularies are pinned in [`telemetry`](super::super::telemetry)'s own
    /// tests; this is about which of them a given consumer gets.
    #[rstest]
    #[case::stamping_on(true)]
    #[case::stamping_off(false)]
    fn series_a_new_tracker_registers(#[case] stamping: bool) {
        let ((), snapshot) = record_async(async {
            drop(FreshnessTracker::new(TrackerSettings {
                simulation_overrides: stamping,
                ..settings(vec![fermiswap()])
            }));
        });

        for (name, label) in [
            (OVERRIDE_MATCH_TOTAL, ("outcome", "matched")),
            (OVERRIDE_BUFFER_TOTAL, ("event", "admitted")),
            (REFUSED_ENTRIES_TOTAL, ("reason", "unstamped")),
        ] {
            assert_eq!(
                counter_registered(&snapshot, name, &[label]),
                stamping,
                "{name} registration with stamping {stamping}"
            );
            assert_eq!(counter_value(&snapshot, name, &[label]), 0, "{name}");
        }
        assert!(counter_registered(&snapshot, INVALID_PROVENANCE_TOTAL, &[]));

        // Six rejection reasons and six reconnect reasons on the price level socket; the override
        // socket adds its one rejection reason and its own six reconnect reasons.
        assert_eq!(
            registered_series(&snapshot, FRAMES_REJECTED),
            if stamping { 7 } else { 6 },
            "frames_rejected series with stamping {stamping}"
        );
        assert_eq!(
            registered_series(&snapshot, RECONNECTS),
            if stamping { 12 } else { 6 },
            "reconnects series with stamping {stamping}"
        );
        for (name, labels) in [
            (FRAMES_REJECTED, [("socket", "price_levels"), ("reason", "too_old")]),
            (FRAMES_REJECTED, [("socket", "price_levels"), ("reason", "parse_error")]),
            (RECONNECTS, [("socket", "price_levels"), ("reason", "closed")]),
        ] {
            assert!(counter_registered(&snapshot, name, &labels), "{name} {labels:?}");
            assert_eq!(counter_value(&snapshot, name, &labels), 0, "{name} {labels:?}");
        }
        assert_eq!(
            counter_registered(
                &snapshot,
                FRAMES_REJECTED,
                &[("socket", "quote_stream"), ("reason", "parse_error")]
            ),
            stamping,
            "the override socket's parse_error with stamping {stamping}"
        );
        assert_eq!(
            counter_registered(
                &snapshot,
                RECONNECTS,
                &[("socket", "quote_stream"), ("reason", "closed")]
            ),
            stamping,
            "the override socket's reconnects with stamping {stamping}"
        );
        assert!(
            !counter_registered(
                &snapshot,
                FRAMES_REJECTED,
                &[("socket", "quote_stream"), ("reason", "too_old")]
            ),
            "a rejection reason the override socket cannot carry is registered"
        );
    }

    /// The join knows a component — a venue address plus both token addresses — and a pair version
    /// above `u64::MAX`, and neither may become a label: one series per pair Titan mentions is
    /// exactly the unbounded growth the no-address-as-label rule exists to prevent. Both stay in
    /// the DEBUG log that accompanies the counter.
    ///
    /// The venue is registered, so its `venue` label is the configured name; an auto-detected
    /// venue is named by its address by design, which is the one exception and not this case.
    #[test]
    fn no_streamed_value_becomes_a_metric_label() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::based_at(WHOLE_SECOND_NANOS);
            let now = clock.after(Duration::ZERO);
            let mut tracker = tracker();
            tracker.on_override_frame(
                one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]),
                now,
            );
            tracker
                .on_frame(
                    frame_at(
                        WHOLE_SECOND_NANOS,
                        vec![versioned(PAMM, "WBTC", "USDC", 100_000.0, SEQUENCE)],
                    ),
                    now,
                )
                .expect("update expected");
        });

        // The join did happen, so the assertion below is about a populated snapshot.
        assert_eq!(counter_value(&snapshot, OVERRIDE_MATCH_TOTAL, &[("outcome", "matched")]), 1);
        let labels = all_label_values(&snapshot);
        for streamed_value in [SEQUENCE.to_string(), pair_id(PAMM, WBTC, USDC), PAMM.to_string()] {
            assert!(
                !labels.contains(&streamed_value.as_str()),
                "{streamed_value} became a metric label: {labels:?}"
            );
        }
    }
}
