# tycho-integration-test

Long-running binary (not a `cargo test` suite) that connects to a live Tycho instance, syncs
protocol state via `tycho-client`, and continuously validates simulation accuracy against on-chain
results. Exits after `--max-blocks` blocks, or runs indefinitely.

## Running

Env vars (can be set in `.claude/settings.local.json`):

| Variable | Purpose |
|---|---|
| `TYCHO_URL` | WebSocket endpoint of the Tycho server (required) |
| `RPC_URL` | Ethereum-compatible JSON-RPC endpoint for on-chain validation (required); with `--pamm baibai`, BaiBai's feed also polls it every second, with `--pamm efficient-prop` the listing doors' feed every 250 ms, and with `--pamm flamm` the Efficient pools feed at every block |
| `TYCHO_API_KEY` | Auth key; defaults to `sampletoken`, which works against local dev instances |
| `TYCHO_NO_TLS` | Set to `true` to talk plain HTTP/WS (same as `--no-tls`) |
| `EFFICIENT_PROP_FRONT`, `EFFICIENT_PROP_FACTORY`, `EFFICIENT_PROP_IMPLEMENTATIONS` | The Efficient PropAMM front, its listing-door factory and the accepted front implementations; required with `--pamm efficient-prop` (see README.md) |
| `EFFICIENT_PROP_QUOTE_LAYOUT` | The quote contract's layout, `builder` only; required with `--pamm efficient-prop` on Ethereum, refused on Base, which reads no quote contract (see README.md) |
| `EFFICIENT_PROP_CELL_CODEHASH` | The desk contracts' creation code hash; required with `--pamm efficient-prop` on Ethereum, refused on Base (see README.md) |
| `EFFICIENT_PROP_STATE_URLS` | Comma-separated URLs of the next block's state map; required with `--pamm efficient-prop` on Ethereum, refused on Base |
| `EFFICIENT_PROP_APPROVED_DESKS`, `EFFICIENT_PROP_APPROVED_DESKS_URL` | The makers' desk contracts whose listings are served (comma-separated), and a URL whose list replaces them while fresh; with neither, nothing is served (see README.md) |
| `EFFICIENT_PROP_PRICE_SOURCES` | Comma-separated ids of the front's price sources a listing reading a price print may be bound to and be served; unset, only listings reading no print are served. A source's code runs inside the quote and again inside the fill and can tell the two apart, by what the call reads of its transaction and block and by state the fill has already changed when the source runs (the front's lock, the input already at the door), so approve a source only where its code cannot change and its answer comes from its own stored prices and the clock alone; a guarded source needs approval as any other. The same holds of a listing's two tokens, which are not screened: name only tokens in the book configuration whose code cannot change and moves exactly the amounts asked |

Both the token loader and the protocol stream default to TLS (`https`/`wss`). Pass `--no-tls`
(or set `TYCHO_NO_TLS=true`) when pointing at a local dev instance served over plain HTTP.

```bash
cargo run -p tycho-integration-test -- \
  --chain ethereum \
  --tycho-url 127.0.0.1:4242 \
  --no-tls \
  --tvl-threshold 100
```

Key optional flags: `--no-tls`, `--disable-onchain`, `--disable-rfq`, `--disable-pamm`,
`--pamm metric,baibai,efficient-prop,flamm` (`baibai` on Base only, `efficient-prop` on Ethereum (refused on Base for now),
`flamm` on Base, Ethereum and BSC), `--rfq-all-pairs`, `--disable-price-level-stream`, `--disable-execution`,
`--protocols uniswap_v2,curve`,
`--protocol-tvl-thresholds lunarbase=10` (replaces `--tvl-threshold` for the named protocols;
each one must be enabled),
`--max-blocks 100`, `--parallel-simulations 5`, `--always-test-components <id,...>`,
`--price-level-stream-block-interval 1`, `--price-level-stream-stale-threshold-secs 10`,
`--test-every-n-updates 10`, `--bypass-executor-timelock`.

`--bypass-executor-timelock` writes `executorsActivationTimestamp = 1` for every executor of the
chain into the execution simulation's state overrides, so an executor that is unapproved or still
inside its activation timelock (`Dispatcher.DELAY_EXECUTOR_ACTIVATION`, 1 day) does not make
`Dispatcher._validateExecutor` revert. The
router keeps its deployed bytecode, and only the read-only simulation call is affected. It cannot
help an executor that has no bytecode deployed. Off by default so that a missing activation still
shows up as a failure.

## Module Structure

- **`main.rs`**: CLI (`Cli` struct), top-level orchestration loop — subscribes to Tycho,
  dispatches blocks to stream processors, tests every Nth protocol update
  (`--test-every-n-updates`, alias `--test-every-n-blocks`), calls `poll_rpc_for_block`
  (every-update mode, and sampled mode under `--partial-blocks`) or `await_target_block`
  (sampled mode without `--partial-blocks`, fetches the sampled block by number) for on-chain
  comparison
- **`stream_processor/`**:
  - `protocol_stream_processor.rs`: Handles on-chain protocol updates — applies deltas to
    `ProtocolSim` instances, runs `get_amount_out` simulations, validates via RPC execution.
    Swaps are executed on the update block's state, in that block's context. The protocols in
    `NEXT_BLOCK_PROTOCOLS` (`main.rs`) are the exception: the decoder prices them for the next
    block through `ProtocolSim::apply_block` (number + 1, timestamp + `block_time_secs`), and
    their quotes move with time, so a full block's swaps execute in that next block's context
    (`simulate_swap_transaction_next_block`). Add a protocol there when its state implements
    `apply_block`
  - `book_stream_processor.rs`: Handles the off-chain book feeds in one processor. RFQ and pAMM
    are selected and checked separately: `RfqSelection` (each RFQ venue whose credentials are
    set, off with `--disable-rfq`) and `PammSelection` (`--pamm`, `PammVenue`, off with
    `--disable-pamm`). It fetches live quotes and compares them against simulation. When
    neither side adds a feed, no book stream starts and a warning names the reason for each
  - `price_level_stream_processor.rs`: Handles Titan pAMM price level stream updates (Ethereum
    only) — emits one sampled update per `--price-level-stream-block-interval` blocks, holding a
    chosen block's latest snapshot back until the stream moves to the next block (least drift to
    the finalized block), samples pair states, validates `get_limits` / `get_amount_out`. Marks
    the served venues stale in metrics when no Titan message arrives within
    `--price-level-stream-stale-threshold-secs`. The stream keeps its simulation overrides on, so
    each state carries the account state Titan priced its quote on. Every swap of a state takes
    those accounts as its own `TychoExecutionInput::account_overwrites` (balances, nonces and
    storage), and is simulated before the quoted block, as a consumer tests a quote: on the
    quoted block's parent, with that block's number and timestamp
    (`simulate_swap_transaction_before_block`). The stream runs with
    `without_fallback_router()`, so venues are served under `pricelevelstream:*` and execute
    directly on the venue, resolving through the single `pricelevelstream` entry in
    `executor_addresses.json` — not under the stream's default `fallback:*`, whose
    `TychoFallbackRouter` needs `user_data` naming a fallback pool the test cannot supply. A swap
    whose state carries no overrides is counted per protocol by
    `tycho_integration_price_level_simulation_overrides_missing_total`; when the venue's freshness
    guard then reverts (`StaleUpdate`, or `FeedStalled` for Metric) the swap counts as a stale
    quote (`tycho_integration_execution_stale_quotes_total`) instead of a revert. With overrides,
    the same revert counts as a revert: the overrides did not work. The stream's own
    `price_level_stream_*` series are served on the same metrics port
