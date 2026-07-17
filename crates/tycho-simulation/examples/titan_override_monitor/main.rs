//! Manual monitor proving that Titan pAMM live overrides reprice pools *mid-block*.
//!
//! Subscribes to a Tycho stream for pAMM protocols (FermiSwap and bopAMM by default), shares the
//! production
//! Titan override provider between the pools and this monitor, and re-quotes every pool each time
//! a Titan frame arrives. Quotes that change between two Tycho block updates demonstrate that
//! [`EVMPoolState`](tycho_simulation::evm::protocol::vm::state::EVMPoolState) resolves live
//! overrides at simulation time rather than once per block.
//!
//! Run with:
//!
//! ```bash
//! TYCHO_API_KEY=... cargo run -p tycho-simulation --example titan_override_monitor -- \
//!     --tycho-url tycho-dev.propellerheads.xyz
//! ```
//!
//! See `Readme.md` for expected output.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use futures::StreamExt;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use tycho_common::{models::Chain, simulation::protocol_sim::ProtocolSim};
use tycho_simulation::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        override_stream::{titan, OverrideSnapshot, StateOverrideProvider},
        protocol::vm::state::EVMPoolState,
        stream::ProtocolStreamBuilder,
    },
    protocol::models::{DecoderContext, ProtocolComponent},
    tycho_client::feed::component_tracker::ComponentFilter,
    utils::load_all_tokens,
};

#[derive(Parser)]
struct Cli {
    /// Tycho server URL.
    #[arg(long, env = "TYCHO_URL")]
    tycho_url: String,

    /// Tycho API key.
    #[arg(long, env = "TYCHO_API_KEY", hide_env_values = true)]
    api_key: String,

    /// pAMM protocol systems to monitor.
    #[arg(long, value_delimiter = ',', default_value = "vm:fermiswap,vm:bopamm")]
    protocols: Vec<String>,

    /// TVL threshold (in native token units) for component tracking.
    #[arg(long, default_value_t = 1.0)]
    tvl_threshold: f64,

    /// Sell amount in whole units of each pool's first token (e.g. 0.01 WETH). Kept small so
    /// pools with expensive base tokens (WBTC) are not quoted beyond their inventory.
    #[arg(long, default_value_t = 0.01)]
    sell_units: f64,

    /// Print full EVM call traces for every simulation (decode-time and quotes). Extremely
    /// verbose — redirect to a file.
    #[arg(long, default_value_t = false)]
    vm_traces: bool,
}

/// A monitored pool: its simulation state plus static component metadata.
struct Pool {
    state: Box<dyn ProtocolSim>,
    component: ProtocolComponent,
}

/// The last quote printed for a pool: display string plus the numeric output (if it succeeded)
/// used to compute relative deltas.
struct LastQuote {
    display: String,
    value: Option<f64>,
}

/// Per-pool quote bookkeeping for the current Tycho block.
#[derive(Default)]
struct BlockStats {
    quotes: u32,
    distinct: HashSet<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();
    let cli = Cli::parse();
    let chain = Chain::Ethereum;

    println!("Loading tokens from {} ...", cli.tycho_url);
    let all_tokens =
        load_all_tokens(&cli.tycho_url, false, Some(&cli.api_key), true, chain, None, None)
            .await
            .expect("failed loading tokens");
    println!("Loaded {} tokens", all_tokens.len());

    // Obtain the production default Titan provider (one shared WS connection, including the
    // TITAN_PAMM_STREAM_URL env override), but keep a handle so this monitor can subscribe to the
    // same frames the pools consume. Registering it explicitly below prevents the builder from
    // spawning a second default provider.
    let providers = titan::default_providers(cli.protocols.iter().cloned());
    if providers.is_empty() {
        eprintln!(
            "None of {:?} is served by the Titan provider; nothing to monitor",
            cli.protocols
        );
        return;
    }

