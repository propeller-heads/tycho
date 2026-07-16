# VM Simulation: Lazy Spot Prices + Limit Caching — Design

## Purpose

Cut the cost of VM `get_amount_out` by eliminating redundant work that the benchmark baseline
identified, without changing any simulation result. Two changes, both in
`crates/tycho-simulation/src/evm/protocol/vm/state.rs`:

1. **Lazy spot prices** — stop eagerly recomputing all token-pair spot prices on the `new_state`
   returned by `get_amount_out`; compute a pair on demand and cache it.
2. **Limit caching** — cache the block-stable `get_limits` result per `(sell, buy)` pair so repeated
   `get_amount_out` calls on the same state reuse it.

Measured baseline (release): eager all-permutation spot-price recompute is ~50% of a single
`get_amount_out`; `get_limits` is another ~5% and is recomputed on every call. Routing
(`find_best_route` → many `get_amount_out`) and `query_pool_swap` (~30-iteration search on one
state) pay these repeatedly. Validated against Fynd: routing reads only `amount`/`gas`, so the eager
recompute is pure waste there; the per-block derived pass reads all spot-price pairs (kept fast by
preserving the eager per-block warm, below).

## Scope

In scope: `EVMPoolState` in `state.rs` and its tests. Out of scope: adapter contract, engine DB,
decoder, and lock-batching of `SHARED_TYCHO_DB` (the baseline showed concurrent reads already scale
~linearly, so lock-batching is unwarranted).

## Mechanism: thread-safe read-through caches (Approach A)

`EVMPoolState` gains two interior-mutable caches and drops `#[derive(Clone)]` for a hand-written
`Clone`:

```rust
spot_price_cache: RwLock<HashMap<(Address, Address), f64>>,
limit_cache:      RwLock<HashMap<(Address, Address), (U256, U256)>>,
```

- `std::sync::RwLock` (already used by the engine DB; no new dependency). Reads take a read lock
  (concurrent — preserves the measured linear multi-thread read scaling); a miss takes a brief write
  lock to insert.
- Two independent locks (not one combined) so a write to one cache never blocks reads of the other.
- `Clone` deep-copies both maps into fresh `RwLock`s (`RwLock::new(self.spot_price_cache.read()…clone())`),
  so a cloned state owns an independent cache — never a shared lock.
- The existing `Serialize`/`Deserialize` impls already return errors; unchanged.

The plain `spot_prices: HashMap` field is replaced by `spot_price_cache`.

## Behaviour

### `spot_price(&self, base, quote)` — lazy read-through
Cache hit → return. Miss → compute that **single** pair using the existing per-pair logic
(`adapter.price()` when the pool has the `PriceFunction` capability, else the two-swap
finite-difference, with the same decimal scaling as today), insert into `spot_price_cache`, return.
The value is identical to what the eager path produced for that pair.

### `get_amount_limits(&self, sell, buy, overwrites)` — cache by `(sell, buy)`
Cache hit → return. Miss → call the adapter (one EVM sim), insert into `limit_cache`, return.

**Validity assumption (correctness-sensitive):** the limit is a property of pool liquidity for the
current pool-state-version and is independent of the *amount* in the passed overwrites (the
overwrites only set a large external-account balance/allowance). Therefore `(sell, buy) → (sell_limit,
buy_limit)` is stable for a given state-version. This is the one assumption the equivalence test
explicitly guards: limits computed via the cache must byte-match limits computed fresh with the
call's own overwrites.

### `get_amount_out` — stop eager recompute, invalidate the child
- The HardLimits check calls `get_amount_limits(self)` → now a cache lookup. This is the limit-caching
  win: the `query_pool_swap` search and multi-path routing issue many `get_amount_out` calls on the
  same `self`, which now share one cached limit instead of recomputing it each time.
- After building `new_state` and applying the swap's storage overwrites, **clear both caches on
  `new_state`** (they are stale post-swap) instead of calling `set_spot_prices`. Correct post-swap
  values are computed lazily if/when `new_state.spot_price()` / `get_limits()` is read (e.g. by
  `query_pool_swap`).
- Returned `amount`/`gas` are unchanged — they never depended on spot prices.

### `update_pool_state` (per block) — eager warm preserved
Clear both caches (state changed), then run the existing eager `set_spot_prices`. Because
`set_spot_prices` already calls `get_amount_limits` per pair, and `get_amount_limits` is now
cache-through, `limit_cache` is warmed naturally as a side effect — no separate population step.
Net effect on the streaming/derived path: identical to today (prices pre-warmed), with limits now
warmed too. The library contract (`delta_transition` pre-warms prices; compute errors surface at
delta time) is preserved.

### Invalidation — one rule
Both caches are valid for the current pool-state-version and are cleared exactly when pool state
changes: (a) on `new_state` in `get_amount_out` after applying swap overwrites, and (b) in
`update_pool_state` per block. Nothing else mutates pool state.

## Error handling
Per-pair lazy compute returns the same `SimulationError` variants the eager path does (e.g. a pair
with no `PriceFunction` capability falls back to the two-swap method; a failed adapter call
propagates). A miss that errors is **not** cached (only successful values are inserted), so a
transient failure does not poison the cache.

