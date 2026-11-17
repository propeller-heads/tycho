//! Runs the simulation token analyzer over a list of tokens and reports latency.
//!
//! Input is a JSON-lines file of `{token, pair, block, label, chain}`: the token, the pool to
//! trade against, the block to analyse at, and the chain, `ethereum` when left out. Each analysis
//! is written to `<out_dir>/<token>@<label>.json` with its latency; existing files are skipped so
//! an interrupted run can be resumed. The pool's token balance is read before the timed analysis,
//! as the indexer has it from its own state.
//!
//! To run:
//! `RPC_URL=<archive rpc> cargo run --release --example analyze-tokens-bench -- \
//!     <requests.jsonl> <out_dir> [concurrency]`

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use alloy::{
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{BlockNumberOrTag, TransactionInput, TransactionRequest},
    sol_types::SolCall,
};
use futures::{stream, StreamExt};
use serde::Deserialize;
use tycho_common::{
    models::{blockchain::BlockTag, token::TokenOwnerStore, Chain},
    traits::TokenAnalyzer,
};
use tycho_ethereum::{
    erc20::balanceOfCall, rpc::EthereumRpcClient, services::token_analyzer::SimulationDetector,
    BytesCodec,
};

const DEFAULT_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Deserialize)]
struct Request {
    token: Address,
    pair: Address,
    block: u64,
    label: String,
    /// Decides the wrapped native token that real swaps are made against.
    #[serde(default)]
    chain: Chain,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        return Err("usage: analyze-tokens-bench <requests.jsonl> <out_dir> [concurrency]".into());
    };
    let concurrency = match args.next() {
        Some(n) => n
            .parse()
            .map_err(|e| format!("concurrency {n}: {e}"))?,
        None => DEFAULT_CONCURRENCY,
    };
    let rpc_url = std::env::var("RPC_URL").map_err(|e| format!("RPC_URL: {e}"))?;
    let rpc = EthereumRpcClient::new(&rpc_url).map_err(|e| format!("RPC client: {e}"))?;
    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse()
            .map_err(|e| format!("RPC URL: {e}"))?,
    );
    let out_dir = PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;

    let requests = read_requests(Path::new(&input))?;
    let results: Vec<Result<Option<f64>, String>> = stream::iter(requests)
        .map(|request| {
            let (rpc, out_dir) = (rpc.clone(), out_dir.clone());
            let provider = &provider;
            async move { analyse_one(&rpc, provider, &request, &out_dir).await }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut latencies = Vec::new();
    let mut failures = 0;
    for result in results {
        match result {
            Ok(Some(ms)) => latencies.push(ms),
            Ok(None) => {}
            Err(error) => {
                failures += 1;
                eprintln!("FAILED {error}");
            }
        }
    }
    report(&mut latencies);
    if failures > 0 {
        return Err(format!("{failures} analyses failed"));
    }
    Ok(())
}

/// Analyses one token; returns its latency in milliseconds, or `None` when it was done before.
async fn analyse_one(
    rpc: &EthereumRpcClient,
    provider: &impl Provider,
    request: &Request,
    out_dir: &Path,
) -> Result<Option<f64>, String> {
    let path = out_dir.join(format!("{:#x}@{}.json", request.token, request.label));
    if path.exists() {
        return Ok(None);
    }
    let balance = pool_balance(provider, request).await?;
    let finder = TokenOwnerStore::new(HashMap::from([(
        request.token.to_bytes(),
        (request.pair.to_bytes(), balance.to_bytes()),
    )]));
    let detector = SimulationDetector::new(rpc, Arc::new(finder), request.chain);

    let start = Instant::now();
    let analysis = detector
        .analyze(request.token.to_bytes(), BlockTag::Number(request.block))
        .await
        .map_err(|e| format!("{:#x}@{}: {e}", request.token, request.label))?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;

    let record = serde_json::json!({ "latency_ms": ms, "analysis": analysis });
    let json = serde_json::to_string_pretty(&record).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;
    eprintln!("{:#x}@{} {:?} in {ms:.0} ms", request.token, request.label, analysis.verdict);
    Ok(Some(ms))
}

async fn pool_balance(provider: &impl Provider, request: &Request) -> Result<U256, String> {
    let calldata = balanceOfCall { _owner: request.pair }.abi_encode();
    let tx = TransactionRequest::default()
        .to(request.token)
        .input(TransactionInput::new(calldata.into()));
    let out = provider
        .call(tx)
        .block(BlockNumberOrTag::Number(request.block).into())
        .await
        // alloy's transport errors quote the request URL, which carries the provider's API key.
        .map_err(|e| format!("balance of pool {:#x}: {}", request.pair, redact(&e.to_string())))?;
    Ok(out
        .get(..32)
        .map(U256::from_be_slice)
        .unwrap_or_default())
}

/// `message` with any URL path and query replaced by `***`.
fn redact(message: &str) -> String {
    let mut out = String::new();
    for word in message.split_inclusive(' ') {
        match word.find("://") {
            Some(scheme) => {
                let rest = &word[scheme + 3..];
                let host_end = rest.find('/').unwrap_or(rest.len());
                out.push_str(&word[..scheme + 3 + host_end]);
                out.push_str("/***");
                if word.ends_with(' ') {
                    out.push(' ');
                }
            }
            None => out.push_str(word),
        }
    }
    out
}

fn report(latencies: &mut [f64]) {
    if latencies.is_empty() {
        eprintln!("no new analyses");
        return;
    }
    latencies.sort_by(f64::total_cmp);
    let at = |percent: usize| latencies[(latencies.len() - 1) * percent / 100];
    eprintln!(
        "{} analyses: p50 {:.0} ms, p90 {:.0} ms, max {:.0} ms",
        latencies.len(),
        at(50),
        at(90),
        at(100)
    );
}

fn read_requests(path: &Path) -> Result<Vec<Request>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))
        })
        .collect()
}
