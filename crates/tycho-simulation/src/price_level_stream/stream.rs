use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    time::Duration,
};

use chrono::Utc;
use num_bigint::BigUint;
use tokio_stream::{Stream, StreamExt};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use super::{
    config::{
        default_denied_pamms, default_served_pamms, PriceLevelStreamConfig,
        DEFAULT_AUTO_DETECTED_GAS_COST,
    },
    state::{PriceLevelStreamQuote, PriceLevelStreamState},
    titan::{
        self, ConnectionSettings, TitanPairLevels, TitanPammLevels, TitanPriceLevel,
        TitanPriceLevelMessage, TITAN_PRICE_LEVEL_URL,
    },
};
use crate::protocol::models::{ProtocolComponent, Update};

/// Static attribute under which each emitted component carries its pAMM venue address.
pub const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

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
/// `pricelevelstream:{pamm}`. The venue address is exposed through the
/// [`PAMM_ADDRESS_ATTRIBUTE`] static attribute for downstream encoding.
#[derive(Default)]
pub struct PriceLevelStreamBuilder {
    registry: HashMap<Bytes, PriceLevelStreamConfig>,
    denied: HashSet<Bytes>,
    tokens: HashMap<Bytes, Token>,
    url: Option<String>,
    auto_detect: bool,
    auto_detected_gas_cost: Option<BigUint>,
    connection: ConnectionSettings,
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

    /// Overrides the longest gap between Titan messages tolerated before the connection is
    /// treated as dead and re-established (default: 30s). Titan pushes several updates per
    /// second, so a multi-second silence means a stalled or half-open connection.
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

    /// Consumes the builder and opens the stream.
    ///
    /// The connection is established lazily on first poll and maintained (with reconnects) for as
    /// long as the stream is polled; it never terminates on its own, and dropping the stream
    /// closes the connection. Frames that contain no served pAMM produce no update.
    ///
    /// Each streamed frame is a complete snapshot of everything Titan currently streams, so
    /// every update carries the full set of the frame's pair states, with `new_pairs` /
    /// `removed_pairs` derived by diffing against the previous frame — a pair (or a whole
    /// venue) the stream stops serving is removed. Frames older than an already processed one
    /// are skipped, so updates never move backwards in block number. Pairs whose tokens are
    /// missing from the provided token metadata are skipped.
    pub fn build(self) -> impl Stream<Item = Update> + Send {
        if self.registry.is_empty() && !self.auto_detect {
            tracing::warn!(
                "No pAMMs registered and auto-detection is off; the stream will never produce \
                 an update"
            );
        }
        if self.tokens.is_empty() {
            tracing::warn!(
                "No token metadata provided; every streamed pair will be skipped and the stream \
                 will never produce an update"
            );
        }
        let url = self
            .url
            .unwrap_or_else(|| TITAN_PRICE_LEVEL_URL.to_string());
        let auto_detected_gas_cost = self
            .auto_detected_gas_cost
            .unwrap_or_else(|| BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));
        let mut tracker = SnapshotTracker::new(
            self.registry,
            self.denied,
            self.tokens,
            self.auto_detect,
            auto_detected_gas_cost,
        );

        titan::messages(url, self.connection).filter_map(move |message| tracker.process(message))
    }
}

/// Turns Titan frames into [`Update`]s, tracking the previously emitted components so pair
/// additions and removals can be diffed against the last snapshot.
struct SnapshotTracker {
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
    /// Components of the last emitted snapshot, across all pAMMs. A frame is a complete
    /// snapshot of everything Titan currently streams, so removals are diffed globally: a
    /// known component a frame does not re-emit is gone — including when its venue vanishes
    /// from the stream entirely.
    components: HashMap<String, ProtocolComponent>,
    /// The newest block number processed so far. Frames targeting an older block (e.g.
    /// delivered around a reconnect) are stale and skipped wholesale — processing one would
    /// emit superseded states and churn the global diff.
    newest_block: u64,
}

impl SnapshotTracker {
    fn new(
        registry: HashMap<Bytes, PriceLevelStreamConfig>,
        denied: HashSet<Bytes>,
        tokens: HashMap<Bytes, Token>,
        auto_detect: bool,
        auto_detected_gas_cost: BigUint,
    ) -> Self {
        Self {
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            components: HashMap::new(),
            newest_block: 0,
        }
    }

