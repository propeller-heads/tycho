use std::{collections::HashMap, env, fmt::Display, str::FromStr, sync::Arc, time::Duration};

use alloy::primitives::B256;
use clap::ValueEnum;
use futures::StreamExt as _;
use miette::{miette, IntoDiagnostic, WrapErr};
use rand::prelude::IteratorRandom;
use tokio::{sync::mpsc::Sender, task::JoinHandle};
use tracing::{info, warn};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_execution::encoding::evm::get_router_address;
use tycho_simulation::{
    book::{
        all_pairs::QuoteRule, quote_tokens::usd_stablecoins_for_chain, BookFeedConfig,
        BookFeedEvent, BookFeedOutcome, BookFeedStreams, BookSnapshot,
    },
    pamm::protocols::{
        baibai::{self, BaibaiFeedBuilder},
        efficient_prop::{self, CreditGas, EfficientPropDeployment, EfficientPropFeedBuilder},
        flamm::{self, FlammFeed, FlammFeedBuilder},
        metric::{self, feed::MetricFeedBuilder},
    },
    rfq::protocols::{
        bebop::{self, feed::BebopFeedBuilder},
        euclid::{self, feed::EuclidFeedBuilder},
        hashflow::{self, feed::HashflowFeedBuilder},
        liquorice::{self, feed::LiquoriceFeedBuilder},
        native::{self, feed::NativeFeedBuilder},
    },
    snapshot_feed::http::HttpFeedConfig,
};
use tycho_test::execution::encoding::USER_ADDR;

use crate::stream_processor::{StreamUpdate, StreamUpdatePayload};

/// The venues that sign binding quotes. The pAMM venues are [`PammVenue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RfqVenue {
    Bebop,
    Euclid,
    Hashflow,
    Liquorice,
    Native,
}

impl RfqVenue {
    /// The venues whose quotes need no credentials, so they run unless RFQ is disabled.
    pub const KEYLESS: [RfqVenue; 1] = [RfqVenue::Euclid];
}

impl Display for RfqVenue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RfqVenue::Bebop => write!(f, "{}", bebop::PROTOCOL_SYSTEM),
            RfqVenue::Euclid => write!(f, "{}", euclid::PROTOCOL_SYSTEM),
            RfqVenue::Hashflow => write!(f, "{}", hashflow::PROTOCOL_SYSTEM),
            RfqVenue::Liquorice => write!(f, "{}", liquorice::PROTOCOL_SYSTEM),
            RfqVenue::Native => write!(f, "{}", native::PROTOCOL_SYSTEM),
        }
    }
}

/// The pAMM venues the book stream can run, named on the command line by `--pamm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum PammVenue {
    Metric,
    /// Base only. Reads its books from the chain over `--rpc-url`, so it needs no credentials.
    Baibai,
    /// Efficient PropAMM listing doors, on Ethereum; the feed refuses to build on Base for now.
    /// Reads every door's quotes over
    /// `--rpc-url`; the front, the listing-door factory, the desk contract code hash (Ethereum
    /// only) and the front implementations it was checked against come from
    /// `EFFICIENT_PROP_FRONT`, `EFFICIENT_PROP_FACTORY`, `EFFICIENT_PROP_CELL_CODEHASH` and
    /// `EFFICIENT_PROP_IMPLEMENTATIONS` (comma-separated), the price sources a listing reading a
    /// print may be served on from `EFFICIENT_PROP_PRICE_SOURCES` (comma-separated ids, none when
    /// unset), the makers' desk contracts whose listings are served from
    /// `EFFICIENT_PROP_APPROVED_DESKS` (comma-separated, none when unset: nothing is served) and
    /// `EFFICIENT_PROP_APPROVED_DESKS_URL` (a list that rules while fresh), and on Ethereum the
    /// URLs of the next block's state map from `EFFICIENT_PROP_STATE_URLS` (comma-separated,
    /// required there) and the listings served with gas named for them from
    /// `EFFICIENT_PROP_CREDIT_LISTINGS` (none when unset).
    EfficientProp,
    /// Efficient pools, on Base, Ethereum and BSC. Reads its books from the chain over
    /// `--rpc-url` and needs no credentials, but serves only what the `FLAMM_*` variables name.
    Flamm,
}

impl PammVenue {
    /// The venues run when `--pamm` is not given.
    pub fn defaults() -> Vec<PammVenue> {
        vec![PammVenue::Metric]
    }

    /// The name `--pamm` takes for this venue.
    fn cli_name(self) -> String {
        self.to_possible_value()
            .expect("no PammVenue is skipped on the command line")
            .get_name()
            .to_string()
    }
}

impl Display for PammVenue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PammVenue::Metric => write!(f, "{}", metric::PROTOCOL_SYSTEM),
            PammVenue::Baibai => write!(f, "{}", baibai::PROTOCOL_SYSTEM),
            PammVenue::EfficientProp => write!(f, "{}", efficient_prop::PROTOCOL_SYSTEM),
            PammVenue::Flamm => write!(f, "{}", flamm::PROTOCOL_SYSTEM),
        }
    }
}

/// Which RFQ venues the book stream runs: each venue whose credentials are set, and every venue
/// that needs none ([`RfqVenue::KEYLESS`]). A venue without its credentials is skipped, since
/// RFQ has no explicit venue list.
#[derive(Clone, PartialEq, Eq)]
pub enum RfqSelection {
    /// `--disable-rfq`: no RFQ venue.
    Disabled,
    /// The venues whose credentials were found, possibly none. The keyless venues run beside
    /// them and are not listed.
    Credentials(HashMap<RfqVenue, (String, String)>),
}

/// Prints the venues only: the credentials are secrets and must not reach the logs.
impl std::fmt::Debug for RfqSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RfqSelection::Disabled => write!(f, "Disabled"),
            RfqSelection::Credentials(credentials) => {
                let mut venues: Vec<_> = credentials.keys().collect();
                venues.sort_by_key(ToString::to_string);
                f.debug_tuple("Credentials")
                    .field(&venues)
                    .finish()
            }
        }
    }
}

impl RfqSelection {
    /// The selection `--disable-rfq` and the venues' credentials describe. `secret` looks up an
    /// environment variable.
    pub fn from_cli(disable_rfq: bool, secret: impl Fn(&str) -> Option<String>) -> Self {
        if disable_rfq {
            return RfqSelection::Disabled;
        }
        let mut credentials = HashMap::new();
        if let Some(key) = secret("BEBOP_KEY") {
            info!("Bebop RFQ credentials found");
            credentials.insert(RfqVenue::Bebop, (String::new(), key));
        } else {
            info!("Bebop RFQ credentials not found. Expected environment variable: BEBOP_KEY");
        }
        if let (Some(user), Some(key)) = (secret("HASHFLOW_USER"), secret("HASHFLOW_KEY")) {
            info!("Hashflow RFQ credentials found");
            credentials.insert(RfqVenue::Hashflow, (user, key));
        } else {
            info!(
                "Hashflow RFQ credentials not found. Expected environment variables: \
                 HASHFLOW_USER, HASHFLOW_KEY"
            );
        }
        if let (Some(user), Some(key)) = (secret("LIQUORICE_USER"), secret("LIQUORICE_KEY")) {
            info!("Liquorice RFQ credentials found");
            credentials.insert(RfqVenue::Liquorice, (user, key));
        } else {
            info!(
                "Liquorice RFQ credentials not found. Expected environment variables: \
                 LIQUORICE_USER, LIQUORICE_KEY"
            );
        }
        if let Some(key) = secret("NATIVE_API_KEY") {
            info!("Native RFQ credentials found");
            credentials.insert(RfqVenue::Native, (String::new(), key));
        } else {
            info!(
                "Native RFQ credentials not found. Expected environment variable: NATIVE_API_KEY"
            );
        }
        RfqSelection::Credentials(credentials)
    }

