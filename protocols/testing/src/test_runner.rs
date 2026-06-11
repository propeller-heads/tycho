use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, LazyLock, RwLock},
};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, U256},
    rpc::types::Block,
};
use figment::{
    providers::{Format, Yaml},
    Figment,
};
use futures::StreamExt;
use itertools::Itertools;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use postgres::NoTls;
use regex::Regex;
use serde_json::json;
use tokio::runtime::Runtime;
use tracing::{debug, error, info, warn};
use tycho_execution::encoding::evm::utils::bytes_to_address;
use tycho_simulation::{
    evm::{
        protocol::{
            u256_num::bytes_to_u256,
            uniswap_v4::hooks::hook_handler_creator::initialize_hook_handlers,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::{DecoderContext, ProtocolComponent as ProtocolComponentModel, Update},
    tycho_client::feed::{
        synchronizer::{Snapshot, StateSyncMessage},
        BlockHeader, FeedMessage,
    },
    tycho_common::{
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            token::Token,
            Chain,
        },
        Bytes,
    },
};
use tycho_test::{
    execution::{
        encoding::{encode_swap, EXECUTOR_ADDRESS},
        models::{TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction,
    },
    RPCTools,
};

use crate::{
    adapter_builder::AdapterContractBuilder,
    config::{IntegrationTest, IntegrationTestsConfig, ProtocolComponentWithTestConfig},
    execution,
    rpc::RPCProvider,
    state_registry::register_protocol,
    tycho_rpc::TychoClient,
    tycho_runner::TychoRunner,
    utils::build_spkg,
};

static CLONE_TO_BASE_PROTOCOL: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
    HashMap::from([
        ("ethereum-sushiswap-v2", "ethereum-uniswap-v2"),
        ("ethereum-pancakeswap-v2", "ethereum-uniswap-v2"),
        ("base-alienbase-v3", "ethereum-uniswap-v3-logs-only"),
        ("unichain-curve", "ethereum-curve"),
    ])
});

pub enum TestType {
    Full(TestTypeFull),
    Range(TestTypeRange),
}

pub struct TestTypeFull {
    pub initial_block: Option<u64>,
}

pub struct TestTypeRange {
    pub match_test: Option<String>,
}

pub struct TestRunner {
    test_type: TestType,
    chain: Chain,
    db_url: String,
    substreams_path: PathBuf,
    config_file_path: PathBuf,
    vm_simulation_traces: bool,
    adapter_contract_builder: AdapterContractBuilder,
    runtime: Runtime,
    rpc_provider: RPCProvider,
    protocol_components: Arc<RwLock<HashMap<String, ProtocolComponentModel>>>,
    reuse_last_sync: bool,
}

