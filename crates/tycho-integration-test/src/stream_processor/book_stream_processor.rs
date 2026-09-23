use std::{collections::HashMap, env, fmt::Display, str::FromStr, sync::Arc, time::Duration};

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
        quote_tokens::usd_stablecoins_for_chain, BookFeedConfig, BookFeedEvent, BookFeedStreams,
        BookSnapshot,
    },
    pamm::protocols::metric::{self, feed::MetricFeedBuilder},
    rfq::protocols::{
        bebop::{self, feed::BebopFeedBuilder},
        hashflow::{self, feed::HashflowFeedBuilder},
        liquorice::{self, feed::LiquoriceFeedBuilder},
        native::{self, feed::NativeFeedBuilder},
    },
    snapshot_feed::{http::HttpFeedConfig, SnapshotFeedOutcome},
};
use tycho_test::execution::encoding::USER_ADDR;

use crate::stream_processor::{StreamUpdate, StreamUpdatePayload};

/// The venues that take credentials, which is every one that signs binding quotes. Metric
/// is the pAMM of the family and is configured on its own.
#[derive(Debug, PartialEq, Eq, Hash)]
enum RfqVenue {
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

pub struct BookStreamProcessor {
    chain: Chain,
    tvl_threshold: f64,
    credentials: HashMap<RfqVenue, (String, String)>,
    sample_size: usize,
    pamm_feeds: bool,
    /// The protocol's stream will skip messages for this duration after processing a message
    skip_messages_duration: Duration,
}

impl BookStreamProcessor {
    pub fn new(
        chain: Chain,
        tvl_threshold: f64,
        sample_size: usize,
        skip_messages_duration: Duration,
        rfq_feeds: bool,
        pamm_feeds: bool,
    ) -> miette::Result<Self> {
        let mut credentials = HashMap::new();
        if rfq_feeds {
            if let Ok(key) = env::var("BEBOP_KEY") {
                info!("Bebop RFQ credentials found");
                credentials.insert(RfqVenue::Bebop, (String::new(), key));
            } else {
                info!("Bebop RFQ credentials not found. Expected environment variable: BEBOP_KEY");
            }
            let (hashflow_user, hashflow_key) =
                (env::var("HASHFLOW_USER").ok(), env::var("HASHFLOW_KEY").ok());
            if let (Some(user), Some(key)) = (hashflow_user, hashflow_key) {
                info!("Hashflow RFQ credentials found");
                credentials.insert(RfqVenue::Hashflow, (user, key));
            } else {
                info!("Hashflow RFQ credentials not found. Expected environment variables: HASHFLOW_USER, HASHFLOW_KEY");
            }
            let (liquorice_user, liquorice_key) =
                (env::var("LIQUORICE_USER").ok(), env::var("LIQUORICE_KEY").ok());
            if let (Some(user), Some(key)) = (liquorice_user, liquorice_key) {
                info!("Liquorice RFQ credentials found");
                credentials.insert(RfqVenue::Liquorice, (user, key));
            } else {
                info!("Liquorice RFQ credentials not found. Expected environment variables: LIQUORICE_USER, LIQUORICE_KEY");
            }
            if let Ok(key) = env::var("NATIVE_API_KEY") {
                info!("Native RFQ credentials found");
                credentials.insert(RfqVenue::Native, (String::new(), key));
            } else {
                info!(
                    "Native RFQ credentials not found. Expected environment variable: NATIVE_API_KEY"
                );
            }
        }

        if credentials.is_empty() {
            if pamm_feeds {
                info!("No RFQ venue is configured. Continuing with the pAMM feeds only.");
            } else {
                return Err(miette!("No RFQ credentials found. Please set BEBOP_KEY, HASHFLOW_USER and HASHFLOW_KEY, LIQUORICE_USER and LIQUORICE_KEY, or NATIVE_API_KEY environment variables, or drop --disable-pamm-feeds to run Metric on its own."));
            }
        }
        Ok(Self {
            chain,
            tvl_threshold,
            credentials,
            sample_size,
            pamm_feeds,
            skip_messages_duration,
        })
    }

