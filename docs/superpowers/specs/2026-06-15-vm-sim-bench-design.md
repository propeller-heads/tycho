# VM Simulation Benchmark & Concurrency Harness — Design

## Purpose

Establish a release-mode measurement harness that baselines VM simulation performance on
current `main`, so a later optimization effort can be proven against three success criteria:

1. Individual `ProtocolSim::get_amount_out` calls get faster.
2. Fynd's routing simulation path gets faster.
3. Results stay correct and throughput improves (or holds) under concurrency.

This deliverable contains **no optimizations**. It produces benches, a concurrency
correctness test, committed fixtures, and saved baselines. The optimizations (lazy spot
prices, limit caching, lock batching) are a separate spec and plan, written after the
baseline numbers are reviewed.

## Background

Measured earlier (release, 2-token Balancer V2, single thread): a single VM `get_amount_out`
≈ 509µs, of which the eager all-permutation spot-price recompute is ~50%, `get_limits` ~5%,
and the swap itself ~35%. Native equivalents are sub-microsecond. All VM pools share one
global `SHARED_TYCHO_DB` (`crates/tycho-simulation/src/evm/engine_db/mod.rs`), so the
per-opcode `Arc<RwLock>` read is a cross-thread contention point under Fynd's parallel
workers.

Fynd consumes `tycho-simulation` from crates.io (`>=0.302.0`, no path/patch). Routing
(`bellman_ford.rs`, `most_liquid.rs`) reads only `amount`/`gas` from `get_amount_out`; spot
prices come from a separate per-block derived pass that reads all token-pair permutations.

`EVMPoolState` is intentionally not serde-serializable, but the upstream
`tycho-client` feed `Snapshot`/`FeedMessage` (which the decoder turns into pool states) is
fully serializable and is the same input Fynd's feed ingests.

## Scope

### In scope
- Capture-once committed fixtures (raw `FeedMessage`/`Snapshot` JSON) for a curated set of
  VM pools.
- `tycho-simulation` criterion benches for `get_amount_out` (and `spot_price`,
  `query_pool_swap` for a complete baseline).
- A concurrency equivalence test plus a contended throughput bench.
- A `fynd-core` criterion bench for the real routing sim path.
- Saved criterion baselines on the current `main` commit.

### Out of scope
- Any change to simulation logic. Lazy spot prices, limit caching, and lock batching are a
  follow-up spec/plan.

## Worktrees & dependency wiring

- `tycho-indexer` worktree `feat/vm-sim-bench`: all `tycho-simulation` benches, the capture
  tool, and the concurrency test.
- `fynd` worktree (separate branch): the `fynd-core` routing bench, with:
  ```toml
  [patch.crates-io]
  tycho-simulation = { path = "<tycho-indexer-worktree>/crates/tycho-simulation" }
  ```
- Both repos add `criterion` as a dev-dependency and declare `[[bench]]` entries with
  `harness = false`.

## Fixture capture (data)

A capture tool — a gated example/bin in the `tycho-simulation` worktree
(`examples/capture_vm_fixtures.rs`) — connects via `tycho-client` using `TYCHO_API_KEY`,
takes the first `Snapshot`, and serializes a curated subset to committed JSON:

- One 2-token Balancer V2 pool.
- One 3-token and one 4-token Curve/Balancer pool (to expose multi-token spot-price
  amplification).

For each chosen component the tool captures: its `ComponentWithState`, the transitively
referenced `vm_storage` (involved + stateless contracts), the block header, and the relevant
`Token` metadata. The tool filters the full snapshot down to only the dependencies of the
chosen components so fixtures stay small.

- Canonical fixtures: `crates/tycho-simulation/benches/fixtures/*.json`.
- The capture tool also writes a copy into the Fynd bench fixtures directory, so there is a
  single source of truth and no hand-edited duplicates.

**Replay** uses the production `TychoStreamDecoder` path, so benches build `EVMPoolState`
instances exactly as production does. After capture, all bench/test runs are offline and
deterministic.

## `tycho-simulation` benches

`benches/get_amount_out.rs` (criterion, `harness = false`):

- `get_amount_out` per fixture pool, with a small and a large input amount, grouped by token
  count (2 / 3 / 4) so the multi-token effect is visible.
- `spot_price` per fixture pool.
- `query_pool_swap` per fixture pool.

A shared `benches/common/mod.rs` loads a fixture JSON and returns decoded pool states keyed
by component id.

## Concurrency verification

`tests/concurrency.rs` (or a `#[test]` module), release-capable, runnable under
`cargo careful` / ThreadSanitizer:

- **Equivalence test (reads only):** build pools on the shared `SHARED_TYCHO_DB`; spawn N
  threads, each running a fixed sequence of `get_amount_out` / `spot_price` calls; assert
  every result is byte-identical to a single-threaded oracle running the same sequence.
- **Read/write interleaving test:** one thread applies block updates (`delta_transition`)
  while reader threads run; assert no panic/deadlock, and that each reader result matches the
  oracle for whichever block version was current at the time (result ∈ {pre-update,
  post-update}).

On baseline code these pass trivially; their discriminating power applies once the future
lazy-cache change introduces interior mutability. The harness is the guardrail for that
change.

`benches/concurrency_throughput.rs` (criterion): measure `get_amount_out` throughput at
1 / 2 / 4 / 8 threads on shared-DB pools, to baseline lock-contention scaling.

## `fynd-core` routing bench

`fynd-core/benches/routing_sim.rs` (criterion, patched to the tycho-sim worktree):

- Ingest the committed fixture through Fynd's existing snapshot-decode path to populate a
  `MarketState` and graph.
- Bench `most_liquid::simulate_path` over a representative multi-hop path.
- Bench `bellman_ford::find_best_route` for a sample order.

This exercises the genuine Fynd routing functions, not an imitation.

## Baseline / compare workflow

- On the baseline commit, in each worktree:
  `cargo bench --bench <name> -- --save-baseline before`.
- After the future optimization lands:
  `cargo bench -- --baseline before` for percentage deltas; the concurrency equivalence test
  must pass.

## Testing

- Benches are `harness = false` criterion binaries; they are not part of `cargo test`.
- The concurrency tests run under `cargo test` and `cargo careful test`.
- Fixture replay is deterministic and offline; the capture tool is the only network-touching
  component and is run manually, not in CI.

## Risks & mitigations

- **Fixture dependency completeness:** a chosen component might reference a stateless contract
  or token proxy not captured. Mitigation: capture follows the same attribute-driven
  dependency resolution the decoder uses; a replay smoke test asserts each fixture pool
  decodes and answers one `get_amount_out` before it is committed.
- **Cross-repo fixture drift:** mitigated by single-source capture writing both copies.
- **Patch path portability:** the `[patch.crates-io]` path is absolute to the local worktree;
  it stays local to the Fynd bench branch and is not merged.
