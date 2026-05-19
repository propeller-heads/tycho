#![doc = include_str!("../README.md")]

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// TODO: We need to use `use pretty_assertions::{assert_eq, assert_ne}` per test module.
#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

use std::{
    collections::HashMap,
    env,
    fs::File,
    io::Read,
    process, slice,
    str::FromStr,
    sync::{mpsc, Arc},
};

use actix_web::{dev::ServerHandle, web, App, HttpResponse, HttpServer, Responder};
use anyhow::anyhow;
use chrono::{NaiveDateTime, Utc};
use clap::Parser;
use futures03::future::select_all;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::Deserialize;
use tokio::{
    runtime::Handle,
    select,
    signal::unix::{signal, SignalKind},
    task::JoinHandle,
};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::EnvFilter;
use tycho_common::{
    models::{
        blockchain::{Block, Transaction},
        contract::AccountDelta,
        Address, Chain, ExtractionState, ImplementationType,
    },
    storage::{ChainGateway, ContractStateGateway, ExtractionStateGateway},
    traits::{AccountExtractor, StorageSnapshotRequest},
    Bytes,
};
use tycho_ethereum::{
    rpc::EthereumRpcClient,
    services::{
        account_extractor::EVMAccountExtractor, token_pre_processor::EthereumTokenPreProcessor,
    },
};
use tycho_indexer::{
    cli::{AnalyzeTokenArgs, Cli, Command, GlobalArgs, IndexArgs, RunSpkgArgs, SubstreamsArgs},
    extractor::{
        runner::ExtractorHandle,
        supervisor::{
            DCIType, ExtractorConfig, ExtractorFactory, ExtractorSupervisor, ProtocolTypeConfig,
        },
        token_analysis_cron::analyze_tokens,
        ExtractionError,
    },
    services::{PlansConfig, ServicesBuilder},
};
use tycho_storage::postgres::{builder::GatewayBuilder, cache::CachedGateway};

mod ot;

#[derive(Debug, Deserialize)]
struct ExtractorConfigs {
    extractors: std::collections::HashMap<String, ExtractorConfig>,
}

impl ExtractorConfigs {
    fn new(extractors: std::collections::HashMap<String, ExtractorConfig>) -> Self {
        Self { extractors }
    }

    fn from_yaml(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let config: ExtractorConfigs = serde_yaml::from_str(&contents)?;
        Ok(config)
    }
}

type ExtractionTasks = Vec<JoinHandle<Result<(), ExtractionError>>>;
type ServerTasks = Vec<JoinHandle<Result<(), ExtractionError>>>; //TODO: introduce an error type for it

fn main() -> Result<(), anyhow::Error> {
    let cli: Cli = Cli::parse();
    let global_args = cli.args();
    match cli.command() {
        Command::Index(indexer_args) => {
            run_indexer(global_args, indexer_args).map_err(|e| anyhow!(e))?;
        }
        Command::Run(run_args) => {
            run_spkg(global_args, run_args).map_err(|e| anyhow!(e))?;
        }
        Command::AnalyzeTokens(analyze_args) => {
            run_analyze_tokens(global_args, analyze_args).map_err(|e| anyhow!(e))?;
        }
        Command::Rpc => {
            run_rpc(global_args).map_err(|e| anyhow!(e))?;
        }
    };
    Ok(())
}

fn create_tracing_subscriber() {
    // Set up the subscriber
    let console_flag = std::env::var("ENABLE_CONSOLE").unwrap_or_else(|_| "false".to_string());
    if console_flag == "true" {
        console_subscriber::init();
    } else {
        // OTLP endpoint is set, construct OTLP pipeline
        if let Ok(otlp_exporter_endpoint) = std::env::var("OTLP_EXPORTER_ENDPOINT") {
            let config = ot::TracingConfig { otlp_exporter_endpoint };
            ot::init_tracing(config).unwrap();
        } else {
            warn!("OTLP_EXPORTER_ENDPOINT not set defaulting to stdout subscriber!");
            let format = tracing_subscriber::fmt::format()
                .with_level(true)
                .with_target(false)
                .compact();
            tracing_subscriber::fmt()
                .event_format(format)
                .with_env_filter(EnvFilter::from_default_env())
                .init();
        }
    }
}

