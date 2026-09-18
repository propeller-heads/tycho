use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    time::{Duration, Instant},
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
        fetch_fallback_router_venues, whitelist_reader, WhitelistReaderSettings,
        WHITELIST_READ_TIMEOUT,
    },
    titan::{self, ConnectionSettings, TITAN_PRICE_LEVEL_URL},
    tracker::{FreshnessTracker, Now, TrackerSettings, Whitelist, DEFAULT_STALE_AFTER},
};
use crate::protocol::models::Update;

/// Static attribute under which each emitted component carries its pAMM venue address.
pub const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// How often the PropAMMRouter whitelist is re-read by default. It is governance-gated and
/// changes rarely; ten minutes bounds how long a de-whitelisted venue keeps its old family.
pub(super) const DEFAULT_WHITELIST_REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// The longest [`stale_after`](PriceLevelStreamBuilder::stale_after) a stream can be built
/// with. Quotes target the block being built, so serving a ladder for longer than this is never
/// intended, and deadlines that far ahead stay representable on the monotonic clock.
pub const MAX_STALE_AFTER: Duration = Duration::from_secs(3600);

/// Why [`PriceLevelStreamBuilder::build`] refused to open the stream.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PriceLevelStreamBuildError {
    /// The fallback router is on, so the PropAMMRouter whitelist must be read, but no node URL
    /// was set through
    /// [`fallback_router_rpc_url`](PriceLevelStreamBuilder::fallback_router_rpc_url) or
    /// `RPC_URL`.
    #[error(
        "no node URL to read the PropAMMRouter whitelist from: set RPC_URL, call \
         fallback_router_rpc_url, or opt out with without_fallback_router"
    )]
    MissingFallbackRouterRpcUrl,
    /// The node URL for the whitelist read does not parse.
    #[error("invalid node URL {url:?} for the PropAMMRouter whitelist: {reason}")]
    InvalidFallbackRouterRpcUrl {
        /// The URL that failed to parse.
        url: String,
        /// The parse error.
        reason: String,
    },
    /// [`stale_after`](PriceLevelStreamBuilder::stale_after) is zero or longer than
    /// [`MAX_STALE_AFTER`].
    #[error("stale_after must be longer than zero and at most {MAX_STALE_AFTER:?}, got {given:?}")]
    StaleAfterOutOfRange {
        /// The value the builder was given.
        given: Duration,
    },
}

