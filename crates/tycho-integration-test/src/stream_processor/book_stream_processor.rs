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

/// The venues that sign binding quotes. pAMMs are configured separately.
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
    /// Whether each venue publishes one component for all of its pairs rather than one each.
    all_pairs: bool,
    pamm_rpc: Option<(String, Option<String>, bool)>,
}

impl BookStreamProcessor {
    pub fn new(
        chain: Chain,
        tvl_threshold: f64,
        sample_size: usize,
        skip_messages_duration: Duration,
        rfq_feeds: bool,
        pamm_feeds: bool,
        all_pairs: bool,
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
                return Err(miette!("No RFQ credentials found. Please set BEBOP_KEY, HASHFLOW_USER and HASHFLOW_KEY, LIQUORICE_USER and LIQUORICE_KEY, or NATIVE_API_KEY environment variables, or drop --disable-pamm to run the pAMM feeds."));
            }
        }
        Ok(Self {
            chain,
            tvl_threshold,
            credentials,
            sample_size,
            pamm_feeds,
            skip_messages_duration,
            all_pairs,
            pamm_rpc: None,
        })
    }

    pub fn with_pamm_rpc(
        mut self,
        rpc_url: String,
        ws_rpc_url: Option<String>,
        polling: bool,
    ) -> Self {
        self.pamm_rpc = Some((rpc_url, ws_rpc_url, polling));
        self
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

        // RFQ feeds price TVL through these stablecoins; pAMMs handle their own TVL.
        let usd_quote_tokens = usd_stablecoins_for_chain(self.chain).map(Arc::new);

        let mut feeds = BookFeedStreams::new();

        if self.pamm_feeds {
            if matches!(self.chain, Chain::Base | Chain::Bsc | Chain::Robinhood) {
                use tycho_simulation::pamm::protocols::elfomofi::{
                    self, feed::ElfomofiFeedBuilder,
                };

                if let Some((rpc_url, ws_rpc_url, polling)) = &self.pamm_rpc {
                    let mut builder =
                        ElfomofiFeedBuilder::new(book_config.clone(), rpc_url.clone());
                    if *polling {
                        builder = builder.polling();
                    } else if let Some(ws_rpc_url) = ws_rpc_url {
                        builder = builder.ws_rpc_url(ws_rpc_url.clone());
                    }
                    match builder.build() {
                        Ok(feed) => {
                            info!("Adding {} feed...", elfomofi::PROTOCOL_SYSTEM);
                            feeds
                                .add(elfomofi::PROTOCOL_SYSTEM, feed)
                                .expect("each provider is added once");
                        }
                        Err(error) => warn!("Skipping ElFomoFi feed: {error}"),
                    }
                }
            }
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
                        warn!("Failed to build Metric feed, skipping: {e}");
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

                let books = Arc::new(sample_books(&books, sample_size));

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
        Ok(handle)
    }
}

fn sample_books(
    books: &HashMap<String, tycho_simulation::book::Book>,
    sample_size: usize,
) -> HashMap<String, tycho_simulation::book::Book> {
    books
        .iter()
        // A book explicitly advertising no swap direction cannot produce a simulation.
        .filter(|(_, book)| {
            book.component
                .static_attributes
                .get(tycho_simulation::book::SWAP_DIRECTIONS_ATTRIBUTE)
                .is_none_or(|directions| !directions.is_empty())
        })
        .choose_multiple(&mut rand::rng(), sample_size)
        .into_iter()
        .map(|(id, book)| (id.clone(), book.clone()))
        .collect()
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
    use alloy::primitives::U256;
    use tycho_simulation::{
        book::{Book, SWAP_DIRECTIONS_ATTRIBUTE},
        evm::protocol::uniswap_v2::state::UniswapV2State,
        protocol::models::ProtocolComponent,
    };

    use super::*;

    #[test]
    fn empty_books_do_not_spend_the_simulation_sample() {
        let token =
            |byte| Token::new(&Bytes::from(vec![byte; 20]), "T", 18, 0, &[], Chain::Base, 100);
        let active = Book {
            component: ProtocolComponent::new(
                Bytes::from(vec![1; 20]),
                "test:book".into(),
                "book_pool".into(),
                Chain::Base,
                vec![token(1), token(2)],
                vec![],
                HashMap::new(),
                Bytes::default(),
                Default::default(),
            ),
            state: Arc::new(UniswapV2State::new(U256::from(1000), U256::from(1000))),
            updated_at: None,
        };
        let mut books = HashMap::from([("active".to_string(), active.clone())]);
        for n in 0..20 {
            let mut empty = active.clone();
            empty
                .component
                .static_attributes
                .insert(SWAP_DIRECTIONS_ATTRIBUTE.to_string(), Bytes::default());
            books.insert(format!("empty{n}"), empty);
        }
        let sample = sample_books(&books, 1);
        assert_eq!(sample.len(), 1);
        assert!(sample.contains_key("active"));
    }
}
