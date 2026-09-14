//! Turns Titan frames into [`Update`]s.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::Utc;
use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use super::{
    config::PriceLevelStreamConfig,
    fallback_router::RouterVenuesRead,
    state::{PriceLevelStreamQuote, PriceLevelStreamState, QUOTE_TTL},
    stream::PAMM_ADDRESS_ATTRIBUTE,
    telemetry,
    titan::{TitanPairLevels, TitanPammLevels, TitanPriceLevel, TitanPriceLevelMessage},
};
use crate::protocol::models::{ProtocolComponent, Update};

/// How long the data of one frame may be served without a fresher frame carrying it: two block
/// times. Titan streams at 1 Hz and its frames arrive within about 3 s of being built, so this
/// leaves ample margin for jitter while bounding stale exposure.
pub(super) const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(24);

pub(super) const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Furthest a frame's `timestamp` may lie ahead of the local wall clock: one slot. Titan's
/// frames were observed at least 120 ms behind the local clock; anything ahead of it by more
/// than clock skew is implausible.
const MAX_FUTURE_SKEW_NANOS: u64 = 12 * NANOS_PER_SECOND;

/// Post-merge Ethereum slot duration in seconds.
const SECONDS_PER_SLOT: u64 = 12;

/// Extra blocks allowed beyond what elapsed time explains. Titan builds at chain head + 1 and
/// sometimes + 2, so a frame right after a block boundary may jump by two.
const BLOCK_JUMP_SLACK: u64 = 2;

/// How many distinct unregistered venue addresses get a first-sight INFO line per process.
/// Addresses come from an external source; the set is bounded so a misbehaving upstream cannot
/// grow memory, and they never become metric labels.
const MAX_UNREGISTERED_LOGGED: usize = 64;

/// The instants a tracker event is judged against.
#[derive(Clone, Copy, Debug)]
pub(super) struct Now {
    /// Wall clock, nanoseconds since the Unix epoch. Only ever compared against Titan's
    /// `timestamp`.
    pub wall_nanos: u64,
    /// Monotonic clock; drives every deadline, so a wall-clock jump cannot expire or freeze
    /// anything.
    pub instant: Instant,
}

impl Now {
    pub(super) fn current() -> Self {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|since_epoch| u64::try_from(since_epoch.as_nanos()).ok());
        let wall_nanos = match since_epoch {
            Some(wall_nanos) => wall_nanos,
            None => {
                tracing::error!(
                    "System clock unrepresentable as unix nanoseconds; every price level frame \
                     will be rejected as in_future"
                );
                0
            }
        };
        Self { wall_nanos, instant: Instant::now() }
    }
}

/// Why a parsed frame was dropped before touching the served set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    /// Built `stale_after` ago or earlier.
    TooOld,
    /// Built more than one slot ahead of the local clock.
    InFuture,
    /// Older than the newest accepted frame.
    OutOfOrder,
    /// Targets a block below the newest accepted one.
    BlockRegression,
    /// Targets a block further ahead than elapsed time allows.
    BlockJump,
}

impl Rejection {
    fn as_str(self) -> &'static str {
        match self {
            Rejection::TooOld => "too_old",
            Rejection::InFuture => "in_future",
            Rejection::OutOfOrder => "out_of_order",
            Rejection::BlockRegression => "block_regression",
            Rejection::BlockJump => "block_jump",
        }
    }
}

/// A component the stream currently vouches for.
struct Served {
    component: ProtocolComponent,
    /// The venue name, for logs and metric labels.
    venue: String,
    /// The venue address, for whitelist membership checks.
    address: Bytes,
    /// The instant this component's data turns `stale_after` old.
    deadline: Instant,
}

/// What the stream can currently vouch for.
enum Source {
    /// The PropAMMRouter whitelist has not been read yet: the family of every component is
    /// unknown, so frames are validated but nothing is emitted.
    AwaitingWhitelist,
    /// Whitelist known (or not needed) and nothing served: at start, and whenever the last
    /// served component expired.
    Unserved,
    /// At least one component is served; `deadline` is the earliest component deadline.
    Serving { deadline: Instant },
}

impl Source {
    fn telemetry_state(&self) -> telemetry::SourceState {
        match self {
            Source::AwaitingWhitelist => telemetry::SourceState::AwaitingWhitelist,
            Source::Unserved => telemetry::SourceState::Unserved,
            Source::Serving { deadline: _ } => telemetry::SourceState::Serving,
        }
    }
}

