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
    fallback_router::WhitelistRead,
    state::{PriceLevelStreamQuote, PriceLevelStreamState, QUOTE_TTL},
    stream::PAMM_ADDRESS_ATTRIBUTE,
    telemetry,
    titan::{TitanPairLevels, TitanPammLevels, TitanPriceLevel, TitanPriceLevelMessage},
};
use crate::protocol::models::{ProtocolComponent, Update};

/// How long a component stays served after the last accepted frame that carried it: two slots.
/// Titan streams at 1 Hz and its frames arrive within about 3 s of being built, so 24 s absorbs
/// jitter and still removes a silent component within two blocks.
pub(super) const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(24);

pub(super) const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Furthest a frame's `timestamp` may lie ahead of the local wall clock: one slot. Titan's
/// frames were observed at least 120 ms behind the local clock; anything ahead of it by more
/// than clock skew is implausible.
const MAX_FUTURE_SKEW_NANOS: u64 = 12 * NANOS_PER_SECOND;

/// Post-merge Ethereum slot duration in seconds.
const SECONDS_PER_SLOT: u64 = 12;

/// Extra blocks a frame may jump beyond one block per elapsed slot. Titan builds at chain head + 1
/// and sometimes + 2, so a frame right after a block boundary may jump by two.
const BLOCK_JUMP_SLACK: u64 = 2;

/// How many distinct unregistered venue addresses the tracker logs at INFO, once each. The
/// addresses come from the wire, so the set is capped to keep a misbehaving upstream from growing
/// memory, and no address becomes a metric label.
const MAX_UNREGISTERED_LOGGED: usize = 64;

/// The wall-clock and monotonic time at which the tracker handles a frame or a deadline.
#[derive(Clone, Copy, Debug)]
pub(super) struct Now {
    /// Wall clock, nanoseconds since the Unix epoch. Only ever compared against Titan's
    /// `timestamp`.
    pub wall_nanos: u64,
    /// Monotonic clock. Every deadline is computed from it, so a wall-clock jump cannot remove a
    /// component early or keep it served late.
    pub monotonic: Instant,
}

impl Now {
    /// Reads both clocks. A system clock unrepresentable as unix nanoseconds yields
    /// `wall_nanos = 0`, which rejects every frame as `in_future`.
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
        Self { wall_nanos, monotonic: Instant::now() }
    }
}

/// Why the tracker rejected a parsed frame.
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
    /// Targets a block above the newest accepted block by more than one block per elapsed 12 s
    /// plus [`BLOCK_JUMP_SLACK`].
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

/// A component the stream currently serves.
struct ServedComponent {
    component: ProtocolComponent,
    /// The venue name, for logs and metric labels.
    venue_name: String,
    /// The venue address, for whitelist membership checks.
    venue_address: Bytes,
    /// The instant this component's data turns `stale_after` old.
    stale_at: Instant,
}

/// The stream's serving state.
enum ServingState {
    /// The PropAMMRouter whitelist has not been read yet: the family of every component is
    /// unknown, so frames are validated but nothing is emitted.
    AwaitingWhitelist,
    /// Whitelist known (or not needed) and nothing served: at start, and whenever the last
    /// served component turned stale.
    Unserved,
    /// At least one component is served; `deadline` is the earliest component deadline.
    Serving { deadline: Instant },
}

impl ServingState {
    fn telemetry_state(&self) -> telemetry::ServingState {
        match self {
            ServingState::AwaitingWhitelist => telemetry::ServingState::AwaitingWhitelist,
            ServingState::Unserved => telemetry::ServingState::Unserved,
            ServingState::Serving { deadline: _ } => telemetry::ServingState::Serving,
        }
    }
}

