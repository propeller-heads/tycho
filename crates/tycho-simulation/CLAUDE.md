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
- **`snapshot_feed/`**: latest-value snapshot feeds, both ends. `SnapshotFeed` is the contract —
  `run(publisher)` consumes the source and publishes through a `Publisher<Self::Snapshot>` until
  it ends with `Result<(), Self::Error>`. A feed cannot make a `Publisher`: the constructor is
  crate-private and its only callers are the three consumer types, each of which creates the
  watch channel, marks its `None` seed seen, and spawns the feed in its own task — so reading a
  feed is the only way to run one, the channel coalesces for a slow consumer instead of
  back-pressuring the venue, and no consumer can hold a feed's future and give it their own pace.
  `publishing(max_age, snapshots)` is the publisher's whole public surface, so a feed hands over
  its snapshots and the age at which one goes stale and nothing else is its to get right:
  `Publisher`'s `Drop` withdraws whatever it leaves behind however it ends, and a feed nobody
  reads is stopped by dropping its task rather than by noticing. The consumer types all spawn
  the feed — hence `spawn`, not `new` — and abort it when dropped: `SnapshotFeedStream` yields
  `Published` / `Withdrawn` and, as its last item, `Ended(SnapshotFeedOutcome)` — `RanOut`,
  `Failed(E)` or `Panicked(JoinError)`, the same three the watch reports. `Ended` is an event
  rather than just the stream ending because the keyed set needs it per feed: its own end comes
  only once every feed has ended. `SnapshotFeedStreams` keys any number of those by label, is
  itself a `Stream` of `(label, event)`, and hands a feed back from `add` when the label is
  taken; and
  `SnapshotFeedWatch` hands out `watch::Receiver` clones for a consumer that prices on demand,
  with `ended()` for the `SnapshotFeedOutcome` a bare `None` in the channel cannot express —
  reported once, then pending forever, so it can sit in a `select!` arm. The trait's
  associated types carry the bounds every consumer needs (`Snapshot: Send + Sync + 'static`,
  `Error: Send + 'static`), so a feed that compiles can be run. The trait and the streams are generic in both the snapshot and the error; behind the
  `book-feeds` feature come the feeds this crate ships, with `snapshot_feed::errors::FeedError` as their
  error and the transport loops that build one from a provider's source:
  `snapshot_feed::ws::run_ws_feed` drives a `WsSource` (`request` + `decode`),
  `snapshot_feed::http::run_http_poll_feed` an `HttpSource` (`fetch`), each with its tuning type beside it
  (`WsFeedConfig`, `HttpFeedConfig`; no `Default` — spread from the builder's `default_feed_config()`).
  The loops know nothing of a venue at all: the `snapshot_feed{provider}` span their events are
  recorded in is raised by `SnapshotFeedStream::spawn(provider, feed)` from the name the consumer
  reads the feed under, so a venue is spelled once. They publish what the source yields, count
  failures and back off (`failures.rs`), and withdraw a snapshot that goes `max_snapshot_age`
  without a refresh (`publisher.rs`, which also withdraws whatever a feed leaves behind when it
  ends)
- **`book/`**: the shared layer for off-chain market-maker venues, whose pricing arrives as a
  complete book rather than as chain state. A feed publishes `BookSnapshot<A>` (a provider's whole
  set of `Book`s at one anchor — `ReceivedAt` for every feed here; a block-anchored feed would pick
  another `A`), and a `Book` is one pair's simulate-ready component + state, plus the provider's
  `updated_at` where it reports one. A provider's `*Feed` implements `SnapshotFeed` directly, with
  a `run` that calls one of the two loops and nothing else. `BookFeedConfig` (chain, tokens,
  minimum book TVL in USD) is what every book feed needs whatever transport it runs on, and is the
  first argument of every feed builder; `BookFeedStreams`/`BookFeedEvent` are `SnapshotFeedStreams`/`SnapshotFeedEvent`
  with the book types filled in
  - **`book::{levels,sim,component,tvl}`**: what level-based venues share — `Levels`, the
    validated ladder every venue decodes its wire format into at ingestion (`fill`, `invert`,
    `notional`, `average_price`), the direction/scaling/limit pieces every such `ProtocolSim`
    impl needs, the pair component id and builder, and pricing a book's notional in USD through
    the other pairs the same response carried
  - Per provider: `feed.rs` (the `*Feed` and its builder), `client.rs` (everything that talks to
    the venue — its endpoints, its credentials, one pooled `reqwest::Client`, and every request:
    the book poll or WebSocket handshake the feed needs and, where the venue signs quotes, the
    binding-quote request the states make), `source.rs` (what to make of the answers — decoding,
    orientation, TVL normalization, building components and states), `state.rs`, `models.rs` and
    a module-level `PROTOCOL_SYSTEM` constant. No source holds a credential or builds a request:
    it asks its client. Feeds that price book TVL off their own levels also take the USD
    quote-token set (`book::quote_tokens::usd_stablecoins_for_chain` is the curated default); how
    a venue's books are oriented and deduplicated is that provider's business, decided in its
    `source.rs`
- **`rfq/`**: the book feeds that need binding quotes at execution time (Bebop, Hashflow,
  Liquorice, Native) — their clients are shared via `Arc` by the emitted states, which request
  signed quotes through `IndicativelyPriced`; the crate-private `RFQError` covers the quoting
  layer and never reaches a public signature. Credentials are constructor arguments of the feed builders; the
  library never reads the environment
- **`pamm/`**: book feeds for pAMMs — venues priced off-chain but executed directly against
  the pool, no binding quote (Metric today; its state does not implement `IndicativelyPriced`,
  and its client is held by the feed alone rather than by every state).
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
  regardless of call order. By default `build` emits every venue under `fallback:{pamm}`, so
  tycho-execution routes their swaps through `TychoFallbackRouter` (retry on a solver-named
  fallback pool when the venue reverts); `without_fallback_router` keeps every venue on the
  direct `pricelevelstream:` path. Venues may overlap with other integration
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

## Pending-block state for hybrid/VM protocols

`apply_deltas_ephemeral` applies only `state_deltas`; nothing on the pending path writes to the
VM database, so `apply_deltas_ephemeral` can't read the pending state from there. A protocol
whose `delta_transition` re-reads the VM would therefore quote a pending block against confirmed
state. Fluid and Curve close that gap the same way:

1. A `TxDeltaIndexer` implementation — which lives in the consuming repo, not here — builds
   `evm::simulation::PendingOverrides` (storage, native balances and block environment) from the
   accounts a `PendingBlock` carries.
2. It reads the protocol's state under those overrides (`fluid::call_resolver`,
   `curve::read_pool_readings`) and puts the result in a state-delta attribute
   (`pool_reserves_adjusted`, `pool_state_adjusted`).