impl TestRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        test_type: TestType,
        root_path: PathBuf,
        chain: Chain,
        protocol: String,
        db_url: String,
        rpc_url: String,
        vm_simulation_traces: bool,
        reuse_last_sync: bool,
    ) -> miette::Result<Self> {
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

        let rpc_provider = RPCProvider::new(rpc_url);
        let runtime = Runtime::new().into_diagnostic()?;

        Ok(Self {
            test_type,
            chain,
            db_url,
            substreams_path,
            config_file_path,
            vm_simulation_traces,
            adapter_contract_builder,
            runtime,
            rpc_provider,
            reuse_last_sync,
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

        let config = match Self::parse_config(&self.config_file_path) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!("Failed to parse config: {:#}", e);
                return Ok(());
            }
        };

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
                self.run_tests_in_range(config, &substreams_yaml_path, test_type, terminal_width)?;
            }
        }

        Ok(())
    }

    fn parse_config(config_yaml_path: &PathBuf) -> miette::Result<IntegrationTestsConfig> {
        info!("Parsing config YAML at {}", config_yaml_path.display());
        let yaml = Yaml::file(config_yaml_path);
        let figment = Figment::new().merge(yaml);
        let config = figment
            .extract::<IntegrationTestsConfig>()
            .into_diagnostic()
            .wrap_err("Failed to load test configuration:")?;
        Ok(config)
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
                let content = std::fs::read_to_string(substreams_yaml_path).into_diagnostic()?;
                let re = Regex::new(r"initialBlock:\s*(\d+)").unwrap();
                re.captures(&content)
                    .and_then(|cap| cap.get(1))
                    .and_then(|m| m.as_str().parse::<u64>().ok())
                    .ok_or_else(|| {
                        miette!("Failed to extract initialBlock from substreams.yaml. Please specify it explicitly.")
                    })?
            }
        };
        let spkg_path =
            build_spkg(substreams_yaml_path, start_block).wrap_err("Failed to build spkg")?;
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
                module_name,
            ) {
                error!("Tycho Index command failed: {:#}", e);
            }
        });

        // Wait for protocol to be synced before starting live testing
        let tycho_client = TychoClient::new("http://localhost:4242", Some("dummy".to_string()))
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
            "localhost:4242",
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

        let _ = tycho_simulation::evm::engine_db::SHARED_TYCHO_DB.clear();

        let protocol_stream_builder =
            ProtocolStreamBuilder::new("localhost:4242/", chain).skip_state_decode_failures(true);

        let adapter_contract_path = self.get_adapter_contract_path(
            &config.adapter_contract,
            &config.adapter_build_signature,
            &config.adapter_build_args,
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
        )?;

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
                        update.block_number_or_timestamp
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
                    ) {
                        Ok(data) => data,
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
                        .get_block(BlockNumberOrTag::Number(update.block_number_or_timestamp))
                        .await
                    {
                        Ok(block) => block,
                        Err(e) => {
                            error!(
                                "Failed to fetch block {}: {}",
                                update.block_number_or_timestamp, e
                            );
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
                    continue;
                }
            }
        }

        info!("Stream ended, live testing completed");
        Ok(())
    }

    fn run_tests_in_range(
        &self,
        config: IntegrationTestsConfig,
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
            if self.reuse_last_sync {
                info!("Skipping indexing and using existent DB")
            } else {
                let spkg_path = build_spkg(substreams_yaml_path, test.start_block)
                    .wrap_err("Failed to build spkg")?;

                tycho_runner
                    .run_tycho(
                        &spkg_path,
                        test.start_block,
                        test.stop_block,
                        &config.protocol_type_names,
                        &config.protocol_system,
                        config.module_name.clone(),
                    )
                    .wrap_err("Failed to run Tycho")?;
            }
            let rpc_server = tycho_runner.start_rpc_server()?;
            match self.run_test(test, &config, test.stop_block) {
                Ok(_) => {
                    info!("✅ {} passed\n", test.name);
                }
                Err(e) => {
                    failed_tests.push(test.name.clone());
                    error!("❗️{} failed: {:#}\n", test.name, e);
                }
            }
            tycho_runner.stop_rpc_server(rpc_server)?;
            info!("{}\n", "-".repeat(terminal_width));
        }

        info!("Tests finished!");
        info!("Passed {}/{}\n", tests_count - failed_tests.len(), tests_count);
        if !failed_tests.is_empty() {
            Err(miette!("Failed tests: {}", failed_tests.join(", ")))
        } else {
            Ok(())
        }
    }

    fn run_test(
        &self,
        test: &IntegrationTest,
        config: &IntegrationTestsConfig,
        stop_block: u64,
    ) -> miette::Result<()> {
        // Fetch protocol data from Tycho RPC
        let expected_ids = test
            .expected_components
            .iter()
            .map(|c| c.base.id.to_lowercase())
            .collect::<Vec<String>>();

        // Get block header to extract the timestamp
        let block = self
            .runtime
            .block_on(
                self.rpc_provider
                    .get_block_header(stop_block),
            )
            .wrap_err("Failed to get block header")?;

        let (protocol_components, snapshot, all_tokens) =
            self.fetch_from_tycho_rpc(&config.protocol_system, expected_ids, stop_block)?;

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

        // Step 1: Validate that all expected components are present on Tycho after indexing
        self.validate_state(&test.expected_components, protocol_components)?;

        // Step 2: Validate Token Balances
        match config.skip_balance_check {
            true => info!("Skipping balance check"),
            false => {
                self.validate_token_balances(
                    &tokens_by_component,
                    &response_protocol_states_by_id,
                    stop_block,
                )?;
                info!("All token balances match the values found onchain")
            }
        }

        // Step 3: Decode Snapshot with Decoder from tycho simulation
        let adapter_contract_path_str = self.get_adapter_contract_path(
            &config.adapter_contract,
            &config.adapter_build_signature,
            &config.adapter_build_args,
        )?;

        let update = self.decode_snapshot(
            &config.protocol_system,
            &block,
            snapshot,
            all_tokens,
            adapter_contract_path_str,
            self.vm_simulation_traces,
        )?;

        let protocol_components_simulation: HashMap<String, ProtocolComponentModel> =
            update.new_pairs.clone();

        // Step 4: Run Tycho Simulation
        let execution_data = self.run_simulation(
            &update,
            &protocol_components_simulation,
            &test.expected_components,
            &config.protocol_system,
        )?;
        // Step 5: Run Tycho Execution
        self.runtime
            .block_on(self.run_execution(
                execution_data,
                &block,
                &config.protocol_system,
                &test.expected_components,
            ))?;

        Ok(())
    }

    async fn empty_database(&self) -> Result<(), tokio_postgres::Error> {
        // Remove db name from URL. This is required because we cannot drop a database that we are
        // currently connected to.
        let base_url = match self.db_url.rfind('/') {
            Some(pos) => &self.db_url[..pos],
            None => self.db_url.as_str(),
        };
        let (client, connection) = tokio_postgres::connect(base_url, NoTls).await?;

        // Spawn the connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("Database connection error: {:#}", e);
            }
        });

        client
            .execute("DROP DATABASE IF EXISTS \"tycho_indexer_0\" WITH (FORCE)", &[])
            .await?;
        client
            .execute("CREATE DATABASE \"tycho_indexer_0\"", &[])
            .await?;

        Ok(())
    }

    async fn tycho_runner(&self, initialized_accounts: Vec<String>) -> miette::Result<TychoRunner> {
        if !self.reuse_last_sync {
            self.empty_database()
                .await
                .into_diagnostic()
                .wrap_err("Failed to empty the database")?;
        }
        Ok(TychoRunner::new(self.chain, self.db_url.to_string(), initialized_accounts))
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

    /// Fetches protocol data from the Tycho RPC server and prepares it for validation and
    /// simulation.
    ///
    /// This method connects to the running Tycho RPC server to retrieve protocol components,
    /// states, and contract storage. It then sets up the Tycho Decoder and creates an update
    /// message that can be used for validation and simulation testing.
    ///
    /// # Arguments
    /// * `protocol_system` - The protocol system identifier (e.g., "uniswap_v2", "balancer_v2")
    /// * `expected_component_ids` - List of component IDs to fetch from Tycho
    /// * `adapter_contract` - Optional adapter contract name for VM-based protocols
    /// * `adapter_build_signature` - Optional build signature for the adapter contract
    /// * `adapter_build_args` - Optional build arguments for the adapter contract
    /// * `vm_simulation_traces` - Whether to enable VM simulation traces
    /// * `stop_block` - The block number to fetch data for
    ///
    /// # Returns
    /// A tuple containing:
    /// - `Update` - Decoded protocol state update for simulation
    /// - `HashMap<String, ProtocolComponentState>` - Protocol states by component ID
    /// - `Block` - The block header for the specified block
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn fetch_from_tycho_rpc(
        &self,
        protocol_system: &str,
        expected_component_ids: Vec<String>,
        stop_block: u64,
    ) -> miette::Result<(Vec<ProtocolComponent>, Snapshot, HashMap<Bytes, Token>)> {
        info!("Fetching protocol data from Tycho with stop block {}...", stop_block);

        // Create Tycho client for the RPC server
        let tycho_client = TychoClient::new("http://localhost:4242", None)
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
                stop_block,
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
        adapter_contract_path: Option<PathBuf>,
        vm_simulation_traces: bool,
    ) -> miette::Result<Update> {
        // Clear the shared database state to ensure test isolation
        // This prevents state from previous tests from affecting the current test
        let _ = tycho_simulation::evm::engine_db::SHARED_TYCHO_DB.clear();

        let protocol_stream_builder =
            ProtocolStreamBuilder::new("", self.chain).skip_state_decode_failures(true);

        let mut decoder_context = DecoderContext::new().vm_traces(vm_simulation_traces);
        if let Some(vm_adapter_path) = adapter_contract_path.as_ref() {
            if let Some(path_str) = vm_adapter_path.to_str() {
                decoder_context = decoder_context.vm_adapter_path(path_str);
            }
        }
        let protocol_stream_builder = register_protocol(
            protocol_stream_builder,
            protocol_system,
            self.chain,
            decoder_context,
        )?;

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
        debug!("Decoded message for block {}", block_msg.block_number_or_timestamp);
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
        &self,
        expected_components: &Vec<ProtocolComponentWithTestConfig>,
        protocol_components: Vec<ProtocolComponent>,
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

    /// Runs simulations for all protocol components and swap directions.
    ///
    /// This method performs comprehensive simulation testing on protocol components by:
    /// 1. Computing spot prices for all token pairs
    /// 2. Simulating swaps with different input amounts (0.1%, 1%, 10% of limits)
    /// 3. Testing all possible swap directions between tokens
    /// 4. Preparing execution data for each simulation
    ///
    /// The simulation uses the Tycho SDK to calculate expected outputs and prepare
    /// transaction data for subsequent execution testing.
    ///
    /// # Arguments
    /// * `update` - The decoded protocol state containing all component data
    /// * `protocol_components` - Mapping of component IDs to their ProtocolComponent models
    /// * `expected_components` - Test configuration to determine which components to skip
    /// * `protocol_system` - The protocol system identifier
    ///
    /// # Returns
    /// Returns a HashMap of simulation IDs to TychoExecutionInput data for execution.
    ///
    /// # Errors
    /// Returns an error if:
    /// - Spot price calculation fails for any component
    /// - Simulation fails to calculate amount out
    /// - Transaction encoding fails
    ///
    /// Components can be skipped using `skip_simulation` flag in the test configuration.
    fn run_simulation(
        &self,
        update: &Update,
        protocol_components: &HashMap<String, ProtocolComponentModel>,
        expected_components: &[ProtocolComponentWithTestConfig],
        protocol_system: &String,
    ) -> miette::Result<HashMap<String, TychoExecutionInput>> {
        let skip_simulation: HashSet<_> = expected_components
            .iter()
            .filter(|c| c.skip_simulation)
            .map(|c| c.base.id.to_lowercase())
            .collect();

        let skip_execution: HashSet<_> = expected_components
            .iter()
            .filter(|c| c.skip_execution)
            .map(|c| c.base.id.to_lowercase())
            .collect();

        let mut execution_data = HashMap::new();

        for (id, state) in update.states.iter() {
            if skip_simulation.contains(id) {
                info!("Skipping simulation for component {id}");
                continue;
            }
            let component = protocol_components
                .get(id)
                .ok_or_else(|| miette!("Couldn't find protocol component {id}"))?;

            let tokens = component.tokens.clone();
            let formatted_token_str = format!("{:}/{:}", tokens[0].symbol, tokens[1].symbol);
            state
                .spot_price(&tokens[0], &tokens[1])
                .map(|price| info!("[{}] Spot price {:?}: {:?}", id, formatted_token_str, price))
                .into_diagnostic()
                .wrap_err(format!("Error calculating spot price for Pool {id:?}."))?;

            // Test get_amount_out with different percentages of limits. The reserves or limits
            // are relevant because we need to know how much to test with. We
            // don't know if a pool is going to revert with 10 or 10 million
            // USDC, for example, so by using the limits we can use "safe
            // values" where the sim shouldn't break. We then retrieve the
            // amount out for 0.1%, 1% and 10%.
            let percentages = [0.001, 0.01, 0.1];

            // Test all permutations of swap directions
            let swap_directions: Vec<_> = tokens
                .iter()
                .permutations(2)
                .map(|perm| (perm[0], perm[1]))
                .collect();

            for (token_in, token_out) in &swap_directions {
                let (max_input, max_output) = state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .into_diagnostic()
                    .wrap_err(format!(
                        "Error getting limits for Pool {id:?} for in token: {}, and out token: {}",
                        token_in.address, token_out.address
                    ))?;

                info!(
                    "[{}] Retrieved limits. | Max input: {max_input} {} | Max output: {max_output} {}",
                    id, token_in.symbol, token_out.symbol
                );

                for percentage in percentages.iter() {
                    // For precision, multiply by 1000 then divide by 1000
                    let percentage_biguint = BigUint::from((percentage * 1000.0) as u32);
                    let thousand = BigUint::from(1000u32);
                    let amount_in = (&max_input * &percentage_biguint) / &thousand;

                    // Skip if amount is zero
                    if amount_in.is_zero() {
                        return Err(miette!(
                                "Amount in multiplied by percentage {percentage} is zero for pool {id}."
                            ));
                    }

                    let amount_out_result = state
                            .get_amount_out(amount_in.clone(), token_in, token_out)
                            .into_diagnostic()
                            .wrap_err(format!(
                                "Error calculating amount out for Pool {id:?} at {:.1}% with input of {amount_in} {}.",
                                percentage * 100.0,
                                token_in.symbol,
                            ))?;

                    info!(
                            "[{}] Simulated amount out for trading {:.1}% of max: ({} {} -> {} {}) (gas: {})",
                            id,
                            percentage * 100.0,
                            amount_in,
                            token_in.symbol,
                            amount_out_result.amount,
                            token_out.symbol,
                            amount_out_result.gas
                        );

                    if skip_execution.contains(id) {
                        info!("Skipping execution for component {id}");
                        continue;
                    }

                    let chain_model = self.chain;
                    let executors_json = json!({
                        (chain_model.to_string()): {
                            (protocol_system): EXECUTOR_ADDRESS
                        }
                    });
                    let (solution, calldata) = encode_swap(
                        component,
                        None,
                        token_in,
                        token_out,
                        amount_in.clone(),
                        chain_model,
                        Some(executors_json.to_string()),
                        amount_out_result.gas.clone(),
                    )?;

                    // Create unique simulation ID
                    let simulation_id = format!(
                        "test_{}_{}_{}_{}",
                        component.id, token_in.symbol, token_out.symbol, amount_in
                    );

                    execution_data.insert(
                        simulation_id,
                        TychoExecutionInput {
                            solution: solution.clone(),
                            transaction: calldata.clone(),
                            expected_amount_out: amount_out_result.amount.clone(),
                            protocol_system: protocol_system.to_string(),
                            component_id: component.id.to_string(),
                            token_in: token_in.symbol.clone(),
                            token_out: token_out.symbol.clone(),
                            estimated_gas: amount_out_result.gas.clone(),
                        },
                    );
                }
            }
        }

        Ok(execution_data)
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
        if execution_data.is_empty() {
            info!("No execution data to process");
            return Ok(());
        }

        let skip_execution: HashSet<_> = expected_components
            .iter()
            .filter(|c| c.skip_execution)
            .map(|c| c.base.id.to_lowercase())
            .collect();

        // Filter out skipped components
        let filtered_execution_data: HashMap<_, _> = execution_data
            .into_iter()
            .filter(|(_, input)| !skip_execution.contains(&input.component_id.to_lowercase()))
            .collect();

        if filtered_execution_data.is_empty() {
            info!("All components skipped execution");
            return Ok(());
        }

        let chain_model = self.chain;
        let rpc_tools = RPCTools::new(self.rpc_provider.url.as_ref(), &chain_model).await?;

        // Prepare router overwrites data
        let router_overwrites_data =
            Some(execution::create_router_overwrites_data(protocol_system)?);

        info!("Executing {} simulations in batches ...", filtered_execution_data.len());

        // Split execution data into smaller batches to avoid RPC request size limits
        // This happens because our overwrites are colossal
        const BATCH_SIZE: usize = 30;
        let execution_batches: Vec<HashMap<String, TychoExecutionInput>> = filtered_execution_data
            .clone()
            .into_iter()
            .collect::<Vec<_>>()
            .chunks(BATCH_SIZE)
            .map(|chunk| chunk.iter().cloned().collect())
            .collect();

        let mut all_results = HashMap::new();

        // Process each batch sequentially
        for (batch_index, batch) in execution_batches.iter().enumerate() {
            info!(
                "Processing execution batch {} of {} ({} simulations)",
                batch_index + 1,
                execution_batches.len(),
                batch.len()
            );

            let batch_results = simulate_swap_transaction(
                &rpc_tools,
                batch.clone(),
                block,
                router_overwrites_data.clone(),
            )
            .await;

            let batch_results = match batch_results {
                Ok(results) => results,
                Err((error, _, _)) => {
                    error!("Batch {} failed: {:#}", batch_index + 1, error);
                    return Err(error);
                }
            };

            all_results.extend(batch_results);
        }

        let results = all_results;

        let mut success_count = 0;
        let mut failure_count = 0;

        for (simulation_id, expected_input) in &filtered_execution_data {
            match results.get(simulation_id) {
                Some(TychoExecutionResult::Success { amount_out, .. }) => {
                    info!(
                        "[{}] Execution passed: {} {} -> {} {}",
                        expected_input.component_id,
                        expected_input.solution.amount_in(),
                        expected_input.token_in,
                        amount_out,
                        expected_input.token_out
                    );

                    // Compare execution amount out with simulation amount out
                    let diff = BigInt::from(
                        expected_input
                            .expected_amount_out
                            .clone(),
                    ) - BigInt::from(amount_out.clone());
                    let slippage: BigRational =
                        BigRational::new(diff.abs(), BigInt::from(amount_out.clone()));

                    if slippage.to_f64() > Some(0.005) {
                        failure_count += 1;
                        error!(
                            "[{}] Execution amount and simulation amount differ more than 0.05% for {}: simulation={}, execution={}",
                            expected_input.component_id, simulation_id, expected_input.expected_amount_out, amount_out
                        );
                    } else {
                        success_count += 1;
                    }
                }
                Some(TychoExecutionResult::Revert { reason, .. }) => {
                    failure_count += 1;
                    error!(
                        "[{}] Execution reverted for {}: {}",
                        expected_input.component_id, simulation_id, reason
                    );
                }
                Some(TychoExecutionResult::Failed { error_msg }) => {
                    failure_count += 1;
                    error!(
                        "[{}] Execution failed for {}: {}",
                        expected_input.component_id, simulation_id, error_msg
                    );
                }
                None => {
                    failure_count += 1;
                    error!(
                        "[{}] No result found for simulation {}",
                        expected_input.component_id, simulation_id
                    );
                }
            }
        }

        info!("Batch execution complete: {} successes, {} failures", success_count, failure_count);

        Ok(())
    }

    /// Validate that the token balances of the components match the values
    /// on-chain, extracted by querying the token balances using a node.
    fn validate_token_balances(
        &self,
        component_tokens: &HashMap<String, Vec<Token>>,
        protocol_states_by_id: &HashMap<String, ProtocolComponentState>,
        stop_block: u64,
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
                    let component_address =
                        Address::from_str(id.as_str()).expect("Failed to parse component address");
                    let node_balance =
                        self.runtime
                            .block_on(self.rpc_provider.get_token_balance(
                                token_address,
                                component_address,
                                stop_block,
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
        adapter_contract: &Option<String>,
        adapter_build_signature: &Option<String>,
        adapter_build_args: &Option<String>,
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
                            adapter_build_signature.as_deref(),
                            adapter_build_args.as_deref(),
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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env, str::FromStr};

    use dotenv::dotenv;
    use glob::glob;
    use tycho_simulation::tycho_common::{models::protocol::ProtocolComponentState, Bytes};

    use super::*;

    #[test]
    fn test_parse_all_configs() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let curr_dir = PathBuf::from(manifest_dir);
        let parent_dir = curr_dir.parent().unwrap();
        env::set_current_dir(parent_dir).expect("Failed to set working directory");

        let pattern = "./substreams/*/integration_test.tycho.yaml";
        let mut results = Vec::new();

        if glob(pattern).unwrap().count() == 0 {
            panic!("No integration_test.tycho.yaml files found in substreams/*/");
        }
        for entry in glob(pattern).unwrap() {
            match entry {
                Ok(path) => {
                    if !path.is_file() {
                        results.push(Err(format!("Path is not a file: {}", path.display())));
                    } else {
                        let result = TestRunner::parse_config(&path);
                        if let Err(e) = &result {
                            results.push(Err(format!(
                                "Failed to parse config at {}: {e:?}",
                                path.display(),
                            )));
                        } else {
                            results.push(Ok(()));
                        }
                    }
                }
                Err(e) => results.push(Err(format!("Glob error: {e:?}"))),
            }
        }

        let errors: Vec<_> = results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .collect();
        if !errors.is_empty() {
            for error in errors {
                println!("{error:#}");
            }
            panic!("One or more config files failed to parse.");
        }
    }

    fn get_mocked_runner() -> TestRunner {
        dotenv().ok();
        let rpc_url = env::var("RPC_URL").unwrap();
        let current_dir = std::env::current_dir().unwrap();
        TestRunner::new(
            TestType::Range(TestTypeRange { match_test: None }),
            current_dir,
            Chain::Ethereum,
            "test-protocol".to_string(),
            "".to_string(),
            rpc_url,
            false,
            false,
        )
        .unwrap()
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
        balances.insert(token_bytes.clone(), balance_bytes.clone());
        let protocol_state = ProtocolComponentState::new(&component_id, HashMap::new(), balances);

        let mut component_tokens = HashMap::new();
        component_tokens.insert(component_id.clone(), vec![token]);
        let mut protocol_states_by_id = HashMap::new();
        protocol_states_by_id.insert(component_id.clone(), protocol_state);

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
        balances.insert(token_bytes.clone(), balance_bytes.clone());
        let protocol_state = ProtocolComponentState::new(&component_id, HashMap::new(), balances);

        let mut component_tokens = HashMap::new();
        component_tokens.insert(component_id.clone(), vec![token]);
        let mut protocol_states_by_id = HashMap::new();
        protocol_states_by_id.insert(component_id.clone(), protocol_state);

        dotenv().ok();
        let result =
            runner.validate_token_balances(&component_tokens, &protocol_states_by_id, block_number);
        assert!(
            result.is_err(),
            "Should fail when balance check is performed and balances do not match"
        );
    }
}
