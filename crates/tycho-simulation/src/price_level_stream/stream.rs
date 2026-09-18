use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    time::Duration,
};

use async_stream::stream;
use num_bigint::BigUint;
use tokio_stream::{Stream, StreamExt};
use tycho_common::{models::token::Token, Bytes};

use super::{
    config::{
        default_denied_pamms, default_served_pamms, PriceLevelStreamConfig,
        DEFAULT_AUTO_DETECTED_GAS_COST,
    },
    fallback_router::{
        fetch_fallback_router_venues, whitelist_reader, WhitelistRead, WHITELIST_READ_TIMEOUT,
    },
    telemetry::{self, ServingState},
    titan::{self, ConnectionSettings, TITAN_PRICE_LEVEL_URL},
    tracker::{FreshnessTracker, Now, DEFAULT_STALE_AFTER},
};
use crate::protocol::models::Update;

/// Static attribute under which each emitted component carries its pAMM venue address.
pub const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// How often the PropAMMRouter whitelist is re-read by default. It is governance-gated and
/// changes rarely; ten minutes bounds how long a de-whitelisted venue keeps its old family.
pub(super) const DEFAULT_WHITELIST_REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// Builds a stream of [`Update`]s from the Titan pAMM price level WebSocket.
///
/// A new builder serves no pAMMs: register the known venues via
/// [`with_known_pamms`](Self::with_known_pamms), individual ones via
/// [`add_pamm`](Self::add_pamm), or opt into serving unknown streamed venues via
/// [`auto_detect`](Self::auto_detect); [`with_tokens`](Self::with_tokens) provides the token
/// metadata pairs are interpreted with.
///
/// One component is emitted per (pAMM, token pair), identified by the concatenation
/// `pamm ++ token0 ++ token1` (tokens sorted ascending), under the protocol system
/// `pricelevelstream:{pamm}` — or `propammfallback:{pamm}` for venues on the PropAMMRouter
/// whitelist, unless [`without_fallback_router`](Self::without_fallback_router) turns that off.
/// The venue address is exposed through the [`PAMM_ADDRESS_ATTRIBUTE`] static attribute for
/// downstream encoding.
pub struct PriceLevelStreamBuilder {
    registry: HashMap<Bytes, PriceLevelStreamConfig>,
    denied: HashSet<Bytes>,
    tokens: HashMap<Bytes, Token>,
    url: Option<String>,
    auto_detect: bool,
    auto_detected_gas_cost: Option<BigUint>,
    connection: ConnectionSettings,
    /// Whether [`build`](Self::build) reads the PropAMMRouter whitelist and serves the venues on
    /// it under the `propammfallback:` family.
    fallback_router: bool,
    /// See [`stale_after`](Self::stale_after).
    stale_after: Duration,
    /// See [`whitelist_refresh_interval`](Self::whitelist_refresh_interval).
    whitelist_refresh_interval: Duration,
    /// See [`fallback_router_rpc_url`](Self::fallback_router_rpc_url).
    fallback_router_rpc_url: Option<String>,
    /// Whether `RPC_URL` from the environment (or `.env`) is consulted when no explicit
    /// fallback router URL is set. Only tests turn this off, to pin down the no-URL path
    /// without touching process-global state.
    env_rpc_url: bool,
}

impl Default for PriceLevelStreamBuilder {
    fn default() -> Self {
        Self {
            registry: HashMap::new(),
            denied: HashSet::new(),
            tokens: HashMap::new(),
            url: None,
            auto_detect: false,
            auto_detected_gas_cost: None,
            connection: ConnectionSettings::default(),
            fallback_router: true,
            stale_after: DEFAULT_STALE_AFTER,
            whitelist_refresh_interval: DEFAULT_WHITELIST_REFRESH_INTERVAL,
            fallback_router_rpc_url: None,
            env_rpc_url: true,
        }
    }
}

