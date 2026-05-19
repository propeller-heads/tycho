mod metrics;
mod statistics;
mod stream_processor;

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::Address,
    providers::Provider,
    rpc::types::{Block, BlockId},
};
use clap::Parser;
use dotenv::dotenv;
use itertools::Itertools;
use miette::{miette, IntoDiagnostic, NarratableReportHandler, WrapErr};
use num_bigint::BigUint;
use num_traits::{Pow, ToPrimitive, Zero};
use rand::prelude::IndexedRandom;
use tokio::{signal, sync::Semaphore};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use tycho_client::feed::SynchronizerState;
use tycho_common::simulation::protocol_sim::ProtocolSim;
use tycho_simulation::{
    evm::protocol::cowamm::constants::PROTOCOL_SYSTEM as COWAMM_PROTOCOL_SYSTEM,
    protocol::models::ProtocolComponent,
    rfq::protocols::{
        hashflow::{client::HashflowClient, state::HashflowState},
        liquorice::{client::LiquoriceClient, state::LiquoriceState},
    },
    tycho_common::models::Chain,
    utils::load_all_tokens,
};
use tycho_test::{
    execution::{
        encoding::encode_swap,
        models::{TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction, tenderly,
    },
    validation::{batch_validate_components, get_validator, Validator},
};

use crate::{
    statistics::TestStatistics,
    stream_processor::{
        protocol_stream_processor::ProtocolStreamProcessor,
        rfq_stream_processor::RFQStreamProcessor, StreamUpdate, UpdateType,
    },
};

#[derive(Parser, Clone)]
struct Cli {
    /// The tvl threshold in ETH/native token units to filter the graph by
    #[arg(long, default_value_t = 100.0)]
    tvl_threshold: f64,

    #[arg(long, default_value = "ethereum")]
    chain: Chain,

    #[arg(
        long,
        env = "TYCHO_API_KEY",
        hide_env_values = true,
        default_value = "sampletoken",
        hide_default_value = true
    )]
    tycho_api_key: String,

    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    #[arg(long, env = "RPC_URL")]
    rpc_url: String,

    /// Disable on-chain protocols
    #[arg(long, default_value_t = false)]
    disable_onchain: bool,

    /// Disable RFQ protocols
    #[arg(long, default_value_t = false)]
    disable_rfq: bool,

    /// Port for the Prometheus metrics server
    #[arg(long, default_value_t = 9898)]
    metrics_port: u16,

    /// Maximum number of updates to process in parallel.
    /// Set to 1 to process sequentially.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..))]
    parallel_updates: u8,

    /// Maximum number of simulations to run in parallel
    /// Set to 1 to process sequentially.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..))]
    parallel_simulations: u8,

    /// Maximum number of simulations (of updated states) to run per update
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
    max_simulations: u16,

    /// Maximum number of simulations (of stale states) to run per update per protocol
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
    max_simulations_stale: u16,

    /// The RFQ stream will skip messages for this duration (in seconds) after processing a message
    #[arg(long, default_value_t = 600)]
    skip_messages_duration: u64,

    /// Maximum number of attempts to poll the RPC when the update block is ahead of the RPC.
    /// Each attempt is separated by --rpc-poll-interval-ms. Adjust for faster chains (e.g. Base,
    /// Unichain) where blocks arrive more frequently.
    #[arg(long, default_value_t = 10)]
    rpc_poll_attempts: u32,

    /// Interval in milliseconds between RPC polling attempts when waiting for the RPC to reach
    /// the update block number.
    #[arg(long, default_value_t = 500)]
    rpc_poll_interval_ms: u64,

    /// List of component IDs to always include in tests every block if not already selected
    #[arg(long, value_delimiter = ',')]
    always_test_components: Vec<String>,

    /// List of protocols to enable (e.g., uniswap_v2,curve,balancer_v2)
    /// If not provided, defaults to chain-specific protocols
    #[arg(long, value_delimiter = ',')]
    protocols: Option<Vec<String>>,

    /// Maximum number of blocks to process before exiting (0 = run indefinitely)
    #[arg(long, default_value_t = 0)]
    max_blocks: u64,

    /// Ratio used to define the lower bound of the TVL filter for hysteresis.
    /// Components are added when TVL >= tvl_threshold and removed when TVL drops below
    /// tvl_threshold / tvl_buffer_ratio.
    #[arg(long, default_value_t = 1.1)]
    tvl_buffer_ratio: f64,

    /// Minimum token quality to filter by (defaults to 100 if not provided)
    #[arg(long)]
    min_token_quality: Option<i32>,

    /// Maximum number of days since a token was last traded (chain-specific defaults if not
    /// provided)
    #[arg(long)]
    max_days_since_last_trade: Option<u64>,

    /// Enable partial block updates (flashblocks) on the tycho stream.
    /// Be aware this significantly increases the frequency of simulations. You may need to
    /// consider adjusting the max-simulations and max-simulations-stale values.
    #[arg(long, default_value_t = false)]
    partial_blocks: bool,

    /// Seconds without a protocol update before marking all known protocols as stale in metrics.
    #[arg(long, default_value_t = 30)]
    stale_threshold_secs: u64,

    /// Router fee on output in bps (defaults to 10 bps)
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
    router_fee: u16,
}

impl Debug for Cli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cli")
            .field("tvl_threshold", &self.tvl_threshold)
            .field("chain", &self.chain)
            .field("tycho_api_key", &"****")
            .field("tycho_url", &self.tycho_url)
            .field("rpc_url", &self.rpc_url)
            .field("metrics_port", &self.metrics_port)
            .finish()
    }
}

#[derive(Default)]
struct TychoState {
    states: HashMap<String, Box<dyn ProtocolSim>>,
    components: HashMap<String, ProtocolComponent>,
    component_ids_by_protocol: HashMap<String, HashSet<String>>,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    miette::set_hook(Box::new(|_| Box::new(NarratableReportHandler::new())))?;
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();

    // Initialize and start Prometheus metrics
    metrics::initialize_metrics();
    let metrics_task = metrics::create_metrics_exporter(cli.metrics_port).await?;

