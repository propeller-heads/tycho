mod fee_fetcher;
mod metrics;
mod oracle_overrides;
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
use tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_execution::encoding::evm::{
    get_router_address, swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    utils::bytes_to_address, PRICE_LEVEL_STREAM_PREFIX, PROPAMM_FALLBACK_PREFIX,
};
use tycho_simulation::{
    evm::protocol::cowamm::constants::PROTOCOL_SYSTEM as COWAMM_PROTOCOL_SYSTEM,
    protocol::models::ProtocolComponent,
    rfq::protocols::{
        hashflow::{client::HashflowClient, state::HashflowState},
        liquorice::{client::LiquoriceClient, state::LiquoriceState},
    },
    tycho_common::models::{chain_config::TvlThresholdTier, Chain},
    utils::load_all_tokens,
};
use tycho_test::{
    execution::{
        encoding::encode_swap,
        models::{RouterOverwritesData, TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction, tenderly,
    },
    is_block_not_found,
    token_prices::{cap_amount_to_eth_value, load_token_prices},
    validation::{batch_validate_components, get_validator, Validator},
};

use crate::{
    fee_fetcher::{fetch_router_fee_on_output, RouterFeeOnOutput},
    oracle_overrides::{override_protocol, titan_providers, BlockOverrides, OracleOverrides},
    statistics::TestStatistics,
    stream_processor::{
        price_level_stream_processor::PriceLevelStreamProcessor,
        protocol_stream_processor::ProtocolStreamProcessor,
        rfq_stream_processor::RFQStreamProcessor, StreamUpdate, UpdateType,
    },
};

#[derive(Parser, Clone)]
struct Cli {
    /// The TVL threshold in native token units to filter the graph by.
    /// Defaults to a chain-appropriate value targeting ~$200K USD equivalent (Medium tier).
    #[arg(long)]
    tvl_threshold: Option<f64>,

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

    /// Connect to Tycho over plain HTTP/WS instead of TLS. Enable this when targeting a local
    /// dev instance (e.g. http://127.0.0.1:4242).
    #[arg(long, env = "TYCHO_NO_TLS", default_value_t = false)]
    no_tls: bool,

    /// Disable on-chain protocols
    #[arg(long, default_value_t = false)]
    disable_onchain: bool,

    /// Disable RFQ protocols
    #[arg(long, default_value_t = false)]
    disable_rfq: bool,

    /// Run PAMM RFQ protocols.
    #[arg(long, default_value_t = true)]
    run_pamm_protocols: bool,

    /// Disable the Titan pAMM price level stream (only active on Ethereum)
    #[arg(long, default_value_t = false)]
    disable_price_level_stream: bool,

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

    /// The price level stream emits one sampled update per this many blocks
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    price_level_stream_block_interval: u64,

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

    /// Run the protocol-stream test pipeline only on blocks whose number is a multiple of this
    /// value. 1 tests every block (current behavior). Use a higher value on fast chains (e.g.
    /// Robinhood) where the harness cannot keep up with the head, or with --partial-blocks to
    /// cap RPC usage (e.g. Base flashblocks). With --partial-blocks, every partial update of a
    /// sampled block is tested (bursty by design) and blocks are still fetched via the
    /// pending/latest polling path. State from every block is ingested either way.
    #[arg(
        long,
        default_value_t = 1,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    test_every_n_blocks: u64,

    /// Seconds without a protocol update before marking all known protocols as stale in metrics.
    /// 0 disables the watchdog.
    #[arg(long, default_value_t = 30)]
    stale_threshold_secs: u64,

    /// Seconds without a Titan price level message before marking the served pAMMs as stale in
    /// metrics. 0 disables the watchdog.
    #[arg(long, default_value_t = 10)]
    price_level_stream_stale_threshold_secs: u64,

    /// Disable on-chain swap execution validation (RPC simulation only, no swap encoding or
    /// execution). Useful for diagnosing stream latency without execution overhead.
    #[arg(long, default_value_t = false)]
    disable_execution: bool,

    /// Mark all known executors as activated on the router in the execution simulation's state
    /// overrides, so executors that are unapproved or still inside their 3-day activation timelock
    /// can be validated. Read-only: this affects the simulation call only, never a real
    /// transaction. Leave off to keep the test failing when an executor was never activated.
    #[arg(long, default_value_t = false)]
    bypass_executor_timelock: bool,
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
            .field("bypass_executor_timelock", &self.bypass_executor_timelock)
            .finish()
    }
}

#[derive(Default)]
struct TychoState {
    states: HashMap<String, Box<dyn ProtocolSim>>,
    components: HashMap<String, ProtocolComponent>,
    component_ids_by_protocol: HashMap<String, HashSet<String>>,
}

