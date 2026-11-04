use std::{collections::HashMap, env, fmt::Display, str::FromStr, sync::Arc, time::Duration};

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
    pamm::protocols::metric::{self, feed::MetricFeedBuilder},
    rfq::protocols::{
        bebop::{self, feed::BebopFeedBuilder},
        hashflow::{self, feed::HashflowFeedBuilder},
        liquorice::{self, feed::LiquoriceFeedBuilder},
        native::{self, feed::NativeFeedBuilder},
    },
    snapshot_feed::http::HttpFeedConfig,
};
use tycho_test::execution::encoding::USER_ADDR;

use crate::stream_processor::{StreamUpdate, StreamUpdatePayload};

/// The venues that sign binding quotes. The pAMM venues are [`PammVenue`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RfqVenue {
    Bebop,
    Hashflow,
    Liquorice,
    Native,
}

impl Display for RfqVenue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RfqVenue::Bebop => write!(f, "{}", bebop::PROTOCOL_SYSTEM),
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
}

impl PammVenue {
    /// The venues run when `--pamm` is not given.
    pub fn defaults() -> Vec<PammVenue> {
        vec![PammVenue::Metric]
    }

    /// The name `--pamm` takes for this venue.
    fn cli_name(self) -> String {
        self.to_possible_value()
            .map_or_else(|| self.to_string(), |value| value.get_name().to_string())
    }
}

impl Display for PammVenue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PammVenue::Metric => write!(f, "{}", metric::PROTOCOL_SYSTEM),
        }
    }
}

/// Which RFQ venues the book stream runs: each venue whose credentials are set. A venue without
/// them is skipped, since RFQ has no explicit venue list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfqSelection {
    /// `--disable-rfq`: no RFQ venue.
    Disabled,
    /// The venues whose credentials were found, possibly none.
    Credentials(HashMap<RfqVenue, (String, String)>),
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
            info!("Hashflow RFQ credentials not found. Expected environment variables: HASHFLOW_USER, HASHFLOW_KEY");
        }
        if let (Some(user), Some(key)) = (secret("LIQUORICE_USER"), secret("LIQUORICE_KEY")) {
            info!("Liquorice RFQ credentials found");
            credentials.insert(RfqVenue::Liquorice, (user, key));
        } else {
            info!("Liquorice RFQ credentials not found. Expected environment variables: LIQUORICE_USER, LIQUORICE_KEY");
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

    /// Why this selection may have added no feed, for the warning when no book feed runs.
    fn why_empty(&self) -> &'static str {
        match self {
            RfqSelection::Disabled => "RFQ is disabled by --disable-rfq",
            RfqSelection::Credentials(credentials) if credentials.is_empty() => {
                "no RFQ credentials are set (BEBOP_KEY, HASHFLOW_USER and HASHFLOW_KEY, \
                 LIQUORICE_USER and LIQUORICE_KEY, or NATIVE_API_KEY)"
            }
            RfqSelection::Credentials(_) => "no RFQ venue with credentials serves this chain",
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
    /// The selection `--disable-pamm` and `--pamm` describe. Clap rejects the two together.
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

    /// Why this selection may have added no feed, for the warning when no book feed runs.
    fn why_empty(&self) -> &'static str {
        match self {
            PammSelection::Disabled => "pAMM is disabled by --disable-pamm",
            PammSelection::Defaults => "no default pAMM venue could run (see the log above)",
            PammSelection::Listed(_) => "--pamm lists no venue",
        }
    }
}

pub struct BookStreamProcessor {
    chain: Chain,
    tvl_threshold: f64,
    rfq: RfqSelection,
    sample_size: usize,
    pamm: PammSelection,
    /// The protocol's stream will skip messages for this duration after processing a message
    skip_messages_duration: Duration,
    /// Whether each venue publishes one component for all of its pairs rather than one each.
    all_pairs: bool,
}