    /// The venues to build a feed for, each with its credentials: the keyed venues whose
    /// credentials were found, then the keyless ones with empty credentials.
    fn venues(&self) -> Vec<(RfqVenue, String, String)> {
        let RfqSelection::Credentials(credentials) = self else {
            return Vec::new();
        };
        credentials
            .iter()
            .map(|(venue, (user, key))| (*venue, user.clone(), key.clone()))
            .chain(
                RfqVenue::KEYLESS
                    .into_iter()
                    .map(|venue| (venue, String::new(), String::new())),
            )
            .collect()
    }

    /// Why this selection added no feed, for the warning when no book feed runs.
    fn why_empty(&self) -> &'static str {
        match self {
            RfqSelection::Disabled => "RFQ is disabled by --disable-rfq",
            RfqSelection::Credentials(credentials) if credentials.is_empty() => {
                "no RFQ credentials are set (BEBOP_KEY, HASHFLOW_USER and HASHFLOW_KEY, \
                 LIQUORICE_USER and LIQUORICE_KEY, or NATIVE_API_KEY), and no keyless RFQ \
                 venue serves this chain"
            }
            RfqSelection::Credentials(_) => {
                "no RFQ venue with credentials, nor any keyless one, serves this chain"
            }
        }
    }
}

/// Which pAMM venues the book stream runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PammSelection {
    /// `--disable-pamm`: no pAMM venue.
    Disabled,
    /// No `--pamm`: [`PammVenue::defaults`]. A venue that cannot run is skipped.
    Defaults,
    /// `--pamm`: exactly these venues, each once. A venue that cannot run fails the run.
    Listed(Vec<PammVenue>),
}

impl PammSelection {
    /// The selection `--disable-pamm` and `--pamm` describe.
    pub fn from_cli(disable_pamm: bool, pamm: Option<Vec<PammVenue>>) -> Self {
        if disable_pamm {
            return PammSelection::Disabled;
        }
        let Some(listed) = pamm else {
            return PammSelection::Defaults;
        };
        let mut venues = Vec::with_capacity(listed.len());
        for venue in listed {
            if !venues.contains(&venue) {
                venues.push(venue);
            }
        }
        PammSelection::Listed(venues)
    }

    /// Why this selection added no feed, for the warning when no book feed runs. `None` for
    /// `--pamm`: a listed venue that cannot run fails the run instead.
    fn why_empty(&self) -> Option<&'static str> {
        match self {
            PammSelection::Disabled => Some("pAMM is disabled by --disable-pamm"),
            PammSelection::Defaults => Some("no default pAMM venue could run (see the log above)"),
            PammSelection::Listed(_) => None,
        }
    }
}

pub struct BookStreamProcessor {
    chain: Chain,
    tvl_threshold: f64,
    rfq: RfqSelection,
    sample_size: usize,
    pamm: PammSelection,
    /// The node of `chain`, which the pAMM venues that read their books on chain query.
    rpc_url: String,
    /// The protocol's stream will skip messages for this duration after processing a message
    skip_messages_duration: Duration,
    /// Whether each venue publishes one component for all of its pairs rather than one each.
    all_pairs: bool,
}

impl BookStreamProcessor {
    #[expect(clippy::too_many_arguments, reason = "one argument per stream setting")]
    pub fn new(
        chain: Chain,
        tvl_threshold: f64,
        sample_size: usize,
        skip_messages_duration: Duration,
        rfq: RfqSelection,
        pamm: PammSelection,
        rpc_url: String,
        all_pairs: bool,
    ) -> Self {
        Self {
            chain,
            tvl_threshold,
            rfq,
            sample_size,
            pamm,
            rpc_url,
            skip_messages_duration,
            all_pairs,
        }
    }