    // Start metrics server in background
    let _metrics_handle = tokio::spawn(async move {
        if let Err(e) = metrics_task
            .await
            .into_diagnostic()
            .wrap_err("Metrics server task failed")
        {
            warn!("Metrics server error: {}", e);
        }
    });

    // Set up signal handling for graceful shutdown
    let shutdown_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_flag = shutdown_requested.clone();

    tokio::spawn(async move {
        match signal::ctrl_c().await {
            Ok(()) => {
                info!("Received Ctrl+C, initiating graceful shutdown");
                shutdown_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Err(err) => {
                error!("Unable to listen for shutdown signal: {}", err);
            }
        }
    });

    // Run main application with signal support
    let result = tokio::select! {
        result = tokio::spawn(run(cli)) => match result {
          Ok(inner) => inner,
          Err(e) => Err(miette!("run() panicked: {:?}", e)),
        },
        _ = async {
            loop {
                if shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } => {
            info!("Application interrupted by signal");
            Ok(())
        }
    };

    // Force exit to prevent hanging on metrics server thread
    match result {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> miette::Result<()> {
    info!("Starting integration test");

    let cli = Arc::new(cli);
    let chain = cli.chain;

    let rpc_tools = tycho_test::RPCTools::new(&cli.rpc_url, &chain).await?;

    // Load tokens from Tycho
    info!(%cli.tycho_url, "Loading tokens...");
    let all_tokens = load_all_tokens(
        &cli.tycho_url,
        false,
        Some(cli.tycho_api_key.as_str()),
        true,
        chain,
        cli.min_token_quality,
        cli.max_days_since_last_trade,
    )
    .await
    .map_err(|e| miette!("Failed to load tokens: {e:?}"))?;
    info!("Loaded {} tokens", all_tokens.len());

    // Run streams in background tasks with separate channels so RFQ processing
    // cannot block protocol update consumption
    let (protocol_tx, mut protocol_rx) =
        tokio::sync::mpsc::channel::<miette::Result<StreamUpdate>>(64);
    let (rfq_tx, mut rfq_rx) = tokio::sync::mpsc::channel::<miette::Result<StreamUpdate>>(64);
    let mut protocol_handle = None;
    let mut rfq_handle = None;

    if !cli.disable_onchain {
        if let Ok(protocol_stream_processor) = ProtocolStreamProcessor::new(
            chain,
            cli.tycho_url.clone(),
            cli.tycho_api_key.clone(),
            cli.tvl_threshold,
            cli.tvl_buffer_ratio,
            cli.protocols.clone(),
            cli.partial_blocks,
        ) {
            protocol_handle = Some(
                protocol_stream_processor
                    .run_stream(&all_tokens, protocol_tx)
                    .await?,
            );
        }
    }
    if !cli.disable_rfq {
        if let Ok(rfq_stream_processor) = RFQStreamProcessor::new(
            chain,
            cli.tvl_threshold,
            cli.max_simulations as usize,
            Duration::from_secs(cli.skip_messages_duration),
        ) {
            rfq_handle = Some(
                rfq_stream_processor
                    .run_stream(&all_tokens, rfq_tx)
                    .await?,
            );
        }
    }

    let tycho_state = Arc::new(RwLock::new(TychoState::default()));
    // Only collect statistics when max_blocks is set; avoids unbounded growth for indefinite runs
    let statistics: Option<Arc<RwLock<TestStatistics>>> = if cli.max_blocks > 0 {
        Some(Arc::new(RwLock::new(TestStatistics::default())))
    } else {
        None
    };

    // Process streams updates
    if cli.max_blocks > 0 {
        info!("Running integration test for {} blocks", cli.max_blocks);
    } else {
        info!("Running integration test indefinitely");
    }
    info!("Waiting for first protocol update...");
    let protocol_semaphore = Arc::new(Semaphore::new(cli.parallel_updates as usize));
    let rfq_semaphore = Arc::new(Semaphore::new(cli.parallel_updates as usize));
    let mut protocol_stream_open = true;
    let mut rfq_stream_open = !cli.disable_rfq;

    // Staleness watchdog: if no protocol update arrives within stale_threshold_secs, mark all
    // known protocols as Stale in metrics. This catches stream disconnections where the
    // SynchronizerState gauge would otherwise remain frozen at its last known value.
    let stale_threshold = Duration::from_secs(cli.stale_threshold_secs);
    let stale_enabled = !cli.disable_onchain && cli.stale_threshold_secs > 0;
    let mut known_protocols: HashSet<String> = HashSet::new();
    let stale_sleep = tokio::time::sleep(stale_threshold);
    tokio::pin!(stale_sleep);

    loop {
        if !protocol_stream_open && !rfq_stream_open {
            info!("All streams closed, exiting");
            break;
        }

        tokio::select! {
            // Monitor protocol stream termination
            result = async {
                if let Some(handle) = protocol_handle.take() {
                    handle.await
                } else {
                    std::future::pending().await
                }
            }, if protocol_handle.is_some() => {
                match result {
                    Ok(()) => {
                        error!("Protocol stream terminated unexpectedly");
                        return Err(miette!("Protocol stream terminated, exiting application"));
                    }
                    Err(e) => {
                        error!("Protocol stream panicked: {:?}", e);
                        return Err(miette!("Protocol stream panicked, exiting application"));
                    }
                }
            }

            // Monitor RFQ stream termination
            result = async {
                if let Some(handle) = rfq_handle.take() {
                    handle.await
                } else {
                    std::future::pending().await
                }
            }, if rfq_handle.is_some() => {
                match result {
                    Ok(()) => {
                        warn!("RFQ stream terminated");
                        // rfq_handle is already None due to take()
                    }
                    Err(e) => {
                        warn!("RFQ stream panicked: {:?}", e);
                        // rfq_handle is already None due to take()
                    }
                }
            }

            // Staleness watchdog fires when no protocol update arrives within the threshold
            _ = &mut stale_sleep, if stale_enabled => {
                for protocol in &known_protocols {
                    metrics::mark_protocol_stale(protocol);
                }
                stale_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + stale_threshold);
            }

            // Process protocol updates
            update = protocol_rx.recv(), if protocol_stream_open => {
                match update {
                    Some(update) => {
                        let update = match update {
                            Ok(u) => Arc::new(u),
                            Err(e) => {
                                warn!("{}", format_error_chain(&e));
                                continue;
                            }
                        };

                        // Reset the staleness watchdog and register any newly-seen protocols.
                        for protocol in update.update.sync_states.keys() {
                            known_protocols.insert(protocol.clone());
                        }
                        stale_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + stale_threshold);

                        if cli.max_blocks > 0 {
                            if let Some(stats) = statistics.as_ref() {
                                let stats = stats
                                    .read()
                                    .expect("Failed to get read lock for statistics (max-block check)");
                                if stats.blocks_processed >= cli.max_blocks {
                                    drop(stats);
                                    info!("Reached max blocks ({}), stopping...", cli.max_blocks);
                                    break;
                                }
                                let block_num = update.update.block_number_or_timestamp;
                                if !stats.blocks_seen.contains(&block_num)
                                    && stats.blocks_processed >= cli.max_blocks
                                {
                                    drop(stats);
                                    info!(
                                        "Next block would exceed max blocks ({}), stopping...",
                                        cli.max_blocks
                                    );
                                    break;
                                }
                            }
                        }

                        let cli = cli.clone();
                        let rpc_tools = rpc_tools.clone();
                        let tycho_state = tycho_state.clone();
                        let statistics = statistics.clone();
                        let permit = protocol_semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .into_diagnostic()
                            .wrap_err("Failed to acquire protocol permit")?;
                        tokio::spawn(async move {
                            if let Err(e) = process_update(cli, chain, rpc_tools, tycho_state, statistics, &update).await {
                                warn!("{}", format_error_chain(&e));
                            }
                            drop(permit);
                        });
                    }
                    None => {
                        info!("Protocol stream closed");
                        protocol_stream_open = false;
                    }
                }
            }

            // Process RFQ updates independently
            update = rfq_rx.recv(), if rfq_stream_open => {
                match update {
                    Some(update) => {
                        let update = match update {
                            Ok(u) => Arc::new(u),
                            Err(e) => {
                                warn!("{}", format_error_chain(&e));
                                continue;
                            }
                        };

                        let cli = cli.clone();
                        let rpc_tools = rpc_tools.clone();
                        let tycho_state = tycho_state.clone();
                        let statistics = statistics.clone();
                        let permit = rfq_semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .into_diagnostic()
                            .wrap_err("Failed to acquire RFQ permit")?;
                        tokio::spawn(async move {
                            if let Err(e) = process_update(cli, chain, rpc_tools, tycho_state, statistics, &update).await {
                                warn!("{}", format_error_chain(&e));
                            }
                            drop(permit);
                        });
                    }
                    None => {
                        info!("RFQ stream closed");
                        rfq_stream_open = false;
                    }
                }
            }
        }
    }

    // Print summary before exiting (only when stats were collected)
    if let Some(stats) = statistics {
        let stats = stats
            .read()
            .expect("Failed to get read lock for statistics (print summary)");
        stats.print_summary();
        drop(stats);
    }

    Ok(())
}

enum BlockPollResult {
    /// Block numbers match; simulation can proceed.
    Ready(Box<Block>),
    /// Tycho is behind the RPC. Carries the target block's timestamp (if fetchable) so latency
    /// can still be recorded before the update is skipped.
    Stale { target_block_timestamp: Option<u64> },
    /// RPC never reached the target block within the allowed attempts.
    Timeout,
}

/// Polls the RPC until the queried block (`Latest` or `Pending`) matches `target_block`.
///
/// Returns [`BlockPollResult::Ready`] when the block number matches,
/// [`BlockPollResult::Stale`] if the update is already behind the RPC (includes the target
/// block's timestamp for latency recording), or [`BlockPollResult::Timeout`] if the RPC never
/// caught up within `max_attempts`.
///
/// Pass `BlockNumberOrTag::Latest` for confirmed-block mode and `BlockNumberOrTag::Pending`
/// for flashblock mode (requires a flashblocks-capable RPC endpoint).
async fn poll_rpc_for_block(
    rpc_tools: &tycho_test::RPCTools,
    target_block: u64,
    block_tag: BlockNumberOrTag,
    max_attempts: u32,
    poll_interval: Duration,
) -> miette::Result<BlockPollResult> {
    for attempt in 0..max_attempts {
        let block = match rpc_tools
            .provider
            .get_block_by_number(block_tag)
            .await
            .into_diagnostic()
            .wrap_err("Failed to fetch block")
            .ok()
            .flatten()
        {
            Some(b) => b,
            None => {
                warn!(
                    "Failed to retrieve {block_tag} block (attempt {}/{})",
                    attempt + 1,
                    max_attempts
                );
                if attempt < max_attempts - 1 {
                    tokio::time::sleep(poll_interval).await;
                }
                continue;
            }
        };

        let rpc_block = block.header.number;

        if rpc_block > target_block {
            let delay = rpc_block - target_block;
            warn!(
                "Update block ({target_block}) is behind {block_tag} block ({rpc_block}), \
                 skipping to catch up."
            );
            metrics::record_protocol_update_block_delay(delay);

            // Fetch the target block so we can record accurate latency even though simulation
            // will be skipped — excluding stale blocks would bias the histogram toward
            // fast updates only.
            let target_block_timestamp = rpc_tools
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(target_block))
                .await
                .ok()
                .flatten()
                .map(|b| b.header.timestamp);

            return Ok(BlockPollResult::Stale { target_block_timestamp });
        }

        if rpc_block == target_block {
            if attempt > 0 {
                debug!("{block_tag} block caught up to {target_block} after {} poll(s)", attempt);
            }
            return Ok(BlockPollResult::Ready(Box::new(block)));
        }

        debug!(
            "{block_tag} block ({rpc_block}) behind update block ({target_block}), \
             polling... (attempt {}/{})",
            attempt + 1,
            max_attempts
        );
        tokio::time::sleep(poll_interval).await;
    }

    Ok(BlockPollResult::Timeout)
}

async fn process_update(
    cli: Arc<Cli>,
    chain: Chain,
    rpc_tools: tycho_test::RPCTools,
    tycho_state: Arc<RwLock<TychoState>>,
    statistics: Option<Arc<RwLock<TestStatistics>>>,
    update: &StreamUpdate,
) -> miette::Result<()> {
    info!(
        "Got protocol update with block/timestamp {}, {} new pairs, and {} states",
        update.update.block_number_or_timestamp,
        update.update.new_pairs.len(),
        update.update.states.len()
    );

    let block = if let UpdateType::Protocol = update.update_type {
        // Update state cache before block alignment check
        {
            let mut current_state = tycho_state
                .write()
                .map_err(|e| miette!("Failed to acquire write lock on Tycho state: {e}"))?;
            for (id, comp) in update.update.new_pairs.iter() {
                current_state
                    .components
                    .insert(id.clone(), comp.clone());
                current_state
                    .component_ids_by_protocol
                    .entry(comp.protocol_system.clone())
                    .or_insert_with(HashSet::new)
                    .insert(id.clone());
            }
            for (id, state) in update.update.states.iter() {
                current_state
                    .states
                    .insert(id.clone(), state.clone());
            }
            for (removed_id, removed_component) in update.update.removed_pairs.iter() {
                current_state
                    .components
                    .remove(removed_id);
                current_state.states.remove(removed_id);
                current_state
                    .component_ids_by_protocol
                    .get_mut(&removed_component.protocol_system)
                    .map(|id_set| id_set.remove(removed_id));
            }
        }

        let update_block_number = update.update.block_number_or_timestamp;

        // Flashblocks-capable endpoints expose sequencer pre-confirmed state under `pending`;
        // standard endpoints use `latest` (confirmed blocks only).
        let block_tag =
            if cli.partial_blocks { BlockNumberOrTag::Pending } else { BlockNumberOrTag::Latest };
        let poll_interval = Duration::from_millis(cli.rpc_poll_interval_ms);

        let poll_result = poll_rpc_for_block(
            &rpc_tools,
            update_block_number,
            block_tag,
            cli.rpc_poll_attempts,
            poll_interval,
        )
        .await?;

        let block_type = if block_tag == BlockNumberOrTag::Pending { "partial" } else { "full" };
        let block = match poll_result {
            BlockPollResult::Ready(b) => {
                let latency_seconds = update.received_at.as_secs_f64() - b.header.timestamp as f64;
                metrics::record_block_processing_duration(latency_seconds, block_type);
                Arc::new(*b)
            }
            BlockPollResult::Stale { target_block_timestamp } => {
                if let Some(ts) = target_block_timestamp {
                    let latency_seconds = update.received_at.as_secs_f64() - ts as f64;
                    metrics::record_block_processing_duration(latency_seconds, block_type);
                }
                metrics::record_protocol_update_skipped();
                for protocol in update.update.sync_states.keys() {
                    metrics::record_protocol_sync_state_skipped(protocol);
                }
                return Ok(());
            }
            BlockPollResult::Timeout => {
                warn!(
                    "RPC ({block_tag}) did not reach update block {update_block_number}, skipping."
                );
                metrics::record_protocol_update_skipped();
                return Ok(());
            }
        };
        if update.is_first_update {
            info!("Skipping simulation on first protocol update...");
            return Ok(());
        }

        // Record block number for statistics
        if let Some(stats) = statistics.as_ref() {
            let mut stats = stats
                .write()
                .expect("Failed to get write lock for statistics (record block)");
            stats.record_block_processed();
        }

        block
    } else {
        // RFQ updates: fetch latest block without alignment checks
        match rpc_tools
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .into_diagnostic()
            .wrap_err("Failed to fetch latest block")
            .ok()
            .flatten()
        {
            Some(b) => Arc::new(b),
            None => {
                warn!("Failed to retrieve latest block, continuing to next message...");
                return Ok(());
            }
        }
    };

    for (protocol, sync_state) in update.update.sync_states.iter() {
        metrics::record_protocol_sync_state(protocol, sync_state);
    }
    let components_to_process = select_components_to_process(update, &tycho_state, &cli)?;
    // Collect components that implement Validator for batch validation
    let mut validator_components: Vec<(
        &dyn Validator,
        tycho_common::Bytes,
        String, // protocol_system
    )> = Vec::new();

    for (id, component, state) in &components_to_process {
        let component_id = tycho_common::Bytes::from_str(id)
            .unwrap_or_else(|_| tycho_common::Bytes::from(id.as_bytes()));

        if let Some(validator) = get_validator(&component.protocol_system, state.as_ref()) {
            validator_components.push((validator, component_id, component.protocol_system.clone()));
        }
    }

    // Batch validate all components of this block in a single call
    if !validator_components.is_empty() {
        // Extract just the validator data (without protocol_system) for batch_validate_components
        let validator_data: Vec<_> = validator_components
            .iter()
            .map(|(validator, id, _protocol)| (*validator, id.clone()))
            .collect();

        let validation_block_id = if cli.partial_blocks {
            BlockId::pending()
        } else {
            BlockId::from(block.header.number)
        };
        let results =
            batch_validate_components(&cli.rpc_url, &validator_data, validation_block_id).await;

        for (i, result) in results.iter().enumerate() {
            let component_id = &validator_components[i].1;
            let protocol = &validator_components[i].2;
            match result {
                Ok(passed) => {
                    if *passed {
                        debug!(
                            component_id = %component_id,
                            "State validation passed"
                        );
                        if let Some(stats) = statistics.as_ref() {
                            let mut stats = stats
                                .write()
                                .expect("Failed to get write lock for statistics (record validation success)");
                            stats.record_validation_result(protocol, true);
                        }
                    } else {
                        error!(
                            component_id = %component_id,
                            "State validation failed"
                        );
                        metrics::record_validation_failure(protocol);
                        if let Some(stats) = statistics.as_ref() {
                            let mut stats = stats
                                .write()
                                .expect("Failed to get write lock for statistics (record validation failure)");
                            stats.record_validation_result(protocol, false);
                        }
                    }
                }
                Err(e) => {
                    error!(
                        component_id = %component_id,
                        error = %e,
                        "Error validating component"
                    );
                    metrics::record_validation_failure(protocol);
                    if let Some(stats) = statistics.as_ref() {
                        let mut stats = stats.write().expect(
                            "Failed to get write lock for statistics (record validation failure)",
                        );
                        stats.record_validation_result(protocol, false);
                    }
                }
            }
        }
    }

    // Process all components (updated and stale) in parallel
    let semaphore = Arc::new(Semaphore::new(cli.parallel_simulations as usize));
    let mut tasks = Vec::new();

    for (id, component, state) in components_to_process {
        let block = block.clone();
        let statistics = statistics.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .into_diagnostic()
            .wrap_err("Failed to acquire permit")?;

        let task = tokio::spawn(async move {
            let simulation_id = generate_simulation_id(&component.protocol_system, &id);
            let result =
                process_state(&simulation_id, chain, component, &block, id, state, statistics)
                    .await;
            drop(permit);
            result
        });
        tasks.push(task);
    }

    let mut block_execution_info = HashMap::new();

    for task in tasks {
        match task.await {
            Ok(execution_data) => {
                block_execution_info.extend(execution_data);
            }
            Err(e) => {
                warn!("Task failed: {:?}", e);
            }
        }
    }

    if block_execution_info.is_empty() {
        warn!("No simulations were gathered for block {}", block.number());
        return Ok(());
    }

    let results =
        match simulate_swap_transaction(&rpc_tools, block_execution_info.clone(), &block, None)
            .await
        {
            Ok(results) => results,
            Err((e, _, _)) => return Err(e),
        };

    let mut n_reverts = 0;
    let mut n_failures = 0;
    let total_simulations = results.len();
    for (simulation_id, result) in &results {
        let execution_info = match block_execution_info.get(simulation_id) {
            Some(info) => info,
            None => {
                error!("Simulation ID {simulation_id} not found in execution_info HashMap");
                continue;
            }
        }
        .clone();

        let state_str = {
            let current_state = tycho_state
                .read()
                .map_err(|e| miette!("Failed to acquire read lock on Tycho state: {e}"))?;

            match current_state
                .states
                .get(&execution_info.component_id)
            {
                Some(state) => format!("{:?}", state),
                None => "".to_string(),
            }
        };
        // Record unique pool
        if let Some(stats) = statistics.as_ref() {
            let mut stats = stats
                .write()
                .expect("Failed to get write lock for statistics (record pool tested)");
            stats.record_pool_tested(&execution_info.protocol_system, &execution_info.component_id);
        }

        process_execution_result(
            simulation_id,
            result,
            execution_info.clone(),
            state_str,
            (*block).clone(),
            chain.id().to_string(),
            &mut n_reverts,
            &mut n_failures,
            statistics.clone(),
            cli.router_fee,
        );

        // Record statistics
        if let Some(stats) = statistics.as_ref() {
            let mut stats = stats
                .write()
                .expect("Failed to get write lock for statistics (record simulation result)");
            stats.record_execution_simulation_result(&execution_info.protocol_system, result);
        }
    }
    if n_reverts > 0 || n_failures > 0 {
        warn!("For block {}, simulated {total_simulations} executions, {n_reverts} simulations reverted, {n_failures} executions setup failed", block.number())
    }

    Ok(())
}

#[allow(clippy::type_complexity)]
fn select_components_to_process(
    update: &StreamUpdate,
    tycho_state: &Arc<RwLock<TychoState>>,
    cli: &Arc<Cli>,
) -> miette::Result<Vec<(String, ProtocolComponent, Box<dyn ProtocolSim>)>> {
    // Collect all components to process (both updated and stale) for batch validation
    let mut components_to_process: Vec<(String, ProtocolComponent, Box<dyn ProtocolSim>)> =
        Vec::new();

    // Collect updated components
    // As the component ordering is semi-random, it is safe to just take the first N components and
    // have a good coverage
    for (id, state) in update
        .update
        .states
        .iter()
        .take(cli.max_simulations as usize)
    {
        let component = match update.update_type {
            UpdateType::Protocol => {
                let states = &tycho_state
                    .read()
                    .map_err(|e| miette!("Failed to acquire read lock on Tycho state: {e}"))?
                    .components;
                match states.get(id) {
                    Some(comp) => comp.clone(),
                    None => {
                        warn!(id=%id, "Component not found in cached protocol pairs. Potential causes: \
                        there was an error decoding the component, the component was evicted from the cache, \
                        or the component was never added to the cache. Skipping...");
                        continue;
                    }
                }
            }
            UpdateType::Rfq => match update.update.new_pairs.get(id) {
                Some(comp) => comp.clone(),
                None => {
                    warn!(id=%id, "Component not found in update's new pairs. Potential cause: \
                    the `states` and `new_pairs` lists don't contain the same items. Skipping...");
                    continue;
                }
            },
        };
        components_to_process.push((id.clone(), component, state.clone_box()));
    }

    if update.update_type == UpdateType::Protocol {
        // Collect stale components (not updated in this block)
        let selected_ids = {
            let current_state = tycho_state
                .read()
                .map_err(|e| miette!("Failed to acquire write lock on Tycho state: {e}"))?;

            let mut all_selected_ids = Vec::new();

            for component_id in &cli.always_test_components {
                if !update
                    .update
                    .states
                    .keys()
                    .contains(component_id) &&
                    current_state
                        .components
                        .contains_key(component_id)
                {
                    all_selected_ids.push(component_id.clone());
                }
            }

            for (protocol, component_ids) in &current_state.component_ids_by_protocol {
                let protocol_sync_state = update.update.sync_states.get(protocol);
                match protocol_sync_state {
                    None => continue,
                    Some(SynchronizerState::Ready(_)) => {
                        let available_ids: Vec<_> = component_ids
                            .iter()
                            .filter(|id| {
                                !update.update.states.keys().contains(id) &&
                                    !all_selected_ids.contains(id)
                            })
                            .cloned()
                            .collect();

                        let protocol_selected_ids: Vec<_> = available_ids
                            .choose_multiple(
                                &mut rand::rng(),
                                (cli.max_simulations_stale as usize).min(available_ids.len()),
                            )
                            .cloned()
                            .collect();

                        all_selected_ids.extend(protocol_selected_ids);
                    }
                    _ => continue,
                }
            }
            all_selected_ids
        };

        for id in &selected_ids {
            let (component, state) = {
                let current_state = tycho_state
                    .read()
                    .map_err(|e| miette!("Failed to acquire read lock on Tycho state: {e}"))?;

                match (current_state.components.get(id), current_state.states.get(id)) {
                    (Some(comp), Some(state)) => (comp.clone(), state.clone()),
                    (None, _) => {
                        error!(id=%id, "Component not found in saved protocol components.");
                        continue;
                    }
                    (_, None) => {
                        error!(id=%id, "State not found in saved protocol states");
                        continue;
                    }
                }
            };
            components_to_process.push((id.clone(), component, state.clone_box()));
        }
    }
    Ok(components_to_process)
}

#[tracing::instrument(
    skip_all,
    fields(
        simulation_id = %simulation_id,
        protocol = %component.protocol_system,
        component_id = %state_id,
        block_number = %block.header.number,
    )
)]
async fn process_state(
    simulation_id: &str,
    chain: Chain,
    component: ProtocolComponent,
    block: &Block,
    state_id: String,
    state: Box<dyn ProtocolSim>,
    statistics: Option<Arc<RwLock<TestStatistics>>>,
) -> HashMap<String, TychoExecutionInput> {
    let tokens_len = component.tokens.len();
    if tokens_len < 2 {
        error!("Component has less than 2 tokens, skipping...");
        return HashMap::new();
    }
    let mut min_amount = BigUint::ZERO;
    // Get all the possible swap directions
    let swap_directions = match component.protocol_system.as_str() {
        HashflowClient::PROTOCOL_SYSTEM => {
            // Hashflow only supports swaps between the requested base and quote tokens
            // WARN: we read from state because the component.tokens original order
            // is modified here: src/protocol/models.rs: ProtocolComponent::from_with_tokens
            let state = match state
                .as_any()
                .downcast_ref::<HashflowState>()
            {
                Some(s) => s.clone(),
                None => {
                    warn!("Failed to downcast state to HashflowState");
                    return HashMap::new();
                }
            };
            // The smallest amount acceptable for hashflow is the amount of the first level, random
            // small amounts are not accepted. The amount in will be capped to this value
            let min_amount_in = BigUint::from(state.levels.levels[0].quantity.ceil() as u128);
            min_amount = min_amount_in * BigUint::from(10u32).pow(state.base_token.decimals);
            vec![(state.base_token, state.quote_token)]
        }
        LiquoriceClient::PROTOCOL_SYSTEM => {
            let state = match state
                .as_any()
                .downcast_ref::<LiquoriceState>()
            {
                Some(s) => s.clone(),
                None => {
                    warn!("Failed to downcast state to LiquoriceState");
                    return HashMap::new();
                }
            };
            vec![(state.base_token, state.quote_token)]
        }
        _ => component
            .tokens
            .iter()
            .permutations(2)
            .map(|perm| (perm[0].clone(), perm[1].clone()))
            .collect(),
    };
    let mut execution_infos = HashMap::new();
    for (i, (token_in, token_out)) in swap_directions.iter().enumerate() {
        debug!(
            "Processing {} pool {state_id}, from {} to {}",
            component.protocol_system, token_in.symbol, token_out.symbol
        );

        // Get max input/output limits
        let (max_input, max_output) = match state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .into_diagnostic()
            .wrap_err(format!(
                "Error getting limits for token_in: {}, and token_out: {}",
                token_in.symbol, token_out.symbol
            )) {
            Ok(limits) => {
                metrics::record_get_limits_success(&component.protocol_system);
                if let Some(stats) = statistics.as_ref() {
                    let mut stats = stats.write().expect(
                        "Failed to get write lock for statistics (record get_limits success)",
                    );
                    stats.record_get_limits(&component.protocol_system, true);
                }
                limits
            }
            Err(e) => {
                error!(
                    event_type = "get_limits_failure",
                    token_in = %token_in.address,
                    token_out = %token_out.address,
                    error = %format_error_chain(&e),
                    "Get limits operation failed: {}", format_error_chain(&e)
                );
                debug!(
                    event_type = "get_limits_failure",
                    state = ?state,
                    "Get limits operation failed: {}", format_error_chain(&e)
                );
                metrics::record_get_limits_failure(&component.protocol_system);
                if let Some(stats) = statistics.as_ref() {
                    let mut stats = stats.write().expect(
                        "Failed to get write lock for statistics (record get_limits failure)",
                    );
                    stats.record_get_limits(&component.protocol_system, false);
                }
                continue;
            }
        };
        debug!(
            "Retrieved limits: max input {max_input} {}; max output {max_output} {}",
            token_in.symbol, token_out.symbol
        );

        // Calculate amount_in as percentage of max_input
        const PERCENTAGE: f64 = 0.001; // 0.1%
        const PRECISION: u64 = 1_000; // Supports up to 3 decimal places
        let numerator = (PERCENTAGE * PRECISION as f64) as u64;
        let mut amount_in = (&max_input * numerator) / PRECISION;
        if amount_in.is_zero() {
            debug!("Calculated amount_in is zero, skipping...");
            continue;
        }
        amount_in = amount_in.max(min_amount.clone());

        // Cap amount_in to avoid "amount exceeds 96 bits" error (it happens for uniswap v3 and v4
        // sometimes because the returned limits can be very high)
        let max_96_bit = BigUint::from(2u128.pow(96) - 1);
        amount_in = amount_in.min(max_96_bit);

        // Get expected amount out using tycho-simulation and measure duration
        let start_time = std::time::Instant::now();
        let amount_out_result = match state
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .into_diagnostic()
            .wrap_err(format!(
                "Error calculating amount out at {:.1}% with input of {amount_in} {}.",
                PERCENTAGE * 100.0,
                token_in.symbol,
            )) {
            Ok(res) => {
                metrics::record_get_amount_out_success(&component.protocol_system);
                if let Some(stats) = statistics.as_ref() {
                    let mut stats = stats.write().expect(
                        "Failed to get write lock for statistics (record get_amount_out success)",
                    );
                    stats.record_get_amount_out(&component.protocol_system, true);
                }
                res
            }
            Err(e) => {
                error!(
                    event_type = "get_amount_out_failure",
                    token_in = %token_in.address,
                    token_out = %token_out.address,
                    amount_in = %amount_in,
                    error = %format_error_chain(&e),
                    "Get amount out operation failed: {}", format_error_chain(&e)
                );
                debug!(
                    event_type = "get_amount_out_failure",
                    state = ?state,
                    "Get amount out operation failed: {}", format_error_chain(&e)
                );
                metrics::record_get_amount_out_failure(&component.protocol_system);
                if let Some(stats) = statistics.as_ref() {
                    let mut stats = stats.write().expect(
                        "Failed to get write lock for statistics (record get_amount_out failure)",
                    );
                    stats.record_get_amount_out(&component.protocol_system, false);
                }
                continue;
            }
        };
        let duration_seconds = start_time.elapsed().as_secs_f64();
        let expected_amount_out = amount_out_result.amount;
        debug!(
            event_type = "get_amount_out_duration",
            token_in = %token_in.address,
            token_out = %token_out.address,
            amount_in = %amount_in,
            amount_out = %expected_amount_out,
            duration_seconds = duration_seconds,
            "Get amount out operation completed in {:.3}ms", duration_seconds * 1000.0
        );
        metrics::record_get_amount_out_duration(&component.protocol_system, duration_seconds);

        // Sometimes the expected amount out might be zero (e.g. pool is depleted in one direction)
        // Then execution will fail with TychoRouter__UndefinedMinAmountOut
        if expected_amount_out == BigUint::ZERO {
            continue;
        }

        if component.protocol_system == COWAMM_PROTOCOL_SYSTEM {
            debug!("CowAMM protocol system is not supported for execution");
            continue;
        }

        // Simulate execution amount out against the RPC
        let (solution, transaction) = match encode_swap(
            &component,
            Some(Arc::from(state.clone_box())),
            token_in,
            token_out,
            amount_in.clone(),
            chain,
            None,
            amount_out_result.gas,
        ) {
            Ok(res) => res,
            Err(e) => {
                warn!("{}", format_error_chain(&e));
                continue;
            }
        };
        execution_infos.insert(
            format!("{}-{:?}", simulation_id, i),
            TychoExecutionInput {
                solution,
                transaction: transaction.clone(),
                expected_amount_out,
                protocol_system: component.protocol_system.clone(),
                component_id: component.id.to_string(),
                token_in: token_in.address.to_string(),
                token_out: token_out.address.to_string(),
                estimated_gas: transaction.estimated_gas().clone(),
            },
        );
    }
    execution_infos
}

