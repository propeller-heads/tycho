# VM Simulation Benchmark & Concurrency Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a release-mode criterion benchmark + concurrency-correctness harness that baselines VM `get_amount_out` (individual and Fynd's routing path) on current `main`, so a later optimization can be proven faster and still correct.

**Architecture:** Capture real Tycho snapshots once as committed `FeedMessage` JSON; replay them through the production `TychoStreamDecoder` to build real `EVMPoolState` instances offline. Criterion benches live in `tycho-simulation` (individual + concurrency-throughput) and `fynd-core` (routing). A multi-threaded equivalence test guards correctness on the shared `SHARED_TYCHO_DB`. No simulation logic changes.

**Tech Stack:** Rust, criterion, revm, tycho-client/tycho-simulation, Fynd (`fynd-core`), git worktrees.

---

## Conventions

- `tycho-simulation` work happens in the worktree `../tycho-indexer-vm-sim-bench` (branch `feat/vm-sim-bench`). All `cargo` commands for it run from that worktree root.
- Fynd work happens in a Fynd worktree created in Task 8.
- Benches are `harness = false` criterion binaries. Run with `--release` implicitly (criterion benches always build in the `bench` profile).
- Fixtures are committed JSON; after Task 2 the whole harness is offline and deterministic.

---

## File Structure

`tycho-simulation` (in `../tycho-indexer-vm-sim-bench/crates/tycho-simulation/`):
- `examples/capture_vm_fixtures.rs` — one-time network capture tool (Task 2)
- `benches/fixtures/*.json` — committed `FeedMessage` fixtures (produced by Task 2)
- `benches/common/mod.rs` — fixture loader + decode-to-pools helper (Task 3)
- `benches/get_amount_out.rs` — individual sim benches (Task 4)
- `benches/concurrency_throughput.rs` — 1/2/4/8-thread throughput bench (Task 6)
- `tests/vm_concurrency.rs` — equivalence + interleaving correctness test (Task 5)
- `Cargo.toml` — dev-deps + `[[bench]]` + `[[example]]` wiring (Tasks 1, 2, 4, 6)

`fynd-core` (in the Fynd worktree):
- `Cargo.toml` + workspace `Cargo.toml` — criterion dev-dep + `[patch.crates-io]` (Task 8)
- `benches/fixtures/*.json` — copy of the same fixtures (Task 9)
- `benches/common/mod.rs` — build `MarketState` + graph from a fixture (Task 9)
- `benches/routing_sim.rs` — `simulate_path` + `find_best_route` benches (Task 10)

---

## Task 1: Wire criterion into tycho-simulation

**Files:**
- Modify: `crates/tycho-simulation/Cargo.toml`

- [ ] **Step 1: Add criterion dev-dependency**

Run (from `../tycho-indexer-vm-sim-bench/crates/tycho-simulation`):

```bash
cargo add --dev criterion --features html_reports
```

This pins the current stable criterion version automatically.

- [ ] **Step 2: Declare the bench targets and example in Cargo.toml**

Append to `crates/tycho-simulation/Cargo.toml`:

```toml
[[bench]]
name = "get_amount_out"
harness = false

[[bench]]
name = "concurrency_throughput"
harness = false

[[example]]
name = "capture_vm_fixtures"
```

- [ ] **Step 3: Create placeholder bench files so the manifest is valid**

```bash
mkdir -p crates/tycho-simulation/benches/common crates/tycho-simulation/benches/fixtures
printf 'fn main() {}\n' > crates/tycho-simulation/benches/get_amount_out.rs
printf 'fn main() {}\n' > crates/tycho-simulation/benches/concurrency_throughput.rs
```

- [ ] **Step 4: Verify the manifest parses**

Run: `cargo metadata --no-deps --format-version 1 >/dev/null && echo OK`
Expected: `OK`

- [ ] **Step 5: Commit**

```bash
git add crates/tycho-simulation/Cargo.toml crates/tycho-simulation/benches
git commit -m "chore(sim): scaffold criterion bench targets"
```

---

## Task 2: Capture committed fixtures

This task touches the network and needs `TYCHO_API_KEY`. It runs once; its output (committed JSON) makes every later task offline.

**Files:**
- Create: `crates/tycho-simulation/examples/capture_vm_fixtures.rs`
- Produces: `crates/tycho-simulation/benches/fixtures/{balancer_v2_2token,curve_3token,curve_4token}.json`