impl PriceLevelStreamBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables serving pAMMs that are not registered via
    /// [`with_known_pamms`](Self::with_known_pamms) or [`add_pamm`](Self::add_pamm)
    /// (disabled by default).
    ///
    /// When enabled, any unknown streamed venue — except denied ones (see
    /// [`deny_pamm`](Self::deny_pamm)) — is served under its full lowercase hex address
    /// as the name, with the default gas cost. A venue's protocol system therefore changes from
    /// the address form (`pricelevelstream:{0xaddress}`) to a name (`pricelevelstream:{name}`)
    /// once it gets registered — via [`add_pamm`](Self::add_pamm) or a release's
    /// [`default_served_pamms`] recognizing it; the name-independent identifiers — the component id
    /// and the [`PAMM_ADDRESS_ATTRIBUTE`] — stay stable across such renames.
    pub fn auto_detect(mut self, enabled: bool) -> Self {
        self.auto_detect = enabled;
        self
    }

    /// Overrides the per-swap gas cost that auto-detected pAMMs (see
    /// [`auto_detect`](Self::auto_detect)) are served with. Defaults to the maximum over the
    /// known venue profiles, as the conservative choice. Registered venues are unaffected —
    /// their gas cost comes from their [`PriceLevelStreamConfig`].
    pub fn auto_detected_gas_cost(mut self, gas_cost: BigUint) -> Self {
        self.auto_detected_gas_cost = Some(gas_cost);
        self
    }

    /// Overrides the stream endpoint, e.g. to connect to a closer Titan region than the default
    /// (see <https://docs.titanbuilder.xyz/propamms/takers>).
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Overrides how long a single connection attempt may take before it is aborted and retried
    /// (default: 10s), so a hung TCP/TLS handshake cannot block the stream forever.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connection.connect_timeout = timeout;
        self
    }

    /// Overrides the longest gap between parsed Titan frames tolerated before the connection is
    /// treated as dead and re-established (default: 10s). Titan pushes one frame per second and
    /// sends no keepalives, so a multi-second silence means a stalled or half-open connection.
    /// Control frames and unparsable text do not count as liveness.
    pub fn read_idle_timeout(mut self, timeout: Duration) -> Self {
        self.connection.read_idle_timeout = timeout;
        self
    }

    /// Overrides the cap on the exponential reconnect backoff of `2^attempt` seconds
    /// (default: 32s).
    pub fn max_backoff(mut self, max_backoff: Duration) -> Self {
        self.connection.max_backoff = max_backoff;
        self
    }

    /// Registers a pAMM to be served under the given configuration, overriding any default,
    /// denied, or auto-detected one for the same address.
    ///
    /// Between [`add_pamm`](Self::add_pamm) and [`deny_pamm`](Self::deny_pamm) for the same
    /// address, the later call wins; the defaults applied by
    /// [`with_known_pamms`](Self::with_known_pamms) never override either, in any call order.
    pub fn add_pamm(mut self, config: PriceLevelStreamConfig) -> Self {
        self.denied.remove(&config.address);
        self.registry
            .insert(config.address.clone(), config);
        self
    }

    /// Excludes a venue from being served: drops its current registration (default or explicit)
    /// and blocks auto-detecting it.
    ///
    /// Between [`add_pamm`](Self::add_pamm) and [`deny_pamm`](Self::deny_pamm) for the same
    /// address, the later call wins; the defaults applied by
    /// [`with_known_pamms`](Self::with_known_pamms) never override either, in any call order —
    /// so denying a venue from the default set works whether the denial comes before or after
    /// [`with_known_pamms`](Self::with_known_pamms).
    pub fn deny_pamm(mut self, address: Bytes) -> Self {
        self.registry.remove(&address);
        self.denied.insert(address);
        self
    }

    /// Applies what is known about the streamed venues: registers the known-good ones
    /// ([`default_served_pamms`]) to be served and denies the known-bad ones
    /// ([`default_denied_pamms`]) — venues that stream quotes but whose swaps are not executable.
    ///
    /// These defaults never override an explicit [`add_pamm`](Self::add_pamm) or
    /// [`deny_pamm`](Self::deny_pamm) for the same address, regardless of call order.
    pub fn with_known_pamms(mut self) -> Self {
        for config in default_served_pamms() {
            if self.denied.contains(&config.address) {
                continue;
            }
            self.registry
                .entry(config.address.clone())
                .or_insert(config);
        }
        for address in default_denied_pamms() {
            if self.registry.contains_key(&address) {
                continue;
            }
            self.denied.insert(address);
        }
        self
    }

    /// Provides the token metadata used to build components and interpret amounts. Pairs whose
    /// tokens are missing here are skipped.
    pub fn with_tokens(mut self, tokens: HashMap<Bytes, Token>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Keeps every venue on the direct `pricelevelstream:{name}` path, so swaps execute on the
    /// venues themselves and a stale maker quote reverts the route.
    ///
    /// By default [`build`](Self::build) emits venues on Titan's PropAMMRouter whitelist under
    /// `propammfallback:{name}` instead, so tycho-execution routes their swaps through the
    /// router. Opt out when the direct call is what you want to measure or execute, or to skip
    /// the whitelist read at startup.
    pub fn without_fallback_router(mut self) -> Self {
        self.fallback_router = false;
        self
    }

    /// Overrides how long the data of one frame may be served without a fresher frame carrying
    /// it (default: 24s, two block times). A component no accepted frame has carried for this
    /// long is emitted in `removed_pairs`; the next accepted frame carrying it re-adds it in
    /// `new_pairs`. Frames whose data is this old or older are rejected outright. Independent
    /// of this setting, a state refuses to quote once its frame is one block time old.
    pub fn stale_after(mut self, duration: Duration) -> Self {
        self.stale_after = duration;
        self
    }

    /// Overrides how often the PropAMMRouter whitelist is re-read (default: 10 minutes). A
    /// venue whose family changes is removed at once and re-added under the new family by the
    /// next frame carrying it.
    pub fn whitelist_refresh_interval(mut self, interval: Duration) -> Self {
        self.whitelist_refresh_interval = interval;
        self
    }

    /// Sets the node URL the PropAMMRouter whitelist is read from, taking precedence over
    /// `RPC_URL` from the environment or `.env`. Consumers that already hold a node URL as
    /// configuration should pass it here rather than depend on process environment for an
    /// execution-affecting decision.
    pub fn fallback_router_rpc_url(mut self, url: impl Into<String>) -> Self {
        self.fallback_router_rpc_url = Some(url.into());
        self
    }

    #[cfg(test)]
    fn without_env_rpc_url(mut self) -> Self {
        self.env_rpc_url = false;
        self
    }

    /// Consumes the builder and opens the stream.
    ///
    /// The connection is established lazily on first poll and maintained (with reconnects) for as
    /// long as the stream is polled; it never terminates on its own, and dropping the stream
    /// closes the connection and stops the whitelist reader.
    ///
    /// Every accepted frame yields an update with the states of the served pairs it carries,
    /// with `new_pairs` for pairs not currently served. Pairs the frame does not carry keep their
    /// previous state downstream. A component no accepted frame has carried for
    /// [`stale_after`](Self::stale_after) is emitted in `removed_pairs`, together with every
    /// other component expiring at that instant, and re-added by the next accepted frame
    /// carrying it. Frames that are too old, from the future, out of order, or whose block
    /// regresses or jumps implausibly are rejected without effect. Frames that contain no served
    /// pAMM produce no update. Pairs whose tokens are missing from the provided token metadata
    /// are skipped.
    ///
    /// With the fallback router enabled (the default), nothing is emitted until the
    /// PropAMMRouter whitelist has been read from the node at
    /// [`fallback_router_rpc_url`](Self::fallback_router_rpc_url) or `RPC_URL`; each read is
    /// bounded by a timeout, retried with backoff, and refreshed periodically. Without a node URL
    /// an error is logged and nothing is ever served. See the [module documentation](super) for
    /// the full contract.
    pub fn build(self) -> impl Stream<Item = Update> + Send {
        let Self {
            registry,
            denied,
            tokens,
            url,
            auto_detect,
            auto_detected_gas_cost,
            connection,
            fallback_router,
            stale_after,
            whitelist_refresh_interval,
            fallback_router_rpc_url,
            env_rpc_url,
        } = self;
        if registry.is_empty() && !auto_detect {
            tracing::warn!(
                "No pAMMs registered and auto-detection is off; the stream will never produce \
                 an update"
            );
        }
        if tokens.is_empty() {
            tracing::warn!(
                "No token metadata provided; every streamed pair will be skipped and the stream \
                 will never produce an update"
            );
        }
        let url = url.unwrap_or_else(|| TITAN_PRICE_LEVEL_URL.to_string());
        let auto_detected_gas_cost =
            auto_detected_gas_cost.unwrap_or_else(|| BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));
        let rpc_url = match (fallback_router, fallback_router_rpc_url) {
            (false, Some(_)) => {
                tracing::debug!(
                    "fallback_router_rpc_url is set but the fallback router is disabled; \
                     ignoring it"
                );
                None
            }
            (false, None) => None,
            (true, Some(explicit)) => Some(explicit),
            (true, None) => env_rpc_url
                .then(rpc_url_from_env)
                .flatten(),
        };
        if fallback_router && rpc_url.is_none() {
            tracing::error!(
                "No node URL to read the PropAMMRouter whitelist from: set RPC_URL, call \
                 fallback_router_rpc_url, or opt out with without_fallback_router. No pAMM will \
                 be served"
            );
            telemetry::record_serving_state(ServingState::AwaitingWhitelist);
        }
        let mut tracker = FreshnessTracker::new(
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            stale_after,
            fallback_router,
        );
        let max_backoff = connection.max_backoff;

        stream! {
            let frames = titan::messages(url, connection);
            tokio::pin!(frames);
            let mut whitelist: Option<WhitelistReader> = rpc_url.map(|rpc_url| {
                let fetch = move || {
                    let rpc_url = rpc_url.clone();
                    async move { fetch_fallback_router_venues(&rpc_url).await }
                };
                Box::pin(whitelist_reader(
                    fetch,
                    WHITELIST_READ_TIMEOUT,
                    max_backoff,
                    whitelist_refresh_interval,
                )) as WhitelistReader
            });
            loop {
                let deadline = tracker.stale_deadline();
                let sleep_until_deadline = tokio::time::sleep_until(
                    deadline.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std),
                );
                let update = tokio::select! {
                    Some(frame) = frames.next() => tracker.on_frame(frame, Now::current()),
                    () = sleep_until_deadline, if deadline.is_some() => {
                        tracker.on_stale_deadline(Now::current())
                    }
                    read = next_whitelist_read(&mut whitelist) => tracker.on_whitelist_read(read),
                };
                if let Some(update) = update {
                    yield update;
                }
            }
        }
    }
}

