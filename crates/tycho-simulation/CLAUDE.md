# tycho-simulation

Off-chain DeFi protocol simulation library. Computes swap outputs, spot prices, and price impact
for any protocol indexed by Tycho.

## Key Modules (`src/`)

- **`protocol/`**: Consumer-facing models — `ProtocolComponent`, `Update`, and the crate's error
  types. The `ProtocolSim` trait itself lives in `tycho-common` (`simulation/protocol_sim.rs`)
- **`evm/simulation.rs`**: `SimulationEngine` — runs EVM transactions via `revm`
- **`evm/engine_db/`**: Database backends (`SimulationDB` in-memory, `TychoDB` RPC-backed)
- **`evm/decoder.rs`**: `TychoStreamDecoder` — turns feed snapshots into `ProtocolSim` instances.
  **A new protocol must be registered here** to be decodable
- **`evm/stream.rs`**: Tycho feed integration — wires the decoder onto a live `FeedMessage` stream
- **`evm/pending.rs`**: `TxDeltaIndexer` implementation — replays in-flight blocks to produce
  pending-state deltas
- **`evm/protocol/filters.rs`**: Public pool filters consumers pass when registering protocols
- **`evm/override_stream/`**: live per-block VM state overrides for pAMMs — generic
  `StateOverrideProvider`/`OverrideSnapshot` core plus the Titan quote-stream provider; pools
  resolve the latest snapshot on every simulation and can fall back to indexed state per the
  snapshot's `FailurePolicy`
- **`evm/protocol/`**: Protocol implementations
  - **Native** (`uniswap_v2/`, `uniswap_v3/`, `uniswap_v4/`, `ekubo/`, `ekubo_v3/`, `cowamm/`,
    `aerodrome_v1/`, `aerodrome_slipstreams/`, `velodrome_slipstreams/`, `pancakeswap_v2/`,
    `ramses_v3/`, `ring_swap_v2/`, `lunarbase/`, `native_wrapper/`, `sky/`, `etherfi/`,
    `erc4626/`, `rocketpool/`): Pure Rust math, no EVM execution.
    `cpmm.rs` / `clmm.rs` / `safe_math.rs` / `u256_num.rs` / `utils.rs` are shared math helpers,
    not protocols
  - **Hybrid** (`fluid/`, `balancer_v3/`, `curve/`): native Rust quote math over VM-indexed pool
    state (each has both `state.rs` and `vm.rs`)
  - **VM** (`vm/`): Generic Solidity adapter (`TychoSimulationContract`) executed in `revm` for
    protocols without a native implementation
- **`snapshot_feed.rs`**: the generic `SnapshotFeed` trait — a latest-value feed: `subscribe()` consumes
  the source and returns a `watch::Receiver<Self::Snapshot>` plus the driver future
  (`impl Future<Output = Self::Output> + Send + 'static`) the caller spawns; both associated types are
  implementor-chosen (an `Option` snapshot for feeds with a warm-up phase, a `Result` output
  for feeds that can give up)
