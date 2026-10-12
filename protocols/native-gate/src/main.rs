//! Exactness gate: compares native `ProtocolSim` quotes with the same swap executed on chain.
//!
//! For sampled blocks of a live Tycho stream, every chosen pool is quoted over an amount ladder in
//! both directions and the identical swap is executed through TychoRouter (zero router fee) on the
//! node's state of that block, in the block environment the decoder projected the state to. Any
//! wei of difference, or a disagreement between a native error and an on-chain revert, is
//! recorded.

mod ladder;
mod report;
mod stream;

use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::U256,
    providers::Provider,
    rpc::types::{Block, BlockOverrides},
};
use clap::Parser;
use futures::StreamExt;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use rand::seq::SliceRandom;
use tracing::{info, warn};
use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim};
use tycho_simulation::{protocol::models::ProtocolComponent, utils::load_all_tokens};
use tycho_test::{
    execution::{
        encoding::encode_swap,
        models::{RouterOverwritesData, TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction,
    },
    RPCTools,
};

use crate::report::{Outcome, Point, Report};

const FEE_CALCULATOR_FIXTURE: &str =
    include_str!("../../testing/fixtures/FeeCalculator.runtime.json");

#[derive(Parser, Debug)]
#[command(about = "Compare native protocol quotes with on-chain execution, to the wei")]
struct Cli {
    #[arg(long, default_value = "base")]
    chain: String,
    /// Tycho server; defaults to the beta server of the chain.
    #[arg(long)]
    tycho_url: Option<String>,
    #[arg(long, env = "TYCHO_AUTH_KEY", hide_env_values = true)]
    tycho_api_key: String,
    /// Node the swaps execute on. Needs `debug_traceCall` and the state of the streamed blocks.
    #[arg(long, env = "RPC_URL")]
    rpc_url: String,
    /// Protocol systems to check; defaults to the chain's native protocols.
    #[arg(long, value_delimiter = ',')]
    protocols: Option<Vec<String>>,
    /// Minimum component TVL in the chain's native token.
    #[arg(long, default_value_t = 20.0)]
    tvl_threshold: f64,
    /// Check every n-th streamed block.
    #[arg(long, default_value_t = 10)]
    every_n_blocks: u64,
    /// Pools per protocol per checked block, preferring pools that changed in that block.
    #[arg(long, default_value_t = 3)]
    pools_per_protocol: usize,
    /// Seconds between blocks, used to project the execution block like the decoder does.
    #[arg(long, default_value_t = 2)]
    block_time_secs: u64,
    /// Execute in the checked block's own environment instead of the projected next block.
    #[arg(long)]
    no_block_override: bool,
    /// Stop after this many checked blocks.
    #[arg(long)]
    max_checks: Option<u64>,
    #[arg(long, default_value = "native-gate-out")]
    out_dir: PathBuf,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "native_gate=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let chain = Chain::from_str(&cli.chain).map_err(|e| miette!("Unknown chain: {e}"))?;
    let tycho_url = match &cli.tycho_url {
        Some(url) => url.clone(),
        None => tycho_simulation::utils::get_default_url(&chain)
            .ok_or_else(|| miette!("No default Tycho URL for {chain}, pass --tycho-url"))?,
    };
    let protocols = cli
        .protocols
        .clone()
        .unwrap_or_else(|| stream::default_protocols(chain));

    let tokens = load_all_tokens(
        &tycho_url,
        false,
        Some(cli.tycho_api_key.as_str()),
        true,
        chain,
        None,
        None,
    )
    .await
    .map_err(|e| miette!("Failed to load tokens: {e:?}"))?;
    info!(tokens = tokens.len(), "Loaded tokens");

    let rpc_tools = RPCTools::new(&cli.rpc_url, &chain).await?;
    let router_overwrites = zero_fee_router_overwrites()?;
    let mut report = Report::create(&cli.out_dir)?;

    let mut updates = stream::build(
        &tycho_url,
        &cli.tycho_api_key,
        chain,
        &protocols,
        cli.tvl_threshold,
        &tokens,
    )
    .await?;

    let mut components: HashMap<String, ProtocolComponent> = HashMap::new();
    let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
    let mut checks = 0u64;
    let mut first = true;
    while let Some(update) = updates.next().await {
        let update = match update {
            Ok(update) => update,
            Err(e) => {
                warn!("Stream error: {e:?}");
                continue;
            }
        };
        components.extend(update.new_pairs);
        for id in update.removed_pairs.keys() {
            components.remove(id);
            states.remove(id);
        }
        let changed: Vec<String> = update.states.keys().cloned().collect();
        states.extend(update.states);
        let block_number = update.block_number_or_timestamp;
        // The first update is the snapshot of every component, not the pools that traded.
        if std::mem::take(&mut first) || block_number % cli.every_n_blocks != 0 {
            continue;
        }

        let block = fetch_block(&rpc_tools, block_number).await?;
        let pools = choose_pools(&components, &changed, &protocols, cli.pools_per_protocol);
        let points = quote_ladders(&pools, &components, &states);
        let overrides =
            (!cli.no_block_override).then(|| projected_block(&block, cli.block_time_secs));
        let outcomes =
            execute(&rpc_tools, chain, &points, &block, router_overwrites.clone(), overrides).await;
        report.record(&block, points, outcomes)?;
        report.write_summary()?;
        checks += 1;
        if cli
            .max_checks
            .is_some_and(|max| checks >= max)
        {
            break;
        }
    }
    report.write_summary()?;
    info!(out = %cli.out_dir.display(), "Done");
    Ok(())
}

/// Plants a fee calculator that charges nothing, so the router's output is the pool's output.
fn zero_fee_router_overwrites() -> miette::Result<RouterOverwritesData> {
    let fixture: serde_json::Value =
        serde_json::from_str(FEE_CALCULATOR_FIXTURE).into_diagnostic()?;
    let code = fixture["runtimeBytecode"]
        .as_str()
        .ok_or_else(|| miette!("FeeCalculator fixture has no runtimeBytecode"))?;
    let bytecode = alloy::hex::decode(code).into_diagnostic()?;
    Ok(RouterOverwritesData { fee_calculator_bytecode: Some(bytecode), ..Default::default() })
}

/// Waits until the node has the streamed block, so state and quote refer to the same block.
async fn fetch_block(rpc_tools: &RPCTools, number: u64) -> miette::Result<Block> {
    for _ in 0..60 {
        let block = rpc_tools
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to fetch block {number}"))?;
        if let Some(block) = block {
            return Ok(block);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(miette!("Node did not reach block {number} within 60s"))
}

/// The block the decoder projects confirmed state to: next number, one block time later.
fn projected_block(block: &Block, block_time_secs: u64) -> BlockOverrides {
    BlockOverrides {
        number: Some(U256::from(block.header.number + 1)),
        time: Some(block.header.timestamp + block_time_secs),
        ..Default::default()
    }
}

/// Picks up to `per_protocol` pools of each protocol, those that changed in this block first.
fn choose_pools(
    components: &HashMap<String, ProtocolComponent>,
    changed: &[String],
    protocols: &[String],
    per_protocol: usize,
) -> Vec<String> {
    let mut rng = rand::thread_rng();
    let mut chosen = Vec::new();
    for protocol in protocols {
        let of_protocol = |id: &&String| {
            components
                .get(*id)
                .is_some_and(|c| &c.protocol_system == protocol)
        };
        let mut active: Vec<&String> = changed
            .iter()
            .filter(of_protocol)
            .collect();
        active.shuffle(&mut rng);
        let mut picked: Vec<String> = active
            .into_iter()
            .take(per_protocol)
            .cloned()
            .collect();
        if picked.len() < per_protocol {
            let mut idle: Vec<&String> = components
                .keys()
                .filter(of_protocol)
                .filter(|id| !picked.contains(id))
                .collect();
            idle.shuffle(&mut rng);
            picked.extend(
                idle.into_iter()
                    .take(per_protocol - picked.len())
                    .cloned(),
            );
        }
        chosen.extend(picked);
    }
    chosen
}

/// Quotes every ladder amount natively, in every direction of every chosen pool.
fn quote_ladders(
    pools: &[String],
    components: &HashMap<String, ProtocolComponent>,
    states: &HashMap<String, Box<dyn ProtocolSim>>,
) -> Vec<Point> {
    let mut points = Vec::new();
    for id in pools {
        let (Some(component), Some(state)) = (components.get(id), states.get(id)) else {
            continue;
        };
        for sell in &component.tokens {
            for buy in &component.tokens {
                if sell.address == buy.address {
                    continue;
                }
                let limit = state
                    .get_limits(sell.address.clone(), buy.address.clone())
                    .ok()
                    .map(|(max_in, _)| max_in);
                for (rung, amount) in ladder::amounts(sell, limit.as_ref()) {
                    let native = state.get_amount_out(amount.clone(), sell, buy);
                    points.push(Point::new(
                        component,
                        state.as_ref(),
                        sell,
                        buy,
                        rung,
                        amount,
                        native,
                    ));
                }
            }
        }
    }
    points
}

/// Executes every point's swap on chain and pairs each with its outcome.
async fn execute(
    rpc_tools: &RPCTools,
    chain: Chain,
    points: &[Point],
    block: &Block,
    router_overwrites: RouterOverwritesData,
    overrides: Option<BlockOverrides>,
) -> Vec<Outcome> {
    let mut inputs = HashMap::new();
    let mut outcomes: Vec<Outcome> = Vec::with_capacity(points.len());
    for (index, point) in points.iter().enumerate() {
        // A native error or zero quote still gets executed, with no minimum output, to see if the
        // chain agrees. The router rejects a zero expected output, hence the floor of one.
        let expected = point
            .native_amount_out()
            .cloned()
            .unwrap_or_default()
            .max(BigUint::from(1u8));
        match encode_swap(
            &point.component,
            Some(Arc::from(point.state.clone_box())),
            &point.sell,
            &point.buy,
            point.amount_in.clone(),
            chain,
            None,
            point.gas(),
            expected.clone(),
        ) {
            Ok((solution, transaction)) => {
                inputs.insert(
                    index.to_string(),
                    TychoExecutionInput {
                        solution,
                        estimated_gas: transaction.estimated_gas().clone(),
                        transaction,
                        expected_amount_out: expected,
                        protocol_system: point.component.protocol_system.clone(),
                        component_id: point.component.id.to_string(),
                        token_in: point.sell.address.to_string(),
                        token_out: point.buy.address.to_string(),
                    },
                );
                outcomes.push(Outcome::Pending);
            }
            Err(e) => outcomes.push(Outcome::EncodeFailed(format!("{e:?}"))),
        }
    }
    if inputs.is_empty() {
        return outcomes;
    }
    match simulate_swap_transaction(rpc_tools, inputs, block, router_overwrites, None, overrides)
        .await
    {
        Ok(results) => {
            for (id, result) in results {
                let Ok(index) = id.parse::<usize>() else { continue };
                outcomes[index] = match result {
                    TychoExecutionResult::Success { amount_out, gas_used, .. } => {
                        Outcome::Executed { amount_out, gas_used }
                    }
                    TychoExecutionResult::Revert { reason, .. } => Outcome::Reverted(reason),
                    TychoExecutionResult::Failed { error_msg } => Outcome::SimFailed(error_msg),
                };
            }
        }
        Err((e, _, _)) => {
            warn!("Simulation batch failed: {e:?}");
            for outcome in outcomes.iter_mut() {
                if matches!(outcome, Outcome::Pending) {
                    *outcome = Outcome::SimFailed(format!("{e:?}"));
                }
            }
        }
    }
    outcomes
}
