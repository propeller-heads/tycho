use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use num_bigint::BigUint;
use tokio_stream::{Stream, StreamExt};
use tycho_common::{models::token::Token, Bytes};

use super::{
    config::{
        canonical_pamm_address, default_denied_pamms, default_served_pamms, PriceLevelStreamConfig,
        DEFAULT_AUTO_DETECTED_GAS_COST,
    },
    titan::{
        self, ConnectionSettings, TitanQuoteStreamMessage, TITAN_PRICE_LEVEL_URL,
        TITAN_PRICE_LEVEL_URL_ENV, TITAN_QUOTE_STREAM_URL, TITAN_QUOTE_STREAM_URL_ENV,
    },
    tracker::{FreshnessTracker, Now, TrackerSettings, DEFAULT_STALE_AFTER},
};
use crate::protocol::models::Update;

/// Static attribute under which each emitted component carries its pAMM venue address.
pub const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// The longest [`stale_after`](PriceLevelStreamBuilder::stale_after) the builder accepts;
/// longer values are capped to it. Quotes target the block being built, so serving quotes for
/// longer than this is never intended, and deadlines this far ahead stay representable on the
/// monotonic clock.
pub const MAX_STALE_AFTER: Duration = Duration::from_secs(3600);

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
/// `fallback:{pamm}` — or `pricelevelstream:{pamm}` after
/// [`without_fallback_router`](Self::without_fallback_router). The venue address is exposed
/// through the [`PAMM_ADDRESS_ATTRIBUTE`] static attribute for downstream encoding.
pub struct PriceLevelStreamBuilder {
    /// The venues to serve, keyed by canonical public identity.
    registry: HashMap<Bytes, PriceLevelStreamConfig>,
    /// The venues not to serve, by canonical public identity.
    denied: HashSet<Bytes>,
    tokens: HashMap<Bytes, Token>,
    url: Option<String>,
    /// See [`quote_stream_endpoint`](Self::quote_stream_endpoint).
    quote_stream_url: Option<String>,
    /// See [`without_simulation_overrides`](Self::without_simulation_overrides).
    simulation_overrides: bool,
    auto_detect: bool,
    auto_detected_gas_cost: Option<BigUint>,
    connection: ConnectionSettings,
    /// Whether components are emitted under the `fallback:` family, executed through
    /// `TychoFallbackRouter`, instead of the direct `pricelevelstream:` family.
    fallback_router: bool,
    /// See [`stale_after`](Self::stale_after).
    stale_after: Duration,
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
            quote_stream_url: None,
            simulation_overrides: true,
            auto_detect: false,
            auto_detected_gas_cost: None,
            connection: ConnectionSettings::default(),
            fallback_router: true,
            stale_after: DEFAULT_STALE_AFTER,
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
    /// (see <https://docs.titanbuilder.xyz/propamms/takers>). Without it, the
    /// `TITAN_PAMM_PRICE_LEVEL_URL` environment variable is used when set, else the built-in
    /// default.
    ///
    /// This sets the price level socket only; the quote stream socket has its own
    /// [`quote_stream_endpoint`](Self::quote_stream_endpoint) and its own default. To change
    /// region, set both: a region's quote stream matches that region's price levels, and nobody
    /// has tested whether it matches another region's. `build` logs a WARN when only one of the
    /// two is customized.
    pub fn endpoint(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Sets the endpoint of the quote stream socket, which carries the account state Titan priced
    /// the books on. Point it at the same Titan region as [`endpoint`](Self::endpoint). Without
    /// it, the stream uses the `TITAN_PAMM_QUOTE_STREAM_URL` environment variable when set, else
    /// the built-in default. [`without_simulation_overrides`](Self::without_simulation_overrides)
    /// wins in either call order: with it, this setting has no effect.
    pub fn quote_stream_endpoint(mut self, url: impl Into<String>) -> Self {
        self.quote_stream_url = Some(url.into());
        self
    }

    /// Emits states without simulation overrides, and connects no quote stream socket.
    ///
    /// By default the stream also reads Titan's quote stream, which publishes the account state
    /// Titan simulated each venue on to price its books, and attaches it to the matching states
    /// (see [`simulation_overrides`](super::state::PriceLevelStreamState::simulation_overrides)).
    /// A consumer that never simulates a swap with `eth_call` does not need it; opting out spares
    /// the second connection and the memory the held entries use. Quoting is the same either way:
    /// overrides never change a quote.
    pub fn without_simulation_overrides(mut self) -> Self {
        self.simulation_overrides = false;
        self
    }

    /// Overrides how long a single connection attempt may take before it is aborted and retried
    /// (default: 10s), so a hung TCP/TLS handshake cannot block the stream forever. Applies to
    /// both Titan sockets, which are connected and maintained independently.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connection.connect_timeout = timeout;
        self
    }

    /// Overrides the longest gap between parsed Titan messages tolerated before the connection is
    /// treated as dead and re-established (default: 10s). Titan pushes one message per second and
    /// sends no keepalives, so a multi-second silence means a stalled or half-open connection.
    /// Control frames and unparsable text do not reset this timeout, and neither does the time
    /// the consumer spends between polls: the gap is measured while the stream waits on the
    /// socket. Applies to both Titan sockets, each measured on its own traffic.
    pub fn read_idle_timeout(mut self, timeout: Duration) -> Self {
        self.connection.read_idle_timeout = timeout;
        self
    }

