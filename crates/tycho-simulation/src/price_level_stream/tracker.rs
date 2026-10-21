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
    config::{canonical_pamm_address, PriceLevelStreamConfig},
    convert::{ladders_for_book, resolve_book_tokens, Ladder, SideRejection, UnresolvedToken},
    state::{PriceLevelStreamQuote, PriceLevelStreamState, QUOTE_TTL},
    stream::PAMM_ADDRESS_ATTRIBUTE,
    telemetry::{self, RejectReason, SideRejectReason, SkipReason},
    titan::{TitanBook, TitanPriceLevelMessage},
    SLOT,
};
use crate::protocol::models::{ProtocolComponent, Update};

/// How long a component stays served after the last accepted frame that carried it: two slots.
/// Titan streams at 1 Hz and its frames arrive within about 3 s of being built, so 24 s absorbs
/// jitter and still removes a silent component within two blocks.
pub(super) const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(2 * SLOT.as_secs());

pub(super) const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Furthest a frame's `timestamp` may lie ahead of the local wall clock: one slot. Titan's
/// frames were observed at least 120 ms behind the local clock; anything ahead of it by more
/// than clock skew is implausible.
const MAX_FUTURE_SKEW_NANOS: u64 = SLOT.as_secs() * NANOS_PER_SECOND;

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
}

/// Turns Titan frames into [`Update`]s. A frame only adds or refreshes components: a component (or
/// a whole venue) that a frame omits stays served until its own deadline passes in
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
    /// stream order is the one served.
    pub(super) fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update> {
        let frame_age = match self.check_frame(&frame, now) {
            Ok(frame_age) => frame_age,
            Err(reason) => {
                self.log_rejection(reason, &frame);
                telemetry::record_frame_rejected(reason);
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

        let stale_at = now.monotonic +
            self.stale_after
                .saturating_sub(frame_age);
        // `QUOTE_TTL` counts from the frame's `timestamp` and is enforced on the monotonic
        // clock; it does not depend on whether the ladder's content changed, because quiet
        // venues repeat a ladder for minutes.
        let quotable_until = self
            .quote_guard
            .then(|| now.monotonic + QUOTE_TTL.saturating_sub(frame_age));
        let frame_unix_seconds = frame.ts / NANOS_PER_SECOND;
        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();

        for (pamm, books) in group_by_venue(frame.books) {
            self.admit(&pamm);
            let Some(config) = self.registry.get(&pamm) else {
                continue;
            };
            telemetry::record_last_seen(&config.protocol, frame_unix_seconds);

            for book in books {
                let Some(PairLadders { token0, token1, quotes_0_to_1, quotes_1_to_0 }) =
                    ladders_for_pair(&book, &self.tokens)
                else {
                    continue;
                };
                let id = component_id(&config.address, &token0, &token1);
                let id_string = id.to_string();
                match self.served.get_mut(&id_string) {
                    Some(served) => served.stale_at = stale_at,
                    None => {
                        let component = build_component(
                            &self.tokens,
                            config,
                            id,
                            &token0,
                            &token1,
                            self.via_fallback_router,
                        );
                        new_pairs.insert(id_string.clone(), component.clone());
                        self.served.insert(
                            id_string.clone(),
                            ServedComponent {
                                component,
                                venue_name: config.protocol.clone(),
                                stale_at,
                            },
                        );
                    }
                }
                let mut state = PriceLevelStreamState::new(
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    config.gas_cost.clone(),
                );
                if let Some(until) = quotable_until {
                    state = state.with_quotable_until(until);
                }
                states.insert(id_string, Box::new(state));
            }
        }

        self.settle(Frontier { block: frame.block, accepted_at: now.monotonic });
        if states.is_empty() {
            return None;
        }
        Some(Update::new(frame.block, states, new_pairs).set_is_partial(true))
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

/// One pair's quote ladders, in the token order [`PriceLevelStreamState`] and [`component_id`]
/// use: `token0` is the lower address.
struct PairLadders {
    token0: Bytes,
    token1: Bytes,
    quotes_0_to_1: Vec<PriceLevelStreamQuote>,
    quotes_1_to_0: Vec<PriceLevelStreamQuote>,
}

/// The quote ladders of `book`, or `None` if it prices no pair this consumer can serve: an address
/// that is not 20 bytes (`malformed_address`), one absent from `tokens` (`unadmitted_token`), or
/// two addresses that name one asset (`one_asset_pair`).
///
/// A side the conversion rejects yields an empty ladder, so that one direction is unquotable
/// while the opposite direction keeps being served — as for a side the venue quoted empty.
fn ladders_for_pair(book: &TitanBook, tokens: &HashMap<Bytes, Token>) -> Option<PairLadders> {
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
            state::QUOTE_TTL,
            telemetry::{
                recorded::{
                    all_label_values, counter_registered, counter_value, gauge_value,
                    histogram_values, record_async,
                },
                FRAMES_ACCEPTED, FRAMES_REJECTED, FRAME_AGE, LAST_SEEN, REJECTED_SIDES,
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
    /// the start. Tests must pass non-decreasing seconds, as a monotonic clock would give.
    struct Clock {
        start: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self { start: Instant::now() }
        }

        fn at(&self, seconds: u64) -> Now {
            Now {
                wall_nanos: BASE_WALL_NANOS + seconds * NANOS_PER_SECOND,
                monotonic: self.start + Duration::from_secs(seconds),
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
        let Update { block_number, is_partial, sync_states, states, new_pairs, removed_pairs } =
            tracker
                .on_frame(message(100, wbtc_usdc_books()), clock.at(0))
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
                counter_value(&snapshot, FRAMES_REJECTED, &[("reason", reason)]),
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
        assert_eq!(update.block_number, 100);
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
        assert_eq!(removal.block_number, u64::MAX / 2);
        // The next plausible frame is accepted as a first frame again, and nothing from the
        // implausible frame survives: the pair comes back as new.
        let update = tracker
            .on_frame(message_at(100, 25, wbtc_usdc_books()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.block_number, 100);
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
        assert_eq!(update.block_number, 100);
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

    /// A book whose `token1` address sorts above its `token2` address orders the pair the other
    /// way round, so the bids become the `token1 → token0` ladder and the asks the
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

    /// A pair is served only if the consumer admits both of its assets. Neither `NVDAx` nor
    /// `cbBTC` is in this consumer's token map, so either book is skipped and the frame serves
    /// nothing.
    #[rstest]
    #[case::unadmitted_token("NVDAx")]
    #[case::another_unadmitted_token("cbBTC")]
    fn books_naming_an_unadmitted_asset_are_skipped(#[case] token1: &str) {
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
            tracker.on_frame(message(100, vec![book]), clock.at(0));
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
                .on_frame(message(100, self_pair), clock.at(0))
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
}
