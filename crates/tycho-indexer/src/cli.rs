use clap::{Args, Parser, Subcommand};
use tycho_common::{models::Chain, Bytes};
use tycho_ethereum::rpc::{
    config::{RPCBatchingConfig, RPCRetryConfig},
    EthereumRpcClient,
};

use crate::extractor::ExtractionError;

/// Tycho Indexer using Substreams
///
/// Extracts state from the Ethereum blockchain and stores it in a Postgres database.
#[derive(Parser, PartialEq, Debug)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    #[command(flatten)]
    global_args: GlobalArgs,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    pub fn args(&self) -> GlobalArgs {
        self.global_args.clone()
    }

    pub fn command(&self) -> Command {
        self.command.clone()
    }
}

#[derive(Subcommand, Clone, PartialEq, Debug)]
pub enum Command {
    /// Starts the indexing service.
    Index(IndexArgs),
    /// Runs a single substream, intended for testing.
    Run(RunSpkgArgs),
    /// Starts a job to analyze stored tokens for tax and gas cost.
    AnalyzeTokens(AnalyzeTokenArgs),
    /// Starts Tycho RPC only. No extractors.
    Rpc,
}

#[derive(Parser, Debug, Clone, PartialEq)]
#[command(version, about, long_about = None)]
pub struct GlobalArgs {
    /// PostgresDB Connection Url
    #[clap(
        long,
        env,
        hide_env_values = true,
        default_value = "postgres://postgres:mypassword@localhost:5431/tycho_indexer_0"
    )]
    pub database_url: String,

    /// Batch size for the database inserts
    #[clap(long, default_value = "0")]
    pub database_insert_batch_size: usize,

    /// Name of the s3 bucket used to retrieve spkgs
    #[clap(env = "TYCHO_S3_BUCKET", long, default_value = "repo.propellerheads-propellerheads")]
    //Default is for backward compatibility but needs to be removed later
    pub s3_bucket: Option<String>,

    /// Substreams API endpoint
    #[clap(name = "endpoint", long, default_value = "https://mainnet.eth.streamingfast.io")]
    pub endpoint_url: String,

    /// The server IP
    #[clap(long, default_value = "0.0.0.0")]
    pub server_ip: String,

    /// The server port
    #[clap(long, default_value = "4242")]
    pub server_port: u16,

    /// The server version prefix
    #[clap(long, default_value = "v1")]
    pub server_version_prefix: String,

    /// RPC configuration (URL and retry settings)
    #[command(flatten)]
    pub rpc: RPCArgs,
}

/// RPC configuration arguments (url, retry settings, and potentially others, such as batching)
#[derive(Args, Debug, Clone, PartialEq)]
pub struct RPCArgs {
    /// The RPC URL to connect to the Blockchain node
    #[clap(long = "rpc-url", env = "RPC_URL", hide_env_values = true)]
    pub url: String,

    /// Maximum number of RPC retry attempts for failed requests
    #[clap(long = "rpc-max-retries", env = "RPC_MAX_RETRIES", default_value = "5")]
    pub max_retries: usize,

    /// Initial backoff delay in milliseconds before the first retry
    #[clap(long = "rpc-initial-backoff-ms", env = "RPC_INITIAL_BACKOFF_MS", default_value = "150")]
    pub initial_backoff_ms: u64,

    /// Maximum backoff delay in milliseconds (backoff is capped at this value)
    #[clap(long = "rpc-max-backoff-ms", env = "RPC_MAX_BACKOFF_MS", default_value = "5000")]
    pub max_backoff_ms: u64,

    /// Maximum number of requests per RPC batch call. Set to 0 to disable batching.
    /// If not set, uses the client default.
    #[clap(long = "rpc-max-batch-size", env = "RPC_MAX_BATCH_SIZE")]
    pub max_batch_size: Option<usize>,

