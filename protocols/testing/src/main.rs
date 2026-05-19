mod adapter_builder;
mod config;
mod execution;
mod rpc;
mod state_registry;
mod test_runner;
mod tycho_rpc;
mod tycho_runner;
mod utils;

use std::{fmt::Display, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use dotenv::dotenv;
use miette::{miette, IntoDiagnostic, WrapErr};
use tracing_subscriber::EnvFilter;
use tycho_simulation::tycho_common::models::Chain;

use crate::test_runner::{TestRunner, TestType, TestTypeFull, TestTypeRange};

#[derive(Parser)]
#[command(version, long_version = Version::clap_long(), subcommand_required = false, arg_required_else_help = true)]
struct TestsCli {
    #[command(subcommand)]
    subcommand: Option<TestSubcommand>,
}

#[derive(Subcommand)]
enum TestSubcommand {
    Full(FullTestCommand),
    Range(RangeTestCommand),
}

/// Run continuous sync test from a specific initial block
#[derive(Args)]
pub struct FullTestCommand {
    #[command(flatten)]
    common_args: CommonArgs,

    /// Start the test from this block number.
    /// If not provided, it will use the first initial block defined in the protocol's substream
    /// configuration.
    #[arg(long)]
    initial_block: Option<u64>,
}

impl FullTestCommand {
    fn run(self) -> miette::Result<()> {
        let args = self.common_args;
        TestRunner::new(
            TestType::Full(TestTypeFull { initial_block: self.initial_block }),
            args.root_path()?,
            args.chain,
            args.package,
            args.db_url,
            args.rpc_url,
            args.vm_simulation_traces,
            args.reuse_last_sync,
        )?
        .run()
    }
}

/// Run the tests defined in the protocol's integration_test.tycho.yaml file
#[derive(Args)]
pub struct RangeTestCommand {
    #[command(flatten)]
    common_args: CommonArgs,

    /// If provided, only run the tests with a matching name
    #[arg(long)]
    match_test: Option<String>,
}

impl RangeTestCommand {
    fn run(self) -> miette::Result<()> {
        let args = self.common_args;
        TestRunner::new(
            TestType::Range(TestTypeRange { match_test: self.match_test.clone() }),
            args.root_path()?,
            args.chain,
            args.package,
            args.db_url,
            args.rpc_url,
            args.vm_simulation_traces,
            args.reuse_last_sync,
        )?
        .run()
    }
}

#[derive(Args)]
struct CommonArgs {
    /// Path to the root directory containing all packages. If not provided, it will look for
    /// packages in the current working directory.
    root_path: Option<PathBuf>,

    /// Name of the package to test
    #[arg(long)]
    package: String,

    /// Name of the chain to test
    #[arg(long, default_value = "ethereum")]
    chain: Chain,

    /// Postgres database URL for the Tycho indexer
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgres://postgres:mypassword@localhost:5431/tycho_indexer_0"
    )]
    db_url: String,

    #[arg(long, env = "RPC_URL")]
    rpc_url: String,

    /// Enable tracing during vm simulations
    #[arg(long, default_value_t = false)]
    vm_simulation_traces: bool,

    /// Enable tracing during execution simulations
    #[arg(long, default_value_t = false)]
    execution_traces: bool,

    /// If false (default), clears the DB and re-syncs from start. If true:
    ///   - for the ranged test, it skips indexing and uses the current state
    ///   - for the full test, it starts syncing from the last block in the db
    #[arg(long, default_value_t = false)]
    reuse_last_sync: bool,
}

impl CommonArgs {
    fn root_path(&self) -> miette::Result<PathBuf> {
        match self.root_path.as_ref() {
            Some(path) => Ok(path.clone()),
            None => {
                let current_dir = std::env::current_dir()
                    .into_diagnostic()
                    .wrap_err("Failed to get current directory")?;
                let expected_child_dirs = ["substreams", "adapter-integration/evm"];
                let mut dir = current_dir.as_path();
                loop {
                    if expected_child_dirs
                        .iter()
                        .all(|child| dir.join(child).exists())
                    {
                        return Ok(dir.to_path_buf());
                    }
                    dir = dir.parent().ok_or_else(|| {
                        miette!(
                            "Couldn't find a valid root path (containing \
                             substreams/ and adapter-integration/evm/) \
                             in any ancestor of {}",
                            current_dir.display()
                        )
                    })?;
                }
            }
        }
    }
}

struct Version {
    version: String,
    hash: String,
}

impl Version {
    fn from_env() -> miette::Result<Self> {
        let version = option_env!("CARGO_PKG_VERSION")
            .unwrap_or("unknown")
            .to_string();
        let hash = option_env!("GIT_HASH")
            .unwrap_or("unknown")
            .to_string();
        Ok(Self { version, hash })
    }

    fn clap_long() -> &'static str {
        Box::leak(
            Self::from_env()
                .expect("Failed to get binary version")
                .to_string()
                .into_boxed_str(),
        )
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "version: {}, hash: {}", self.version, self.hash)
    }
}

fn main() -> miette::Result<()> {
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = TestsCli::parse();
    match cli.subcommand {
        Some(TestSubcommand::Full(cmd)) => cmd.run(),
        Some(TestSubcommand::Range(cmd)) => cmd.run(),
        None => Err(miette!("No subcommand provided. Use --help for more information.")),
    }
}
