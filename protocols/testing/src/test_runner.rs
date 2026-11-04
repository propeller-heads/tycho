use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Child,
    str::FromStr,
    sync::{Arc, LazyLock, RwLock},
    time::Duration,
};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, U256},
    rpc::types::Block,
};
use futures::StreamExt;
use itertools::Itertools;
use miette::{ensure, miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use num_traits::Zero;
use postgres::NoTls;
use tokio::runtime::Runtime;
use tracing::{debug, error, info, warn};
use tycho_execution::encoding::evm::utils::bytes_to_address;
use tycho_simulation::{
    evm::{
        protocol::{
            u256_num::bytes_to_u256,
            uniswap_v4::hooks::hook_handler_creator::initialize_hook_handlers,
        },
        stream::{ProtocolStreamBuilder, StreamEndPolicy},
    },
    protocol::models::{DecoderContext, ProtocolComponent as ProtocolComponentModel, Update},
    tycho_client::feed::{
        component_tracker::ComponentFilter,
        synchronizer::{Snapshot, StateSyncMessage},
        BlockHeader, FeedMessage,
    },
    tycho_common::{
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            token::Token,
            Chain,
        },
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    },
};
use tycho_test::{
    checks::{
        execution::check_executions_match,
        ladder::{self, executable_max_input, LadderOutput},
        sheet,
    },
    execution::{
        models::{TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction,
    },
    RPCTools,
};

use crate::{
    adapter_builder::AdapterContractBuilder,
    balances::{self, BalanceCheck},
    config::{
        component_directions, Direction, Harness, IntegrationTest, IntegrationTestsConfig,
        LimitKind, ProtocolComponentWithTestConfig, Skip, Tolerance,
    },
    execution,
    report::{CheckId, Checks, Report},
    rpc::RPCProvider,
    state_registry::{register_protocol, register_protocol_with_filter},
    stream,
    tycho_rpc::TychoClient,
    tycho_runner::TychoRunner,
    utils::{build_spkg, extract_initial_block},
};

/// How long the stream is polled before checking on the indexer.
const STREAM_POLL: Duration = Duration::from_secs(30);
/// How long the stream may send nothing before it reaches the end of the range.
const STREAM_IDLE_LIMIT: Duration = Duration::from_secs(600);

static CLONE_TO_BASE_PROTOCOL: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
    HashMap::from([
        ("ethereum-sushiswap-v2", "ethereum-uniswap-v2"),
        ("base-sushiswap-v2", "ethereum-uniswap-v2"),
        ("bsc-lunarbase", "base-lunarbase"),
        ("ethereum-pancakeswap-v2", "ethereum-uniswap-v2"),
        ("arc-uniswap-v2", "ethereum-uniswap-v2"),
        ("arc-uniswap-v3", "ethereum-uniswap-v3-logs-only"),
        ("arc-uniswap-v4-no-hooks", "ethereum-uniswap-v4/no-hooks"),
        ("base-balancer-v3", "ethereum-balancer-v3"),
        ("arbitrum-balancer-v3", "ethereum-balancer-v3"),
        ("gnosis-balancer-v3", "ethereum-balancer-v3"),
        ("base-alienbase-v3", "ethereum-uniswap-v3-logs-only"),
        ("robinhood-sushiswap-v3", "ethereum-uniswap-v3-logs-only"),
        ("robinhood-robinswap-v3", "ethereum-uniswap-v3-logs-only"),
        ("robinhood-gigadex-v3", "ethereum-pancakeswap-v3"),
        ("unichain-curve", "ethereum-curve"),
        ("robinhood-ramses-v3", "polygon-ramses-v3"),
        ("robinhood-ekubo-v3", "ethereum-ekubo-v3"),
        ("bsc-ekubo-v3", "ethereum-ekubo-v3"),
        ("base-ekubo-v3", "ethereum-ekubo-v3"),
        ("robinhood-up-v3", "base-aerodrome-slipstreams"),
        ("robinhood-pancakeswap-infinity-cl", "base-pancakeswap-infinity-cl"),
        ("robinhood-uniswap-v4-with-hooks", "ethereum-uniswap-v4/with-hooks"),
        ("bsc-ring-swap-v2", "ethereum-ring-swap-v2"),
    ])
});

/// The chain the balance check reads, at one block.
struct RpcChain<'a> {
    runner: &'a TestRunner,
    block: u64,
}

impl balances::Chain for RpcChain<'_> {
    fn balance_of(&self, token: &Bytes, holder: &Bytes) -> Result<BigUint, String> {
        let token = bytes_to_address(token).map_err(|e| e.to_string())?;
        let holder = bytes_to_address(holder).map_err(|e| e.to_string())?;
        let balance = self
            .runner
            .runtime
            .block_on(
                self.runner
                    .rpc_provider
                    .get_token_balance(token, holder, self.block),
            )
            .map_err(|e| format!("{e:#}"))?;
        Ok(BigUint::from_bytes_be(&balance.to_be_bytes::<32>()))
    }

    fn call(&self, to: &Bytes, calldata: &Bytes) -> Result<Bytes, String> {
        let to = bytes_to_address(to).map_err(|e| e.to_string())?;
        let returned = self
            .runner
            .runtime
            .block_on(
                self.runner
                    .rpc_provider
                    .call(to, calldata.to_vec().into(), self.block),
            )
            .map_err(|e| format!("{e:#}"))?;
        Ok(Bytes::from(returned.to_vec()))
    }
}

/// Strict mode tests each direction's limit as declared, keyed by lowercase component id;
/// `from_yaml` made sure every direction of every expected component has a declaration.
fn limit_kinds(
    test: &IntegrationTest,
    config: &IntegrationTestsConfig,
) -> HashMap<(String, Direction), LimitKind> {
    test.expected_components
        .iter()
        .flat_map(|component| {
            component_directions(component)
                .into_iter()
                .filter_map(move |direction| {
                    let kind = config
                        .limits_for(component, &direction)?
                        .kind;
                    Some(((component.base.id.to_lowercase(), direction), kind))
                })
        })
        .collect()
}

/// A component's quote sheet: each question it answers, with the answer.
type Sheet = BTreeMap<String, String>;

/// What streaming a range produced, for the checks after it.
struct Streamed {
    /// For each block the stream delivered at or past the stop block, the quote sheet of each
    /// expected component then, by lowercase id. The snapshot is compared with the latest one at
    /// or before its block.
    sheets: BTreeMap<u64, HashMap<String, Sheet>>,
    /// Errors the stream reported.
    errors: Vec<String>,
    coverage: stream::Coverage,
    /// The output of each simulated block, in block order, for execution.
    simulated: Vec<(u64, LadderOutput)>,
}

impl Streamed {
    /// The sheets to compare with the snapshot at `block`.
    fn sheets_at(&self, block: u64) -> miette::Result<&HashMap<String, Sheet>> {
        self.sheets
            .range(..=block)
            .next_back()
            .map(|(_, sheets)| sheets)
            .ok_or_else(|| {
                let last = self
                    .sheets
                    .keys()
                    .next()
                    .map_or_else(|| "before the stop block".to_string(), |b| format!("at {b}"));
                miette!("the stream has no state at the snapshot block {block}: it starts {last}")
            })
    }
}

/// Notes every execution in `inputs` that returned an output, for the report's drift.
fn observe_executions(
    checks: &mut Checks,
    inputs: &HashMap<String, TychoExecutionInput>,
    results: &HashMap<String, TychoExecutionResult>,
) {
    for (simulation_id, input) in inputs {
        if let Some(TychoExecutionResult::Success { amount_out, .. }) = results.get(simulation_id) {
            checks.observe_execution(
                simulation_id,
                &input.component_id,
                &input.expected_amount_out,
                amount_out,
            );
        }
    }
}

pub enum TestType {
    Full(TestTypeFull),
    Range(TestTypeRange),
}

pub struct TestTypeFull {
    pub initial_block: Option<u64>,
}

pub struct TestTypeRange {
    pub match_test: Option<String>,
    /// Where to write the outcome of every check, if anywhere.
    pub report: Option<PathBuf>,
    /// Strict: stream the range while it is indexed, and check it at its changed blocks.
    pub stream: bool,
}

pub struct RunnerConfig {
    pub test_type: TestType,
    /// Directory holding `substreams/` and `adapter-integration/evm/`.
    pub root_path: PathBuf,
    pub chain: Chain,
    pub protocol: String,
    pub db_url: String,
    pub rpc_url: String,
    pub tycho_server_port: u16,
    pub vm_simulation_traces: bool,
    pub reuse_last_sync: bool,
    /// Skip compiling the Substreams WASM binaries and pack the ones already present.
    pub prebuilt_wasm: bool,
}

pub struct TestRunner {
    test_type: TestType,
    /// The package name as given on the command line, clone names included.
    protocol: String,
    chain: Chain,
    db_url: String,
    tycho_server_port: u16,
    substreams_path: PathBuf,
    config_file_path: PathBuf,
    vm_simulation_traces: bool,
    adapter_contract_builder: AdapterContractBuilder,
    runtime: Runtime,
    rpc_provider: RPCProvider,
    protocol_components: Arc<RwLock<HashMap<String, ProtocolComponentModel>>>,
    reuse_last_sync: bool,
    prebuilt_wasm: bool,
}

