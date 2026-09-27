# Live token-metadata benchmark

This opt-in benchmark sends real, read-only RPC requests through Tycho's token
preprocessor and `EthCallDetector`. It does not use a mock RPC server and does not
submit transactions. It is an ignored Rust test so it can reuse private methods
without adding a production API just for benchmarking.

## Run

From the repository root:

```sh
ROBINHOOD_RPC_URL=https://rpc.mainnet.chain.robinhood.com \
TOKEN_BENCH_ROUNDS=10 \
TOKEN_BENCH_OUTPUT=/tmp/robinhood-token-metadata.jsonl \
cargo test --locked -p tycho-ethereum --lib benchmark_robinhood_token_metadata \
  -- --ignored --nocapture --test-threads=1

python3 crates/tycho-ethereum/benchmarks/token-metadata/summarize.py \
  /tmp/robinhood-token-metadata.jsonl
```

The benchmark compares `sequential` (original), `concurrent` (no batching) and
`batched` (current production path). Ten rounds produce 60 measured samples.
The output must not already exist. Each observation is flushed as it completes,
so interrupted or failed runs leave their partial evidence intact. An incomplete
run is not a successful benchmark. Use a fresh filename for every run.

The [documented public Robinhood endpoint](https://docs.robinhood.com/chain/connecting/)
is rate-limited and not intended for production. The benchmark pauses two seconds
between samples and defaults to ten rounds. Do not turn it into a public-endpoint
stress test. A provider endpoint can be supplied through the same environment
variable. Record which endpoint and machine were used when sharing results, but
never commit API keys. The report deliberately does not save the RPC URL.

## Workload and checks

- `robinhood.json` uses WETH and USDG held by the RobinSwap pool already recorded
  in `protocols/substreams/ethereum-uniswap-v3-logs-only/integration_test_robinhood_robinswap_v3.tycho.yaml`.
  The settlement address is the Robinhood TychoRouter from
  `docs/for-solvers/execution/contract-addresses.md`; it is an explicit benchmark
  choice, not an assertion about production's extractor configuration.
- The benchmark verifies chain ID 4663 and reads the pool's actual token balances
  at a recorded block. It rejects an unfunded holder: otherwise analysis could
  return immediately without exercising its simulation RPC.
- Symbol/decimals must decode to the fixture's expected values. Analysis must
  produce quality 100 and gas measurements. All variants are warmed before
  measurement. All returned fields are checked against the preflight result,
  including ordering, gas, tax and quality, not just token addresses.
- Warnings invalidate a sample, even when a metadata fallback happens to equal
  the expected value (for example WETH's default 18 decimals). Existing retry
  backoff events are counted without printing logs in the timed region.
- Each round compares one token (WETH) and two tokens (WETH + USDG). The order of
  variant execution rotates. These counts are **selected
  workloads**, not measured `new_tokens_count` values from production.
- The sequential baseline reproduces the original call order and output
  normalization from `2dc825c14`. Awaiting the new `get_token` in a loop would
  still overlap its three calls and would be an incorrect baseline.
- Both concurrent variants call the actual `get_tokens` implementation. For
  `concurrent`, batching is explicitly disabled on a cloned client; `batched`
  uses the default batch configuration. This separates the benefit of concurrency
  from the benefit of batching. No timeout or quality policy is changed by the benchmark.
- Analysis uses the recorded block throughout; metadata still uses `latest`,
  matching production. The block hash is checked again at the end. This detects
  a changed block but does not make it finalized.
- A 120-second benchmark watchdog records a timed-out sample and stops the run.
  It is only a harness safety limit, not a fix for production stalls.

## Interpreting the numbers

Raw JSONL contains total `get_tokens` timings and sequential symbol, decimals and
analysis timings. These are client-side elapsed durations, including decoding,
retries and backoff where applicable—not isolated network round-trip latency.
Balances, tokens, block hash, timestamps, retry configuration and individual
sample outcomes are retained. Failed samples are not silently removed.

The default command uses the existing unoptimized test build; the report records
`debug_assertions`. It measures network-dominated wall time, not production CPU
throughput. Use a release build for deployment-profile measurements, accounting
for its additional disk/build requirements. All variants always share the same
build, shared HTTP connection pool, holder fixture and block.

With ten observations per variant/workload, nearest-rank p95 equals the maximum.
This is a small live experiment, not a reliable estimate of production tail
latency. Repeated calls can benefit from provider caching and connection reuse.
The two-token fixture does not exercise four-token saturation, unusually large
blocks, rate-limiting incidents or the reported production stall window.

Production's unknown tokens per block cannot be recovered from public RPC alone:
they depend on each extractor's protocol coverage and token-cache contents.
`construct_currency_tokens` already records `new_tokens_count`; obtaining its
actual distribution requires production traces or an equivalent recorded replay
with the same cache state. This benchmark does not fabricate that missing data
and cannot, by itself, establish that issue #1317 is fully resolved.