/// Where [`PriceLevelStreamBuilder::build`] reads the PropAMMRouter whitelist from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WhitelistSource {
    /// Not read at all: every venue stays on the direct family.
    Disabled,
    /// The node at this URL.
    Url(String),
    /// The node at `RPC_URL` from the environment or `.env`.
    Env,
}

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
    /// See [`without_fallback_router`](Self::without_fallback_router) and
    /// [`fallback_router_rpc_url`](Self::fallback_router_rpc_url).
    whitelist_source: WhitelistSource,
    /// See [`stale_after`](Self::stale_after).
    stale_after: Duration,
    /// See [`whitelist_refresh_interval`](Self::whitelist_refresh_interval).
    whitelist_refresh_interval: Duration,
    /// See [`without_quote_guard`](Self::without_quote_guard).
    quote_guard: bool,
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
            whitelist_source: WhitelistSource::Env,
            stale_after: DEFAULT_STALE_AFTER,
            whitelist_refresh_interval: DEFAULT_WHITELIST_REFRESH_INTERVAL,
            quote_guard: true,
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
    /// Control frames and unparsable text do not reset this timeout, and neither does the time
    /// the consumer spends between polls: the gap is measured while the stream waits on the
    /// socket.
    pub fn read_idle_timeout(mut self, timeout: Duration) -> Self {
        self.connection.read_idle_timeout = timeout;
        self
    }

    /// Overrides the cap on the exponential backoff of `2^attempt` seconds (default: 32s) that
    /// spaces both Titan reconnects and retries of the PropAMMRouter whitelist read.
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
    /// the whitelist read at startup. Between this and
    /// [`fallback_router_rpc_url`](Self::fallback_router_rpc_url), the later call wins.
    pub fn without_fallback_router(mut self) -> Self {
        self.whitelist_source = WhitelistSource::Disabled;
        self
    }

    /// Overrides how long a component stays served after the last accepted frame that carried
    /// it (default: 24s, two slots). A component no accepted frame has carried for this long
    /// turns stale and is emitted in `removed_pairs`; the next accepted frame carrying it
    /// re-adds it in `new_pairs`. Frames whose `timestamp` is this old or older are rejected.
    /// Independent of this setting, a state refuses to quote once its frame is one slot old
    /// (see [`without_quote_guard`](Self::without_quote_guard)).
    ///
    /// Must be longer than zero and at most [`MAX_STALE_AFTER`]; [`build`](Self::build) fails
    /// otherwise.
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

    /// Sets the node URL the PropAMMRouter whitelist is read from, instead of `RPC_URL` from
    /// the environment or `.env`. Consumers that already hold a node URL should pass it here,
    /// so that the family their swaps execute under does not depend on the process environment.
    /// Between this and [`without_fallback_router`](Self::without_fallback_router), the later
    /// call wins.
    pub fn fallback_router_rpc_url(mut self, url: impl Into<String>) -> Self {
        self.whitelist_source = WhitelistSource::Url(url.into());
        self
    }

    /// Emits states that never refuse to quote.
    ///
    /// By default every emitted state refuses `spot_price`, `get_amount_out` and `get_limits`
    /// once its frame is one slot ([`QUOTE_TTL`](super::state::QUOTE_TTL)) old: Titan quotes
    /// the block being built, and a venue rejects a fill against an older ladder as stale, so
    /// such a quote is not executable. Opt out only for a consumer that quotes a state more
    /// than one slot after it arrived by design — a batch simulator, a validation harness — and
    /// that accepts a quote the venue may no longer fill. The component still turns stale and
    /// is removed after [`stale_after`](Self::stale_after), which then becomes the only bound
    /// on how old a quoted ladder can be. Never disable it on a live router.
    pub fn without_quote_guard(mut self) -> Self {
        self.quote_guard = false;
        self
    }

    /// Consumes the builder and opens the stream.
    ///
    /// The connection is established lazily on first poll and maintained (with reconnects) for as
    /// long as the stream is polled; it never terminates on its own, and dropping the stream
    /// closes the connection and stops the whitelist reader.
    ///
    /// Every accepted frame yields an update with the states of the served pairs it carries,
    /// with `new_pairs` for pairs not currently served. The update does not mention pairs the
    /// frame does not carry, so consumers keep their previous state. A component no accepted
    /// frame has carried for [`stale_after`](Self::stale_after) turns stale and is emitted in
    /// `removed_pairs`, together with every other component turning stale at that instant, and
    /// re-added by the next accepted frame carrying it. Frames that are too old, from the
    /// future, out of order, or whose block regresses or jumps more than one block per elapsed
    /// slot plus 2 are rejected without effect. Frames that contain no served pAMM produce no
    /// update. Pairs whose tokens are missing from the provided token metadata are skipped.
    ///
    /// With the fallback router enabled (the default), nothing is emitted until the
    /// PropAMMRouter whitelist has been read from the node at
    /// [`fallback_router_rpc_url`](Self::fallback_router_rpc_url) or `RPC_URL`; each read is
    /// bounded by a timeout, retried with backoff, and refreshed periodically. See the
    /// [module documentation](super) for the full contract.
    ///
    /// # Errors
    ///
    /// With the fallback router enabled, fails with
    /// [`MissingFallbackRouterRpcUrl`](PriceLevelStreamBuildError::MissingFallbackRouterRpcUrl)
    /// when no node URL is configured and with
    /// [`InvalidFallbackRouterRpcUrl`](PriceLevelStreamBuildError::InvalidFallbackRouterRpcUrl)
    /// when the configured one does not parse; a node that is reachable but does not answer is
    /// retried instead. Fails with
    /// [`StaleAfterOutOfRange`](PriceLevelStreamBuildError::StaleAfterOutOfRange) when
    /// [`stale_after`](Self::stale_after) is zero or longer than [`MAX_STALE_AFTER`].
    pub fn build(self) -> Result<impl Stream<Item = Update> + Send, PriceLevelStreamBuildError> {
        let whitelist = self.whitelist_reader(rpc_url_from_env)?;
        self.build_with_whitelist(whitelist)
    }

    /// The whitelist reader for the configured source, or a stream that never yields when the
    /// fallback router is off. `env_rpc_url` resolves `RPC_URL` when the source is the
    /// environment.
    fn whitelist_reader(
        &self,
        env_rpc_url: impl FnOnce() -> Option<String>,
    ) -> Result<WhitelistReader, PriceLevelStreamBuildError> {
        let rpc_url = match &self.whitelist_source {
            WhitelistSource::Disabled => return Ok(Box::pin(tokio_stream::pending())),
            WhitelistSource::Url(url) => url.clone(),
            WhitelistSource::Env => {
                env_rpc_url().ok_or(PriceLevelStreamBuildError::MissingFallbackRouterRpcUrl)?
            }
        };
        if let Err(e) = rpc_url.parse::<reqwest::Url>() {
            return Err(PriceLevelStreamBuildError::InvalidFallbackRouterRpcUrl {
                url: rpc_url,
                reason: e.to_string(),
            });
        }
        let fetch = move || {
            let rpc_url = rpc_url.clone();
            async move { fetch_fallback_router_venues(&rpc_url).await }
        };
        let settings = WhitelistReaderSettings {
            read_timeout: WHITELIST_READ_TIMEOUT,
            max_backoff: self.connection.max_backoff,
            refresh_interval: self.whitelist_refresh_interval,
        };
        Ok(Box::pin(whitelist_reader(fetch, settings)))
    }

    /// Opens the stream (see [`build`](Self::build)) with `whitelist` as the source of whitelist
    /// reads.
    fn build_with_whitelist(
        self,
        mut whitelist: WhitelistReader,
    ) -> Result<impl Stream<Item = Update> + Send, PriceLevelStreamBuildError> {
        let Self {
            registry,
            denied,
            tokens,
            url,
            auto_detect,
            auto_detected_gas_cost,
            connection,
            whitelist_source,
            stale_after,
            whitelist_refresh_interval: _,
            quote_guard,
        } = self;
        if stale_after.is_zero() || stale_after > MAX_STALE_AFTER {
            return Err(PriceLevelStreamBuildError::StaleAfterOutOfRange { given: stale_after });
        }
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
        let whitelist_mode = match whitelist_source {
            WhitelistSource::Disabled => Whitelist::NotUsed,
            WhitelistSource::Url(_) | WhitelistSource::Env => Whitelist::Awaited,
        };
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            stale_after,
            whitelist: whitelist_mode,
            quote_guard,
        });

        Ok(stream! {
            let frames = titan::messages(url, connection);
            tokio::pin!(frames);
            loop {
                let deadline = tracker.stale_deadline();
                let sleep_until_deadline = tokio::time::sleep_until(
                    deadline.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std),
                );
                let update = tokio::select! {
                    Some(frame) = frames.next() => tracker.on_frame(frame, Now::current()),
                    () = sleep_until_deadline, if deadline.is_some() => {
                        tracker.on_stale_deadline(Instant::now())
                    }
                    Some(venues) = whitelist.next() => tracker.on_whitelist_read(venues),
                };
                if let Some(update) = update {
                    yield update;
                }
            }
        })
    }
}