/// Creates and runs the Prometheus metrics exporter using Actix Web.
pub fn create_metrics_exporter() -> tokio::task::JoinHandle<()> {
    let exporter_builder = PrometheusBuilder::new();
    let handle = exporter_builder
        .install_recorder()
        .expect("Failed to install Prometheus recorder");

    tokio::spawn(async move {
        if let Err(e) = HttpServer::new(move || {
            App::new().route(
                "/metrics",
                web::get().to({
                    let handle = handle.clone();
                    move || metrics_handler(handle.clone())
                }),
            )
        })
        .bind(("0.0.0.0", 9898))
        .expect("Failed to bind metrics server")
        .run()
        .await
        {
            error!("Metrics server failed: {}", e);
        }
    })
}

/// Handles requests to the /metrics endpoint, rendering Prometheus metrics.
async fn metrics_handler(handle: PrometheusHandle) -> impl Responder {
    let metrics = handle.render();
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(metrics)
}

/// Spawns a background task that emits jemalloc allocator stats as Prometheus gauges every 60s.
///
/// Emits `jemalloc_allocated_bytes` (live allocations) and `jemalloc_resident_bytes` (RSS as seen
/// by jemalloc).
#[cfg(feature = "jemalloc")]
fn spawn_jemalloc_stats_reporter() {
    use metrics::gauge;
    use tikv_jemalloc_ctl::{epoch, stats};

    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            // Advance the epoch to refresh stats.
            if epoch::advance().is_err() {
                continue;
            }
            if let Ok(allocated) = stats::allocated::read() {
                gauge!("jemalloc_allocated_bytes").set(allocated as f64);
            }
            if let Ok(resident) = stats::resident::read() {
                gauge!("jemalloc_resident_bytes").set(resident as f64);
            }
        }
    });
}