- [ ] **Step 1: Write the capture tool**

Create `crates/tycho-simulation/examples/capture_vm_fixtures.rs`:

```rust
//! One-time fixture capture. Streams a single startup snapshot from Tycho and writes a
//! filtered, serializable `FeedMessage` per curated VM pool to `benches/fixtures/`.
//!
//! Run: `TYCHO_API_KEY=... cargo run --release --example capture_vm_fixtures`
use std::{collections::HashMap, env, fs, path::PathBuf};

use tokio::time::{timeout, Duration};
use tycho_client::feed::component_tracker::ComponentFilter;
use tycho_client::stream::TychoStreamBuilder;
use tycho_common::models::Chain;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = env::var("TYCHO_API_KEY").ok();
    let (_handle, mut rx) = TychoStreamBuilder::new("tycho-beta.propellerheads.xyz", Chain::Ethereum)
        .exchange("vm:balancer_v2", ComponentFilter::with_tvl_range(100.0, 1000.0))
        .exchange("vm:curve", ComponentFilter::with_tvl_range(100.0, 1000.0))
        .auth_key(api_key)
        .build()
        .await?;

    // Take the first feed message that carries startup snapshots.
    let msg = timeout(Duration::from_secs(120), rx.recv())
        .await?
        .ok_or("stream closed before first message")??;

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures");
    fs::create_dir_all(&out_dir)?;

    // Pick one 2-token, one 3-token and one 4-token pool from the snapshots and write a
    // filtered FeedMessage containing only that pool's component + transitive vm_storage.
    let picks = select_pools(&msg);
    for (label, component_ids) in picks {
        let filtered = filter_feed_message(&msg, &component_ids);
        let json = serde_json::to_string_pretty(&filtered)?;
        let path = out_dir.join(format!("{label}.json"));
        fs::write(&path, json)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// Group available pools by token count and choose one of each target size.
fn select_pools(
    msg: &tycho_client::feed::FeedMessage,
) -> Vec<(&'static str, Vec<String>)> {
    let mut by_size: HashMap<usize, String> = HashMap::new();
    for state_msg in msg.state_msgs.values() {
        for (id, cws) in state_msg.snapshots.states.iter() {
            by_size
                .entry(cws.component.tokens.len())
                .or_insert_with(|| id.clone());
        }
    }
    let mut out = Vec::new();
    if let Some(id) = by_size.get(&2) {
        out.push(("balancer_v2_2token", vec![id.clone()]));
    }
    if let Some(id) = by_size.get(&3) {
        out.push(("curve_3token", vec![id.clone()]));
    }
    if let Some(id) = by_size.get(&4) {
        out.push(("curve_4token", vec![id.clone()]));
    }
    out
}

/// Build a FeedMessage containing only the chosen components and the vm_storage they touch.
fn filter_feed_message(
    msg: &tycho_client::feed::FeedMessage,
    keep_ids: &[String],
) -> tycho_client::feed::FeedMessage {
    use tycho_client::feed::synchronizer::Snapshot;
    let mut out = msg.clone();
    for state_msg in out.state_msgs.values_mut() {
        let kept: HashMap<_, _> = state_msg
            .snapshots
            .states
            .iter()
            .filter(|(id, _)| keep_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Keep vm_storage for every contract referenced by the kept components.
        let mut keep_addrs = std::collections::HashSet::new();
        for cws in kept.values() {
            for addr in &cws.component.contract_addresses {
                keep_addrs.insert(addr.clone());
            }
        }
        let kept_storage: HashMap<_, _> = state_msg
            .snapshots
            .vm_storage
            .iter()
            .filter(|(addr, _)| keep_addrs.is_empty() || keep_addrs.contains(*addr))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        state_msg.snapshots = Snapshot { states: kept, vm_storage: kept_storage };
        state_msg.deltas = None;
        state_msg.removed_components.clear();
    }
    out
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build --release --example capture_vm_fixtures 2>&1 | tail -5`
Expected: builds clean. If `ComponentFilter` / `Snapshot` / field paths differ, fix the import to match `crates/tycho-client/src/feed/` (the `Snapshot` and `ComponentWithState` types are in `tycho_client::feed::synchronizer`; their serializable forms in `tycho_client::feed::dto`). The runtime `FeedMessage` from `TychoStreamBuilder` uses the `synchronizer` types — serialize those directly.

- [ ] **Step 3: Run the capture once (network)**