impl TestRunner {
    pub fn new(config: RunnerConfig) -> miette::Result<Self> {
        let RunnerConfig {
            test_type,
            root_path,
            chain,
            protocol,
            db_url,
            rpc_url,
            tycho_server_port,
            vm_simulation_traces,
            reuse_last_sync,
            prebuilt_wasm,
        } = config;

        let base_protocol = CLONE_TO_BASE_PROTOCOL
            .get(protocol.as_str())
            .unwrap_or(&protocol.as_str())
            .to_string();
        let substreams_path = root_path
            .join("substreams")
            .join(&base_protocol);
        let evm_path = root_path.join("adapter-integration/evm");
        let adapter_contract_builder =
            AdapterContractBuilder::new(evm_path.to_string_lossy().to_string());

        // Calculate config file path based on protocol. Clone protocols use their full name
        // in the integration test filename (e.g. integration_test_unichain_curve.tycho.yaml).
        let config_file_name = if protocol != base_protocol {
            format!("integration_test_{}.tycho.yaml", protocol.replace('-', "_"))
        } else {
            "integration_test.tycho.yaml".to_string()
        };
        let config_file_path = substreams_path.join(&config_file_name);

        let rpc_provider = RPCProvider::new(&rpc_url);
        let runtime = Runtime::new().into_diagnostic()?;

        Ok(Self {
            test_type,
            protocol,
            chain,
            db_url,
            tycho_server_port,
            substreams_path,
            config_file_path,
            vm_simulation_traces,
            adapter_contract_builder,
            runtime,
            rpc_provider,
            reuse_last_sync,
            prebuilt_wasm,
            protocol_components: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn run(&self) -> miette::Result<()> {
        let terminal_width = termsize::get()
            .map(|size| size.cols as usize - 35) // Remove length of log prefix (35)
            .unwrap_or(80);
        info!("{}\n", "-".repeat(terminal_width));

        // Skip if test files don't exist
        if !self.config_file_path.exists() {
            warn!("Config file not found at {}.", self.config_file_path.display());
            return Ok(());
        }

        let config = Self::parse_config(&self.config_file_path)?;

        let substreams_yaml_path = self
            .substreams_path
            .join(&config.substreams_yaml_path);
        if !substreams_yaml_path.exists() {
            warn!("substreams.yaml file not found at {}", substreams_yaml_path.display());
            return Ok(());
        }

        match &self.test_type {
            TestType::Full(test_type) => {
                self.runtime.block_on(async {
                    self.run_full_test(config, &substreams_yaml_path, test_type)
                        .await
                })?;
            }
            TestType::Range(test_type) => {
                self.run_tests_in_range(&config, &substreams_yaml_path, test_type, terminal_width)?;
            }
        }

        Ok(())
    }

    fn parse_config(config_yaml_path: &PathBuf) -> miette::Result<IntegrationTestsConfig> {
        info!("Parsing config YAML at {}", config_yaml_path.display());
        let file = std::fs::File::open(config_yaml_path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to open {}", config_yaml_path.display()))?;
        IntegrationTestsConfig::from_yaml(file, &config_yaml_path.display().to_string())
    }

    async fn run_full_test(
        &self,
        config: IntegrationTestsConfig,
        substreams_yaml_path: &PathBuf,
        test_type: &TestTypeFull,
    ) -> miette::Result<()> {
        let start_block = match test_type.initial_block {
            Some(b) => b,
            None => {
                let yaml = std::fs::read_to_string(substreams_yaml_path)
                    .into_diagnostic()
                    .wrap_err_with(|| {
                        format!("Failed to read {}", substreams_yaml_path.display())
                    })?;
                extract_initial_block(&yaml).wrap_err_with(|| {
                    format!(
                        "Failed to determine the initial block from {}",
                        substreams_yaml_path.display()
                    )
                })?
            }
        };
        // Only an explicit override rewrites the manifest — a start block derived from the manifest
        // is where the package already starts.
        let spkg_path =
            build_spkg(substreams_yaml_path, test_type.initial_block, self.prebuilt_wasm)
                .wrap_err("Failed to build spkg")?;
        let initialized_accounts = config
            .initialized_accounts
            .clone()
            .unwrap_or_default();

        // Use tycho-indexer's Index command which handles both continuous syncing and RPC server
        let tycho_runner = self
            .tycho_runner(initialized_accounts)
            .await?;

        let spkg_path_for_index = spkg_path.clone();
        let protocol_type_names = config.protocol_type_names.clone();
        let protocol_system = config.protocol_system.clone();
        let module_name = config.module_name.clone();

        let _index_handle = std::thread::spawn(move || {
            if let Err(e) = tycho_runner.run_tycho_index(
                &spkg_path_for_index,
                start_block,
                &protocol_type_names,
                &protocol_system,
                module_name.as_deref(),
            ) {
                error!("Tycho Index command failed: {:#}", e);
            }
        });

        // Wait for protocol to be synced before starting live testing
        let tycho_client = TychoClient::new(&self.tycho_http_url(), Some("dummy".to_string()))
            .into_diagnostic()
            .wrap_err("Failed to create Tycho client for sync check")?;

        tycho_client
            .wait_for_protocol_sync(&config.protocol_system, self.chain)
            .await
            .into_diagnostic()
            .wrap_err("Failed to wait for protocol sync")?;

        self.run_tvl_import()?;

        // Start live testing with streaming (updates will come as indexer catches up)
        self.run_live_testing(&config).await
    }

    /// Runs live testing by streaming from Tycho RPC and processing updates in real-time.
    /// Processes each update immediately with simulation and batched execution for scalability.
    async fn run_live_testing(&self, config: &IntegrationTestsConfig) -> miette::Result<()> {
        info!("Starting live testing for protocol {}", &config.protocol_system);

        let chain = self.chain;
        // Load tokens for the stream
        let all_tokens = tycho_simulation::utils::load_all_tokens(
            &self.tycho_host(),
            true,
            Some("dummy"),
            true,
            chain,
            None,
            None,
        )
        .await
        .into_diagnostic()
        .wrap_err("Failed to load tokens from Tycho RPC")?;

        tycho_simulation::evm::engine_db::SHARED_TYCHO_DB
            .clear()
            .into_diagnostic()?;

        let protocol_stream_builder =
            ProtocolStreamBuilder::new(&format!("{}/", self.tycho_host()), chain)
                .skip_state_decode_failures(true);

        let adapter_contract_path = self.get_adapter_contract_path(
            config.adapter_contract.as_deref(),
            config
                .adapter_build_signature
                .as_deref(),
            config.adapter_build_args.as_deref(),
        )?;
        let adapter_contract_path_str = adapter_contract_path
            .as_ref()
            .map(|p| p.to_str().unwrap());

        let mut decoder_context = DecoderContext::new().vm_traces(self.vm_simulation_traces);
        if let Some(vm_adapter_path) = adapter_contract_path_str {
            decoder_context = decoder_context.vm_adapter_path(vm_adapter_path);
        }
        let protocol_stream_builder = register_protocol(
            protocol_stream_builder,
            &config.protocol_system,
            chain,
            decoder_context,
        );

        let stream_builder = protocol_stream_builder
            .skip_state_decode_failures(true)
            .set_tokens(all_tokens)
            .await;

        let stream = stream_builder
            .build()
            .await
            .into_diagnostic()
            .wrap_err("Failed to build protocol stream")?;
        tokio::pin!(stream);

        info!("Live testing started. Processing stream updates...");

        // Process stream updates sequentially
        while let Some(result) = stream.next().await {
            match result {
                Ok(update) => {
                    info!(
                        "Received protocol update with {} new pairs and {} states for block {}",
                        update.new_pairs.len(),
                        update.states.len(),
                        update.block_number
                    );

                    match self.protocol_components.write() {
                        Ok(mut components) => {
                            for (id, component) in update.new_pairs.iter() {
                                components.insert(id.clone(), component.clone());
                            }
                        }
                        Err(e) => {
                            error!("Failed to acquire write lock on protocol components: {:#}. Skipping component update.", e);
                        }
                    }

                    let protocol_components: HashMap<String, ProtocolComponentModel> = match self
                        .protocol_components
                        .read()
                    {
                        Ok(components) => update
                            .states
                            .keys()
                            .filter_map(|id| {
                                components
                                    .get(id)
                                    .map(|comp| (id.clone(), comp.clone()))
                            })
                            .collect(),
                        Err(e) => {
                            error!("Failed to acquire read lock on protocol components: {:#}. Using fallback to new_pairs only.", e);
                            // Fallback to the old behavior if we can't read the persistent state
                            update
                                .new_pairs
                                .iter()
                                .map(|(id, component)| (id.clone(), component.clone()))
                                .collect()
                        }
                    };

                    // Step 1: Run simulation get amount out
                    let execution_data = match self.run_simulation(
                        &update,
                        &protocol_components,
                        &[], // No skip filters for live testing
                        &config.protocol_system,
                        None,
                    ) {
                        Ok(output) => output.executions,
                        Err(e) => {
                            error!("Failed to run simulation: {:#}", e);
                            continue;
                        }
                    };

                    if execution_data.is_empty() {
                        info!("No simulation data to execute for this update");
                        continue;
                    }

                    info!(
                        "Gathered {} simulation entries, sleeping for block time...",
                        execution_data.len()
                    );

                    // Step 2: Get the actual block from RPC
                    let block = match self
                        .rpc_provider
                        .get_block(BlockNumberOrTag::Number(update.block_number))
                        .await
                    {
                        Ok(block) => block,
                        Err(e) => {
                            error!("Failed to fetch block {}: {}", update.block_number, e);
                            continue;
                        }
                    };

                    info!(
                        "Fetched block {}, executing {} execution simulations...",
                        block.header.number,
                        execution_data.len()
                    );

                    // Step 3: Execute the batch against the real block
                    match self
                        .run_execution(
                            execution_data,
                            &block,
                            &config.protocol_system,
                            &[], // No skip filters for live testing
                        )
                        .await
                    {
                        Ok(_) => {
                            info!(
                                "Batch execution completed successfully for block {}",
                                block.header.number
                            );
                        }
                        Err(e) => {
                            error!(
                                "Batch execution failed for block {}: {}",
                                block.header.number, e
                            );
                        }
                    }
                }
                Err(e) => {
                    error!("Stream error: {:#}", e);
                    // Continue processing instead of breaking - streams can have temporary errors
                }
            }
        }

        info!("Stream ended, live testing completed");
        Ok(())
    }

    fn run_tests_in_range(
        &self,
        config: &IntegrationTestsConfig,
        substreams_yaml_path: &PathBuf,
        test_type: &TestTypeRange,
        terminal_width: usize,
    ) -> miette::Result<()> {
        let tests = match &test_type.match_test {
            Some(filter) => config
                .tests
                .iter()
                .filter(|test| test.name.contains(filter))
                .collect::<Vec<&IntegrationTest>>(),
            None => config
                .tests
                .iter()
                .collect::<Vec<&IntegrationTest>>(),
        };
        let tests_count = tests.len();

        info!("Running {} tests on Chain {}...\n", tests_count, self.chain);

        let mut failed_tests: Vec<String> = Vec::new();
        let mut report = Report {
            package: self.protocol.clone(),
            chain: self.chain.to_string(),
            harness: config.harness,
            results: Vec::new(),
            drift: Vec::new(),
            coverage: Vec::new(),
        };

        for (idx, test) in tests.iter().enumerate() {
            info!("TEST {}: {}", idx + 1, test.name);
            let mut initialized_accounts = config
                .initialized_accounts
                .clone()
                .unwrap_or_default();
            initialized_accounts.extend(
                test.initialized_accounts
                    .clone()
                    .unwrap_or_default(),
            );
            let tycho_runner = self
                .runtime
                .block_on(self.tycho_runner(initialized_accounts))?;
            let mut checks = Checks::new(config.harness, &test.name);
            let streaming = config.harness == Harness::Strict && test_type.stream;
            let mut streamed = None;
            if self.reuse_last_sync {
                info!("Skipping indexing and using existent DB");
                if streaming {
                    let reason = "--reuse-last-sync reads an indexed database, with no stream";
                    checks.skip(CheckId::StreamMatchesSnapshot, reason);
                    checks.skip(CheckId::StreamCoverage, reason);
                }
            } else {
                let indexed =
                    build_spkg(substreams_yaml_path, Some(test.start_block), self.prebuilt_wasm)
                        .wrap_err("Failed to build spkg")
                        .and_then(|spkg_path| {
                            if streaming {
                                streamed = Some(self.index_streaming(
                                    &tycho_runner,
                                    &spkg_path,
                                    test,
                                    config,
                                    &mut checks,
                                )?);
                                return Ok(());
                            }
                            tycho_runner
                                .run_tycho(
                                    &spkg_path,
                                    test.start_block,
                                    test.stop_block,
                                    &config.protocol_type_names,
                                    &config.protocol_system,
                                    config.module_name.as_deref(),
                                )
                                .wrap_err("Failed to run Tycho")
                        });
                if let Err(e) = indexed {
                    checks.fail(CheckId::Index, &e);
                    report.results.extend(checks.finish().1);
                    Self::write_report(test_type, &report)?;
                    return Err(e);
                }
            }
            if let Some(Ok(streamed)) = &mut streamed {
                self.execute_streamed(streamed, config, &mut checks);
            }
            let rpc_server = tycho_runner.start_rpc_server();
            let result = match self
                .runtime
                .block_on(self.last_indexed_block(&config.protocol_system))
                .and_then(|last_indexed| snapshot_block(last_indexed, test.stop_block))
            {
                Ok(block) => {
                    info!(
                        "Reading the snapshot at block {block}, the last one the indexer committed \
                         (stop block {})",
                        test.stop_block
                    );
                    let run = self.run_test(
                        test,
                        config,
                        block,
                        streamed.as_ref().map(|streamed| {
                            streamed
                                .as_ref()
                                .map_err(String::as_str)
                        }),
                        &mut checks,
                    );
                    match streamed {
                        Some(Ok(streamed)) => {
                            let floor_problems = streamed.coverage.shortfalls(
                                stream::Floor::DEFAULT,
                                &simulated_ids(test),
                                &executed_ids(test),
                            );
                            checks.warn(CheckId::StreamCoverage, &floor_problems);
                            report.coverage.push(streamed.coverage);
                        }
                        Some(Err(_)) => {
                            checks.not_run(CheckId::StreamCoverage, CheckId::StreamMatchesSnapshot)
                        }
                        None => {}
                    }
                    report.drift.extend(checks.take_drift());
                    let (outcome, results) = checks.finish();
                    report.results.extend(results);
                    run.and(outcome)
                }
                Err(e) => {
                    checks.fail(CheckId::Index, &e);
                    report.results.extend(checks.finish().1);
                    Err(e)
                }
            };
            match result {
                Ok(_) => {
                    info!("✅ {} passed\n", test.name);
                }
                Err(e) => {
                    failed_tests.push(test.name.clone());
                    error!("❗️{} failed: {:#}\n", test.name, e);
                }
            }
            TychoRunner::stop_rpc_server(rpc_server)?;
            info!("{}\n", "-".repeat(terminal_width));
        }

        Self::write_report(test_type, &report)?;

        info!("Tests finished!");
        info!("Passed {}/{}\n", tests_count - failed_tests.len(), tests_count);
        if !failed_tests.is_empty() {
            Err(miette!("Failed tests: {}", failed_tests.join(", ")))
        } else {
            Ok(())
        }
    }

    /// Runs the checks of one test on the snapshot at `block`, recording each in `checks`.
    ///
    /// In legacy mode the first failing check returns its error. In strict mode every check that
    /// does not depend on a failed one still runs, and the caller reads the outcome from `checks`.
    ///
    /// `streamed` is the outcome of streaming the range, when it was streamed: the stream's state
    /// must then quote exactly as the snapshot's.
    fn run_test(
        &self,
        test: &IntegrationTest,
        config: &IntegrationTestsConfig,
        block: u64,
        streamed: Option<Result<&Streamed, &str>>,
        checks: &mut Checks,
    ) -> miette::Result<()> {
        let expected_ids = test
            .expected_components
            .iter()
            .map(|c| c.base.id.to_lowercase())
            .collect::<Vec<String>>();
        // Strict: the components whose balances are declared to feed only TVL, which the runner
        // verifies by quoting them again with their balances halved.
        let tvl_only_ids: Vec<String> = match config.harness {
            Harness::Strict => test
                .expected_components
                .iter()
                .filter(|component| component.tvl_only.is_some())
                .map(|component| component.base.id.clone())
                .collect(),
            Harness::Legacy => Vec::new(),
        };

        let Some((header, (protocol_components, snapshot, all_tokens))) =
            checks.run(CheckId::Index, || {
                // The header carries the timestamp the decoder and the execution run with.
                let header = self
                    .runtime
                    .block_on(
                        self.rpc_provider
                            .get_block_header(block),
                    )
                    .wrap_err("Failed to get block header")?;
                let fetched =
                    self.fetch_from_tycho_rpc(&config.protocol_system, expected_ids, block)?;
                Ok((header, fetched))
            })?
        else {
            for id in [
                CheckId::StateComponents,
                CheckId::StateExcluded,
                CheckId::Balances,
                CheckId::Decode,
                CheckId::SimLadder,
                CheckId::ExecLadder,
            ] {
                checks.not_run(id, CheckId::Index);
            }
            if streamed.is_some() {
                checks.not_run(CheckId::StreamMatchesSnapshot, CheckId::Index);
            }
            if !tvl_only_ids.is_empty() {
                checks.not_run(CheckId::BalancesTvlOnly, CheckId::Index);
            }
            return Ok(());
        };

        let response_protocol_states_by_id: HashMap<String, ProtocolComponentState> = snapshot
            .states
            .clone()
            .into_iter()
            .map(|(id, component_with_state)| (id.to_lowercase(), component_with_state.state))
            .collect();

        let tokens_by_component: HashMap<String, Vec<Token>> = protocol_components
            .iter()
            .map(|component| {
                let tokens = component
                    .tokens
                    .iter()
                    .filter_map(|token| all_tokens.get(token).cloned())
                    .collect();
                (component.id.clone(), tokens)
            })
            .collect();

        checks.run(CheckId::StateComponents, || {
            Self::validate_state(&test.expected_components, &protocol_components)
        })?;
        checks.run(CheckId::StateExcluded, || {
            Self::validate_excluded_components(&test.excluded_components, &protocol_components)
        })?;

        if config.harness == Harness::Strict {
            // `from_yaml` rejected a package-level skip.
            for component in &test.expected_components {
                let tolerance = component
                    .tvl_only
                    .as_ref()
                    .map_or(Tolerance::EXACT, |tvl_only| tvl_only.tolerance());
                let failures = self.check_component_balances(
                    component,
                    tolerance,
                    &protocol_components,
                    &response_protocol_states_by_id,
                    &all_tokens,
                    block,
                );
                checks.record_component(CheckId::Balances, &component.base.id, &failures);
            }
        } else if let Some(reason) = config.skip_balance_check.reason() {
            info!("Skipping balance check: {reason}");
            checks.skip(CheckId::Balances, reason);
        } else {
            checks.run(CheckId::Balances, || {
                self.validate_token_balances(
                    &tokens_by_component,
                    &response_protocol_states_by_id,
                    block,
                )?;
                info!("All token balances match the values found onchain");
                Ok(())
            })?;
        }

        for component in &test.expected_components {
            let id = &component.base.id;
            if let Some(reason) = component.skip_simulation.reason() {
                checks.skip_component(CheckId::SimLadder, id, reason);
                checks.skip_component(CheckId::ExecLadder, id, reason);
            } else if let Some(reason) = component.skip_execution.reason() {
                checks.skip_component(CheckId::ExecLadder, id, reason);
            }
        }

        let adapter_contract_path = || {
            self.get_adapter_contract_path(
                config.adapter_contract.as_deref(),
                config
                    .adapter_build_signature
                    .as_deref(),
                config.adapter_build_args.as_deref(),
            )
        };
        // Taken before the snapshot is decoded, which consumes it.
        let halved = (!tvl_only_ids.is_empty()).then(|| {
            let mut halved = snapshot.clone();
            let changed = tycho_test::checks::tvl_only::halve_balances(&mut halved, &tvl_only_ids);
            (halved, changed, all_tokens.clone())
        });

        let Some(update) = checks.run(CheckId::Decode, || {
            let adapter_contract_path_str = adapter_contract_path()?;
            let update = self.decode_snapshot(
                &config.protocol_system,
                &header,
                snapshot,
                all_tokens,
                adapter_contract_path_str.as_deref(),
                self.vm_simulation_traces,
            )?;
            let undecoded = undecoded_components(&test.expected_components, update.states.keys());
            ensure!(
                undecoded.is_empty(),
                "Expected components failed to decode, see the StateDecodingFailure warnings: {}",
                undecoded.join(", ")
            );
            Ok(update)
        })?
        else {
            checks.not_run(CheckId::SimLadder, CheckId::Decode);
            checks.not_run(CheckId::ExecLadder, CheckId::Decode);
            if halved.is_some() {
                checks.not_run(CheckId::BalancesTvlOnly, CheckId::Decode);
            }
            if streamed.is_some() {
                checks.not_run(CheckId::StreamMatchesSnapshot, CheckId::Decode);
            }
            return Ok(());
        };

        if let Some(streamed) = streamed {
            // Before anything decodes again: VM states read the shared database the next decode
            // replaces.
            checks.run(CheckId::StreamMatchesSnapshot, || {
                let streamed = streamed.map_err(|e| miette!("the stream failed: {e}"))?;
                Self::check_stream_matches_snapshot(test, streamed, &update, block)
            })?;
        }

        if let Some((snapshot, changed, all_tokens)) = halved {
            // Decoding again replaces the VM database with the same storage, so the states
            // decoded above still simulate as before.
            let decoded = adapter_contract_path().and_then(|path| {
                self.decode_snapshot(
                    &config.protocol_system,
                    &header,
                    snapshot,
                    all_tokens,
                    path.as_deref(),
                    self.vm_simulation_traces,
                )
            });
            for id in &tvl_only_ids {
                let failures = match &decoded {
                    Ok(halved) => Self::check_tvl_only(id, &update, halved, &changed),
                    Err(e) => {
                        vec![format!("decoding the snapshot with halved balances failed: {e:#}")]
                    }
                };
                checks.record_component(CheckId::BalancesTvlOnly, id, &failures);
            }
        }

        let protocol_components_simulation: HashMap<String, ProtocolComponentModel> =
            update.new_pairs.clone();

        let limit_kinds = (config.harness == Harness::Strict).then(|| limit_kinds(test, config));

        // A test-wide ladder result covers the components without a skip. When every component
        // skips, nothing runs, and the ladder is reported skipped rather than passed.
        let every_component = |skips: fn(&ProtocolComponentWithTestConfig) -> bool| {
            !test.expected_components.is_empty() &&
                test.expected_components
                    .iter()
                    .all(skips)
        };
        if every_component(|c| c.skip_simulation.is_skipped()) {
            let reason = "every expected component skips simulation";
            checks.skip(CheckId::SimLadder, reason);
            checks.skip(CheckId::ExecLadder, reason);
            if limit_kinds.is_some() {
                for id in
                    [CheckId::LimitsAtLimit, CheckId::LimitsPastLimit, CheckId::LimitsMaxOutput]
                {
                    checks.skip(id, reason);
                }
            }
            return Ok(());
        }

        let Some(output) = checks.run(CheckId::SimLadder, || {
            self.run_simulation(
                &update,
                &protocol_components_simulation,
                &test.expected_components,
                &config.protocol_system,
                limit_kinds.as_ref(),
            )
        })?
        else {
            checks.not_run(CheckId::ExecLadder, CheckId::SimLadder);
            if limit_kinds.is_some() {
                for id in
                    [CheckId::LimitsAtLimit, CheckId::LimitsPastLimit, CheckId::LimitsMaxOutput]
                {
                    checks.not_run(id, CheckId::SimLadder);
                }
            }
            return Ok(());
        };

        let executes_nothing =
            every_component(|c| c.skip_simulation.is_skipped() || c.skip_execution.is_skipped())
                .then_some("every expected component skips execution");
        if limit_kinds.is_some() {
            return self.check_strict_executions(
                &output,
                Some(&header),
                &config.protocol_system,
                config.tolerance(),
                executes_nothing,
                checks,
            );
        }
        if let Some(reason) = executes_nothing {
            checks.skip(CheckId::ExecLadder, reason);
            return Ok(());
        }

        // The drift is noted whether or not the executions match, so a failing run still
        // reports how far off it was.
        let mut executed = None;
        let ladder = checks.run(CheckId::ExecLadder, || {
            let (inputs, results) = self
                .runtime
                .block_on(self.execute_unskipped(
                    output.executions,
                    &header,
                    &config.protocol_system,
                    &test.expected_components,
                ))?;
            let matched = check_executions_match(&inputs, &results, config.tolerance());
            executed = Some((inputs, results));
            matched
        });
        if let Some((inputs, results)) = &executed {
            observe_executions(checks, inputs, results);
        }
        ladder?;

        Ok(())
    }

    /// Writes `report` to the path the range command was given, if it was given one.
    fn write_report(test_type: &TestTypeRange, report: &Report) -> miette::Result<()> {
        let Some(path) = &test_type.report else {
            return Ok(());
        };
        report.write(path)?;
        info!("Wrote the test report to {}", path.display());
        Ok(())
    }

    /// Host and port of the spawned tycho-indexer server, e.g. `localhost:4242`.
    fn tycho_host(&self) -> String {
        format!("localhost:{}", self.tycho_server_port)
    }

    /// HTTP URL of the spawned tycho-indexer server, e.g. `http://localhost:4242`.
    fn tycho_http_url(&self) -> String {
        format!("http://{}", self.tycho_host())
    }

    async fn empty_database(&self) -> miette::Result<()> {
        // Split the URL into the server part and the database name. We must connect without the
        // database name because we cannot drop a database that we are currently connected to.
        let (base_url, db_name) = match self.db_url.rfind('/') {
            Some(pos) => (&self.db_url[..pos], &self.db_url[pos + 1..]),
            None => (self.db_url.as_str(), ""),
        };
        if db_name.is_empty() {
            return Err(miette!(
                "Database URL '{}' has no database name; expected e.g. \
                 postgres://user:password@host:port/db_name",
                self.db_url
            ));
        }
        let (client, connection) = tokio_postgres::connect(base_url, NoTls)
            .await
            .into_diagnostic()?;

        // Spawn the connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("Database connection error: {:#}", e);
            }
        });

        // Identifiers cannot be bound as query parameters; quote them, escaping embedded quotes.
        let quoted_name = format!("\"{}\"", db_name.replace('"', "\"\""));
        client
            .execute(&format!("DROP DATABASE IF EXISTS {quoted_name} WITH (FORCE)"), &[])
            .await
            .into_diagnostic()?;
        client
            .execute(&format!("CREATE DATABASE {quoted_name}"), &[])
            .await
            .into_diagnostic()?;

        Ok(())
    }

    /// Returns the last main-chain block committed by the protocol's extractor on this chain,
    /// or `None` when it has no committed extraction state.
    ///
    /// Stopgap: `tycho-indexer run` does not wait for its final commit before exiting, so the
    /// committed block is `stop_block` or `stop_block + 1` depending on how fast the stream
    /// ends. Once the indexer commits through its stop block on stream end, read at
    /// `stop_block` and remove this query, which depends on the indexer's private storage schema.
    async fn last_indexed_block(&self, protocol_system: &str) -> miette::Result<Option<u64>> {
        let (client, connection) = tokio_postgres::connect(&self.db_url, NoTls)
            .await
            .into_diagnostic()
            .wrap_err("Failed to connect to the test database")?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("Database connection error: {:#}", e);
            }
        });

        let row = client
            .query_opt(
                "SELECT b.number FROM extraction_state e \
                 JOIN chain c ON c.id = e.chain_id \
                 JOIN block b ON b.id = e.block_id AND b.chain_id = e.chain_id \
                 WHERE c.name = $1 AND e.name = $2 AND b.main",
                &[&self.chain.to_string(), &protocol_system],
            )
            .await
            .into_diagnostic()
            .wrap_err_with(|| {
                format!(
                    "Failed to query the last indexed block for {protocol_system} on {}",
                    self.chain
                )
            })?;
        row.map(|row| u64::try_from(row.get::<_, i64>(0)))
            .transpose()
            .into_diagnostic()
    }

    async fn tycho_runner(&self, initialized_accounts: Vec<String>) -> miette::Result<TychoRunner> {
        if !self.reuse_last_sync {
            self.empty_database()
                .await
                .wrap_err("Failed to empty the database")?;
        }
        Ok(TychoRunner::new(
            self.chain,
            self.db_url.to_string(),
            self.tycho_server_port,
            initialized_accounts,
        ))
    }

    fn run_tvl_import(&self) -> miette::Result<()> {
        debug!("Running token price import for chain: {}", self.chain);

        const IMPORT_SCRIPT: &str = include_str!("../scripts/import_token_prices.sh");
        const TVL_SCRIPT: &str = include_str!("../scripts/update_component_tvl.sh");

        let temp_dir = std::env::temp_dir();
        let import_script_path = temp_dir.join("import_token_prices.sh");

        std::fs::write(&import_script_path, IMPORT_SCRIPT)
            .into_diagnostic()
            .wrap_err("Failed to write import_token_prices.sh to temp directory")?;

        let import_output = std::process::Command::new("bash")
            .arg(&import_script_path)
            .arg("--chain")
            .arg(self.chain.to_string())
            .arg("--db-url")
            .arg(&self.db_url)
            .output()
            .into_diagnostic()
            .wrap_err("Failed to execute import_token_prices.sh")?;

        if !import_output.status.success() {
            let stderr = String::from_utf8_lossy(&import_output.stderr);
            let stdout = String::from_utf8_lossy(&import_output.stdout);
            error!("Token price import failed: {}, {}", stderr, stdout);
            return Err(miette::miette!("Token price import failed: {}, {}", stderr, stdout));
        }

        let stdout = String::from_utf8_lossy(&import_output.stdout);
        debug!("Imported token prices successfully:\n{}", stdout);

        // Step 2: Update component TVL
        debug!("Running component TVL update");

        let tvl_script_path = temp_dir.join("update_component_tvl.sh");

        std::fs::write(&tvl_script_path, TVL_SCRIPT)
            .into_diagnostic()
            .wrap_err("Failed to write update_component_tvl.sh to temp directory")?;

        let tvl_output = std::process::Command::new("bash")
            .arg(&tvl_script_path)
            .arg("--db-url")
            .arg(&self.db_url)
            .output()
            .into_diagnostic()
            .wrap_err("Failed to execute update_component_tvl.sh")?;

        if !tvl_output.status.success() {
            let stderr = String::from_utf8_lossy(&tvl_output.stderr);
            let stdout = String::from_utf8_lossy(&tvl_output.stdout);
            error!("Component TVL update failed: {}, {}", stderr, stdout);
            return Err(miette::miette!("Component TVL update failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&tvl_output.stdout);
        debug!("Updated component TVL successfully:\n{}", stdout);

        info!("TVL import completed successfully");
        Ok(())
    }

    /// Fetches protocol data from the Tycho RPC server for validation and decoding.
    ///
    /// Retrieves the protocol's components, then the snapshot of the expected components at
    /// `block`, including the contracts and storage slots their traced entry points access.
    ///
    /// # Arguments
    /// * `protocol_system` - The protocol system identifier (e.g., "uniswap_v2", "balancer_v2")
    /// * `expected_component_ids` - Lowercase ids of the components to snapshot; empty means all of
    ///   the protocol's components
    /// * `block` - The block number to fetch the snapshot at
    ///
    /// # Returns
    /// A tuple containing:
    /// - `Vec<ProtocolComponent>` - Every component of the protocol system
    /// - `Snapshot` - States and contract storage of the expected components at `block`
    /// - `HashMap<Bytes, Token>` - All tokens on the chain, by address
    fn fetch_from_tycho_rpc(
        &self,
        protocol_system: &str,
        expected_component_ids: Vec<String>,
        block: u64,
    ) -> miette::Result<(Vec<ProtocolComponent>, Snapshot, HashMap<Bytes, Token>)> {
        info!("Fetching protocol data from Tycho at block {block}...");

        // Create Tycho client for the RPC server
        let tycho_client = TychoClient::new(&self.tycho_http_url(), None)
            .into_diagnostic()
            .wrap_err("Failed to create Tycho client")?;

        let chain = self.chain;

        // Fetch data from Tycho RPC. We use block_on to avoid using async functions on the testing
        // module, in order to simplify debugging
        let protocol_components = self
            .runtime
            .block_on(tycho_client.get_protocol_components(protocol_system, chain))
            .into_diagnostic()
            .wrap_err("Failed to get protocol components")?;

        // If no expected component IDs are provided, use all components from the protocol
        let expected_component_ids = if expected_component_ids.is_empty() {
            protocol_components
                .iter()
                .map(|c| c.id.to_lowercase())
                .collect::<Vec<String>>()
        } else {
            expected_component_ids
        };

        let traced_entry_points = self
            .runtime
            .block_on(tycho_client.get_traced_entry_points(
                protocol_system,
                expected_component_ids.clone(),
                chain,
            ))
            .into_diagnostic()
            .wrap_err("Failed to get trace points")?;

        // Create a map of component IDs to components for easy lookup
        let mut components_by_id: HashMap<String, ProtocolComponent> = protocol_components
            .clone()
            .into_iter()
            .map(|c| (c.id.to_lowercase(), c))
            .collect();
        if !expected_component_ids.is_empty() {
            components_by_id.retain(|id, _| expected_component_ids.contains(id))
        };

        let contract_ids: Vec<Bytes> = protocol_components
            .clone()
            .into_iter()
            .flat_map(|component| component.contract_addresses)
            .chain(
                traced_entry_points
                    .values()
                    .flatten()
                    .flat_map(|(_, results)| results.accessed_slots.keys().cloned()),
            )
            .collect();

        let snapshot = self
            .runtime
            .block_on(tycho_client.get_snapshots(
                chain,
                block,
                protocol_system,
                &components_by_id,
                &contract_ids,
                &traced_entry_points,
            ))
            .into_diagnostic()
            .wrap_err("Failed to get snapshot")?;

        debug!("Found {} protocol components", components_by_id.len());
        debug!("Found {} protocol states", snapshot.states.len());
        debug!("Found {} traced entry points", traced_entry_points.len());

        let all_tokens = self
            .runtime
            .block_on(tycho_client.get_tokens(self.chain, None, None))
            .into_diagnostic()
            .wrap_err("Failed to get tokens")?;
        debug!("Loaded {} tokens", all_tokens.len());

        Ok((protocol_components, snapshot, all_tokens))
    }

    fn decode_snapshot(
        &self,
        protocol_system: &str,
        block: &Block,
        snapshot: Snapshot,
        all_tokens: HashMap<Bytes, Token>,
        adapter_contract_path: Option<&Path>,
        vm_simulation_traces: bool,
    ) -> miette::Result<Update> {
        // Clear the shared database state to ensure test isolation
        // This prevents state from previous tests from affecting the current test
        tycho_simulation::evm::engine_db::SHARED_TYCHO_DB
            .clear()
            .into_diagnostic()?;

        let protocol_stream_builder =
            ProtocolStreamBuilder::new("", self.chain).skip_state_decode_failures(true);

        let mut decoder_context = DecoderContext::new().vm_traces(vm_simulation_traces);
        if let Some(vm_adapter_path) = adapter_contract_path {
            if let Some(path_str) = vm_adapter_path.to_str() {
                decoder_context = decoder_context.vm_adapter_path(path_str);
            }
        }
        let protocol_stream_builder = register_protocol(
            protocol_stream_builder,
            protocol_system,
            self.chain,
            decoder_context,
        );

        let decoder = protocol_stream_builder.get_decoder();
        initialize_hook_handlers().into_diagnostic()?;

        let state_msgs: HashMap<String, StateSyncMessage<BlockHeader>> = HashMap::from([(
            String::from(protocol_system),
            StateSyncMessage {
                header: BlockHeader {
                    hash: (*block.hash()).into(),
                    number: block.number(),
                    parent_hash: Bytes::default(),
                    revert: false,
                    timestamp: block.header.timestamp,
                    partial_block_index: None,
                },
                snapshots: snapshot,
                deltas: None,
                removed_components: HashMap::new(),
            },
        )]);

        self.runtime
            .block_on(decoder.set_tokens(all_tokens));

        let message: FeedMessage = FeedMessage { state_msgs, sync_states: Default::default() };

        let block_msg = self
            .runtime
            .block_on(decoder.decode(&message))
            .into_diagnostic()
            .wrap_err("Failed to decode message")?;
        debug!("Decoded message for block {}", block_msg.block_number);
        debug!("Update contains {} component states", block_msg.states.len());

        Ok(block_msg)
    }

    /// Validates that the protocol components retrieved from Tycho match the expected
    /// configuration.
    ///
    /// This method compares each expected component from the test configuration against
    /// the actual components found in the protocol state update. It ensures that all
    /// expected components are present and their properties (tokens, addresses, fees, etc.)
    /// match the expected values.
    ///
    /// # Arguments
    /// * `expected_components` - Vector of expected protocol components with their test
    ///   configuration
    /// * `protocol_components` - HashMap of component ID to component. These components are the raw
    ///   Tycho RPC response ones. Not the ones from tycho simulation
    ///
    /// # Returns
    /// Returns `Ok(())` if all expected components are found and match their expected state.
    ///
    /// # Errors
    /// Returns an error if:
    /// - Any expected component is missing from the Tycho state
    /// - Any component's properties don't match the expected values (shows detailed diff)
    fn validate_state(
        expected_components: &Vec<ProtocolComponentWithTestConfig>,
        protocol_components: &[ProtocolComponent],
    ) -> miette::Result<()> {
        if expected_components.is_empty() {
            debug!("No expected components defined for this test. Skipping state validation.");
            return Ok(());
        }

        let protocol_components_by_id: HashMap<String, ProtocolComponent> = protocol_components
            .iter()
            .map(|component| (component.id.clone().to_lowercase(), component.clone()))
            .collect();
        debug!("Validating {:?} expected components", expected_components.len());
        for expected_component in expected_components {
            let component_id = expected_component
                .base
                .id
                .to_lowercase();

            let component = protocol_components_by_id
                .get(&component_id)
                .ok_or_else(|| miette!("Component {:?} was not found on Tycho", component_id))?;

            let diff = expected_component
                .base
                .compare(component, true);
            match diff {
                Some(diff) => {
                    return Err(miette!(
                        "Component {} does not match the expected state:\n{}",
                        component_id,
                        diff
                    ));
                }
                None => {
                    info!("Component {} matches the expected state", component_id);
                }
            }
        }
        info!(
            "All expected components were successfully found on Tycho and match the expected state"
        );
        Ok(())
    }

    fn validate_excluded_components(
        excluded_components: &[String],
        protocol_components: &[ProtocolComponent],
    ) -> miette::Result<()> {
        if excluded_components.is_empty() {
            return Ok(());
        }

        let indexed_ids: HashSet<String> = protocol_components
            .iter()
            .map(|c| c.id.to_lowercase())
            .collect();

        for excluded_id in excluded_components {
            let excluded = excluded_id.to_lowercase();
            if indexed_ids.contains(&excluded) {
                return Err(miette!(
                    "Component {excluded_id} was indexed but is listed in excluded_components"
                ));
            }
            info!("Component {excluded_id} correctly absent from Tycho output");
        }

        Ok(())
    }

    /// Runs the ladder on `update` for the expected components, honoring their skips.
    fn run_simulation(
        &self,
        update: &Update,
        protocol_components: &HashMap<String, ProtocolComponentModel>,
        expected_components: &[ProtocolComponentWithTestConfig],
        protocol_system: &str,
        limit_kinds: Option<&HashMap<(String, Direction), LimitKind>>,
    ) -> miette::Result<LadderOutput> {
        ladder::run_ladder(
            self.chain,
            update,
            protocol_components,
            &skip_reasons(expected_components, |c| &c.skip_simulation),
            &skip_reasons(expected_components, |c| &c.skip_execution),
            protocol_system,
            limit_kinds,
        )
    }

    /// Simulates executing trades through RPC requests using historical block data
    /// and validates the accuracy of the Tycho simulation predictions.
    ///
    /// This method processes large execution sets using batching (batch size: 30) to avoid
    /// RPC request size limits caused by large state overwrites. Each batch is processed
    /// sequentially to maintain stability and provide detailed progress reporting.
    ///
    /// # Arguments
    /// * `execution_data` - HashMap of simulation IDs to TychoExecutionInput data
    /// * `block` - The historical block to use for execution testing
    /// * `protocol_system` - The protocol system identifier
    /// * `expected_components` - Test configuration to determine which components to skip
    ///
    /// # Returns
    /// Returns `Ok(())` if all executions complete successfully within tolerance.
    ///
    /// # Errors
    /// Returns an error if:
    /// - Execution simulation fails or reverts
    /// - Difference between simulation and execution exceeds 0.5% tolerance
    /// - Any critical execution failures occur
    ///
    /// Components can be skipped using `skip_execution` flag in the test configuration.
    async fn run_execution(
        &self,
        execution_data: HashMap<String, TychoExecutionInput>,
        block: &Block,
        protocol_system: &str,
        expected_components: &[ProtocolComponentWithTestConfig],
    ) -> miette::Result<()> {
        let (inputs, results) = self
            .execute_unskipped(execution_data, block, protocol_system, expected_components)
            .await?;
        check_executions_match(&inputs, &results, Tolerance::LEGACY)
    }

    /// Simulates executing every input whose component does not skip execution, returning the
    /// inputs executed and their results.
    async fn execute_unskipped(
        &self,
        execution_data: HashMap<String, TychoExecutionInput>,
        block: &Block,
        protocol_system: &str,
        expected_components: &[ProtocolComponentWithTestConfig],
    ) -> miette::Result<(HashMap<String, TychoExecutionInput>, HashMap<String, TychoExecutionResult>)>
    {
        if execution_data.is_empty() {
            info!("No execution data to process");
            return Ok((HashMap::new(), HashMap::new()));
        }

        let skip_execution = skip_reasons(expected_components, |c| &c.skip_execution);

        // Filter out skipped components
        let filtered_execution_data: HashMap<_, _> = execution_data
            .into_iter()
            .filter(|(_, input)| !skip_execution.contains_key(&input.component_id.to_lowercase()))
            .collect();

        if filtered_execution_data.is_empty() {
            info!("All components skipped execution");
            return Ok((HashMap::new(), HashMap::new()));
        }

        let results = self
            .simulate_executions(&filtered_execution_data, block, protocol_system)
            .await?;
        Ok((filtered_execution_data, results))
    }

    /// Simulates executing every input at `block` in one batch, keyed like the inputs.
    async fn simulate_executions(
        &self,
        inputs: &HashMap<String, TychoExecutionInput>,
        block: &Block,
        protocol_system: &str,
    ) -> miette::Result<HashMap<String, TychoExecutionResult>> {
        if inputs.is_empty() {
            return Ok(HashMap::new());
        }
        let rpc_tools = RPCTools::new(self.rpc_provider.url.as_ref(), &self.chain).await?;
        let router_overwrites_data =
            execution::create_router_overwrites_data(self.chain, protocol_system)?;

        info!("Executing {} simulations ...", inputs.len());

        simulate_swap_transaction(&rpc_tools, inputs.clone(), block, router_overwrites_data, None)
            .await
            .map_err(|(error, _, _)| error)
    }

    /// Strict mode's execution checks: the ladder, then per direction the quote at the limit,
    /// the revert one unit past a hard limit, and the max output. Every execution runs in one
    /// batch, and each limit check is recorded per component and direction. With
    /// `executes_nothing`, the reason every component skips execution, the ladder is recorded as
    /// skipped. With no `block` to execute at, nothing is executed or recorded for the ladder, and
    /// the limit checks are recorded from the simulation alone: a streamed block that only
    /// simulates.
    fn check_strict_executions(
        &self,
        output: &LadderOutput,
        block: Option<&Block>,
        protocol_system: &str,
        tolerance: Tolerance,
        executes_nothing: Option<&str>,
        checks: &mut Checks,
    ) -> miette::Result<()> {
        let LadderOutput { executions, at_limit, past_limit, .. } = output;

        let mut inputs = executions.clone();
        inputs.extend(at_limit.clone());
        inputs.extend(past_limit.clone());
        let execute = block.is_some();
        let executed = match block {
            Some(block) => self
                .runtime
                .block_on(self.simulate_executions(&inputs, block, protocol_system)),
            None => Ok(HashMap::new()),
        };
        let no_results = HashMap::new();
        let results = match &executed {
            Ok(results) if !execute => results,
            Ok(results) => {
                observe_executions(checks, executions, results);
                observe_executions(checks, at_limit, results);
                match executes_nothing {
                    Some(reason) => checks.skip(CheckId::ExecLadder, reason),
                    None => {
                        checks.run(CheckId::ExecLadder, || {
                            check_executions_match(executions, results, tolerance)
                        })?;
                    }
                }
                results
            }
            Err(e) => {
                checks.fail(CheckId::ExecLadder, e);
                &no_results
            }
        };
        // When the batch could not run, every execution a limit check needs fails with its error.
        let executed = match &executed {
            _ if !execute => None,
            Ok(_) => Some(Ok(results)),
            Err(e) => Some(Err(format!("the executions could not be simulated: {e:#}"))),
        };
        let executed = executed.as_ref().map(|executed| {
            executed
                .as_ref()
                .map(|results| *results)
                .map_err(String::as_str)
        });
        for judged in ladder::judge_limits(output, executed, tolerance) {
            checks.record_direction(
                judged.check,
                &judged.component,
                &judged.direction,
                judged.skipped,
                &judged.failures,
            );
        }
        Ok(())
    }

    /// Strict: verifies that component `id`, declared `tvl_only`, quotes the same in `halved`, the
    /// snapshot decoded with its balances halved, as in `update`. `changed` counts the balances
    /// halving changed, per component. Returns every difference.
    fn check_tvl_only(
        id: &str,
        update: &Update,
        halved: &Update,
        changed: &HashMap<String, usize>,
    ) -> Vec<String> {
        if changed
            .get(id)
            .is_none_or(|count| *count == 0)
        {
            return vec!["the component has no nonzero indexed balance, so halving its balances \
                 verifies nothing; it needs no `tvl_only`"
                .to_string()];
        }
        fn find<'a>(update: &'a Update, id: &str) -> Option<(&'a String, &'a dyn ProtocolSim)> {
            update
                .states
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(id))
                .map(|(key, state)| (key, state.as_ref()))
        }
        let (Some((key, original)), Some((_, quoted_halved))) =
            (find(update, id), find(halved, id))
        else {
            return vec!["the component did not decode with its balances halved".to_string()];
        };
        let Some(component) = update.new_pairs.get(key) else {
            return vec![format!("the update has no component {key}")];
        };
        tycho_test::checks::tvl_only::compare(
            original,
            quoted_halved,
            &component.tokens,
            executable_max_input,
        )
    }

    /// Strict: indexes the test's range while streaming it. The indexer runs in the background,
    /// serving the blocks it indexes; the runner subscribes before extraction starts, so every
    /// block's deltas go through the decoder. Returns the indexer's outcome, and the stream's,
    /// which may fail on its own.
    fn index_streaming(
        &self,
        tycho_runner: &TychoRunner,
        spkg_path: &str,
        test: &IntegrationTest,
        config: &IntegrationTestsConfig,
        checks: &mut Checks,
    ) -> miette::Result<Result<Streamed, String>> {
        let limit_kinds = limit_kinds(test, config);
        let mut indexer = tycho_runner
            .start_tycho_streaming(
                spkg_path,
                test.start_block,
                test.stop_block,
                &config.protocol_type_names,
                &config.protocol_system,
                config.module_name.as_deref(),
            )
            .wrap_err("Failed to run Tycho")?;
        let streamed = self.runtime.block_on(self.stream_range(
            test,
            config,
            &limit_kinds,
            &mut indexer,
            checks,
        ));
        let indexed = TychoRunner::stop_tycho(indexer);
        indexed?;
        Ok(streamed.map_err(|e| {
            error!("[{}] streaming the range failed: {e:#}", test.name);
            format!("{e:#}")
        }))
    }

    /// Subscribes to the indexer of the test's range and follows it to the end of the range:
    /// applies every block, and runs the simulation checks at the sampled changed blocks.
    async fn stream_range(
        &self,
        test: &IntegrationTest,
        config: &IntegrationTestsConfig,
        limit_kinds: &HashMap<(String, Direction), LimitKind>,
        indexer: &mut Child,
        checks: &mut Checks,
    ) -> miette::Result<Streamed> {
        let mut exited = || {
            indexer
                .try_wait()
                .map_or(true, |status| status.is_some())
        };
        let expected: HashSet<String> = test
            .expected_components
            .iter()
            .map(|component| component.base.id.to_lowercase())
            .collect();

        // The indexer's server comes up before extraction starts; wait for it.
        let mut waited = 0;
        let tokens = loop {
            match tycho_simulation::utils::load_all_tokens(
                &self.tycho_host(),
                true,
                Some("dummy"),
                true,
                self.chain,
                None,
                None,
            )
            .await
            {
                Ok(tokens) => break tokens,
                Err(e) if exited() || waited >= 300 => {
                    return Err(miette!("the indexer's server did not come up: {e}"))
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    waited += 1;
                }
            }
        };

        let adapter_contract_path = self.get_adapter_contract_path(
            config.adapter_contract.as_deref(),
            config
                .adapter_build_signature
                .as_deref(),
            config.adapter_build_args.as_deref(),
        )?;
        let mut decoder_context = DecoderContext::new().vm_traces(self.vm_simulation_traces);
        if let Some(path) = adapter_contract_path
            .as_deref()
            .and_then(Path::to_str)
        {
            decoder_context = decoder_context.vm_adapter_path(path);
        }
        tycho_simulation::evm::engine_db::SHARED_TYCHO_DB
            .clear()
            .into_diagnostic()?;
        let builder = ProtocolStreamBuilder::new(&self.tycho_host(), self.chain)
            .auth_key(Some("dummy".to_string()))
            // After `auth_key`, which turns TLS on.
            .no_tls(true)
            .min_token_quality(0)
            .stream_end_policy(StreamEndPolicy::AnyEnded)
            // The indexer sends the range as fast as it extracts it, and the client drops a
            // subscription whose buffer fills, so the buffer holds the whole range while blocks
            // are simulated.
            .subscription_buffer_size(tokio::sync::Semaphore::MAX_PERMITS);
        let builder = register_protocol_with_filter(
            builder,
            &config.protocol_system,
            decoder_context,
            ComponentFilter::Ids(expected.iter().cloned().collect()),
        )
        .skip_state_decode_failures(false)
        .set_tokens(tokens)
        .await;
        let stream = builder
            .build()
            .await
            .into_diagnostic()
            .wrap_err("Failed to subscribe to the indexer")?;
        tokio::pin!(stream);
        info!("[{}] Streaming blocks {}-{}", test.name, test.start_block, test.stop_block);

        let mut sampler =
            stream::Sampler::new(test.start_block, test.stop_block, stream::MAX_SIMULATED_BLOCKS);
        let mut streamed = Streamed {
            sheets: BTreeMap::new(),
            errors: Vec::new(),
            coverage: stream::Coverage { test: test.name.clone(), ..Default::default() },
            simulated: Vec::new(),
        };
        let mut components: HashMap<String, ProtocolComponentModel> = HashMap::new();
        let mut latest: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut first = true;
        let mut reached_stop = false;
        let mut idle = Duration::ZERO;
        loop {
            // The stream ends when the indexer has extracted the range and ended the
            // subscription. Waiting is bounded in case it ends without saying so.
            let next = tokio::time::timeout(STREAM_POLL, stream.next()).await;
            let update = match next {
                Ok(Some(Ok(update))) => update,
                Ok(Some(Err(e))) => {
                    streamed.errors.push(format!("{e:#}"));
                    continue;
                }
                Ok(None) => break,
                Err(_) => {
                    idle += STREAM_POLL;
                    if reached_stop || exited() {
                        break;
                    }
                    if idle >= STREAM_IDLE_LIMIT {
                        return Err(miette!(
                            "the stream sent nothing for {}s before reaching block {}",
                            idle.as_secs(),
                            test.stop_block
                        ));
                    }
                    continue;
                }
            };
            idle = Duration::ZERO;
            let block = update.block_number;
            for (id, component) in &update.new_pairs {
                if expected.contains(&id.to_lowercase()) {
                    components.insert(id.clone(), component.clone());
                }
            }
            let changed: HashMap<String, Box<dyn ProtocolSim>> = update
                .states
                .iter()
                .filter(|(id, _)| expected.contains(&id.to_lowercase()))
                .map(|(id, state)| (id.clone(), state.clone_box()))
                .collect();
            for (id, state) in &changed {
                latest.insert(id.clone(), state.clone_box());
            }

            // The first message is the snapshot the stream starts from, not a block's deltas.
            let in_range = (test.start_block..=test.stop_block).contains(&block);
            if !first && in_range {
                streamed.coverage.blocks_streamed += 1;
            }
            if !first && in_range && !changed.is_empty() {
                let coverage = &mut streamed.coverage;
                coverage.changed_blocks += 1;
                for id in changed.keys() {
                    *coverage
                        .changed_blocks_by_component
                        .entry(id.to_lowercase())
                        .or_default() += 1;
                }
                if sampler.take(block) {
                    let new_pairs = changed
                        .keys()
                        .filter_map(|id| {
                            components
                                .get(id)
                                .map(|c| (id.clone(), c.clone()))
                        })
                        .collect();
                    let at_block = Update::new(block, changed, new_pairs);
                    checks.at_block(Some(block));
                    let output = checks.run(CheckId::SimLadder, || {
                        self.run_simulation(
                            &at_block,
                            &components,
                            &test.expected_components,
                            &config.protocol_system,
                            Some(limit_kinds),
                        )
                    });
                    checks.at_block(None);
                    if let Some(output) = output? {
                        for limits in &output.limit_directions {
                            coverage
                                .quoted_directions
                                .entry(limits.component.to_lowercase())
                                .or_default()
                                .insert(limits.direction.to_string());
                        }
                        coverage.simulated_blocks.push(block);
                        streamed.simulated.push((block, output));
                    }
                }
            }
            first = false;

            if block >= test.stop_block {
                reached_stop = true;
                let sheets = latest
                    .iter()
                    .filter_map(|(id, state)| {
                        let component = components.get(id)?;
                        let sheet = sheet::quote_sheet(
                            state.as_ref(),
                            &component.tokens,
                            executable_max_input,
                        );
                        Some((id.to_lowercase(), sheet))
                    })
                    .collect();
                streamed.sheets.insert(block, sheets);
            }
        }

        // The directions that trade at the end of the stream, which the floor asks to see quoted.
        for (id, state) in &latest {
            let Some(component) = components.get(id) else {
                continue;
            };
            let tradable = component
                .tokens
                .iter()
                .permutations(2)
                .filter(|pair| {
                    state
                        .get_limits(pair[0].address.clone(), pair[1].address.clone())
                        .is_ok_and(|(max_input, _)| !max_input.is_zero())
                })
                .map(|pair| {
                    Direction {
                        token_in: pair[0].address.clone(),
                        token_out: pair[1].address.clone(),
                    }
                    .to_string()
                })
                .collect();
            streamed
                .coverage
                .tradable_directions
                .insert(id.to_lowercase(), tradable);
        }
        info!(
            "[{}] Streamed {} blocks: {} changed an expected component, {} simulated",
            test.name,
            streamed.coverage.blocks_streamed,
            streamed.coverage.changed_blocks,
            streamed.simulated.len()
        );
        Ok(streamed)
    }

    /// Strict, after a streamed range: executes at the fewest simulated blocks, spread evenly,
    /// that meet the floor, and records the limit checks of every simulated block.
    fn execute_streamed(
        &self,
        streamed: &mut Streamed,
        config: &IntegrationTestsConfig,
        checks: &mut Checks,
    ) {
        let counts = |output: &LadderOutput| {
            stream::count_by_component(
                output
                    .executions
                    .values()
                    .chain(output.at_limit.values())
                    .chain(output.past_limit.values())
                    .map(|input| input.component_id.as_str()),
            )
        };
        let candidates: Vec<stream::Candidate> = streamed
            .simulated
            .iter()
            .map(|(block, output)| stream::Candidate { block: *block, executions: counts(output) })
            .collect();
        let to_execute = stream::blocks_to_execute(&candidates, stream::Floor::DEFAULT);
        info!("Executing at {} of the {} simulated blocks", to_execute.len(), candidates.len());

        for (block, output) in std::mem::take(&mut streamed.simulated) {
            checks.at_block(Some(block));
            let header = if to_execute.contains(&block) {
                match self.runtime.block_on(
                    self.rpc_provider
                        .get_block_header(block),
                ) {
                    Ok(header) => Some(header),
                    Err(e) => {
                        checks.fail(CheckId::ExecLadder, &e);
                        None
                    }
                }
            } else {
                None
            };
            if header.is_some() {
                let coverage = &mut streamed.coverage;
                for (id, count) in counts(&output) {
                    coverage.executions += count;
                    *coverage
                        .executions_by_component
                        .entry(id)
                        .or_default() += count;
                }
                coverage.executed_blocks.push(block);
            }
            // Strict mode records each failure and goes on, so this never returns an error.
            if let Err(e) = self.check_strict_executions(
                &output,
                header.as_ref(),
                &config.protocol_system,
                config.tolerance(),
                None,
                checks,
            ) {
                error!("Executing at block {block} failed: {e:#}");
            }
        }
        checks.at_block(None);
    }

    /// Strict, streamed: each simulated expected component of the snapshot at `block` must quote
    /// exactly as its streamed state did then.
    fn check_stream_matches_snapshot(
        test: &IntegrationTest,
        streamed: &Streamed,
        update: &Update,
        block: u64,
    ) -> miette::Result<()> {
        let sheets = streamed.sheets_at(block)?;
        let mut differences: Vec<String> = streamed
            .errors
            .iter()
            .map(|e| format!("the stream reported an error: {e}"))
            .collect();
        for id in simulated_ids(test) {
            let Some((key, state)) = update
                .states
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(&id))
            else {
                differences.push(format!("{id}: not in the snapshot"));
                continue;
            };
            let Some(component) = update.new_pairs.get(key) else {
                differences.push(format!("{id}: the snapshot has no component for it"));
                continue;
            };
            let snapshot =
                sheet::quote_sheet(state.as_ref(), &component.tokens, executable_max_input);
            match sheets.get(&id) {
                None => differences.push(format!("{id}: not in the stream")),
                Some(streamed) => differences.extend(
                    sheet::sheet_differences(streamed, &snapshot)
                        .into_iter()
                        .map(|difference| format!("{id}: {difference}")),
                ),
            }
        }
        ensure!(differences.is_empty(), "{}", differences.join("; "));
        Ok(())
    }

    /// Strict: checks an expected component's indexed balances against the source it declares,
    /// at `block`, within `tolerance`. Returns every failure, empty when all its balances match.
    fn check_component_balances(
        &self,
        expected: &ProtocolComponentWithTestConfig,
        tolerance: Tolerance,
        indexed_components: &[ProtocolComponent],
        states: &HashMap<String, ProtocolComponentState>,
        all_tokens: &HashMap<Bytes, Token>,
        block: u64,
    ) -> Vec<String> {
        let id = expected.base.id.to_lowercase();
        let (Some(indexed), Some(state)) = (
            indexed_components
                .iter()
                .find(|component| component.id.to_lowercase() == id),
            states.get(&id),
        ) else {
            return vec!["the component is not in the snapshot".to_string()];
        };
        let tokens: Vec<(Bytes, String)> = indexed
            .tokens
            .iter()
            .map(|address| {
                let symbol = all_tokens
                    .get(address)
                    .map_or_else(|| address.to_string(), |token| token.symbol.clone());
                (address.clone(), symbol)
            })
            .collect();
        let component = balances::Component {
            id: &id,
            tokens: &tokens,
            balances: &state.balances,
            static_attributes: &indexed.static_attributes,
            state_attributes: &state.attributes,
        };
        balances::check_component(
            &component,
            expected
                .balance_check
                .as_ref()
                .unwrap_or(&BalanceCheck::Default),
            tolerance,
            &RpcChain { runner: self, block },
        )
    }

    /// Validate that the token balances of the components match the values
    /// on-chain, extracted by querying the token balances using a node.
    fn validate_token_balances(
        &self,
        component_tokens: &HashMap<String, Vec<Token>>,
        protocol_states_by_id: &HashMap<String, ProtocolComponentState>,
        block: u64,
    ) -> miette::Result<()> {
        for (id, component) in protocol_states_by_id.iter() {
            let tokens = component_tokens.get(id);
            if let Some(tokens) = tokens {
                for token in tokens {
                    let mut balance: U256 = U256::from(0);
                    let bal = component.balances.get(&token.address);
                    if let Some(bal) = bal {
                        let bal = bal.clone().into();
                        balance = bytes_to_u256(bal);
                    }

                    info!(
                        "Validating token balance for component {} and token {}",
                        id, token.symbol
                    );
                    let token_address = bytes_to_address(&token.address).into_diagnostic()?;
                    let component_address = Address::from_str(id.as_str())
                        .into_diagnostic()
                        .wrap_err("Failed to parse component address")?;
                    let node_balance =
                        self.runtime
                            .block_on(self.rpc_provider.get_token_balance(
                                token_address,
                                component_address,
                                block,
                            ))?;
                    if balance != node_balance {
                        return Err(miette!(
                            "Token balance mismatch for component {id} and token {}. Balance: {balance}, Node balance: {node_balance}",
                            token.symbol
                        ));
                    }
                    info!(
                        "Token balance for component {} and token {} matches the expected value",
                        id, token.symbol
                    );
                }
            } else {
                return Err(miette!("Couldn't find tokens for component {}", id,));
            }
        }
        Ok(())
    }

    /// Gets the adapter contract path if needed, building it if not found
    fn get_adapter_contract_path(
        &self,
        adapter_contract: Option<&str>,
        adapter_build_signature: Option<&str>,
        adapter_build_args: Option<&str>,
    ) -> miette::Result<Option<PathBuf>> {
        // Adapter contract will only be configured for VM protocols, not natively implemented
        // protocols.
        if let Some(adapter_contract_name) = adapter_contract {
            let adapter_contract_path = match self
                .adapter_contract_builder
                .find_contract(adapter_contract_name)
            {
                Ok(path) => path,
                Err(_) => {
                    info!("Adapter contract not found, building it...");
                    self.adapter_contract_builder
                        .build_target(
                            adapter_contract_name,
                            adapter_build_signature,
                            adapter_build_args,
                        )
                        .wrap_err("Failed to build adapter contract")?
                }
            };
            debug!("Using adapter contract: {}", adapter_contract_path.display());
            Ok(Some(adapter_contract_path))
        } else {
            Ok(None)
        }
    }
}