/// Turns Titan frames into [`Update`]s. Frames are additive only: a component (or a whole venue)
/// a frame omits is presumed still fresh until its own deadline lapses in
/// [`on_stale_deadline`](Self::on_stale_deadline). There is no diff-based removal — omission from
/// a single frame never removes anything by itself.
pub(super) struct SnapshotTracker {
    registry: HashMap<Bytes, PriceLevelStreamConfig>,
    /// Venues excluded from auto-detection. The builder keeps this disjoint from the registry:
    /// denying removes any registration and registering removes any denial.
    denied: HashSet<Bytes>,
    tokens: HashMap<Bytes, Token>,
    /// Whether frames from pAMMs absent from the registry get an address-named configuration
    /// synthesized (and cached in the registry) instead of being skipped.
    auto_detect: bool,
    /// The per-swap gas cost synthesized auto-detected configurations are served with.
    auto_detected_gas_cost: BigUint,
    /// Venues whose components are emitted under the `propammfallback:` family, so their swaps
    /// execute through Titan's PropAMMRouter instead of the venue directly.
    router_venues: HashSet<Bytes>,
    /// The components currently served, keyed by component id.
    served: HashMap<String, Served>,
    /// What the stream can currently vouch for; drives [`Self::stale_deadline`] and the
    /// per-venue gauges.
    source: Source,
    /// The data freshness window; see [`DEFAULT_STALE_AFTER`].
    stale_after: Duration,
    /// The `timestamp` of the newest accepted frame. Frames older than it are out of order;
    /// equal ones are re-emissions within a build round and accepted. Never reset.
    newest_timestamp: u64,
    /// The block of the newest accepted frame. Later frames may not regress below it, nor jump
    /// further ahead than the elapsed time explains. Reset with `last_accepted`.
    newest_block: u64,
    /// When the newest frame was accepted; `None` whenever nothing is served, which skips the
    /// block checks for the next frame. The frontier is established only while the stream
    /// serves something: a frame that serves nothing — because the whitelist is still awaited,
    /// or because it carries nothing this tracker can build — resets it immediately.
    last_accepted: Option<Instant>,
    /// Whether the last frame was rejected: the first rejection of a streak logs at WARN, the
    /// rest at DEBUG, and the counter carries the rate.
    rejecting: bool,
    /// Unregistered venue addresses already logged, at most [`MAX_UNREGISTERED_LOGGED`].
    seen_unregistered: HashSet<Bytes>,
}