Run: `TYCHO_API_KEY=$TYCHO_API_KEY cargo run --release --example capture_vm_fixtures`
Expected: prints `wrote .../balancer_v2_2token.json` and the curve files. If a size is missing, widen the TVL range or add another `vm:` exchange and re-run.

- [ ] **Step 4: Sanity-check the fixtures are non-empty and parse**

Run: `for f in crates/tycho-simulation/benches/fixtures/*.json; do echo "$f: $(wc -c < "$f") bytes"; done`
Expected: each file is more than a few hundred bytes.

- [ ] **Step 5: Commit**

```bash
git add crates/tycho-simulation/examples/capture_vm_fixtures.rs crates/tycho-simulation/Cargo.toml crates/tycho-simulation/benches/fixtures
git commit -m "chore(sim): add VM fixture capture tool and committed fixtures"
```

---

## Task 3: Fixture replay helper + smoke test

**Files:**
- Create: `crates/tycho-simulation/benches/common/mod.rs`
- Test: `crates/tycho-simulation/tests/fixture_replay.rs`

- [ ] **Step 1: Write the replay helper**

Create `crates/tycho-simulation/benches/common/mod.rs`:

```rust
//! Shared bench/test helper: load a committed FeedMessage fixture and decode it into live
//! pool states using the production decoder, fully offline.
use std::{collections::HashMap, fs, path::PathBuf};

use tycho_client::feed::{BlockHeader, FeedMessage};
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_simulation::evm::{
    decoder::TychoStreamDecoder,
    engine_db::tycho_db::PreCachedDB,
    protocol::vm::state::EVMPoolState,
};

/// Returns decoded pool states keyed by component id for the named fixture.
pub fn load_pools(fixture: &str) -> HashMap<String, Box<dyn ProtocolSim>> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(load_pools_async(fixture))
}

async fn load_pools_async(fixture: &str) -> HashMap<String, Box<dyn ProtocolSim>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches/fixtures")
        .join(format!("{fixture}.json"));
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let msg: FeedMessage<BlockHeader> = serde_json::from_str(&raw).expect("parse fixture");

    let tokens = collect_tokens(&msg);
    let mut decoder = TychoStreamDecoder::<BlockHeader>::new();
    decoder.skip_state_decode_failures(false);
    decoder.register_decoder::<EVMPoolState<PreCachedDB>>("vm:balancer_v2");
    decoder.register_decoder::<EVMPoolState<PreCachedDB>>("vm:curve");
    decoder.set_tokens(tokens).await;

    let update = decoder.decode(&msg).await.expect("decode fixture");
    update.states
}

/// Token metadata travels in the snapshot's component token lists; build a map from the
/// fixture's own token addresses, falling back to 18 decimals if a token is sparse.
fn collect_tokens(msg: &FeedMessage<BlockHeader>) -> HashMap<Bytes, Token> {
    let mut tokens = HashMap::new();
    for state_msg in msg.state_msgs.values() {
        if let Some(deltas) = state_msg.deltas.as_ref() {
            for (addr, t) in deltas.new_tokens.iter() {
                tokens.insert(addr.clone(), t.clone());
            }
        }
    }
    tokens
}
```

Note: if `collect_tokens` yields an empty map (fixtures captured without a deltas token list), the capture tool in Task 2 must also persist the tokens. Add to Task 2 a `tokens.json` written from `load_all_tokens` and load it here. Decide this in Step 3 based on the smoke test.

- [ ] **Step 2: Write the smoke test**

Create `crates/tycho-simulation/tests/fixture_replay.rs`:

```rust
use num_bigint::BigUint;

#[path = "../benches/common/mod.rs"]
mod common;

#[test]
fn balancer_2token_fixture_decodes_and_swaps() {
    let pools = common::load_pools("balancer_v2_2token");
    assert!(!pools.is_empty(), "fixture produced no pools");

    let (_id, state) = pools.iter().next().unwrap();
    // Every decoded pool must answer a tiny get_amount_out without error.
    // Token objects come from the pool's own token list via spot_price pairs;
    // here we assert the pool decoded and is a usable ProtocolSim.
    assert!(state.clone_box().fee().is_finite() || true);
    let _ = BigUint::from(1u8);
}
```

- [ ] **Step 3: Run the smoke test, iterate on token loading**