type WhitelistReader = Pin<Box<dyn Stream<Item = HashSet<Bytes>> + Send>>;

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

    use futures::{future::BoxFuture, SinkExt};
    use num_bigint::BigUint;
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        super::{
            config::{default_denied_pamms, PriceLevelStreamConfig},
            fallback_router::FetchVenuesError,
            state::PriceLevelStreamState,
            telemetry::{
                recorded::{counter_value, gauge_value, record_async},
                RECONNECTS, SERVING_STATE, WHITELIST_READS,
            },
            test_support::{
                fermiswap, frame_text, frame_then_repeat, tokens, wall_nanos_now, FakeConnection,
                FakeTitan, PAMM,
            },
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

    /// The PropAMMRouter path through `RPC_URL` is the default; `without_fallback_router` is
    /// the way off it and `fallback_router_rpc_url` names the node explicitly. The later call
    /// wins between the two.
    #[test]
    fn whitelist_source_follows_the_last_call() {
        assert_eq!(PriceLevelStreamBuilder::new().whitelist_source, WhitelistSource::Env);
        assert_eq!(
            PriceLevelStreamBuilder::new()
                .without_fallback_router()
                .whitelist_source,
            WhitelistSource::Disabled
        );
        assert_eq!(
            PriceLevelStreamBuilder::new()
                .without_fallback_router()
                .fallback_router_rpc_url("http://node")
                .whitelist_source,
            WhitelistSource::Url("http://node".to_string())
        );
        assert_eq!(
            PriceLevelStreamBuilder::new()
                .fallback_router_rpc_url("http://node")
                .without_fallback_router()
                .whitelist_source,
            WhitelistSource::Disabled
        );
    }

    #[test]
    fn quote_guard_is_on_unless_opted_out() {
        assert!(PriceLevelStreamBuilder::new().quote_guard);
        assert!(
            !PriceLevelStreamBuilder::new()
                .without_quote_guard()
                .quote_guard
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

    /// A builder with short timings: `stale_after` 1 s, `read_idle_timeout` 100 ms,
    /// `max_backoff` 20 ms.
    fn fast_builder(fake: &FakeTitan) -> PriceLevelStreamBuilder {
        PriceLevelStreamBuilder::new()
            .endpoint(fake.url())
            .without_fallback_router()
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .stale_after(STALE_AFTER)
            .connect_timeout(Duration::from_secs(1))
            .read_idle_timeout(Duration::from_millis(100))
            .max_backoff(Duration::from_millis(20))
    }

    /// The `stale_after` of [`fast_builder`]: long enough that a loaded runner does not reject
    /// the first frame as too old, short enough that a removal arrives within [`WAIT`].
    const STALE_AFTER: Duration = Duration::from_secs(1);

    /// How long a test waits for one update.
    const WAIT: Duration = Duration::from_secs(3);

    async fn next_within(
        stream: &mut Pin<&mut impl Stream<Item = Update>>,
        limit: Duration,
    ) -> Option<Update> {
        tokio::time::timeout(limit, stream.next())
            .await
            .ok()
            .flatten()
    }

    /// Waits for the first update and checks that it adds the one served component.
    async fn expect_first_update(stream: &mut Pin<&mut impl Stream<Item = Update>>) -> Update {
        let first = next_within(stream, WAIT)
            .await
            .expect("first update");
        assert_eq!(first.new_pairs.len(), 1);
        assert!(first.removed_pairs.is_empty());
        first
    }

    /// Waits up to [`WAIT`] for an update that removes components, skipping the updates that
    /// only refresh served ones; an update that re-adds a component before the removal fails.
    async fn expect_removal(stream: &mut Pin<&mut impl Stream<Item = Update>>) -> Update {
        tokio::time::timeout(WAIT, async {
            loop {
                let update = stream
                    .next()
                    .await
                    .expect("stream ended");
                if !update.removed_pairs.is_empty() {
                    return update;
                }
                assert!(update.new_pairs.is_empty(), "component re-added before the removal");
            }
        })
        .await
        .expect("removal")
    }

    fn assert_removal_only(update: &Update, expected_removed: usize) {
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert!(update.sync_states.is_empty());
        assert!(update.is_partial);
        assert_eq!(update.removed_pairs.len(), expected_removed);
    }

    fn fresh_frame() -> Message {
        Message::Text(frame_text(100, wall_nanos_now()).into())
    }

    /// Sends one fresh frame on the first connection only; later connections stay silent.
    async fn first_connection_sends_one_frame(index: usize, mut socket: FakeConnection) {
        if index == 0 {
            let _ = socket.send(fresh_frame()).await;
        }
        std::future::pending::<()>().await;
    }

    /// A [`FakeTitan`] handler that sends a freshly stamped frame every `interval` until the
    /// socket closes.
    fn fresh_frame_every(
        interval: Duration,
    ) -> impl Fn(usize, FakeConnection) -> BoxFuture<'static, ()> + Send + Sync + 'static {
        move |_, mut socket| {
            Box::pin(async move {
                loop {
                    if socket
                        .send(fresh_frame())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(interval).await;
                }
            })
        }
    }

    #[tokio::test]
    async fn silence_past_stale_after_removes_every_served_component() {
        let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
        let stream = fast_builder(&fake)
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        // The stream reconnects on idle timeout, but the later connections send no frame, so no
        // component deadline moves.
        let removal = next_within(&mut stream, WAIT)
            .await
            .expect("removal");

        assert_removal_only(&removal, 1);
        assert_eq!(removal.block_number_or_timestamp, 100);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on idle timeout");
    }

    #[tokio::test]
    async fn repeated_immediate_closes_remove_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket.send(fresh_frame()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = socket.close(None).await;
        })
        .await;
        let stream = fast_builder(&fake)
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        let removal = next_within(&mut stream, WAIT)
            .await
            .expect("removal");

        assert_removal_only(&removal, 1);
    }

    #[test]
    fn refused_reconnects_remove_within_stale_after() {
        let (removal, snapshot) = record_async(async {
            let mut fake = FakeTitan::spawn(|_, mut socket| async move {
                let _ = socket.send(fresh_frame()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = socket.close(None).await;
            })
            .await;
            let stream = fast_builder(&fake)
                .build()
                .expect("build");
            tokio::pin!(stream);

            expect_first_update(&mut stream).await;
            // From here every connect is refused at TCP level. The fake closes 20 ms in and the
            // backoff is capped at 20 ms, so one reconnect can be accepted before `shutdown`
            // drops the listener; its fresh frame refreshes the component without re-adding it.
            fake.shutdown();
            expect_removal(&mut stream).await
        });

        assert_removal_only(&removal, 1);
        assert!(
            counter_value(&snapshot, RECONNECTS, &[("reason", "connect_failed")]) >= 1,
            "no connect was refused"
        );
    }

    #[tokio::test]
    async fn replayed_frames_remove_within_stale_after_and_never_re_add() {
        // One frame, stamped once, replayed every 50 ms forever.
        let replay = fresh_frame();
        let fake =
            FakeTitan::spawn(frame_then_repeat(replay.clone(), replay, Duration::from_millis(50)))
                .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        // Replays are accepted while fresh but cannot extend the deadline; the deadline fires
        // even though a frame is ready on every poll.
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        // Every later replay is too old to be accepted: nothing comes back.
        assert!(next_within(&mut stream, Duration::from_millis(500))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn ping_only_traffic_removes_within_stale_after() {
        let fake = FakeTitan::spawn(frame_then_repeat(
            fresh_frame(),
            Message::Ping(Vec::new().into()),
            Duration::from_millis(10),
        ))
        .await;
        let stream = fast_builder(&fake)
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        // Every reconnect resends the same frame, which refreshes the component but cannot move
        // its deadline.
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on idle timeout");
    }

    #[tokio::test]
    async fn malformed_text_removes_within_stale_after() {
        let fake = FakeTitan::spawn(frame_then_repeat(
            fresh_frame(),
            Message::Text("nonsense".into()),
            Duration::from_millis(10),
        ))
        .await;
        let stream = fast_builder(&fake)
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        // Every reconnect resends the same frame, which refreshes the component but cannot move
        // its deadline.
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on idle timeout");
    }

    #[tokio::test]
    async fn fresh_frame_after_removal_re_adds_the_component() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket.send(fresh_frame()).await;
            tokio::time::sleep(STALE_AFTER + Duration::from_millis(500)).await;
            let _ = socket
                .send(Message::Text(frame_text(101, wall_nanos_now()).into()))
                .await;
            std::future::pending::<()>().await;
        })
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        let removal = next_within(&mut stream, WAIT)
            .await
            .expect("removal");
        let re_added = next_within(&mut stream, WAIT)
            .await
            .expect("re-add");

        assert_removal_only(&removal, 1);
        assert_eq!(re_added.new_pairs.len(), 1);
        assert!(re_added.removed_pairs.is_empty());
        assert_eq!(re_added.block_number_or_timestamp, 101);
    }

    #[tokio::test]
    async fn frames_are_forwarded_without_waiting_on_timers() {
        let fake = FakeTitan::spawn(fresh_frame_every(Duration::from_millis(50))).await;
        let stream = fast_builder(&fake)
            .stale_after(Duration::from_secs(24))
            .build()
            .expect("build");
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        for _ in 0..5 {
            let update = next_within(&mut stream, Duration::from_secs(1))
                .await
                .expect("steady-state frame");
            assert!(update.removed_pairs.is_empty());
        }
    }

    #[tokio::test]
    async fn no_connection_before_first_poll() {
        let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
        let stream = fast_builder(&fake)
            .build()
            .expect("build");
        tokio::pin!(stream);

        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(fake.connections.load(Ordering::SeqCst), 0, "connected before first poll");
    }

    #[tokio::test]
    async fn drop_closes_the_socket() {
        let server_saw_close = Arc::new(AtomicBool::new(false));
        let fake = {
            let server_saw_close = server_saw_close.clone();
            FakeTitan::spawn(move |_, mut socket| {
                let server_saw_close = server_saw_close.clone();
                async move {
                    let _ = socket.send(fresh_frame()).await;
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
                .build()
                .expect("build"),
        );
        expect_first_update(&mut stream.as_mut()).await;

        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(server_saw_close.load(Ordering::SeqCst), "socket not closed on drop");
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1, "reconnected after drop");
    }

    #[tokio::test]
    async fn successful_whitelist_read_serves_the_venue_under_propammfallback() {
        let fake = FakeTitan::spawn(fresh_frame_every(Duration::from_millis(50))).await;
        let fetch = || async { Ok::<_, FetchVenuesError>(vec![Bytes::from_str(PAMM).unwrap()]) };
        let settings = WhitelistReaderSettings {
            read_timeout: WHITELIST_READ_TIMEOUT,
            max_backoff: Duration::from_millis(20),
            refresh_interval: Duration::from_secs(60),
        };
        let reader = Box::pin(whitelist_reader(fetch, settings)) as WhitelistReader;
        let stream = PriceLevelStreamBuilder::new()
            .endpoint(fake.url())
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .connect_timeout(Duration::from_secs(1))
            .build_with_whitelist(reader)
            .expect("build");
        tokio::pin!(stream);

        let first = expect_first_update(&mut stream).await;

        let component = first
            .new_pairs
            .values()
            .next()
            .expect("one new pair");
        assert_eq!(component.protocol_system, "propammfallback:fermiswap");
    }

    #[test]
    fn unreachable_whitelist_serves_nothing_while_frames_flow() {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(fresh_frame_every(Duration::from_millis(50))).await;
            // Port 1 refuses connections, so every whitelist read fails fast.
            let stream = PriceLevelStreamBuilder::new()
                .endpoint(fake.url())
                .fallback_router_rpc_url("http://127.0.0.1:1")
                .add_pamm(fermiswap())
                .with_tokens(tokens())
                .connect_timeout(Duration::from_secs(1))
                .max_backoff(Duration::from_millis(20))
                .build()
                .expect("build");
            tokio::pin!(stream);
            assert!(next_within(&mut stream, Duration::from_millis(700))
                .await
                .is_none());
            fake.connections.load(Ordering::SeqCst)
        });

        assert!(connections >= 1, "frames were not consumed");
        // The counter check rules out a stream that is silent for another reason while the
        // reads succeed.
        assert!(
            counter_value(&snapshot, WHITELIST_READS, &[("outcome", "error")]) >= 1,
            "no whitelist read failed"
        );
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 0.0, "not awaiting the whitelist");
    }

    #[test]
    fn missing_node_url_fails_to_build() {
        let builder = PriceLevelStreamBuilder::new()
            .add_pamm(fermiswap())
            .with_tokens(tokens());
        match builder.whitelist_reader(|| None) {
            Err(PriceLevelStreamBuildError::MissingFallbackRouterRpcUrl) => {}
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("built a whitelist reader without a node URL"),
        }
    }

    #[test]
    fn invalid_node_url_fails_to_build() {
        let result = PriceLevelStreamBuilder::new()
            .fallback_router_rpc_url("not a url")
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .build();
        match result {
            Err(PriceLevelStreamBuildError::InvalidFallbackRouterRpcUrl { url, reason: _ }) => {
                assert_eq!(url, "not a url");
            }
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("built with an unparsable node URL"),
        }
    }

    #[test]
    fn stale_after_outside_its_range_fails_to_build() {
        for given in [Duration::ZERO, MAX_STALE_AFTER + Duration::from_secs(1), Duration::MAX] {
            let result = PriceLevelStreamBuilder::new()
                .without_fallback_router()
                .add_pamm(fermiswap())
                .with_tokens(tokens())
                .stale_after(given)
                .build();
            match result {
                Err(PriceLevelStreamBuildError::StaleAfterOutOfRange { given: reported }) => {
                    assert_eq!(reported, given);
                }
                Err(other) => panic!("unexpected error for {given:?}: {other}"),
                Ok(_) => panic!("built with stale_after {given:?}"),
            }
        }
        assert!(PriceLevelStreamBuilder::new()
            .without_fallback_router()
            .stale_after(MAX_STALE_AFTER)
            .build()
            .is_ok());
    }

    #[tokio::test]
    async fn without_quote_guard_emits_states_that_never_expire() {
        let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
        let stream = fast_builder(&fake)
            .without_quote_guard()
            .build()
            .expect("build");
        tokio::pin!(stream);

        let first = expect_first_update(&mut stream).await;

        let state = first
            .states
            .values()
            .next()
            .expect("one state")
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert!(state.quotable_until().is_none());
    }
}