impl BookStreamProcessor {
    pub fn new(
        chain: Chain,
        tvl_threshold: f64,
        sample_size: usize,
        skip_messages_duration: Duration,
        rfq: RfqSelection,
        pamm: PammSelection,
        all_pairs: bool,
    ) -> Self {
        Self { chain, tvl_threshold, rfq, sample_size, pamm, skip_messages_duration, all_pairs }
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

        add_pamm_feeds(&self.pamm, &book_config, &mut feeds, |name| env::var(name).ok())?;

        let no_credentials = HashMap::new();
        let credentials = match &self.rfq {
            RfqSelection::Disabled => &no_credentials,
            RfqSelection::Credentials(credentials) => credentials,
        };
        for (protocol, (user, key)) in credentials {
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
                        key.clone(),
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
                        user.clone(),
                        key.clone(),
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
                        user.clone(),
                        key.clone(),
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
                RfqVenue::Native => {
                    // Native serves a fixed set of chains; on any other chain the feed is
                    // skipped rather than failing the whole processor.
                    let mut builder = NativeFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        key.clone(),
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
                (rfq, pamm) => {
                    warn!("No book feed is running: {}; {}", rfq.why_empty(), pamm.why_empty());
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

                // Sample random RFQ quotes from the complete book
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

/// Adds the feed of each venue `selection` names. `secret` looks up an environment variable.
///
/// # Errors
/// Returns an error when a venue listed in `--pamm` lacks its credentials or its feed fails to
/// build. A default venue that cannot run is logged and skipped instead.
fn add_pamm_feeds(
    selection: &PammSelection,
    book_config: &BookFeedConfig,
    feeds: &mut BookFeedStreams,
    secret: impl Fn(&str) -> Option<String>,
) -> miette::Result<()> {
    let (venues, listed) = match selection {
        PammSelection::Disabled => return Ok(()),
        PammSelection::Defaults => (PammVenue::defaults(), false),
        PammSelection::Listed(venues) => (venues.clone(), true),
    };
    for venue in venues {
        match venue {
            PammVenue::Metric => {
                let Some(api_key) = secret("METRIC_API_KEY") else {
                    if listed {
                        return Err(miette!(
                            "--pamm lists {}, but METRIC_API_KEY is not set. Set it, or drop {} \
                             from --pamm.",
                            venue.cli_name(),
                            venue.cli_name()
                        ));
                    }
                    info!(
                        "Metric credentials not found. Expected environment variable: METRIC_API_KEY"
                    );
                    continue;
                };
                // Metric's own defaults already set its cadence and withdrawal age.
                match MetricFeedBuilder::new(book_config.clone(), api_key).build() {
                    Ok(metric_feed) => {
                        info!("Adding {venue} feed...");
                        feeds
                            .add(venue.to_string(), metric_feed)
                            .map_err(|_feed| miette!("The {venue} feed was added twice"))?;
                    }
                    Err(e) if listed => {
                        return Err(miette!(
                            "Failed to build the {venue} feed that --pamm lists: {e}"
                        ));
                    }
                    Err(e) => warn!("Failed to build Metric feed, skipping: {e}"),
                }
            }
        }
    }
    Ok(())
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
    use super::*;

    fn book_config() -> BookFeedConfig {
        BookFeedConfig { chain: Chain::Base, tokens: Arc::new(HashMap::new()), min_tvl_usd: 0.0 }
    }

    /// The labels of the feeds `selection` adds when `METRIC_API_KEY` holds `key`.
    fn added(selection: &PammSelection, key: Option<&str>) -> miette::Result<Vec<String>> {
        let mut feeds = BookFeedStreams::new();
        add_pamm_feeds(selection, &book_config(), &mut feeds, |name| {
            key.filter(|_| name == "METRIC_API_KEY")
                .map(str::to_string)
        })?;
        Ok(feeds
            .labels()
            .map(str::to_string)
            .collect())
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
        let error = added(&PammSelection::Listed(vec![PammVenue::Metric]), Some("")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("trading key is empty"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_venue_with_credentials_is_added_under_its_protocol_system() {
        for selection in [PammSelection::Defaults, PammSelection::Listed(vec![PammVenue::Metric])] {
            assert_eq!(added(&selection, Some("key")).unwrap(), [metric::PROTOCOL_SYSTEM]);
        }
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

    async fn run(rfq: RfqSelection, pamm: PammSelection) -> Option<JoinHandle<()>> {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        BookStreamProcessor::new(Chain::Base, 0.0, 1, Duration::ZERO, rfq, pamm, false)
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
            (RfqSelection::Disabled, PammSelection::Listed(Vec::new())),
            (no_credentials, PammSelection::Listed(Vec::new())),
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
