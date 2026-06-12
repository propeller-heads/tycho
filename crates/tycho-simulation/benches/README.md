# tycho-simulation benchmarks

Criterion benchmarks for the Uniswap V3 CLMM swap simulation
(`UniswapV3State::get_amount_out`), driven by a **frozen corpus** of realistic
tick data from the top mainnet Uniswap V3 pools by 30-day volume.

The harness is a *reference* against which amortized-optimization candidates
(per-block precompute curves, batch APIs, zero-alloc quotes) are compared, for
both performance and bit-exact output equality.

## Layout

```
benches/
  clmm_swap.rs        # criterion harness (the [[bench]] target)
  common/mod.rs       # BenchQuoter trait, fixtures, integer stratification, validation
  data/
    fetch_pools.py    # snapshot fetcher (live RPC -> JSON)
    pool_universe.json# candidate pool addresses (from a Dune top-volume query)
    index.json        # list of snapshots + fetch block
    <PAIR>_<fee>.json # one snapshot per pool
tests/
  clmm_replay.rs      # output-equality + determinism tests (reuses benches/common)
```

## The `BenchQuoter` abstraction

Every benchmark group and the replay test are generic over `common::BenchQuoter`:

```rust
trait BenchQuoter: Sized {
    fn name() -> &'static str;
    fn prepare(pool: &LoadedPool) -> Self;          // owned fixture, untimed setup
    fn warm(&mut self, direction: Direction);       // per-block precompute
    fn quote(&self, amount: &BigUint, direction: Direction) -> QuoteOutput;
}
```

