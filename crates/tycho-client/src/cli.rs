use std::{path::Path, str::FromStr};

use clap::Parser;
use tracing::info;
use tracing_appender::rolling;
use tycho_common::{dto::TvlThresholdTier, models::Chain};

use crate::{
    feed::{component_tracker::ComponentFilter, dto::FeedMessage as FeedMessageDto},
    stream::TychoStreamBuilder,
};

/// Tycho Client CLI - A tool for indexing and tracking blockchain protocol data
///
/// This CLI tool connects to a Tycho server and tracks various blockchain protocols,
/// providing real-time updates about their state.
#[derive(Parser, Debug, Clone, PartialEq)]
#[clap(version = env!("CARGO_PKG_VERSION"))]
struct CliArgs {
    /// Tycho server URL, without protocol. Example: localhost:4242
    #[clap(long, default_value = "localhost:4242", env = "TYCHO_URL")]
    tycho_url: String,

    /// Tycho gateway API key, used as authentication for both websocket and http connections.
    /// Can be set with TYCHO_AUTH_TOKEN env variable.
    #[clap(short = 'k', long, env = "TYCHO_AUTH_TOKEN")]
    auth_key: Option<String>,

    /// If set, use unsecured transports: http and ws instead of https and wss.
    #[clap(long)]
    no_tls: bool,

    /// The blockchain to index on
    #[clap(short = 'c', long, default_value = "ethereum")]
    pub chain: String,

    /// Specifies exchanges. Optionally also supply a pool address in the format
    /// {exchange}-{pool_address}
    #[clap(short = 'e', long, number_of_values = 1)]
    exchange: Vec<String>,

    /// Specifies the minimum TVL to filter the components. Denoted in the native token.
    /// Defaults to a chain-appropriate value targeting ~$20K USD equivalent.
    /// Ignored if addresses or range tvl values are provided.
    #[clap(long)]
    min_tvl: Option<f64>,

    /// Specifies the lower bound of the TVL threshold range. Denoted in the native token (e.g.
    /// Mainnet -> ETH). Components below this TVL will be removed from tracking.
    #[clap(long)]
    remove_tvl_threshold: Option<f64>,

    /// Specifies the upper bound of the TVL threshold range. Denoted in the native token (e.g.
    /// Mainnet -> ETH). Components above this TVL will be added to tracking.
    #[clap(long)]
    add_tvl_threshold: Option<f64>,

    /// Expected block time in seconds. Defaults to the canonical block time for the selected
    /// chain (e.g. 12s for Ethereum, 2s for Base).
    ///
    /// Adjusting `block_time` helps balance efficiency and responsiveness:
    /// - **Low values**: Increase sync frequency but may waste resources on retries.
    /// - **High values**: Reduce sync frequency but may delay updates on faster chains.
    #[clap(long)]
    block_time: Option<u64>,

    /// Maximum wait time in seconds beyond the block time. Useful for handling
    /// chains with variable block intervals or network delays. Defaults to a chain-appropriate
    /// value.
    #[clap(long)]
    timeout: Option<u64>,

    /// Logging folder path.
    #[clap(long, default_value = "logs")]
    log_folder: String,

    /// Run the example on a single block with UniswapV2 and UniswapV3.
    #[clap(long)]
    example: bool,

    /// If set, only component and tokens are streamed, any snapshots or state updates
    /// are omitted from the stream.
    #[clap(long)]
    no_state: bool,

    /// Maximum amount of messages to process before exiting. Useful for debugging e.g.
    /// to easily get a state sync messages for a fixture. Alternatively this may be
    /// used to trigger a regular restart or resync.
    #[clap(short='n', long, default_value=None)]
    max_messages: Option<usize>,

    /// Maximum blocks an exchange can be absent for before it is marked as stale. Used
    /// in conjunction with block_time to calculate a timeout: block_time * max_missed_blocks.
    /// Defaults to a chain-appropriate value.
    #[clap(long)]
    max_missed_blocks: Option<u64>,