    /// Overrides the cap on the exponential reconnect backoff of `2^attempt` seconds
    /// (default: 32s). Applies to both Titan sockets, each backing off on its own failures.
    pub fn max_backoff(mut self, max_backoff: Duration) -> Self {
        self.connection.max_backoff = max_backoff;
        self
    }

    /// Registers a pAMM to be served under the given configuration, overriding any default,
    /// denied, or auto-detected one for the same venue.
    ///
    /// The configured address may be either of a venue's addresses — the one Titan streams its
    /// books under or its canonical public identity (see
    /// [`PAMM_ADDRESS_ALIASES`](super::config::PAMM_ADDRESS_ALIASES)) — because it is normalized
    /// to the public identity, which is what the emitted components then carry. Between
    /// [`add_pamm`](Self::add_pamm) and [`deny_pamm`](Self::deny_pamm) for the same venue, the
    /// later call wins, whichever of its addresses each named; the defaults applied by
    /// [`with_known_pamms`](Self::with_known_pamms) never override either, in any call order.
    pub fn add_pamm(mut self, config: PriceLevelStreamConfig) -> Self {
        let config =
            PriceLevelStreamConfig { address: canonical_pamm_address(&config.address), ..config };
        self.denied.remove(&config.address);
        self.registry
            .insert(config.address.clone(), config);
        self
    }

    /// Excludes a venue from being served: drops its current registration (default or explicit)
    /// and blocks auto-detecting it.
    ///
    /// As for [`add_pamm`](Self::add_pamm), `address` may be either of the venue's addresses, so
    /// denying a venue by its public identity also denies the books it streams under another
    /// one. Between [`add_pamm`](Self::add_pamm) and [`deny_pamm`](Self::deny_pamm) for the same
    /// venue, the later call wins; the defaults applied by
    /// [`with_known_pamms`](Self::with_known_pamms) never override either, in any call order —
    /// so denying a venue from the default set works whether the denial comes before or after
    /// [`with_known_pamms`](Self::with_known_pamms).
    pub fn deny_pamm(mut self, address: Bytes) -> Self {
        let address = canonical_pamm_address(&address);
        self.registry.remove(&address);
        self.denied.insert(address);
        self
    }

    /// Applies what is known about the streamed venues: registers the known-good ones
    /// ([`default_served_pamms`]) to be served and denies the known-bad ones
    /// ([`default_denied_pamms`]) — venues that stream quotes but whose swaps are not executable.
    ///
    /// These defaults never override an explicit [`add_pamm`](Self::add_pamm) or
    /// [`deny_pamm`](Self::deny_pamm) for the same venue, regardless of call order.
    pub fn with_known_pamms(mut self) -> Self {
        for config in default_served_pamms() {
            let config = PriceLevelStreamConfig {
                address: canonical_pamm_address(&config.address),
                ..config
            };
            if self.denied.contains(&config.address) {
                continue;
            }
            self.registry
                .entry(config.address.clone())
                .or_insert(config);
        }
        for address in default_denied_pamms() {
            let address = canonical_pamm_address(&address);
            if self.registry.contains_key(&address) {
                continue;
            }
            self.denied.insert(address);
        }
        self
    }