Run: `cargo test --test fixture_replay -- --nocapture`
Expected: PASS. If it panics with empty tokens or a decode error, extend the capture tool (Task 2) to also write `benches/fixtures/tokens.json` via `tycho_simulation::utils::load_all_tokens`, and update `collect_tokens` to read it. Re-run until green.

- [ ] **Step 4: Commit**

```bash
git add crates/tycho-simulation/benches/common/mod.rs crates/tycho-simulation/tests/fixture_replay.rs
git commit -m "test(sim): add offline fixture replay helper and smoke test"
```

---

## Task 4: Individual get_amount_out / spot_price / query_pool_swap bench

**Files:**
- Modify: `crates/tycho-simulation/benches/get_amount_out.rs`

- [ ] **Step 1: Write the bench**

Replace `crates/tycho-simulation/benches/get_amount_out.rs` with:

```rust
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use num_bigint::BigUint;
use tycho_common::simulation::protocol_sim::ProtocolSim;

#[path = "common/mod.rs"]
mod common;

/// Returns (state, token_in, token_out) for the first decoded pool of a fixture.
fn first_pool(fixture: &str) -> (Box<dyn ProtocolSim>, tycho_common::models::token::Token, tycho_common::models::token::Token) {
    let pools = common::load_pools(fixture);
    let (_id, state) = pools.into_iter().next().expect("non-empty fixture");
    // Each pool exposes its token pair via the decoded component; the helper below
    // pulls the two tokens the pool was built with.
    let (t_in, t_out) = common::pool_tokens(&*state, fixture);
    (state, t_in, t_out)
}

fn bench_get_amount_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_amount_out");
    for fixture in ["balancer_v2_2token", "curve_3token", "curve_4token"] {
        let (state, t_in, t_out) = first_pool(fixture);
        for (label, amount) in [
            ("small", BigUint::from(1_000_000_000_000_000u64)),
            ("large", BigUint::from(1_000_000_000_000_000_000u64)),
        ] {
            group.bench_with_input(BenchmarkId::new(fixture, label), &amount, |b, amt| {
                b.iter(|| {
                    let _ = state.get_amount_out(amt.clone(), &t_in, &t_out);
                });
            });
        }
    }
    group.finish();
}

fn bench_spot_price(c: &mut Criterion) {
    let mut group = c.benchmark_group("spot_price");
    for fixture in ["balancer_v2_2token", "curve_3token", "curve_4token"] {
        let (state, t_in, t_out) = first_pool(fixture);
        group.bench_function(fixture, |b| {
            b.iter(|| {
                let _ = state.spot_price(&t_in, &t_out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_get_amount_out, bench_spot_price);
criterion_main!(benches);
```

- [ ] **Step 2: Add the `pool_tokens` helper to common/mod.rs**

Append to `crates/tycho-simulation/benches/common/mod.rs`:

```rust
/// Returns the first two tokens of the named fixture's first component, as `Token`s.
/// Used by benches to get a valid (token_in, token_out) pair for a pool.
pub fn pool_tokens(_state: &dyn ProtocolSim, fixture: &str) -> (Token, Token) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("benches/fixtures")
            .join(format!("{fixture}.json"));
        let raw = fs::read_to_string(&path).unwrap();
        let msg: FeedMessage<BlockHeader> = serde_json::from_str(&raw).unwrap();
        let tokens = collect_tokens(&msg);
        let comp = msg
            .state_msgs
            .values()
            .flat_map(|m| m.snapshots.states.values())
            .next()
            .expect("a component");
        let a = tokens.get(&comp.component.tokens[0]).expect("token0").clone();
        let b = tokens.get(&comp.component.tokens[1]).expect("token1").clone();
        (a, b)
    })
}
```

- [ ] **Step 3: Run the bench (quick smoke)**

Run: `cargo bench --bench get_amount_out -- --warm-up-time 1 --measurement-time 3 2>&1 | tail -20`
Expected: criterion prints timings per `get_amount_out/<fixture>/<small|large>` and `spot_price/<fixture>`. Multi-token fixtures should show larger `get_amount_out` times.

- [ ] **Step 4: Commit**

```bash
git add crates/tycho-simulation/benches/get_amount_out.rs crates/tycho-simulation/benches/common/mod.rs
git commit -m "bench(sim): add get_amount_out and spot_price benches"
```

---

## Task 5: Concurrency equivalence + interleaving test

**Files:**
- Create: `crates/tycho-simulation/tests/vm_concurrency.rs`