    /// If set, the synchronizer will include TVL in the messages.
    /// Enabling this option will increase the number of network requests made during start-up,
    /// which may result in increased start-up latency.
    #[clap(long)]
    include_tvl: bool,

    /// If set, disable compression for WebSocket messages.
    /// By default, messages are compressed using zstd.
    #[clap(long)]
    disable_compression: bool,

    /// If set, enables receiving partial block updates (flashblocks).
    /// This allows the client to receive incremental updates within a block, allowing for
    /// lower latency.
    #[clap(long)]
    partial_blocks: bool,

    /// Enable verbose logging. This will show more detailed information about the
    /// synchronization process and any errors that occur.
    #[clap(long)]
    verbose: bool,

    /// Maximum number of retry attempts for failed startups
    #[clap(long, default_value = "32")]
    max_retries: u64,

    /// Path to a TOML file containing component IDs to exclude from tracking.
    #[clap(long)]
    blocklist_config: Option<std::path::PathBuf>,
}

impl CliArgs {
    fn validate(&self) -> Result<(), String> {
        // TVL thresholds must be set together - either both or neither
        match (self.remove_tvl_threshold, self.add_tvl_threshold) {
            (Some(remove), Some(add)) if remove >= add => {
                return Err("remove_tvl_threshold must be less than add_tvl_threshold".to_string());
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    "Both remove_tvl_threshold and add_tvl_threshold must be set.".to_string()
                );
            }
            _ => {}
        }

        Ok(())
    }
}

#[derive(serde::Deserialize)]
struct BlocklistFile {
    ids: Vec<String>,
}

fn load_blocklist(path: &Path) -> Result<Vec<String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read blocklist file {}: {e}", path.display()))?;
    let file: BlocklistFile = toml::from_str(&content)
        .map_err(|e| format!("Failed to parse blocklist file {}: {e}", path.display()))?;
    Ok(file.ids)
}