/// Processes the result of a Tycho simulation execution and emits metrics.
///
/// Handles success, revert, and failure cases by logging appropriate events and recording
/// metrics. Calculates slippage for successful executions and updates counters for
/// reverts and failures.
///
/// Returns updated counters for reverts and failures.
#[tracing::instrument(
    skip_all,
    fields(
        simulation_id = %simulation_id,
        protocol = %execution_info.protocol_system,
        block_number = %block.header.number,
        component_id = %execution_info.component_id,
    )
)]
#[allow(clippy::too_many_arguments)]
fn process_execution_result(
    simulation_id: &String,
    result: &TychoExecutionResult,
    execution_info: TychoExecutionInput,
    state_str: String,
    block: Block,
    chain_id: String,
    n_reverts: &mut i32,
    n_failures: &mut i32,
    statistics: Option<Arc<RwLock<TestStatistics>>>,
    router_fee: u16,
) {
    match result {
        TychoExecutionResult::Success {
            gas_used,
            amount_out,
            state_overwrites,
            overwrite_metadata,
        } => {
            debug!(
                event_type = "simulation_execution_success",
                amount_out = amount_out.to_string(),
                gas_used = gas_used,
                "Simulation execution succeeded"
            );

            metrics::record_simulation_execution_success(&execution_info.protocol_system);

            // Remove the router fee from the expected simulated amount out.
            let simulated_amount_out_without_fee = execution_info
                .expected_amount_out
                .clone() -
                (execution_info.expected_amount_out * BigUint::from(router_fee)) /
                    BigUint::from(10000u64);

            // Calculate slippage: positive = simulated > expected, negative = simulated <
            // expected
            let slippage = if amount_out >= &simulated_amount_out_without_fee {
                let diff = amount_out - &simulated_amount_out_without_fee;
                ((diff.clone() * BigUint::from(10000u32)) / &simulated_amount_out_without_fee)
                    .to_f64()
                    .unwrap_or(0.0) /
                    100.0
            } else {
                let diff = &simulated_amount_out_without_fee - amount_out;
                -((diff.clone() * BigUint::from(10000u32)) / &simulated_amount_out_without_fee)
                    .to_f64()
                    .unwrap_or(0.0) /
                    100.0
            };

            // Generate Tenderly URL for debugging with state overrides
            let overrides =
                tenderly::TenderlySimParams { network: Some(chain_id), ..Default::default() };
            let tenderly_url = tenderly::build_tenderly_url(
                &overrides,
                Some(&execution_info.transaction),
                Some(&block),
                Address::from_slice(&execution_info.solution.sender()[..20]),
            );

            let overwrites_string = if let Some(overwrites) = state_overwrites.as_ref() {
                tenderly::get_overwrites_string(overwrites, overwrite_metadata)
            } else {
                String::new()
            };

            if !(-0.2..=0.2).contains(&slippage) {
                error!(
                    event_type = "execution_slippage",
                    token_in = %execution_info.token_in,
                    token_out = %execution_info.token_out,
                    amount_in = %execution_info.solution.amount_in(),
                    executed_amount  = %amount_out,
                    simulated_amount = %simulated_amount_out_without_fee,
                    slippage_ratio = slippage,
                    tenderly = tenderly_url,
                    overwrites = %overwrites_string,
                    "Execution slippage: {:.2}%",
                    slippage
                );
            } else {
                // don't show the state in this case to not overwhelm the logs
                debug!(
                    event_type = "execution_slippage",
                    token_in = %execution_info.token_in,
                    token_out = %execution_info.token_out,
                    executed_amount  = %amount_out,
                    simulated_amount = %simulated_amount_out_without_fee,
                    slippage_ratio = slippage,
                    tenderly = tenderly_url,
                    overwrites = %overwrites_string,
                    "Execution slippage: {:.2}%",
                    slippage
                );
            }

            metrics::record_execution_slippage(&execution_info.protocol_system, slippage);

            let estimated_gas = execution_info.estimated_gas.clone();

            if let Some(estimated) = estimated_gas.to_f64() {
                metrics::record_gas_signed_error_ratio(
                    &execution_info.protocol_system,
                    estimated,
                    *gas_used as f64,
                );
                metrics::record_gas_signed_error_absolute(
                    &execution_info.protocol_system,
                    estimated,
                    *gas_used as f64,
                );
            }

            // Record slippage in statistics
            if let Some(ref stats) = statistics {
                let mut stats = stats
                    .write()
                    .expect("Failed to get write lock for statistics (record slippage)");
                stats.record_execution_slippage(&execution_info.protocol_system, slippage);
            }
        }
        TychoExecutionResult::Revert { reason, state_overwrites, overwrite_metadata } => {
            *n_reverts += 1;
            let error_msg = reason.to_string();

            // Extract revert reason from error message
            // Error format is typically "Transaction reverted: <reason>"
            let revert_reason =
                if let Some(reason) = error_msg.strip_prefix("Transaction reverted: ") {
                    reason
                } else {
                    &error_msg
                };

            // Extract error name (first word or function signature)
            let error_name = extract_error_name(revert_reason);

            // Generate Tenderly URL for debugging without state overrides
            let overrides =
                tenderly::TenderlySimParams { network: Some(chain_id), ..Default::default() };
            let tenderly_url = tenderly::build_tenderly_url(
                &overrides,
                Some(&execution_info.transaction),
                Some(&block),
                Address::from_slice(&execution_info.solution.sender()[..20]),
            );

            let overwrites_string = if let Some(overwrites) = state_overwrites.as_ref() {
                tenderly::get_overwrites_string(overwrites, overwrite_metadata)
            } else {
                String::new()
            };
            let error_category = categorize_error(revert_reason);
            error!(
                event_type = "simulation_execution_revert",
                error_message = %revert_reason,
                error_name = %error_name,
                error_category = %error_category,
                amount_in =%execution_info.solution.amount_in(),
                token_in = %execution_info.token_in,
                token_out = %execution_info.token_out,
                tenderly_url = %tenderly_url,
                overwrites = %overwrites_string,
                "Failed to simulate swap: {error_msg}"
            );
            debug!(event_type = "simulation_execution_revert",
                state = ?state_str,
                "State of failed swap: {error_msg}");
            metrics::record_simulation_execution_revert(
                &execution_info.protocol_system,
                error_category,
            );
        }
        TychoExecutionResult::Failed { error_msg } => {
            *n_failures += 1;

            let error_category = categorize_error(error_msg);
            error!(
                event_type = "simulation_execution_failure",
                error_message = %error_msg,
                error_category = %error_category,
                amount_in =%execution_info.solution.amount_in(),
                token_in = %execution_info.token_in,
                token_out = %execution_info.token_out,
                "Failed to simulate swap: {error_msg}"
            );
            metrics::record_simulation_execution_failure(
                &execution_info.protocol_system,
                error_category,
            );
        }
    }
}