impl SnapshotTracker {
    pub(super) fn new(
        registry: HashMap<Bytes, PriceLevelStreamConfig>,
        denied: HashSet<Bytes>,
        tokens: HashMap<Bytes, Token>,
        auto_detect: bool,
        auto_detected_gas_cost: BigUint,
        stale_after: Duration,
        fallback_router: bool,
    ) -> Self {
        let source = if fallback_router { Source::AwaitingWhitelist } else { Source::Unserved };
        telemetry::source_state(source.telemetry_state());
        // Pre-initialise every per-venue series so a venue that never appears is a visible
        // zero, not a missing series.
        for config in registry.values() {
            telemetry::served_components(&config.protocol, 0);
            telemetry::last_seen(&config.protocol, 0);
        }
        Self {
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            router_venues: HashSet::new(),
            served: HashMap::new(),
            source,
            stale_after,
            newest_timestamp: 0,
            newest_block: 0,
            last_accepted: None,
            rejecting: false,
            seen_unregistered: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn set_router_venues_for_test(&mut self, venues: HashSet<Bytes>) {
        self.router_venues = venues;
    }

    /// Applies a whitelist read. A failed read keeps the last known set (the reader already
    /// logged it). A successful read that changes a served venue's family removes that venue's
    /// components now; the next accepted frame carrying them re-adds them under the new family.
    pub(super) fn on_router_venues(&mut self, read: RouterVenuesRead) -> Option<Update> {
        let venues = match read {
            RouterVenuesRead::Failed(error) => {
                tracing::debug!(error = %error, "Whitelist read failed; whitelist unchanged");
                return None;
            }
            RouterVenuesRead::Ok(venues) => venues,
        };
        telemetry::whitelisted_venues(venues.len());
        if let Source::AwaitingWhitelist = self.source {
            self.router_venues = venues;
            tracing::info!(
                venues = self.router_venues.len(),
                "PropAMMRouter venue whitelist read; serving pAMMs from the next frame"
            );
            self.source = Source::Unserved;
            telemetry::source_state(self.source.telemetry_state());
            return None;
        }
        let moved: Vec<String> = self
            .served
            .iter()
            .filter(|(_, served)| {
                self.router_venues
                    .contains(&served.address) !=
                    venues.contains(&served.address)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.router_venues = venues;
        if moved.is_empty() {
            return None;
        }
        let mut removed = HashMap::with_capacity(moved.len());
        for id in moved {
            let Some(served) = self.served.remove(&id) else {
                continue;
            };
            tracing::info!(
                venue = %served.venue,
                "pAMM changed PropAMMRouter whitelist membership; re-adding it under its new \
                 family on the next frame"
            );
            removed.insert(id, served.component);
        }
        // Build the update off the still-current frontier before `refresh_source` may reset it
        // (it does, once nothing is left served) — mirrors `on_stale_deadline`.
        let update = self.removal_update(removed);
        self.refresh_source();
        Some(update)
    }

    /// Checks a frame against the freshness and ordering rules, returning its age when it is
    /// acceptable.
    fn accept(&self, frame: &TitanPriceLevelMessage, now: Now) -> Result<Duration, Rejection> {
        let frame_age = Duration::from_nanos(
            now.wall_nanos
                .saturating_sub(frame.timestamp),
        );
        if frame_age >= self.stale_after {
            return Err(Rejection::TooOld);
        }
        if frame.timestamp >
            now.wall_nanos
                .saturating_add(MAX_FUTURE_SKEW_NANOS)
        {
            return Err(Rejection::InFuture);
        }
        if frame.timestamp < self.newest_timestamp {
            return Err(Rejection::OutOfOrder);
        }
        let Some(last_accepted) = self.last_accepted else {
            return Ok(frame_age);
        };
        if frame.block_number < self.newest_block {
            return Err(Rejection::BlockRegression);
        }
        let elapsed_slots = now
            .instant
            .saturating_duration_since(last_accepted)
            .as_secs() /
            SECONDS_PER_SLOT;
        let allowed = self
            .newest_block
            .saturating_add(elapsed_slots)
            .saturating_add(BLOCK_JUMP_SLACK);
        if frame.block_number > allowed {
            return Err(Rejection::BlockJump);
        }
        Ok(frame_age)
    }

    /// Processes one frame into an [`Update`], or `None` if the frame is rejected (see
    /// [`Rejection`]), the whitelist has not been read yet, or the frame carries nothing
    /// relevant.
    pub(super) fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update> {
        let frame_age = match self.accept(&frame, now) {
            Ok(frame_age) => frame_age,
            Err(rejection) => {
                self.log_rejection(rejection, &frame);
                telemetry::frame_rejected(rejection.as_str());
                return None;
            }
        };
        self.rejecting = false;
        telemetry::frame_accepted();
        self.newest_timestamp = frame.timestamp;
        self.newest_block = frame.block_number;
        self.last_accepted = Some(now.instant);

        if let Source::AwaitingWhitelist = self.source {
            self.refresh_source();
            return None;
        }

        let deadline = now.instant +
            self.stale_after
                .saturating_sub(frame_age);
        let quotable_until = now.instant + QUOTE_TTL.saturating_sub(frame_age);
        let frame_unix_seconds = frame.timestamp / NANOS_PER_SECOND;
        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();

        for TitanPammLevels { pamm, pairs } in frame.pamms {
            let Some(config) = self.resolve_config(&pamm) else {
                continue;
            };
            telemetry::last_seen(&config.protocol, frame_unix_seconds);

            for ((token0, token1), (quotes_0_to_1, quotes_1_to_0)) in self.merge_pairs(pairs) {
                let id = component_id(&config.address, &token0, &token1);
                let id_string = id.to_string();
                let component = match self.served.get(&id_string) {
                    Some(served) => served.component.clone(),
                    None => {
                        let via_router = self
                            .router_venues
                            .contains(&config.address);
                        let component = build_component(
                            &self.tokens,
                            &config,
                            id,
                            &token0,
                            &token1,
                            via_router,
                        );
                        new_pairs.insert(id_string.clone(), component.clone());
                        component
                    }
                };
                let state = PriceLevelStreamState::new(
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    config.gas_cost.clone(),
                )
                .with_quotable_until(quotable_until);
                states.insert(id_string.clone(), Box::new(state));
                self.served.insert(
                    id_string,
                    Served {
                        component,
                        venue: config.protocol.clone(),
                        address: config.address.clone(),
                        deadline,
                    },
                );
            }
        }

        self.refresh_source();
        if states.is_empty() {
            return None;
        }
        Some(Update::new(frame.block_number, states, new_pairs).set_is_partial(true))
    }

    /// The configuration a streamed venue is served under, or `None` when it is skipped.
    fn resolve_config(&mut self, pamm: &Bytes) -> Option<PriceLevelStreamConfig> {
        if let Some(config) = self.registry.get(pamm) {
            return Some(config.clone());
        }
        if self.denied.contains(pamm) {
            tracing::debug!(%pamm, "Skipping denied pAMM");
            return None;
        }
        if !self.auto_detect {
            telemetry::unregistered_pamm();
            if self.seen_unregistered.len() < MAX_UNREGISTERED_LOGGED &&
                self.seen_unregistered
                    .insert(pamm.clone())
            {
                tracing::info!(
                    %pamm,
                    "Skipping unregistered pAMM; register it via add_pamm to serve it"
                );
            }
            return None;
        }
        tracing::info!(%pamm, "Serving auto-detected pAMM");
        let config = PriceLevelStreamConfig::auto_detected(
            pamm.clone(),
            self.auto_detected_gas_cost.clone(),
        );
        telemetry::served_components(&config.protocol, 0);
        telemetry::last_seen(&config.protocol, 0);
        self.registry
            .insert(pamm.clone(), config.clone());
        Some(config)
    }

    /// Merges the frame's per-direction ladders into one entry per unordered token pair,
    /// skipping pairs with unknown tokens. A direction the frame does not carry yields an empty
    /// ladder, so that direction cannot be quoted.
    fn merge_pairs(
        &self,
        pairs: Vec<TitanPairLevels>,
    ) -> HashMap<(Bytes, Bytes), (Vec<PriceLevelStreamQuote>, Vec<PriceLevelStreamQuote>)> {
        let mut merged: HashMap<(Bytes, Bytes), (Vec<_>, Vec<_>)> = HashMap::new();
        for TitanPairLevels { token_in, token_out, order_book } in pairs {
            if !self.tokens.contains_key(&token_in) || !self.tokens.contains_key(&token_out) {
                tracing::debug!(%token_in, %token_out, "Skipping pair with unknown token");
                continue;
            }
            let sells_token0 = token_in < token_out;
            let key = if sells_token0 {
                (token_in.clone(), token_out.clone())
            } else {
                (token_out.clone(), token_in.clone())
            };
            let quotes = order_book
                .into_iter()
                .map(|TitanPriceLevel { amount_in, amount_out }| {
                    PriceLevelStreamQuote::new(amount_in, amount_out)
                })
                .collect();
            let entry = merged.entry(key).or_default();
            if sells_token0 {
                entry.0 = quotes;
            } else {
                entry.1 = quotes;
            }
        }
        merged
    }

    /// Removes every served component whose deadline has passed, as one [`Update`].
    pub(super) fn on_stale_deadline(&mut self, now: Now) -> Option<Update> {
        let due: Vec<String> = self
            .served
            .iter()
            .filter(|(_, served)| served.deadline <= now.instant)
            .map(|(id, _)| id.clone())
            .collect();
        if due.is_empty() {
            return None;
        }
        let mut removed = HashMap::with_capacity(due.len());
        let mut venues = BTreeSet::new();
        for id in due {
            let Some(served) = self.served.remove(&id) else {
                continue;
            };
            telemetry::stale_removal(&served.venue);
            venues.insert(served.venue);
            removed.insert(id, served.component);
        }
        let component_ids: Vec<&String> = removed.keys().collect();
        tracing::warn!(
            removed = removed.len(),
            venues = ?venues,
            components = ?component_ids,
            stale_after_secs = self.stale_after.as_secs(),
            "Removing price level components: no fresh frame carried them within the window"
        );
        // Build the update off the still-current frontier before `refresh_source` may reset it
        // (it does, once nothing is left served) — a removal always reports the block it was
        // last known fresh at, never the reset value.
        let update = self.removal_update(removed);
        self.refresh_source();
        Some(update)
    }

    /// The instant the earliest served component turns stale, if anything is served.
    pub(super) fn stale_deadline(&self) -> Option<Instant> {
        match self.source {
            Source::Serving { deadline } => Some(deadline),
            Source::AwaitingWhitelist | Source::Unserved => None,
        }
    }

    fn removal_update(&self, removed: HashMap<String, ProtocolComponent>) -> Update {
        Update::new(self.newest_block, HashMap::new(), HashMap::new())
            .set_is_partial(true)
            .set_removed_pairs(removed)
    }

    /// Recomputes the source state and the per-venue gauges from the served set. The block
    /// frontier is reset whenever nothing is served — after the last served component expired,
    /// after a frame that carried nothing this tracker can build, and on every frame accepted
    /// while the whitelist is still awaited — so one absurd block can never outlive the window
    /// it was served for. The whitelist wait itself is left only by a successful read, never by
    /// the served set.
    fn refresh_source(&mut self) {
        let awaiting_whitelist = match self.source {
            Source::AwaitingWhitelist => true,
            Source::Unserved | Source::Serving { deadline: _ } => false,
        };
        let served_state = match self
            .served
            .values()
            .map(|served| served.deadline)
            .min()
        {
            Some(deadline) => Source::Serving { deadline },
            None => {
                self.newest_block = 0;
                self.last_accepted = None;
                Source::Unserved
            }
        };
        if !awaiting_whitelist {
            self.source = served_state;
        }
        telemetry::source_state(self.source.telemetry_state());
        let mut per_venue: HashMap<&str, usize> = self
            .registry
            .values()
            .map(|config| (config.protocol.as_str(), 0))
            .collect();
        for served in self.served.values() {
            *per_venue
                .entry(served.venue.as_str())
                .or_default() += 1;
        }
        for (venue, count) in per_venue {
            telemetry::served_components(venue, count);
        }
    }

    /// WARN for the first rejected frame of a streak, DEBUG for the rest.
    fn log_rejection(&mut self, rejection: Rejection, frame: &TitanPriceLevelMessage) {
        if self.rejecting {
            tracing::debug!(
                reason = rejection.as_str(),
                block_number = frame.block_number,
                timestamp = frame.timestamp,
                "Rejecting price level frame"
            );
            return;
        }
        self.rejecting = true;
        tracing::warn!(
            reason = rejection.as_str(),
            block_number = frame.block_number,
            timestamp = frame.timestamp,
            newest_block = self.newest_block,
            newest_timestamp = self.newest_timestamp,
            "Rejecting price level frame; further rejections logged at debug until a frame is \
             accepted"
        );
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

    use super::{
        super::{
            config::DEFAULT_AUTO_DETECTED_GAS_COST,
            fallback_router::{FetchVenuesError, RouterVenuesRead},
            state::QUOTE_TTL,
            test_support::*,
        },
        *,
    };

    /// 2026-09-05 16:09:18 UTC, the first frame of the live capture.
    const BASE_WALL_NANOS: u64 = 1_788_624_558_000_000_000;

    /// A test clock: wall and monotonic time both start at zero offset from a fixed base. Tests
    /// must only ever ask for non-decreasing seconds, mirroring a monotonic clock.
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
                instant: self.start + Duration::from_secs(seconds),
            }
        }
    }

    fn tracker() -> SnapshotTracker {
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        )
    }

    fn level(amount_in: u64, amount_out: u64) -> TitanPriceLevel {
        TitanPriceLevel {
            amount_in: BigUint::from(amount_in),
            amount_out: BigUint::from(amount_out),
        }
    }

    fn pair_levels(
        token_in: &str,
        token_out: &str,
        order_book: Vec<TitanPriceLevel>,
    ) -> TitanPairLevels {
        TitanPairLevels {
            token_in: Bytes::from_str(token_in).unwrap(),
            token_out: Bytes::from_str(token_out).unwrap(),
            order_book,
        }
    }

    /// A frame built `seconds` after the base instant.
    fn message_at(
        block_number: u64,
        seconds: u64,
        pairs: Vec<TitanPairLevels>,
    ) -> TitanPriceLevelMessage {
        TitanPriceLevelMessage {
            block_number,
            timestamp: BASE_WALL_NANOS + seconds * NANOS_PER_SECOND,
            pamms: vec![TitanPammLevels { pamm: Bytes::from_str(PAMM).unwrap(), pairs }],
        }
    }

    fn message(block_number: u64, pairs: Vec<TitanPairLevels>) -> TitanPriceLevelMessage {
        message_at(block_number, 0, pairs)
    }

    fn wbtc_usdc_pairs() -> Vec<TitanPairLevels> {
        vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
        ]
    }