/// Turns Titan frames into [`Update`]s. A frame only adds or refreshes components: a component (or
/// a whole venue) that a frame omits stays served until its own deadline passes in
/// [`on_stale_deadline`](Self::on_stale_deadline).
pub(super) struct FreshnessTracker {
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
    served: HashMap<String, ServedComponent>,
    /// The stream's serving state; drives [`Self::stale_deadline`] and the per-venue gauges.
    serving_state: ServingState,
    /// How long a component stays served after the last accepted frame that carried it; see
    /// [`DEFAULT_STALE_AFTER`].
    stale_after: Duration,
    /// The `timestamp` of the newest accepted frame. Frames older than it are out of order;
    /// equal ones are re-emissions within a build round and accepted. Never reset.
    newest_timestamp_nanos: u64,
    /// The block of the newest accepted frame. Later frames may not regress below it, nor jump
    /// more than one block per elapsed slot plus 2. Reset with `last_accepted`.
    newest_block: u64,
    /// When the newest frame was accepted; `None` whenever nothing is served, which skips the
    /// block checks for the next frame. It is set only while the stream serves something: a
    /// frame that serves nothing — because the whitelist is still awaited, or because it carries
    /// nothing this tracker can build — resets it immediately.
    last_accepted: Option<Instant>,
    /// Whether the last frame was rejected. The first rejection of a streak logs at WARN and the
    /// rest at DEBUG; `price_level_stream_frames_rejected_total` counts every rejection.
    rejecting: bool,
    /// Unregistered venue addresses already logged, at most [`MAX_UNREGISTERED_LOGGED`].
    logged_unregistered: HashSet<Bytes>,
}