## Testing / correctness bar
- **Equivalence**: existing `state.rs` tests pin exact outputs (e.g. `test_get_amount_out` asserts
  `amount == 137780051463393923` and `assert_ne!` on spot prices between states). These must still
  pass; update only the assertions that read the now-private `spot_prices` field to use
  `spot_price()` (lazy) instead, keeping the exact expected values. Add a focused test asserting a
  cached `get_limits` equals a freshly-computed one (guards the §`get_amount_limits` assumption).
- **Concurrency**: extend `tests/vm_concurrency.rs` to also hammer `spot_price()` from N threads on a
  state with a cold cache, comparing every result to a single-threaded oracle — proving the
  read-through cache races are benign (multiple threads may compute the same pair; inserts are
  idempotent).
- **Benchmarks** (compare to the saved `before` baselines): in the tycho-sim worktree
  `cargo bench -- --baseline before` for `get_amount_out` (expect a large drop, largest on the
  multi-token curve fixtures) and `concurrency_throughput` (no regression); in the Fynd worktree
  `cargo bench -p fynd-core --bench routing_sim -- --baseline before` (expect a routing-path drop).
- `cargo clippy` clean; `cargo +nightly fmt`.

## Future work (V2) — background / parallel cache warming
Not in this cycle. The thread-safe read-through caches make this a natural extension: a background
warmer simply calls `spot_price()` / `get_amount_limits()` ahead of demand from another thread, and a
later foreground read becomes a hit — no redesign.
- **Good fit:** the per-block / per-pool warm is embarrassingly parallel (pools independent, pairs
  independent) and `SHARED_TYCHO_DB` reads scale ~linearly (measured). Warming across pools/pairs on a
  thread pool (e.g. `rayon`) off the block-processing critical path could cut per-block warm latency
  on multi-core. In Fynd this maps onto the existing async per-block derived-data pass.
- **Not applicable:** the transient `new_state` caches in `get_amount_out` are created and consumed
  within a single routing computation — no idle window to pre-warm; they stay lazy.
- **Defer the "only when idle" part:** CPU-idle detection adds a scheduler/work-queue and risks
  warming pairs nobody queries. Start with plain parallelization; add idle-gating only if profiling
  justifies it. Best implemented at the consumer/derived-data layer or behind an opt-in library API,
  not forced into the synchronous library path.

## Non-goals
- No change to `get_amount_out`'s returned amounts or gas.
- No change to the per-block streaming behaviour beyond also warming the limit cache.
- No lock-batching of the shared DB.

## Results — after lazy caching (deltas vs `before` baseline)

Measured on branch `feat/vm-sim-bench` (optimized) vs the `before` baseline re-saved on the
rebased pre-optimization tip (same latest-main code minus the caching change), release, x86_64,
`cargo bench -- --baseline before`. All changes statistically significant (p < 0.05).

### get_amount_out (median, and change)

| fixture | before | after | change |
|---|---|---|---|
| balancer_v2_2token / small | ~467 µs | 197.6 µs | **−57.6%** |
| balancer_v2_2token / large | ~459 µs | 200.6 µs | **−56.2%** |
| curve_3token / small | ~1.83 ms | 382.2 µs | **−79.1%** |
| curve_3token / large | ~1.81 ms | 377.8 µs | **−79.1%** |
| curve_4token / small | ~1.30 ms | 275.0 µs | **−78.9%** |
| curve_4token / large | ~1.33 ms | 274.8 µs | **−79.4%** |

Multi-token pools gain the most (−79%): they had the most token-pair permutations to eagerly
recompute per call. The 2-token pool still gains −57% (eager recompute removed + limit reuse).

### spot_price (median)

Now a lock-guarded cache read: ~51 ns (from ~0.95 µs), **−94.5%** across all fixtures.

### get_amount_out_contended (throughput, no regression)

| threads | before thrpt | after thrpt | change |
|---|---|---|---|
| 1 | ~1.53 Kelem/s | 1.98 Kelem/s | **+29%** |
| 2 | ~2.86 Kelem/s | 3.84 Kelem/s | **+34%** |
| 4 | ~5.20 Kelem/s | 7.64 Kelem/s | **+47%** |
| 8 | ~10.3 Kelem/s | 15.6 Kelem/s | **+52%** |

Concurrent throughput improves (limit caching helps the repeated-call loop; the `RwLock` reads do
not regress scaling). The concurrency equivalence tests (`vm_concurrency`) stay green, including the
cold-miss `spot_price` race.

### Correctness

All exact-value unit tests unchanged (amounts, limits, spot prices byte-identical). Override-enabled
pools bypass the caches and keep today's eager behaviour. One pre-existing flaky test
(`test_failing_overrides_error_by_default`, ~1/10 under parallel runs) is unrelated — confirmed
present at the same rate on the pre-optimization base.

### Fynd routing (find_best_route, median) — the end-to-end consumer metric

| fixture | after | change vs before |
|---|---|---|
| balancer_v2_2token | 208.6 µs | **−55.3%** |
| curve_3token | 1.155 ms | **−41.7%** |
| curve_4token | 3.284 ms | **−36.1%** |

Fynd's real `MostLiquidAlgorithm::find_best_route` over VM pools. The end-to-end gain is smaller than
the raw `get_amount_out` gain because routing also does native graph traversal, path scoring, and gas
math that this change does not touch — the VM-simulation slice shrinks while the rest is unchanged.
(The routing `before` baseline was saved against tycho-simulation 0.305 pre-optimization; the
0.305→0.335 main-drift was separately measured as noise-level on `get_amount_out`, so the delta
reflects the optimization.)