/// Returns the block a range test reads its snapshot at: the last block the indexer committed.
///
/// The indexer keeps only the latest version of every attribute and may commit one block past
/// `stop_block`, depending on how fast the stream ends. A read at an earlier block would miss
/// every attribute written after it, so the read targets the committed block instead. Fails
/// when that block is missing or below `stop_block`, because the database then lacks the range
/// under test, and when it is past `stop_block + 1`, because the database then holds a later
/// range, such as one reused with `--reuse-last-sync` from a longer run.
fn snapshot_block(last_indexed_block: Option<u64>, stop_block: u64) -> miette::Result<u64> {
    let Some(block) = last_indexed_block else {
        return Err(miette!(
            "The extractor has no committed block, expected at least stop block {stop_block}"
        ));
    };
    ensure!(
        block >= stop_block,
        "The indexer committed up to block {block}, below the stop block {stop_block}"
    );
    ensure!(
        block <= stop_block + 1,
        "The indexer committed up to block {block}, past the stop block {stop_block}; the \
         database holds a later range, rerun without --reuse-last-sync"
    );
    Ok(block)
}

/// Returns the lowercase ids of the expected components without `skip_simulation` that are
/// absent from `decoded_ids`, compared case-insensitively. Returns an empty list when every such
/// component decoded.
fn undecoded_components<'a>(
    expected_components: &[ProtocolComponentWithTestConfig],
    decoded_ids: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let decoded: HashSet<String> = decoded_ids
        .into_iter()
        .map(|id| id.to_lowercase())
        .collect();
    expected_components
        .iter()
        .filter(|component| !component.skip_simulation.is_skipped())
        .map(|component| component.base.id.to_lowercase())
        .filter(|id| !decoded.contains(id))
        .collect()
}