    fn expected_id() -> String {
        // pamm ++ token0 ++ token1 with WBTC < USDC.
        format!("{PAMM}{}{}", &WBTC[2..], &USDC[2..])
    }

    fn quotable_until(update: &Update) -> Instant {
        update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state")
            .quotable_until
            .expect("guard set")
    }

    #[test]
    fn first_snapshot_emits_new_pair_with_both_directions() {
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
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
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

        let PriceLevelStreamState {
            token0,
            token1,
            quotes_0_to_1,
            quotes_1_to_0,
            gas_cost,
            quotable_until: _,
        } = states[&id]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(token0, &Bytes::from_str(WBTC).unwrap());
        assert_eq!(token1, &Bytes::from_str(USDC).unwrap());
        assert_eq!(quotes_0_to_1.len(), 1);
        assert_eq!(quotes_1_to_0.len(), 1);
        assert_eq!(quotes_0_to_1[0].amount_in, BigUint::from(100_000_000u64));
        assert_eq!(gas_cost, &BigUint::from(120_000u64));
    }

    #[test]
    fn repeated_snapshot_is_not_a_new_pair() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        let update = tracker
            .on_frame(message(101, wbtc_usdc_pairs()), clock.at(0))
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
            .on_frame(message_at(101, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let regressed =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        assert!(tracker
            .on_frame(message_at(100, 1, regressed), clock.at(1))
            .is_none());

        let update = tracker
            .on_frame(message_at(102, 2, wbtc_usdc_pairs()), clock.at(2))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    #[test]
    fn data_exactly_stale_after_old_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=0, judged at t=24: exactly the window, already stale.
        assert!(tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(24))
            .is_none());
        // One second younger is accepted.
        assert!(tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(24))
            .is_some());
    }

    #[test]
    fn frame_from_the_future_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        assert!(tracker
            .on_frame(message_at(100, 13, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
        assert!(tracker
            .on_frame(message_at(100, 12, wbtc_usdc_pairs()), clock.at(0))
            .is_some());
    }

    #[test]
    fn older_timestamp_frame_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(5))
            .expect("update expected");
        assert!(tracker
            .on_frame(message_at(100, 4, wbtc_usdc_pairs()), clock.at(5))
            .is_none());
    }

    #[test]
    fn equal_timestamp_frame_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(5))
            .expect("update expected");
        // Titan re-emits within a build round with the same timestamp and different content.
        let changed = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 101_000_000_000)]),
            pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
        ];
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
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // One second later the chain cannot have advanced by more than 0 + 2 blocks.
        assert!(tracker
            .on_frame(message_at(103, 1, wbtc_usdc_pairs()), clock.at(1))
            .is_none());
        assert!(tracker
            .on_frame(message_at(u64::MAX / 2, 1, wbtc_usdc_pairs()), clock.at(1))
            .is_none());
        // The tracker is not frozen: a plausible block is still accepted afterwards.
        assert!(tracker
            .on_frame(message_at(101, 2, wbtc_usdc_pairs()), clock.at(2))
            .is_some());
    }

    #[test]
    fn block_jump_within_the_elapsed_bound_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // 60 s later: 60 / 12 + 2 = 7 blocks allowed.
        assert!(tracker
            .on_frame(message_at(107, 60, wbtc_usdc_pairs()), clock.at(60))
            .is_some());
    }

    #[test]
    fn first_frame_skips_the_block_checks() {
        let clock = Clock::new();
        let mut tracker = tracker();
        assert!(tracker
            .on_frame(message_at(25_912_232, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_some());
    }

    #[test]
    fn rejections_are_counted_by_reason() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{counter_value, snapshot_map},
            FRAMES_ACCEPTED, FRAMES_REJECTED,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(5));
            // Still fresh (16 s old) but stamped before the accepted frame: out_of_order.
            tracker.on_frame(message_at(100, 4, wbtc_usdc_pairs()), clock.at(20));
            tracker.on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(29)); // too_old
            tracker.on_frame(message_at(100, 45, wbtc_usdc_pairs()), clock.at(29)); // in_future
            tracker.on_frame(message_at(99, 6, wbtc_usdc_pairs()), clock.at(29)); // block_regression
            tracker.on_frame(message_at(200, 6, wbtc_usdc_pairs()), clock.at(29)); // block_jump
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
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
    fn quote_guard_is_one_block_after_the_frame_was_built() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=7, accepted at t=9: the data is 2 s old, so 10 s of quotability remain.
        let update = tracker
            .on_frame(message_at(100, 7, wbtc_usdc_pairs()), clock.at(9))
            .expect("update expected");
        assert_eq!(
            quotable_until(&update),
            clock.at(9).instant + (QUOTE_TTL - Duration::from_secs(2))
        );
    }

    #[test]
    fn quote_guard_never_exceeds_one_block_for_a_future_stamped_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Maximum accepted skew: stamped 12 s ahead of the local clock.
        let update = tracker
            .on_frame(message_at(100, 12, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(0).instant + QUOTE_TTL);
    }

    #[test]
    fn frame_older_than_one_block_yields_an_unquotable_state() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // 15 s old: inside the 24 s window, past the 12 s quote guard.
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(15))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(15).instant);
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );
    }

    #[test]
    fn omitted_pair_is_not_removed_by_the_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let weth_usdc =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        let update = tracker
            .on_frame(message_at(101, 1, weth_usdc), clock.at(1))
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
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        let empty = TitanPriceLevelMessage {
            block_number: 101,
            timestamp: BASE_WALL_NANOS + NANOS_PER_SECOND,
            pamms: vec![],
        };
        assert!(tracker
            .on_frame(empty, clock.at(1))
            .is_none());
        assert!(tracker.stale_deadline().is_some());
    }

    #[test]
    fn component_expires_at_its_own_deadline_and_is_re_added_by_the_next_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );

        // One second early: nothing due.
        assert!(tracker
            .on_stale_deadline(clock.at(23))
            .is_none());

        let update = tracker
            .on_stale_deadline(clock.at(24))
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

        // Firing again with nothing served emits nothing.
        assert!(tracker
            .on_stale_deadline(clock.at(25))
            .is_none());

        let update = tracker
            .on_frame(message_at(102, 26, wbtc_usdc_pairs()), clock.at(26))
            .expect("update expected");
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn stale_removal_is_counted_per_venue() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{counter_value, snapshot_map},
            STALE_REMOVALS,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker
                .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
                .expect("update expected");
            tracker
                .on_stale_deadline(clock.at(24))
                .expect("removal expected");
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("pamm", "fermiswap")]), 1);
    }

    #[test]
    fn deadline_is_shortened_by_the_frame_age_at_acceptance() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=0, accepted at t=3: the data is already 3 s old.
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(3))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );
    }

    #[test]
    fn replayed_frame_does_not_extend_the_deadline() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // The same frame arriving again 10 s later carries the same timestamp.
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(10))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );
    }

    #[test]
    fn omitted_pair_expires_alone_while_the_rest_stays_served() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
            pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
        ];
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        // Only WETH/USDC keeps being carried, up to t=23; the clock never goes backwards.
        for second in 1..=23 {
            let weth_usdc = vec![pair_levels(
                WETH,
                USDC,
                vec![level(1_000_000_000_000_000_000, 3_000_000_000)],
            )];
            tracker.on_frame(message_at(100 + second / 12, second, weth_usdc), clock.at(second));
        }
        let update = tracker
            .on_stale_deadline(clock.at(24))
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
            clock.at(47).instant
        );
    }

    #[test]
    fn components_from_one_frame_expire_together() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
        ];
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        let update = tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        assert_eq!(update.removed_pairs.len(), 2);
    }

    #[test]
    fn pair_set_oscillation_emits_no_removal() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let wide = || {
            vec![
                pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
                pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
                pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
            ]
        };
        let narrow =
            || vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
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
    fn poisoned_first_block_recovers_after_expiry() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // A fresh first frame with an absurd block becomes the frontier.
        tracker
            .on_frame(message_at(u64::MAX / 2, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // Sane frames regress below it and are rejected for the whole window.
        for second in 1..=23 {
            assert!(tracker
                .on_frame(message_at(100, second, wbtc_usdc_pairs()), clock.at(second))
                .is_none());
        }
        // Expiry clears the served set and resets the frontier.
        let removal = tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        assert_eq!(removal.removed_pairs.len(), 1);
        assert_eq!(removal.block_number_or_timestamp, u64::MAX / 2);
        // The next sane frame is accepted as a first frame again, and nothing from the
        // poisoned frame survives: the pair comes back as new.
        let update = tracker
            .on_frame(message_at(100, 25, wbtc_usdc_pairs()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn poisoned_block_during_whitelist_wait_does_not_wedge_the_tracker() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        // Accepted, but nothing is served while the whitelist is unknown, so the absurd block
        // must not survive as the frontier.
        assert!(tracker
            .on_frame(message_at(u64::MAX / 2, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_none());

        assert!(tracker
            .on_router_venues(read_ok(&[PAMM]))
            .is_none());

        // The first frame that can be served is judged as a first frame, not against the block
        // of a frame that served nothing.
        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn poisoned_block_on_a_frame_serving_nothing_does_not_wedge_the_tracker() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Fresh enough to be accepted, but its only pair prices an unknown token, so the frame
        // serves nothing and its absurd block must not survive as the frontier.
        let unknown = vec![pair_levels(
            "0x1111111111111111111111111111111111111111",
            USDC,
            vec![level(1, 1)],
        )];
        assert!(tracker
            .on_frame(message_at(u64::MAX / 2, 0, unknown), clock.at(0))
            .is_none());

        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(1))
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
        let both = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
        ];
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        // After the outage only WETH/USDC comes back: WBTC/USDC stays absent.
        let weth_usdc =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        let update = tracker
            .on_frame(message_at(102, 25, weth_usdc), clock.at(25))
            .expect("update expected");
        assert_eq!(update.new_pairs.len(), 1);
        assert!(!update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn one_direction_frame_re_adds_the_pair_with_the_other_direction_unquotable() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let one_way = vec![pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)])];
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

    #[test]
    fn served_components_are_gauged_per_venue_from_zero() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{gauge_value, snapshot_map},
            LAST_SEEN, SERVED_COMPONENTS, SOURCE_STATE,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let tracker = tracker();
            drop(tracker);
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("pamm", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, LAST_SEEN, &[("pamm", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, SOURCE_STATE, &[]), 1.0);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0));
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("pamm", "fermiswap")]), 1.0);
        assert_eq!(
            gauge_value(&snapshot, LAST_SEEN, &[("pamm", "fermiswap")]),
            (BASE_WALL_NANOS / NANOS_PER_SECOND) as f64
        );
        assert_eq!(gauge_value(&snapshot, SOURCE_STATE, &[]), 2.0);
    }

    #[test]
    fn unregistered_pamm_produces_no_update_without_auto_detection() {
        let clock = Clock::new();
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        assert!(tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
    }

    #[test]
    fn denied_pamm_is_not_auto_detected() {
        let clock = Clock::new();
        let denied = HashSet::from([Bytes::from_str(PAMM).unwrap()]);
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            denied,
            tokens(),
            true,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        assert!(tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
    }

    #[test]
    fn auto_detected_pamm_is_served_under_its_address() {
        let clock = Clock::new();
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            true,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        let update = tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let component = &update.new_pairs[&expected_id()];
        assert_eq!(component.protocol_system, format!("pricelevelstream:{PAMM}"));
        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.gas_cost, BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));

        // The synthesized config is cached: the next snapshot is not a new pair again.
        let update = tracker
            .on_frame(message(101, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    #[test]
    fn auto_detected_gas_cost_override_applies() {
        let clock = Clock::new();
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            true,
            BigUint::from(42_000u64),
            DEFAULT_STALE_AFTER,
            false,
        );
        let update = tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.gas_cost, BigUint::from(42_000u64));
    }

    /// A venue on the router's whitelist is emitted under `propammfallback:{name}`, so its swaps
    /// execute through Titan's PropAMMRouter; identity and attributes stay the same.
    #[test]
    fn whitelisted_venue_is_served_under_the_fallback_family() {
        let clock = Clock::new();
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        let mut tracker = SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        tracker.set_router_venues_for_test(HashSet::from([Bytes::from_str(PAMM).unwrap()]));

        let update = tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let component = &update.new_pairs[&expected_id()];
        assert_eq!(component.protocol_system, "propammfallback:fermiswap");
        assert_eq!(
            component.static_attributes[PAMM_ADDRESS_ATTRIBUTE],
            Bytes::from_str(PAMM).unwrap()
        );
    }

    /// The whitelist check is by address, so it also covers auto-detected, address-named venues.
    #[test]
    fn auto_detected_whitelisted_venue_is_served_under_the_fallback_family() {
        let clock = Clock::new();
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            true,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        tracker.set_router_venues_for_test(HashSet::from([Bytes::from_str(PAMM).unwrap()]));

        let update = tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let component = &update.new_pairs[&expected_id()];
        assert_eq!(component.protocol_system, format!("propammfallback:{PAMM}"));
    }

    /// A venue absent from the whitelist keeps the direct `pricelevelstream:{name}` family — the
    /// router reverts `UnknownVenue` for it, which would send every swap to the Uniswap V3
    /// fallback.
    #[test]
    fn unwhitelisted_venue_keeps_the_direct_family() {
        let clock = Clock::new();
        let other_venue =
            Bytes::from_str("0x71e790dd841c8a9061487cb3e78c288e75ce0b3d").expect("valid address");
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        let mut tracker = SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        tracker.set_router_venues_for_test(HashSet::from([other_venue]));

        let update = tracker
            .on_frame(message(100, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "pricelevelstream:fermiswap");
    }

    fn read_ok(addresses: &[&str]) -> RouterVenuesRead {
        RouterVenuesRead::Ok(
            addresses
                .iter()
                .map(|address| Bytes::from_str(address).unwrap())
                .collect(),
        )
    }

    fn tracker_awaiting_whitelist() -> SnapshotTracker {
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            true,
        )
    }

    #[test]
    fn nothing_is_emitted_until_the_whitelist_is_known() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        assert!(tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
        assert!(tracker.stale_deadline().is_none());

        assert!(tracker
            .on_router_venues(read_ok(&[PAMM]))
            .is_none());
        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "propammfallback:fermiswap");
    }

    #[test]
    fn failed_whitelist_read_keeps_the_previous_set() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(read_ok(&[PAMM]));
        assert!(tracker
            .on_router_venues(RouterVenuesRead::Failed(FetchVenuesError::Call {
                reason: "boom".to_string(),
            }))
            .is_none());
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "propammfallback:fermiswap");
    }

    #[test]
    fn failed_first_read_keeps_waiting() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(RouterVenuesRead::Failed(FetchVenuesError::Call {
            reason: "boom".to_string(),
        }));
        assert!(tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
    }

    #[test]
    fn family_change_removes_now_and_re_adds_under_the_new_family() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(read_ok(&[PAMM]));
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        // The venue gets de-whitelisted.
        let update = tracker
            .on_router_venues(read_ok(&[]))
            .expect("removal expected");
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert_eq!(
            update.removed_pairs[&expected_id()].protocol_system,
            "propammfallback:fermiswap"
        );
        assert!(tracker.stale_deadline().is_none());

        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "pricelevelstream:fermiswap");
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn unchanged_whitelist_emits_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(read_ok(&[PAMM]));
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert!(tracker
            .on_router_venues(read_ok(&[PAMM]))
            .is_none());
    }

    #[test]
    fn whitelist_read_gauges_the_venue_count() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{gauge_value, snapshot_map},
            WHITELISTED_VENUES,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let mut tracker = tracker_awaiting_whitelist();
            tracker.on_router_venues(read_ok(&[PAMM]));
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, WHITELISTED_VENUES, &[]), 1.0);
    }

    #[test]
    fn unregistered_venues_are_counted_without_labels_and_logged_boundedly() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{counter_value, snapshot_map},
            UNREGISTERED_PAMM_FRAMES,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let logged = metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = SnapshotTracker::new(
                HashMap::new(),
                HashSet::new(),
                tokens(),
                false,
                BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
                DEFAULT_STALE_AFTER,
                false,
            );
            // 70 distinct unregistered addresses, each seen twice.
            for round in 0..2u64 {
                for index in 0..70u64 {
                    let address = Bytes::from(
                        format!("{index:040x}")
                            .as_bytes()
                            .to_vec(),
                    );
                    let frame = TitanPriceLevelMessage {
                        block_number: 100,
                        timestamp: BASE_WALL_NANOS + (round * 70 + index) * 1_000,
                        pamms: vec![TitanPammLevels { pamm: address, pairs: wbtc_usdc_pairs() }],
                    };
                    tracker.on_frame(frame, clock.at(1));
                }
            }
            tracker.seen_unregistered.len()
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_FRAMES, &[]), 140);
        assert_eq!(logged, MAX_UNREGISTERED_LOGGED);
    }

    #[test]
    fn unknown_tokens_are_skipped() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let unknown = vec![pair_levels(
            "0x1111111111111111111111111111111111111111",
            USDC,
            vec![level(1, 1)],
        )];
        assert!(tracker
            .on_frame(message(100, unknown), clock.at(0))
            .is_none());
    }
}