    /// Processes one frame into an [`Update`], or `None` if the frame targets an older block
    /// than an already processed one or contains nothing relevant (no registered pAMM with at
    /// least one known pair or a pair removal).
    fn process(&mut self, message: TitanPriceLevelMessage) -> Option<Update> {
        if message.block_number < self.newest_block {
            tracing::warn!(
                block_number = message.block_number,
                newest_block = self.newest_block,
                "Skipping out-of-order price level frame"
            );
            return None;
        }
        self.newest_block = message.block_number;

        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();
        // The frame is a complete snapshot: every known component is presumed gone until the
        // frame re-emits it below.
        let mut previous = std::mem::take(&mut self.components);

        for TitanPammLevels { pamm, pairs } in message.pamms {
            let config = match self.registry.entry(pamm.clone()) {
                Entry::Occupied(entry) => &*entry.into_mut(),
                Entry::Vacant(entry) => {
                    if !self.auto_detect {
                        tracing::debug!(%pamm, "Skipping unregistered pAMM");
                        continue;
                    }
                    if self.denied.contains(&pamm) {
                        tracing::debug!(%pamm, "Skipping denied pAMM");
                        continue;
                    }
                    tracing::info!(%pamm, "Serving auto-detected pAMM");
                    &*entry.insert(PriceLevelStreamConfig::auto_detected(
                        pamm.clone(),
                        self.auto_detected_gas_cost.clone(),
                    ))
                }
            };

            // Merge the frame's per-direction ladders into one entry per unordered token pair.
            let mut merged_pairs: HashMap<(Bytes, Bytes), (Vec<_>, Vec<_>)> = HashMap::new();
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
                let entry = merged_pairs.entry(key).or_default();
                if sells_token0 {
                    entry.0 = quotes;
                } else {
                    entry.1 = quotes;
                }
            }

            for ((token0, token1), (quotes_0_to_1, quotes_1_to_0)) in merged_pairs {
                let id = component_id(&config.address, &token0, &token1);
                let id_string = id.to_string();
                let component = previous
                    .remove(&id_string)
                    .unwrap_or_else(|| {
                        let component = build_component(&self.tokens, config, id, &token0, &token1);
                        new_pairs.insert(id_string.clone(), component.clone());
                        component
                    });

                let state = PriceLevelStreamState::new(
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    config.gas_cost.clone(),
                );

                states.insert(id_string.clone(), Box::new(state));
                self.components
                    .insert(id_string, component);
            }
        }

        // Every re-emitted pair was moved back into `self.components` above — whatever remains
        // is gone: the pair, or its whole venue, is no longer streamed.
        let removed_pairs = previous;

        if states.is_empty() && new_pairs.is_empty() && removed_pairs.is_empty() {
            return None;
        }

        Some(
            // Quotes target the block currently being built, hence partial. Sync states stay
            // empty (like the RFQ path) because no full block header is available.
            Update::new(message.block_number, states, new_pairs)
                .set_is_partial(true)
                .set_removed_pairs(removed_pairs),
        )
    }
}