pub async fn run_cli() -> Result<(), String> {
    // Parse CLI Args
    let args: CliArgs = CliArgs::parse();
    args.validate()?;

    // Setup Logging
    let log_level = if args.verbose { "debug" } else { "info" };
    let (non_blocking, _guard) =
        tracing_appender::non_blocking(rolling::never(&args.log_folder, "dev_logs.log"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .with_writer(non_blocking)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| format!("Failed to set up logging subscriber: {e}"))?;

    // Build the list of exchanges.  When --example is provided, we seed the list with a fixed
    // pair of well-known pools, otherwise we parse user supplied values (either plain exchange
    // names or exchange-pool pairs in the {exchange}-{pool_address} format).
    let exchanges: Vec<(String, Option<String>)> = if args.example {
        // You will need to port-forward tycho to run the example:
        //
        // ```bash
        // kubectl port-forward -n dev-tycho deploy/tycho-indexer 8888:4242
        // ```
        vec![
            (
                "uniswap_v3".to_string(),
                Some("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640".to_string()),
            ),
            (
                "uniswap_v2".to_string(),
                Some("0xa478c2975ab1ea89e8196811f51a7b7ade33eb11".to_string()),
            ),
        ]
    } else {
        args.exchange
            .iter()
            .filter_map(|e| {
                if e.contains('-') {
                    let parts: Vec<&str> = e.split('-').collect();
                    if parts.len() == 2 {
                        Some((parts[0].to_string(), Some(parts[1].to_string())))
                    } else {
                        tracing::warn!("Ignoring invalid exchange format: {}", e);
                        None
                    }
                } else {
                    Some((e.to_string(), None))
                }
            })
            .collect()
    };

    info!("Running with exchanges: {:?}", exchanges);

    run(exchanges, args).await?;
    Ok(())
}

async fn run(exchanges: Vec<(String, Option<String>)>, args: CliArgs) -> Result<(), String> {
    let blocklist = match &args.blocklist_config {
        Some(path) => load_blocklist(path)?,
        None => Vec::new(),
    };

    let chain = Chain::from_str(&args.chain)
        .map_err(|_| format!("Unknown chain: {chain}", chain = args.chain))?;

    let mut builder = TychoStreamBuilder::new(&args.tycho_url, chain)
        .auth_key(args.auth_key)
        .no_tls(args.no_tls)
        .no_state(args.no_state)
        .include_tvl(args.include_tvl)
        .max_retries(args.max_retries)
        .blocklisted_ids(blocklist);

    if let Some(bt) = args.block_time {
        builder = builder.block_time(bt);
    }
    if let Some(to) = args.timeout {
        builder = builder.timeout(to);
    }
    if let Some(mmb) = args.max_missed_blocks {
        builder = builder.max_missed_blocks(mmb);
    }
    if args.disable_compression {
        builder = builder.disable_compression();
    }
    if args.partial_blocks {
        builder = builder.enable_partial_blocks();
    }
    if let Some(n) = args.max_messages {
        builder = builder.max_messages(n);
    }
    // Register exchanges
    let builder = exchanges
        .into_iter()
        .fold(builder, |b, (name, address)| {
            let filter = if let Some(addr) = address {
                ComponentFilter::Ids(vec![addr])
            } else if let (Some(remove_tvl), Some(add_tvl)) =
                (args.remove_tvl_threshold, args.add_tvl_threshold)
            {
                ComponentFilter::with_tvl_range(remove_tvl, add_tvl)
            } else {
                let default_min_tvl = chain.default_tvl_threshold(TvlThresholdTier::Low);
                let min_tvl = args.min_tvl.unwrap_or(default_min_tvl);
                ComponentFilter::with_tvl_range(min_tvl, min_tvl)
            };
            b.exchange(&name, filter)
        });

    let (handle, mut rx) = builder
        .build()
        .await
        .map_err(|e| e.to_string())?;

    let msg_printer = tokio::spawn(async move {
        while let Some(result) = rx.recv().await {
            let msg =
                result.map_err(|e| format!("Message printer received synchronizer error: {e}"))?;

            let json = serde_json::to_string(&FeedMessageDto::from(msg))
                .map_err(|e| format!("Message printer failed to serialize message: {e}"))?;
            println!("{json}");
        }

        Ok::<(), String>(())
    });

    // Monitor the stream handle and message printer futures.
    let (failed_task, shutdown_reason) = tokio::select! {
        res = handle => (
            "Stream",
            res.err().map(|e| e.to_string())
        ),
        res = msg_printer => (
            "MessagePrinter",
            extract_nested_error(res)
        ),
    };

    Err(format!(
        "{failed_task} task terminated: {}",
        shutdown_reason.unwrap_or("unknown reason".to_string())
    ))
}

#[inline]
fn extract_nested_error<T, E1: ToString, E2: ToString>(
    res: Result<Result<T, E1>, E2>,
) -> Option<String> {
    res.map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
        .err()
}

#[cfg(test)]
mod cli_tests {
    use clap::Parser;

    use super::CliArgs;

    #[tokio::test]
    async fn test_cli_args() {
        let args = CliArgs::parse_from([
            "tycho-client",
            "--tycho-url",
            "localhost:5000",
            "--exchange",
            "uniswap_v2",
            "--min-tvl",
            "3000",
            "--block-time",
            "50",
            "--timeout",
            "5",
            "--log-folder",
            "test_logs",
            "--example",
            "--max-messages",
            "1",
            "--blocklist-config",
            "blocklist.toml",
        ]);
        let exchanges: Vec<String> = vec!["uniswap_v2".to_string()];
        assert_eq!(args.tycho_url, "localhost:5000");
        assert_eq!(args.exchange, exchanges);
        assert_eq!(args.min_tvl, Some(3000.0));
        assert_eq!(args.block_time, Some(50));
        assert_eq!(args.timeout, Some(5));
        assert_eq!(args.log_folder, "test_logs");
        assert_eq!(args.max_messages, Some(1));
        assert!(args.example);
        assert_eq!(args.disable_compression, false);
        assert_eq!(args.partial_blocks, false);
        assert_eq!(args.blocklist_config, Some(std::path::PathBuf::from("blocklist.toml")));
    }
}