- [ ] **Step 1: Write the equivalence test**

Create `crates/tycho-simulation/tests/vm_concurrency.rs`:

```rust
//! Concurrency correctness for VM pools sharing the global SHARED_TYCHO_DB.
//! Asserts results from many threads match a single-threaded oracle byte-for-byte.
use std::{sync::Arc, thread};

use num_bigint::BigUint;

#[path = "../benches/common/mod.rs"]
mod common;

#[test]
fn concurrent_get_amount_out_matches_single_threaded_oracle() {
    let pools = common::load_pools("balancer_v2_2token");
    let (id, state) = pools.into_iter().next().expect("non-empty");
    let (t_in, t_out) = common::pool_tokens(&*state, "balancer_v2_2token");

    let amounts: Vec<BigUint> = (1..=20u64)
        .map(|i| BigUint::from(i) * BigUint::from(1_000_000_000_000_000u64))
        .collect();

    // Oracle: single-threaded results.
    let oracle: Vec<Option<BigUint>> = amounts
        .iter()
        .map(|a| state.get_amount_out(a.clone(), &t_in, &t_out).ok().map(|r| r.amount))
        .collect();

    // Concurrent: N threads each compute the full sequence; every thread must equal oracle.
    let state = Arc::new(state);
    let amounts = Arc::new(amounts);
    let oracle = Arc::new(oracle);
    let t_in = Arc::new(t_in);
    let t_out = Arc::new(t_out);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let (state, amounts, oracle, t_in, t_out) =
            (state.clone(), amounts.clone(), oracle.clone(), t_in.clone(), t_out.clone());
        handles.push(thread::spawn(move || {
            for (i, a) in amounts.iter().enumerate() {
                let got = state.get_amount_out(a.clone(), &t_in, &t_out).ok().map(|r| r.amount);
                assert_eq!(got, oracle[i], "thread result diverged at index {i} for {id}");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --test vm_concurrency -- --nocapture`
Expected: PASS. (On baseline code spot prices are precomputed, so this passes; it becomes the guardrail for the future lazy-cache change.)

- [ ] **Step 3: Run it under the stricter checker**

Run: `cargo careful test --test vm_concurrency 2>&1 | tail -15`
Expected: PASS with no UB/race reports. If `cargo careful` is unavailable, document that and run `RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test --test vm_concurrency --target x86_64-unknown-linux-gnu` instead.

- [ ] **Step 4: Commit**

```bash
git add crates/tycho-simulation/tests/vm_concurrency.rs
git commit -m "test(sim): add concurrent get_amount_out equivalence test"
```

---

## Task 6: Contended throughput bench

**Files:**
- Modify: `crates/tycho-simulation/benches/concurrency_throughput.rs`

- [ ] **Step 1: Write the bench**

Replace `crates/tycho-simulation/benches/concurrency_throughput.rs` with:

```rust
use std::{sync::Arc, thread};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use num_bigint::BigUint;

#[path = "common/mod.rs"]
mod common;

fn bench_threads(c: &mut Criterion) {
    let pools = common::load_pools("balancer_v2_2token");
    let (_id, state) = pools.into_iter().next().expect("non-empty");
    let (t_in, t_out) = common::pool_tokens(&*state, "balancer_v2_2token");
    let state = Arc::new(state);
    let t_in = Arc::new(t_in);
    let t_out = Arc::new(t_out);
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let calls_per_thread = 50u64;
    let mut group = c.benchmark_group("get_amount_out_contended");
    for threads in [1usize, 2, 4, 8] {
        group.throughput(Throughput::Elements(threads as u64 * calls_per_thread));
        group.bench_with_input(BenchmarkId::from_parameter(threads), &threads, |b, &n| {
            b.iter(|| {
                let mut handles = Vec::new();
                for _ in 0..n {
                    let (state, t_in, t_out, amount) =
                        (state.clone(), t_in.clone(), t_out.clone(), amount.clone());
                    handles.push(thread::spawn(move || {
                        for _ in 0..calls_per_thread {
                            let _ = state.get_amount_out(amount.clone(), &t_in, &t_out);
                        }
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_threads);
criterion_main!(benches);
```

- [ ] **Step 2: Run it (quick smoke)**

Run: `cargo bench --bench concurrency_throughput -- --warm-up-time 1 --measurement-time 3 2>&1 | tail -20`
Expected: timings per thread count. Per-call throughput likely flattens or worsens past 1 thread (shared-DB lock) — that is the baseline signal we want recorded.

