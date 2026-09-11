//! Opt-in network benchmark; see benchmarks/token-metadata/README.md.

use std::{
    collections::HashMap,
    env,
    fs::OpenOptions,
    io::Write,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use alloy::{primitives::U256, rpc::types::BlockId};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing_subscriber::{layer::Context, prelude::*, Layer, Registry};
use tycho_common::models::token::TokenOwnerStore;

use super::*;
use crate::{erc20::balanceOfCall, rpc::config::RPCBatchingConfig};

#[derive(Deserialize)]
struct Fixture {
    settlement_contract: Address,
    tokens: Vec<Holder>,
}

#[derive(Deserialize)]
struct Holder {
    address: Address,
    owner: Address,
    symbol: String,
    decimals: u8,
}

#[derive(Clone, Default)]
struct Observations {
    warnings: Arc<AtomicUsize>,
    retry_backoffs: Arc<AtomicUsize>,
}

impl Layer<Registry> for Observations {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // Tests also exercise these callsites without a subscriber on other threads.
        // Resolve enablement against the current dispatcher when collecting observations.
        tracing::subscriber::Interest::sometimes()
    }

    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, Registry>) {
        let metadata = event.metadata();
        if metadata
            .target()
            .starts_with("tycho_ethereum") &&
            *metadata.level() <= tracing::Level::WARN
        {
            self.warnings
                .fetch_add(1, Ordering::Relaxed);
        }
        if metadata.target() == "tycho_ethereum::rpc::retry" &&
            metadata
                .fields()
                .field("attempts_left")
                .is_some()
        {
            self.retry_backoffs
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Observations {
    fn take(&self) -> (usize, usize) {
        (
            self.warnings.swap(0, Ordering::Relaxed),
            self.retry_backoffs
                .swap(0, Ordering::Relaxed),
        )
    }
}

fn record(output: &mut impl Write, value: Value) {
    serde_json::to_writer(&mut *output, &value).unwrap();
    writeln!(output).unwrap();
    // Preserve completed observations even if a later request fails or the run is interrupted.
    output.flush().unwrap();
}

// Sequential call order and output normalization from 2dc825c14. Keep
// this independent of get_token: awaiting get_token sequentially would still run its
// three calls concurrently and would not measure the original implementation.
async fn sequential(
    processor: &EthereumTokenPreProcessor,
    addresses: &[Bytes],
    detector: &EthCallDetector,
    block: BlockTag,
) -> (Vec<Token>, Vec<Value>) {
    let mut tokens = Vec::with_capacity(addresses.len());
    let mut calls = Vec::with_capacity(addresses.len());
    for address in addresses {
        let start = Instant::now();
        let symbol = processor
            .call_symbol(Address::from_bytes(address))
            .await;
        let symbol_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let decimals = processor
            .call_decimals(Address::from_bytes(address))
            .await;
        let decimals_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let analysis = detector
            .analyze(address.clone(), block)
            .await;
        let analysis_ms = start.elapsed().as_secs_f64() * 1000.0;
        let analysis_ok = analysis.is_ok();
        let (token_quality, gas, tax) =
            analysis.unwrap_or_else(|_| (TokenQuality::bad("Detection failed"), None, None));
        let mut quality = if token_quality.is_good() { 100 } else { 10 };
        if quality == 100 && tax.is_some_and(|tax_value| tax_value > 0) {
            quality = 50;
        }
        tokens.push(Token {
            metadata_status: Default::default(),
            address: address.clone(),
            symbol: symbol
                .replace('\0', "")
                .graphemes(true)
                .take(255)
                .collect(),
            decimals: decimals.into(),
            tax: tax.unwrap_or(0),
            gas: gas
                .map(|g| vec![Some(g)])
                .unwrap_or_default(),
            chain: processor.chain,
            quality,
        });
        calls.push(json!({
            "address": address, "symbol_ms": symbol_ms, "decimals_ms": decimals_ms,
            "analysis_ms": analysis_ms, "analysis_ok": analysis_ok,
        }));
    }
    (tokens, calls)
}

/// Real RPC calls, not a CI timing assertion or a production load test.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires explicit RPC access; see benchmarks/token-metadata/README.md"]
async fn benchmark_robinhood_token_metadata() {
    let rpc_url = env::var("ROBINHOOD_RPC_URL").expect("Set ROBINHOOD_RPC_URL explicitly");
    let report_path = env::var("TOKEN_BENCH_OUTPUT").expect("Set TOKEN_BENCH_OUTPUT");
    let rounds: usize = env::var("TOKEN_BENCH_ROUNDS")
        .unwrap_or_else(|_| "10".into())
        .parse()
        .expect("TOKEN_BENCH_ROUNDS must be an integer");
    assert!((1..=100).contains(&rounds), "Use 1–100 rounds; this is not a load test");
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../benchmarks/token-metadata/robinhood.json"))
            .unwrap();
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report_path)
        .expect("Report must be a new file; existing measurements are never overwritten");
    let rpc = EthereumRpcClient::new(&rpc_url).unwrap();
    let identity_rpc = alloy::rpc::client::ClientBuilder::default().http(rpc_url.parse().unwrap());
    let chain_id: U256 = identity_rpc
        .request_noparams("eth_chainId")
        .await
        .unwrap();
    assert_eq!(chain_id, U256::from(4663), "Expected Robinhood mainnet");
    let observations = Observations::default();
    // All benchmark futures run on this thread. Observe existing tracing events without
    // logging inside timed regions or changing the production RPC client.
    let subscriber = tracing_subscriber::registry().with(observations.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let processor =
        EthereumTokenPreProcessor::new(&rpc, Chain::Robinhood, fixture.settlement_contract);
    let concurrent_processor = EthereumTokenPreProcessor::new(
        &rpc.clone()
            .with_batching(RPCBatchingConfig::Disabled),
        Chain::Robinhood,
        fixture.settlement_contract,
    );
    let block = rpc
        .eth_get_block_by_number(BlockId::Number(BlockNumberOrTag::Latest))
        .await
        .expect("Cannot read the benchmark block");
    let number = block.header.number;
    let block_tag = BlockTag::Number(number);
    let retry = rpc.get_retry_config();
    record(
        &mut output,
        json!({
            "kind": "run", "started_at": chrono::Utc::now().to_rfc3339(),
            "chain": "robinhood", "block": number, "block_hash": block.header.hash,
            "block_timestamp": block.header.timestamp, "rounds": rounds,
            "settlement_contract": fixture.settlement_contract,
            "max_concurrent_tokens": MAX_CONCURRENT_TOKENS,
            "max_retries": retry.max_retries, "initial_backoff_ms": retry.initial_backoff_ms,
            "max_backoff_ms": retry.max_backoff_ms,
            "metadata_block": "latest", "analysis_block": number,
            "debug_assertions": cfg!(debug_assertions),
            "variants": ["sequential", "concurrent", "batched"],
            "metadata_batch_size": rpc.get_batching_config().max_batch_size(),
            "workload": "selected real pool tokens; NOT observed production cache misses",
            "timing": "client-side elapsed milliseconds, including retries",
        }),
    );

    let mut owners = HashMap::new();
    for holder in &fixture.tokens {
        let result = rpc
            .eth_call(
                call_request(
                    None,
                    holder.address,
                    balanceOfCall { _owner: holder.owner }.abi_encode(),
                ),
                BlockNumberOrTag::Number(number),
            )
            .await
            .expect("Cannot fetch the real holder balance");
        let balance = balanceOfCall::abi_decode_returns_validate(&result).unwrap();
        // Without a funded owner, analyze returns before making its simulation RPC.
        assert!(balance >= U256::from(100_000), "Fixture holder has insufficient balance");
        owners.insert(holder.address.to_bytes(), (holder.owner.to_bytes(), balance.to_bytes()));
        record(
            &mut output,
            json!({
                "kind": "holder", "address": holder.address, "owner": holder.owner,
                "balance": balance.to_string(), "block": number,
            }),
        );
    }
    let finder = Arc::new(TokenOwnerStore::new(owners));
    let detector = EthCallDetector::new(&rpc, finder.clone(), fixture.settlement_contract);
    let addresses: Vec<_> = fixture
        .tokens
        .iter()
        .map(|holder| holder.address.to_bytes())
        .collect();

    // Validate metadata without the production fallbacks, then require a successful
    // simulation with gas measurements. Fast failures must not look like an improvement.
    for holder in &fixture.tokens {
        let symbol = rpc
            .eth_call(
                call_request(None, holder.address, symbolCall {}.abi_encode()),
                BlockNumberOrTag::Latest,
            )
            .await
            .expect("Symbol RPC failed during preflight");
        assert_eq!(symbolCall::abi_decode_returns_validate(&symbol).unwrap(), holder.symbol);
        let decimals = rpc
            .eth_call(
                call_request(None, holder.address, decimalsCall {}.abi_encode()),
                BlockNumberOrTag::Latest,
            )
            .await
            .expect("Decimals RPC failed during preflight");
        assert_eq!(decimalsCall::abi_decode_returns_validate(&decimals).unwrap(), holder.decimals);
    }
    let (reference, calls) = sequential(&processor, &addresses, &detector, block_tag).await;
    record(&mut output, json!({"kind": "preflight", "tokens": reference, "calls": calls}));
    for (token, holder) in reference.iter().zip(&fixture.tokens) {
        assert_eq!(token.symbol, holder.symbol);
        assert_eq!(token.decimals, u32::from(holder.decimals));
        assert_eq!(
            token.quality, 100,
            "Fixture must complete the transfer simulation successfully"
        );
        assert!(!token.gas.is_empty(), "Analysis must not skip the simulation RPC");
    }
    for processor in [&processor, &concurrent_processor] {
        let warmup = processor
            .get_tokens(addresses.clone(), finder.clone(), block_tag)
            .await;
        assert_eq!(
            serde_json::to_value(&warmup).unwrap(),
            serde_json::to_value(&reference).unwrap()
        );
    }
    assert_eq!(observations.take().0, 0, "Preflight/warmup emitted warnings");

    let mut invalid = 0;
    for round in 0..rounds {
        for count in 1..=addresses.len() {
            // Rotate which implementation goes first to reduce connection/cache-order bias.
            let mut variants = ["sequential", "concurrent", "batched"];
            variants.rotate_left((round + count) % 3);
            for variant in variants {
                // Keep this small experiment gentle on shared, rate-limited public endpoints.
                tokio::time::sleep(Duration::from_secs(2)).await;
                let input = addresses[..count].to_vec();
                let start = Instant::now();
                let result = tokio::time::timeout(Duration::from_secs(120), async {
                    if variant == "sequential" {
                        sequential(&processor, &input, &detector, block_tag).await
                    } else {
                        let processor =
                            if variant == "batched" { &processor } else { &concurrent_processor };
                        (
                            processor
                                .get_tokens(input, finder.clone(), block_tag)
                                .await,
                            Vec::new(),
                        )
                    }
                })
                .await;
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                let (warnings, retry_backoffs) = observations.take();
                let Ok((tokens, calls)) = result else {
                    record(
                        &mut output,
                        json!({"kind": "sample", "round": round,
                        "variant": variant, "count": count, "elapsed_ms": elapsed_ms,
                        "valid": false, "timed_out": true}),
                    );
                    panic!("Benchmark watchdog expired; partial observations were preserved");
                };
                // Token::PartialEq compares only addresses; compare every serialized field.
                let valid = warnings == 0 &&
                    serde_json::to_value(&tokens).unwrap() ==
                        serde_json::to_value(&reference[..count]).unwrap();
                invalid += usize::from(!valid);
                record(
                    &mut output,
                    json!({"kind": "sample", "round": round,
                    "variant": variant, "count": count, "elapsed_ms": elapsed_ms,
                    "valid": valid, "warnings": warnings, "retry_backoffs": retry_backoffs,
                    "tokens": tokens, "calls": calls}),
                );
                eprintln!(
                    "round {round}, {count} tokens, {variant}: {elapsed_ms:.1} ms, valid={valid}"
                );
            }
        }
    }
    let end_block = rpc
        .eth_get_block_by_number(BlockId::Number(BlockNumberOrTag::Number(number)))
        .await
        .expect("Cannot verify the benchmark block after the run");
    let canonical = end_block.header.hash == block.header.hash;
    record(
        &mut output,
        json!({"kind": "complete", "finished_at": chrono::Utc::now().to_rfc3339(),
        "invalid_samples": invalid, "block_hash_unchanged": canonical}),
    );
    assert!(canonical, "Benchmark block changed; do not compare results across a reorg");
    assert_eq!(invalid, 0, "Invalid outputs were recorded; do not report them as speedups");
}

#[tokio::test(flavor = "current_thread")]
async fn test_benchmark_detects_decimals_fallback_matching_expected_value() {
    let mut server = mockito::Server::new_async().await;
    let response = server
        .mock("POST", "/")
        .with_header("content-type", "application/json")
        .with_body(r#"{"jsonrpc":"2.0","id":0,"result":"0x"}"#)
        .expect(1)
        .create_async()
        .await;
    let observations = Observations::default();
    let subscriber = tracing_subscriber::registry().with(observations.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Robinhood, Address::ZERO);
    // A value-only check would accept this as WETH's real decimals, despite invalid ABI.
    assert_eq!(
        processor
            .call_decimals(Address::repeat_byte(1))
            .await,
        18
    );
    assert_eq!(observations.take(), (1, 0));
    assert_eq!(observations.take(), (0, 0));
    response.assert_async().await;
}
