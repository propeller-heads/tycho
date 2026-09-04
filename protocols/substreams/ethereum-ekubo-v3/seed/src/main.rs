//! Seed writer for the `ethereum-ekubo-v3` Substreams package.
//!
//! Reconstructs every Ekubo v3 pool on Ethereum at one block and writes it as the seed the package
//! embeds, so a manifest whose `initialBlock` is that block skips the protocol's history. The only
//! source so far is an archive node (`rpc`); the common subcommands come from `tycho-seed`.

mod layout;
mod packing;
mod pb;
mod reserves;
mod rpc_source;
mod time;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use prost::Message as _;
use tycho_seed::cli::{Common, Package};

use crate::pb::ekubo::Seed;

const PACKAGE: Package =
    Package { name: "ethereum-ekubo-v3", first_block: rpc_source::FIRST_BLOCK, describe };

#[derive(Parser)]
#[command(name = "ethereum-ekubo-v3-seed", about, version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reconstruct every pool at a block from an archive node's logs and storage.
    Rpc(rpc_source::Args),
    #[command(flatten)]
    Common(Common),
}

#[tokio::main]
async fn main() -> Result<()> {
    tycho_seed::cli::init();

    match Cli::parse().command {
        Command::Rpc(args) => rpc_source::run(&PACKAGE, args).await,
        Command::Common(common) => common.run(&PACKAGE).await,
    }
}

/// Human-readable summary of a seed body.
fn describe(body: &[u8]) -> Result<String> {
    let seed = Seed::decode(body).context("seed body is not a valid `ekubo.Seed` protobuf")?;

    let snapshots = seed
        .pools
        .iter()
        .filter_map(|pool| pool.snapshot.as_ref());
    let ticks: usize = snapshots
        .clone()
        .map(|snapshot| snapshot.ticks.len())
        .sum();
    let timed = snapshots
        .clone()
        .filter(|snapshot| snapshot.timed.is_some())
        .count();
    let rate_deltas: usize = snapshots
        .filter_map(|snapshot| snapshot.timed.as_ref())
        .map(|timed| timed.rate_deltas.len())
        .sum();

    Ok(format!(
        "{} pools, {ticks} ticks, {timed} timed pools with {rate_deltas} rate deltas",
        seed.pools.len()
    ))
}