/// Executes all extractors configured in the extractor configuration file and starts the server.
///
/// Note: This function utilizes two distinct runtimes: one for extraction tasks and another
/// for others operations such as server and gateway.
///
/// By using separate runtimes, extraction processes in Tycho can run independently, ensuring
/// that server-related tasks do not interfere with the extraction workflow, and overall
/// system performance is maintained.
fn run_indexer(global_args: GlobalArgs, index_args: IndexArgs) -> Result<(), ExtractionError> {
    let extraction_threads = std::env::var("EXTRACTION_WORKER_THREADS")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .expect("EXTRACTION_WORKER_THREADS must be a number");
    let main_threads = std::env::var("MAIN_WORKER_THREADS")
        .unwrap_or_else(|_| "3".to_string())
        .parse()
        .expect("MAIN_WORKER_THREADS must be a number");
    // We spawn a dedicated runtime for extraction
    let extraction_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(extraction_threads)
        .enable_all()
        .build()
        .unwrap();

    let main_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(main_threads)
        .enable_all()
        .build()
        .unwrap();

    let (control_tx, control_rx) = mpsc::channel();

    let (extraction_tasks, other_tasks) = main_runtime
        .block_on(async {
            create_tracing_subscriber();
            let _metrics_task = create_metrics_exporter();
            #[cfg(feature = "jemalloc")]
            spawn_jemalloc_stats_reporter();

            info!("Starting Tycho");
            debug!("{} CPUs detected", num_cpus::get());
            let extractors_config = ExtractorConfigs::from_yaml(&index_args.extractors_config)
                .map_err(|e| {
                    ExtractionError::Setup(format!("Failed to load extractors.yaml. {e}"))
                })?;

            let retention_horizon: NaiveDateTime = index_args
                .retention_horizon
                .parse()
                .expect("Failed to parse retention horizon");

            let (extraction_tasks, other_tasks) = create_indexing_tasks(
                &global_args,
                &index_args.substreams_args,
                &index_args
                    .chains
                    .iter()
                    .map(|chain_str| {
                        Chain::from_str(chain_str)
                            .unwrap_or_else(|_| panic!("Unknown chain {chain_str}"))
                    })
                    .collect::<Vec<_>>(),
                retention_horizon,
                extractors_config,
                Some(extraction_runtime.handle()),
                index_args.settlement_contract,
            )
            .await?;

            Ok::<_, ExtractionError>((extraction_tasks, other_tasks))
        })
        .expect("Should not fail during tasks creation");

    let extractor_ctrl_tx = control_tx.clone();
    extraction_runtime.spawn(async move {
        // Wait for ALL supervisors to complete (each manages its own restarts).
        // Only signal the main thread once all extractors are permanently stopped.
        let results = futures03::future::join_all(extraction_tasks).await;
        let mut any_error = false;
        for result in &results {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    error!(error = %err, "Supervisor exited with error");
                    any_error = true;
                }
                Err(join_err) => {
                    error!(error = %join_err, "Supervisor task panicked");
                    any_error = true;
                }
            }
        }

        let res: Result<(), ExtractionError> = if any_error {
            Err(ExtractionError::Unknown(
                "All extractors have stopped — at least one with errors".into(),
            ))
        } else {
            Ok(())
        };

        if extractor_ctrl_tx.send(res).is_err() {
            error!("Fatal: failed to communicate with main thread. Exiting the process...");
            process::exit(1);
        }
    });

    let services_ctrl_tx = control_tx.clone();
    main_runtime.spawn(async move {
        let (res, _, _) = select_all(other_tasks).await;

        if services_ctrl_tx.send(res.unwrap_or_else(|join_err| Err(ExtractionError::Unknown(format!("Task panicked: {join_err}"))))).is_err() {
            error!("Fatal service task exited and failed trying to communicate with main thread. Exiting the process...");
            process::exit(1);
        }
    });

    let res = control_rx
        .recv()
        .expect("Control channel unexpectedly closed");

    match res {
        Ok(()) => Ok(()),
        Err(err) => {
            error!(error = %err, "Fatal error, shutting down");
            Err(err)
        }
    }
}

#[tokio::main]
async fn run_spkg(global_args: GlobalArgs, run_args: RunSpkgArgs) -> Result<(), ExtractionError> {
    create_tracing_subscriber();
    info!("Starting Tycho");

    let dci_plugin = run_args
        .dci_plugin
        .clone()
        .map_or(Ok(None), |s| match s.as_str() {
            "rpc" => Ok(Some(DCIType::RPC)),
            _ => Err(ExtractionError::Setup(format!("Unknown DCI plugin: {s}"))),
        })?;

    let config = ExtractorConfigs::new(HashMap::from([(
        run_args.protocol_system.clone(),
        ExtractorConfig::new(
            run_args.protocol_system.clone(),
            Chain::from_str(&run_args.chain).unwrap(),
            ImplementationType::Vm,
            1, /* TODO: if we want to increase this, we need to commit the cache when we reached
                * `end_block` */
            run_args.start_block,
            run_args.stop_block(),
            run_args
                .protocol_type_names
                .into_iter()
                .map(|name| {
                    ProtocolTypeConfig::new(name, tycho_common::models::FinancialType::Swap)
                })
                .collect::<Vec<_>>(),
            run_args.spkg,
            run_args.module,
            run_args.initialized_accounts,
            run_args.initialization_block,
            None,
            dci_plugin,
            None,
        ),
    )]));

    let (supervisor_tasks, mut other_tasks) = create_indexing_tasks(
        &global_args,
        &run_args.substreams_args,
        &[Chain::from_str(&run_args.chain).unwrap()],
        Utc::now().naive_utc(),
        config,
        None,
        run_args.settlement_contract,
    )
    .await?;

    let mut all_tasks: Vec<JoinHandle<Result<(), ExtractionError>>> = supervisor_tasks;
    all_tasks.append(&mut other_tasks);

    let (res, _, _) = select_all(all_tasks).await;
    res.expect("Tasks should not panic")?;
    Ok(())
}