- **`book/`**: the shared book-feed layer for off-chain market-maker venues — `Book` (one pair's
  simulate-ready component + state, with the provider's `updated_at` where reported) and
  `BookSnapshot<A>` (a provider's complete set of books at one anchor — `ReceivedAt`, the feed's
  receipt time, for every feed here; a block-anchored feed picks another `A`), the WS/HTTP
  feed loops (`run_ws_feed`, `run_http_poll_feed`) with failure counting and backoff, which
  drive a provider's crate-private `HttpBookSource` (`fetch_books`) or `WsBookSource`
  (`request` + `decode`) by reference — each loop runs in a `book_feed{provider}` tracing span
  with a `poll` / `ws_connection` / `ws_frame` span per unit of work, so source code logs
  without naming its venue; variables go in fields, retried conditions are `warn`, per-item
  skips `debug` — every `<Provider>Feed` is `{ feed_config, source }`
  with the `<Provider>BookSource` (the provider integration proper: fetching/decoding, TVL
  normalization, pair building) in the provider's private `source.rs`,
  `Ws/HttpFeedConfig` (no `Default` — each builder's `default_feed_config()` is the base to spread
  from, so venue-specific values are never dropped by accident), `FeedError`, and `CommonConfig` (chain, tokens, minimum book TVL in USD —
  taken by value as the first argument of every feed builder and stored whole; the builders'
  own setters cover provider-specific options only). Feeds that normalize book TVL from price
  levels (Bebop, Hashflow, Liquorice, Native) additionally take the USD quote-token set
  (`usd_stablecoins_for_chain` is the curated default). Two-sided providers (Bebop, Native)
  publish a pair in either orientation; `book::dedup` groups entries by unordered pair and fixes
  one orientation (the USD quote token on the quote side, else address order) — Bebop drops the
  mirror, Native merges both sides into one book. The rest of a provider's shared shape lives in
  crate-private helpers: `book::levels` (`Levels`, the validated ladder every venue decodes its
  wire levels into at ingestion — finite positive quantities and prices, zero-quantity
  placeholders dropped, an invalid level rejects the whole response — with `fill`, `invert`,
  `notional`, `average_price` as methods), `book::sim` (`SwapDirection`, atomic/human scaling,
  `fill_result`, `limits` — the pieces every level-based `ProtocolSim` impl needs),
  `book::component` (pair ids — unordered for two-sided venues, directed for one-directional
  ones, always prefixed with the venue because consumers key states by id across protocols — and
  `pair_component`), `book::http::fetch_json`, `CommonConfig::{pair_tokens, clears_min_tvl}`. Every provider ships a
  `<Provider>Feed` type plus its `<Provider>FeedBuilder` in the same `feed.rs`, and a
  module-level `PROTOCOL_SYSTEM` constant. Feeds implement `SnapshotFeed` with
  `Snapshot = Option<BookSnapshot<ReceivedAt>>` (`None` before the first book and after the loop
  withdrew one nobody was refreshing) and `Output = Result<(), FeedError>`; `subscribe()` is the whole
  streaming surface (consumers compose the per-provider receivers themselves, e.g.
  `StreamMap<String, WatchStream<Option<BookSnapshot<ReceivedAt>>>>` plus a `JoinSet` over the feed futures)
- **`rfq/`**: the book feeds that need binding quotes at execution time (Bebop, Hashflow,
  Liquorice, Native) — per-protocol clients (shared via `Arc` by the emitted states) request signed
  quotes through `IndicativelyPriced`; `RFQError` covers the quoting layer. Credentials are constructor arguments of the feed builders; the
  library never reads the environment
- **`pamm/`**: book feeds for pAMMs — venues priced off-chain but executed directly against
  the pool, no binding quote (Metric today; its state does not implement `IndicativelyPriced`).
  The line to `rfq/` is the counterparty: a maker who can decline a specific trade after pricing
  it makes an RFQ, a pool that fills any taker with fresh price data makes a pAMM
- **`price_level_stream/`**: Titan pAMM price level stream — `PriceLevelStreamBuilder` turns the
  Titan WebSocket's per-pair quote-ladder snapshots directly into `Update`s (no indexer feed
  round-trip); `PriceLevelStreamState` quotes by interpolating the ladder. Components are
  identified as `pricelevelstream:{pamm}`. A new builder serves nothing: `with_known_pamms`
  registers the known-good venues and denies known-unexecutable ones, `add_pamm` registers
  individual ones, `deny_pamm` excludes one (dropping any registration and blocking
  auto-detection), and opt-in auto-detection additionally serves unknown venues under their
  address (`pricelevelstream:{0xaddress}`). Precedence: between `add_pamm` and `deny_pamm` for
  the same address the later call wins; `with_known_pamms` defaults never override either,
  regardless of call order. `build` emits venues on Titan's PropAMMRouter whitelist under
  `propammfallback:{pamm}` instead, so tycho-execution routes their swaps through the router
  (Uniswap V3 fallback on venue revert); it reads that whitelist once on the first poll via
  `RPC_URL`, and warns and stays on the direct path without it. `without_fallback_router` skips
  the read and keeps every venue on the direct path. Venues may overlap with other integration
  paths of the same liquidity (e.g. `vm:fermiswap`) — consumers must deduplicate by venue where
  double-counting matters

## Simulation Approaches

**Always prefer native.** If a protocol's behaviour can be ported to Rust, it should be. VM is a
fallback for protocols too complex to port, not a default.

1. **Native** — pure Rust math; fastest. Use whenever the protocol logic can be expressed in Rust.
2. **Hybrid** — native Rust math for swap calculation, but reads/updates pool state via the local
   VM (`SimulationDB`). Use when the swap logic can be ported but state is complex to track
   independently. Examples: Fluid V1, Balancer V3, Curve. Note that a hybrid protocol keeps its
   VM-shaped indexing — component keys stay `vm:*` and the indexer still tracks full contract
   storage; only the quote path changes.
3. **VM** — Solidity adapter in `revm`; works for any EVM protocol but is slower and requires an
   adapter contract in `protocols/adapter-integration/`. Use only when native is not feasible.
4. **RFQ** — off-chain quotes via API; for protocols that cannot be simulated on-chain at all.

## Features

| Feature | Default | Contents |
|---------|---------|----------|
| `evm` | yes | `revm`, `SimulationEngine`, all EVM protocol impls |
| `rfq` | yes | RFQ WebSocket client and protocol adapters |
| `price-level-stream` | yes | Titan pAMM price level stream client |
| `network_tests` | no | Gates tests that require live network access |

## Conventions

- CI pins a nightly toolchain for both `fmt` and `clippy` (see `.github/workflows/ci-rust.yaml`);
  stable for builds and tests
- `rstest`: name each parametrised case with `#[case::descriptive_name(...)]`
- Mark every test that hits external services `#[ignore = "Requires RPC_URL ..."]`. CI runs
  `--all-features`, so `#[cfg_attr(not(feature = "network_tests"), ignore)]` does not exclude the
  test and it fails without `RPC_URL`