    /// Provides the token metadata used to build components and interpret amounts, and the set of
    /// assets the stream may price at all: a book is served only if both addresses it names are
    /// present here, and those tokens supply the decimals its price levels are converted with.
    /// Books naming any other asset are skipped.
    pub fn with_tokens(mut self, tokens: HashMap<Bytes, Token>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Keeps every venue on the direct `pricelevelstream:{name}` path, so swaps execute on the
    /// venues themselves and a stale maker quote reverts the route.
    ///
    /// By default components are emitted under `fallback:{name}`, so tycho-execution routes
    /// their swaps through `TychoFallbackRouter`. Opt out when the direct call is what you want
    /// to measure or execute.
    pub fn without_fallback_router(mut self) -> Self {
        self.fallback_router = false;
        self
    }

    /// Overrides how long a component stays served after the last accepted message that carried
    /// it (default: 24s, two slots). A component no accepted message has carried for this long
    /// turns stale and is emitted in `removed_pairs`; the next accepted message carrying it
    /// re-adds it in `new_pairs`. Messages whose `ts` is this old or older are rejected.
    /// Independent of this setting, a state refuses to quote once its message is one slot old
    /// (see [`without_quote_guard`](Self::without_quote_guard)).
    ///
    /// Values above [`MAX_STALE_AFTER`] are capped to it. A value shorter than the age messages
    /// arrive with rejects every message as `too_old`, which the
    /// `price_level_stream_frames_rejected_total` counter and a WARN log show.
    pub fn stale_after(mut self, duration: Duration) -> Self {
        self.stale_after = duration.min(MAX_STALE_AFTER);
        self
    }

    /// Emits states that never refuse to quote.
    ///
    /// By default every emitted state refuses `spot_price`, `get_amount_out` and `get_limits`
    /// once its message is one slot ([`QUOTE_TTL`](super::state::QUOTE_TTL)) old: Titan prices
    /// the block being built, so after one slot the quotes were priced for a block already built
    /// and may no longer match what the venue fills. Opt out only for a consumer that quotes a
    /// state more than one slot after it arrived by design — a batch simulator, a validation
    /// harness — and that accepts a quote the venue may no longer fill. The component still
    /// turns stale and is removed after [`stale_after`](Self::stale_after), which then becomes
    /// the only bound on how old served quotes can be. Never disable it on a live router.
    pub fn without_quote_guard(mut self) -> Self {
        self.quote_guard = false;
        self
    }

    /// Consumes the builder and opens the stream.
    ///
    /// Components are emitted under `fallback:{name}`, so tycho-execution routes their swaps
    /// through `TychoFallbackRouter`, which retries a reverted pAMM swap — a stale maker quote
    /// reverts in any simulation against a mined block — on the fallback pool the solver names.
    /// [`without_fallback_router`](Self::without_fallback_router) keeps them on the direct
    /// `pricelevelstream:` path.
    ///
    /// The stream connects the price level socket, and the quote stream socket unless
    /// [`without_simulation_overrides`](Self::without_simulation_overrides) turned it off, on the
    /// first poll. It reconnects both for as long as it is polled; neither ends on its own, and
    /// dropping the stream closes both. A quote stream message produces no update of its own: the
    /// tracker holds its entries until a later price level message's books name them. A quote
    /// stream that is refused, silent or malformed costs the states their overrides and nothing
    /// else: the stream never waits on the quote stream socket to serve a price level message or
    /// a stale removal.
    ///
    /// Every accepted message yields an update with the states of the served pairs it carries,
    /// with `new_pairs` for pairs not currently served. The update does not mention pairs the
    /// message does not carry, so consumers keep their previous state. A component no accepted
    /// message has carried for [`stale_after`](Self::stale_after) turns stale and is emitted in
    /// `removed_pairs`, together with every other component turning stale at that instant, and
    /// re-added by the next accepted message carrying it. Messages that are too old, from the
    /// future, out of order, or whose block regresses or jumps more than one block per elapsed
    /// slot plus 2 are rejected without effect. Messages that contain no served pAMM produce no
    /// update. Books naming an address absent from the provided token metadata are skipped, as is
    /// any one book side whose levels do not state a usable price function — leaving that
    /// direction unquotable.
    ///
    /// Every update carries the block its message targets, and a removal carries the newest
    /// accepted block. Block numbers never decrease while something is served. Once every
    /// component has turned stale, the next accepted message is judged as a first message and may
    /// carry a lower block than the removal did: that is how the stream recovers from a message
    /// with an implausible block, so consumers must not rely on the block number to order
    /// updates across such a gap. See the [module documentation](super) for the full contract.
    pub fn build(self) -> impl Stream<Item = Update> + Send {
        let Self {
            registry,
            denied,
            tokens,
            url,
            quote_stream_url,
            simulation_overrides,
            auto_detect,
            auto_detected_gas_cost,
            connection,
            fallback_router,
            stale_after,
            quote_guard,
        } = self;
        if registry.is_empty() && !auto_detect {
            tracing::warn!(
                "No pAMMs registered and auto-detection is off; the stream will never produce \
                 an update"
            );
        }
        if tokens.is_empty() {
            tracing::warn!(
                "No token metadata provided; every streamed book will be skipped and the stream \
                 will never produce an update"
            );
        }
        let url = url.unwrap_or_else(|| {
            std::env::var(TITAN_PRICE_LEVEL_URL_ENV)
                .unwrap_or_else(|_| TITAN_PRICE_LEVEL_URL.to_string())
        });
        let quote_stream_url = simulation_overrides.then(|| {
            quote_stream_url.unwrap_or_else(|| {
                std::env::var(TITAN_QUOTE_STREAM_URL_ENV)
                    .unwrap_or_else(|_| TITAN_QUOTE_STREAM_URL.to_string())
            })
        });
        if let Some(quote_stream_url) = &quote_stream_url {
            if (url == TITAN_PRICE_LEVEL_URL) != (quote_stream_url == TITAN_QUOTE_STREAM_URL) {
                tracing::warn!(
                    price_level_url = url,
                    quote_stream_url,
                    "Only one of the two Titan endpoints is customized; the quote stream of one \
                     region may not match the price levels of another, which costs states their \
                     simulation overrides"
                );
            }
        }
        let auto_detected_gas_cost =
            auto_detected_gas_cost.unwrap_or_else(|| BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));
        let mut tracker = FreshnessTracker::new(TrackerSettings {
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            stale_after,
            via_fallback_router: fallback_router,
            quote_guard,
            simulation_overrides,
        });

        stream! {
            let messages = titan::messages(url, connection);
            tokio::pin!(messages);
            // A stream that never yields stands in for the quote stream socket when it is off.
            let mut quote_stream_messages: Pin<
                Box<dyn Stream<Item = TitanQuoteStreamMessage> + Send>,
            > = match quote_stream_url {
                Some(url) => {
                    Box::pin(titan::messages::<TitanQuoteStreamMessage>(url, connection))
                }
                None => Box::pin(tokio_stream::pending()),
            };
            loop {
                let deadline = tracker.stale_deadline();
                let sleep_until_deadline = tokio::time::sleep_until(
                    deadline.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std),
                );
                let update = tokio::select! {
                    Some(message) = messages.next() => {
                        tracker.on_price_level_message(message, now())
                    }
                    Some(message) = quote_stream_messages.next() => {
                        tracker.on_quote_stream_message(message);
                        None
                    }
                    () = sleep_until_deadline, if deadline.is_some() => {
                        tracker.on_stale_deadline(timer_now())
                    }
                };
                if let Some(update) = update {
                    yield update;
                }
            }
        }
    }
}

/// The clocks a message is judged against: the wall clock for Titan's `ts`, and the
/// timer's clock for every deadline. This is the only place the stream reads a clock; the
/// tracker only ever sees the values it is handed.
fn now() -> Now {
    Now { wall_nanos: wall_clock_nanos(), monotonic: timer_now() }
}