/// The lowercase ids of the expected components that are simulated.
fn simulated_ids(test: &IntegrationTest) -> Vec<String> {
    test.expected_components
        .iter()
        .filter(|component| !component.skip_simulation.is_skipped())
        .map(|component| component.base.id.to_lowercase())
        .collect()
}

/// The lowercase ids of the expected components that are executed.
fn executed_ids(test: &IntegrationTest) -> Vec<String> {
    test.expected_components
        .iter()
        .filter(|component| {
            !component.skip_simulation.is_skipped() && !component.skip_execution.is_skipped()
        })
        .map(|component| component.base.id.to_lowercase())
        .collect()
}

/// The lowercase ids of the expected components whose `flag` is skipped, each with its reason.
fn skip_reasons<'a>(
    expected_components: &'a [ProtocolComponentWithTestConfig],
    flag: impl Fn(&'a ProtocolComponentWithTestConfig) -> &'a Skip,
) -> HashMap<String, &'a str> {
    let mut reasons = HashMap::new();
    for component in expected_components {
        if let Some(reason) = flag(component).reason() {
            reasons.insert(component.base.id.to_lowercase(), reason);
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env, str::FromStr};

    use dotenv::dotenv;
    use glob::glob;
    use tycho_simulation::tycho_common::{models::protocol::ProtocolComponentState, Bytes};

    use super::*;

    #[test]
    fn snapshot_block_reads_at_the_last_committed_block() {
        assert_eq!(snapshot_block(Some(51_696_284), 51_696_283).unwrap(), 51_696_284);
        assert_eq!(snapshot_block(Some(51_696_283), 51_696_283).unwrap(), 51_696_283);
    }

    #[test]
    fn snapshot_block_rejects_a_database_short_of_the_stop_block() {
        assert!(snapshot_block(Some(51_696_282), 51_696_283).is_err());
        assert!(snapshot_block(None, 51_696_283).is_err());
    }

    #[test]
    fn snapshot_block_rejects_a_database_past_the_stop_block() {
        assert!(snapshot_block(Some(51_696_285), 51_696_283).is_err());
    }

    #[test]
    fn undecoded_components_lists_expected_components_missing_from_the_update() {
        let expected: Vec<ProtocolComponentWithTestConfig> = serde_yaml::from_str(
            r#"
            - id: "0xAA"
              tokens: []
              creation_tx: "0x01"
            - id: "0xbb"
              tokens: []
              creation_tx: "0x02"
            - id: "0xcc"
              tokens: []
              creation_tx: "0x03"
              skip_simulation: true
            "#,
        )
        .unwrap();
        let decoded = ["0xaa".to_string()];

        assert_eq!(undecoded_components(&expected, &decoded), vec!["0xbb".to_string()]);
        assert!(
            undecoded_components(&expected, &["0xaa".to_string(), "0xBB".to_string()]).is_empty()
        );
    }

    /// A limit no swap could be executed with is capped; anything at or below the ceiling is the
    /// size the venue reported, untouched.
    /// Every integration test yaml under `protocols/substreams/`, clone and nested ones included,
    /// as paths relative to `protocols/`.
    fn all_test_yamls() -> Vec<String> {
        let protocols_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let pattern = protocols_dir.join("substreams/**/integration_test*.tycho.yaml");
        let mut yamls: Vec<String> = glob(pattern.to_str().unwrap())
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .strip_prefix(&protocols_dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|path| !path.contains("/target/"))
            .collect();
        yamls.sort();
        assert!(!yamls.is_empty(), "No integration test yamls found under substreams/");
        yamls
    }

    fn protocols_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join(relative)
    }

    #[test]
    fn test_parse_all_configs() {
        let errors: Vec<String> = all_test_yamls()
            .into_iter()
            .filter_map(|path| {
                TestRunner::parse_config(&protocols_path(&path))
                    .err()
                    .map(|e| format!("{path}: {e:?}"))
            })
            .collect();
        assert!(errors.is_empty(), "Config files failed to parse:\n{}", errors.join("\n"));
    }

    /// `PackageExpectations`, which reads a package's declarations alone, agrees with the test
    /// configuration on every package: the tolerance, and the limit of every direction of every
    /// expected component.
    #[test]
    fn package_expectations_agree_with_every_test_configuration() {
        for path in all_test_yamls() {
            let full = protocols_path(&path);
            let config = TestRunner::parse_config(&full).unwrap();
            let package = tycho_test::expectations::PackageExpectations::from_yaml(
                std::fs::File::open(&full).unwrap(),
                &path,
            )
            .unwrap();
            assert_eq!(package.tolerance(), config.tolerance(), "{path}");
            for component in config
                .tests
                .iter()
                .flat_map(|test| &test.expected_components)
            {
                for direction in component_directions(component) {
                    assert_eq!(
                        package.limits_for(&component.base.id, &direction),
                        config.limits_for(component, &direction),
                        "{path}: {} {direction}",
                        component.base.id
                    );
                }
            }
        }
    }

    #[test]
    fn arc_uniswap_packages_use_shared_packages_and_arc_manifests() {
        let root_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("protocols/testing must live below protocols")
            .to_path_buf();

        for (protocol, base_protocol, config_file_name, manifest_path) in [
            (
                "arc-uniswap-v2",
                "ethereum-uniswap-v2",
                "integration_test_arc_uniswap_v2.tycho.yaml",
                "./arc-uniswap-v2.yaml",
            ),
            (
                "arc-uniswap-v3",
                "ethereum-uniswap-v3-logs-only",
                "integration_test_arc_uniswap_v3.tycho.yaml",
                "./arc-uniswap-v3.yaml",
            ),
            (
                "arc-uniswap-v4-no-hooks",
                "ethereum-uniswap-v4/no-hooks",
                "integration_test_arc_uniswap_v4_no_hooks.tycho.yaml",
                "./arc-uniswap-v4-no-hooks.yaml",
            ),
        ] {
            let runner = TestRunner::new(RunnerConfig {
                test_type: TestType::Range(TestTypeRange {
                    match_test: None,
                    report: None,
                    stream: true,
                }),
                root_path: root_path.clone(),
                chain: Chain::Arc,
                protocol: protocol.to_string(),
                db_url: String::new(),
                rpc_url: "http://localhost:8545".to_string(),
                tycho_server_port: 4242,
                vm_simulation_traces: false,
                reuse_last_sync: false,
                prebuilt_wasm: false,
            })
            .expect("Arc package resolution must produce a runner");

            assert!(
                runner
                    .substreams_path
                    .ends_with(PathBuf::from("substreams").join(base_protocol)),
                "unexpected shared package for {protocol}",
            );
            assert!(
                runner
                    .config_file_path
                    .ends_with(config_file_name),
                "unexpected config file for {protocol}",
            );

            let config = TestRunner::parse_config(&runner.config_file_path)
                .expect("Arc integration test configuration must parse");
            assert_eq!(
                config.substreams_yaml_path, manifest_path,
                "unexpected manifest for {protocol}",
            );
            assert!(
                runner
                    .substreams_path
                    .join(config.substreams_yaml_path)
                    .is_file(),
                "manifest must exist for {protocol}",
            );
        }
    }

    fn get_mocked_runner() -> TestRunner {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a missing .env file is normal; the variables may come from the environment"
        )]
        let _ = dotenv();
        let rpc_url = env::var("RPC_URL").unwrap();
        get_mocked_runner_for(Chain::Ethereum, "test-protocol", rpc_url)
    }

    /// Builds a runner for `protocol` on `chain` rooted at the current directory. The RPC URL
    /// is only stored, so callers that never reach the network can pass a placeholder.
    fn get_mocked_runner_for(chain: Chain, protocol: &str, rpc_url: String) -> TestRunner {
        let current_dir = std::env::current_dir().unwrap();
        TestRunner::new(RunnerConfig {
            test_type: TestType::Range(TestTypeRange {
                match_test: None,
                report: None,
                stream: true,
            }),
            root_path: current_dir,
            chain,
            protocol: protocol.to_string(),
            db_url: "".to_string(),
            rpc_url,
            tycho_server_port: 4242,
            vm_simulation_traces: false,
            reuse_last_sync: false,
            prebuilt_wasm: false,
        })
        .unwrap()
    }

    #[test]
    fn robinhood_uniswap_v4_with_hooks_resolves_to_the_nested_with_hooks_config() {
        // Path resolution never contacts the RPC, so a placeholder URL keeps this test runnable
        // in the no-external-deps CI job, which does not set RPC_URL.
        let runner = get_mocked_runner_for(
            Chain::Robinhood,
            "robinhood-uniswap-v4-with-hooks",
            "http://localhost:8545".to_string(),
        );

        assert!(
            runner
                .substreams_path
                .ends_with("ethereum-uniswap-v4/with-hooks"),
            "unexpected substreams_path: {}",
            runner.substreams_path.display()
        );
        assert_eq!(
            runner
                .config_file_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("integration_test_robinhood_uniswap_v4_with_hooks.tycho.yaml")
        );
    }

    #[test]
    fn bsc_ring_swap_v2_uses_shared_package_config() {
        let runner = get_mocked_runner_for(
            Chain::Bsc,
            "bsc-ring-swap-v2",
            "http://localhost:8545".to_string(),
        );

        assert!(
            runner
                .substreams_path
                .ends_with("substreams/ethereum-ring-swap-v2"),
            "unexpected substreams_path: {}",
            runner.substreams_path.display()
        );
        assert_eq!(
            runner
                .config_file_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("integration_test_bsc_ring_swap_v2.tycho.yaml")
        );
    }

    #[test]
    fn test_token_balance_validation() {
        let runner = get_mocked_runner();
        // Setup mock data
        let block_number = 21998530;
        let token_bytes = Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let component_id = "0x787B8840100d9BaAdD7463f4a73b5BA73B00C6cA".to_string();
        let token = Token::new(&token_bytes, "FAKE", 18, 0, &[], Chain::Ethereum, 100);

        let mut balances = HashMap::new();
        let balance_bytes = Bytes::from(
            U256::from_str("1070041574684539264153")
                .unwrap()
                .to_be_bytes::<32>(),
        );
        balances.insert(token_bytes, balance_bytes);
        let protocol_state = ProtocolComponentState::new(&component_id, HashMap::new(), balances);

        let mut component_tokens = HashMap::new();
        component_tokens.insert(component_id.clone(), vec![token]);
        let mut protocol_states_by_id = HashMap::new();
        protocol_states_by_id.insert(component_id, protocol_state);

        let result =
            runner.validate_token_balances(&component_tokens, &protocol_states_by_id, block_number);
        assert!(result.is_ok(), "Should pass when balance check is performed and balances match");
    }

    #[test]
    fn test_token_balance_validation_fails_on_mismatch() {
        let runner = get_mocked_runner();

        // Setup mock data
        let block_number = 21998530;
        let token_bytes = Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let component_id = "0x787B8840100d9BaAdD7463f4a73b5BA73B00C6cA".to_string();
        let token = Token::new(&token_bytes, "FAKE", 18, 0, &[], Chain::Ethereum, 100);

        // Set expected balance to zero
        let mut balances = HashMap::new();
        let balance_bytes = Bytes::from(U256::from(0).to_be_bytes::<32>());
        balances.insert(token_bytes, balance_bytes);
        let protocol_state = ProtocolComponentState::new(&component_id, HashMap::new(), balances);

        let mut component_tokens = HashMap::new();
        component_tokens.insert(component_id.clone(), vec![token]);
        let mut protocol_states_by_id = HashMap::new();
        protocol_states_by_id.insert(component_id, protocol_state);

        #[expect(
            clippy::let_underscore_must_use,
            reason = "a missing .env file is normal; the variables may come from the environment"
        )]
        let _ = dotenv();
        let result =
            runner.validate_token_balances(&component_tokens, &protocol_states_by_id, block_number);
        assert!(
            result.is_err(),
            "Should fail when balance check is performed and balances do not match"
        );
    }
}