type WhitelistReader = Pin<Box<dyn Stream<Item = WhitelistRead> + Send>>;

/// The next whitelist read, or a future that never resolves when the whitelist is not read at
/// all, so the `select!` arm simply never fires.
async fn next_whitelist_read(reader: &mut Option<WhitelistReader>) -> WhitelistRead {
    let Some(reader) = reader else {
        return std::future::pending().await;
    };
    match reader.next().await {
        Some(read) => read,
        None => std::future::pending().await,
    }
}

/// The node URL the whitelist is read from: `RPC_URL` from the environment, falling back to
/// `.env`.
fn rpc_url_from_env() -> Option<String> {
    std::env::var("RPC_URL")
        .ok()
        .or_else(|| {
            dotenv::dotenv().ok()?;
            std::env::var("RPC_URL").ok()
        })
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };

    use futures::SinkExt;
    use num_bigint::BigUint;
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        super::{
            config::{default_denied_pamms, PriceLevelStreamConfig},
            test_support::{frame_text, tokens, wall_nanos_now, FakeConnection, FakeTitan, PAMM},
            tracker::DEFAULT_STALE_AFTER,
        },
        *,
    };

    #[test]
    fn explicit_add_and_deny_are_last_wins() {
        let address = Bytes::from_str(PAMM).unwrap();
        let custom =
            || PriceLevelStreamConfig::new("custom", Bytes::from_str(PAMM).unwrap(), 1u64.into());

        let builder = PriceLevelStreamBuilder::new()
            .add_pamm(custom())
            .deny_pamm(address.clone());
        assert!(!builder.registry.contains_key(&address));
        assert!(builder.denied.contains(&address));

        let builder = PriceLevelStreamBuilder::new()
            .deny_pamm(address.clone())
            .add_pamm(custom());
        assert_eq!(builder.registry[&address].protocol, "custom");
        assert!(builder.denied.is_empty());
    }

    #[test]
    fn defaults_never_override_explicit_calls() {
        // Denying a venue from the default set works in either call order.
        let fermiswap_router = Bytes::from_str(PAMM).unwrap();
        for builder in [
            PriceLevelStreamBuilder::new()
                .deny_pamm(fermiswap_router.clone())
                .with_known_pamms(),
            PriceLevelStreamBuilder::new()
                .with_known_pamms()
                .deny_pamm(fermiswap_router.clone()),
        ] {
            assert!(!builder
                .registry
                .contains_key(&fermiswap_router));
            assert!(builder
                .denied
                .contains(&fermiswap_router));
            // The other defaults are unaffected.
            assert!(!builder.registry.is_empty());
        }

        // Registering a venue from the default deny set works in either call order.
        let denied_venue = default_denied_pamms().remove(0);
        let custom = || PriceLevelStreamConfig::new("custom", denied_venue.clone(), 1u64.into());
        for builder in [
            PriceLevelStreamBuilder::new()
                .add_pamm(custom())
                .with_known_pamms(),
            PriceLevelStreamBuilder::new()
                .with_known_pamms()
                .add_pamm(custom()),
        ] {
            assert_eq!(builder.registry[&denied_venue].protocol, "custom");
            assert!(!builder.denied.contains(&denied_venue));
        }
    }

    #[test]
    fn with_known_pamms_registers_known_venues() {
        // PAMM is the FermiSwap router, one of the default venues.
        let fermiswap_router = Bytes::from_str(PAMM).unwrap();

        let builder = PriceLevelStreamBuilder::new();
        assert!(builder.registry.is_empty());
        assert!(builder.denied.is_empty());

        let builder = builder.with_known_pamms();
        assert_eq!(builder.registry[&fermiswap_router].protocol, "fermiswap");
        // The known-bad venues get denied alongside, and never overlap the served defaults.
        assert!(!builder.denied.is_empty());
        assert!(builder.denied.is_disjoint(
            &builder
                .registry
                .keys()
                .cloned()
                .collect()
        ));

        // An `add_pamm` entry wins over the default for the same address, in either call order.
        let custom =
            || PriceLevelStreamConfig::new("custom", fermiswap_router.clone(), BigUint::from(1u64));
        for builder in [
            PriceLevelStreamBuilder::new()
                .add_pamm(custom())
                .with_known_pamms(),
            PriceLevelStreamBuilder::new()
                .with_known_pamms()
                .add_pamm(custom()),
        ] {
            assert_eq!(builder.registry[&fermiswap_router].protocol, "custom");
            assert_eq!(builder.registry[&fermiswap_router].gas_cost, BigUint::from(1u64));
        }
    }

    /// The PropAMMRouter path is the default; `without_fallback_router` is the way off it.
    #[test]
    fn fallback_router_is_on_unless_opted_out() {
        assert!(PriceLevelStreamBuilder::new().fallback_router);
        assert!(
            !PriceLevelStreamBuilder::new()
                .without_fallback_router()
                .fallback_router
        );
    }

    /// The families this stream emits are the ones tycho-execution resolves an encoder for. A
    /// drift between the two makes every route through a pAMM fail to encode.
    #[test]
    fn families_match_the_execution_side_prefixes() {
        use tycho_execution::encoding::evm::{PRICE_LEVEL_STREAM_PREFIX, PROPAMM_FALLBACK_PREFIX};

        use super::super::config::{PRICE_LEVEL_STREAM_FAMILY, PROPAMM_FALLBACK_FAMILY};

        assert_eq!(format!("{PRICE_LEVEL_STREAM_FAMILY}:"), PRICE_LEVEL_STREAM_PREFIX);
        assert_eq!(format!("{PROPAMM_FALLBACK_FAMILY}:"), PROPAMM_FALLBACK_PREFIX);
    }

    fn fermiswap() -> PriceLevelStreamConfig {
        PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        )
    }

    /// Short windows: 300 ms of data freshness, a 100 ms idle watchdog, 20 ms backoff cap.
    fn fast_builder(fake: &FakeTitan) -> PriceLevelStreamBuilder {
        PriceLevelStreamBuilder::new()
            .endpoint(fake.url())
            .without_fallback_router()
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .stale_after(Duration::from_millis(300))
            .connect_timeout(Duration::from_secs(1))
            .read_idle_timeout(Duration::from_millis(100))
            .max_backoff(Duration::from_millis(20))
    }

    async fn next_within(
        stream: &mut Pin<&mut impl Stream<Item = Update>>,
        limit: Duration,
    ) -> Option<Update> {
        tokio::time::timeout(limit, stream.next())
            .await
            .ok()
            .flatten()
    }

    fn assert_removal_only(update: &Update, expected_removed: usize) {
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert!(update.sync_states.is_empty());
        assert!(update.is_partial);
        assert_eq!(update.removed_pairs.len(), expected_removed);
    }

    /// Sends one fresh frame on the first connection only; later connections stay silent.
    async fn first_connection_sends_one_frame(index: usize, mut socket: FakeConnection) {
        if index == 0 {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
        }
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn silence_past_stale_after_removes_every_served_component() {
        let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        let first = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        assert_eq!(first.new_pairs.len(), 1);
        assert!(first.removed_pairs.is_empty());

        // Idle reconnects happen (silent later connections), but no frame refreshes anything.
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert_eq!(removal.block_number_or_timestamp, 100);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn repeated_immediate_closes_remove_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = socket.close(None).await;
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
    }

    #[tokio::test]
    async fn refused_reconnects_remove_within_stale_after() {
        let mut fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = socket.close(None).await;
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        // From here every connect is refused at TCP level.
        fake.shutdown();
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        // The fake closes 20 ms in and the backoff is capped at 20 ms, so under load one
        // reconnect can be accepted before `shutdown` drops the listener. What the removal
        // proves is that refused reconnects re-add nothing, not the exact connection count.
        assert!(fake.connections.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn replayed_frames_remove_within_stale_after_and_never_re_add() {
        // One frame, stamped once, replayed every 50 ms forever.
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let replay = frame_text(100, wall_nanos_now());
            loop {
                if socket
                    .send(Message::Text(replay.clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        // Replays are accepted while fresh but cannot extend the deadline; the deadline fires
        // even though a frame is ready on every poll.
        let removal = loop {
            let update = next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("stream stays alive");
            if !update.removed_pairs.is_empty() {
                break update;
            }
            assert!(update.new_pairs.is_empty());
        };
        assert_removal_only(&removal, 1);
        // Every later replay is too old to be accepted: nothing comes back.
        assert!(next_within(&mut stream, Duration::from_millis(500))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn ping_only_traffic_removes_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await;
            }
            loop {
                if socket
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);
        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "idle watchdog did not reconnect");
    }

    #[tokio::test]
    async fn malformed_text_removes_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await;
            }
            loop {
                if socket
                    .send(Message::Text("nonsense".into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);
        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "idle watchdog did not reconnect");
    }

    #[tokio::test]
    async fn fresh_frame_after_removal_re_adds_the_component() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = socket
                .send(Message::Text(frame_text(101, wall_nanos_now()).into()))
                .await;
            std::future::pending::<()>().await;
        })
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        let first = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        assert_eq!(first.new_pairs.len(), 1);
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        let re_added = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("re-add");
        assert_eq!(re_added.new_pairs.len(), 1);
        assert!(re_added.removed_pairs.is_empty());
        assert_eq!(re_added.block_number_or_timestamp, 101);
    }

    #[tokio::test]
    async fn frames_are_forwarded_without_waiting_on_timers() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            loop {
                if socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake)
            .stale_after(Duration::from_secs(24))
            .build();
        tokio::pin!(stream);
        next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let update = next_within(&mut stream, Duration::from_secs(1))
                .await
                .expect("steady-state frame");
            assert!(started.elapsed() < Duration::from_millis(200));
            assert!(update.removed_pairs.is_empty());
        }
    }

    #[tokio::test]
    async fn no_connection_before_first_poll_and_drop_closes_the_socket() {
        let server_saw_close = Arc::new(AtomicBool::new(false));
        let fake = {
            let server_saw_close = server_saw_close.clone();
            FakeTitan::spawn(move |_, mut socket| {
                let server_saw_close = server_saw_close.clone();
                async move {
                    let _ = socket
                        .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                        .await;
                    // Read until the client goes away.
                    while let Some(Ok(message)) = socket.next().await {
                        if matches!(message, Message::Close(_)) {
                            break;
                        }
                    }
                    server_saw_close.store(true, Ordering::SeqCst);
                }
            })
            .await
        };
        // `Box::pin` rather than `tokio::pin!`, so that `drop` below drops the stream itself
        // rather than a `Pin<&mut _>` pointing at a value that outlives the assertion.
        let mut stream = Box::pin(
            fast_builder(&fake)
                .read_idle_timeout(Duration::from_secs(5))
                .build(),
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(fake.connections.load(Ordering::SeqCst), 0, "connected before first poll");

        {
            let mut polled = stream.as_mut();
            next_within(&mut polled, Duration::from_secs(2))
                .await
                .expect("first snapshot");
        }
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);

        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(server_saw_close.load(Ordering::SeqCst), "socket not closed on drop");
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1, "reconnected after drop");
    }

    // `metrics::with_local_recorder` takes a sync closure, so this test drives its own
    // current-thread runtime instead of using `#[tokio::test]`.
    #[test]
    fn unreachable_whitelist_serves_nothing_while_frames_flow() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            recorded::{counter_value, snapshot_map},
            WHITELIST_READS,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let connections = metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let fake = FakeTitan::spawn(|_, mut socket| async move {
                    loop {
                        if socket
                            .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await;
                // Port 1 refuses connections, so every whitelist read fails fast.
                let stream = PriceLevelStreamBuilder::new()
                    .endpoint(fake.url())
                    .fallback_router_rpc_url("http://127.0.0.1:1")
                    .add_pamm(fermiswap())
                    .with_tokens(tokens())
                    .connect_timeout(Duration::from_secs(1))
                    .max_backoff(Duration::from_millis(20))
                    .build();
                tokio::pin!(stream);
                assert!(next_within(&mut stream, Duration::from_millis(700))
                    .await
                    .is_none());
                fake.connections.load(Ordering::SeqCst)
            })
        });
        assert!(connections >= 1, "frames were not consumed");
        // Without this the test would also pass if the reads had succeeded and the stream were
        // silent for some other reason.
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert!(
            counter_value(&snapshot, WHITELIST_READS, &[("outcome", "error")]) >= 1,
            "no whitelist read failed"
        );
    }

    // `metrics::with_local_recorder` takes a sync closure, so this test drives its own
    // current-thread runtime instead of using `#[tokio::test]`.
    #[test]
    fn missing_node_url_serves_nothing_and_reports_awaiting_whitelist() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            recorded::{gauge_value, snapshot_map},
            SERVING_STATE,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
                let stream = PriceLevelStreamBuilder::new()
                    .endpoint(fake.url())
                    .without_env_rpc_url()
                    .add_pamm(fermiswap())
                    .with_tokens(tokens())
                    .connect_timeout(Duration::from_secs(1))
                    .build();
                tokio::pin!(stream);
                assert!(next_within(&mut stream, Duration::from_millis(500))
                    .await
                    .is_none());
            });
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 0.0);
    }

    #[test]
    fn knob_defaults() {
        let builder = PriceLevelStreamBuilder::new();
        assert_eq!(builder.stale_after, DEFAULT_STALE_AFTER);
        assert_eq!(builder.whitelist_refresh_interval, DEFAULT_WHITELIST_REFRESH_INTERVAL);
        assert!(builder
            .fallback_router_rpc_url
            .is_none());
        assert!(builder.env_rpc_url);
    }
}
