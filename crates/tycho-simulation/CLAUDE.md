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
    not protocols. Uniswap V4 hook handlers register per `(Chain, hook address)` in
    `uniswap_v4/hooks/hook_handler_creator.rs`; the generic VM fallback covers only Ethereum and
    Unichain, and Pons V2 is the native handler for Robinhood. The Uniswap v3 and v4 states and
    the v3 forks (`ramses_v3/`, `aerodrome_slipstreams/`, `velodrome_slipstreams/`) cache the swap
    steps they already took (`utils/uniswap/swap_step_cache.rs`), shared by their clones: code
    that changes a state's price, tick, liquidity, fee or ticks must start an empty step cache,
    and changes ticks only through `Arc::make_mut`
  - **Hybrid** (`fluid/`, `balancer_v3/`, `curve/`): native Rust quote math over VM-indexed pool
    state (each has both `state.rs` and `vm.rs`)
  - **VM** (`vm/`): Generic Solidity adapter (`TychoSimulationContract`) executed in `revm` for
    protocols without a native implementation
- **`snapshot_feed/`**: latest-value snapshot feeds, both ends, generic in the snapshot and the
  error.
  - `SnapshotFeed` is the contract: `run(publisher)` consumes the source and publishes through a
    `Publisher<Self::Snapshot>` until it ends with `Result<(), Self::Error>`. Its associated
    types carry the bounds every consumer needs (`Snapshot: Send + Sync + 'static`, `Error:
    Send + 'static`)
  - `Publisher` has one public method, `publishing(max_age, snapshots)`, and a crate-private
    constructor only the three consumer types call, so reading a feed is the only way to run
    one. Its `Drop` withdraws whatever a feed leaves behind, and `publisher.rs` withdraws a
    snapshot that goes `max_snapshot_age` without a refresh
  - the consumer types spawn the feed — hence `spawn`, not `new` — and abort it when dropped.
    `SnapshotFeedStream` yields `Published` / `Withdrawn` and, last, `Ended(SnapshotFeedOutcome)`
    (`RanOut`, `Failed(E)`, `Panicked(String)`); `SnapshotFeedStreams` keys any number of those
    by label, is itself a `Stream` of `(label, event)`, and hands a feed back from `add` when the
    label is taken; `SnapshotFeedWatch` hands out `watch::Receiver` clones for a consumer that
    prices on demand, with `ended()` for the outcome a bare `None` cannot express — reported
    once, then pending forever, so it sits in a `select!` arm. `Ended` is an event because the
    keyed set needs it per feed
  - behind the `book-feeds` feature, the transport loops that build a feed from a provider's
    source, with `snapshot_feed::errors::FeedError` as their error: `ws::run_ws_feed` drives a
    `WsSource` (`request` + `decode`), `http::run_http_poll_feed` an `HttpSource` (`fetch`),
    each with its tuning type beside it (`WsFeedConfig`, `HttpFeedConfig`; no `Default` — spread
    from the builder's `default_feed_config()`). They publish what the source yields, count
    failures and back off (`failures.rs`), and name no venue: the `snapshot_feed{provider}` span
    comes from `SnapshotFeedStream::spawn(provider, feed)`, so a venue is spelled once
- **`book/`**: the shared layer for off-chain market-maker venues, whose pricing arrives as a
  complete book rather than as chain state. A feed publishes `BookSnapshot<A>` (a provider's whole
  set of `Book`s at one anchor — `ReceivedAt` for every feed here; a block-anchored feed would pick
  another `A`), and a `Book` is one book's simulate-ready component + state, plus the provider's
  `updated_at` where it reports one: a pair for Bebop, Native and Metric, a pair and one market
  maker for Hashflow and Liquorice, whose firm quote comes from the maker it is asked of. A
  provider's `*Feed` implements `SnapshotFeed` directly, with a `run` that calls one of the two
  loops and nothing else. `BookFeedConfig` (chain, tokens, minimum book TVL in USD) is what every
  book feed needs whatever transport it runs on, and is the first argument of every feed builder;
  `BookFeedStreams`/`BookFeedEvent` are `SnapshotFeedStreams`/`SnapshotFeedEvent` with the book
  types filled in
  - **`book::{levels,sim,component,tvl,wire,all_pairs}`**: what level-based venues share —
    `Levels`, the validated ladder every venue decodes its wire format into at ingestion (`fill`,
    `invert`, `notional`, `average_price`), the direction/scaling/limit pieces every such
    `ProtocolSim` impl needs, the component ids and builders (per pair, and per maker and pair),
    pricing a book's notional in USD through the other pairs the same response carried, and `wire`'s
    `SkipInvalidEntries` and `EntryReader`, which drop a venue's bad entries and fail only when none
    survive. `PairState` is what a book's state answers with: its two tokens and the directions it
    has levels on, which a component carries as its `swap_directions` static attribute
    (`book::{SWAP_DIRECTIONS_ATTRIBUTE, decode_swap_directions}`) wherever its token list does not
    already read as them — one side of a book, or neither, and every all-pairs component.
    `all_pairs` is the opt-in layout (`all_pairs()` on the four RFQ builders, taking the `QuoteRule`
    on the two that name their makers) that publishes a venue as one component for every pair it
    prices: `AllPairs<P, T>` holds the per-pair states the other layout would publish, picks the
    best book still allowed to quote under the component's `QuoteRule` (`once_per_maker`,
    `once_per_venue`), and records it as spent in the state the swap returns. `T` is how the venue
    counts turns — `SignedByMakers` where it names the maker behind each book, `SignedByVenue`
    where it signs its own quotes and so has no rule to pick, which is why only the first pair of
    builders takes one. A venue's `<Venue>AllPairsState` is a newtype over `AllPairs<P, T>`,
    because `#[typetag::serde]` registers concrete types; the `all_pairs_state!` macro declares it
    from those two types, so the four differ only in them and in what a venue refuses before a
    book is picked (`RefusesQuote::check_amount`). A book reporting no direction is left out of
    the state, so its tokens and its directions agree
  - Per provider: `feed.rs` (the `*Feed` and its builder), `client.rs` (everything that talks to
    the venue — its endpoints, its credentials, one pooled `reqwest::Client`, and every request:
    the book poll or WebSocket handshake the feed needs and, where the venue signs quotes, the
    binding-quote request the states make), `source.rs` (what to make of the answers — decoding,
    orientation, TVL normalization, building components and states), `state.rs`, `models.rs` and
    a module-level `PROTOCOL_SYSTEM` constant. No source holds a credential or builds a request:
    it asks its client. A venue that names the chain in its requests lists the ones it serves as a
    `<Venue>SupportedChain` in its `models.rs`, which `build()` resolves the configured `Chain`
    into, failing `Fatal` when it is not one of them; a venue addressed by a chain id alone keeps
    no such list, and its client instead recognises that venue's own refusal in the body of a
    failing response (`snapshot_feed::http::RefusalCheck`, passed to the fetch), which makes it
    the `Fatal` that ends the feed rather than a failure worth retrying. Feeds that price book TVL
    off their own levels also take the USD quote-token set
    (`book::quote_tokens::usd_stablecoins_for_chain` is the curated default); how a venue's books
    are oriented and deduplicated is that provider's business, decided in its `source.rs`