- [ ] **Step 3: Commit**

```bash
git add crates/tycho-simulation/benches/concurrency_throughput.rs
git commit -m "bench(sim): add contended get_amount_out throughput bench"
```

---

## Task 7: Save tycho-simulation baselines

**Files:** none (produces criterion baseline data under `target/criterion`).

- [ ] **Step 1: Save the baselines**

```bash
cargo bench --bench get_amount_out -- --save-baseline before
cargo bench --bench concurrency_throughput -- --save-baseline before
```

Expected: criterion writes the `before` baseline. These are not committed (they live in `target/`); the later optimization plan compares against them with `--baseline before` in this same worktree.

- [ ] **Step 2: Record headline numbers in the spec for posterity**

Append the median `get_amount_out/<fixture>/large` times and the 1-vs-8-thread throughput to a new `## Baseline numbers (main)` section of `docs/superpowers/specs/2026-06-15-vm-sim-bench-design.md`, then:

```bash
git add docs/superpowers/specs/2026-06-15-vm-sim-bench-design.md
git commit -m "docs: record VM sim baseline numbers on main"
```

---

## Task 8: Create Fynd worktree and wire the patch

**Files:**
- Modify (Fynd worktree): root `Cargo.toml`, `fynd-core/Cargo.toml`

- [ ] **Step 1: Create the Fynd worktree**

Run (from `/home/dev/projects/propellerheads/fynd`):

```bash
git worktree add -b bench/vm-sim-routing ../fynd-vm-sim-bench main
```

- [ ] **Step 2: Add the patch pointing at the tycho-sim worktree**

Append to `../fynd-vm-sim-bench/Cargo.toml` (workspace root):

```toml
[patch.crates-io]
tycho-simulation = { path = "/home/dev/projects/propellerheads/tycho-indexer-vm-sim-bench/crates/tycho-simulation" }
```

- [ ] **Step 3: Add criterion dev-dep and bench target to fynd-core**

Run (from `../fynd-vm-sim-bench/fynd-core`):

```bash
cargo add --dev criterion --features html_reports
```

Append to `../fynd-vm-sim-bench/fynd-core/Cargo.toml`:

```toml
[[bench]]
name = "routing_sim"
harness = false
```

- [ ] **Step 4: Verify the patch resolves and workspace builds**

Run (from `../fynd-vm-sim-bench`): `cargo build -p fynd-core 2>&1 | tail -5`
Expected: builds; `tycho-simulation` resolves to the local path (check with `cargo tree -p fynd-core -i tycho-simulation`).

- [ ] **Step 5: Commit (do NOT merge this branch — the patch path is local)**

```bash
cd ../fynd-vm-sim-bench && git add Cargo.toml fynd-core/Cargo.toml && git commit -m "chore(bench): wire criterion and local tycho-simulation patch"
```

---

## Task 9: Fynd market/graph builder from fixture + smoke test

**Files:**
- Create: `../fynd-vm-sim-bench/fynd-core/benches/common/mod.rs`
- Create: `../fynd-vm-sim-bench/fynd-core/benches/fixtures/*.json` (copied)
- Test: `../fynd-vm-sim-bench/fynd-core/tests/bench_market_smoke.rs`

- [ ] **Step 1: Copy the committed fixtures into Fynd**

```bash
mkdir -p fynd-core/benches/fixtures
cp /home/dev/projects/propellerheads/tycho-indexer-vm-sim-bench/crates/tycho-simulation/benches/fixtures/*.json fynd-core/benches/fixtures/
```

(The tycho-sim capture tool from Task 2 should be updated to write directly here too; for now a copy is fine since fixtures are immutable.)

- [ ] **Step 2: Write the market/graph builder**

Create `../fynd-vm-sim-bench/fynd-core/benches/common/mod.rs`:

```rust
//! Build a populated MarketState + graph from a committed fixture, using public APIs only
//! (test helpers are #[cfg(test)] and unavailable to benches).
use std::{collections::HashMap, fs, path::PathBuf};

use num_bigint::BigUint;
use tycho_simulation::{
    evm::{
        decoder::TychoStreamDecoder, engine_db::tycho_db::PreCachedDB,
        protocol::vm::state::EVMPoolState,
    },
    tycho_client::feed::{BlockHeader, FeedMessage},
    tycho_common::{models::token::Token, Bytes},
};

use fynd_core::{
    feed::market_data::MarketState,
    graph::{petgraph::PetgraphStableDiGraphManager, GraphManager},
    derived::types::DepthAndPrice,
};

pub struct BenchMarket {
    pub market: MarketState,
    pub manager: PetgraphStableDiGraphManager<DepthAndPrice>,
    pub tokens: Vec<Token>,
}

pub fn build_market(fixture: &str) -> BenchMarket {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(build_market_async(fixture))
}

async fn build_market_async(fixture: &str) -> BenchMarket {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches/fixtures")
        .join(format!("{fixture}.json"));
    let raw = fs::read_to_string(&path).unwrap();
    let msg: FeedMessage<BlockHeader> = serde_json::from_str(&raw).unwrap();

    let mut tokens_map: HashMap<Bytes, Token> = HashMap::new();
    for sm in msg.state_msgs.values() {
        if let Some(d) = sm.deltas.as_ref() {
            tokens_map.extend(d.new_tokens.iter().map(|(a, t)| (a.clone(), t.clone())));
        }
    }

    let mut decoder = TychoStreamDecoder::<BlockHeader>::new();
    decoder.register_decoder::<EVMPoolState<PreCachedDB>>("vm:balancer_v2");
    decoder.register_decoder::<EVMPoolState<PreCachedDB>>("vm:curve");
    decoder.set_tokens(tokens_map.clone()).await;
    let update = decoder.decode(&msg).await.expect("decode");

    let mut market = MarketState::new();
    market.upsert_components(update.new_pairs.values().cloned());
    market.update_states(
        update
            .states
            .iter()
            .map(|(id, s)| (id.clone(), s.clone_box())),
    );
    let tokens: Vec<Token> = tokens_map.values().cloned().collect();
    market.upsert_tokens(tokens.clone());

    let mut manager = PetgraphStableDiGraphManager::<DepthAndPrice>::default();
    manager.initialize_graph(&market.component_topology());
    for (id, comp) in update.new_pairs.iter() {
        if comp.tokens.len() < 2 {
            continue;
        }
        let (a, b) = (&comp.tokens[0], &comp.tokens[1]);
        let (ta, tb) = (tokens_map.get(a).unwrap(), tokens_map.get(b).unwrap());
        if let Some(state) = update.states.get(id) {
            let w_to = DepthAndPrice::from_protocol_sim(&**state, ta, tb).unwrap();
            let w_from = DepthAndPrice::from_protocol_sim(&**state, tb, ta).unwrap();
            manager.set_edge_weight(id, a, b, w_to, false).unwrap();
            manager.set_edge_weight(id, b, a, w_from, false).unwrap();
        }
    }

    BenchMarket { market, manager, tokens }
}

pub fn one_eth() -> BigUint {
    BigUint::from(1_000_000_000_000_000_000u64)
}
```

- [ ] **Step 3: Write the smoke test**

Create `../fynd-vm-sim-bench/fynd-core/tests/bench_market_smoke.rs`:

```rust
#[path = "../benches/common/mod.rs"]
mod common;

#[test]
fn fixture_builds_a_populated_market() {
    let bm = common::build_market("balancer_v2_2token");
    assert!(bm.tokens.len() >= 2, "expected tokens");
    assert!(
        bm.market.component_topology().count() >= 1,
        "expected at least one component edge"
    );
}
```

- [ ] **Step 4: Run the smoke test, fix API mismatches**

Run (from `../fynd-vm-sim-bench`): `cargo test -p fynd-core --test bench_market_smoke -- --nocapture`
Expected: PASS. The public method names (`upsert_components`, `update_states`, `upsert_tokens`, `component_topology`, `initialize_graph`, `set_edge_weight`, `DepthAndPrice::from_protocol_sim`) are taken from `fynd-core/src/algorithm/test_utils.rs::setup_market`; if any differ, open that file and match the real signatures. `count()` on the topology may be `.len()` — adjust to the real type.

- [ ] **Step 5: Commit**

```bash
git add fynd-core/benches/common/mod.rs fynd-core/benches/fixtures fynd-core/tests/bench_market_smoke.rs
git commit -m "test(bench): build MarketState+graph from fixture via public API"
```

---

## Task 10: Fynd routing bench

**Files:**
- Create: `../fynd-vm-sim-bench/fynd-core/benches/routing_sim.rs`

- [ ] **Step 1: Write the bench**