- **`statistics.rs`**: `TestStatistics` + `ProtocolStatistics` — per-protocol counters for
  simulation success/failure, execution reverts, slippage, `get_limits` / `get_amount_out` calls
- **`metrics.rs`**: Prometheus metrics (served on `--metrics-port`, default 9898)
- **`fee_fetcher.rs`**: One-shot start-up read of the router fee on output from the deployed
  FeeCalculator, so the test backs the current fee out of the simulated amount instead of
  hard-coding it
- **`packages.rs`**: `StrictPackages`, the strict-mode packages of the chain, read at start-up from
  the `protocols/substreams/**/integration_test*.tycho.yaml` files `build.rs` embeds: every one
  but those under a `target` directory or a `-template-` package. The build
  fails when it finds none (`.dockerignore` lets those yamls into the image build), and the binary
  fails at start-up when any of them is invalid, whatever its chain
- **`limit_probes.rs`**: limit probes for strict protocols (see "Strict packages" below)
- **`grafana/integration-test.json`**: the Integration test dashboard (uid `ceztze8otfk00c`,
  folder `tycho`), including the "Strict checks" row. Import it by hand after editing
- **`grafana/pamm-simulation-quality.json`**: Grafana dashboard (VictoriaMetrics datasource) for
  the price level swaps this service checks and the stream's own `price_level_stream_*` series.
  Its `job` variable also lists Fynd's jobs, which fill the stream rows. Import it by hand