impl FreshnessTracker {
    pub(super) fn new(
        registry: HashMap<Bytes, PriceLevelStreamConfig>,
        denied: HashSet<Bytes>,
        tokens: HashMap<Bytes, Token>,
        auto_detect: bool,
        auto_detected_gas_cost: BigUint,
        stale_after: Duration,
        fallback_router: bool,
    ) -> Self {
        let serving_state =
            if fallback_router { ServingState::AwaitingWhitelist } else { ServingState::Unserved };
        telemetry::record_serving_state(serving_state.telemetry_state());
        // Pre-initialise every per-venue series so a venue that never appears is a visible
        // zero, not a missing series.
        for config in registry.values() {
            telemetry::record_served_components(&config.protocol, 0);
            telemetry::record_last_seen(&config.protocol, 0);
        }
        Self {
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            router_venues: HashSet::new(),
            served: HashMap::new(),
            serving_state,
            stale_after,
            newest_timestamp_nanos: 0,
            newest_block: 0,
            last_accepted: None,
            rejecting: false,
            logged_unregistered: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn set_router_venues_for_test(&mut self, venues: HashSet<Bytes>) {
        self.router_venues = venues;
    }

    /// Applies a whitelist read. A failed read keeps the last known set. A successful read that
    /// changes a served venue's family removes that venue's components now; the next accepted
    /// frame carrying them re-adds them under the new family.
    pub(super) fn on_whitelist_read(&mut self, read: WhitelistRead) -> Option<Update> {
        let venues = match read {
            WhitelistRead::Failed(error) => {
                tracing::debug!(error = %error, "Whitelist read failed; whitelist unchanged");
                return None;
            }
            WhitelistRead::Ok(venues) => venues,
        };
        telemetry::record_whitelisted_venues(venues.len());
        if let ServingState::AwaitingWhitelist = self.serving_state {
            self.router_venues = venues;
            tracing::info!(
                venues = self.router_venues.len(),
                "PropAMMRouter venue whitelist read; serving pAMMs from the next frame"
            );
            self.serving_state = ServingState::Unserved;
            telemetry::record_serving_state(self.serving_state.telemetry_state());
            return None;
        }
        let moved: Vec<String> = self
            .served
            .iter()
            .filter(|(_, served)| {
                self.router_venues
                    .contains(&served.venue_address) !=
                    venues.contains(&served.venue_address)
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
                venue = %served.venue_name,
                "pAMM changed PropAMMRouter whitelist membership; re-adding it under its new \
                 family on the next frame"
            );
            removed.insert(id, served.component);
        }
        // Build the update before `refresh_serving_state`, which resets `newest_block` to 0 once
        // nothing is served: the removal must carry the newest accepted block.
        let update = self.removal_update(removed);
        self.refresh_serving_state();
        Some(update)
    }

    /// Returns the frame's age if the frame passes the timestamp and block checks, or the
    /// [`Rejection`] that names the first failed check.
    fn check_frame(&self, frame: &TitanPriceLevelMessage, now: Now) -> Result<Duration, Rejection> {
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
        if frame.timestamp < self.newest_timestamp_nanos {
            return Err(Rejection::OutOfOrder);
        }
        let Some(last_accepted) = self.last_accepted else {
            return Ok(frame_age);
        };
        if frame.block_number < self.newest_block {
            return Err(Rejection::BlockRegression);
        }
        let elapsed_slots = now
            .monotonic
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
    /// [`Rejection`]), the whitelist has not been read yet, or the frame carries no served pAMM
    /// with a pair of known tokens.
    pub(super) fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update> {
        let frame_age = match self.check_frame(&frame, now) {
            Ok(frame_age) => frame_age,
            Err(rejection) => {
                self.log_rejection(rejection, &frame);
                telemetry::record_frame_rejected(rejection.as_str());
                return None;
            }
        };
        self.rejecting = false;
        telemetry::record_frame_accepted();
        self.newest_timestamp_nanos = frame.timestamp;
        self.newest_block = frame.block_number;
        self.last_accepted = Some(now.monotonic);

        if let ServingState::AwaitingWhitelist = self.serving_state {
            self.refresh_serving_state();
            return None;
        }

        let deadline = now.monotonic +
            self.stale_after
                .saturating_sub(frame_age);
        // `QUOTE_TTL` counts from the frame's wire `timestamp` and is enforced on the monotonic
        // clock; it does not depend on whether the ladder's content changed, because quiet
        // venues repeat a ladder for minutes.
        let quotable_until = now.monotonic + QUOTE_TTL.saturating_sub(frame_age);
        let frame_unix_seconds = frame.timestamp / NANOS_PER_SECOND;
        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();

        for TitanPammLevels { pamm, pairs } in frame.pamms {
            let Some(config) = self.resolve_config(&pamm) else {
                continue;
            };
            telemetry::record_last_seen(&config.protocol, frame_unix_seconds);

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
                    ServedComponent {
                        component,
                        venue_name: config.protocol.clone(),
                        venue_address: config.address.clone(),
                        stale_at: deadline,
                    },
                );
            }
        }

        self.refresh_serving_state();
        if states.is_empty() {
            return None;
        }
        Some(Update::new(frame.block_number, states, new_pairs).set_is_partial(true))
    }

    /// The configuration a streamed venue is served under. `None` when the venue is denied, or
    /// unregistered while auto-detection is off. An auto-detected venue is added to the registry.
    fn resolve_config(&mut self, pamm: &Bytes) -> Option<PriceLevelStreamConfig> {
        if let Some(config) = self.registry.get(pamm) {
            return Some(config.clone());
        }
        if self.denied.contains(pamm) {
            tracing::debug!(%pamm, "Skipping denied pAMM");
            return None;
        }
        if !self.auto_detect {
            telemetry::record_unregistered_pamm();
            if self.logged_unregistered.len() < MAX_UNREGISTERED_LOGGED &&
                self.logged_unregistered
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
        telemetry::record_served_components(&config.protocol, 0);
        telemetry::record_last_seen(&config.protocol, 0);
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
            .filter(|(_, served)| served.stale_at <= now.monotonic)
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
            telemetry::record_stale_removal(&served.venue_name);
            venues.insert(served.venue_name);
            removed.insert(id, served.component);
        }
        let component_ids: Vec<&String> = removed.keys().collect();
        tracing::warn!(
            removed = removed.len(),
            venues = ?venues,
            components = ?component_ids,
            stale_after_secs = self.stale_after.as_secs(),
            "Removing price level components: no accepted frame carried them within stale_after"
        );
        // Build the update before `refresh_serving_state`, which resets `newest_block` to 0 once
        // nothing is served: the removal must carry the newest accepted block.
        let update = self.removal_update(removed);
        self.refresh_serving_state();
        Some(update)
    }

    /// The instant the earliest served component turns stale, if anything is served.
    pub(super) fn stale_deadline(&self) -> Option<Instant> {
        match self.serving_state {
            ServingState::Serving { deadline } => Some(deadline),
            ServingState::AwaitingWhitelist | ServingState::Unserved => None,
        }
    }

    fn removal_update(&self, removed: HashMap<String, ProtocolComponent>) -> Update {
        Update::new(self.newest_block, HashMap::new(), HashMap::new())
            .set_is_partial(true)
            .set_removed_pairs(removed)
    }

    /// Recomputes the serving state and the per-venue gauges from the served set. When nothing is
    /// served it also resets `newest_block` and `last_accepted`, so the next frame skips the block
    /// checks. This happens after the last stale removal, after a frame whose pairs were all
    /// skipped, and on every frame accepted while the whitelist is awaited. A frame with an
    /// implausible block therefore cannot cause rejections for longer than `stale_after`. Only
    /// [`on_whitelist_read`](Self::on_whitelist_read) leaves `AwaitingWhitelist`.
    fn refresh_serving_state(&mut self) {
        let awaiting_whitelist = match self.serving_state {
            ServingState::AwaitingWhitelist => true,
            ServingState::Unserved | ServingState::Serving { deadline: _ } => false,
        };
        let served_state = match self
            .served
            .values()
            .map(|served| served.stale_at)
            .min()
        {
            Some(deadline) => ServingState::Serving { deadline },
            None => {
                self.newest_block = 0;
                self.last_accepted = None;
                ServingState::Unserved
            }
        };
        if !awaiting_whitelist {
            self.serving_state = served_state;
        }
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
            newest_timestamp_nanos = self.newest_timestamp_nanos,
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
            fallback_router::{FetchVenuesError, WhitelistRead},
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
                monotonic: self.start + Duration::from_secs(seconds),
            }
        }
    }

    fn tracker() -> FreshnessTracker {
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        FreshnessTracker::new(
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
            recorded::{counter_value, snapshot_map},
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
            clock.at(9).monotonic + (QUOTE_TTL - Duration::from_secs(2))
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
        assert_eq!(quotable_until(&update), clock.at(0).monotonic + QUOTE_TTL);
    }

    #[test]
    fn frame_older_than_one_block_yields_an_unquotable_state() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // 15 s old: inside the 24 s window, past the 12 s quote guard.
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(15))
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
            clock.at(24).monotonic
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
            recorded::{counter_value, snapshot_map},
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
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("venue", "fermiswap")]), 1);
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
            clock.at(24).monotonic
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
            clock.at(24).monotonic
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
            clock.at(47).monotonic
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
            .on_whitelist_read(read_ok(&[PAMM]))
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
            recorded::{gauge_value, snapshot_map},
            LAST_SEEN, SERVED_COMPONENTS, SERVING_STATE,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let tracker = tracker();
            drop(tracker);
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("venue", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, LAST_SEEN, &[("venue", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 1.0);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0));
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("venue", "fermiswap")]), 1.0);
        assert_eq!(
            gauge_value(&snapshot, LAST_SEEN, &[("venue", "fermiswap")]),
            (BASE_WALL_NANOS / NANOS_PER_SECOND) as f64
        );
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 2.0);
    }

    #[test]
    fn unregistered_pamm_produces_no_update_without_auto_detection() {
        let clock = Clock::new();
        let mut tracker = FreshnessTracker::new(
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
        let mut tracker = FreshnessTracker::new(
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
        let mut tracker = FreshnessTracker::new(
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
        let mut tracker = FreshnessTracker::new(
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
        let mut tracker = FreshnessTracker::new(
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
        let mut tracker = FreshnessTracker::new(
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
        let mut tracker = FreshnessTracker::new(
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

    fn read_ok(addresses: &[&str]) -> WhitelistRead {
        WhitelistRead::Ok(
            addresses
                .iter()
                .map(|address| Bytes::from_str(address).unwrap())
                .collect(),
        )
    }

    fn tracker_awaiting_whitelist() -> FreshnessTracker {
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        FreshnessTracker::new(
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
            .on_whitelist_read(read_ok(&[PAMM]))
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
        tracker.on_whitelist_read(read_ok(&[PAMM]));
        assert!(tracker
            .on_whitelist_read(WhitelistRead::Failed(FetchVenuesError::Call {
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
        tracker.on_whitelist_read(WhitelistRead::Failed(FetchVenuesError::Call {
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
        tracker.on_whitelist_read(read_ok(&[PAMM]));
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        // The venue gets de-whitelisted.
        let update = tracker
            .on_whitelist_read(read_ok(&[]))
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
        tracker.on_whitelist_read(read_ok(&[PAMM]));
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert!(tracker
            .on_whitelist_read(read_ok(&[PAMM]))
            .is_none());
    }

    #[test]
    fn whitelist_read_gauges_the_venue_count() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            recorded::{gauge_value, snapshot_map},
            WHITELISTED_VENUES,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let mut tracker = tracker_awaiting_whitelist();
            tracker.on_whitelist_read(read_ok(&[PAMM]));
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, WHITELISTED_VENUES, &[]), 1.0);
    }

    #[test]
    fn unregistered_venues_are_counted_without_labels_and_logged_boundedly() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            recorded::{counter_value, snapshot_map},
            UNREGISTERED_PAMM_ENTRIES,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let logged = metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = FreshnessTracker::new(
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
            tracker.logged_unregistered.len()
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_ENTRIES, &[]), 140);
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
