use std::{
    collections::{HashMap, HashSet},
    env,
    fmt::Display,
    time::Duration,
};

use miette::{miette, IntoDiagnostic, WrapErr};
use rand::prelude::IteratorRandom;
use tokio::{sync::mpsc::Sender, task::JoinHandle};
use tracing::{info, warn};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_simulation::rfq::{
    protocols::{
        bebop::{client::BebopClient, client_builder::BebopClientBuilder, state::BebopState},
        hashflow::{
            client::HashflowClient, client_builder::HashflowClientBuilder, state::HashflowState,
        },
        liquorice::{
            client::LiquoriceClient, client_builder::LiquoriceClientBuilder, state::LiquoriceState,
        },
        metric::{client::MetricClient, client_builder::MetricClientBuilder, state::MetricState},
    },
    stream::RFQStreamBuilder,
};

use crate::stream_processor::{StreamUpdate, UpdateType};

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum RFQProtocol {
    Bebop,
    Hashflow,
    Liquorice,
    Metric,
}

impl Display for RFQProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RFQProtocol::Bebop => write!(f, "{}", BebopClient::PROTOCOL_SYSTEM),
            RFQProtocol::Hashflow => write!(f, "{}", HashflowClient::PROTOCOL_SYSTEM),
            RFQProtocol::Liquorice => write!(f, "{}", LiquoriceClient::PROTOCOL_SYSTEM),
            RFQProtocol::Metric => write!(f, "{}", MetricClient::PROTOCOL_SYSTEM),
        }
    }
}

pub struct RFQStreamProcessor {
    chain: Chain,
    tvl_threshold: f64,
    rfq_credentials: HashMap<RFQProtocol, (String, String)>,
    sample_size: usize,
    run_pamm_protocols: bool,
    /// The protocol's stream will skip messages for this duration after processing a message
    skip_messages_duration: Duration,
}

impl RFQStreamProcessor {
    pub fn new(
        chain: Chain,
        tvl_threshold: f64,
        sample_size: usize,
        skip_messages_duration: Duration,
        run_pamm_protocols: bool,
    ) -> miette::Result<Self> {
        let mut rfq_credentials = HashMap::new();
        let (bebop_user, bebop_key) = (env::var("BEBOP_USER").ok(), env::var("BEBOP_KEY").ok());
        if let (Some(user), Some(key)) = (bebop_user, bebop_key) {
            info!("Bebop RFQ credentials found");
            rfq_credentials.insert(RFQProtocol::Bebop, (user, key));
        } else {
            info!("Bebop RFQ credentials not found. Expected environment variables: BEBOP_USER, BEBOP_KEY");
        }
        let (hashflow_user, hashflow_key) =
            (env::var("HASHFLOW_USER").ok(), env::var("HASHFLOW_KEY").ok());
        if let (Some(user), Some(key)) = (hashflow_user, hashflow_key) {
            info!("Hashflow RFQ credentials found");
            rfq_credentials.insert(RFQProtocol::Hashflow, (user, key));
        } else {
            info!("Hashflow RFQ credentials not found. Expected environment variables: HASHFLOW_USER, HASHFLOW_KEY");
        }
        let (liquorice_user, liquorice_key) =
            (env::var("LIQUORICE_USER").ok(), env::var("LIQUORICE_KEY").ok());
        if let (Some(user), Some(key)) = (liquorice_user, liquorice_key) {
            info!("Liquorice RFQ credentials found");
            rfq_credentials.insert(RFQProtocol::Liquorice, (user, key));
        } else {
            info!("Liquorice RFQ credentials not found. Expected environment variables: LIQUORICE_USER, LIQUORICE_KEY");
        }

        if rfq_credentials.is_empty() {
            if run_pamm_protocols {
                info!(
                    "No authenticated RFQ credentials found. Continuing with PAMM RFQ protocols only."
                );
            } else {
                return Err(miette!("No RFQ credentials found. Please set BEBOP_USER and BEBOP_KEY, HASHFLOW_USER and HASHFLOW_KEY, or LIQUORICE_USER and LIQUORICE_KEY environment variables. To run PAMM RFQ protocols, pass --run-pamm-protocols."));
            }
        }
        Ok(Self {
            chain,
            tvl_threshold,
            rfq_credentials,
            sample_size,
            run_pamm_protocols,
            skip_messages_duration,
        })
    }