- **`rfq/`**: the book feeds that need binding quotes at execution time (Bebop, Hashflow,
  Liquorice, Native) — their clients are shared via `Arc` by the emitted states, which request
  signed quotes through `IndicativelyPriced`; the crate-private `RFQError` covers the quoting layer
  and never reaches a public signature. Credentials are constructor arguments of the feed builders;
  the library never reads the environment. `with_fallback_router()` on the Bebop, Hashflow,
  Liquorice and Native builders (and Metric's, in `pamm/`) stamps components
  `fallback:rfq:{venue}` instead, so tycho-execution routes their swaps through that venue's
  fallback router; off by default. The Native and Liquorice routers are not deployed yet. The
  fallback routers trade ERC20s only, so Native's fallback feed offers no native-token books
- **`pamm/`**: book feeds for pAMMs — venues priced off-chain but executed directly against
  the pool, no binding quote (Metric today; its state does not implement `IndicativelyPriced`,
  and its client is held by the feed alone rather than by every state).
  The line to `rfq/` is the counterparty: a maker who can decline a specific trade after pricing
  it makes an RFQ, a pool that fills any taker with fresh price data makes a pAMM
- **`price_level_stream/`**: Titan pAMM price level stream — `PriceLevelStreamBuilder` turns the
  Titan WebSocket's book frames directly into `Update`s (no indexer feed
  round-trip); `PriceLevelStreamState` quotes by interpolating the ladder and refuses to quote
  once its frame is one slot old (`quotable_until`, monotonic, never serialized, ignored by
  `eq`; `without_quote_guard` on the builder turns it off for slow quoters). A frame carries one
  flat `books` list — one book per venue and pair, priced in human units as float
  `price`/`cumVolume` levels, with both assets named by address in `token1Address`/`token2Address`
  — so `convert.rs` derives the integer ladders the states serve: `resolve_book_tokens` looks the
  two addresses up and `ladders_for_book`
  turns the bids into the `token1 -> token2` ladder and the asks into the reverse, converting the
  sides independently so a rejected one never costs the other. `tracker.rs` groups the books per
  venue and feeds the freshness machinery below unchanged. Resolution is fail-closed: a pair
  prices only if both addresses are in the consumer's token map, which also supplies the decimals.
  The book's `token1`/`token2` symbols are display labels read for the DEBUG logs alone, never
  resolved — so a token calling itself `WBTC` neither captures another asset's quotes nor keeps
  it from being served. Frames are best
  effort, so `tracker.rs` never removes a component because a frame omits it. It gives every
  component its own deadline and emits `removed_pairs` only when the component's data is
  `stale_after` old; the next accepted frame carrying the component re-adds it. Frames are
  accepted only if their stream `ts` is younger than `stale_after`, not more than one slot
  in the future, not older than the newest accepted one (equal allowed), and their block neither
  regresses nor jumps more than one block per elapsed slot plus 2; the block frontier lives in
  `ServingState::Serving`, so it exists only while something is served. The frame's beacon `slot`
  carries no freshness decision, so a frame without it is judged like one carrying it. The windows
  and their
  defaults are listed under `# Freshness contract` in `price_level_stream/mod.rs`; every window
  is a multiple of `SLOT` in `mod.rs`; the `stale_after` setter caps its value at
  `MAX_STALE_AFTER` (one hour). `build()` is an `async_stream` loop that selects over frames and
  a timer set to the earliest component deadline, and the only place that reads a clock: the
  tracker gets both clocks injected through `Now`. `titan.rs` reconnects
  when no frame parses within the idle timeout; pings, unparsable text and the consumer's own
  pauses between polls do not count. `telemetry.rs` emits `price_level_stream_*` metrics through
  the `metrics` facade, with label values as enums there and a frame-age histogram at
  acceptance; `skipped_books_total{reason}` counts the books left unpriced
  (`unadmitted_token`, `malformed_address`, `one_asset_pair`) and `rejected_sides_total{reason}` the
  sides dropped whole (`unsupported_level`, `non_finite_amount`, `zero_amount`,
  `conflicting_knots`). Per-venue series and every one of those reasons start at zero, and no
  label carries a streamed value except the address of an auto-detected venue — a token address
  or symbol never becomes a label, only a DEBUG log field. Components are identified as
  `pricelevelstream:{pamm}`, where `{pamm}` names the venue's **canonical public identity**: the
  address a swap goes to. `config.rs`'s `PAMM_ADDRESS_ALIASES` normalizes every boundary onto it —
  incoming book `quoteAddress`es as well as the addresses handed to `add_pamm`/`deny_pamm`, either
  of which may name either address (today's two entries are Kipseli, streamed under `0xc9a9…` and
  executed at `0x71e7…`, and Metric, streamed under `0xe844…` and executed at `0x99ab…`).
  Component ids, venue names, gas costs, denial and the `pamm_address`
  attribute therefore always carry the public identity, never the streamed one; `maker` is
  recorded nowhere. A
  new builder serves nothing: `with_known_pamms` registers the known-good venues and denies
  known-unexecutable ones, `add_pamm` registers individual ones, `deny_pamm` excludes one
  (dropping any registration and blocking auto-detection), and opt-in auto-detection
  additionally serves unknown venues under their address (`pricelevelstream:{0xaddress}`), at
  most 64 per process. Precedence: between `add_pamm` and `deny_pamm` for the same venue the
  later call wins, whichever of its addresses each named; `with_known_pamms` defaults never
  override either, regardless of call order.
  By default `build` emits every venue under `fallback:{pamm}`, so tycho-execution routes their
  swaps through `TychoFallbackRouter` (retry on a solver-named fallback pool when the venue
  reverts); `without_fallback_router` keeps every venue on the direct `pricelevelstream:` path.
  Consumers cannot tell a stale removal from a retired venue. Venues may overlap with other
  integration paths of the same liquidity (e.g. `vm:fermiswap`) — consumers must deduplicate by
  venue where double-counting matters

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