    pub async fn run_stream(
        &self,
        all_tokens: Arc<HashMap<Bytes, Token>>,
        stream_tx: Sender<miette::Result<StreamUpdate>>,
    ) -> miette::Result<JoinHandle<()>> {
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

        if self.pamm_feeds {
            match env::var("METRIC_API_KEY") {
                // Metric's own defaults already set its cadence and withdrawal age.
                Ok(api_key) => match MetricFeedBuilder::new(book_config.clone(), api_key).build() {
                    Ok(metric_feed) => {
                        info!("Adding {} feed...", metric::PROTOCOL_SYSTEM);
                        feeds
                            .add(metric::PROTOCOL_SYSTEM, metric_feed)
                            .expect("each provider is added once");
                    }
                    Err(e) => {
                        warn!("Metric not supported on chain {:?}, skipping: {e}", self.chain);
                    }
                },
                Err(_) => {
                    info!("Metric credentials not found. Expected environment variable: METRIC_API_KEY");
                }
            }
        }

        for (protocol, (user, key)) in &self.credentials {
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
                    let hashflow_feed = HashflowFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        user.clone(),
                        key.clone(),
                    )
                    .feed_config(HttpFeedConfig {
                        poll_interval: Duration::from_secs(30),
                        max_snapshot_age: Some(Duration::from_secs(90)),
                        ..HashflowFeedBuilder::default_feed_config()
                    })
                    .build()
                    .into_diagnostic()
                    .wrap_err("Failed to create Hashflow feed")?;

                    feeds
                        .add(hashflow::PROTOCOL_SYSTEM, hashflow_feed)
                        .expect("each provider is added once");
                }
                RfqVenue::Liquorice => {
                    let liquorice_feed = LiquoriceFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        user.clone(),
                        key.clone(),
                    )
                    .feed_config(HttpFeedConfig {
                        poll_interval: Duration::from_secs(30),
                        max_snapshot_age: Some(Duration::from_secs(90)),
                        ..LiquoriceFeedBuilder::default_feed_config()
                    })
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
                    match NativeFeedBuilder::new(
                        book_config.clone(),
                        Arc::clone(usd_quote_tokens),
                        key.clone(),
                    )
                    .feed_config(HttpFeedConfig {
                        poll_interval: Duration::from_secs(30),
                        max_snapshot_age: Some(Duration::from_secs(90)),
                        ..NativeFeedBuilder::default_feed_config()
                    })
                    .build()
                    {
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

        let mut is_first_update = true;
        let sample_size = self.sample_size;
        let skip_messages_duration = self.skip_messages_duration;
        let mut next_stream_times: HashMap<String, tokio::time::Instant> = HashMap::new();

        let handle = tokio::spawn(async move {
            info!("RFQ stream processor started");
            while let Some((protocol_system, event)) = feeds.next().await {
                let snapshot = match event {
                    BookFeedEvent::Published(snapshot) => Ok(snapshot),
                    BookFeedEvent::Withdrawn => {
                        warn!("{protocol_system} withdrew its books, nothing servable from it");
                        continue;
                    }
                    BookFeedEvent::Ended(SnapshotFeedOutcome::Failed(error)) => {
                        Err(miette!(error).wrap_err(format!("{protocol_system} gave up")))
                    }
                    BookFeedEvent::Ended(SnapshotFeedOutcome::Panicked(error)) => {
                        Err(miette!(error).wrap_err(format!("{protocol_system} feed task died")))
                    }
                    BookFeedEvent::Ended(SnapshotFeedOutcome::RanOut) => {
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
            // Either every feed terminated (each already reported above) or the receiver
            // dropped; dropping the JoinSet aborts any feed task still running.
            info!("RFQ stream processor stopping");
        });
        Ok(handle)
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