/// Extract the error name from a revert reason string
/// Examples:
/// - "TychoRouter__NegativeSlippage(1000, 990)" -> "TychoRouter__NegativeSlippage"
/// - "arithmetic underflow or overflow" -> "arithmetic underflow or overflow"
/// - "Error(string): insufficient balance" -> "Error"
fn extract_error_name(revert_reason: &str) -> String {
    // Check if it's a function-style error (e.g., "ErrorName(...)")
    if let Some(paren_pos) = revert_reason.find('(') {
        revert_reason[..paren_pos]
            .trim()
            .to_string()
    } else if let Some(colon_pos) = revert_reason.find(':') {
        // Handle "Error: message" format
        revert_reason[..colon_pos]
            .trim()
            .to_string()
    } else {
        // Return the whole message for simple errors
        revert_reason.trim().to_string()
    }
}

fn categorize_error(error_message: &str) -> &'static str {
    // We can add more categories here when we find new meaningful ones
    match error_message {
        e if e.contains("Couldn't find storage slot") => "Storage slot not found",
        e if e.contains("TychoRouter__NegativeSlippage") => "TychoRouter__NegativeSlippage",
        e if e.contains("0xf7bf5832") => "Fee token", /* Decodes to TychoRouter__AmountOutNotFullyReceived */
        e if e.contains("UniswapV2: K") => "Fee token",
        e if e.contains("Insufficient balance for amount + tax") => "Fee token",
        _ => "other",
    }
}

/// Generate a unique simulation ID based on protocol system and state ID
fn generate_simulation_id(protocol_system: &str, state_id: &str) -> String {
    let random_number: u32 = rand::random_range(10000..=99999);
    let component_prefix = state_id
        .chars()
        .take(8)
        .collect::<String>();
    format!("{}_{}_{}", protocol_system, component_prefix, random_number)
}

/// Format the full error chain into a single string, without newlines
fn format_error_chain(e: &miette::Error) -> String {
    let mut chain = vec![];
    for cause in e.chain() {
        chain.push(format!("{cause}"));
    }
    chain.join(" -> ")
}