#[tokio::main]
async fn run_rpc(global_args: GlobalArgs) -> Result<(), ExtractionError> {
    create_tracing_subscriber();

    let rpc_client = global_args.rpc.build_client()?;

    let direct_gw = GatewayBuilder::new(&global_args.database_url)
        .set_chains(&[Chain::Ethereum]) // TODO: handle multichain
        .build_direct_gw()
        .await?;

    info!("Starting Tycho RPC");
    let server_url = format!("http://{}:{}", global_args.server_ip, global_args.server_port);
    let api_key = env::var("AUTH_API_KEY").map_err(|_| {
        ExtractionError::Setup("AUTH_API_KEY environment variable is not set".to_string())
    })?;

    let plans_config = PlansConfig::from_yaml("./plans.yaml").map_err(ExtractionError::Setup)?;

    let (server_handle, server_task) =
        ServicesBuilder::new(direct_gw.clone(), rpc_client.clone(), api_key)
            .prefix(&global_args.server_version_prefix)
            .bind(&global_args.server_ip)
            .port(global_args.server_port)
            .plans_config(plans_config)
            .run()?;
    info!(server_url, "Http and Ws server started");
    let shutdown_task = tokio::spawn(shutdown_handler(server_handle, vec![], None));
    let (res, _, _) = select_all([server_task, shutdown_task]).await;
    res.expect("ServiceTasks shouldn't panic!")
}

/// Creates extraction and server tasks.
async fn create_indexing_tasks(
    global_args: &GlobalArgs,
    substreams_args: &SubstreamsArgs,
    chains: &[Chain],
    retention_horizon: NaiveDateTime,
    extractors_config: ExtractorConfigs,
    extraction_runtime: Option<&Handle>,
    settlement_contract: alloy::primitives::Address,
) -> Result<(ExtractionTasks, ServerTasks), ExtractionError> {
    let rpc_client = global_args.rpc.build_client()?;

    let protocol_systems: Vec<String> = extractors_config
        .extractors
        .keys()
        .cloned()
        .collect();

    let dci_protocols: Vec<String> = extractors_config
        .extractors
        .iter()
        .filter(|(_, cfg)| cfg.dci_plugin.is_some())
        .map(|(name, _)| name.clone())
        .collect();

    let (cached_gw, gw_writer_handle) = GatewayBuilder::new(&global_args.database_url)
        .set_chains(chains)
        .set_protocol_systems(&protocol_systems)
        .set_retention_horizon(retention_horizon)
        .build()
        .await?;
    let token_processor = EthereumTokenPreProcessor::new(
        &rpc_client,
        *chains
            .first()
            .expect("No chain provided"), //TODO: handle multichain?
        settlement_contract,
    );

    // Create the reset channel shared by all supervisors
    let (reset_tx, reset_rx) = tokio::sync::mpsc::channel::<String>(32);

    let (supervisors, extractor_handles, pending_deltas_rxs) = build_all_extractors(
        &extractors_config,
        chains,
        &global_args.endpoint_url,
        global_args.s3_bucket.as_deref(),
        &substreams_args.substreams_api_token,
        &cached_gw,
        global_args.database_insert_batch_size,
        &token_processor,
        &rpc_client,
        extraction_runtime,
        substreams_args.enable_partial_blocks,
        reset_tx,
    )
    .await
    .map_err(|e| ExtractionError::Setup(format!("Failed to create extractors: {e}")))?;

    let server_url = format!("http://{}:{}", global_args.server_ip, global_args.server_port);
    let api_key = env::var("AUTH_API_KEY").map_err(|_| {
        ExtractionError::Setup("AUTH_API_KEY environment variable is not set".to_string())
    })?;
    let plans_config = PlansConfig::from_yaml("./plans.yaml").map_err(ExtractionError::Setup)?;

    let (server_handle, server_task) =
        ServicesBuilder::new(cached_gw.clone(), rpc_client.clone(), api_key)
            .prefix(&global_args.server_version_prefix)
            .bind(&global_args.server_ip)
            .port(global_args.server_port)
            .plans_config(plans_config)
            .dci_protocols(dci_protocols)
            .protocol_systems(protocol_systems)
            .register_extractors(extractor_handles.clone())
            .pending_deltas(pending_deltas_rxs, reset_rx)
            .run()?;
    info!(server_url, "Http and Ws server started");

    let shutdown_task =
        tokio::spawn(shutdown_handler(server_handle, extractor_handles, Some(gw_writer_handle)));

    let runtime = extraction_runtime
        .cloned()
        .unwrap_or_else(|| tokio::runtime::Handle::current());

    let supervisor_tasks = supervisors
        .into_iter()
        .map(|supervisor| runtime.spawn(supervisor.run()))
        .collect::<Vec<_>>();

    Ok((supervisor_tasks, vec![server_task, shutdown_task]))
}

