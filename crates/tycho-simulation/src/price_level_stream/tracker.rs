//! Turns Titan's price level messages into [`Update`]s.

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
    config::{canonical_pamm_address, PriceLevelStreamConfig},
    convert::{quotes_for_book, resolve_book_tokens, SideQuotes, SideRejection, UnresolvedToken},
    overrides::{slot_of, BufferEvents, OverrideBuffer, PammSimulationOverrides},
    state::{PriceLevelStreamQuote, PriceLevelStreamState, QUOTE_TTL},
    stream::PAMM_ADDRESS_ATTRIBUTE,
    telemetry::{self, MatchOutcome, MissingField, RejectReason, SideRejectReason, SkipReason},
    titan::{
        self, EntryRejection, QuoteSequence, TitanBook, TitanMessage, TitanPriceLevelMessage,
        TitanQuoteStreamMessage, VenueOverrides,
    },
    NANOS_PER_SECOND, SLOT,
};
use crate::protocol::models::{ProtocolComponent, Update};

/// How long a component stays served after the last accepted message that carried it: two slots.
/// Titan streams at 1 Hz and its messages arrive within about 3 s of being built, so 24 s absorbs
/// jitter and still removes a silent component within two blocks.
pub(super) const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(2 * SLOT.as_secs());

/// Furthest a message's `ts` may lie ahead of the local wall clock: one slot. Titan's
/// messages were observed at least 120 ms behind the local clock; anything ahead of it by more
/// than clock skew is implausible.
const MAX_FUTURE_SKEW_NANOS: u64 = SLOT.as_secs() * NANOS_PER_SECOND;

/// Extra blocks a message may jump beyond one block per elapsed slot. Titan builds at chain head +
/// 1 and sometimes + 2, so a message right after a block boundary may jump by two.
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

/// The wall-clock and monotonic time at which the tracker handles a message. The tracker never
/// reads a clock itself: the caller supplies both, so every decision is a function of its inputs.
#[derive(Clone, Copy, Debug)]
pub(super) struct Now {
    /// Wall clock, nanoseconds since the Unix epoch. Only ever compared against Titan's `ts`.
    pub wall_nanos: u64,
    /// Monotonic clock. Every deadline is computed from it, so a wall-clock jump cannot remove a
    /// component early or keep it served late.
    pub monotonic: Instant,
}

/// What one accepted price level message's states are built with, derived from the message and
/// its age at acceptance.
#[derive(Clone, Copy)]
struct MessageTiming {
    /// The block the message's quotes target.
    block: u64,
    /// The message's beacon slot, which a book's quote sequence is checked against.
    slot: Option<u64>,
    /// The message's `ts`, in nanoseconds since the Unix epoch.
    ts: u64,
    /// The instant the components the message carries turn stale.
    stale_at: Instant,
    /// The instant their states stop quoting, or `None` while the quote guard is off.
    quotable_until: Option<Instant>,
}