    // Forward every frame of every subscribed protocol into one channel for the select loop.
    // Values are read from the receiver at quote time; the channel only signals "new frame".
    let (frame_tx, mut frame_rx) = mpsc::channel::<String>(64);
    for (protocol, provider) in &providers {
        let mut rx = provider
            .subscribe(protocol)
            .expect("provider serves the protocol it was registered for");
        let protocol = protocol.clone();
        let frame_tx = frame_tx.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                if frame_tx
                    .send(protocol.clone())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    let tvl_filter = ComponentFilter::with_tvl_range(cli.tvl_threshold, cli.tvl_threshold);
    let mut builder = ProtocolStreamBuilder::new(&cli.tycho_url, chain);
    for protocol in &cli.protocols {
        builder = builder.exchange_with_decoder_context::<EVMPoolState<PreCachedDB>>(
            protocol,
            tvl_filter.clone(),
            None,
            DecoderContext::new().vm_traces(cli.vm_traces),
        );
    }
    for (protocol, provider) in &providers {
        builder = builder.with_override_provider(protocol, provider.clone());
    }
    let stream = builder
        .auth_key(Some(cli.api_key.clone()))
        .skip_state_decode_failures(true)
        .set_tokens(all_tokens)
        .await
        .build()
        .await
        .expect("failed building the protocol stream");
    tokio::pin!(stream);
    println!("Waiting for the first Tycho block update (snapshot decoding may take a while) ...\n");

    let mut pools: HashMap<String, Pool> = HashMap::new();
    let mut last_printed: HashMap<String, LastQuote> = HashMap::new();
    let mut block_stats: HashMap<String, BlockStats> = HashMap::new();
    let mut current_block: Option<u64> = None;
    let mut last_probe = Instant::now() - Duration::from_secs(1);

    loop {
        tokio::select! {
            update = stream.next() => {
                let Some(update) = update else {
                    println!("Tycho stream closed, exiting");
                    return;
                };
                let update = match update {
                    Ok(update) => update,
                    Err(e) => {
                        eprintln!("stream error: {e}");
                        continue;
                    }
                };
                print_block_recap(current_block, &pools, &block_stats);
                block_stats.clear();
                current_block = Some(update.block_number_or_timestamp);

                for (id, component) in update.new_pairs {
                    // The stream also carries synthetic components (e.g. the native ETH/WETH
                    // wrapper); only monitor the pAMM protocols that were asked for.
                    if !cli.protocols.contains(&component.protocol_system) {
                        continue;
                    }
                    let Some(state) = update.states.get(&id) else { continue };
                    pools.insert(id.clone(), Pool { state: state.clone(), component });
                }
                for (id, state) in update.states {
                    if let Some(pool) = pools.get_mut(&id) {
                        pool.state = state;
                    }
                }
                for (id, _) in update.removed_pairs {
                    pools.remove(&id);
                }

                println!(
                    "\n═══ tycho block {} ({} pool(s)) ═══",
                    update.block_number_or_timestamp,
                    pools.len()
                );
                quote_all(&pools, &providers, cli.sell_units, &mut last_printed, &mut block_stats, true);
            }
            Some(_protocol) = frame_rx.recv() => {
                // Titan pushes several frames per second; a handful of probes per second is
                // enough to show mid-block movement without hammering the simulator.
                if last_probe.elapsed() < Duration::from_millis(250) || pools.is_empty() {
                    continue;
                }
                last_probe = Instant::now();
                quote_all(&pools, &providers, cli.sell_units, &mut last_printed, &mut block_stats, false);
            }
            _ = tokio::signal::ctrl_c() => {
                print_block_recap(current_block, &pools, &block_stats);
                println!("\nbye");
                return;
            }
        }
    }
}

/// Quotes `sell_units` of `token0` on every pool and prints each quote that differs from the last
/// one printed for that pool. With `force`, prints unconditionally (block-boundary baseline).
fn quote_all(
    pools: &HashMap<String, Pool>,
    providers: &HashMap<String, std::sync::Arc<dyn StateOverrideProvider>>,
    sell_units: f64,
    last_printed: &mut HashMap<String, LastQuote>,
    block_stats: &mut HashMap<String, BlockStats>,
    force: bool,
) {
    for (id, pool) in pools {
        let [token_in, token_out, ..] = pool.component.tokens.as_slice() else { continue };
        let amount_in = BigUint::from((10f64.powi(token_in.decimals as i32) * sell_units) as u128);
        let result = pool
            .state
            .get_amount_out(amount_in, token_in, token_out);
        // `raw` keys the distinct-quote counter on the exact integer output, so movement below
        // the display precision is still counted as a reprice.
        let (display, value, raw) = match &result {
            Ok(res) => {
                let human =
                    res.amount.to_f64().unwrap_or(f64::NAN) / 10f64.powi(token_out.decimals as i32);
                (
                    format!(
                        "{sell_units} {} = {} {}",
                        token_in.symbol,
                        format_amount(human),
                        token_out.symbol
                    ),
                    Some(human),
                    res.amount.to_string(),
                )
            }
            Err(e) => {
                let display = format!("{}->{} revert: {e}", token_in.symbol, token_out.symbol);
                (display.clone(), None, display)
            }
        };

        let stats = block_stats
            .entry(id.clone())
            .or_default();
        stats.quotes += 1;
        stats.distinct.insert(raw);

        if force ||
            last_printed
                .get(id)
                .is_none_or(|last| last.display != display)
        {
            let snapshot = providers
                .get(&pool.component.protocol_system)
                .and_then(|provider| provider.subscribe(&pool.component.protocol_system))
                .map(|rx| rx.borrow().clone())
                .unwrap_or_default();
            println!(
                "{}  {:>14} {} | {}{}",
                wall_clock(),
                pool.component.protocol_system,
                describe_snapshot(&snapshot),
                display,
                delta_suffix(
                    last_printed
                        .get(id)
                        .and_then(|last| last.value),
                    value
                ),
            );
            last_printed.insert(id.clone(), LastQuote { display, value });
        }
    }
}

/// One line per pool: how many quotes were computed in the finished block and how many distinct
/// values they produced. More than one distinct value proves mid-block repricing.
fn print_block_recap(
    block: Option<u64>,
    pools: &HashMap<String, Pool>,
    stats: &HashMap<String, BlockStats>,
) {
    let Some(block) = block else { return };
    for (id, stat) in stats {
        let Some(pool) = pools.get(id) else { continue };
        let [token_in, token_out, ..] = pool.component.tokens.as_slice() else { continue };
        println!(
            "    block {block} recap: {}->{} {} quote(s), {} distinct{}",
            token_in.symbol,
            token_out.symbol,
            stat.quotes,
            stat.distinct.len(),
            if stat.distinct.len() > 1 { "  <-- repriced mid-block" } else { "" },
        );
    }
}

/// The live snapshot's block context, e.g. `titan_block=25445671` or `no override`.
fn describe_snapshot(snapshot: &OverrideSnapshot) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    match snapshot.block_number {
        Some(block) if snapshot.is_expired(now) => format!("titan_block={block} (expired)"),
        Some(block) => format!("titan_block={block}"),
        None => "no override".to_string(),
    }
}

/// Formats an output amount with enough precision that small quotes (e.g. USDC->WETH) still show
/// movement instead of rounding to a constant.
fn format_amount(value: f64) -> String {
    if value == 0.0 || value.abs() >= 0.001 {
        format!("{value:.6}")
    } else {
        format!("{value:.10}")
    }
}

/// Relative change vs the previously printed quote, e.g. ` (Δ -0.0021%)`.
fn delta_suffix(previous: Option<f64>, current: Option<f64>) -> String {
    let (Some(previous), Some(current)) = (previous, current) else {
        return String::new();
    };
    if previous == 0.0 {
        return String::new();
    }
    format!(" (Δ {:+.4}%)", (current - previous) / previous * 100.0)
}

/// Current wall-clock time as `HH:MM:SS.mmm` (UTC).
fn wall_clock() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock");
    let secs = now.as_secs();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60,
        now.subsec_millis()
    )
}