`ReferenceQuoter` wraps `UniswapV3State::get_amount_out` exactly as production
calls it (including the per-call state clone); its `warm` is a no-op. A candidate
implements the trait and registers in one line per group (and in the replay
test's `assert_matches_reference` call site). `prepare`/`warm` run in the
`iter_batched` setup closure (untimed) and the timed routine receives the owned,
warmed fixture — so a lazy-cache candidate's per-block build never lands inside
the timed window.

## Benchmark groups

| Group | Measures | Pools |
|---|---|---|
| `clmm_swap_1k` (primary) | warm amortized path: 1000 quotes/pool/direction, `Throughput::Elements(1000)` | all |
| `clmm_swap_1k_cold` | cold-first: same routine, setup does **not** `warm()` | all |
| `clmm_precompute` | once-per-block build: timed routine is one `warm()` (fixture returned so drop is untimed) | all |
| `clmm_swap_per_stratum` | per-regime distribution: 100 same-stratum quotes × 10 strata | subset |
| `clmm_overhead` | per-call alloc floor: 1000× fixture clone+box+drop | all |
| `clmm_swap_size_grid` (secondary) | single-swap size gradient (fractions of valid limit) | subset |

All pool-state benches use `iter_batched(BatchSize::LargeInput)` and black-box
inputs and outputs. The expensive per-stratum and size-grid groups run over a
**fee-tier-diverse subset** (thinnest + thickest pool per fee tier, capped at 10)
because at ~90 pools they would otherwise dominate total wall time; the primary,
cold, precompute, and overhead groups cover the whole corpus.

### Production vs. math-only numbers

`clmm_swap_1k` is the production-faithful number (the reference clones the full
pool state per call, as production does). To attribute time to swap math alone,
subtract the matching `clmm_overhead` floor for that pool. `clmm_swap_per_stratum`
is the lens for regime-asymmetric candidates (e.g. a curve that is neutral on
0-tick swaps and 3-20× on multi-tick swaps): a blended 1000-call mean would hide
that, the per-stratum split surfaces it.

### Stratified amounts (integer-only, platform-independent)

`common::stratified_amounts(valid_limit, n, seed)` splits `1..=valid_limit` into
10 log2-spaced strata by **bit length**, merging spans that would collapse for
small limits, and draws amounts uniformly within each stratum's integer range
from a seeded `ChaCha8Rng` (seed 42). It uses **only `BigUint` integer
arithmetic** — no `f64`/libm — so the same `(valid_limit, n, seed)` yields a
bit-identical sequence on any platform/build. This is what makes the replay
equality diff trustworthy across hosts.

`valid_limit` is the per-direction *window-bounded* limit: 90% of the
simulation's own `get_limits` max `amount_in`. `get_limits` walks only the loaded
ticks and stops at the fetched window edge, so amounts never reach the truncation
boundary — every benchmarked swap is a real interior swap, not a window artifact.
It is computed in Rust at load time (not stored in JSON) to reuse the real swap
engine and stay deterministic.

### Validation pass

Before any timing, every pool/direction runs all 1000 amounts once through the
reference and **panics** on (a) any early bail-out (no-liquidity / fatal /
overflow / price-limit) — guaranteeing the timed loop's black-boxed result never
hides no-work bailouts — and (b) stratum degeneracy (>50% identical amounts in a
stratum). Partial fills (the `Ticks exceeded` path, which still carries a real
swap) are counted and reported, not rejected.

## Output-equality replay

`tests/clmm_replay.rs` runs the identical seeded, validated sequence through every
registered quoter and asserts `QuoteOutput::equivalent` per input: same fill/bail
classification, and for fills identical amount, gas, partial flag, and resulting
pool state (compared via `ProtocolSim::eq`, which covers sqrt_price, tick,
liquidity, and ticks). This is the acceptance gate for candidates. It currently
runs reference-vs-reference (self-consistency + plumbing).

```bash
cargo test -p tycho-simulation --test clmm_replay
```

## Comparison workflow (reference vs. candidate)

```bash
# Save the current implementation as the reference baseline
cargo bench -p tycho-simulation --bench clmm_swap -- --save-baseline reference

# After implementing a candidate (registered under its own BenchQuoter::name())
cargo bench -p tycho-simulation --bench clmm_swap -- --save-baseline candidate

# Compare (requires: cargo install critcmp)
critcmp reference candidate

# CI-style assert against a committed baseline without overwriting it
cargo bench -p tycho-simulation --bench clmm_swap -- --baseline reference
```

`--quick` is for fast sanity checks only — never for baselines used in critcmp
(it reduces sample count).

## Data corpus

### Frozen corpus (canonical)

The committed `data/*.json` snapshots are the **canonical, frozen corpus**: all
saved baselines and the replay test are tied to it. The corpus is fully offline
and reproducible from the committed files alone — no network needed to run the
benchmark or the test.

Provenance:
- Pool universe (`pool_universe.json`): top ~100 mainnet Uniswap V3 pools by
  30-day USD volume, from a Dune query against `dex.trades`.
- Per pool, `slot0` / `liquidity` / `fee` / `tickSpacing` / tokens (with
  `decimals`/`symbol`) are read directly; initialized ticks via the canonical
  **TickLens** (`0xbfd8137f7d1516D3ea5cA83523914859ec47F573`), one bitmap word at
  a time over a window centred on the current tick.
- The window starts at ~±20% price (`ln(1.2)/ln(1.0001) ≈ 1823` ticks each side,
  widened to whole bitmap words) and **expands per pool until there is at least
  one initialized tick on both sides of the current tick**, so neither swap
  direction is a one-sided truncation artifact. Pools that cannot satisfy this
  within a hard cap are **dropped**. Ticks beyond `MAX_TICKS_PER_SIDE` (250) each
  side are trimmed; `valid_limit` keeps swaps inside what is kept, so the trim is
  safe.

Data is **real**, not synthetic. `index.json` records the `fetch_block`.

### Extending / re-fetching the corpus (changes the baseline)

Re-running the fetcher fetches **current** chain state — different ticks, limits,
and therefore different amount sequences and numbers. It does **not** reproduce
the committed snapshots. Treat a re-fetch as creating a *new* corpus and re-save
the `reference` baseline afterwards.

```bash
export RPC_URL=https://<your-ethereum-rpc>   # or ETH_RPC_URL
cd crates/tycho-simulation/benches/data
uv run fetch_pools.py                  # rewrites <PAIR>_<fee>.json + index.json
uv run fetch_pools.py --max-pools 30   # smaller corpus
```

To reproduce the *committed* snapshots exactly you need an **archive node** and
must pin `--block <fetch_block>` (from `index.json`); non-archive nodes prune
state and revert on historical `slot0` reads. For most users the committed JSON
is the source of truth and no fetch is needed.

Fetcher implementation notes:
- Defaults to `latest`.
- int24 fields (`tick`, `tickSpacing`) and the int16 TickLens word index are
  decoded/encoded as full sign-extended 256-bit two's-complement words.
- JSON-RPC batches are chunked and degrade to per-call requests when a provider
  rejects or rate-limits batch arrays.
- Edit the pool universe by re-running the Dune query and replacing
  `pool_universe.json`.

## Scope / deferred

V4 (`uniswap_v4`) is **not** covered yet: the V4 curve (dynamic fees, hooks) has
no candidate consumer in this repo, so the V4 fixtures would have no use. V4
coverage lands with the first V4-touching candidate and is a precondition before
any such candidate is accepted.