#[allow(clippy::too_many_arguments)]
async fn build_all_extractors(
    config: &ExtractorConfigs,
    chains: &[Chain],
    endpoint_url: &str,
    s3_bucket: Option<&str>,
    substreams_api_token: &str,
    cached_gw: &CachedGateway,
    database_insert_batch_size: usize,
    token_pre_processor: &EthereumTokenPreProcessor,
    rpc_client: &EthereumRpcClient,
    runtime: Option<&tokio::runtime::Handle>,
    partial_blocks: bool,
    reset_tx: tokio::sync::mpsc::Sender<String>,
) -> Result<
    (
        Vec<ExtractorSupervisor>,
        Vec<ExtractorHandle>,
        Vec<tokio::sync::mpsc::Receiver<tycho_indexer::extractor::ExtractorMsg>>,
    ),
    ExtractionError,
> {
    let mut supervisors = Vec::new();
    let mut extractor_handles = Vec::new();
    let mut pending_deltas_rxs = Vec::new();

    let chain = *chains
        .first()
        .expect("No chain provided");

    for extractor_config in config.extractors.values() {
        initialize_accounts(
            extractor_config
                .initialized_accounts
                .clone(),
            extractor_config.initialized_accounts_block,
            rpc_client,
            chain,
            cached_gw,
        )
        .await;

        let runtime_handle = runtime
            .cloned()
            .unwrap_or_else(|| tokio::runtime::Handle::current());

        let factory = ExtractorFactory::new(
            extractor_config.clone(),
            endpoint_url.to_owned(),
            s3_bucket.map(ToString::to_string),
            substreams_api_token.to_owned(),
            chain,
            cached_gw.clone(),
            token_pre_processor.clone(),
            rpc_client.clone(),
            database_insert_batch_size,
            partial_blocks,
            Some(runtime_handle),
        )
        .await?;

        // Create dedicated PendingDeltas channel for this extractor
        let (pd_tx, pd_rx) = tokio::sync::mpsc::channel(256);

        // WS subscription map — shared between supervisor and runner
        let ws_subscriptions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        let supervisor =
            ExtractorSupervisor::new(factory, ws_subscriptions, pd_tx, reset_tx.clone());
        let handle = supervisor.handle();

        supervisors.push(supervisor);
        extractor_handles.push(handle);
        pending_deltas_rxs.push(pd_rx);
    }

    Ok((supervisors, extractor_handles, pending_deltas_rxs))
}

async fn with_transaction<F, Fut, R>(gw: &CachedGateway, block: &Block, f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    gw.start_transaction(block, Some("accountExtractor"))
        .await;
    let result = f().await;
    gw.commit_transaction(0)
        .await
        .expect("Failed to commit transaction");
    result
}