fn build_component(
    tokens: &HashMap<Bytes, Token>,
    config: &PriceLevelStreamConfig,
    id: Bytes,
    token0: &Bytes,
    token1: &Bytes,
) -> ProtocolComponent {
    let protocol_system = config.protocol_system();
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
    use std::str::FromStr;

    use num_bigint::BigUint;

    use super::*;

    const PAMM: &str = "0x5979458912f80b96d30d4220af8e2e4925a33320";
    const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn tokens() -> HashMap<Bytes, Token> {
        [token(WBTC, "WBTC", 8), token(USDC, "USDC", 6), token(WETH, "WETH", 18)]
            .into_iter()
            .map(|token| (token.address.clone(), token))
            .collect()
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

    fn message(block_number: u64, pairs: Vec<TitanPairLevels>) -> TitanPriceLevelMessage {
        TitanPriceLevelMessage {
            block_number,
            pamms: vec![TitanPammLevels { pamm: Bytes::from_str(PAMM).unwrap(), pairs }],
        }
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

    #[test]
    fn first_snapshot_emits_new_pair_with_both_directions() {
        let mut tracker = tracker();
        let Update {
            block_number_or_timestamp,
            is_partial,
            sync_states,
            states,
            new_pairs,
            removed_pairs,
        } = tracker
            .process(message(100, wbtc_usdc_pairs()))
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

        let PriceLevelStreamState { token0, token1, quotes_0_to_1, quotes_1_to_0, gas_cost } =
            states[&id]
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
        let mut tracker = tracker();
        tracker
            .process(message(100, wbtc_usdc_pairs()))
            .expect("update expected");
        let update = tracker
            .process(message(101, wbtc_usdc_pairs()))
            .expect("update expected");

        assert!(update.new_pairs.is_empty());
        assert!(update.removed_pairs.is_empty());
        assert!(update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn dropped_pair_is_removed() {
        let mut tracker = tracker();
        tracker
            .process(message(100, wbtc_usdc_pairs()))
            .expect("update expected");
        let weth_usdc =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        let update = tracker
            .process(message(101, weth_usdc))
            .expect("update expected");

        assert_eq!(update.removed_pairs.len(), 1);
        assert!(update
            .removed_pairs
            .contains_key(&expected_id()));
        assert_eq!(update.new_pairs.len(), 1);
        assert_eq!(update.states.len(), 1);
    }

    #[test]
    fn out_of_order_frame_is_skipped() {
        let mut tracker = tracker();
        tracker
            .process(message(101, wbtc_usdc_pairs()))
            .expect("update expected");

        // A frame for an older block is stale: no update, and the caches stay untouched even
        // though the frame's snapshot differs completely.
        let stale =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        assert!(tracker
            .process(message(100, stale))
            .is_none());

        // The next current frame diffs against the pre-stale state: nothing was added or
        // removed in between.
        let update = tracker
            .process(message(102, wbtc_usdc_pairs()))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn vanished_pamm_has_its_pairs_removed() {
        let mut tracker = tracker();
        tracker
            .process(message(100, wbtc_usdc_pairs()))
            .expect("update expected");

        // The next frame no longer contains the pAMM at all: a complete snapshot without a
        // venue means the venue is gone, pairs and all.
        let update = tracker
            .process(TitanPriceLevelMessage { block_number: 101, pamms: vec![] })
            .expect("update expected");
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert_eq!(update.removed_pairs.len(), 1);
        assert!(update
            .removed_pairs
            .contains_key(&expected_id()));

        // Nothing served and nothing changed: no update.
        assert!(tracker
            .process(TitanPriceLevelMessage { block_number: 102, pamms: vec![] })
            .is_none());

        // A venue that reappears is a new pair again.
        let update = tracker
            .process(message(103, wbtc_usdc_pairs()))
            .expect("update expected");
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn unregistered_pamm_produces_no_update_without_auto_detection() {
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
        );
        assert!(tracker
            .process(message(100, wbtc_usdc_pairs()))
            .is_none());
    }

    #[test]
    fn denied_pamm_is_not_auto_detected() {
        let denied = HashSet::from([Bytes::from_str(PAMM).unwrap()]);
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            denied,
            tokens(),
            true,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
        );
        assert!(tracker
            .process(message(100, wbtc_usdc_pairs()))
            .is_none());
    }

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
    fn auto_detected_pamm_is_served_under_its_address() {
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            true,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
        );
        let update = tracker
            .process(message(100, wbtc_usdc_pairs()))
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
            .process(message(101, wbtc_usdc_pairs()))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    #[test]
    fn auto_detected_gas_cost_override_applies() {
        let mut tracker = SnapshotTracker::new(
            HashMap::new(),
            HashSet::new(),
            tokens(),
            true,
            BigUint::from(42_000u64),
        );
        let update = tracker
            .process(message(100, wbtc_usdc_pairs()))
            .expect("update expected");

        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.gas_cost, BigUint::from(42_000u64));
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

    #[test]
    fn unknown_tokens_are_skipped() {
        let mut tracker = tracker();
        let unknown = vec![pair_levels(
            "0x1111111111111111111111111111111111111111",
            USDC,
            vec![level(1, 1)],
        )];
        assert!(tracker
            .process(message(100, unknown))
            .is_none());
    }
}