    /// Starts the book stream, or returns `None` when no book feed was added: both kinds are
    /// disabled, or neither has a venue that can run, which is logged with the reason for each.
    pub async fn run_stream(
        &self,
        all_tokens: Arc<HashMap<Bytes, Token>>,
        stream_tx: Sender<miette::Result<StreamUpdate>>,
    ) -> miette::Result<Option<JoinHandle<()>>> {
        info!("Starting book stream processor for chain {:?}", self.chain);
        // A receiver holds only its provider's latest complete book, so the per-provider
        // throttling below never samples a backlog: whenever it decides to emit, what it reads
        // is the freshest state that provider has.
        let book_config = BookFeedConfig {
            chain: self.chain,
            tokens: all_tokens,
            min_tvl_usd: self.tvl_threshold,
        };

        // The RFQ feeds price their books' TVL in these; Metric reports USD TVL itself, so it is
        // the one venue a chain without a curated set can still serve.
        let usd_quote_tokens = usd_stablecoins_for_chain(self.chain).map(Arc::new);

        let mut feeds = BookFeedStreams::new();

        add_pamm_feeds(&self.pamm, &book_config, &self.rpc_url, &mut feeds, |name| {
            env::var(name).ok()
        })?;

        for (protocol, user, key) in self.rfq.venues() {
            let Some(usd_quote_tokens) = &usd_quote_tokens else {
                warn!(
                    "Skipping {protocol}: it prices its books' TVL in USD stablecoins, and none \
                     are curated for chain {:?}",
                    self.chain
                );
                continue;
            };
            info!("Adding {protocol} feed...");
            match protocol {
                RfqVenue::Bebop => {
                    // Bebop can require origin identification per API account; identify the
                    // simulated flow with the test user EOA and the router the encoded
                    // transactions target.
                    let mut bebop_builder = BebopFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        key,
                    )
                    .origin_address(
                        Bytes::from_str(USER_ADDR)
                            .into_diagnostic()
                            .wrap_err("Invalid test user address")?,
                    )
                    .origin_source("tycho-integration-test".to_string());
                    if self.all_pairs {
                        bebop_builder = bebop_builder.all_pairs();
                    }

                    if let Ok(router_address) = get_router_address(&self.chain) {
                        bebop_builder = bebop_builder.origin_target(router_address.clone());
                    }

                    let bebop_feed = bebop_builder
                        .build()
                        .into_diagnostic()
                        .wrap_err("Failed to create Bebop feed")?;

                    feeds
                        .add(bebop::PROTOCOL_SYSTEM, bebop_feed)
                        .expect("each provider is added once");
                }
                RfqVenue::Hashflow => {
                    let mut builder = HashflowFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        user,
                        key,
                    )
                    .feed_config(HttpFeedConfig {
                        poll_interval: Duration::from_secs(30),
                        max_snapshot_age: Some(Duration::from_secs(90)),
                        ..HashflowFeedBuilder::default_feed_config()
                    });
                    if self.all_pairs {
                        builder = builder.all_pairs(QuoteRule::OncePerMaker);
                    }
                    let hashflow_feed = builder
                        .build()
                        .into_diagnostic()
                        .wrap_err("Failed to create Hashflow feed")?;

                    feeds
                        .add(hashflow::PROTOCOL_SYSTEM, hashflow_feed)
                        .expect("each provider is added once");
                }
                RfqVenue::Liquorice => {
                    let mut builder = LiquoriceFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        user,
                        key,
                    )
                    .feed_config(HttpFeedConfig {
                        poll_interval: Duration::from_secs(30),
                        max_snapshot_age: Some(Duration::from_secs(90)),
                        ..LiquoriceFeedBuilder::default_feed_config()
                    });
                    if self.all_pairs {
                        builder = builder.all_pairs(QuoteRule::OncePerMaker);
                    }
                    let liquorice_feed = builder
                        .build()
                        .into_diagnostic()
                        .wrap_err("Failed to create Liquorice feed")?;

                    feeds
                        .add(liquorice::PROTOCOL_SYSTEM, liquorice_feed)
                        .expect("each provider is added once");
                }
                RfqVenue::Euclid => {
                    // Euclid serves Ethereum; on any other chain the feed is skipped rather
                    // than failing the whole processor.
                    let mut builder =
                        EuclidFeedBuilder::new(book_config.clone(), Arc::clone(usd_quote_tokens));
                    if self.all_pairs {
                        builder = builder.all_pairs();
                    }
                    match builder.build() {
                        Ok(euclid_feed) => {
                            feeds
                                .add(euclid::PROTOCOL_SYSTEM, euclid_feed)
                                .expect("each provider is added once");
                        }
                        Err(e) => {
                            warn!(
                                "Euclid RFQ not supported on chain {:?}, skipping: {e}",
                                self.chain
                            )
                        }
                    }
                }
                RfqVenue::Native => {
                    // Native serves a fixed set of chains; on any other chain the feed is
                    // skipped rather than failing the whole processor.
                    let mut builder = NativeFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        key,
                    )
                    .feed_config(HttpFeedConfig {
                        poll_interval: Duration::from_secs(30),
                        max_snapshot_age: Some(Duration::from_secs(90)),
                        ..NativeFeedBuilder::default_feed_config()
                    });
                    if self.all_pairs {
                        builder = builder.all_pairs();
                    }
                    match builder.build() {
                        Ok(native_feed) => {
                            feeds
                                .add(native::PROTOCOL_SYSTEM, native_feed)
                                .expect("each provider is added once");
                        }
                        Err(e) => {
                            warn!(
                                "Native RFQ not supported on chain {:?}, skipping: {e}",
                                self.chain
                            )
                        }
                    }
                }
            }
        }

        if feeds.labels().next().is_none() {
            match (&self.rfq, &self.pamm) {
                (RfqSelection::Disabled, PammSelection::Disabled) => {
                    info!("Book feeds are disabled");
                }
                (
                    RfqSelection::Disabled | RfqSelection::Credentials(_),
                    PammSelection::Disabled | PammSelection::Defaults | PammSelection::Listed(_),
                ) => {
                    let mut reasons = vec![self.rfq.why_empty()];
                    reasons.extend(self.pamm.why_empty());
                    warn!("No book feed is running: {}", reasons.join("; "));
                }
            }
            return Ok(None);
        }

        let mut is_first_update = true;
        let sample_size = self.sample_size;
        let skip_messages_duration = self.skip_messages_duration;
        let mut next_stream_times: HashMap<String, tokio::time::Instant> = HashMap::new();

        let handle = tokio::spawn(async move {
            info!("book stream processor started");
            while let Some((protocol_system, event)) = feeds.next().await {
                let snapshot = match event {
                    BookFeedEvent::Published(snapshot) => Ok(snapshot),
                    BookFeedEvent::Withdrawn => {
                        warn!("{protocol_system} withdrew its books, nothing servable from it");
                        continue;
                    }
                    BookFeedEvent::Ended(BookFeedOutcome::Failed(error)) => {
                        Err(miette!(error).wrap_err(format!("{protocol_system} gave up")))
                    }
                    BookFeedEvent::Ended(BookFeedOutcome::Panicked(error)) => {
                        Err(miette!(error).wrap_err(format!("{protocol_system} feed task died")))
                    }
                    BookFeedEvent::Ended(BookFeedOutcome::RanOut) => {
                        Err(miette!("{protocol_system} has nothing left to serve"))
                    }
                };
                let BookSnapshot { anchor, books } = match snapshot {
                    Ok(snapshot) => snapshot,
                    // The provider is gone for good; the harness hears why, and the loop goes on
                    // with the others.
                    Err(ended) => {
                        if !forward(&stream_tx, Err(ended)).await {
                            break;
                        }
                        continue;
                    }
                };

                // Handle throttling for the update's protocol
                let next_stream_time = next_stream_times
                    .entry(protocol_system.clone())
                    .or_insert_with(tokio::time::Instant::now);
                let now = tokio::time::Instant::now();
                if now < *next_stream_time {
                    continue;
                }
                *next_stream_time = now + skip_messages_duration;

                // Sample random books from the venue's complete snapshot
                let books = Arc::new(
                    books
                        .iter()
                        .choose_multiple(&mut rand::rng(), sample_size)
                        .into_iter()
                        .map(|(id, book)| (id.clone(), book.clone()))
                        .collect(),
                );

                let received_at =
                    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                        Ok(duration) => duration,
                        Err(e) => {
                            if !forward(
                                &stream_tx,
                                Err(miette!(e).wrap_err("Error getting current timestamp")),
                            )
                            .await
                            {
                                break;
                            }
                            continue;
                        }
                    };

                // Send the latest update
                let update = StreamUpdate {
                    payload: StreamUpdatePayload::Book {
                        protocol_system,
                        received_at: anchor.0,
                        books,
                    },
                    is_first_update,
                    received_at,
                };
                if is_first_update {
                    is_first_update = false;
                }
                if !forward(&stream_tx, Ok(update)).await {
                    break;
                }
            }
            // Either every feed ended (each reported above) or the receiver dropped; dropping
            // the BookFeedStreams aborts any feed task still running.
            info!("book stream processor stopping");
        });
        Ok(Some(handle))
    }
}