#[instrument(skip_all, fields(n_accounts = %accounts.len(), block_id = block_id))]
async fn initialize_accounts(
    accounts: Vec<Address>,
    block_id: u64,
    rpc: &EthereumRpcClient,
    chain: Chain,
    cached_gw: &CachedGateway,
) {
    if accounts.is_empty() {
        return;
    }
    let (block, extracted_accounts) = get_accounts_data(accounts, block_id, rpc, chain).await;

    info!(block_number = block.number, "Initializing accounts");

    let tx = Transaction {
        hash: Bytes::random(32), //TODO: remove Bytes length assumption
        block_hash: block.hash.clone(),
        from: Bytes::from([0u8; 20]),
        to: None,
        index: 0,
    };

    // First transaction
    with_transaction(cached_gw, &block, || async {
        cached_gw
            .upsert_block(slice::from_ref(&block))
            .await
            .expect("Failed to insert block");

        cached_gw
            .upsert_tx(slice::from_ref(&tx))
            .await
            .expect("Failed to insert tx");
    })
    .await;

    // Process account updates
    for account_update in extracted_accounts.into_values() {
        with_transaction(cached_gw, &block, || async {
            let new_account = account_update.ref_into_account(&tx);
            info!(block_number = block.number, contract_address = ?new_account.address, "NewContract");

            // Insert new accounts
            cached_gw
                .insert_contract(&new_account)
                .await
                .expect("Failed to insert contract");
            cached_gw
                .update_contracts(&[(tx.hash.clone(), account_update)])
                .await
                .expect("Failed to update contract");
        })
        .await;
    }

    with_transaction(cached_gw, &block, || async {
        let state = ExtractionState::new(
            "accountExtractor".to_string(),
            chain,
            None,
            "account_cursor".as_bytes(),
            block.hash.clone(),
        );

        cached_gw
            .save_state(&state)
            .await
            .expect("Failed to save cursor");
    })
    .await;
}

async fn get_accounts_data(
    accounts: Vec<Address>,
    block_id: u64,
    rpc: &EthereumRpcClient,
    chain: Chain,
) -> (Block, HashMap<Bytes, AccountDelta>) {
    let account_extractor = EVMAccountExtractor::new(rpc, chain);

    let block = account_extractor
        .get_block_data(block_id)
        .await
        .expect("Failed to get block data");

    let requests = accounts
        .iter()
        .map(|address| StorageSnapshotRequest { address: address.clone(), slots: None })
        .collect::<Vec<_>>();

    let extracted_accounts: HashMap<Bytes, AccountDelta> = account_extractor
        .get_accounts_at_block(&block, &requests)
        .await
        .expect("Failed to extract accounts");
    (block, extracted_accounts)
}

async fn shutdown_handler(
    server_handle: ServerHandle,
    extractors: Vec<ExtractorHandle>,
    db_write_executor_handle: Option<JoinHandle<()>>,
) -> Result<(), ExtractionError> {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm =
        signal(SignalKind::terminate()).map_err(|e| ExtractionError::Unknown(e.to_string()))?;

    tokio::select! {
        _ = ctrl_c => {
            info!("SIGINT (Ctrl+C) received. Cleaning up...");
        },
        _ = sigterm.recv() => {
            info!("SIGTERM received. Cleaning up...");
        },
    }

    for e in extractors.iter() {
        if let Err(err) = e.stop().await {
            warn!(error = %err, "Failed to stop extractor during shutdown");
        }
    }
    server_handle.stop(true).await;
    if let Some(handle) = db_write_executor_handle {
        handle.abort();
    }
    Ok(())
}