    pub async fn run_stream(
        &self,
        all_tokens: &HashMap<Bytes, Token>,
        stream_tx: Sender<miette::Result<StreamUpdate>>,
    ) -> miette::Result<JoinHandle<()>> {
        info!("Starting RFQ stream processor for chain {:?}", self.chain);
        // Set up RFQ stream
        let rfq_tokens: HashSet<Bytes> = all_tokens.keys().cloned().collect();
        let mut rfq_stream_builder = RFQStreamBuilder::new()
            .set_tokens(all_tokens.clone())
            .await;
        let metric_enabled = if self.run_pamm_protocols {
            match MetricClientBuilder::new(self.chain)
                .tokens(rfq_tokens.clone())
                .token_metadata(all_tokens.clone())
                .tvl_threshold(self.tvl_threshold)
                .poll_time(Duration::from_secs(30))
                .build()
            {
                Ok(metric_client) => {
                    info!("Adding {} RFQ client...", RFQProtocol::Metric);
                    rfq_stream_builder = rfq_stream_builder
                        .add_client::<MetricState>("metric", Box::new(metric_client));
                    true
                }
                Err(e) => {
                    return Err(miette!(
                        "No PAMM RFQ client could be added with --run-pamm-protocols: {e}"
                    ));
                }
            }
        } else {
            false
        };

        for (protocol, (user, key)) in &self.rfq_credentials {
            info!("Adding {protocol} RFQ client...");
            match protocol {
                RFQProtocol::Bebop => {
                    let bebop_client =
                        BebopClientBuilder::new(self.chain, user.clone(), key.clone())
                            .tokens(rfq_tokens.clone())
                            .tvl_threshold(self.tvl_threshold)
                            .build()
                            .into_diagnostic()
                            .wrap_err("Failed to create Bebop RFQ client")?;
                    rfq_stream_builder = rfq_stream_builder
                        .add_client::<BebopState>("bebop", Box::new(bebop_client));
                }
                RFQProtocol::Hashflow => {
                    let hashflow_client =
                        HashflowClientBuilder::new(self.chain, user.clone(), key.clone())
                            .tokens(rfq_tokens.clone())
                            .tvl_threshold(self.tvl_threshold)
                            .poll_time(Duration::from_secs(30))
                            .build()
                            .into_diagnostic()
                            .wrap_err("Failed to create Hashflow RFQ client")?;
                    rfq_stream_builder = rfq_stream_builder
                        .add_client::<HashflowState>("hashflow", Box::new(hashflow_client))
                }
                RFQProtocol::Liquorice => {
                    let liquorice_client =
                        LiquoriceClientBuilder::new(self.chain, user.clone(), key.clone())
                            .tokens(rfq_tokens.clone())
                            .tvl_threshold(self.tvl_threshold)
                            .poll_time(Duration::from_secs(30))
                            .build()
                            .into_diagnostic()
                            .wrap_err("Failed to create Liquorice RFQ client")?;
                    rfq_stream_builder = rfq_stream_builder
                        .add_client::<LiquoriceState>("liquorice", Box::new(liquorice_client))
                }
                RFQProtocol::Metric => unreachable!("Metric RFQ does not use credential storage"),
            }
        }

        // Start the RFQ stream
        let mut is_first_update = true;
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let _handle = tokio::spawn(rfq_stream_builder.build(tx));
        let sample_size = self.sample_size;
        let skip_messages_duration = self.skip_messages_duration;
        let mut next_stream_times: HashMap<String, tokio::time::Instant> = self
            .rfq_credentials
            .keys()
            .map(|protocol| (protocol.to_string(), tokio::time::Instant::now()))
            .collect();
        if metric_enabled {
            next_stream_times.insert(RFQProtocol::Metric.to_string(), tokio::time::Instant::now());
        }
        let handle = tokio::spawn(async move {
            info!("RFQ stream processor started");
            while let Some(mut update) = rx.recv().await {
                // Handle throttling for the update's protocol
                if let Some((_, component)) = update.new_pairs.iter().next() {
                    let next_stream_time =
                        if let Some(t) = next_stream_times.get_mut(&component.protocol_system) {
                            t
                        } else {
                            if stream_tx
                                .send(Err(miette!(
                                    "Protocol system not configured: {}",
                                    component.protocol_system
                                )))
                                .await
                                .is_err()
                            {
                                warn!("Receiver dropped, stopping stream processor");
                                _handle.abort();
                                break;
                            }
                            continue;
                        };
                    let now = tokio::time::Instant::now();
                    if now < *next_stream_time {
                        continue;
                    } else {
                        *next_stream_time = now + skip_messages_duration;
                    }
                } else {
                    continue;
                };

                // Sample random RFQ quotes
                update.states = update
                    .states
                    .into_iter()
                    .choose_multiple(&mut rand::rng(), sample_size)
                    .into_iter()
                    .collect();
                update
                    .new_pairs
                    .retain(|key, _| update.states.contains_key(key));

                let received_at =
                    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                        Ok(duration) => duration,
                        Err(e) => {
                            if stream_tx
                                .send(Err(miette!(e).wrap_err("Error getting current timestamp")))
                                .await
                                .is_err()
                            {
                                warn!("Receiver dropped, stopping stream processor");
                                break;
                            }
                            continue;
                        }
                    };

                // Send the latest update
                let update = StreamUpdate {
                    update_type: UpdateType::Rfq,
                    update,
                    is_first_update,
                    received_at,
                };
                if is_first_update {
                    is_first_update = false;
                }
                if stream_tx
                    .send(Ok(update))
                    .await
                    .is_err()
                {
                    warn!("Receiver dropped, stopping stream processor");
                    _handle.abort();
                    break;
                }
            }
        });
        Ok(handle)
    }
}