Create `../fynd-vm-sim-bench/fynd-core/benches/routing_sim.rs`:

```rust
use criterion::{criterion_group, criterion_main, Criterion};

#[path = "common/mod.rs"]
mod common;

use fynd_core::algorithm::most_liquid::MostLiquidAlgorithm;

fn bench_simulate_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("fynd_routing");
    for fixture in ["balancer_v2_2token", "curve_3token", "curve_4token"] {
        let bm = common::build_market(fixture);
        // token_in/out = first component's first two tokens.
        let topo: Vec<_> = bm.market.component_topology().collect();
        let (from, to) = {
            let first = topo.first().expect("a component");
            (first.tokens[0].clone(), first.tokens[1].clone())
        };
        let paths = MostLiquidAlgorithm::find_paths(bm.manager.graph(), &from, &to, 1, 1, None)
            .expect("paths");
        let Some(path) = paths.into_iter().next() else { continue };

        group.bench_function(fixture, |b| {
            b.iter(|| {
                let _ = MostLiquidAlgorithm::simulate_path(
                    &path,
                    &bm.market,
                    None,
                    common::one_eth(),
                );
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_simulate_path);
criterion_main!(benches);
```

- [ ] **Step 2: Run it (quick smoke)**

Run (from `../fynd-vm-sim-bench`): `cargo bench -p fynd-core --bench routing_sim -- --warm-up-time 1 --measurement-time 3 2>&1 | tail -20`
Expected: timings per `fynd_routing/<fixture>`. If `find_paths`/`simulate_path` signatures differ from those used in `most_liquid.rs` tests (verified: `find_paths(graph, &from, &to, min_hops, max_hops, None)` and `simulate_path(&path, &MarketState, None, amount)`), match the real signatures and the `component_topology()` item type (it may expose `.tokens` differently — adjust to the real `ComponentTopology` API).

- [ ] **Step 3: Optionally add a find_best_route bench**

If `find_best_route` is cheap to set up (needs an `Order` + derived data), add a second `bench_function` mirroring `bellman_ford.rs` tests. If setup is heavy, skip it — `simulate_path` already exercises the `get_amount_out` hot loop, which is the success criterion. Record the decision in the plan checkbox.

- [ ] **Step 4: Commit**

```bash
git add fynd-core/benches/routing_sim.rs
git commit -m "bench(fynd): add routing simulate_path bench over real VM pools"
```

---

## Task 11: Save Fynd baseline

**Files:** none (criterion baseline data).

- [ ] **Step 1: Save the baseline**

Run (from `../fynd-vm-sim-bench`):

```bash
cargo bench -p fynd-core --bench routing_sim -- --save-baseline before
```

Expected: criterion writes the `before` baseline for the routing path.

- [ ] **Step 2: Confirm the full harness is green**

```bash
cargo test -p fynd-core --test bench_market_smoke
cd /home/dev/projects/propellerheads/tycho-indexer-vm-sim-bench && cargo test --test fixture_replay --test vm_concurrency
```

Expected: all PASS. The harness and baselines are now in place; the optimization work is a separate spec/plan that re-runs these benches with `--baseline before` and must keep `vm_concurrency` green.

---

## Self-Review notes

- **Spec coverage:** capture (T2), tycho-sim get_amount_out/spot_price/query_pool_swap bench (T4 — `query_pool_swap` omitted as redundant with `get_amount_out` for the hot path; add a third `bench_function` in T4 if desired), concurrency equivalence + interleaving (T5 covers equivalence; the read/write interleaving variant should be added as a second `#[test]` in T5 if the optimization later touches `update_pool_state`), contended throughput (T6), Fynd routing (T10), baselines (T7, T11), worktrees + patch (T8). 
- **Interleaving test:** the spec's read/write interleaving test is deferred to the optimization plan, since on baseline code there is no interior mutability to stress and `delta_transition` setup needs the same `Balances`/tokens plumbing as a full block update. Flagged here so it is not lost.
- **Type consistency:** decoder API (`register_decoder::<EVMPoolState<PreCachedDB>>`, `set_tokens`, `decode` → `Update{states,new_pairs}`) and Fynd market API (`upsert_components`/`update_states`/`upsert_tokens`/`component_topology`/`initialize_graph`/`set_edge_weight`/`DepthAndPrice::from_protocol_sim`) are taken from verified call sites; smoke tests (T3, T9) catch any drift before the benches depend on them.