#[tokio::main]
async fn run_analyze_tokens(
    global_args: GlobalArgs,
    analyzer_args: AnalyzeTokenArgs,
) -> Result<(), anyhow::Error> {
    let rpc_client = global_args.rpc.build_client()?;

    create_tracing_subscriber();
    let (cached_gw, gw_writer_thread) = GatewayBuilder::new(&global_args.database_url)
        .set_chains(&[analyzer_args.chain])
        .build()
        .await?;
    let cached_gw = Arc::new(cached_gw);
    let analyze_thread = analyze_tokens(analyzer_args, &rpc_client, cached_gw.clone());
    select! {
         res = analyze_thread => {
            res?;
         },
         res = gw_writer_thread => {
            res?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test_serial_db {
    use once_cell::sync::Lazy;
    use tycho_storage::postgres::testing::run_against_db;

    use super::*;

    static RPC: Lazy<EthereumRpcClient> = Lazy::new(|| {
        let rpc_url = std::env::var("RPC_URL").expect("RPC URL must be set for testing");
        EthereumRpcClient::new(&rpc_url).expect("Failed to create RPC client")
    });

    #[tokio::test]
    #[ignore = "require archive node (RPC)"]
    async fn initialize_account_saves_correct_state() {
        run_against_db(|_| async move {
            let accounts =
                vec![Address::from_str("0xba12222222228d8ba445958a75a0704d566bf2c8").unwrap()];
            let block_id = 20378314;
            let db_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");

            let chain = Chain::Ethereum;

            let (cached_gw, _) = GatewayBuilder::new(&db_url.to_string())
                .set_chains(&[chain])
                .build()
                .await
                .expect("Failed to create Gateway");
            initialize_accounts(accounts, block_id, &RPC, chain, &cached_gw).await;

            let contracts = cached_gw
                .get_contracts(&chain, None, None, true, None)
                .await
                .unwrap()
                .entity;

            assert_eq!(contracts.len(), 1);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "require archive node (RPC)"]
    async fn initialize_multiple_accounts_saves_correct_state() {
        run_against_db(|_| async move {
            let accounts = vec![
                Address::from_str("0xba12222222228d8ba445958a75a0704d566bf2c8").unwrap(),
                Address::from_str("0x3175Df0976dFA876431C2E9eE6Bc45b65d3473CC").unwrap(),
            ];
            let block_id = 20378314;
            let db_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
            let chain = Chain::Ethereum;

            let (cached_gw, _) = GatewayBuilder::new(db_url.as_str())
                .set_chains(&[chain])
                .build()
                .await
                .expect("Failed to create Gateway");

            initialize_accounts(accounts, block_id, &RPC, chain, &cached_gw).await;

            let contracts = cached_gw
                .get_contracts(&chain, None, None, true, None)
                .await
                .unwrap()
                .entity;

            assert_eq!(contracts.len(), 2);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "require archive node (RPC)"]
    async fn initialize_multiple_accounts_different_blocks() {
        run_against_db(|_| async move {
            let accounts =
                vec![Address::from_str("0xba12222222228d8ba445958a75a0704d566bf2c8").unwrap()];
            let block_id = 20378314;
            let db_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
            let chain = Chain::Ethereum;

            let (cached_gw, _) = GatewayBuilder::new(db_url.as_str())
                .set_chains(&[chain])
                .build()
                .await
                .expect("Failed to create Gateway");

            initialize_accounts(accounts, block_id, &RPC, chain, &cached_gw).await;
            let accounts =
                vec![Address::from_str("0x3175Df0976dFA876431C2E9eE6Bc45b65d3473CC").unwrap()];
            initialize_accounts(accounts, 20378315, &RPC, chain, &cached_gw).await;

            let contracts = cached_gw
                .get_contracts(&chain, None, None, true, None)
                .await
                .unwrap()
                .entity;

            assert_eq!(contracts.len(), 2);
        })
        .await;
    }

    #[tokio::test]
    async fn initialize_accounts_handles_empty_accounts() {
        run_against_db(|_| async move {
            let accounts = vec![];
            let block_id = 20378314;
            let rpc_url = "http://localhost:0000";
            let db_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
            let chain = Chain::Ethereum;

            // RPC client won't be used since an account list is empty, so we can create a stub one
            let rpc = EthereumRpcClient::new(rpc_url).expect("Failed to create RPC client");

            let (cached_gw, _) = GatewayBuilder::new(db_url.as_str())
                .set_chains(&[chain])
                .build()
                .await
                .expect("Failed to create Gateway");

            initialize_accounts(accounts, block_id, &rpc, chain, &cached_gw).await;
        })
        .await;
    }
}