/// Adds the feed of each venue `selection` names. `rpc_url` is the node of the book config's
/// chain, and `secret` looks up an environment variable.
///
/// # Errors
/// Returns an error when a venue listed in `--pamm` lacks its credentials or its feed fails to
/// build. A default venue that cannot run is logged and skipped instead.
fn add_pamm_feeds(
    selection: &PammSelection,
    book_config: &BookFeedConfig,
    rpc_url: &str,
    feeds: &mut BookFeedStreams,
    secret: impl Fn(&str) -> Option<String>,
) -> miette::Result<()> {
    let (venues, on_failure) = match selection {
        PammSelection::Disabled => return Ok(()),
        PammSelection::Defaults => (PammVenue::defaults(), OnFailure::Skip),
        PammSelection::Listed(venues) => (venues.clone(), OnFailure::FailRun),
    };
    for venue in venues {
        let added = match venue {
            PammVenue::Metric => match secret("METRIC_API_KEY") {
                // Metric's own defaults already set its cadence and withdrawal age.
                Some(api_key) => MetricFeedBuilder::new(book_config.clone(), api_key)
                    .build()
                    .map_err(|e| Unavailable::BuildFailed(e.to_string()))
                    .map(|feed| {
                        feeds
                            .add(venue.to_string(), feed)
                            .is_ok()
                    }),
                None => Err(Unavailable::MissingSecret("METRIC_API_KEY")),
            },
            PammVenue::Baibai => BaibaiFeedBuilder::new(book_config.clone(), rpc_url.to_string())
                .build()
                .map_err(|e| Unavailable::BuildFailed(e.to_string()))
                .map(|feed| {
                    feeds
                        .add(venue.to_string(), feed)
                        .is_ok()
                }),
            PammVenue::EfficientProp => efficient_prop_deployment(&secret).and_then(|deployment| {
                let sources = efficient_prop_price_sources(&secret)?;
                let mut builder = EfficientPropFeedBuilder::new(
                    book_config.clone(),
                    rpc_url.to_string(),
                    deployment,
                )
                .approved_price_sources(sources)
                .with_approved_desks(efficient_prop_approved_desks(&secret)?);
                if let Some(url) = secret("EFFICIENT_PROP_APPROVED_DESKS_URL") {
                    builder = builder.with_approved_desks_url(url.trim());
                }
                if let Some(hash) = secret("EFFICIENT_PROP_DOOR_CODEHASH") {
                    let hash = Bytes::from_str(hash.trim())
                        .ok()
                        .filter(|bytes| bytes.len() == 32)
                        .ok_or_else(|| {
                            Unavailable::BuildFailed(format!(
                                "EFFICIENT_PROP_DOOR_CODEHASH {hash} is not a hash of 32 bytes"
                            ))
                        })?;
                    builder = builder.with_door_code_hash(hash);
                }
                if let Some(state_urls) = efficient_prop_state_urls(&secret) {
                    builder = builder.state_urls(state_urls);
                }
                for (listing, gas) in efficient_prop_credit_listings(&secret)? {
                    builder = builder.credit_listing(listing, gas);
                }
                builder
                    .build()
                    .map_err(|e| Unavailable::BuildFailed(e.to_string()))
                    .map(|feed| {
                        feeds
                            .add(venue.to_string(), feed)
                            .is_ok()
                    })
            }),
            // no default venue, so a feed that will not build always fails the run
            PammVenue::Flamm => flamm_feed(book_config, rpc_url, &secret)
                .map_err(|e| Unavailable::BuildFailed(e.to_string()))
                .map(|feed| {
                    feeds
                        .add(venue.to_string(), feed)
                        .is_ok()
                }),
        };
        match added {
            Ok(true) => info!("Added {venue} feed"),
            Ok(false) => return Err(miette!("The {venue} feed was added twice")),
            Err(unavailable) => unavailable.handle(venue, &on_failure)?,
        }
    }
    Ok(())
}

/// The Efficient PropAMM quote layout, front, listing-door factory, desk contract code hash and
/// front implementations, from `EFFICIENT_PROP_QUOTE_LAYOUT` and `EFFICIENT_PROP_CELL_CODEHASH`
/// (none when unset: the feed requires both on Ethereum, where the quote contract is read, and
/// refuses both on Base), `EFFICIENT_PROP_FRONT`, `EFFICIENT_PROP_FACTORY` and
/// `EFFICIENT_PROP_IMPLEMENTATIONS` (comma-separated). `secret` looks up an environment variable.
/// The doors' code hash is not configured: the feed reads it from the factory.
fn efficient_prop_deployment(
    secret: impl Fn(&str) -> Option<String>,
) -> Result<EfficientPropDeployment, Unavailable> {
    let read = |name: &'static str| secret(name).ok_or(Unavailable::MissingSecret(name));
    let parse = |name: &str, value: &str| {
        Bytes::from_str(value.trim())
            .map_err(|e| Unavailable::BuildFailed(format!("{name} {value} is not an address: {e}")))
    };
    let quote_layout =
        secret("EFFICIENT_PROP_QUOTE_LAYOUT").map(|layout| layout.trim().to_string());
    let front = parse("EFFICIENT_PROP_FRONT", &read("EFFICIENT_PROP_FRONT")?)?;
    let factory = parse("EFFICIENT_PROP_FACTORY", &read("EFFICIENT_PROP_FACTORY")?)?;
    let cell_codehash = secret("EFFICIENT_PROP_CELL_CODEHASH")
        .map(|hash| {
            Bytes::from_str(hash.trim())
                .ok()
                .filter(|bytes| bytes.len() == 32)
                .ok_or_else(|| {
                    Unavailable::BuildFailed(format!(
                        "EFFICIENT_PROP_CELL_CODEHASH {hash} is not a hash of 32 bytes"
                    ))
                })
        })
        .transpose()?;
    let implementations = read("EFFICIENT_PROP_IMPLEMENTATIONS")?
        .split(',')
        .map(|implementation| parse("EFFICIENT_PROP_IMPLEMENTATIONS", implementation))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EfficientPropDeployment { quote_layout, front, factory, cell_codehash, implementations })
}

/// The price sources a listing door reading a print may be served on, from
/// `EFFICIENT_PROP_PRICE_SOURCES` (comma-separated ids, as the front's `registerSource` numbered
/// them): none when it is unset, so only listings reading no print are served.
fn efficient_prop_price_sources(
    secret: impl Fn(&str) -> Option<String>,
) -> Result<Vec<u32>, Unavailable> {
    let Some(list) = secret("EFFICIENT_PROP_PRICE_SOURCES") else {
        return Ok(Vec::new());
    };
    list.split(',')
        .filter(|id| !id.trim().is_empty())
        .map(|id| {
            id.trim().parse::<u32>().map_err(|e| {
                Unavailable::BuildFailed(format!(
                    "EFFICIENT_PROP_PRICE_SOURCES {list} holds {id}, not a source id: {e}"
                ))
            })
        })
        .collect()
}

/// The desk contracts whose listings are served, from `EFFICIENT_PROP_APPROVED_DESKS`
/// (comma-separated addresses): none when it is unset, so nothing is served but what the list at
/// `EFFICIENT_PROP_APPROVED_DESKS_URL` names while it is fresh.
fn efficient_prop_approved_desks(
    secret: impl Fn(&str) -> Option<String>,
) -> Result<Vec<Bytes>, Unavailable> {
    let Some(list) = secret("EFFICIENT_PROP_APPROVED_DESKS") else {
        return Ok(Vec::new());
    };
    list.split(',')
        .map(str::trim)
        .filter(|desk| !desk.is_empty())
        .map(|desk| {
            Bytes::from_str(desk)
                .ok()
                .filter(|bytes| bytes.len() == 20)
                .ok_or_else(|| {
                    Unavailable::BuildFailed(format!(
                        "EFFICIENT_PROP_APPROVED_DESKS {list} holds {desk}, not an address"
                    ))
                })
        })
        .collect()
}