    /// Maximum number of eth_getStorageAt requests per batch. Overrides max_batch_size for storage
    /// slot queries. If not set, falls back to max_batch_size.
    #[clap(long = "rpc-storage-slot-max-batch-size", env = "RPC_STORAGE_SLOT_MAX_BATCH_SIZE")]
    pub storage_slot_max_batch_size: Option<usize>,
}

impl RPCArgs {
    pub fn build_client(&self) -> Result<EthereumRpcClient, ExtractionError> {
        let retry_config =
            RPCRetryConfig::new(self.max_retries, self.initial_backoff_ms, self.max_backoff_ms);

        if self.storage_slot_max_batch_size == Some(0) {
            panic!("--rpc-storage-slot-max-batch-size must be greater than 0");
        }

        let batching_config = match self.max_batch_size {
            Some(0) => Some(RPCBatchingConfig::Disabled),
            Some(max_batch_size) => Some(RPCBatchingConfig::Enabled {
                max_batch_size,
                storage_slot_max_batch_size_override: self.storage_slot_max_batch_size,
            }),
            None => None,
        };

        EthereumRpcClient::new(&self.url)
            .map_err(|e| ExtractionError::Setup(format!("Failed to create RPC client: {e}")))
            .map(|client| {
                let client = client.with_retry(retry_config);
                match batching_config {
                    Some(config) => client.with_batching(config),
                    None => client,
                }
            })
    }
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct SubstreamsArgs {
    /// Substreams API token
    #[clap(long, env, hide_env_values = true, alias = "api_token")]
    pub substreams_api_token: String,
    /// Enable partial blocks
    #[clap(long, default_value = "false")]
    pub enable_partial_blocks: bool,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct IndexArgs {
    #[clap(flatten)]
    pub substreams_args: SubstreamsArgs,

    /// Extractors configuration file
    #[clap(long, env, default_value = "./extractors.yaml")]
    pub extractors_config: String,

    /// A comma separated list of blockchains to index on
    #[clap(long, default_value = "ethereum", value_delimiter = ',')]
    pub chains: Vec<String>,

    /// Retention horizon date
    ///
    /// Any data before this date is not kept in storage.
    #[clap(long, env, default_value = "2024-01-01T00:00:00")]
    pub retention_horizon: String,

    /// Settlement contract address used for token transfer simulation.
    /// Defaults to the CoW Swap settlement contract on Ethereum mainnet.
    #[clap(long, default_value = "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")]
    pub settlement_contract: alloy::primitives::Address,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct RunSpkgArgs {
    /// The blockchain to index on
    #[clap(long, default_value = "ethereum")]
    pub chain: String,

    #[clap(flatten)]
    pub substreams_args: SubstreamsArgs,

    /// Substreams Package file
    #[clap(long)]
    pub spkg: String,

    /// Substreams Module name
    #[clap(long)]
    pub module: String,

    // The names of the protocol_types to index
    #[clap(long, value_delimiter = ',')]
    pub protocol_type_names: Vec<String>,

    // Protocol system to index
    #[clap(long)]
    pub protocol_system: String,

    /// Substreams start block
    #[clap(long)]
    pub start_block: i64,

    /// Substreams stop block
    ///
    /// Optional. If not provided, the extractor will run until the latest block.
    /// If prefixed with a `+` the value is interpreted as an increment to the start block.
    /// Defaults to STOP_BLOCK env var or None.
    #[clap(long)]
    stop_block: Option<String>,

    /// Account addresses to be initialized before indexing
    #[clap(long, value_delimiter = ',')]
    pub initialized_accounts: Vec<Bytes>,

    /// Block number to initialize the accounts at
    #[clap(long, default_value = "0")]
    pub initialization_block: u64,

    /// DCI plugin to use
    ///
    /// Optional. If not provided, the extractor will not use DCI. Available plugins:
    /// - `rpc` - RPC is used to trace and retrieve detected accounts.
    #[clap(long)]
    pub dci_plugin: Option<String>,

    /// Settlement contract address used for token transfer simulation.
    /// Defaults to the CoW Swap settlement contract on Ethereum mainnet.
    #[clap(long, default_value = "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")]
    pub settlement_contract: alloy::primitives::Address,
}

impl RunSpkgArgs {
    pub fn stop_block(&self) -> Option<i64> {
        if let Some(s) = &self.stop_block {
            if s.starts_with('+') {
                let increment: i64 = s
                    .strip_prefix('+')
                    .expect("stripped stop block value")
                    .parse()
                    .expect("stop block value");
                Some(self.start_block + increment)
            } else {
                Some(s.parse().expect("stop block value"))
            }
        } else {
            None
        }
    }
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeTokenArgs {
    /// Blockchain to execute analysis for.
    #[clap(long)]
    pub chain: Chain,
    /// Settlement contract address used for transfer simulation.
    /// Defaults to the CoW Swap settlement contract on Ethereum mainnet.
    #[clap(long, default_value = "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")]
    pub settlement_contract: alloy::primitives::Address,
    /// How many concurrent threads to use for token analysis.
    #[clap(long)]
    pub concurrency: usize,
    /// How many tokens to update in a batch per thread.
    #[clap(long)]
    pub update_batch_size: usize,
    /// How many tokens to fetch from the db to distribute to threads (page size). This
    /// should be at least `concurrency * update_batch_size`.
    #[clap(long)]
    pub fetch_batch_size: usize,
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[tokio::test]
    async fn test_arg_parsing_run_cmd() {
        let cli = Cli::try_parse_from(vec![
            "tycho-indexer",
            "--endpoint",
            "http://example.com",
            "--database-url",
            "my_db",
            "--database-insert-batch-size",
            "256",
            "--rpc-url",
            "http://example.com",
            "run",
            "--api_token",
            "your_api_token",
            "--spkg",
            "package.spkg",
            "--module",
            "module_name",
            "--start-block",
            "17361664",
            "--protocol-type-names",
            "pt1,pt2",
            "--protocol-system",
            "test_protocol",
        ])
        .expect("parse errored");

        let expected_args = Cli {
            global_args: GlobalArgs {
                endpoint_url: "http://example.com".to_string(),
                database_url: "my_db".to_string(),
                database_insert_batch_size: 256,
                s3_bucket: Some("repo.propellerheads-propellerheads".to_string()),
                server_ip: "0.0.0.0".to_string(),
                server_port: 4242,
                server_version_prefix: "v1".to_string(),
                rpc: RPCArgs {
                    url: "http://example.com".to_string(),
                    max_retries: 5,
                    initial_backoff_ms: 150,
                    max_backoff_ms: 5000,
                    max_batch_size: None,
                    storage_slot_max_batch_size: None,
                },
            },
            command: Command::Run(RunSpkgArgs {
                chain: "ethereum".to_string(),
                spkg: "package.spkg".to_string(),
                module: "module_name".to_string(),
                protocol_type_names: vec!["pt1".to_string(), "pt2".to_string()],
                protocol_system: "test_protocol".to_string(),
                start_block: 17361664,
                stop_block: None,
                substreams_args: SubstreamsArgs {
                    substreams_api_token: "your_api_token".to_string(),
                    enable_partial_blocks: false,
                },
                initialized_accounts: vec![],
                initialization_block: 0,
                dci_plugin: None,
                settlement_contract: "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58"
                    .parse()
                    .unwrap(),
            }),
        };

        assert_eq!(cli, expected_args);
    }

    #[tokio::test]
    async fn test_arg_parsing_index_cmd() {
        let cli = Cli::try_parse_from(vec![
            "tycho-indexer",
            "--endpoint",
            "http://example.com",
            "--database-url",
            "my_db",
            "--rpc-url",
            "http://example.com",
            "--rpc-max-retries",
            "10",
            "--rpc-initial-backoff-ms",
            "200",
            "--rpc-max-backoff-ms",
            "10000",
            "index",
            "--extractors-config",
            "/opt/extractors.yaml",
            "--api_token",
            "your_api_token",
            "--enable-partial-blocks",
        ])
        .expect("parse errored");

        let expected_args = Cli {
            global_args: GlobalArgs {
                endpoint_url: "http://example.com".to_string(),
                database_url: "my_db".to_string(),
                database_insert_batch_size: 0,
                s3_bucket: Some("repo.propellerheads-propellerheads".to_string()),
                server_ip: "0.0.0.0".to_string(),
                server_port: 4242,
                server_version_prefix: "v1".to_string(),
                rpc: RPCArgs {
                    url: "http://example.com".to_string(),
                    max_retries: 10,
                    initial_backoff_ms: 200,
                    max_backoff_ms: 10000,
                    max_batch_size: None,
                    storage_slot_max_batch_size: None,
                },
            },
            command: Command::Index(IndexArgs {
                substreams_args: SubstreamsArgs {
                    substreams_api_token: "your_api_token".to_string(),
                    enable_partial_blocks: true,
                },
                chains: vec!["ethereum".to_string()],
                extractors_config: "/opt/extractors.yaml".to_string(),
                retention_horizon: "2024-01-01T00:00:00".to_string(),
                settlement_contract: "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58"
                    .parse()
                    .unwrap(),
            }),
        };

        assert_eq!(cli, expected_args);
    }

    #[test]
    fn test_arg_parsing_missing_val() {
        let args = Cli::try_parse_from(vec![
            "tycho-indexer",
            "--spkg",
            "package.spkg",
            "--module",
            "module_name",
        ]);

        assert!(args.is_err());
    }

    #[test]
    fn test_rpc_args_conversion_to_rpc() {
        let rpc_args = RPCArgs {
            url: "https://eth.example.com".to_string(),
            max_retries: 7,
            initial_backoff_ms: 250,
            max_backoff_ms: 8000,
            max_batch_size: Some(100),
            storage_slot_max_batch_size: Some(2000),
        };

        let rpc_client = rpc_args.build_client().unwrap();

        let rpc_config = rpc_client.get_retry_config();
        assert_eq!(rpc_config.max_retries, 7);
        assert_eq!(rpc_config.initial_backoff_ms, 250);
        assert_eq!(rpc_config.max_backoff_ms, 8000);
        assert_eq!(rpc_client.get_url(), "https://eth.example.com");

        let batching_config = rpc_client.get_batching_config();
        assert_eq!(batching_config.max_batch_size(), Some(100));
        assert_eq!(batching_config.storage_slot_max_batch_size(), Some(2000));
    }

    #[test]
    fn test_rpc_args_storage_slot_falls_back_to_max_batch_size() {
        let rpc_args = RPCArgs {
            url: "https://eth.example.com".to_string(),
            max_retries: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 5000,
            max_batch_size: Some(10),
            storage_slot_max_batch_size: None,
        };

        let rpc_client = rpc_args.build_client().unwrap();
        let batching_config = rpc_client.get_batching_config();
        assert_eq!(batching_config.max_batch_size(), Some(10));
        assert_eq!(batching_config.storage_slot_max_batch_size(), Some(10));
    }

    #[test]
    fn test_rpc_args_batching_disabled() {
        let rpc_args = RPCArgs {
            url: "https://eth.example.com".to_string(),
            max_retries: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 5000,
            max_batch_size: Some(0),
            storage_slot_max_batch_size: None,
        };

        let rpc_client = rpc_args.build_client().unwrap();
        let batching_config = rpc_client.get_batching_config();
        assert_eq!(batching_config.max_batch_size(), None);
        assert_eq!(batching_config.storage_slot_max_batch_size(), None);
    }

    #[test]
    #[should_panic(expected = "--rpc-storage-slot-max-batch-size must be greater than 0")]
    fn test_rpc_args_zero_storage_slot_batch_size_panics() {
        let rpc_args = RPCArgs {
            url: "https://eth.example.com".to_string(),
            max_retries: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 5000,
            max_batch_size: Some(50),
            storage_slot_max_batch_size: Some(0),
        };

        rpc_args.build_client().unwrap();
    }
}