impl MessageTiming {
    fn new(
        message: &TitanPriceLevelMessage,
        now: Now,
        message_age: Duration,
        stale_after: Duration,
        quote_guard: bool,
    ) -> Self {
        // `QUOTE_TTL` counts from the message's `ts` and is enforced on the monotonic clock; it
        // does not depend on whether the quotes changed, because quiet venues repeat the same
        // quotes for minutes. A message whose `ts` is ahead of the local clock has age zero, so it
        // gets one slot of quotability and never more.
        let quotable_for = QUOTE_TTL.saturating_sub(message_age);
        Self {
            block: message.block,
            slot: message.slot,
            ts: message.ts,
            stale_at: now.monotonic + stale_after.saturating_sub(message_age),
            quotable_until: quote_guard.then(|| now.monotonic + quotable_for),
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

/// The newest accepted message while something is served. A later message may not target a block
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
    /// `frontier` the newest accepted message. Both exist only in this state, so a message
    /// accepted while nothing is served never binds the block checks of a later message.
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
    /// Whether messages from pAMMs absent from the registry get an address-named configuration
    /// synthesized (and cached in the registry) instead of being skipped.
    pub auto_detect: bool,
    /// The per-swap gas cost synthesized auto-detected configurations are served with.
    pub auto_detected_gas_cost: BigUint,
    /// How long a component stays served after the last accepted message that carried it; see
    /// [`DEFAULT_STALE_AFTER`].
    pub stale_after: Duration,
    /// Whether components are emitted under the `fallback:` family, so their swaps execute
    /// through `TychoFallbackRouter` instead of the venue directly.
    pub via_fallback_router: bool,
    /// Whether every emitted state refuses to quote once its message is [`QUOTE_TTL`] old.
    pub quote_guard: bool,
    /// Whether states carry the simulation overrides of their quotes. Off means no quote stream
    /// socket feeds this tracker, so nothing is stored, looked up or attached, and no match
    /// outcome is reported.
    pub simulation_overrides: bool,
}

/// Turns Titan messages into [`Update`]s. A message only adds or refreshes components: a component
/// (or a whole venue) that a message omits stays served until its own deadline passes in
/// [`on_stale_deadline`](Self::on_stale_deadline).
pub(super) struct FreshnessTracker {
    registry: HashMap<Bytes, PriceLevelStreamConfig>,
    denied: HashSet<Bytes>,
    tokens: HashMap<Bytes, Token>,
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
    /// The `ts` of the newest accepted message. Messages older than it are out of order; equal
    /// ones are accepted, because Titan can send several messages with one `ts`. Never reset.
    newest_timestamp_nanos: u64,
    /// Whether the last message was rejected. The first rejection of a streak logs at WARN and the
    /// rest at DEBUG; `price_level_stream_frames_rejected_total` counts every rejection.
    rejecting: bool,
    /// Unregistered venue addresses already logged, at most [`MAX_UNREGISTERED_LOGGED`].
    logged_unregistered: HashSet<Bytes>,
    /// Venues auto-detection has added to the registry, at most [`MAX_AUTO_DETECTED`].
    auto_detected: usize,
    /// The quote stream's venue entries, held until the books that name them arrive; `None` when
    /// the consumer opted out of simulation overrides.
    overrides: Option<OverrideBuffer>,
}

impl FreshnessTracker {
    pub(super) fn new(settings: TrackerSettings) -> Self {
        let TrackerSettings {
            registry,
            denied,
            tokens,
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
            init_venue_series(&config.protocol, simulation_overrides);
        }
        telemetry::init_book_counters();
        // The stream builds one tracker, so the tracker also registers the series the socket layer
        // fills, the quote stream socket's only when that socket exists. It registers the
        // overrides series only when overrides are on, because nothing else can move them.
        telemetry::init_socket_counters(simulation_overrides);
        if simulation_overrides {
            telemetry::init_override_counters();
        }
        Self {
            registry,
            denied,
            tokens,
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

    /// Returns the message's age if the message passes the timestamp and block checks, or the
    /// [`RejectReason`] that names the first failed check. The block checks bind only while
    /// something is served.
    fn check_message(
        &self,
        message: &TitanPriceLevelMessage,
        now: Now,
    ) -> Result<Duration, RejectReason> {
        let message_age = Duration::from_nanos(
            now.wall_nanos
                .saturating_sub(message.ts),
        );
        if message_age >= self.stale_after {
            return Err(RejectReason::TooOld);
        }
        if message.ts >
            now.wall_nanos
                .saturating_add(MAX_FUTURE_SKEW_NANOS)
        {
            return Err(RejectReason::InFuture);
        }
        if message.ts < self.newest_timestamp_nanos {
            return Err(RejectReason::OutOfOrder);
        }
        let ServingState::Serving { deadline: _, frontier } = self.serving_state else {
            return Ok(message_age);
        };
        if message.block < frontier.block {
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
        if message.block > allowed {
            return Err(RejectReason::BlockJump);
        }
        Ok(message_age)
    }

    /// Processes one price level message into an [`Update`], or `None` if the message is rejected
    /// (see [`RejectReason`]) or carries no book of a served pAMM that prices an admitted pair.
    ///
    /// One book carries both directions of its pair, so a venue needs only one book per pair. If
    /// a message carries several for one pair anyway (a stream anomaly, never observed), the last
    /// in stream order is the one served, with its own quote sequence (see [`served_pairs`]).
    pub(super) fn on_price_level_message(
        &mut self,
        message: TitanPriceLevelMessage,
        now: Now,
    ) -> Option<Update> {
        let message_age = match self.check_message(&message, now) {
            Ok(message_age) => message_age,
            Err(reason) => {
                self.log_rejection(reason, &message);
                // Every check here is one the tracker makes of a price level message;
                // `titan::messages` counts the quote stream socket's parse errors under its own
                // `socket` label.
                telemetry::record_frame_rejected(TitanPriceLevelMessage::SOCKET, reason);
                return None;
            }
        };
        self.rejecting = false;
        telemetry::record_frame_accepted();
        // Signed, unlike `message_age`: a message whose `ts` is ahead of the local clock records a
        // negative age, so a clock skew between Titan and this host shows on the histogram instead
        // of looking like a perfect feed.
        let age_nanos = i128::from(now.wall_nanos) - i128::from(message.ts);
        telemetry::record_frame_age(age_nanos as f64 / NANOS_PER_SECOND as f64);
        self.newest_timestamp_nanos = message.ts;
        if message.slot.is_none() {
            telemetry::record_message_without_slot();
        }

        let timing =
            MessageTiming::new(&message, now, message_age, self.stale_after, self.quote_guard);
        self.advance_override_window(&message);
        let (states, new_pairs) = self.serve_books(message.books, timing);

        self.settle(Frontier { block: message.block, accepted_at: now.monotonic });
        if states.is_empty() {
            return None;
        }
        Some(Update::new(message.block, states, new_pairs).set_is_partial(true))
    }

    /// Serves every pair of every served venue `books` carries: the state of each, and the
    /// component of each pair that was not served before.
    fn serve_books(
        &mut self,
        books: Vec<TitanBook>,
        timing: MessageTiming,
    ) -> (HashMap<String, Box<dyn ProtocolSim>>, HashMap<String, ProtocolComponent>) {
        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();
        for (pamm, books) in group_by_venue(books) {
            self.admit(&pamm);
            let Some(config) = self.registry.get(&pamm) else {
                continue;
            };
            telemetry::record_last_seen(&config.protocol, timing.ts / NANOS_PER_SECOND);

            for pair in served_pairs(&books, &self.tokens, timing.slot) {
                let id = component_id(&config.address, &pair.quotes.token0, &pair.quotes.token1);
                let id_string = id.to_string();
                match self.served.get_mut(&id_string) {
                    Some(served) => served.stale_at = timing.stale_at,
                    None => {
                        let component = build_component(
                            &self.tokens,
                            config,
                            id,
                            &pair.quotes.token0,
                            &pair.quotes.token1,
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
                let overrides = self
                    .overrides
                    .as_ref()
                    .and_then(|buffer| {
                        find_overrides(
                            buffer,
                            &config.protocol,
                            &pamm,
                            &pair,
                            &id_string,
                            timing.block,
                        )
                    });
                let PairQuotes { token0, token1, quotes_0_to_1, quotes_1_to_0 } = pair.quotes;
                let mut state = PriceLevelStreamState::new(
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    config.gas_cost.clone(),
                );
                if let Some(until) = timing.quotable_until {
                    state = state.with_quotable_until(until);
                }
                if let Some(overrides) = overrides {
                    state = state.with_simulation_overrides(overrides);
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
            init_venue_series(&config.protocol, self.overrides.is_some());
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

    /// Moves the overrides buffer's slot window to `message`'s slot, so the buffer only holds the
    /// entries this message's books, and the next ones, can still match.
    ///
    /// A message naming no slot falls back on the newest slot its books' quote sequences carry,
    /// and leaves the window where it was when no book carries a readable one.
    fn advance_override_window(&mut self, message: &TitanPriceLevelMessage) {
        let Some(buffer) = self.overrides.as_mut() else {
            return;
        };
        let newest_book_slot = || {
            message
                .books
                .iter()
                .filter_map(|book| match book.calculated_from {
                    QuoteSequence::Valid(sequence) => Some(slot_of(sequence)),
                    QuoteSequence::Absent | QuoteSequence::Unreadable => None,
                })
                .max()
        };
        let Some(slot) = message.slot.or_else(newest_book_slot) else {
            return;
        };
        let mut events = BufferEvents::new();
        buffer.advance(slot, &mut events);
        record_buffer_events(&events);
    }

    /// Stores the venue entries of one quote stream message.
    ///
    /// Returns no update, because an entry changes neither what is served nor how it quotes: the
    /// tracker holds it until a book of the price level socket names it, and attaches it to that
    /// book's state then. A state handed to the consumer before its entry arrived never changes.
    ///
    /// The buffer stores an entry only under the venues this tracker serves (see
    /// [`OverrideBuffer::store`]), which keeps memory to what can be matched. An auto-detected
    /// venue is served from its first book on, so the entries that arrive before that book are not
    /// stored, and the states of its first message get no overrides.
    pub(super) fn on_quote_stream_message(&mut self, message: TitanQuoteStreamMessage) {
        // Unreachable while overrides are off: the builder connects no quote stream socket then.
        let Some(buffer) = self.overrides.as_mut() else {
            return;
        };
        let mut events = BufferEvents::new();
        for outcome in titan::validate(message) {
            let entry = match outcome {
                Ok(entry) => entry,
                Err(reason) => {
                    record_refused_entry(reason);
                    continue;
                }
            };
            record_entry_shape(&entry);
            if !self
                .registry
                .contains_key(&canonical_pamm_address(&entry.quote_address))
            {
                telemetry::record_unserved_venue_entry();
            }
            buffer.store(entry, |venue| self.registry.contains_key(venue), &mut events);
        }
        record_buffer_events(&events);
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
            "Removing price level components: no accepted message carried them within stale_after"
        );
        // Carries the newest accepted block, so the removal orders after every update the
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
    /// message, and records the serving-state and per-venue gauges. Nothing served means
    /// `Unserved`, which forgets the frontier: the next message is judged as a first message, so a
    /// message with an implausible block cannot cause rejections for longer than `stale_after`.
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

    /// Logs the first rejected message of a streak at WARN and the rest at DEBUG.
    fn log_rejection(&mut self, reason: RejectReason, message: &TitanPriceLevelMessage) {
        let reason: &'static str = reason.into();
        if self.rejecting {
            tracing::debug!(
                reason,
                block = message.block,
                slot = ?message.slot,
                ts = message.ts,
                "Rejecting price level message"
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
            block = message.block,
            slot = ?message.slot,
            ts = message.ts,
            newest_block = ?newest_block,
            newest_timestamp_nanos = self.newest_timestamp_nanos,
            "Rejecting price level message; further rejections logged at debug until a message is \
             accepted"
        );
    }
}

/// Groups a message's books by the canonical public identity of the venue that quoted them,
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

/// One pair of one venue, as a price level message serves it: the quotes for it, and the quote
/// sequence the book that quoted them was priced at.
struct ServedPair {
    quotes: PairQuotes,
    quote_sequence: Option<u128>,
}

/// The quote sequence `book` was priced at, or `None` when it named none or one this parser cannot
/// read.
///
/// It counts an unreadable quote sequence, so a change in Titan's format cannot hide among books
/// that name none, and it counts a readable one whose slot is not the message's, because matching
/// relies on that slot.
fn quote_sequence_of(book: &TitanBook, slot: Option<u64>) -> Option<u128> {
    match book.calculated_from {
        QuoteSequence::Valid(sequence) => {
            if slot.is_some_and(|slot| slot_of(sequence) != slot) {
                telemetry::record_sequence_slot_mismatch(TitanPriceLevelMessage::SOCKET);
                tracing::debug!(
                    quote_address = %book.quote_address,
                    ?slot,
                    sequence,
                    "A book's quote sequence does not carry the message's beacon slot"
                );
            }
            Some(sequence)
        }
        QuoteSequence::Unreadable => {
            telemetry::record_unreadable_quote_sequence();
            tracing::debug!(
                quote_address = %book.quote_address,
                token1 = book.token1,
                token2 = book.token2,
                "Serving a book whose calculated_from this parser cannot read"
            );
            None
        }
        QuoteSequence::Absent => None,
    }
}

/// The pairs one venue's books price, in the stream order of each pair's first book.
///
/// One book carries both directions of its pair, so a venue needs only one book per pair. A
/// message carrying several for one pair is a stream anomaly, never observed: the last book's
/// quotes and quote sequence are served together, and the case is counted.
fn served_pairs(
    books: &[TitanBook],
    tokens: &HashMap<Bytes, Token>,
    slot: Option<u64>,
) -> Vec<ServedPair> {
    let mut pairs: Vec<ServedPair> = Vec::new();
    let mut positions: HashMap<(Bytes, Bytes), usize> = HashMap::new();
    for book in books {
        let Some(quotes) = quotes_for_pair(book, tokens) else {
            continue;
        };
        let quote_sequence = quote_sequence_of(book, slot);
        let key = (quotes.token0.clone(), quotes.token1.clone());
        let pair = ServedPair { quotes, quote_sequence };
        match positions.get(&key) {
            Some(&position) => {
                telemetry::record_duplicate_books();
                pairs[position] = pair;
            }
            None => {
                positions.insert(key, pairs.len());
                pairs.push(pair);
            }
        }
    }
    pairs
}

/// The simulation overrides to attach to the state of `venue`'s `pair`, recording which
/// [`MatchOutcome`] the lookup reached under `venue_name`. `block` is the block the price level
/// message targets; `component` names the pair's component in the logs.
fn find_overrides(
    buffer: &OverrideBuffer,
    venue_name: &str,
    venue: &Bytes,
    pair: &ServedPair,
    component: &str,
    block: u64,
) -> Option<std::sync::Arc<PammSimulationOverrides>> {
    let Some(sequence) = pair.quote_sequence else {
        telemetry::record_override_match(venue_name, MatchOutcome::NoQuoteSequence);
        return None;
    };
    let Some(found) = buffer.find(venue, &pair.quotes.token0, &pair.quotes.token1, sequence) else {
        telemetry::record_override_match(venue_name, MatchOutcome::NoneInSlot);
        tracing::debug!(
            venue = venue_name,
            component,
            sequence,
            "Serving pAMM quotes without simulation overrides: no entry in the book's slot at or \
             below its quote sequence"
        );
        return None;
    };
    let outcome = if found.same_sequence {
        MatchOutcome::SameSequence
    } else {
        MatchOutcome::EarlierSequence
    };
    telemetry::record_override_match(venue_name, outcome);
    if found.overrides.block_number != Some(block) {
        telemetry::record_override_block_mismatch();
        tracing::debug!(
            venue = venue_name,
            component,
            sequence,
            block,
            overrides_block = ?found.overrides.block_number,
            "Attaching pAMM simulation overrides that name another block than the message's"
        );
    }
    if found.merged_sources > 0 {
        telemetry::record_override_merged_sources(venue_name, found.merged_sources);
    }
    if found.skipped_sources > 0 {
        telemetry::record_override_skipped_sources(venue_name, found.skipped_sources);
    }
    Some(found.overrides)
}

/// One pair's quotes, in the token order [`PriceLevelStreamState`] and [`component_id`] use:
/// `token0` is the lower address.
struct PairQuotes {
    token0: Bytes,
    token1: Bytes,
    quotes_0_to_1: Vec<PriceLevelStreamQuote>,
    quotes_1_to_0: Vec<PriceLevelStreamQuote>,
}

/// The quotes of `book`, or `None` if it prices no pair this consumer can serve: an address that
/// is not 20 bytes (`malformed_address`), one absent from `tokens` (`unadmitted_token`), or two
/// addresses that name one asset (`one_asset_pair`).
///
/// A side the conversion rejects yields no quotes, so that one direction is unquotable while the
/// opposite direction keeps being served, as for a side the venue quoted empty.
fn quotes_for_pair(book: &TitanBook, tokens: &HashMap<Bytes, Token>) -> Option<PairQuotes> {
    let (base, quote) = match resolve_book_tokens(book, tokens) {
        Ok(resolved) => resolved,
        Err(unresolved) => {
            let (reason, address) = match unresolved {
                UnresolvedToken::Malformed(address) => (SkipReason::MalformedAddress, address),
                UnresolvedToken::Unadmitted(address) => (SkipReason::UnadmittedToken, address),
            };
            telemetry::record_skipped_book(reason);
            // The address comes from Titan, so it goes in the log and never in a metric label.
            tracing::debug!(
                quote_address = %book.quote_address,
                token1 = book.token1,
                token2 = book.token2,
                reason = <&'static str>::from(reason),
                %address,
                "Skipping book: it names no pair of admitted tokens"
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
            "Skipping book: both of its addresses name one asset"
        );
        return None;
    }

    let (sell_base, sell_quote) = quotes_for_book(book, base, quote);
    let sell_base = accepted_side(sell_base, book, "bids");
    let sell_quote = accepted_side(sell_quote, book, "asks");
    if base.address < quote.address {
        Some(PairQuotes {
            token0: base.address.clone(),
            token1: quote.address.clone(),
            quotes_0_to_1: sell_base,
            quotes_1_to_0: sell_quote,
        })
    } else {
        Some(PairQuotes {
            token0: quote.address.clone(),
            token1: base.address.clone(),
            quotes_0_to_1: sell_quote,
            quotes_1_to_0: sell_base,
        })
    }
}

/// The side's quotes, or none if the conversion rejected the side.
fn accepted_side(
    side_quotes: SideQuotes,
    book: &TitanBook,
    side: &'static str,
) -> Vec<PriceLevelStreamQuote> {
    side_quotes.unwrap_or_else(|rejection| {
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

/// Registers the per-venue series of `venue` at zero, so a venue that never appears is a visible
/// zero rather than a missing series.
fn init_venue_series(venue: &str, simulation_overrides: bool) {
    telemetry::record_served_components(venue, 0);
    telemetry::record_last_seen(venue, 0);
    telemetry::init_stale_removals(venue);
    if simulation_overrides {
        telemetry::init_override_venue_series(venue);
    }
}

/// Counts a venue entry the validation refused whole: the pairs it carried get no overrides from
/// it.
fn record_refused_entry(reason: EntryRejection) {
    telemetry::record_refused_override_entry(reason);
    tracing::debug!(reason = <&'static str>::from(reason), "Refusing a pAMM venue entry whole");
}

/// Records what an accepted venue entry looks like: the fields a consumer needs that it omitted,
/// its size against the per-entry caps, and whether its quote sequences carry its slot.
fn record_entry_shape(entry: &VenueOverrides) {
    let accounts = &entry.overrides.accounts;
    let slots = accounts
        .values()
        .map(|account| account.state_diff.len())
        .sum();
    telemetry::record_override_entry_size(accounts.len(), slots);
    if entry.overrides.block_number.is_none() {
        telemetry::record_override_entry_missing_field(MissingField::BlockNumber);
    }
    let Some(slot) = entry.slot else {
        telemetry::record_override_entry_missing_field(MissingField::Slot);
        return;
    };
    for pair in &entry.pairs {
        if slot_of(pair.sequence) != slot {
            telemetry::record_sequence_slot_mismatch(TitanQuoteStreamMessage::SOCKET);
            tracing::debug!(
                quote_address = %entry.quote_address,
                slot,
                sequence = pair.sequence,
                "A venue entry's quote sequence does not carry the entry's beacon slot"
            );
        }
    }
}

/// Counts what storing entries, or moving the slot window, did to the buffer, and logs the counts
/// at DEBUG.
fn record_buffer_events(events: &BufferEvents) {
    for (&event, &times) in events {
        telemetry::record_buffer_event(event, times);
    }
    if !events.is_empty() {
        tracing::debug!(?events, "Updated the pAMM simulation overrides buffer");
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
            config::{default_served_pamms, DEFAULT_AUTO_DETECTED_GAS_COST},
            state::QUOTE_TTL,
            telemetry::{
                recorded::{
                    all_label_values, counter_registered, counter_value, gauge_value,
                    histogram_values, record_async, SnapshotMap,
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

    /// 2026-09-05 16:09:18 UTC, the first message of the live capture.
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

        /// A clock based at `base` wall nanoseconds, for the 2026-10-16 captures, whose timestamps
        /// lie a month past [`BASE_WALL_NANOS`].
        fn based_at(base: u64) -> Self {
            Self { base, start: Instant::now() }
        }

        fn at(&self, seconds: u64) -> Now {
            self.after(Duration::from_secs(seconds))
        }

        /// The reading `elapsed` after the base. It takes a `Duration` because the two 2026-10-16
        /// captures' timestamps lie less than a second apart.
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
            token1_address: address(symbol_address(token1)),
            token2_address: address(symbol_address(token2)),
            calculated_from: QuoteSequence::Absent,
            asks,
            bids,
        }
    }

    /// A two-sided book of the venue at `quote_address`: one whole `token1` sells for `price`
    /// `token2`, and `price` `token2` buys one whole `token1`.
    fn two_sided(quote_address: &str, token1: &str, token2: &str, price: f64) -> TitanBook {
        book(quote_address, token1, token2, vec![level(price, 1.0)], vec![level(price, 1.0)])
    }

    /// A message built `seconds` after the base instant.
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

    /// Both FermiSwap books in one message, as two components of one venue.
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
    fn first_message_emits_new_pair_with_both_directions() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let Update { block_number, is_partial, sync_states, states, new_pairs, removed_pairs } =
            tracker
                .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
                .expect("update expected");

        assert_eq!(block_number, 100);
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
    fn repeated_message_is_not_a_new_pair() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        let update = tracker
            .on_price_level_message(message(101, wbtc_usdc_books()), clock.at(0))
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
            .on_price_level_message(message_at(101, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        assert!(tracker
            .on_price_level_message(message_at(100, 1, weth_usdc_books()), clock.at(1))
            .is_none());

        let update = tracker
            .on_price_level_message(message_at(102, 2, wbtc_usdc_books()), clock.at(2))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    /// Judged at t=24, a message built at t=0 is exactly `stale_after` old and already stale; one
    /// built a second later is accepted.
    #[rstest]
    #[case::at_the_limit(0, false)]
    #[case::inside_the_limit(1, true)]
    fn message_age_at_the_stale_after_limit(#[case] built_at: u64, #[case] accepted: bool) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let update = tracker
            .on_price_level_message(message_at(100, built_at, wbtc_usdc_books()), clock.at(24));
        assert_eq!(update.is_some(), accepted);
    }

    /// Judged at t=0, a message whose `ts` is 13 s ahead lies beyond the one-slot skew; 12 s ahead
    /// is accepted.
    #[rstest]
    #[case::beyond_the_limit(13, false)]
    #[case::at_the_limit(12, true)]
    fn message_timestamp_at_the_future_skew_limit(#[case] built_at: u64, #[case] accepted: bool) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let update = tracker
            .on_price_level_message(message_at(100, built_at, wbtc_usdc_books()), clock.at(0));
        assert_eq!(update.is_some(), accepted);
    }

    #[test]
    fn older_timestamp_message_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 5, wbtc_usdc_books()), clock.at(5))
            .expect("update expected");
        assert!(tracker
            .on_price_level_message(message_at(100, 4, wbtc_usdc_books()), clock.at(5))
            .is_none());
    }

    #[test]
    fn equal_timestamp_message_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 5, wbtc_usdc_books()), clock.at(5))
            .expect("update expected");
        // Titan re-emits within a build round with the same `ts` and different content.
        let changed = vec![two_sided(PAMM, "WBTC", "USDC", 101_000.0)];
        let update = tracker
            .on_price_level_message(message_at(100, 5, changed), clock.at(5))
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
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // One second later the chain cannot have advanced by more than 0 + 2 blocks.
        assert!(tracker
            .on_price_level_message(message_at(103, 1, wbtc_usdc_books()), clock.at(1))
            .is_none());
        assert!(tracker
            .on_price_level_message(message_at(u64::MAX / 2, 1, wbtc_usdc_books()), clock.at(1))
            .is_none());
        // A plausible block is still accepted afterwards.
        assert!(tracker
            .on_price_level_message(message_at(101, 2, wbtc_usdc_books()), clock.at(2))
            .is_some());
    }

    #[test]
    fn block_jump_within_the_elapsed_bound_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // 60 s later: 60 / 12 + 2 = 7 blocks allowed.
        assert!(tracker
            .on_price_level_message(message_at(107, 60, wbtc_usdc_books()), clock.at(60))
            .is_some());
    }

    #[test]
    fn rejections_are_counted_by_reason() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_price_level_message(message_at(100, 5, wbtc_usdc_books()), clock.at(5));
            // Still fresh (16 s old) but older than the accepted message's `ts`: out_of_order.
            tracker.on_price_level_message(message_at(100, 4, wbtc_usdc_books()), clock.at(20));
            // too_old
            tracker.on_price_level_message(message_at(100, 5, wbtc_usdc_books()), clock.at(29));
            // in_future
            tracker.on_price_level_message(message_at(100, 45, wbtc_usdc_books()), clock.at(29));
            // block_regression
            tracker.on_price_level_message(message_at(99, 6, wbtc_usdc_books()), clock.at(29));
            // block_jump
            tracker.on_price_level_message(message_at(200, 6, wbtc_usdc_books()), clock.at(29));
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
    fn accepted_message_records_its_signed_age() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            // Built at t=7, accepted at t=9: 2 s old.
            tracker.on_price_level_message(message_at(100, 7, wbtc_usdc_books()), clock.at(9));
            // A `ts` 3 s ahead of the local clock: a negative age, so a skew is visible.
            tracker.on_price_level_message(message_at(100, 12, wbtc_usdc_books()), clock.at(9));
            // Rejected messages record no age.
            tracker.on_price_level_message(message_at(100, 6, wbtc_usdc_books()), clock.at(9));
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
                let message = TitanPriceLevelMessage {
                    block: 100,
                    slot: None,
                    ts: BASE_WALL_NANOS + index * 1_000,
                    books: vec![two_sided(&venue, "WBTC", "USDC", 100_000.0)],
                };
                let served = tracker
                    .on_price_level_message(message, clock.at(1))
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
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(15))
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
    fn quotable_until_is_one_block_after_the_message_was_built() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=7, accepted at t=9: the data is 2 s old, so 10 s of quotability remain.
        let update = tracker
            .on_price_level_message(message_at(100, 7, wbtc_usdc_books()), clock.at(9))
            .expect("update expected");
        assert_eq!(
            quotable_until(&update),
            clock.at(9).monotonic + (QUOTE_TTL - Duration::from_secs(2))
        );
    }

    #[test]
    fn quotable_until_never_exceeds_one_block_for_a_message_ahead_of_the_clock() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Maximum accepted skew: a `ts` 12 s ahead of the local clock.
        let update = tracker
            .on_price_level_message(message_at(100, 12, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(0).monotonic + QUOTE_TTL);
    }

    #[test]
    fn message_older_than_one_block_yields_an_unquotable_state() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // 15 s old: inside the 24 s window, past `QUOTE_TTL`.
        let update = tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(15))
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
    fn omitted_pair_is_not_removed_by_the_message() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let update = tracker
            .on_price_level_message(message_at(101, 1, weth_usdc_books()), clock.at(1))
            .expect("update expected");
        assert!(update.removed_pairs.is_empty());
        assert_eq!(update.new_pairs.len(), 1);
        assert_eq!(update.states.len(), 1);
        assert!(!update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn omitted_venue_is_not_removed_by_the_message() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert!(tracker
            .on_price_level_message(message_at(101, 1, Vec::new()), clock.at(1))
            .is_none());
        assert!(tracker.stale_deadline().is_some());
    }

    #[test]
    fn stale_deadline_fired_a_second_early_removes_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
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
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
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
        assert_eq!(update.block_number, 100);
        assert!(update.is_partial);
        assert!(tracker.stale_deadline().is_none());
    }

    #[test]
    fn stale_deadline_fired_with_nothing_served_removes_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");

        assert!(tracker
            .on_stale_deadline(clock.at(25).monotonic)
            .is_none());
    }

    #[test]
    fn fresh_message_after_the_stale_removal_re_adds_the_component() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");

        let update = tracker
            .on_price_level_message(message_at(102, 26, wbtc_usdc_books()), clock.at(26))
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
                .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
                .expect("update expected");
            tracker
                .on_stale_deadline(clock.at(24).monotonic)
                .expect("removal expected");
        });
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("venue", "fermiswap")]), 1);
    }

    #[test]
    fn deadline_is_shortened_by_the_message_age_at_acceptance() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=0, accepted at t=3: the data is already 3 s old.
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(3))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).monotonic
        );
    }

    #[test]
    fn replayed_message_does_not_extend_the_deadline() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // The same message arriving again 10 s later carries the same timestamp.
        tracker
            .on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(10))
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
            .on_price_level_message(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        // Only WETH/USDC keeps being carried, up to t=23; the clock never goes backwards.
        for second in 1..=23 {
            tracker.on_price_level_message(
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
        // WETH/USDC is still served, with a deadline 24 s after its last message at t=23.
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(47).monotonic
        );
    }

    #[test]
    fn components_from_one_message_turn_stale_together() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = wbtc_and_weth_books();
        tracker
            .on_price_level_message(message_at(100, 0, both), clock.at(0))
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
            .on_price_level_message(message_at(100, 0, wide()), clock.at(0))
            .expect("update expected");
        for second in 1..=10u64 {
            let pairs = if second % 2 == 0 { wide() } else { narrow() };
            let update = tracker
                .on_price_level_message(message_at(100, second, pairs), clock.at(second))
                .expect("update expected");
            assert!(update.removed_pairs.is_empty(), "second {second}");
            assert!(update.new_pairs.is_empty(), "second {second}");
        }
    }

    #[test]
    fn implausible_first_block_is_forgotten_at_stale_removal() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // A fresh first message skips the block checks, so an implausible block becomes
        // `newest_block`.
        tracker
            .on_price_level_message(message_at(u64::MAX / 2, 0, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        // Plausible messages regress below it and are rejected for the whole window.
        for second in 1..=23 {
            assert!(tracker
                .on_price_level_message(
                    message_at(100, second, wbtc_usdc_books()),
                    clock.at(second)
                )
                .is_none());
        }
        // The stale removal clears the served set and resets `newest_block`.
        let removal = tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");
        assert_eq!(removal.removed_pairs.len(), 1);
        assert_eq!(removal.block_number, u64::MAX / 2);
        // The next plausible message is accepted as a first message again, and nothing from the
        // implausible message survives: the pair comes back as new.
        let update = tracker
            .on_price_level_message(message_at(100, 25, wbtc_usdc_books()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.block_number, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn implausible_block_on_a_message_serving_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Fresh enough to be accepted, but its only book prices an unbound symbol, so the message
        // serves nothing and its implausible block must not survive as `newest_block`.
        let unbound = vec![two_sided(PAMM, "NVDAx", "USDC", 180.0)];
        assert!(tracker
            .on_price_level_message(message_at(u64::MAX / 2, 0, unbound), clock.at(0))
            .is_none());

        let update = tracker
            .on_price_level_message(message_at(100, 1, wbtc_usdc_books()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.block_number, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn recovery_re_adds_only_the_pairs_the_message_carries() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = wbtc_and_weth_books();
        tracker
            .on_price_level_message(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24).monotonic)
            .expect("removal expected");
        // After the outage only WETH/USDC comes back: WBTC/USDC stays absent.
        let update = tracker
            .on_price_level_message(message_at(102, 25, weth_usdc_books()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.new_pairs.len(), 1);
        assert!(!update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    /// A book whose `token1` address sorts above its `token2` address orders the pair the other
    /// way round, so the bids become the `token1 → token0` quotes and the asks the
    /// `token0 → token1` ones. Both sides carry distinct amounts, so quotes landing in the wrong
    /// slot cannot pass: WETH sorts above USDC, the bids sell 1 WETH for 3,000 USDC, and the asks
    /// sell 6,200 USDC for 2 WETH.
    #[test]
    fn a_reversed_order_pair_keeps_each_direction_on_its_own_quotes() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let reversed =
            vec![book(PAMM, "WETH", "USDC", vec![level(3_000.0, 1.0)], vec![level(3_100.0, 2.0)])];

        let update = tracker
            .on_price_level_message(message(100, reversed), clock.at(0))
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
    fn one_direction_message_re_adds_the_pair_with_the_other_direction_unquotable() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // A book quoted on the bid side only: USDC buys nothing.
        let one_way = vec![book(PAMM, "WBTC", "USDC", vec![level(100_000.0, 1.0)], Vec::new())];
        let update = tracker
            .on_price_level_message(message_at(100, 0, one_way), clock.at(0))
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
                .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
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
    fn accepted_message_gauges_its_venue_as_served() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0));
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
            tracker.on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0));
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
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
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
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
            .is_none());
    }

    #[test]
    fn auto_detected_pamm_is_served_under_its_address() {
        let clock = Clock::new();
        let mut tracker =
            FreshnessTracker::new(TrackerSettings { auto_detect: true, ..settings(vec![]) });
        let update = tracker
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");

        let component = &update.new_pairs[&expected_id()];
        assert_eq!(component.protocol_system, format!("pricelevelstream:{PAMM}"));
        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.gas_cost, BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));

        // The synthesized config is cached: the next message is not a new pair again.
        let update = tracker
            .on_price_level_message(message(101, wbtc_usdc_books()), clock.at(0))
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
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
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
                    let message = TitanPriceLevelMessage {
                        block: 100,
                        slot: None,
                        ts: BASE_WALL_NANOS + (round * 70 + index) * 1_000,
                        books: vec![two_sided(&venue, "WBTC", "USDC", 100_000.0)],
                    };
                    tracker.on_price_level_message(message, clock.at(1));
                }
            }
            tracker.logged_unregistered.len()
        });
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_ENTRIES, &[]), 140);
        assert_eq!(logged, MAX_UNREGISTERED_LOGGED);
    }

    /// A pair is served only if the consumer admits both of its assets. Neither `NVDAx` nor
    /// `cbBTC` is in this consumer's token map, so either book is skipped and the message serves
    /// nothing.
    #[rstest]
    #[case::unadmitted_token("NVDAx")]
    #[case::another_unadmitted_token("cbBTC")]
    fn books_naming_an_unadmitted_asset_are_skipped(#[case] token1: &str) {
        let clock = Clock::new();
        let mut tracker = tracker();
        let unserved = vec![two_sided(PAMM, token1, "USDC", 180.0)];
        assert!(tracker
            .on_price_level_message(message(100, unserved), clock.at(0))
            .is_none());
    }

    /// Every book left unpriced is counted under the reason it failed on, so an asset a venue
    /// quotes but this consumer never serves is visible rather than silent.
    #[rstest]
    #[case::unadmitted_token("NVDAx", false, "unadmitted_token")]
    #[case::another_unadmitted_token("cbBTC", false, "unadmitted_token")]
    #[case::malformed_address("WBTC", true, "malformed_address")]
    fn skipped_books_are_counted_by_reason(
        #[case] token1: &str,
        #[case] malform: bool,
        #[case] expected_reason: &str,
    ) {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = tracker();
            let mut book = two_sided(PAMM, token1, "USDC", 180.0);
            if malform {
                // Nineteen bytes: a width that identifies no asset.
                book.token1_address = address("0x2260fac5e5542a773aa44fbcfedf7c193bc2c5");
            }
            tracker.on_price_level_message(message(100, vec![book]), clock.at(0));
        });

        for reason in ["malformed_address", "unadmitted_token", "one_asset_pair"] {
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
                .on_price_level_message(message(100, corrupt), clock.at(0))
                .expect("the valid side is still served");
        });

        assert_eq!(counter_value(&snapshot, REJECTED_SIDES, &[("reason", expected_reason)]), 1);
    }

    /// A token symbol comes from Titan, so it must never become a metric label: the
    /// counters carry fixed reasons and the symbol appears only in the accompanying log. The
    /// message here carries an unbound symbol, an unadmitted one and a corrupt side, so every
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
                .on_price_level_message(message(100, books), clock.at(0))
                .expect("the WBTC/USDC asks are served");
        });

        // The counters did fire, so the assertion below is about a populated snapshot. Neither
        // NVDAx nor cbBTC is in this consumer's token map, so both count as unadmitted.
        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "unadmitted_token")]), 2);
        assert_eq!(counter_value(&snapshot, REJECTED_SIDES, &[("reason", "unsupported_level")]), 1);

        let labels = all_label_values(&snapshot);
        for symbol in ["NVDAx", "cbBTC", "WBTC", "USDC"] {
            assert!(!labels.contains(&symbol), "{symbol} became a metric label: {labels:?}");
        }
    }

    /// Two legs naming one asset state no pair, so the book is skipped — and counted — rather
    /// than served as a component that would quote an asset against itself.
    #[test]
    fn a_book_whose_legs_name_one_asset_is_skipped() {
        let ((), snapshot) = record_async(async {
            let clock = Clock::new();
            let mut tracker = FreshnessTracker::new(settings(vec![fermiswap()]));
            let mut book = two_sided(PAMM, "USDC", "USDC", 1.0);
            // Titan naming one asset on both legs, under two of its symbols.
            book.token2 = "USDbC".to_string();
            let self_pair = vec![book];
            assert!(tracker
                .on_price_level_message(message(100, self_pair), clock.at(0))
                .is_none());
        });

        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "one_asset_pair")]), 1);
        // The pair failed on neither address: both resolved, to the same asset.
        assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &[("reason", "malformed_address")]), 0);
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
        updates.extend(
            tracker.on_price_level_message(message_at(100, 0, wbtc_usdc_books()), clock.at(0)),
        );
        updates.extend(tracker.on_stale_deadline(clock.at(24).monotonic));
        updates.extend(
            tracker.on_price_level_message(message_at(102, 25, wbtc_usdc_books()), clock.at(25)),
        );
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
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
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
            .on_price_level_message(message(100, captured_books_of("Kipseli")), clock.at(0))
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
            .on_price_level_message(message(100, captured_books_of("Kipseli")), clock.at(0))
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
            .on_price_level_message(message(100, corrupt_bids), clock.at(0))
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
            .on_price_level_message(message(100, wbtc_usdc_books()), clock.at(0))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, format!("fallback:{PAMM}"));
    }

    /// The `ts` of the price level message that matches the captured quote stream message, and the
    /// base of the capture clock, so the tracker accepts that message as fresh.
    const MATCHING_MESSAGE_TS: u64 = 1_792_115_920_294_943_036;

    /// The block the matching capture was taken in, whose beacon slot is [`CAPTURED_SLOT`].
    const CAPTURED_BLOCK: u64 = 26_201_954;

    // Captured venues, as Titan streams them. None of these is aliased, so each is also the venue's
    // public identity.
    const GOLIATH: &str = "0x4367985a63063fc970b009c6e599e9225181f551";
    const TAURUSFI: &str = "0x20861433d9119b866eeb398cb1ad46652a3cf953";
    /// Metric's public identity.
    const METRIC: &str = "0x99ab7c1c5eab47801f7937075998527a550fba8a";
    /// The other quote address whose state Metric's router reads, and whose entries merge.
    const METRIC_SECOND_SOURCE: &str = "0x30a88bf894d1b806125a23ebf23d715496fd7102";
    /// A venue no test registers, which only auto-detection serves.
    const UNREGISTERED_VENUE: &str = "0x00000000000000000000000000000000deadbeef";

    // The metric names the tests below read, spelled out rather than read off the constants the
    // counters are emitted under, because they are the contract with dashboards and alerts, as in
    // `telemetry`'s own tests.
    const OVERRIDE_MATCH_TOTAL: &str = "price_level_stream_override_match_total";
    const OVERRIDE_BUFFER_TOTAL: &str = "price_level_stream_override_buffer_total";
    const REFUSED_ENTRIES_TOTAL: &str = "price_level_stream_override_entries_refused_total";
    const UNREADABLE_QUOTE_SEQUENCE_TOTAL: &str =
        "price_level_stream_unreadable_quote_sequence_total";
    const SEQUENCE_SLOT_MISMATCH_TOTAL: &str = "price_level_stream_sequence_slot_mismatch_total";
    const OVERRIDE_BLOCK_MISMATCH_TOTAL: &str = "price_level_stream_override_block_mismatch_total";
    const OVERRIDE_MERGED_SOURCES_TOTAL: &str = "price_level_stream_override_merged_sources_total";
    const OVERRIDE_SKIPPED_SOURCES_TOTAL: &str =
        "price_level_stream_override_skipped_sources_total";
    const MESSAGES_WITHOUT_SLOT_TOTAL: &str = "price_level_stream_messages_without_slot_total";
    const MISSING_FIELD_TOTAL: &str = "price_level_stream_override_entry_missing_field_total";
    const UNSERVED_VENUE_ENTRIES_TOTAL: &str = "price_level_stream_unserved_venue_entries_total";
    const DUPLICATE_BOOKS_TOTAL: &str = "price_level_stream_duplicate_books_total";
    const ENTRY_ACCOUNTS: &str = "price_level_stream_override_entry_accounts";
    const ENTRY_STORAGE_SLOTS: &str = "price_level_stream_override_entry_storage_slots";

    /// A tracker serving the known venues, which are every venue of the 2026-10-16 captures,
    /// with the token metadata their books need. Registered up front rather than auto-detected,
    /// because entries are only stored for venues already served when they arrive.
    fn capture_tracker() -> FreshnessTracker {
        let known = default_served_pamms()
            .into_iter()
            .map(|config| PriceLevelStreamConfig {
                address: canonical_pamm_address(&config.address),
                ..config
            })
            .collect();
        FreshnessTracker::new(TrackerSettings { tokens: captured_tokens(), ..settings(known) })
    }

    /// The matching price level message, as the capture holds it.
    fn matching_message() -> TitanPriceLevelMessage {
        TitanPriceLevelMessage {
            block: CAPTURED_BLOCK,
            slot: Some(CAPTURED_SLOT),
            ts: MATCHING_MESSAGE_TS,
            books: matching_books(),
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

    /// The ids of `update`'s states whose simulation overrides are present (`true`) or absent
    /// (`false`), sorted.
    fn ids_with_overrides(update: &Update, present: bool) -> Vec<String> {
        let mut ids: Vec<String> = update
            .states
            .keys()
            .filter(|id| {
                state_at(update, id)
                    .simulation_overrides()
                    .is_some() ==
                    present
            })
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    /// How often `outcome` was counted, summed over every venue.
    fn outcome_total(snapshot: &SnapshotMap, outcome: &str) -> u64 {
        snapshot
            .iter()
            .filter(|((_, name, labels), _)| {
                name == OVERRIDE_MATCH_TOTAL &&
                    labels
                        .get("outcome")
                        .map(String::as_str) ==
                        Some(outcome)
            })
            .map(|((_, name, labels), _)| {
                let pairs: Vec<(&str, &str)> = labels
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                counter_value(snapshot, name, &pairs)
            })
            .sum()
    }

    /// The two captures, fed through the tracker as the stream feeds them. The expected values
    /// were worked out from the fixture JSON independently of this code: of the 30 served pairs,
    /// 16 find their venue's entry at their own quote sequence and 11 at an earlier one of the
    /// same slot, and only Kipseli's two pairs and TaurusFi's one find none.
    ///
    /// The Goliath state is checked against the hand transcription of its entry, and Metric's
    /// against the merge of its second quote address.
    #[test]
    fn the_captured_messages_attach_overrides_to_every_matching_book() {
        let clock = Clock::based_at(MATCHING_MESSAGE_TS);
        let mut tracker = capture_tracker();

        let (update, snapshot) = record_async(async {
            tracker.on_quote_stream_message(captured_quote_stream_message());
            tracker.on_price_level_message(matching_message(), clock.after(Duration::ZERO))
        });

        let update = update.expect("update expected");
        assert_eq!(update.states.len(), 30, "a captured book went unserved");
        let mut without = vec![
            pair_id(KIPSELI, WBTC, USDC),
            pair_id(KIPSELI, WETH, USDC),
            pair_id(TAURUSFI, WETH, USDC),
        ];
        without.sort();
        assert_eq!(ids_with_overrides(&update, false), without);
        assert_eq!(
            state_at(&update, &pair_id(GOLIATH, WETH, USDC)).simulation_overrides(),
            Some(&test_overrides())
        );
        let metric = state_at(&update, &pair_id(METRIC, WETH, USDC))
            .simulation_overrides()
            .expect("Metric's WETH/USDC overrides");
        assert!(
            metric
                .accounts
                .contains_key(&address(METRIC_SECOND_SOURCE)),
            "Metric's second quote address was not merged in"
        );
        for (outcome, count) in
            [("same_sequence", 16), ("earlier_sequence", 11), ("none_in_slot", 3)]
        {
            assert_eq!(outcome_total(&snapshot, outcome), count, "{outcome}");
        }
        // Metric's WBTC/USDC, WETH/USDC, WETH/USDT and cbBTC/USDC: its second quote address
        // carries no USDC/USDT pair.
        assert_eq!(
            counter_value(&snapshot, OVERRIDE_MERGED_SOURCES_TOTAL, &[("venue", "metric")]),
            4
        );
        assert_eq!(counter_value(&snapshot, OVERRIDE_BLOCK_MISMATCH_TOTAL, &[]), 0);
        assert_eq!(counter_value(&snapshot, MESSAGES_WITHOUT_SLOT_TOTAL, &[]), 0);
    }

    /// A quote sequence numbered `n` in `slot`: the slot in the high 64 bits, as Titan's are.
    const fn sequence_in(slot: u64, n: u64) -> u128 {
        ((slot as u128) << 64) | n as u128
    }

    /// A quote sequence above `u64::MAX` in the captured slot, as Titan's are.
    const SEQUENCE: u128 = sequence_in(CAPTURED_SLOT, 48_901);

    /// A synthetic venue entry, as raw JSON: `venue` priced each of `pairs` (a `(token_1,
    /// token_2, quote sequence)` each) at `built_at_ns`, overriding one account's `nonce`. The
    /// nonce is what tells two entries apart.
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

    /// A quote stream message carrying `entries`, parsed from the raw JSON they form.
    fn quote_stream_message(entries: &[String]) -> TitanQuoteStreamMessage {
        serde_json::from_str(&format!(r#"{{"venues": [{}]}}"#, entries.join(", ")))
            .expect("the quote stream message parses")
    }

    /// A one-entry quote stream message: `venue` priced `pairs` at `built_at_ns`, with nonce 1.
    fn one_entry(
        venue: &str,
        built_at_ns: u64,
        pairs: &[(&str, &str, u128)],
    ) -> TitanQuoteStreamMessage {
        quote_stream_message(&[override_entry(venue, built_at_ns, 1, pairs)])
    }

    /// The captured second, with no sub-second part: the `ts` of the synthetic cases' messages.
    const WHOLE_SECOND_NANOS: u64 = 1_792_115_920_000_000_000;

    /// A price level message of `books` with `ts` [`WHOLE_SECOND_NANOS`], in the captured block and
    /// slot.
    fn captured_slot_message(books: Vec<TitanBook>) -> TitanPriceLevelMessage {
        message_at_slot(CAPTURED_BLOCK, Some(CAPTURED_SLOT), books)
    }

    /// A price level message with `ts` [`WHOLE_SECOND_NANOS`] of `block`, naming `slot`.
    fn message_at_slot(
        block: u64,
        slot: Option<u64>,
        books: Vec<TitanBook>,
    ) -> TitanPriceLevelMessage {
        TitanPriceLevelMessage { block, slot, ts: WHOLE_SECOND_NANOS, books }
    }

    /// A two-sided book of the venue at `quote_address`, priced at `sequence`.
    fn book_at_sequence(
        quote_address: &str,
        token1: &str,
        token2: &str,
        price: f64,
        sequence: u128,
    ) -> TitanBook {
        TitanBook {
            calculated_from: QuoteSequence::Valid(sequence),
            ..two_sided(quote_address, token1, token2, price)
        }
    }

    /// The FermiSwap WBTC/USDC book at 100,000 USDC, priced at `sequence`.
    fn wbtc_usdc_at(sequence: u128) -> TitanBook {
        book_at_sequence(PAMM, "WBTC", "USDC", 100_000.0, sequence)
    }

    /// A tracker serving FermiSwap and auto-detecting whatever else a message carries, with
    /// overrides on or off.
    fn detecting_tracker(simulation_overrides: bool) -> FreshnessTracker {
        FreshnessTracker::new(TrackerSettings {
            auto_detect: true,
            simulation_overrides,
            ..settings(vec![fermiswap()])
        })
    }

    /// A tracker serving FermiSwap with overrides on or off.
    fn tracker_with_overrides(simulation_overrides: bool) -> FreshnessTracker {
        FreshnessTracker::new(TrackerSettings {
            simulation_overrides,
            ..settings(vec![fermiswap()])
        })
    }

    /// What `state` returns for one whole WBTC, in USDC's smallest unit.
    fn usdc_for_one_wbtc(state: &PriceLevelStreamState) -> BigUint {
        state
            .get_amount_out(
                BigUint::from(100_000_000u64),
                &token(WBTC, "WBTC", 8),
                &token(USDC, "USDC", 6),
            )
            .expect("the state quotes")
            .amount
    }

    /// A one-book FermiSwap message whose `calculated_from` is the raw JSON `calculated_from`,
    /// parsed as the stream parses it: the field is read leniently, so only a message built from
    /// text covers what an unreadable value does.
    fn with_calculated_from(calculated_from: &str) -> TitanPriceLevelMessage {
        let text = format!(
            r#"{{"slot": 1, "block": 100, "ts": {BASE_WALL_NANOS}, "books": [
                {{"maker": "Fermi", "quoteAddress": "{PAMM}", "token1": "WBTC", "token2": "USDC",
                  "token1Address": "{WBTC}", "token2Address": "{USDC}",
                  "calculated_from": {calculated_from},
                  "bids": [{{"price": 100000.0, "cumVolume": 1.0}}],
                  "asks": [{{"price": 100000.0, "cumVolume": 1.0}}]}}
            ]}}"#
        );
        serde_json::from_str(&text).expect("the message parses")
    }

    /// A `calculated_from` this parser cannot read: a string where Titan sends a number.
    const UNREADABLE: &str = r#""283851543383726791766048901""#;

    /// A book whose `calculated_from` this parser cannot read still quotes: the quote sequence only
    /// finds overrides, so an unreadable one costs the state its overrides and nothing else. The
    /// decode cases of every unreadable shape are in `titan.rs`.
    #[test]
    fn a_book_with_an_unreadable_quote_sequence_still_quotes() {
        let clock = Clock::new();
        let mut tracker = tracker();

        let update = tracker.on_price_level_message(with_calculated_from(UNREADABLE), clock.at(0));

        let update = update.expect("update expected");
        assert_eq!(usdc_for_one_wbtc(state_of(&update)), BigUint::from(100_000_000_000u64));
    }

    /// Both sockets name a venue by the address Titan streams it under, and both normalize it, so
    /// an aliased venue's entry reaches its books whichever of the venue's two addresses the entry
    /// names, and the overrides land on the component built from the public identity.
    ///
    /// Kipseli is registered under its public identity, as `with_known_pamms` registers it. The
    /// test fails if one socket's addresses are normalized and the other's are not, which would
    /// stop every aliased venue getting overrides while every other test passed.
    #[rstest]
    #[case::entry_under_the_streamed_address(KIPSELI_STREAMED)]
    #[case::entry_under_the_public_identity(KIPSELI)]
    fn an_aliased_venue_gets_overrides_under_its_public_identity(#[case] entry_venue: &str) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker_serving(vec![kipseli_custom()], false);
        // The book arrives under the streamed address, as Titan sends Kipseli's.
        let books = vec![book_at_sequence(KIPSELI_STREAMED, "WBTC", "USDC", 100_000.0, SEQUENCE)];

        tracker.on_quote_stream_message(one_entry(
            entry_venue,
            WHOLE_SECOND_NANOS,
            &[(WBTC, USDC, SEQUENCE)],
        ));
        let update = tracker
            .on_price_level_message(captured_slot_message(books), clock.after(Duration::ZERO));

        let update = update.expect("update expected");
        assert_eq!(ids_with_overrides(&update, true), vec![pair_id(KIPSELI, WBTC, USDC)]);
        for id in update.states.keys() {
            assert!(!id.contains(&KIPSELI_STREAMED[2..]), "{id} carries the streamed address");
        }
    }

    /// An entry reaches only the pairs it lists, of the venue it was streamed under: a third pair
    /// of the same venue and the same pair of another venue get nothing, however exactly their
    /// books name its quote sequence.
    #[test]
    fn an_entry_reaches_only_its_own_venue_and_pairs() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = detecting_tracker(true);
        let books = vec![
            wbtc_usdc_at(SEQUENCE),
            book_at_sequence(PAMM, "WETH", "USDC", 3_000.0, SEQUENCE),
            book_at_sequence(PAMM, "WBTC", "WETH", 33.0, SEQUENCE),
            book_at_sequence(UNREGISTERED_VENUE, "WBTC", "USDC", 100_000.0, SEQUENCE),
        ];

        tracker.on_quote_stream_message(one_entry(
            PAMM,
            WHOLE_SECOND_NANOS,
            &[(WBTC, USDC, SEQUENCE), (WETH, USDC, SEQUENCE)],
        ));
        let update = tracker
            .on_price_level_message(captured_slot_message(books), clock.after(Duration::ZERO));

        let update = update.expect("update expected");
        assert_eq!(update.states.len(), 4);
        let mut listed = vec![pair_id(PAMM, WBTC, USDC), pair_id(PAMM, WETH, USDC)];
        listed.sort();
        assert_eq!(ids_with_overrides(&update, true), listed);
        // One entry, shared by both pairs it listed.
        assert_eq!(
            state_at(&update, &pair_id(PAMM, WBTC, USDC)).simulation_overrides(),
            state_at(&update, &pair_id(PAMM, WETH, USDC)).simulation_overrides()
        );
    }

    /// An entry whose `pricedAddresses` name a served venue is merged into that venue's own
    /// entry, and the merge is counted under the venue; other quote addresses whose only entries
    /// are later than the book are counted as skipped.
    #[test]
    fn an_entry_merges_into_the_venues_its_priced_addresses_name() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        let second_source = "0x00000000000000000000000000000000000000aa";
        let second_account = "0x00000000000000000000000000000000000000bb";
        let pairs = [(WBTC, USDC, SEQUENCE)];
        let quote_address = format!(r#""quoteAddress": "{second_source}","#);
        let priced = format!(r#""pricedAddresses": ["{second_source}", "{PAMM}"],"#);
        let second = override_entry(second_source, WHOLE_SECOND_NANOS, 7, &pairs)
            .replace(&quote_address, &format!("{quote_address} {priced}"))
            .replace(SYNTHETIC_ACCOUNT, second_account);
        // Two more quote addresses naming the venue, whose only entries are later than the book.
        let later_only = |source: &str| {
            override_entry(source, WHOLE_SECOND_NANOS, 9, &[(WBTC, USDC, SEQUENCE + 1)]).replace(
                &format!(r#""quoteAddress": "{source}","#),
                &format!(r#""quoteAddress": "{source}", "pricedAddresses": ["{PAMM}"],"#),
            )
        };
        let entries = quote_stream_message(&[
            second,
            later_only("0x00000000000000000000000000000000000000cc"),
            later_only("0x00000000000000000000000000000000000000dd"),
            override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &pairs),
        ]);

        let (update, snapshot) = record_async(async {
            tracker.on_quote_stream_message(entries);
            tracker.on_price_level_message(
                captured_slot_message(vec![wbtc_usdc_at(SEQUENCE)]),
                clock.after(Duration::ZERO),
            )
        });

        let update = update.expect("update expected");
        let overrides = state_of(&update)
            .simulation_overrides()
            .expect("overrides");
        assert_eq!(overrides.accounts.len(), 2);
        assert!(overrides
            .accounts
            .contains_key(&address(second_account)));
        assert_eq!(overrides.accounts[&address(SYNTHETIC_ACCOUNT)].nonce, Some(1));
        assert_eq!(
            counter_value(&snapshot, OVERRIDE_MERGED_SOURCES_TOTAL, &[("venue", "fermiswap")]),
            1
        );
        assert_eq!(
            counter_value(&snapshot, OVERRIDE_SKIPPED_SOURCES_TOTAL, &[("venue", "fermiswap")]),
            2
        );
    }

    /// Entries are only stored for venues already served, so an auto-detected venue's entries
    /// that arrive before its first book are dropped, and its next entry is stored once that book
    /// has made it a served venue.
    #[test]
    fn entries_are_stored_once_their_venue_is_served() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let now = clock.after(Duration::ZERO);
        let pairs = [(WBTC, USDC, SEQUENCE)];
        let message = || {
            captured_slot_message(vec![book_at_sequence(
                UNREGISTERED_VENUE,
                "WBTC",
                "USDC",
                100_000.0,
                SEQUENCE,
            )])
        };
        let mut tracker = detecting_tracker(true);

        tracker.on_quote_stream_message(one_entry(UNREGISTERED_VENUE, WHOLE_SECOND_NANOS, &pairs));
        let first = tracker.on_price_level_message(message(), now);
        tracker.on_quote_stream_message(one_entry(UNREGISTERED_VENUE, WHOLE_SECOND_NANOS, &pairs));
        let second = tracker.on_price_level_message(message(), now);

        let first = first.expect("update expected");
        let second = second.expect("update expected");
        assert!(ids_with_overrides(&first, true).is_empty(), "stored before the venue was served");
        assert_eq!(
            ids_with_overrides(&second, true),
            vec![pair_id(UNREGISTERED_VENUE, WBTC, USDC)]
        );
    }

    /// A book whose venue has no entry for its pair at or below its quote sequence quotes as it
    /// always does, without overrides: a state without them is the normal case, not a broken one.
    #[test]
    fn a_book_with_only_a_later_entry_gets_none_and_still_quotes() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();

        tracker.on_quote_stream_message(one_entry(
            PAMM,
            WHOLE_SECOND_NANOS,
            &[(WBTC, USDC, SEQUENCE + 1)],
        ));
        let update = tracker.on_price_level_message(
            captured_slot_message(vec![wbtc_usdc_at(SEQUENCE)]),
            clock.after(Duration::ZERO),
        );

        let update = update.expect("update expected");
        let state = state_of(&update);
        assert_eq!(state.simulation_overrides(), None);
        assert_eq!(usdc_for_one_wbtc(state), BigUint::from(100_000_000_000u64));
    }

    /// When one message carries two books for one pair, the last is served with its own quote
    /// sequence: its price and the entry of its quote sequence travel together, and the case is
    /// counted. The first book names the later quote sequence, so a state that kept the first
    /// book's quote sequence would carry the other entry.
    #[test]
    fn a_duplicate_book_is_served_with_its_own_quote_sequence() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        let entries = quote_stream_message(&[
            override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &[(WBTC, USDC, SEQUENCE)]),
            override_entry(PAMM, WHOLE_SECOND_NANOS, 2, &[(WBTC, USDC, SEQUENCE + 5)]),
        ]);
        let books = vec![
            wbtc_usdc_at(SEQUENCE + 5),
            book_at_sequence(PAMM, "WBTC", "USDC", 101_000.0, SEQUENCE),
        ];

        let (update, snapshot) = record_async(async {
            tracker.on_quote_stream_message(entries);
            tracker
                .on_price_level_message(captured_slot_message(books), clock.after(Duration::ZERO))
        });

        let update = update.expect("update expected");
        let state = state_of(&update);
        assert_eq!(usdc_for_one_wbtc(state), BigUint::from(101_000_000_000u64));
        let overrides = state
            .simulation_overrides()
            .expect("overrides");
        assert_eq!(overrides.accounts[&address(SYNTHETIC_ACCOUNT)].nonce, Some(1));
        assert_eq!(counter_value(&snapshot, DUPLICATE_BOOKS_TOTAL, &[]), 1);
    }

    /// A venue entry the validation refuses whole costs that entry and nothing else: the rest of
    /// the message is stored and reaches the books it priced.
    #[test]
    fn a_refused_entry_does_not_cost_the_rest_of_its_message() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let no_timestamp = override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &[(WETH, USDC, SEQUENCE)])
            .replace(&WHOLE_SECOND_NANOS.to_string(), "null");
        let mut tracker = tracker();
        let books =
            vec![book_at_sequence(PAMM, "WETH", "USDC", 3_000.0, SEQUENCE), wbtc_usdc_at(SEQUENCE)];

        tracker.on_quote_stream_message(quote_stream_message(&[
            no_timestamp,
            override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &[(WBTC, USDC, SEQUENCE)]),
        ]));
        let update = tracker
            .on_price_level_message(captured_slot_message(books), clock.after(Duration::ZERO));

        let update = update.expect("update expected");
        assert_eq!(ids_with_overrides(&update, true), vec![pair_id(PAMM, WBTC, USDC)]);
    }

    /// With overrides off nothing is stored, so no state carries any, and a quote stream message,
    /// which no socket delivers then, changes nothing if one arrives anyway.
    #[test]
    fn overrides_off_leaves_every_state_without_overrides() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker_with_overrides(false);

        tracker.on_quote_stream_message(one_entry(
            PAMM,
            WHOLE_SECOND_NANOS,
            &[(WBTC, USDC, SEQUENCE)],
        ));
        let update = tracker.on_price_level_message(
            captured_slot_message(vec![wbtc_usdc_at(SEQUENCE)]),
            clock.after(Duration::ZERO),
        );

        assert_eq!(state_of(&update.expect("update expected")).simulation_overrides(), None);
    }

    /// Every served pair is counted under the outcome it reached, per venue, so the sum over the
    /// outcomes equals the pairs served. Each case drives one tracker into exactly one outcome,
    /// and the other three stay at zero.
    #[rstest]
    #[case::same_sequence(Some(SEQUENCE), wbtc_usdc_at(SEQUENCE), "same_sequence")]
    #[case::earlier_sequence(Some(SEQUENCE), wbtc_usdc_at(SEQUENCE + 3), "earlier_sequence")]
    #[case::none_in_slot(None, wbtc_usdc_at(SEQUENCE), "none_in_slot")]
    #[case::no_quote_sequence(
        Some(SEQUENCE),
        two_sided(PAMM, "WBTC", "USDC", 100_000.0),
        "no_quote_sequence"
    )]
    fn each_match_outcome_is_counted_under_its_venue(
        #[case] entry_sequence: Option<u128>,
        #[case] book: TitanBook,
        #[case] outcome: &str,
    ) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();

        let ((), snapshot) = record_async(async {
            if let Some(sequence) = entry_sequence {
                tracker.on_quote_stream_message(one_entry(
                    PAMM,
                    WHOLE_SECOND_NANOS,
                    &[(WBTC, USDC, sequence)],
                ));
            }
            tracker.on_price_level_message(
                captured_slot_message(vec![book]),
                clock.after(Duration::ZERO),
            );
        });

        for counted in ["same_sequence", "earlier_sequence", "none_in_slot", "no_quote_sequence"] {
            assert_eq!(
                counter_value(
                    &snapshot,
                    OVERRIDE_MATCH_TOTAL,
                    &[("venue", "fermiswap"), ("outcome", counted)]
                ),
                u64::from(counted == outcome),
                "{counted}"
            );
        }
    }

    /// The buffer keeps the slot of the latest price level message and one slot either side: a
    /// message two slots on drops the older entries, and an entry arriving for a slot already
    /// left behind is not stored.
    #[test]
    fn the_slot_window_follows_the_price_level_messages() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        let two_slots_on = message_at_slot(
            CAPTURED_BLOCK + 2,
            Some(CAPTURED_SLOT + 2),
            vec![wbtc_usdc_at(sequence_in(CAPTURED_SLOT + 2, 1))],
        );
        let entry = || one_entry(PAMM, WHOLE_SECOND_NANOS, &[(WBTC, USDC, SEQUENCE)]);

        let (update, snapshot) = record_async(async {
            tracker.on_quote_stream_message(entry());
            let update = tracker.on_price_level_message(two_slots_on, clock.after(Duration::ZERO));
            tracker.on_quote_stream_message(entry());
            update
        });

        assert_eq!(state_of(&update.expect("update expected")).simulation_overrides(), None);
        for (event, count) in
            [("added", 1), ("left_slot_window", 1), ("arrived_outside_slot_window", 1)]
        {
            assert_eq!(
                counter_value(&snapshot, OVERRIDE_BUFFER_TOTAL, &[("event", event)]),
                count,
                "{event}"
            );
        }
    }

    /// A price level message naming no slot is counted, moves the window to the newest slot its
    /// books' quote sequences carry, and leaves it where it was when no book carries one. Its
    /// books' quote sequences are not checked against a slot it does not name.
    #[rstest]
    #[case::a_book_names_a_later_slot(wbtc_usdc_at(sequence_in(CAPTURED_SLOT + 2, 1)), 1)]
    #[case::no_book_names_a_quote_sequence(two_sided(PAMM, "WBTC", "USDC", 100_000.0), 0)]
    fn a_message_without_a_slot_moves_the_window_by_its_books(
        #[case] book: TitanBook,
        #[case] left_slot_window: u64,
    ) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();

        let ((), snapshot) = record_async(async {
            tracker.on_quote_stream_message(one_entry(
                PAMM,
                WHOLE_SECOND_NANOS,
                &[(WBTC, USDC, SEQUENCE)],
            ));
            tracker.on_price_level_message(
                message_at_slot(CAPTURED_BLOCK + 2, None, vec![book]),
                clock.after(Duration::ZERO),
            );
        });

        assert_eq!(
            counter_value(&snapshot, OVERRIDE_BUFFER_TOTAL, &[("event", "left_slot_window")]),
            left_slot_window
        );
        assert_eq!(counter_value(&snapshot, MESSAGES_WITHOUT_SLOT_TOTAL, &[]), 1);
        assert_eq!(
            counter_value(&snapshot, SEQUENCE_SLOT_MISMATCH_TOTAL, &[("socket", "price_levels")]),
            0
        );
    }

    /// The two assumptions matching rests on are counted when they break: a book's quote
    /// sequence carrying a slot other than its message's, and attached overrides naming a block
    /// other than the message's. Neither removes the overrides.
    #[test]
    fn broken_assumptions_are_counted() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();
        // The message names the next slot, one block on, while its book names the captured slot,
        // which the window still holds.
        let next_slot = message_at_slot(
            CAPTURED_BLOCK + 1,
            Some(CAPTURED_SLOT + 1),
            vec![wbtc_usdc_at(SEQUENCE)],
        );

        let (update, snapshot) = record_async(async {
            tracker.on_quote_stream_message(one_entry(
                PAMM,
                WHOLE_SECOND_NANOS,
                &[(WBTC, USDC, SEQUENCE)],
            ));
            tracker.on_price_level_message(next_slot, clock.after(Duration::ZERO))
        });

        assert!(state_of(&update.expect("update expected"))
            .simulation_overrides()
            .is_some());
        assert_eq!(
            counter_value(&snapshot, SEQUENCE_SLOT_MISMATCH_TOTAL, &[("socket", "price_levels")]),
            1
        );
        assert_eq!(counter_value(&snapshot, OVERRIDE_BLOCK_MISMATCH_TOTAL, &[]), 1);
    }

    /// The tracker counts what an accepted venue entry looks like: the fields it omits, a pair
    /// whose quote sequence carries another slot than the entry, a quote address this consumer
    /// does not serve, and every entry's size.
    #[test]
    fn the_shape_of_each_accepted_entry_is_counted() {
        let pairs = [(WBTC, USDC, SEQUENCE)];
        let entry = || override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &pairs);
        let entries = quote_stream_message(&[
            entry().replace(&format!(r#""blockNumber": {CAPTURED_BLOCK},"#), ""),
            entry().replace(&format!(r#""slot": {CAPTURED_SLOT},"#), ""),
            override_entry(
                PAMM,
                WHOLE_SECOND_NANOS,
                1,
                &[(WBTC, USDC, sequence_in(CAPTURED_SLOT + 5, 1))],
            ),
            override_entry(UNREGISTERED_VENUE, WHOLE_SECOND_NANOS, 1, &pairs),
        ]);
        let mut tracker = tracker();

        let ((), snapshot) = record_async(async { tracker.on_quote_stream_message(entries) });

        for (name, labels) in [
            (MISSING_FIELD_TOTAL, vec![("field", "block_number")]),
            (MISSING_FIELD_TOTAL, vec![("field", "slot")]),
            (SEQUENCE_SLOT_MISMATCH_TOTAL, vec![("socket", "quote_stream")]),
            (UNSERVED_VENUE_ENTRIES_TOTAL, vec![]),
        ] {
            assert_eq!(counter_value(&snapshot, name, &labels), 1, "{name} {labels:?}");
        }
        assert_eq!(histogram_values(&snapshot, ENTRY_ACCOUNTS, &[]), vec![1.0; 4]);
        assert_eq!(histogram_values(&snapshot, ENTRY_STORAGE_SLOTS, &[]), vec![0.0; 4]);
    }

    /// An unreadable `calculated_from` is counted apart from an absent one. Both leave the state
    /// without overrides, but only the first says Titan sent something this parser does not
    /// understand.
    ///
    /// The third case is why the series is registered whether or not overrides are on: the value
    /// is read off a served book either way, so the counter must still move.
    #[rstest]
    #[case::unreadable(UNREADABLE, true, 1)]
    #[case::absent("null", true, 0)]
    #[case::unreadable_without_overrides(UNREADABLE, false, 1)]
    fn an_unreadable_quote_sequence_is_counted_apart_from_an_absent_one(
        #[case] calculated_from: &str,
        #[case] overrides_on: bool,
        #[case] counted: u64,
    ) {
        let clock = Clock::new();

        // Built inside the recorder, because the tracker registers the series when it is built.
        let (update, snapshot) = record_async(async {
            tracker_with_overrides(overrides_on)
                .on_price_level_message(with_calculated_from(calculated_from), clock.at(0))
        });

        assert!(update.is_some());
        assert!(
            counter_registered(&snapshot, UNREADABLE_QUOTE_SEQUENCE_TOTAL, &[]),
            "unreadable_quote_sequence_total is a missing series"
        );
        assert_eq!(counter_value(&snapshot, UNREADABLE_QUOTE_SEQUENCE_TOTAL, &[]), counted);
    }

    /// What storing entries did is counted per pair, and an entry the validation refused whole is
    /// counted on its own counter: one Titan sent wrong is a different failure, with a different
    /// fix, from one the buffer's rules turned away. Each entry carries two pairs, so each event
    /// counts twice.
    #[test]
    fn buffer_events_are_counted() {
        let pairs = [(WBTC, USDC, SEQUENCE), (WETH, USDC, SEQUENCE)];
        let no_timestamp = override_entry(PAMM, WHOLE_SECOND_NANOS, 1, &pairs)
            .replace(&WHOLE_SECOND_NANOS.to_string(), "null");
        let messages = [
            one_entry(PAMM, WHOLE_SECOND_NANOS, &pairs),
            one_entry(PAMM, WHOLE_SECOND_NANOS, &pairs),
            one_entry(PAMM, WHOLE_SECOND_NANOS + 1, &pairs),
            quote_stream_message(&[override_entry(PAMM, WHOLE_SECOND_NANOS + 1, 2, &pairs)]),
            quote_stream_message(&[no_timestamp]),
        ];
        let mut tracker = tracker();

        let ((), snapshot) = record_async(async {
            for message in messages {
                tracker.on_quote_stream_message(message);
            }
        });

        for event in
            ["added", "resent_unchanged", "replaced_by_newer", "resent_with_different_state"]
        {
            assert_eq!(
                counter_value(&snapshot, OVERRIDE_BUFFER_TOTAL, &[("event", event)]),
                2,
                "{event}"
            );
        }
        assert_eq!(
            counter_value(&snapshot, REFUSED_ENTRIES_TOTAL, &[("reason", "no_timestamp")]),
            1
        );
    }

    /// A new tracker registers exactly the series this consumer can fill: the overrides series
    /// only while overrides are on, because no socket fills the buffer otherwise, and the
    /// unreadable quote sequence counter either way, because a book's `calculated_from` is read
    /// whether or not overrides are on. The tracker forwards its flag to the socket series, whose
    /// label sets [`telemetry`](super::super::telemetry)'s own tests pin.
    #[rstest]
    #[case::overrides_on(true)]
    #[case::overrides_off(false)]
    fn series_a_new_tracker_registers(#[case] overrides_on: bool) {
        let ((), snapshot) = record_async(async {
            drop(tracker_with_overrides(overrides_on));
        });

        for (name, labels) in [
            (OVERRIDE_MATCH_TOTAL, vec![("venue", "fermiswap"), ("outcome", "same_sequence")]),
            (OVERRIDE_MERGED_SOURCES_TOTAL, vec![("venue", "fermiswap")]),
            (OVERRIDE_SKIPPED_SOURCES_TOTAL, vec![("venue", "fermiswap")]),
            (OVERRIDE_BUFFER_TOTAL, vec![("event", "added")]),
            (REFUSED_ENTRIES_TOTAL, vec![("reason", "no_timestamp")]),
            (OVERRIDE_BLOCK_MISMATCH_TOTAL, vec![]),
            (RECONNECTS, vec![("socket", "quote_stream"), ("reason", "closed")]),
        ] {
            assert_eq!(
                counter_registered(&snapshot, name, &labels),
                overrides_on,
                "{name} registration with overrides {overrides_on}"
            );
            assert_eq!(counter_value(&snapshot, name, &labels), 0, "{name}");
        }
        assert!(counter_registered(&snapshot, UNREADABLE_QUOTE_SEQUENCE_TOTAL, &[]));
        assert!(counter_registered(&snapshot, MESSAGES_WITHOUT_SLOT_TOTAL, &[]));
    }

    /// An auto-detected venue gets its per-venue overrides series registered at zero when it is
    /// first served, like the other per-venue series, and only while overrides are on.
    #[rstest]
    #[case::overrides_on(true)]
    #[case::overrides_off(false)]
    fn an_auto_detected_venue_registers_its_match_outcomes(#[case] overrides_on: bool) {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = detecting_tracker(overrides_on);
        let books = vec![two_sided(UNREGISTERED_VENUE, "WBTC", "USDC", 100_000.0)];

        let ((), snapshot) = record_async(async {
            tracker
                .on_price_level_message(captured_slot_message(books), clock.after(Duration::ZERO));
        });

        for outcome in ["same_sequence", "earlier_sequence", "none_in_slot", "no_quote_sequence"] {
            assert_eq!(
                counter_registered(
                    &snapshot,
                    OVERRIDE_MATCH_TOTAL,
                    &[("venue", UNREGISTERED_VENUE), ("outcome", outcome)]
                ),
                overrides_on,
                "{outcome}"
            );
        }
        assert_eq!(
            counter_value(
                &snapshot,
                OVERRIDE_MATCH_TOTAL,
                &[("venue", UNREGISTERED_VENUE), ("outcome", "no_quote_sequence")]
            ),
            u64::from(overrides_on)
        );
    }

    /// Matching knows a component (a venue address plus both token addresses) and a quote
    /// sequence above `u64::MAX`, and neither may become a label: one series per pair Titan
    /// mentions is exactly the unbounded growth the no-address-as-label rule exists to prevent.
    /// Both stay in the DEBUG log that accompanies the counter.
    ///
    /// The venue is registered, so its `venue` label is the configured name; an auto-detected
    /// venue is named by its address by design, which is the one exception and not this case.
    #[test]
    fn no_streamed_value_becomes_a_metric_label() {
        let clock = Clock::based_at(WHOLE_SECOND_NANOS);
        let mut tracker = tracker();

        let (update, snapshot) = record_async(async {
            tracker.on_quote_stream_message(one_entry(
                PAMM,
                WHOLE_SECOND_NANOS,
                &[(WBTC, USDC, SEQUENCE)],
            ));
            tracker.on_price_level_message(
                captured_slot_message(vec![wbtc_usdc_at(SEQUENCE)]),
                clock.after(Duration::ZERO),
            )
        });

        assert!(update.is_some());
        // The match did happen, so the assertion below is about a populated snapshot.
        assert_eq!(
            counter_value(
                &snapshot,
                OVERRIDE_MATCH_TOTAL,
                &[("venue", "fermiswap"), ("outcome", "same_sequence")]
            ),
            1
        );
        let labels = all_label_values(&snapshot);
        for streamed_value in [SEQUENCE.to_string(), pair_id(PAMM, WBTC, USDC), PAMM.to_string()] {
            assert!(
                !labels.contains(&streamed_value.as_str()),
                "{streamed_value} became a metric label: {labels:?}"
            );
        }
    }
}