/// Shared, periodically-refreshed token-price snapshot (raw token units per ETH).
type SharedTokenPrices = Arc<RwLock<Arc<HashMap<Bytes, f64>>>>;

/// How often to reload the token-price snapshot. The S3 dump is refreshed weekly; reloading every
/// day picks up a new dump without restarting this long-running binary.
const TOKEN_PRICE_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Swap input value used for simulation, denominated in ETH. Tick-based protocols (Uniswap V3/V4)
/// report near-infinite `get_limits`, so swapping the raw limit produces unrealistic input amounts
/// and gas estimates. Capping the input to a realistic value (~10k USD at recent ETH prices) keeps
/// simulation and the dashboard gas estimates representative.
const MAX_INPUT_VALUE_ETH: f64 = 5.0;

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

    if cli.bypass_executor_timelock {
        warn!(
            "Executor activation timelock is bypassed in execution simulations - a passing run does \
             not prove that the executors are activated on the router"
        );
    }

    let chain = cli.chain;
    let tvl_threshold = cli
        .tvl_threshold
        .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Medium));
    let cli = Arc::new(cli);

    let rpc_tools = tycho_test::RPCTools::new(&cli.rpc_url, &chain).await?;

    // Everything is simulated against the deployed contracts; the executors are only activated
    // when asked for, so that a missing activation still surfaces as a revert.
    let router_overwrites_data = if cli.bypass_executor_timelock {
        create_router_overwrites_data(chain)?
    } else {
        RouterOverwritesData::default()
    };

    // Read the router fee on output from the on-chain FeeCalculator once at start-up. Slippage is
    // computed after backing this fee out of the simulated amount out, so a stale or wrong fee
    // would skew every slippage result.
    let router_address = get_router_address(&chain)
        .map_err(|e| miette!("No Tycho router address configured for chain {chain:?}: {e}"))?;
    let router_fee = fetch_router_fee_on_output(&rpc_tools.provider, router_address)
        .await
        .wrap_err("Failed to read router fee on output from the on-chain FeeCalculator")?;
    info!(
        numerator = router_fee.numerator(),
        denominator = router_fee.denominator(),
        "Loaded router fee on output from on-chain FeeCalculator"
    );

    // Load tokens from Tycho
    info!(%cli.tycho_url, "Loading tokens...");
    let all_tokens = load_all_tokens(
        &cli.tycho_url,
        cli.no_tls,
        Some(cli.tycho_api_key.as_str()),
        true,
        chain,
        cli.min_token_quality,
        cli.max_days_since_last_trade,
    )
    .await
    .map_err(|e| miette!("Failed to load tokens: {e:?}"))?;
    info!("Loaded {} tokens", all_tokens.len());

    let initial_prices = load_token_prices(chain)
        .await
        .wrap_err("Failed to load token prices for input capping")?;
    info!("Loaded {} token prices", initial_prices.len());
    let token_prices: SharedTokenPrices = Arc::new(RwLock::new(Arc::new(initial_prices)));
    tokio::spawn(refresh_token_prices(chain, token_prices.clone(), TOKEN_PRICE_REFRESH_INTERVAL));

    // Run streams in background tasks with separate channels so RFQ and price level stream
    // processing cannot block protocol update consumption
    let (protocol_tx, mut protocol_rx) =
        tokio::sync::mpsc::channel::<miette::Result<StreamUpdate>>(64);
    let (rfq_tx, mut rfq_rx) = tokio::sync::mpsc::channel::<miette::Result<StreamUpdate>>(64);
    let (price_level_tx, mut price_level_rx) =
        tokio::sync::mpsc::channel::<miette::Result<StreamUpdate>>(64);
    let mut protocol_handle = None;
    let mut rfq_handle = None;
    let mut price_level_handle = None;

    // One Titan connection serves both the indexed pAMM pools and the execution overrides. The
    // price level stream only runs on Ethereum, so off it the providers are only worth opening
    // for the pools.
    let overrides_wanted =
        chain == Chain::Ethereum && !cli.disable_price_level_stream && !cli.disable_execution;
    let override_providers =
        if !cli.disable_onchain || overrides_wanted { titan_providers() } else { HashMap::new() };

    if !cli.disable_onchain {
        if let Ok(protocol_stream_processor) = ProtocolStreamProcessor::new(
            chain,
            cli.tycho_url.clone(),
            cli.tycho_api_key.clone(),
            tvl_threshold,
            cli.tvl_buffer_ratio,
            cli.protocols.clone(),
            cli.partial_blocks,
            cli.no_tls,
        ) {
            protocol_handle = Some(
                protocol_stream_processor
                    .with_override_providers(override_providers.clone())
                    .run_stream(&all_tokens, protocol_tx)
                    .await?,
            );
        }
    }
    if !cli.disable_rfq {
        let rfq_stream_processor = RFQStreamProcessor::new(
            chain,
            tvl_threshold,
            cli.max_simulations as usize,
            Duration::from_secs(cli.skip_messages_duration),
            cli.run_pamm_protocols,
        )
        .unwrap_or_else(|e| panic!("Failed to create RFQ stream processor: {e}"));
        rfq_handle = Some(
            rfq_stream_processor
                .run_stream(&all_tokens, rfq_tx)
                .await?,
        );
    }
    if !cli.disable_price_level_stream {
        if let Some(price_level_stream_processor) = PriceLevelStreamProcessor::new(
            chain,
            cli.max_simulations as usize,
            cli.price_level_stream_block_interval,
            Duration::from_secs(cli.price_level_stream_stale_threshold_secs),
        ) {
            price_level_handle = Some(
                price_level_stream_processor
                    .run_stream(&all_tokens, price_level_tx)
                    .await?,
            );
        } else {
            debug!(?chain, "The Titan pAMM price level stream only serves Ethereum; not starting");
        }
    }

    // Without the overrides the venue reverts `StaleUpdate()`.
    let oracle_overrides = if price_level_handle.is_some() && !cli.disable_execution {
        OracleOverrides::spawn(&override_providers).map(Arc::new)
    } else {
        None
    };

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
    let price_level_semaphore = Arc::new(Semaphore::new(cli.parallel_updates as usize));
    let mut protocol_stream_open = true;
    let mut rfq_stream_open = !cli.disable_rfq;
    let mut price_level_stream_open = price_level_handle.is_some();

    // Staleness watchdog: if no protocol update arrives within stale_threshold_secs, mark all
    // known protocols as Stale in metrics. This catches stream disconnections where the
    // SynchronizerState gauge would otherwise remain frozen at its last known value.
    let stale_threshold = Duration::from_secs(cli.stale_threshold_secs);
    let stale_enabled = !cli.disable_onchain && cli.stale_threshold_secs > 0;
    let mut known_protocols: HashSet<String> = HashSet::new();
    let stale_sleep = tokio::time::sleep(stale_threshold);
    tokio::pin!(stale_sleep);

    loop {
        if !protocol_stream_open && !rfq_stream_open && !price_level_stream_open {
            info!("All streams closed, exiting");
            break;
        }

        tokio::select! {
            // Monitor protocol stream termination. The handles are awaited by reference: the
            // select futures are recreated (and the losers dropped) every iteration, so taking
            // the handle out would detach the task and disable this arm after one poll.
            result = async {
                if let Some(handle) = protocol_handle.as_mut() {
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
                if let Some(handle) = rfq_handle.as_mut() {
                    handle.await
                } else {
                    std::future::pending().await
                }
            }, if rfq_handle.is_some() => {
                rfq_handle = None;
                match result {
                    Ok(()) => {
                        warn!("RFQ stream terminated");
                    }
                    Err(e) => {
                        warn!("RFQ stream panicked: {:?}", e);
                    }
                }
            }

            // Monitor price level stream termination
            result = async {
                if let Some(handle) = price_level_handle.as_mut() {
                    handle.await
                } else {
                    std::future::pending().await
                }
            }, if price_level_handle.is_some() => {
                price_level_handle = None;
                match result {
                    Ok(()) => {
                        warn!("Price level stream terminated");
                    }
                    Err(e) => {
                        warn!("Price level stream panicked: {:?}", e);
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

                        if reached_max_blocks(cli.max_blocks, statistics.as_ref()) {
                            info!("Reached max blocks ({}), stopping...", cli.max_blocks);
                            break;
                        }

                        let cli = cli.clone();
                        let rpc_tools = rpc_tools.clone();
                        let tycho_state = tycho_state.clone();
                        let statistics = statistics.clone();
                        let token_prices = token_prices.clone();
                        let oracle_overrides = oracle_overrides.clone();
                        let router_overwrites_data = router_overwrites_data.clone();
                        let permit = protocol_semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .into_diagnostic()
                            .wrap_err("Failed to acquire protocol permit")?;
                        tokio::spawn(async move {
                            if let Err(e) = process_update(cli, chain, rpc_tools, tycho_state, statistics, token_prices, router_fee, oracle_overrides, &update, router_overwrites_data).await {
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
                        let token_prices = token_prices.clone();
                        let oracle_overrides = oracle_overrides.clone();
                        let router_overwrites_data = router_overwrites_data.clone();
                        let permit = rfq_semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .into_diagnostic()
                            .wrap_err("Failed to acquire RFQ permit")?;
                        tokio::spawn(async move {
                            if let Err(e) = process_update(cli, chain, rpc_tools, tycho_state, statistics, token_prices, router_fee, oracle_overrides, &update, router_overwrites_data).await {
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

            // Process price level stream updates independently
            update = price_level_rx.recv(), if price_level_stream_open => {
                match update {
                    Some(update) => {
                        let update = match update {
                            Ok(u) => Arc::new(u),
                            Err(e) => {
                                warn!("{}", format_error_chain(&e));
                                continue;
                            }
                        };

                        if reached_max_blocks(cli.max_blocks, statistics.as_ref()) {
                            info!("Reached max blocks ({}), stopping...", cli.max_blocks);
                            break;
                        }

                        let cli = cli.clone();
                        let rpc_tools = rpc_tools.clone();
                        let tycho_state = tycho_state.clone();
                        let statistics = statistics.clone();
                        let token_prices = token_prices.clone();
                        let oracle_overrides = oracle_overrides.clone();
                        let router_overwrites_data = router_overwrites_data.clone();
                        let permit = price_level_semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .into_diagnostic()
                            .wrap_err("Failed to acquire price level stream permit")?;
                        tokio::spawn(async move {
                            if let Err(e) = process_update(cli, chain, rpc_tools, tycho_state, statistics, token_prices, router_fee, oracle_overrides, &update, router_overwrites_data).await {
                                warn!("{}", format_error_chain(&e));
                            }
                            drop(permit);
                        });
                    }
                    None => {
                        info!("Price level stream closed");
                        price_level_stream_open = false;
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

/// Periodically reloads the token-price snapshot so a freshly published weekly dump is picked up
/// without restarting the test. On a failed reload the previous snapshot is kept.
async fn refresh_token_prices(chain: Chain, prices: SharedTokenPrices, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        match load_token_prices(chain).await {
            Ok(new_prices) => {
                let count = new_prices.len();
                match prices.write() {
                    Ok(mut guard) => {
                        *guard = Arc::new(new_prices);
                        info!("Refreshed token prices ({count} entries)");
                    }
                    Err(e) => {
                        error!("Failed to acquire write lock to refresh token prices: {e}");
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to refresh token prices, keeping previous snapshot: {}",
                    format_error_chain(&e)
                );
            }
        }
    }
}

/// Waits until the RPC has reached `target_block` and returns exactly that block. `Ok(None)`
/// means the chain did not reach the target within `max_attempts`; `Err` means the polling ended
/// on an RPC failure instead, carrying the failed operation, target block, and attempt.
///
/// Unlike [`poll_rpc_for_block`], an RPC that has already moved past the target is not treated
/// as stale: the target block is fetched by number, since the caller wants to simulate at that
/// exact (recent) block. Transient RPC failures are retried like a lagging chain — an `Err` is
/// only returned once the attempts are exhausted, and a healthy poll clears earlier failures.
async fn await_target_block(
    rpc_tools: &tycho_test::RPCTools,
    target_block: u64,
    max_attempts: u32,
    poll_interval: Duration,
) -> miette::Result<Option<Block>> {
    let mut last_failure: Option<miette::Report> = None;
    for attempt in 1..=max_attempts {
        match rpc_tools
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
        {
            Ok(Some(latest)) if latest.header.number == target_block => {
                return Ok(Some(latest));
            }
            Ok(Some(latest)) if latest.header.number > target_block => {
                // The chain has moved past the target; fetch it by number. A failure here must
                // not drop the update — keep polling with the remaining attempts instead.
                match rpc_tools
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Number(target_block))
                    .await
                {
                    Ok(Some(target)) => return Ok(Some(target)),
                    Ok(None) => {
                        last_failure = Some(miette!(
                            "RPC reports latest block {} but served no block {target_block} \
                             (attempt {attempt}/{max_attempts})",
                            latest.header.number
                        ));
                    }
                    Err(e) => {
                        last_failure = Some(miette!(e).wrap_err(format!(
                            "Failed to fetch target block {target_block} by number (attempt \
                             {attempt}/{max_attempts})"
                        )));
                    }
                }
            }
            // The chain has not reached the target yet and the RPC is healthy: any earlier
            // failure is stale, the block is simply not there.
            Ok(Some(_)) => last_failure = None,
            Ok(None) => {
                last_failure = Some(miette!(
                    "RPC served no latest block while awaiting target block {target_block} \
                     (attempt {attempt}/{max_attempts})"
                ));
            }
            Err(e) => {
                last_failure = Some(miette!(e).wrap_err(format!(
                    "Failed to fetch the latest block while awaiting target block \
                     {target_block} (attempt {attempt}/{max_attempts})"
                )));
            }
        }
        tokio::time::sleep(poll_interval).await;
    }
    match last_failure {
        Some(failure) => Err(failure),
        None => Ok(None),
    }
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

/// Creates the router overwrites for the execution simulation: no bytecode is replaced, since the
/// deployed contracts are what this test validates, but every executor known for `chain` is marked
/// as activated on the router. That lets an executor be validated while it is unapproved or still
/// inside its 3-day activation timelock, which `Dispatcher._validateExecutor` would otherwise
/// reject.
///
/// This cannot help an executor that has no bytecode deployed at its address.
///
/// # Errors
/// Returns an error if the default swap encoders cannot be built for `chain`, or if the chain has
/// no executors configured.
fn create_router_overwrites_data(chain: Chain) -> miette::Result<RouterOverwritesData> {
    let registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to build the default swap encoders for {chain}"))?;

    let mut executors = HashMap::new();
    for (protocol, executor_address) in registry.executor_addresses() {
        let executor = bytes_to_address(&executor_address)
            .into_diagnostic()
            .wrap_err_with(|| format!("Invalid executor address for protocol {protocol}"))?;
        // Protocol systems share executors, so this deduplicates by address.
        executors.insert(executor, None);
    }
    if executors.is_empty() {
        return Err(miette!("No executors configured for {chain}"));
    }
    debug!("Activating {} executors for the execution simulations", executors.len());

    Ok(RouterOverwritesData { executors, ..Default::default() })
}

#[allow(clippy::too_many_arguments)]
async fn process_update(
    cli: Arc<Cli>,
    chain: Chain,
    rpc_tools: tycho_test::RPCTools,
    tycho_state: Arc<RwLock<TychoState>>,
    statistics: Option<Arc<RwLock<TestStatistics>>>,
    token_prices: SharedTokenPrices,
    router_fee: RouterFeeOnOutput,
    oracle_overrides: Option<Arc<OracleOverrides>>,
    update: &StreamUpdate,
    router_overwrites_data: RouterOverwritesData,
) -> miette::Result<()> {
    info!(
        "Got protocol update with block/timestamp {}, {} new pairs, and {} states",
        update.update.block_number_or_timestamp,
        update.update.new_pairs.len(),
        update.update.states.len()
    );

    let token_prices = token_prices
        .read()
        .map_err(|e| miette!("Failed to acquire read lock on token prices: {e}"))?
        .clone();

    let block = match update.update_type {
        UpdateType::Protocol => {
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

                for (protocol, component_ids) in &current_state.component_ids_by_protocol {
                    metrics::record_protocol_pool_count(protocol, component_ids.len());
                }
            }

            let update_block_number = update.update.block_number_or_timestamp;

            if !is_sampled_block(update_block_number, cli.test_every_n_blocks) {
                metrics::record_protocol_update_sampled_out();
                return Ok(());
            }

            let poll_interval = Duration::from_millis(cli.rpc_poll_interval_ms);

            let by_number =
                should_fetch_block_by_number(cli.test_every_n_blocks, cli.partial_blocks);

            let block = if by_number {
                // On fast chains the head is expected to be past the update, so fetch the target
                // block by number instead of racing the head.
                match await_target_block(
                    &rpc_tools,
                    update_block_number,
                    cli.rpc_poll_attempts,
                    poll_interval,
                )
                .await
                {
                    Ok(Some(b)) => {
                        let latency_seconds =
                            update.received_at.as_secs_f64() - b.header.timestamp as f64;
                        metrics::record_block_processing_duration(latency_seconds, "full");
                        Arc::new(b)
                    }
                    Ok(None) => {
                        warn!("RPC did not serve sampled block {update_block_number}, skipping.");
                        metrics::record_protocol_update_skipped();
                        for protocol in update.update.sync_states.keys() {
                            metrics::record_protocol_sync_state_skipped(protocol);
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        metrics::record_protocol_update_skipped();
                        for protocol in update.update.sync_states.keys() {
                            metrics::record_protocol_sync_state_skipped(protocol);
                        }
                        return Err(e);
                    }
                }
            } else {
                // Flashblocks-capable endpoints expose sequencer pre-confirmed state under
                // `pending`; standard endpoints use `latest` (confirmed blocks only).
                let block_tag = if cli.partial_blocks && update.update.is_partial {
                    BlockNumberOrTag::Pending
                } else {
                    BlockNumberOrTag::Latest
                };

                let poll_result = poll_rpc_for_block(
                    &rpc_tools,
                    update_block_number,
                    block_tag,
                    cli.rpc_poll_attempts,
                    poll_interval,
                )
                .await?;

                let block_type =
                    if block_tag == BlockNumberOrTag::Pending { "partial" } else { "full" };
                match poll_result {
                    BlockPollResult::Ready(b) => {
                        let latency_seconds =
                            update.received_at.as_secs_f64() - b.header.timestamp as f64;
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
                            "RPC ({block_tag}) did not reach update block \
                             {update_block_number}, skipping."
                        );
                        metrics::record_protocol_update_skipped();
                        return Ok(());
                    }
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
                stats.record_block_processed(update.update.block_number_or_timestamp);
            }

            block
        }
        UpdateType::Rfq => {
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
        }
        UpdateType::PriceLevelStream => {
            // The quotes target the block Titan was building, so execution is simulated at
            // exactly that block: the overrides carry its timestamp.
            let target_block = update.update.block_number_or_timestamp;
            let poll_interval = Duration::from_millis(cli.rpc_poll_interval_ms);
            // RPC failures propagate instead of counting as a miss: the miss metric means "the
            // chain did not reach the quoted block", not "the RPC was down".
            match await_target_block(&rpc_tools, target_block, cli.rpc_poll_attempts, poll_interval)
                .await?
            {
                Some(b) => {
                    if let Some(stats) = statistics.as_ref() {
                        let mut stats = stats
                            .write()
                            .expect("Failed to get write lock for statistics (record block)");
                        stats.record_block_processed(target_block);
                    }
                    Arc::new(b)
                }
                None => {
                    metrics::record_price_level_target_block_miss();
                    debug!(
                        target_block,
                        "RPC did not serve the price level update's target block, skipping."
                    );
                    return Ok(());
                }
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

        let validation_block_id = if update.update.is_partial {
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
                    if is_block_not_found(&e.to_string()) {
                        // The RPC node still lags behind Tycho after the batch-validation retries
                        // exhausted: the block genuinely isn't available yet. This is infra
                        // latency, not a state mismatch, so skip it rather than polluting the
                        // validation-failure metric.
                        warn!(
                            component_id = %component_id,
                            "Skipping validation: RPC block not yet available after retries"
                        );
                        continue;
                    }
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
        let token_prices = token_prices.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .into_diagnostic()
            .wrap_err("Failed to acquire permit")?;

        let task = tokio::spawn(async move {
            let simulation_id = generate_simulation_id(&component.protocol_system, &id);
            let result = process_state(
                &simulation_id,
                chain,
                component,
                &block,
                id,
                state,
                statistics,
                token_prices,
            )
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

    if cli.disable_execution {
        return Ok(());
    }

    // Titan publishes overrides per block, so only price level stream updates take them.
    let oracle_overwrites = match update.update_type {
        UpdateType::PriceLevelStream => {
            let overrides = oracle_overrides
                .as_ref()
                .and_then(|overrides| overrides.for_block(block.header.number));
            record_oracle_override_misses(
                &block_execution_info,
                overrides.as_ref(),
                block.number(),
            );
            overrides.map(BlockOverrides::into_storage)
        }
        UpdateType::Protocol | UpdateType::Rfq => None,
    };

    let results = match simulate_swap_transaction(
        &rpc_tools,
        block_execution_info.clone(),
        &block,
        router_overwrites_data,
        oracle_overwrites,
    )
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

        // A `StaleUpdate()` revert is an expected outcome, not an integration failure, so it
        // is recorded separately from the revert metrics.
        if let Some(pamm) = pamm_venue(&execution_info.protocol_system) {
            if let TychoExecutionResult::Revert { reason, .. } = result {
                if is_oracle_stale_revert(pamm, reason) {
                    debug!(
                        block = block.number(),
                        protocol = %execution_info.protocol_system,
                        "pAMM price feed not fresh for the quoted block (block not built by \
                         Titan or update missing); skipping execution comparison"
                    );
                    metrics::record_execution_stale_quote(&execution_info.protocol_system);
                    continue;
                }
            }
        }

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
            router_fee,
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
        warn!(
            "For block {}, simulated {total_simulations} executions, {n_reverts} simulations reverted, {n_failures} executions setup failed",
            block.number()
        )
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
            UpdateType::Rfq | UpdateType::PriceLevelStream => {
                match update.update.new_pairs.get(id) {
                    Some(comp) => comp.clone(),
                    None => {
                        warn!(id=%id, "Component not found in update's new pairs. Potential cause: \
                        the `states` and `new_pairs` lists don't contain the same items. Skipping...");
                        continue;
                    }
                }
            }
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
#[allow(clippy::too_many_arguments)]
async fn process_state(
    simulation_id: &str,
    chain: Chain,
    component: ProtocolComponent,
    block: &Block,
    state_id: String,
    state: Box<dyn ProtocolSim>,
    statistics: Option<Arc<RwLock<TestStatistics>>>,
    token_prices: Arc<HashMap<Bytes, f64>>,
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

        // Cap the limit to a realistic input value (~10k USD) using the weekly token prices.
        // Tick-based protocols (Uniswap V3/V4) report near-infinite limits; tokens missing
        // from the price snapshot are left to the 96-bit safety bound below.
        let mut amount_in = cap_amount_to_eth_value(
            max_input,
            &token_in.address,
            &token_prices,
            MAX_INPUT_VALUE_ETH,
        );
        if amount_in.is_zero() {
            debug!("Calculated amount_in is zero, skipping...");
            continue;
        }
        amount_in = amount_in.max(min_amount.clone());

        // Safety bound for tokens missing from the price snapshot, whose limit is left uncapped:
        // avoids the "amount exceeds 96 bits" error seen on Uniswap V3/V4 with very high limits.
        let max_96_bit = BigUint::from(2u128.pow(96) - 1);
        amount_in = amount_in.min(max_96_bit);

        // Get expected amount out using tycho-simulation and measure duration
        let start_time = std::time::Instant::now();
        let amount_out_result = match state
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .into_diagnostic()
            .wrap_err(format!(
                "Error calculating amount out with input of {amount_in} {}.",
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

        // Sometimes the expected amount out might be zero (e.g. pool is depleted in one direction).
        // Skip: passing expectedAmountOut=0 means minAmountOut=0, which makes the slippage check
        // trivial.
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
            expected_amount_out.clone(),
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
    router_fee: RouterFeeOnOutput,
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

            // Remove the router fee from the expected simulated amount out. The on-chain router
            // deducts this fee from the swap output, so the simulated amount out (which is
            // fee-free) must be reduced by the same fraction before comparing against the
            // executed amount.
            let simulated_amount_out_without_fee = &execution_info.expected_amount_out -
                (&execution_info.expected_amount_out * BigUint::from(router_fee.numerator())) /
                    BigUint::from(router_fee.denominator());

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

/// Whether the run has processed `--max-blocks` blocks. Always false when no cap is set or
/// statistics are disabled.
fn reached_max_blocks(max_blocks: u64, statistics: Option<&Arc<RwLock<TestStatistics>>>) -> bool {
    if max_blocks == 0 {
        return false;
    }
    let Some(stats) = statistics else {
        return false;
    };
    let stats = stats
        .read()
        .expect("Failed to get read lock for statistics (max-block check)");
    stats.blocks_processed >= max_blocks
}

/// True when `block_number` is selected by the `--test-every-n-blocks` sampling interval.
fn is_sampled_block(block_number: u64, interval: u64) -> bool {
    block_number.is_multiple_of(interval)
}

/// True when a sampled update's target block should be fetched by number rather than polled for.
///
/// A flashblock's pending state cannot be fetched by number after the fact, so under
/// `--partial-blocks` sampling only gates which updates are tested.
fn should_fetch_block_by_number(interval: u64, partial_blocks: bool) -> bool {
    interval > 1 && !partial_blocks
}

/// Selector of the priority-update-registry's `StaleUpdate()` error, the freshness guard of the
/// registry-priced pAMMs (FermiSwap, Kipseli, Bebop, TaurusFi).
const STALE_UPDATE_SELECTOR: &str = "666a2814";

/// Selector of `FeedStalled()`, the equivalent freshness guard of the Metric pAMM's own price
/// feed.
const FEED_STALLED_SELECTOR: &str = "9a0423af";

/// The bare venue name of a pAMM component, or `None` for any other protocol system.
fn pamm_venue(protocol_system: &str) -> Option<&str> {
    protocol_system
        .strip_prefix(PRICE_LEVEL_STREAM_PREFIX)
        .or_else(|| protocol_system.strip_prefix(PROPAMM_FALLBACK_PREFIX))
}

/// Counts, per protocol system, the pAMM swaps about to be simulated without the overrides their
/// own venue needs.
///
/// Split in two so a gap Titan could close is not confused with a venue it never serves: a venue
/// whose protocol published nothing for this block counts as a miss, one Titan's override stream
/// carries no channel for counts as unserved. Both fill on the router's Uniswap V3 pool instead of
/// the venue, so neither swap compares a venue's quote against its own fill.
fn record_oracle_override_misses(
    execution_info: &HashMap<String, TychoExecutionInput>,
    overrides: Option<&BlockOverrides>,
    block: u64,
) {
    let mut missing: HashSet<&str> = HashSet::new();
    let mut unserved: HashSet<&str> = HashSet::new();
    for info in execution_info.values() {
        let Some(venue) = pamm_venue(&info.protocol_system) else {
            continue;
        };
        match override_protocol(venue) {
            Some(protocol) => {
                if !overrides.is_some_and(|overrides| overrides.covers(protocol)) {
                    missing.insert(&info.protocol_system);
                }
            }
            None => {
                unserved.insert(&info.protocol_system);
            }
        }
    }

    for protocol in missing {
        debug!(
            block,
            protocol, "Titan published no state overrides for the venue at the quoted block"
        );
        metrics::record_price_level_oracle_override_miss(protocol);
    }
    for protocol in unserved {
        debug!(block, protocol, "Titan's state override stream serves no channel for the venue");
        metrics::record_price_level_oracle_override_unserved(protocol);
    }
}

/// Returns whether a revert reason is the freshness guard of `pamm`: `StaleUpdate()` for every
/// pAMM, plus `FeedStalled()` for Metric.
///
/// Selectors are matched without a `0x` prefix, because a wrapped error carries the inner
/// selector as ABI-encoded bytes.
fn is_oracle_stale_revert(pamm: &str, reason: &str) -> bool {
    if reason.contains("StaleUpdate") || reason.contains(STALE_UPDATE_SELECTOR) {
        return true;
    }
    pamm == "metric" && (reason.contains("FeedStalled") || reason.contains(FEED_STALLED_SELECTOR))
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

#[cfg(test)]
mod tests {
    use clap::Parser;
    use rstest::rstest;

    use super::{
        is_oracle_stale_revert, is_sampled_block, pamm_venue, should_fetch_block_by_number, Cli,
    };

    #[rstest]
    #[case::direct("pricelevelstream:fermiswap", Some("fermiswap"))]
    #[case::through_the_router("propammfallback:fermiswap", Some("fermiswap"))]
    #[case::auto_detected(
        "pricelevelstream:0x5979458912f80b96d30d4220af8e2e4925a33320",
        Some("0x5979458912f80b96d30d4220af8e2e4925a33320")
    )]
    #[case::indexed_pamm("vm:fermiswap", None)]
    #[case::other_protocol("uniswap_v3", None)]
    fn the_venue_is_read_from_either_pamm_family(
        #[case] protocol_system: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(pamm_venue(protocol_system), expected);
    }

    #[rstest]
    #[case::stale_update_name("fermiswap", "execution reverted: StaleUpdate()")]
    #[case::stale_update_bare_selector("kipseli", "execution reverted: 0x666a2814")]
    // Metric's FeedStalled() reaches the router wrapped in an outer error whose decoded reason
    // carries the inner selector only as ABI-encoded bytes, without a `0x` prefix (realistic
    // shape captured from a live run log).
    #[case::feed_stalled_inside_wrapped_error(
        "metric",
        "WrappedError(0xc0b06c4adfabb5be10ddb1dcd1c80caa6742f7bc, \
         0xc1701b6700000000000000000000000000000000000000000000000000000000, \
         0x9a0423af00000000000000000000000000000000000000000000000000000000, 0x)"
    )]
    #[case::feed_stalled_name("metric", "execution reverted: FeedStalled()")]
    fn stale_guard_reverts_are_expected(#[case] pamm: &str, #[case] reason: &str) {
        assert!(is_oracle_stale_revert(pamm, reason));
    }

    #[rstest]
    #[case::negative_slippage("metric", "TychoRouter__NegativeSlippage(1000, 990)")]
    #[case::plain_revert("fermiswap", "execution reverted")]
    #[case::arithmetic("fermiswap", "arithmetic underflow or overflow")]
    #[case::feed_stalled_on_non_metric_pamm("fermiswap", "execution reverted: FeedStalled()")]
    fn other_reverts_stay_real_failures(#[case] pamm: &str, #[case] reason: &str) {
        assert!(!is_oracle_stale_revert(pamm, reason));
    }

    #[rstest]
    #[case::interval_one_selects_every_block(1, 41513952, true)]
    #[case::interval_one_selects_multiples_too(1, 41513950, true)]
    #[case::multiple_of_interval(10, 41513950, true)]
    #[case::not_a_multiple(10, 41513952, false)]
    #[case::block_zero(10, 0, true)]
    fn sampling_selects_multiples_of_interval(
        #[case] interval: u64,
        #[case] block: u64,
        #[case] expected: bool,
    ) {
        assert_eq!(is_sampled_block(block, interval), expected);
    }

    #[test]
    fn sampled_with_partial_blocks_polls_instead_of_fetching_by_number() {
        assert!(!should_fetch_block_by_number(10, true));
    }

    #[test]
    fn test_every_n_blocks_works_with_partial_blocks() {
        let cli = Cli::try_parse_from([
            "tycho-integration-test",
            "--tycho-url",
            "localhost:4242",
            "--rpc-url",
            "http://localhost:8545",
            "--partial-blocks",
            "--test-every-n-blocks",
            "10",
        ])
        .expect("--test-every-n-blocks must be accepted alongside --partial-blocks");
        assert!(cli.partial_blocks);
        assert_eq!(cli.test_every_n_blocks, 10);
    }
}