/// The listings served with gas named for them, each with the gas its fills are charged, from
/// `EFFICIENT_PROP_CREDIT_LISTINGS` (comma-separated `listing=custody sell:custody buy:route
/// sell:route buy`, the four figures measured on that listing's pool, and optionally a fifth, its
/// dearest fill before refunds, which its book's gas limit addition is sized from and which is at
/// least the dearest of the four and 19,900): none when it is unset, so
/// no listing with a credit mode, of a stock token's pair, or of a pair and place the feed holds
/// no measured figure for is served. The feed refuses one on Base.
fn efficient_prop_credit_listings(
    secret: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(u32, CreditGas)>, Unavailable> {
    let Some(list) = secret("EFFICIENT_PROP_CREDIT_LISTINGS") else {
        return Ok(Vec::new());
    };
    list.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let refused = |why: String| {
                Unavailable::BuildFailed(format!(
                    "EFFICIENT_PROP_CREDIT_LISTINGS {list} holds {entry}: {why}"
                ))
            };
            let (listing, gas) = entry
                .split_once('=')
                .ok_or_else(|| refused("not listing=gas".into()))?;
            let listing = listing
                .trim()
                .parse::<u32>()
                .map_err(|e| refused(format!("not a listing id: {e}")))?;
            let gas = gas
                .parse::<CreditGas>()
                .map_err(refused)?;
            Ok((listing, gas))
        })
        .collect()
}

