//! The subcommands every seed writer offers besides its sources.
//!
//! A writer flattens [`Common`] into its own subcommand enum and dispatches to [`Common::run`]
//! with its [`Package`]:
//!
//! ```ignore
//! #[derive(clap::Subcommand)]
//! enum Command {
//!     /// Reconstruct the state at a block from an archive node.
//!     Rpc(rpc_source::Args),
//!     #[command(flatten)]
//!     Common(tycho_seed::cli::Common),
//! }
//! ```

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;
use tycho_seed_format::Header;

use crate::{file, manifest, rpc::Rpc};

/// What the common subcommands need to know about the package a writer serves.
pub struct Package {
    /// Cargo package name of the Substreams package; every seed header carries it and the package
    /// refuses seeds written for another name.
    pub name: &'static str,
    /// The stock manifest's `initialBlock`. The empty seed is pinned to the block before it, which
    /// the stock manifest never streams.
    pub first_block: u64,
    /// One-line summary of a decoded seed body, printed by `inspect`.
    pub describe: fn(&[u8]) -> Result<String>,
}

#[derive(Subcommand)]
pub enum Common {
    /// Write a seed with an empty body, pinned to a block: the file a package embeds when it is
    /// not meant to start from a snapshot.
    Empty(EmptyArgs),
    /// Print the header and a summary of a seed file.
    Inspect(InspectArgs),
    /// Write a copy of a Substreams manifest with every `initialBlock` set to the seed block.
    Manifest(ManifestArgs),
}

#[derive(Args)]
pub struct EmptyArgs {
    /// Block the empty seed is pinned to; defaults to the block before the protocol's first.
    #[arg(long)]
    block: Option<u64>,
    /// Path of the seed file to write.
    #[arg(long)]
    out: PathBuf,
    /// JSON-RPC URL used to look up the block hash.
    #[arg(long, env = "RPC_URL", hide_env_values = true)]
    rpc_url: String,
}

#[derive(Args)]
pub struct InspectArgs {
    seed: PathBuf,
    /// Print only the seed block number.
    #[arg(long)]
    block_number: bool,
}

#[derive(Args)]
pub struct ManifestArgs {
    #[arg(long)]
    seed: PathBuf,
    #[arg(long = "in")]
    input: PathBuf,
    #[arg(long = "out")]
    output: PathBuf,
}

impl Common {
    pub async fn run(self, package: &Package) -> Result<()> {
        match self {
            Common::Empty(args) => {
                let block = args
                    .block
                    .unwrap_or(package.first_block - 1);
                let header = Rpc::connect(&args.rpc_url)?
                    .block_header(block)
                    .await?;
                let header = Header {
                    package: package.name.to_owned(),
                    block_number: header.number,
                    block_hash: header.hash.0,
                };
                let bytes = file::write(&args.out, &header, &[])?;
                info!(path = %args.out.display(), bytes, "wrote empty seed");
                println!("{}", summary(&header, "empty"));
            }
            Common::Inspect(args) => {
                let (header, body) = file::read_for(&args.seed, package.name)?;
                if args.block_number {
                    println!("{}", header.block_number);
                } else {
                    println!("{}", summary(&header, &(package.describe)(&body)?));
                }
            }
            Common::Manifest(args) => {
                let (header, _) = file::read_for(&args.seed, package.name)?;
                let yaml = fs::read_to_string(&args.input)
                    .with_context(|| format!("reading manifest {}", args.input.display()))?;
                fs::write(&args.output, manifest::rewrite(&yaml, header.block_number)?)
                    .with_context(|| format!("writing manifest {}", args.output.display()))?;
                println!(
                    "wrote {} with initialBlock {}",
                    args.output.display(),
                    header.block_number
                );
            }
        }

        Ok(())
    }
}

fn summary(header: &Header, body: &str) -> String {
    format!(
        "{} at block {} (0x{}): {body}",
        header.package,
        header.block_number,
        alloy::hex::encode(header.block_hash)
    )
}

/// Loads `.env` and installs a `RUST_LOG`-driven subscriber that defaults to `info`.
pub fn init() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