## What it validates

For each tested update (every `--test-every-n-updates`-th, never the first), for the first
`--max-simulations` updated states, the `--always-test-components`, and up to
`--max-simulations-stale` random stale components of each ready protocol, in every direction
between the component's tokens (a component with a swap-directions attribute: at most
`--max-simulations` of its listed directions, sampled):
1. `ProtocolSim::get_limits`: a failure is counted and the direction is skipped
2. `ProtocolSim::get_amount_out` at the max input, capped at 5 ETH worth of the token (a token
   missing from the price snapshot is not capped by value) and at 2^96 - 1, and raised to
   Hashflow's minimum quote size for Hashflow: a failure is counted; a zero amount in or out skips
   the direction
3. Encodes a swap via `tycho-execution` and simulates it via RPC (`debug_traceCall` with balance
   and allowance overrides): a revert or failure is counted by error category, and a success
   records the slippage against the fee-deducted quote, in percent, logging an error outside
   ±0.2%. There is no pass or fail for this swap; the limit probes below have one

## Strict packages

A protocol whose package sets `harness: strict` (protocols/testing/README.md, "Strict mode")
gets limit probes on top of the swap above; every other protocol gets only that swap.
For each direction the swap above tests, the probe swaps one size relative to the limit, in
turn: 0.1%, 1%, 10%, then the limit (`ProbeSize::ALL`: the ladder's rungs, `ProbeSize::LADDER`,
then the limit), and one unit past a hard or under-cap limit, in the same batch
(`checks::limits`). A soft limit's claim, that twice the limit quotes, is checked in simulation.

| Check | Passes when |
|---|---|
| `sim.ladder` | a fraction quotes |
| `exec.ladder` | a fraction executes for its quote, less the router fee, within the tolerance the component declares, else the package's (exact without either) |
| `limits.at_limit` | the limit quotes, and executes as the fractions must |
| `limits.past_limit` | past the limit, the venue behaves as the declared kind claims: one unit past a hard limit the quote errors and, once the limit executed, the swap reverts (fails `past_limit_executed` otherwise); one unit past an under-cap limit the quote and the swap succeed (fails `past_limit_reverted` otherwise); twice a soft limit quotes |
| `limits.max_output` | the quote at the limit pays no more than `get_limits`' max output |

The limit comes from the component's `limits` when a test lists it, or else the package's. The
probes size from `get_limits` as `range` does, capped at `int128::MAX` (`executable_max_input`);
past a capped limit nothing is probed (`execution_cap`).

Each turn plans the checks of its size (`sim.ladder` and `exec.ladder` for a fraction, the three
`limits.*` for the limit) and records each exactly once (`limit_probes::Turn`). A check that
cannot run is `not_run`, with a reason from `limit_probes::NotRun`; a failure has a reason from
`limit_probes::Fail`; their docs say what each means. A revert the router raises itself, before
the swap reaches the venue (`EnforcedPause()` while it is paused), is `not_run` with `router`, not
the venue's revert. A check still unsettled when the turn's
swaps are dropped is recorded as not run: `batch_failed` when its batch failed or did not run
because an earlier batch of the update failed (the probes of the batches that ran are judged),
`execution_disabled` under `--disable-execution`, and `unrecorded` otherwise, which is a bug in
the probes and is logged with the component and the checks. Outcomes go to
`tycho_integration_check_total{protocol, check, outcome, reason, size}` and executed drift to
`tycho_integration_probe_drift_bps{protocol, size}`, never to the realistic swap's metrics; a
failure is also logged with `event_type="check_failure"` and the component.