3. `delta_transition` branches on that attribute and rebuilds from it, falling back to the VM read
   when it is absent.

Reading under the pending block's own number and timestamp matters: anything with on-chain time
math (Fluid's expanding limits, Curve's ramping `A()`) is wrong under the parent block's clock.

## Features

| Feature | Default | Contents |
|---------|---------|----------|
| `evm` | yes | `revm`, `SimulationEngine`, all EVM protocol impls |
| `book-feeds` | yes | The off-chain book feeds: `snapshot_feed`'s transport loops, `book/`, `rfq/`, `pamm/` (implies `evm`). The `SnapshotFeed` trait is unconditional |
| `price-level-stream` | yes | Titan pAMM price level stream client |
| `network_tests` | no | Gates tests that require live network access |

## Conventions

- CI pins a nightly toolchain for both `fmt` and `clippy` (see `.github/workflows/ci-rust.yaml`);
  stable for builds and tests
- `rstest`: name each parametrised case with `#[case::descriptive_name(...)]`
- A `tracing` span whose fields identify what an event is about (`snapshot_feed{provider}`,
  `quote_request{...}`) is `error`-level: a span the subscriber's filter rejects is never entered,
  so its fields attach to nothing and a `warn!` inside it prints bare. Spans that only time work
  stay fieldless at `debug`
- Mark every test that hits external services `#[ignore = "Requires RPC_URL ..."]`. CI runs
  `--all-features`, so `#[cfg_attr(not(feature = "network_tests"), ignore)]` does not exclude the
  test and it fails without `RPC_URL`