/// The URLs the listing doors read the next block's state map from on Ethereum, from
/// `EFFICIENT_PROP_STATE_URLS` (comma-separated): none when it is unset, which the feed refuses
/// on Ethereum, and a URL set on Base fails the feed's build.
fn efficient_prop_state_urls(secret: impl Fn(&str) -> Option<String>) -> Option<Vec<String>> {
    secret("EFFICIENT_PROP_STATE_URLS").map(|list| {
        list.split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// The Efficient pools feed the environment configures. Its factories differ by chain and what it
/// may serve is the run's to say, so nothing defaults: `FLAMM_FACTORIES`, then `FLAMM_POOLS` to
/// name pools, or `FLAMM_HOOK_CLASSES` with `FLAMM_PRICE_FEEDS` or `FLAMM_PRICE_FEED_CODE_HASHES`
/// to admit them by what they run. Every listed class is an Efficient pools hook, of the one shape
/// there is.
///
/// # Errors
/// Returns an error when a variable does not parse or the feed refuses what they add up to.
fn flamm_feed(
    book_config: &BookFeedConfig,
    rpc_url: &str,
    secret: impl Fn(&str) -> Option<String>,
) -> miette::Result<FlammFeed> {
    let mut builder = FlammFeedBuilder::new(book_config.clone(), rpc_url.to_string())
        .factories(listed::<Bytes>("FLAMM_FACTORIES", &secret)?)
        .hook_classes(
            listed::<B256>("FLAMM_HOOK_CLASSES", &secret)?
                .into_iter()
                .map(|class| (class, flamm::HookShape::Everlong))
                .collect(),
        )
        .price_feeds(listed::<Bytes>("FLAMM_PRICE_FEEDS", &secret)?)
        .price_feed_code_hashes(listed::<B256>("FLAMM_PRICE_FEED_CODE_HASHES", &secret)?);
    let pools = listed::<Bytes>("FLAMM_POOLS", &secret)?;
    if !pools.is_empty() {
        builder = builder.pools(pools);
    }
    builder.build().into_diagnostic()
}

/// The comma-separated values of the variable `name`, none where it is not set.
///
/// # Errors
/// Returns an error naming the variable and the value that does not parse.
fn listed<T>(name: &str, secret: &impl Fn(&str) -> Option<String>) -> miette::Result<Vec<T>>
where
    T: FromStr,
    T::Err: Display,
{
    secret(name)
        .iter()
        .flat_map(|values| values.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|e| miette!("{name} holds {value}, which does not parse: {e}"))
        })
        .collect()
}

/// What a pAMM venue that cannot run does to the run.
enum OnFailure {
    /// A default venue: logged and skipped.
    Skip,
    /// A venue listed in `--pamm`: the run fails.
    FailRun,
}

/// Why a pAMM venue cannot run.
enum Unavailable {
    /// This environment variable, which holds the venue's credentials, is not set.
    MissingSecret(&'static str),
    /// The venue's feed failed to build, for this reason.
    BuildFailed(String),
}

impl Unavailable {
    /// Logs and skips `venue`, or fails the run, as `on_failure` says.
    fn handle(self, venue: PammVenue, on_failure: &OnFailure) -> miette::Result<()> {
        match (self, on_failure) {
            (Unavailable::MissingSecret(name), OnFailure::FailRun) => Err(miette!(
                "--pamm lists {}, but {name} is not set. Set it, or drop {} from --pamm.",
                venue.cli_name(),
                venue.cli_name()
            )),
            (Unavailable::MissingSecret(name), OnFailure::Skip) => {
                info!("Skipping {venue}: its credentials are not set. Expected variable: {name}");
                Ok(())
            }
            (Unavailable::BuildFailed(e), OnFailure::FailRun) => {
                Err(miette!("Failed to build the {venue} feed that --pamm lists: {e}"))
            }
            (Unavailable::BuildFailed(e), OnFailure::Skip) => {
                warn!("Failed to build {venue} feed, skipping: {e}");
                Ok(())
            }
        }
    }
}

/// Hands one update to the harness. `false` once nobody is receiving, which is the processor's
/// signal to stop.
async fn forward(
    stream_tx: &Sender<miette::Result<StreamUpdate>>,
    update: miette::Result<StreamUpdate>,
) -> bool {
    let receiving = stream_tx.send(update).await.is_ok();
    if !receiving {
        warn!("Receiver dropped, stopping stream processor");
    }
    receiving
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The variables an Efficient pools feed needs to admit pools by what they run.
    const FLAMM_LISTS: [(&str, &str); 3] = [
        ("FLAMM_FACTORIES", "0x1111111111111111111111111111111111111111"),
        (
            "FLAMM_HOOK_CLASSES",
            "0x2222222222222222222222222222222222222222222222222222222222222222",
        ),
        ("FLAMM_PRICE_FEEDS", "0x3333333333333333333333333333333333333333"),
    ];

    /// The labels of the feeds `selection` adds on `chain` with `variables` set.
    fn added_on(
        chain: Chain,
        selection: &PammSelection,
        variables: &[(&str, &str)],
    ) -> miette::Result<Vec<String>> {
        let book_config =
            BookFeedConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 };
        let mut feeds = BookFeedStreams::new();
        add_pamm_feeds(selection, &book_config, "http://127.0.0.1:8545", &mut feeds, |name| {
            variables
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_string())
        })?;
        Ok(feeds
            .labels()
            .map(str::to_string)
            .collect())
    }

    /// The labels of the feeds `selection` adds on Base when `METRIC_API_KEY` holds `key`.
    fn added(selection: &PammSelection, key: Option<&str>) -> miette::Result<Vec<String>> {
        let variables: Vec<(&str, &str)> = key
            .into_iter()
            .map(|key| ("METRIC_API_KEY", key))
            .collect();
        added_on(Chain::Base, selection, &variables)
    }

    #[test]
    fn the_cli_selects_disabled_defaults_or_the_listed_venues_once() {
        assert_eq!(PammSelection::from_cli(true, None), PammSelection::Disabled);
        assert_eq!(PammSelection::from_cli(false, None), PammSelection::Defaults);
        assert_eq!(
            PammSelection::from_cli(false, Some(vec![PammVenue::Metric, PammVenue::Metric])),
            PammSelection::Listed(vec![PammVenue::Metric])
        );
    }

    #[test]
    fn a_venue_is_named_metric_on_the_cli_and_by_its_protocol_system_elsewhere() {
        assert_eq!(PammVenue::Metric.cli_name(), "metric");
        assert_eq!(PammVenue::Metric.to_string(), metric::PROTOCOL_SYSTEM);
        assert_eq!(PammVenue::Baibai.cli_name(), "baibai");
        assert_eq!(PammVenue::Baibai.to_string(), baibai::PROTOCOL_SYSTEM);
        assert_eq!(PammVenue::EfficientProp.cli_name(), "efficient-prop");
        assert_eq!(PammVenue::EfficientProp.to_string(), efficient_prop::PROTOCOL_SYSTEM);
        assert_eq!(PammVenue::Flamm.cli_name(), "flamm");
        assert_eq!(PammVenue::Flamm.to_string(), flamm::PROTOCOL_SYSTEM);
    }

    #[tokio::test]
    async fn a_default_venue_without_credentials_is_skipped() {
        assert!(added(&PammSelection::Defaults, None)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_default_venue_whose_feed_fails_to_build_is_skipped() {
        assert!(added(&PammSelection::Defaults, Some(""))
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_listed_venue_without_credentials_fails_the_run() {
        let error = added(&PammSelection::Listed(vec![PammVenue::Metric]), None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("METRIC_API_KEY"), "{message}");
        assert!(message.contains("--pamm"), "{message}");
    }

    #[tokio::test]
    async fn a_listed_venue_whose_feed_fails_to_build_fails_the_run() {
        // Metric refuses an empty trading key when its feed is built.
        let error = added(&PammSelection::Listed(vec![PammVenue::Metric]), Some("")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--pamm"), "{message}");
        assert!(message.contains(metric::PROTOCOL_SYSTEM), "{message}");
    }

    #[tokio::test]
    async fn a_venue_with_credentials_is_added_under_its_protocol_system() {
        for selection in [PammSelection::Defaults, PammSelection::Listed(vec![PammVenue::Metric])] {
            assert_eq!(added(&selection, Some("key")).unwrap(), [metric::PROTOCOL_SYSTEM]);
        }
    }

    #[tokio::test]
    async fn a_listed_venue_that_needs_no_credentials_is_added_without_them() {
        let listed = PammSelection::Listed(vec![PammVenue::Baibai]);
        assert_eq!(added(&listed, None).unwrap(), [baibai::PROTOCOL_SYSTEM]);
    }

    #[tokio::test]
    async fn a_listed_flamm_feed_is_added_when_its_lists_are_set() {
        let listed = PammSelection::Listed(vec![PammVenue::Flamm]);
        assert_eq!(added_on(Chain::Base, &listed, &FLAMM_LISTS).unwrap(), [flamm::PROTOCOL_SYSTEM]);
    }

    #[tokio::test]
    async fn the_flamm_feed_takes_named_pools_in_place_of_its_lists() {
        let listed = PammSelection::Listed(vec![PammVenue::Flamm]);
        let variables = [
            FLAMM_LISTS[0],
            (
                "FLAMM_POOLS",
                "0x4444444444444444444444444444444444444444, \
                 0x5555555555555555555555555555555555555555",
            ),
        ];
        assert_eq!(added_on(Chain::Base, &listed, &variables).unwrap(), [flamm::PROTOCOL_SYSTEM]);
    }

    #[tokio::test]
    async fn the_listed_venues_are_added_together() {
        let listed = PammSelection::Listed(vec![PammVenue::Metric, PammVenue::Baibai]);
        let mut labels = added(&listed, Some("key")).unwrap();
        labels.sort();
        assert_eq!(labels, [baibai::PROTOCOL_SYSTEM, metric::PROTOCOL_SYSTEM]);
    }

    #[tokio::test]
    async fn a_listed_venue_off_its_chain_fails_the_run() {
        let listed = PammSelection::Listed(vec![PammVenue::Baibai]);
        let message = added_on(Chain::Ethereum, &listed, &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("--pamm"), "{message}");
        assert!(message.contains(baibai::PROTOCOL_SYSTEM), "{message}");
    }

    /// The listing doors' deployment, as the environment names it.
    fn efficient_prop_env(name: &str) -> Option<String> {
        match name {
            "EFFICIENT_PROP_FRONT" => Some("0x00000000000000000000000000000000000000f0".into()),
            "EFFICIENT_PROP_FACTORY" => Some("0x00000000000000000000000000000000000000fa".into()),
            "EFFICIENT_PROP_IMPLEMENTATIONS" => Some(
                "0x00000000000000000000000000000000000000a1, \
                 0x00000000000000000000000000000000000000a2"
                    .into(),
            ),
            "EFFICIENT_PROP_PRICE_SOURCES" => Some("1, 2".into()),
            _ => None,
        }
    }

    /// The labels of the feeds `--pamm efficient-prop` adds on Base when the environment holds
    /// `env`'s variables.
    fn efficient_prop_added(env: impl Fn(&str) -> Option<String>) -> miette::Result<Vec<String>> {
        efficient_prop_added_on(Chain::Base, env)
    }

    /// The labels of the feeds `--pamm efficient-prop` adds on `chain` when the environment holds
    /// `env`'s variables.
    fn efficient_prop_added_on(
        chain: Chain,
        env: impl Fn(&str) -> Option<String>,
    ) -> miette::Result<Vec<String>> {
        let book_config =
            BookFeedConfig { chain, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 };
        let mut feeds = BookFeedStreams::new();
        let listed = PammSelection::Listed(vec![PammVenue::EfficientProp]);
        add_pamm_feeds(&listed, &book_config, "http://127.0.0.1:8545", &mut feeds, env)?;
        Ok(feeds
            .labels()
            .map(str::to_string)
            .collect())
    }

    /// The listing doors are not served on Base yet: with a whole deployment in the environment
    /// the feed refuses to build there, which fails the run and says why.
    #[tokio::test]
    async fn the_listing_doors_fail_the_run_on_base_until_it_is_served() {
        let message = efficient_prop_added(efficient_prop_env)
            .unwrap_err()
            .to_string();

        assert!(message.contains("--pamm"), "{message}");
        assert!(message.contains("not served on Base yet"), "{message}");
    }

    #[rstest::rstest]
    #[case::no_front("EFFICIENT_PROP_FRONT", None, "EFFICIENT_PROP_FRONT")]
    #[case::no_allow_list("EFFICIENT_PROP_IMPLEMENTATIONS", None, "EFFICIENT_PROP_IMPLEMENTATIONS")]
    #[case::a_quote_layout_on_base(
        "EFFICIENT_PROP_QUOTE_LAYOUT",
        Some("builder"),
        "not served on Base yet"
    )]
    #[case::a_cell_code_hash_that_is_no_hash(
        "EFFICIENT_PROP_CELL_CODEHASH",
        Some("cell"),
        "not a hash of 32 bytes"
    )]
    #[case::a_factory_that_is_no_address(
        "EFFICIENT_PROP_FACTORY",
        Some("factory"),
        "not an address"
    )]
    #[case::an_approved_desk_that_is_no_address(
        "EFFICIENT_PROP_APPROVED_DESKS",
        Some("0x4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753, desk"),
        "holds desk, not an address"
    )]
    #[case::a_door_code_hash_that_is_no_hash(
        "EFFICIENT_PROP_DOOR_CODEHASH",
        Some("0x1234"),
        "not a hash of 32 bytes"
    )]
    #[case::price_sources_that_are_not_ids(
        "EFFICIENT_PROP_PRICE_SOURCES",
        Some("1, lane"),
        "not a source id"
    )]
    #[tokio::test]
    async fn the_listing_doors_without_their_deployment_fail_the_run(
        #[case] variable: &str,
        #[case] value: Option<&str>,
        #[case] reason: &str,
    ) {
        let env = |name: &str| {
            if name == variable {
                value.map(str::to_string)
            } else {
                efficient_prop_env(name)
            }
        };

        let message = efficient_prop_added(env)
            .unwrap_err()
            .to_string();

        assert!(message.contains("--pamm"), "{message}");
        assert!(message.contains(reason), "{message}");
    }

    /// On Ethereum the listing doors serve the credit listings `EFFICIENT_PROP_CREDIT_LISTINGS`
    /// names with their gas; an entry that is not `listing=four figures`, or five with a dearest
    /// fill before refunds that is at least the dearest of the four and 19,900, fails the run,
    /// and so does any on Base.
    #[rstest::rstest]
    #[case::one_listing(Chain::Ethereum, "1=137909:133427:925544:1035811", None)]
    #[case::its_dearest_fill_named(Chain::Ethereum, "1=137909:133427:925544:1035811:1134823", None)]
    #[case::a_dearest_fill_under_a_charge_and_its_refund(
        Chain::Ethereum,
        "1=137909:133427:925544:1035811:1055710",
        Some("1055710 is under 1055711")
    )]
    #[case::two_listings(
        Chain::Ethereum,
        "1=137909:133427:925544:1035811, 7=187000:192000:610501:610501",
        None
    )]
    #[case::no_gas(Chain::Ethereum, "1", Some("not listing=gas"))]
    #[case::three_figures(Chain::Ethereum, "1=137909:133427:925544", Some("not four gas figures"))]
    #[case::a_listing_that_is_no_id(Chain::Ethereum, "one=1:2:3:4", Some("not a listing id"))]
    #[case::named_twice(Chain::Ethereum, "1=1:2:3:4,1=1:2:3:4", Some("named twice"))]
    #[case::on_base(Chain::Base, "1=137909:133427:925544:1035811", Some("not served on Base yet"))]
    #[tokio::test]
    async fn the_listing_doors_read_their_credit_listings_from_the_environment(
        #[case] chain: Chain,
        #[case] credit: &str,
        #[case] refused: Option<&str>,
    ) {
        let env = |name: &str| match name {
            "EFFICIENT_PROP_CREDIT_LISTINGS" => Some(credit.to_string()),
            "EFFICIENT_PROP_STATE_URLS" if chain == Chain::Ethereum => {
                Some("http://127.0.0.1:29222/v1/1/state".into())
            }
            "EFFICIENT_PROP_CELL_CODEHASH" if chain == Chain::Ethereum => {
                Some(format!("0x{}", "cc".repeat(32)))
            }
            "EFFICIENT_PROP_QUOTE_LAYOUT" if chain == Chain::Ethereum => Some("builder".into()),
            name => efficient_prop_env(name),
        };

        let added = efficient_prop_added_on(chain, env);

        match refused {
            None => assert_eq!(added.unwrap(), [efficient_prop::PROTOCOL_SYSTEM]),
            Some(reason) => {
                let message = added.unwrap_err().to_string();
                assert!(message.contains(reason), "{message}");
            }
        }
    }

    /// On Ethereum the listing doors read the next block's state map from the URLs
    /// `EFFICIENT_PROP_STATE_URLS` names, and fail the run without them; on Base a state URL
    /// fails it.
    #[rstest::rstest]
    #[case::ethereum_with_state_urls(
        Chain::Ethereum,
        Some("http://127.0.0.1:29222/v1/1/state, http://127.0.0.1:29223/v1/1/state"),
        None
    )]
    #[case::ethereum_without(Chain::Ethereum, None, Some("state URL"))]
    #[case::base_with_one(
        Chain::Base,
        Some("http://127.0.0.1:29222/v1/1/state"),
        Some("not served on Base yet")
    )]
    #[tokio::test]
    async fn the_listing_doors_read_their_state_urls_from_the_environment(
        #[case] chain: Chain,
        #[case] state_urls: Option<&str>,
        #[case] refused: Option<&str>,
    ) {
        let env = |name: &str| match name {
            "EFFICIENT_PROP_STATE_URLS" => state_urls.map(str::to_string),
            "EFFICIENT_PROP_CELL_CODEHASH" if chain == Chain::Ethereum => {
                Some(format!("0x{}", "cc".repeat(32)))
            }
            "EFFICIENT_PROP_QUOTE_LAYOUT" if chain == Chain::Ethereum => Some("builder".into()),
            name => efficient_prop_env(name),
        };

        let added = efficient_prop_added_on(chain, env);

        match refused {
            None => assert_eq!(added.unwrap(), [efficient_prop::PROTOCOL_SYSTEM]),
            Some(reason) => {
                let message = added.unwrap_err().to_string();
                assert!(message.contains("--pamm"), "{message}");
                assert!(message.contains(reason), "{message}");
            }
        }
    }

    /// The desk contract code hash, `EFFICIENT_PROP_CELL_CODEHASH`, is required on Ethereum, where
    /// a maker's quote is placed by it, must be 32 bytes, and fails the run on Base. No door code
    /// hash is read from the environment: the feed reads it from the factory.
    #[rstest::rstest]
    #[case::ethereum_with_it(Chain::Ethereum, Some(format!("0x{}", "cc".repeat(32))), None)]
    #[case::ethereum_without(Chain::Ethereum, None, Some("desk contract code hash"))]
    #[case::ethereum_not_32_bytes(Chain::Ethereum, Some("0x1234".to_string()), Some("32 bytes"))]
    #[case::base_with_it(
        Chain::Base,
        Some(format!("0x{}", "cc".repeat(32))),
        Some("not served on Base yet")
    )]
    #[tokio::test]
    async fn the_listing_doors_read_their_desk_code_hash_from_the_environment(
        #[case] chain: Chain,
        #[case] cell_codehash: Option<String>,
        #[case] refused: Option<&str>,
    ) {
        let env = |name: &str| match name {
            "EFFICIENT_PROP_CELL_CODEHASH" => cell_codehash.clone(),
            "EFFICIENT_PROP_QUOTE_LAYOUT" if chain == Chain::Ethereum => Some("builder".into()),
            "EFFICIENT_PROP_STATE_URLS" if chain == Chain::Ethereum => {
                Some("http://127.0.0.1:29222/v1/1/state".to_string())
            }
            name => efficient_prop_env(name),
        };

        let added = efficient_prop_added_on(chain, env);

        match refused {
            None => assert_eq!(added.unwrap(), [efficient_prop::PROTOCOL_SYSTEM]),
            Some(reason) => {
                let message = added.unwrap_err().to_string();
                assert!(message.contains("--pamm"), "{message}");
                assert!(message.contains(reason), "{message}");
            }
        }
    }

    /// The quote layout, `EFFICIENT_PROP_QUOTE_LAYOUT`, is read on Ethereum only, where it must be
    /// `builder`; on Base, which reads no quote contract, it fails the run.
    #[rstest::rstest]
    #[case::ethereum_builder(Chain::Ethereum, Some("builder"), None)]
    #[case::ethereum_without(Chain::Ethereum, None, Some("is not one this integration reads"))]
    #[case::ethereum_another(
        Chain::Ethereum,
        Some("storage"),
        Some("is not one this integration reads")
    )]
    #[case::base_builder(Chain::Base, Some("builder"), Some("not served on Base yet"))]
    #[tokio::test]
    async fn the_listing_doors_read_their_quote_layout_on_ethereum_only(
        #[case] chain: Chain,
        #[case] layout: Option<&str>,
        #[case] refused: Option<&str>,
    ) {
        let env = |name: &str| match name {
            "EFFICIENT_PROP_QUOTE_LAYOUT" => layout.map(str::to_string),
            "EFFICIENT_PROP_CELL_CODEHASH" if chain == Chain::Ethereum => {
                Some(format!("0x{}", "cc".repeat(32)))
            }
            "EFFICIENT_PROP_STATE_URLS" if chain == Chain::Ethereum => {
                Some("http://127.0.0.1:29222/v1/1/state".to_string())
            }
            name => efficient_prop_env(name),
        };

        let added = efficient_prop_added_on(chain, env);

        match refused {
            None => assert_eq!(added.unwrap(), [efficient_prop::PROTOCOL_SYSTEM]),
            Some(reason) => {
                let message = added.unwrap_err().to_string();
                assert!(message.contains("--pamm"), "{message}");
                assert!(message.contains(reason), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn metric_and_the_flamm_feed_are_added_together() {
        let listed = PammSelection::Listed(vec![PammVenue::Metric, PammVenue::Flamm]);
        let mut variables = FLAMM_LISTS.to_vec();
        variables.push(("METRIC_API_KEY", "key"));
        let mut labels = added_on(Chain::Base, &listed, &variables).unwrap();
        labels.sort();
        assert_eq!(labels, [flamm::PROTOCOL_SYSTEM, metric::PROTOCOL_SYSTEM]);
    }

    #[tokio::test]
    async fn the_flamm_feed_is_no_default_venue() {
        let labels = added_on(Chain::Base, &PammSelection::Defaults, &FLAMM_LISTS).unwrap();
        assert!(labels.is_empty(), "{labels:?}");
    }

    #[rstest]
    #[case::no_factory(&FLAMM_LISTS[1..], "factory")]
    #[case::no_hook_class(&[FLAMM_LISTS[0], FLAMM_LISTS[2]], "nothing admits a pool")]
    #[case::no_price_feed(&FLAMM_LISTS[..2], "nothing admits a pool")]
    #[case::a_value_that_does_not_parse(
        &[("FLAMM_FACTORIES", "not hex")],
        "FLAMM_FACTORIES holds not hex"
    )]
    #[case::a_hash_of_the_wrong_length(
        &[FLAMM_LISTS[0], ("FLAMM_HOOK_CLASSES", "0x22"), FLAMM_LISTS[2]],
        "FLAMM_HOOK_CLASSES holds 0x22"
    )]
    #[tokio::test]
    async fn a_listed_flamm_feed_that_cannot_build_fails_the_run(
        #[case] variables: &[(&str, &str)],
        #[case] reason: &str,
    ) {
        let listed = PammSelection::Listed(vec![PammVenue::Flamm]);
        let error = added_on(Chain::Base, &listed, variables).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--pamm lists"), "{message}");
        assert!(message.contains(reason), "{message}");
    }

    #[tokio::test]
    async fn a_listed_flamm_feed_off_its_chains_fails_the_run() {
        let listed = PammSelection::Listed(vec![PammVenue::Flamm]);
        let error = added_on(Chain::Arbitrum, &listed, &FLAMM_LISTS).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Ethereum, Base and BSC"), "{message}");
    }

    #[tokio::test]
    async fn disabling_the_pamm_feeds_adds_nothing() {
        assert!(added(&PammSelection::Disabled, Some("key"))
            .unwrap()
            .is_empty());
    }

    fn secrets<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            set.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn debug_prints_the_rfq_venues_but_not_their_credentials() {
        let selection = RfqSelection::from_cli(
            false,
            secrets(&[("HASHFLOW_USER", "hashflow-user"), ("HASHFLOW_KEY", "hashflow-key")]),
        );

        let printed = format!("{selection:?}");

        assert_eq!(printed, "Credentials([Hashflow])");
    }

    #[test]
    fn the_cli_selects_the_rfq_venues_whose_credentials_are_set() {
        assert_eq!(
            RfqSelection::from_cli(true, secrets(&[("BEBOP_KEY", "key")])),
            RfqSelection::Disabled
        );
        assert_eq!(
            RfqSelection::from_cli(false, secrets(&[])),
            RfqSelection::Credentials(HashMap::new())
        );
        let selection = RfqSelection::from_cli(
            false,
            secrets(&[
                ("BEBOP_KEY", "bebop"),
                ("HASHFLOW_USER", "user"),
                ("NATIVE_API_KEY", "native"),
            ]),
        );
        let RfqSelection::Credentials(credentials) = selection else {
            panic!("RFQ is not disabled");
        };
        let mut venues: Vec<String> = credentials
            .keys()
            .map(ToString::to_string)
            .collect();
        venues.sort();
        // Hashflow needs both its user and its key.
        assert_eq!(venues, [bebop::PROTOCOL_SYSTEM, native::PROTOCOL_SYSTEM]);
    }

    /// Euclid's gateway is public: it runs beside the keyed venues without credentials, and not
    /// at all with RFQ disabled.
    #[test]
    fn the_keyless_venues_run_unless_rfq_is_disabled() {
        let venues = |selection: RfqSelection| -> Vec<RfqVenue> {
            selection
                .venues()
                .into_iter()
                .map(|(venue, ..)| venue)
                .collect()
        };
        assert_eq!(venues(RfqSelection::from_cli(false, secrets(&[]))), [RfqVenue::Euclid]);
        assert_eq!(
            venues(RfqSelection::from_cli(false, secrets(&[("BEBOP_KEY", "key")]))),
            [RfqVenue::Bebop, RfqVenue::Euclid]
        );
        assert!(venues(RfqSelection::Disabled).is_empty());
        // With no credentials, the warning still names the variables to set.
        assert!(RfqSelection::from_cli(false, secrets(&[]))
            .why_empty()
            .contains("BEBOP_KEY"));
    }

    /// Runs the processor in the current-thread test runtime, which polls none of the spawned
    /// feed tasks before the test ends, so no request leaves the machine.
    async fn run(rfq: RfqSelection, pamm: PammSelection) -> Option<JoinHandle<()>> {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        BookStreamProcessor::new(
            Chain::Base,
            0.0,
            1,
            Duration::ZERO,
            rfq,
            pamm,
            "http://127.0.0.1:8545".to_string(),
            false,
        )
        .run_stream(Arc::new(HashMap::new()), sender)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn no_book_stream_starts_without_a_feed() {
        let no_credentials = RfqSelection::Credentials(HashMap::new());
        for (rfq, pamm) in [
            (RfqSelection::Disabled, PammSelection::Disabled),
            (no_credentials.clone(), PammSelection::Disabled),
        ] {
            assert!(run(rfq, pamm).await.is_none());
        }
    }

    #[tokio::test]
    async fn the_book_stream_starts_with_one_rfq_feed_and_the_pamm_feeds_disabled() {
        let credentials = HashMap::from([(RfqVenue::Bebop, (String::new(), "key".to_string()))]);
        let handle = run(RfqSelection::Credentials(credentials), PammSelection::Disabled).await;
        handle
            .expect("the Bebop feed runs")
            .abort();
    }
}