/// Nanoseconds since the Unix epoch. A system clock unrepresentable as unix nanoseconds yields
/// 0, which rejects every message as `in_future`.
fn wall_clock_nanos() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since_epoch| u64::try_from(since_epoch.as_nanos()).ok());
    match since_epoch {
        Some(wall_nanos) => wall_nanos,
        None => {
            tracing::error!(
                "System clock unrepresentable as unix nanoseconds; every price level message will \
                 be rejected as in_future"
            );
            0
        }
    }
}

/// The clock the deadline timer runs on. Deadlines are set and swept with it so that a sweep
/// fired by the timer finds the component due, also under `tokio::time::pause()`, where the
/// runtime's virtual time and `std::time::Instant` diverge.
fn timer_now() -> Instant {
    tokio::time::Instant::now().into_std()
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        str::FromStr,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use futures::{future::BoxFuture, SinkExt};
    use num_bigint::BigUint;
    use rstest::rstest;
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        super::{
            config::{default_denied_pamms, PriceLevelStreamConfig},
            state::PriceLevelStreamState,
            telemetry::{
                recorded::{counter_value, record_async},
                RECONNECTS,
            },
            test_support::{
                address, captured_message_text, captured_quote_stream_text, captured_tokens,
                fermiswap, matching_message_text, message_text, message_then_repeat, token,
                token_map, tokens, wall_nanos_now, FakeConnection, FakeTitan, KIPSELI,
                KIPSELI_STREAMED, PAMM, USDC, WBTC, WETH,
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
    fn quote_guard_is_on_unless_opted_out() {
        assert!(PriceLevelStreamBuilder::new().quote_guard);
        assert!(
            !PriceLevelStreamBuilder::new()
                .without_quote_guard()
                .quote_guard
        );
    }

    /// Short timings for a test: `stale_after` 2 s, `read_idle_timeout` 100 ms, `max_backoff`
    /// 20 ms, reading price level messages from `fake`.
    ///
    /// It leaves the quote stream socket at its default, Titan's live endpoint, so no test builds
    /// on it directly: tests use [`fast_timings`], which turns that socket off, or
    /// [`two_socket_timings`], which points it at a second fake.
    fn timings(fake: &FakeTitan) -> PriceLevelStreamBuilder {
        PriceLevelStreamBuilder::new()
            .endpoint(fake.url())
            .stale_after(STALE_AFTER)
            .connect_timeout(Duration::from_secs(1))
            .read_idle_timeout(Duration::from_millis(100))
            .max_backoff(Duration::from_millis(20))
    }

    /// [`timings`] with no quote stream socket, for everything that socket has no part in.
    fn fast_timings(fake: &FakeTitan) -> PriceLevelStreamBuilder {
        timings(fake).without_simulation_overrides()
    }

    /// [`timings`] with the quote stream socket pointed at a second fake: the default shape of the
    /// stream, with both endpoints local.
    fn two_socket_timings(prices: &FakeTitan, overrides: &FakeTitan) -> PriceLevelStreamBuilder {
        timings(prices).quote_stream_endpoint(overrides.url())
    }

    /// A [`fast_timings`] builder serving the FermiSwap venue with the bound assets' metadata.
    fn fast_builder(fake: &FakeTitan) -> PriceLevelStreamBuilder {
        fast_timings(fake)
            .add_pamm(fermiswap())
            .with_tokens(tokens())
    }

    /// The `stale_after` of [`fast_builder`]: long enough that a loaded runner does not reject
    /// the first message as too old, short enough that a removal arrives within [`WAIT`].
    const STALE_AFTER: Duration = Duration::from_secs(2);

    /// How long a test waits for one update.
    const WAIT: Duration = Duration::from_secs(5);

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

    fn fresh_message() -> Message {
        Message::Text(message_text(100, wall_nanos_now()).into())
    }

    /// Sends one fresh message on the first connection only; later connections stay silent.
    async fn first_connection_sends_one_message(index: usize, mut socket: FakeConnection) {
        if index == 0 {
            let _ = socket.send(fresh_message()).await;
        }
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn silence_past_stale_after_removes_every_served_component() {
        let fake = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        // The stream reconnects on idle timeout, but the later connections send no message, so no
        // component deadline moves.
        let removal = next_within(&mut stream, WAIT)
            .await
            .expect("removal");

        assert_removal_only(&removal, 1);
        assert_eq!(removal.block_number, 100);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on idle timeout");
    }

    #[tokio::test]
    async fn repeated_immediate_closes_remove_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket.send(fresh_message()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = socket.close(None).await;
        })
        .await;
        let stream = fast_builder(&fake).build();
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
                let _ = socket.send(fresh_message()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = socket.close(None).await;
            })
            .await;
            let stream = fast_builder(&fake).build();
            tokio::pin!(stream);

            expect_first_update(&mut stream).await;
            // From here every connect is refused at TCP level. The fake closes 20 ms in and the
            // backoff is capped at 20 ms, so one reconnect can be accepted before `shutdown`
            // drops the listener; its fresh message refreshes the component without re-adding it.
            fake.shutdown();
            expect_removal(&mut stream).await
        });

        assert_removal_only(&removal, 1);
        assert!(
            counter_value(
                &snapshot,
                RECONNECTS,
                &[("reason", "connect_failed"), ("socket", "price_levels")]
            ) >= 1,
            "no connect was refused"
        );
    }

    #[tokio::test]
    async fn replayed_messages_remove_within_stale_after_and_never_re_add() {
        // One message, with one `ts`, replayed every 50 ms forever.
        let replay = fresh_message();
        let fake = FakeTitan::spawn(message_then_repeat(
            replay.clone(),
            replay,
            Duration::from_millis(50),
        ))
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        // Replays are accepted while fresh but cannot extend the deadline; the deadline fires
        // even though a message is ready on every poll.
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        // Every later replay is too old to be accepted: nothing comes back.
        assert!(next_within(&mut stream, Duration::from_millis(500))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn fresh_message_after_removal_re_adds_the_component() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket.send(fresh_message()).await;
            tokio::time::sleep(STALE_AFTER + Duration::from_millis(500)).await;
            let _ = socket
                .send(Message::Text(message_text(101, wall_nanos_now()).into()))
                .await;
            std::future::pending::<()>().await;
        })
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
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
        assert_eq!(re_added.block_number, 101);
    }

    #[tokio::test]
    async fn messages_are_forwarded_without_waiting_on_timers() {
        let fake = FakeTitan::spawn(fresh_message_every(Duration::from_millis(50))).await;
        let stream = fast_builder(&fake)
            .stale_after(Duration::from_secs(24))
            .build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        for _ in 0..5 {
            let update = next_within(&mut stream, Duration::from_secs(1))
                .await
                .expect("steady-state message");
            assert!(update.removed_pairs.is_empty());
        }
    }

    #[tokio::test]
    async fn no_connection_before_first_poll() {
        let fake = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = fast_builder(&fake).build();
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
                    let _ = socket.send(fresh_message()).await;
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
        expect_first_update(&mut stream.as_mut()).await;

        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(server_saw_close.load(Ordering::SeqCst), "socket not closed on drop");
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1, "reconnected after drop");
    }

    /// `Instant + Duration` panics on overflow, so an absurd `stale_after` must be capped before
    /// the first accepted message computes a deadline from it.
    #[tokio::test]
    async fn stale_after_above_the_cap_still_serves() {
        assert_eq!(
            PriceLevelStreamBuilder::new()
                .stale_after(Duration::MAX)
                .stale_after,
            MAX_STALE_AFTER
        );
        let fake = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = fast_builder(&fake)
            .stale_after(Duration::MAX)
            .build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
    }

    #[tokio::test]
    async fn without_quote_guard_emits_states_that_never_expire() {
        let fake = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = fast_builder(&fake)
            .without_quote_guard()
            .build();
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

    /// A [`FakeTitan`] handler that sends a message with a current `ts` every `interval` until the
    /// socket closes.
    fn fresh_message_every(
        interval: Duration,
    ) -> impl Fn(usize, FakeConnection) -> BoxFuture<'static, ()> + Send + Sync + 'static {
        move |_, mut socket| {
            Box::pin(async move {
                while socket
                    .send(fresh_message())
                    .await
                    .is_ok()
                {
                    tokio::time::sleep(interval).await;
                }
            })
        }
    }

    /// The fallback router path is the default; `without_fallback_router` is the way off it.
    #[test]
    fn fallback_router_is_on_unless_opted_out() {
        assert!(PriceLevelStreamBuilder::new().fallback_router);
        assert!(
            !PriceLevelStreamBuilder::new()
                .without_fallback_router()
                .fallback_router
        );
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

        // Registering a venue from the default deny set works in either call order. Any one of
        // them exercises that; the set is empty while every streamed venue is executable.
        let Some(denied_venue) = default_denied_pamms().pop() else { return };
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
        assert_eq!(
            builder.denied,
            default_denied_pamms()
                .into_iter()
                .collect()
        );
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

    /// The families this stream emits are the ones tycho-execution resolves an encoder for. A
    /// drift between the two makes every route through a pAMM fail to encode.
    #[test]
    fn families_match_the_execution_side_prefixes() {
        use tycho_execution::encoding::evm::{FALLBACK_PREFIX, PRICE_LEVEL_STREAM_PREFIX};

        use super::super::config::{FALLBACK_FAMILY, PRICE_LEVEL_STREAM_FAMILY};

        assert_eq!(format!("{PRICE_LEVEL_STREAM_FAMILY}:"), PRICE_LEVEL_STREAM_PREFIX);
        assert_eq!(format!("{FALLBACK_FAMILY}:"), FALLBACK_PREFIX);
    }

    /// A registration may name either of a venue's addresses: the streamed one normalizes to the
    /// public identity, which the registry is keyed by and the configuration then carries, so the
    /// components of an aliased venue execute against the public identity either way.
    #[rstest]
    #[case::by_public_identity(KIPSELI)]
    #[case::by_streamed_address(KIPSELI_STREAMED)]
    fn registering_a_venue_registers_its_public_identity(#[case] registered: &str) {
        let public = address(KIPSELI);

        let builder = PriceLevelStreamBuilder::new().add_pamm(PriceLevelStreamConfig::new(
            "custom",
            address(registered),
            1u64.into(),
        ));

        assert_eq!(builder.registry[&public].protocol, "custom");
        assert_eq!(builder.registry[&public].address, public);
        assert_eq!(builder.registry.len(), 1);
    }

    /// Denial is on the public identity too, so denying a venue by the address its swaps go to
    /// also drops the default registration of the books it streams under another address.
    #[rstest]
    #[case::by_public_identity(KIPSELI)]
    #[case::by_streamed_address(KIPSELI_STREAMED)]
    fn denying_a_venue_denies_its_public_identity(#[case] denied: &str) {
        let public = address(KIPSELI);

        let builder = PriceLevelStreamBuilder::new()
            .with_known_pamms()
            .deny_pamm(address(denied));

        assert!(!builder.registry.contains_key(&public));
        assert!(!builder
            .registry
            .contains_key(&address(KIPSELI_STREAMED)));
        assert!(builder.denied.contains(&public));
        // The other default venues are unaffected.
        assert!(builder
            .registry
            .contains_key(&address(PAMM)));
    }

    /// Mixed calls naming different addresses of one venue land on one canonical key, so the
    /// later call still wins — as it does when both name the same address.
    #[rstest]
    #[case::add_public_then_deny_streamed(KIPSELI, KIPSELI_STREAMED)]
    #[case::add_streamed_then_deny_public(KIPSELI_STREAMED, KIPSELI)]
    fn mixed_address_forms_are_last_wins(#[case] added: &str, #[case] denied: &str) {
        let public = address(KIPSELI);
        let custom = || PriceLevelStreamConfig::new("custom", address(added), 1u64.into());

        let builder = PriceLevelStreamBuilder::new()
            .add_pamm(custom())
            .deny_pamm(address(denied));
        assert!(builder.registry.is_empty());
        assert!(builder.denied.contains(&public));

        let builder = PriceLevelStreamBuilder::new()
            .deny_pamm(address(denied))
            .add_pamm(custom());
        assert_eq!(builder.registry[&public].protocol, "custom");
        assert!(builder.denied.is_empty());
    }

    /// Synthetic: a token that calls itself `WBTC` at an address no default binding names.
    const IMPOSTOR_WBTC: &str = "0x00000000000000000000000000000000deadbeef";

    /// Sends one message with a current `ts`, of Kipseli's captured books, on the first connection
    /// only.
    /// The books arrive under the address Titan streams them by, so serving them at all also
    /// exercises the alias on the live path.
    async fn first_connection_sends_captured_books(index: usize, mut socket: FakeConnection) {
        if index == 0 {
            let message = captured_message_text("Kipseli", 100, wall_nanos_now());
            let _ = socket
                .send(Message::Text(message.into()))
                .await;
        }
        std::future::pending::<()>().await;
    }

    /// The address Titan streams is the identity, so an impostor token calling itself `WBTC` is
    /// never priced and never blocks the real asset. Kipseli quoted WBTC/USDC and WETH/USDC in
    /// the capture, so the WETH/USDC component must survive every case.
    ///
    /// Every case admits USDC and WETH plus the `WBTC`-named tokens of its first parameter, as
    /// `(address, decimals)`; the second is the address expected to be priced for the book's
    /// WBTC leg, or `None` when the book stays unserved.
    #[rstest]
    #[case::canonical_asset(vec![(WBTC, 8)], Some(WBTC))]
    #[case::impostor_only(vec![(IMPOSTOR_WBTC, 18)], None)]
    #[case::both_present(vec![(WBTC, 8), (IMPOSTOR_WBTC, 18)], Some(WBTC))]
    #[tokio::test]
    async fn an_impostor_symbol_is_never_priced(
        #[case] bitcoins: Vec<(&str, u32)>,
        #[case] expected_btc: Option<&str>,
    ) {
        let admitted = [token(USDC, "USDC", 6), token(WETH, "WETH", 18)]
            .into_iter()
            .chain(
                bitcoins
                    .into_iter()
                    .map(|(address, decimals)| token(address, "WBTC", decimals)),
            );
        let fake = FakeTitan::spawn(first_connection_sends_captured_books).await;
        let stream = fast_timings(&fake)
            .add_pamm(PriceLevelStreamConfig::new("kipseli", address(KIPSELI), 1u64.into()))
            .with_tokens(token_map(admitted))
            .build();
        tokio::pin!(stream);

        let update = next_within(&mut stream, WAIT)
            .await
            .expect("first update");

        // Every served component, as the addresses of its token pair in component order.
        let mut served: Vec<Vec<String>> = update
            .new_pairs
            .values()
            .map(|component| {
                component
                    .tokens
                    .iter()
                    .map(|token| token.address.to_string())
                    .collect()
            })
            .collect();
        served.sort();
        let mut expected = vec![vec![USDC.to_string(), WETH.to_string()]];
        if let Some(btc) = expected_btc {
            let mut pair = vec![btc.to_string(), USDC.to_string()];
            pair.sort();
            expected.push(pair);
        }
        expected.sort();
        assert_eq!(served, expected);
        // Whichever assets were priced, the venue executes against its public identity.
        for component in update.new_pairs.values() {
            assert_eq!(component.static_attributes[PAMM_ADDRESS_ATTRIBUTE], address(KIPSELI));
        }
    }

    /// Traffic that is not a parsed message keeps neither the socket nor the component alive: the
    /// idle timeout reconnects, every reconnect resends the same message, which refreshes the
    /// component but cannot move its deadline, and the removal follows.
    #[rstest]
    #[case::ping_only(Message::Ping(Vec::new().into()))]
    #[case::malformed_text(Message::Text("nonsense".into()))]
    #[tokio::test]
    async fn traffic_that_is_not_a_message_removes_within_stale_after(#[case] filler: Message) {
        let fake = FakeTitan::spawn(message_then_repeat(
            fresh_message(),
            filler,
            Duration::from_millis(10),
        ))
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on idle timeout");
    }

    /// How long a two-socket test waits for the quote stream socket to have been read before it
    /// sends the price level message whose books name its entries. Both sockets connect on the
    /// first poll, so this orders the two messages rather than racing them.
    const SOCKET_HEAD_START: Duration = Duration::from_millis(250);

    /// A builder serving the known venues, which are every venue of the 2026-10-16 captures,
    /// reading both captures from `prices` and `overrides`. Registered up front rather than
    /// auto-detected, because entries are only stored for venues already served when they arrive.
    fn capture_builder(prices: &FakeTitan, overrides: &FakeTitan) -> PriceLevelStreamBuilder {
        two_socket_timings(prices, overrides)
            .with_known_pamms()
            .with_tokens(captured_tokens())
    }

    /// The price level capture whose books match the captured quote stream message, with its `ts`
    /// set to now so that it passes the freshness checks.
    fn matching_message() -> Message {
        Message::Text(matching_message_text(100, wall_nanos_now()).into())
    }

    /// The captured quote stream message, verbatim: matching reads no clock, so its timestamps can
    /// stay as captured.
    fn quote_stream_message() -> Message {
        Message::Text(captured_quote_stream_text().into())
    }

    /// A [`FakeTitan`] handler that sends `message` after `delay` on the first connection only,
    /// then holds the socket open: a message the stream must see exactly once, at a known point
    /// relative to the other socket's.
    fn send_once_after(
        delay: Duration,
        message: Message,
    ) -> impl Fn(usize, FakeConnection) -> BoxFuture<'static, ()> + Send + Sync + 'static {
        move |index, mut socket| {
            let message = message.clone();
            Box::pin(async move {
                if index == 0 {
                    tokio::time::sleep(delay).await;
                    let _ = socket.send(message).await;
                }
                std::future::pending::<()>().await;
            })
        }
    }

    /// How many connections `fake` has accepted.
    fn connections(fake: &FakeTitan) -> usize {
        fake.connections.load(Ordering::SeqCst)
    }

    /// The states of `update` that carry simulation overrides.
    fn states_with_overrides(update: &Update) -> Vec<&PriceLevelStreamState> {
        update
            .states
            .values()
            .map(|state| {
                state
                    .as_any()
                    .downcast_ref::<PriceLevelStreamState>()
                    .expect("price level state")
            })
            .filter(|state| state.simulation_overrides().is_some())
            .collect()
    }

    /// Both captures, read from two local sockets with the quote stream message arriving first:
    /// the 27 served pairs whose venue's entry carries the pair in the book's slot come out with
    /// overrides, and the other 3 without. The count comes from the fixtures themselves, worked
    /// out independently of this code.
    #[tokio::test]
    async fn a_quote_stream_message_attaches_overrides_to_the_books_it_priced() {
        let overrides =
            FakeTitan::spawn(send_once_after(Duration::ZERO, quote_stream_message())).await;
        let prices = FakeTitan::spawn(send_once_after(SOCKET_HEAD_START, matching_message())).await;
        let stream = capture_builder(&prices, &overrides)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        let update = next_within(&mut stream, WAIT)
            .await
            .expect("first update");

        assert_eq!(update.states.len(), 30);
        let with_overrides = states_with_overrides(&update);
        assert_eq!(with_overrides.len(), 27);
        for state in with_overrides {
            let overrides = state
                .simulation_overrides()
                .expect("overrides");
            assert!(!overrides.accounts.is_empty(), "overrides with no account");
            assert_eq!(overrides.block_number, Some(26_201_954));
        }
    }

    /// An entry that arrives after the books it priced changes nothing already served: the update
    /// the consumer holds keeps the states it was given. The next message with the same books gets
    /// overrides, which shows the first one's lack of them is about ordering, not matching.
    #[tokio::test]
    async fn a_late_quote_stream_message_leaves_served_states_without_overrides() {
        let overrides =
            FakeTitan::spawn(send_once_after(SOCKET_HEAD_START, quote_stream_message())).await;
        let prices = FakeTitan::spawn(move |index, mut socket| async move {
            if index == 0 {
                let _ = socket.send(matching_message()).await;
                tokio::time::sleep(2 * SOCKET_HEAD_START).await;
                let _ = socket.send(matching_message()).await;
            }
            std::future::pending::<()>().await;
        })
        .await;
        let stream = capture_builder(&prices, &overrides)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        let first = next_within(&mut stream, WAIT).await;
        let second = next_within(&mut stream, WAIT).await;

        let first = first.expect("first update");
        assert_eq!(first.new_pairs.len(), 30);
        assert!(states_with_overrides(&first).is_empty(), "overrides before any entry arrived");
        assert_eq!(states_with_overrides(&second.expect("second update")).len(), 27);
    }

    /// Opting out connects no quote stream socket at all, so no state gets overrides and nothing
    /// is read from an endpoint the consumer never wanted, whichever of the opt-out and
    /// `quote_stream_endpoint` is called first.
    #[rstest]
    #[case::opt_out_after_the_endpoint(false)]
    #[case::opt_out_before_the_endpoint(true)]
    #[tokio::test]
    async fn without_simulation_overrides_connects_no_quote_stream_socket(
        #[case] opt_out_first: bool,
    ) {
        let overrides =
            FakeTitan::spawn(send_once_after(Duration::ZERO, quote_stream_message())).await;
        let prices = FakeTitan::spawn(send_once_after(SOCKET_HEAD_START, matching_message())).await;
        let builder = if opt_out_first {
            timings(&prices)
                .without_simulation_overrides()
                .quote_stream_endpoint(overrides.url())
        } else {
            timings(&prices)
                .quote_stream_endpoint(overrides.url())
                .without_simulation_overrides()
        };
        let stream = builder
            .with_known_pamms()
            .with_tokens(captured_tokens())
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        let update = next_within(&mut stream, WAIT)
            .await
            .expect("first update");

        assert_eq!(update.states.len(), 30);
        assert!(states_with_overrides(&update).is_empty());
        assert_eq!(connections(&overrides), 0, "connected the quote stream socket");
    }

    /// Whatever the quote stream socket does, the price level socket keeps serving: the states lose
    /// their overrides and nothing else. A silent socket reconnects on the idle timeout, malformed
    /// text is skipped and then reconnected for the same reason, a close is reconnected, and a
    /// flood is read without starving the other two arms of the select. In every case the first
    /// update arrives and the stale removal that follows the price level socket's silence still
    /// fires on time.
    ///
    /// `connects` is the least number of connections the quote stream socket must have made by
    /// then: 2 for the cases that test recovery, so a socket that gives up after one failure fails
    /// them; 1 for the flood, whose messages parse and keep the connection alive.
    #[rstest]
    #[case::silent(None, 2)]
    #[case::malformed(Some(Message::Text("nonsense".into())), 2)]
    #[case::closed(Some(Message::Close(None)), 2)]
    #[case::flooding(Some(quote_stream_message()), 1)]
    #[tokio::test]
    async fn the_price_level_socket_survives_any_quote_stream_socket(
        #[case] repeat: Option<Message>,
        #[case] connects: usize,
    ) {
        let overrides = FakeTitan::spawn(move |_, mut socket| {
            let repeat = repeat.clone();
            async move {
                while let Some(message) = repeat.clone() {
                    if socket.send(message).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                std::future::pending::<()>().await;
            }
        })
        .await;
        let prices = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = two_socket_timings(&prices, &overrides)
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        assert!(
            connections(&overrides) >= connects,
            "the quote stream socket connected {} times, expected at least {connects}",
            connections(&overrides)
        );
    }

    /// A refused quote stream socket does not stall the price level socket, which serves and
    /// expires on its own schedule.
    #[tokio::test]
    async fn a_refused_quote_stream_socket_does_not_stall_the_price_level_socket() {
        let mut overrides =
            FakeTitan::spawn(send_once_after(Duration::ZERO, quote_stream_message())).await;
        let url = overrides.url();
        overrides.shutdown();
        let prices = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = timings(&prices)
            .quote_stream_endpoint(url)
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
    }

    /// A quote stream socket whose handshake never completes does not stall the price level
    /// socket, which serves and expires on its own schedule. The quote stream socket's connect
    /// timeout fires and it is retried; without the timeout the stream would wait on the hung
    /// handshake for as long as the peer keeps the connection open.
    #[tokio::test]
    async fn a_hung_quote_stream_socket_does_not_stall_the_price_level_socket() {
        let overrides = FakeTitan::spawn_hung().await;
        let prices = FakeTitan::spawn(first_connection_sends_one_message).await;
        let stream = two_socket_timings(&prices, &overrides)
            .connect_timeout(Duration::from_millis(200))
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .build();
        tokio::pin!(stream);

        expect_first_update(&mut stream).await;
        let removal = expect_removal(&mut stream).await;

        assert_removal_only(&removal, 1);
        assert!(
            connections(&overrides) >= 2,
            "the hung socket was connected to {} times, expected a retry",
            connections(&overrides)
        );
    }

    /// Isolation holds the other way too: with the price level socket refused the stream yields
    /// nothing, and the quote stream socket is still read. The captured quote stream message names
    /// venues this stream does not serve, which it counts as it reads them.
    #[test]
    fn a_refused_price_level_socket_does_not_stall_the_quote_stream_socket() {
        let (update, snapshot) = record_async(async {
            let overrides = FakeTitan::spawn(message_then_repeat(
                quote_stream_message(),
                quote_stream_message(),
                Duration::from_millis(20),
            ))
            .await;
            let mut prices = FakeTitan::spawn(first_connection_sends_one_message).await;
            prices.shutdown();
            let stream = two_socket_timings(&prices, &overrides)
                .add_pamm(fermiswap())
                .with_tokens(tokens())
                .build();
            tokio::pin!(stream);
            next_within(&mut stream, Duration::from_millis(500)).await
        });

        assert!(update.is_none(), "an update without a price level message");
        assert!(
            counter_value(&snapshot, "price_level_stream_unserved_venue_entries_total", &[]) > 0,
            "the quote stream socket was never read"
        );
    }

    /// Dropping the stream closes both sockets, and neither is reconnected afterwards.
    #[tokio::test]
    async fn drop_closes_both_sockets() {
        let closed = Arc::new(AtomicUsize::new(0));
        let spawn_watching = |closed: Arc<AtomicUsize>, first: Message| async move {
            FakeTitan::spawn(move |_, mut socket| {
                let closed = closed.clone();
                let first = first.clone();
                async move {
                    let _ = socket.send(first).await;
                    while let Some(Ok(message)) = socket.next().await {
                        if matches!(message, Message::Close(_)) {
                            break;
                        }
                    }
                    closed.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        };
        let overrides = spawn_watching(closed.clone(), quote_stream_message()).await;
        let prices = spawn_watching(closed.clone(), matching_message()).await;
        let mut stream = Box::pin(
            capture_builder(&prices, &overrides)
                .read_idle_timeout(Duration::from_secs(5))
                .build(),
        );
        next_within(&mut stream.as_mut(), WAIT)
            .await
            .expect("first update");

        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(closed.load(Ordering::SeqCst), 2, "a socket was not closed on drop");
        assert_eq!(connections(&prices), 1, "reconnected after drop");
        assert_eq!(connections(&overrides), 1, "reconnected after drop");
    }
}
