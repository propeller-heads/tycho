# PriceLevelStream Freshness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Titan pAMM price level stream fail closed on stale data: every served component expires 24 s after the frame that carried it unless a fresher frame carries it again, expiry is emitted as `removed_pairs`, and the next fresh frame carrying it re-adds it.

**Architecture:** `PriceLevelStreamBuilder::build()` becomes an `async_stream` loop that selects over Titan frames, an earliest-deadline timer, and a PropAMMRouter whitelist reader, and feeds typed events into a synchronous `SnapshotTracker` (moved to its own file) that takes an injected `Now`. The tracker keys freshness per component, never removes on frame diff, and only removes on expiry or a whitelist family change. `PriceLevelStreamState` carries a monotonic, live-only one-block quote guard as defence in depth.

**Tech Stack:** Rust on the workspace `stable` toolchain (`rust-toolchain.toml`; crate MSRV 1.91), fmt and clippy on `nightly-2026-06-28`, the exact toolchain CI pins, installed in Task 0 and used for every lint command in this plan, tokio 1.47 (`select!`, `time`), `async-stream`, `futures`, `tokio-tungstenite` 0.28 (client and, in tests, `accept_async` loopback server), `metrics` 0.24 facade, `metrics-util` 0.20 `DebuggingRecorder` in tests, `serde`.

**Spec:** `.claude/specs/2026-09-04-price-level-stream-freshness-design.md`

## Global Constraints

- Changes are confined to `crates/tycho-simulation/` and `crates/tycho-simulation/CLAUDE.md`, plus the workspace `Cargo.lock` that the new dependencies update. No Fynd, no indexer, no GitBook.
- `PriceLevelStreamBuilder::build()` keeps its signature `impl Stream<Item = Update> + Send`, lazy connect on first poll, never terminates, drop closes the socket and the whitelist reader.
- `stale_after` default `24 s`; a frame whose `frame_age >= stale_after` is rejected. `QUOTE_TTL = 12 s`, monotonic, `#[serde(skip)]`. `MAX_FUTURE_SKEW = 12 s`. Block jump slack `+2`. `read_idle_timeout` default `10 s`. `whitelist_refresh_interval` default `10 min`. `WHITELIST_READ_TIMEOUT = 15 s`.
- Acceptance order: age, future, order (`>=`, equal allowed), block regression, block jump. The two block checks are skipped whenever `last_accepted` is `None`, which is true at start and after every return to `Unserved`.
- Removal triggers are exactly two: component deadline, whitelist family change. No diff-based removal.
- An id never appears in both `removed_pairs` and `new_pairs` of one `Update`.
- Every `Update` has `is_partial = true`, empty `sync_states`, `block_number_or_timestamp` = newest accepted block.
- Metric names are exactly the ten in spec section 5.11, prefix `price_level_stream_`. No metric label ever carries a value from the wire; per-venue series are initialised to 0 for registered venues.
- Tests use no external network. Loopback WebSocket servers and scripted fetch closures are the only I/O.
- Before every commit: `cargo +nightly-2026-06-28 fmt` and `cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings`, the same pinned toolchain CI uses (`.github/workflows/ci-rust.yaml`). Never substitute an unpinned `nightly`. `/run-ci` runs stable clippy and is a supplement, not the CI equivalent.
- Commit messages: conventional, `feat(simulation): ...`, `fix(simulation): ...`, `refactor(simulation): ...`, `test(simulation): ...`, `docs(simulation): ...`, subject ≤ 72 chars, imperative. Branch: `tl/price-level-stream-freshness` (this repo's convention, 25 existing `tl/` branches). Never push to main.
- No model or assistant attribution anywhere in commit messages, commit trailers, or the PR body. Authentication setup is not a plan step and the plan never mutates git configuration.
- Test expectations name the tests and require zero failures; exact totals are not asserted because they drift as tasks add tests. The one ignored test throughout is `fallback_router::tests::test_fetch_fallback_router_venues_against_mainnet`.
- Rust style from `.claude/knowledge/rust.md`: `let ... else` early returns, no wildcard matches, enums for state, `tracing` for logs, docstrings say what, not where called.

---

## File structure

| File | Responsibility after this plan |
|---|---|
| `crates/tycho-simulation/src/price_level_stream/mod.rs` | Module docs (the contract), `pub mod` list, `mod tracker; mod telemetry; mod titan; #[cfg(test)] mod test_support;` |
| `.../titan.rs` | Wire types (now with `timestamp`), connection loop, idle timeout measured from parsed frames, reconnect telemetry, `pub(super) fn backoff` |
| `.../tracker.rs` (new) | `Now`, `Source`, `Served`, `Rejection`, `SnapshotTracker` with `on_frame` / `on_stale_deadline` / `on_router_venues` / `stale_deadline`, `build_component`, `component_id`, tracker tests |
| `.../telemetry.rs` (new) | Metric name constants, one emitting fn per metric, `SourceState`, test helpers for reading a `DebuggingRecorder` snapshot |
| `.../fallback_router.rs` | Existing `fetch_fallback_router_venues` plus `RouterVenuesRead` and `router_venues_reader` (timeout + retry + refresh stream) |
| `.../state.rs` | `PriceLevelStreamState` with `quotable_until`, `QUOTE_TTL`, `with_quotable_until`, `ensure_quotable` |
| `.../stream.rs` | `PriceLevelStreamBuilder` (three new knobs, one test seam), `build()` event loop, builder tests, loop tests |
| `.../test_support.rs` (new, `cfg(test)`) | Shared constants, token helpers, `FakeTitan` WebSocket server with `shutdown()`, `frame_text`, `wall_nanos_now` |
| `.../test_responses/pamm_price_levels_1788624558231060482.json` (new) | First raw frame of the 2026-09-05 capture |
| `crates/tycho-simulation/Cargo.toml`, workspace `Cargo.lock` | `metrics` optional dep under `price-level-stream`; `metrics-util` dev-dep |
| `crates/tycho-simulation/CLAUDE.md` | Updated `price_level_stream/` paragraph |

---

### Task 0: Branch and baseline

**Files:** none modified.

- [ ] **Step 1: Create the branch from an up-to-date main**

```bash
cd /home/dev/projects/propellerheads/tycho-indexer
git fetch origin
git switch -c tl/price-level-stream-freshness origin/main
```

- [ ] **Step 2: Capture the baseline once**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures; one ignored (`test_fetch_fallback_router_venues_against_mainnet`). Note the passing count for reference only.

- [ ] **Step 3: Install the pinned CI toolchain and confirm it works**

CI (`.github/workflows/ci-rust.yaml`) runs fmt and clippy on `nightly-2026-06-28`. Install that exact toolchain so every lint command in this plan matches CI:

```bash
rustup toolchain install nightly-2026-06-28 --component rustfmt --component clippy
rustup run nightly-2026-06-28 rustc --version
cargo +nightly-2026-06-28 fmt --check
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
```

Expected: the version line prints a `nightly` build dated 2026-06-27 or 2026-06-28, and both cargo commands exit 0.

---

### Task 1: Parse Titan's `timestamp`

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/titan.rs:64-74` (the `TitanPriceLevelMessage` struct) and its tests at `:254-275`
- Modify: `crates/tycho-simulation/src/price_level_stream/stream.rs:576-581` (`message()` test helper) and `:717` (`vanished_pamm_has_its_pairs_removed` literals)

**Interfaces:**
- Produces: `TitanPriceLevelMessage { block_number: u64, timestamp: u64, pamms: Vec<TitanPammLevels> }`; `timestamp` is nanoseconds since the Unix epoch and required.

- [ ] **Step 1: Write the failing tests in `titan.rs`**

Add to `parses_documented_sample_message` right after the `block_number` assertion:

```rust
        assert_eq!(message.timestamp, 1781801564588230787);
```

Add a new test in the same `mod tests`:

```rust
    /// The wire `timestamp` is the only per-frame freshness signal, so a frame without it is
    /// unusable and must not parse.
    #[test]
    fn rejects_frame_without_timestamp() {
        let json = r#"{"slot": 1, "blockNumber": 2, "pamms": []}"#;
        assert!(serde_json::from_str::<TitanPriceLevelMessage>(json).is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tycho-simulation --lib price_level_stream::titan`
Expected: compile error `no field timestamp`; once the field exists but before the test is right, `rejects_frame_without_timestamp` fails.

- [ ] **Step 3: Add the field**

```rust
/// A parsed price level stream frame: the quote ladders of every pAMM Titan simulated in one
/// build round, targeting the block currently being built.
///
/// Frames are best effort, not complete snapshots: a venue or pair can be absent from one frame
/// and present in the next (observed on 7.7% of frames in a 15 minute capture), so absence must
/// never be read as retirement.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanPriceLevelMessage {
    /// The L1 block number the quotes target (the block currently being built).
    pub block_number: u64,
    /// When Titan built this frame, in nanoseconds since the Unix epoch. Frames re-emitted
    /// within one build round share a timestamp, so it is a freshness marker, not an identity.
    pub timestamp: u64,
    /// Per-pAMM quote snapshots.
    pub pamms: Vec<TitanPammLevels>,
}
```

- [ ] **Step 4: Fix the `stream.rs` test helpers so the crate compiles**

`message()` gains `timestamp: 0,`; both literals in `vanished_pamm_has_its_pairs_removed` become `TitanPriceLevelMessage { block_number: 101, timestamp: 0, pamms: vec![] }` (and `102`).

- [ ] **Step 5: Run the module tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures; `rejects_frame_without_timestamp` and `parses_documented_sample_message` pass.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/titan.rs crates/tycho-simulation/src/price_level_stream/stream.rs
git commit -m "feat(simulation): parse the Titan price level frame timestamp"
```

---

### Task 2: Monotonic quote-time guard on `PriceLevelStreamState`

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/state.rs` (struct at `:37-44`, `new` at `:52-64`, `consumed` at `:123-131`, `ProtocolSim` impl at `:141-266`, tests from `:268`)

**Interfaces:**
- Produces: `pub const QUOTE_TTL: Duration = Duration::from_secs(12)`; `PriceLevelStreamState::quotable_until: Option<Instant>` (`#[serde(skip)]`, so never serialized and deserialized as `None`); `pub fn with_quotable_until(self, until: Instant) -> Self`; private `fn ensure_quotable(&self, now: Instant) -> Result<(), SimulationError>`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `state.rs` (add `use std::time::{Duration, Instant};` to the test imports):

```rust
    #[test]
    fn ensure_quotable_is_strict_at_the_boundary() {
        let until = Instant::now() + Duration::from_secs(60);
        let state = state().with_quotable_until(until);
        assert!(state
            .ensure_quotable(until - Duration::from_nanos(1))
            .is_ok());
        assert!(state.ensure_quotable(until).is_err());
        assert!(state
            .ensure_quotable(until + Duration::from_secs(1))
            .is_err());
    }

    #[test]
    fn state_without_a_guard_never_expires() {
        let far = Instant::now() + Duration::from_secs(1_000_000);
        assert!(state().ensure_quotable(far).is_ok());
    }

    #[test]
    fn expired_state_refuses_every_query() {
        // A guard set at construction time is already in the past by the time we query.
        let state = state().with_quotable_until(Instant::now());
        assert!(matches!(
            state.get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc()),
            Err(SimulationError::RecoverableError(_))
        ));
        assert!(matches!(state.spot_price(&wbtc(), &usdc()), Err(SimulationError::RecoverableError(_))));
        assert!(matches!(
            state.get_limits(wbtc().address, usdc().address),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    #[test]
    fn fresh_state_quotes_and_its_successor_keeps_the_guard() {
        let until = Instant::now() + Duration::from_secs(60);
        let state = state().with_quotable_until(until);
        assert!(state.spot_price(&wbtc(), &usdc()).is_ok());
        assert!(state
            .get_limits(wbtc().address, usdc().address)
            .is_ok());
        let result = state
            .get_amount_out(BigUint::from(100_000_000u64), &wbtc(), &usdc())
            .expect("fresh state quotes");
        let successor = result
            .new_state
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(successor.quotable_until, Some(until));
    }

    #[test]
    fn guard_is_never_serialized_and_deserializes_as_none() {
        let live = state().with_quotable_until(Instant::now());
        let json = serde_json::to_value(&live).unwrap();
        assert!(json
            .as_object()
            .unwrap()
            .get("quotable_until")
            .is_none());
        // A recording made after this feature therefore replays without a guard.
        let replayed: PriceLevelStreamState = serde_json::from_value(json).unwrap();
        assert_eq!(replayed.quotable_until, None);
        assert!(replayed.spot_price(&wbtc(), &usdc()).is_ok());
    }

    #[test]
    fn eq_includes_the_guard() {
        let until = Instant::now() + Duration::from_secs(60);
        assert!(state().eq(&state()));
        assert!(!state().eq(&state().with_quotable_until(until)));
        assert!(state()
            .with_quotable_until(until)
            .eq(&state().with_quotable_until(until)));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tycho-simulation --lib price_level_stream::state`
Expected: compile errors for `with_quotable_until`, `ensure_quotable`, `quotable_until`.

- [ ] **Step 3: Implement**

Add `use std::time::{Duration, Instant};` to the imports and, after them:

```rust
/// How long the ladders of one frame stay quotable: one block time. Titan quotes the pending
/// block, so a ladder older than that is not fillable directly (the venue reverts
/// `StaleUpdate`) and must not be priced. Anchored on the frame's wire `timestamp` at
/// acceptance and enforced on the monotonic clock; never derived from whether the ladder's
/// content changed, since quiet venues legitimately repeat a ladder for minutes.
pub const QUOTE_TTL: Duration = Duration::from_secs(12);
```

Change the struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevelStreamState {
    pub token0: Bytes,
    pub token1: Bytes,
    pub quotes_0_to_1: Vec<PriceLevelStreamQuote>,
    pub quotes_1_to_0: Vec<PriceLevelStreamQuote>,
    pub gas_cost: BigUint,
    /// Monotonic instant from which every query on this state is refused. A live, in-process
    /// property: never serialized, and `None` (never expires) after deserialization, so
    /// recordings replay as they always did.
    #[serde(skip)]
    pub quotable_until: Option<Instant>,
}
```

In `new`, build with `quotable_until: None`. Add to the inherent `impl`:

```rust
    /// Sets the monotonic instant from which every query on this state is refused.
    pub fn with_quotable_until(mut self, until: Instant) -> Self {
        self.quotable_until = Some(until);
        self
    }

    /// Errors once `now` has reached `quotable_until`.
    fn ensure_quotable(&self, now: Instant) -> Result<(), SimulationError> {
        match self.quotable_until {
            Some(until) if now >= until => Err(SimulationError::RecoverableError(
                "price levels expired: the frame that carried them is older than one block"
                    .to_string(),
            )),
            Some(_) | None => Ok(()),
        }
    }
```

In `consumed()`, add `quotable_until: self.quotable_until,` to the constructed struct.

In the `ProtocolSim` impl, add `self.ensure_quotable(Instant::now())?;` as the first line of
`spot_price`, `get_amount_out`, and `get_limits`.

In `eq`, destructure and compare the new field:

```rust
                let Self { token0, token1, quotes_0_to_1, quotes_1_to_0, gas_cost, quotable_until } =
                    other;
                &self.token0 == token0 &&
                    &self.token1 == token1 &&
                    &self.quotes_0_to_1 == quotes_0_to_1 &&
                    &self.quotes_1_to_0 == quotes_1_to_0 &&
                    &self.gas_cost == gas_cost &&
                    &self.quotable_until == quotable_until
```

The existing test `first_snapshot_emits_new_pair_with_both_directions` in `stream.rs`
destructures the state; add `quotable_until: _` to that pattern so it compiles.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures; the six new state tests pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/state.rs crates/tycho-simulation/src/price_level_stream/stream.rs
git commit -m "feat(simulation): refuse price level quotes older than one block"
```

---

### Task 3: Telemetry module and dependencies

**Files:**
- Modify: `crates/tycho-simulation/Cargo.toml` (dependencies near `:89-99`, `[dev-dependencies]` at `:101`, `price-level-stream` feature at `:121-127`); workspace `Cargo.lock` updates as a consequence
- Create: `crates/tycho-simulation/src/price_level_stream/telemetry.rs`
- Modify: `crates/tycho-simulation/src/price_level_stream/mod.rs:39-43` (add `mod telemetry;`)

**Interfaces:**
- Produces (all `pub(super)`): constants `FRAMES_ACCEPTED`, `FRAMES_REJECTED`, `LAST_SEEN`, `SERVED_COMPONENTS`, `STALE_REMOVALS`, `SOURCE_STATE`, `RECONNECTS`, `WHITELIST_READS`, `WHITELISTED_VENUES`, `UNREGISTERED_PAMM_FRAMES`; `enum SourceState { AwaitingWhitelist = 0, Unserved = 1, Serving = 2 }`; fns `frame_accepted()`, `frame_rejected(reason: &'static str)`, `last_seen(pamm: &str, unix_seconds: u64)`, `served_components(pamm: &str, count: usize)`, `stale_removal(pamm: &str)`, `source_state(state: SourceState)`, `reconnect(reason: &'static str)`, `whitelist_read(outcome: &'static str)`, `whitelisted_venues(count: usize)`, `unregistered_pamm()`; test helpers `test_support::{snapshot_map, counter_value, gauge_value}`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml` `[dependencies]`, next to `async-stream`:

```toml
metrics = { version = "0.24", optional = true }
```

In `[dev-dependencies]`:

```toml
metrics-util = { version = "0.20", features = ["debugging"] }
```

In the `price-level-stream` feature list add `"dep:metrics",` after `"dep:async-stream",`.

- [ ] **Step 2: Write the failing test**

Create `telemetry.rs` with only the test first:

```rust
#[cfg(test)]
mod tests {
    use metrics_util::debugging::DebuggingRecorder;

    use super::{test_support::*, *};

    #[test]
    fn every_helper_emits_its_named_metric() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            frame_accepted();
            frame_rejected("too_old");
            frame_rejected("too_old");
            last_seen("fermiswap", 1_700_000_000);
            served_components("fermiswap", 3);
            stale_removal("fermiswap");
            source_state(SourceState::Serving);
            reconnect("idle_timeout");
            whitelist_read("ok");
            whitelisted_venues(5);
            unregistered_pamm();
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, FRAMES_ACCEPTED, &[]), 1);
        assert_eq!(counter_value(&snapshot, FRAMES_REJECTED, &[("reason", "too_old")]), 2);
        assert_eq!(gauge_value(&snapshot, LAST_SEEN, &[("pamm", "fermiswap")]), 1_700_000_000.0);
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("pamm", "fermiswap")]), 3.0);
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("pamm", "fermiswap")]), 1);
        assert_eq!(gauge_value(&snapshot, SOURCE_STATE, &[]), 2.0);
        assert_eq!(counter_value(&snapshot, RECONNECTS, &[("reason", "idle_timeout")]), 1);
        assert_eq!(counter_value(&snapshot, WHITELIST_READS, &[("outcome", "ok")]), 1);
        assert_eq!(gauge_value(&snapshot, WHITELISTED_VENUES, &[]), 5.0);
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_FRAMES, &[]), 1);
    }
}
```

Add `mod telemetry;` to `mod.rs` after `mod titan;`.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p tycho-simulation --lib price_level_stream::telemetry`
Expected: compile errors, nothing is defined yet.

- [ ] **Step 4: Implement the module above the tests**

```rust
//! Metrics of the price level stream, emitted through the `metrics` facade. A consumer that
//! installs a recorder (Fynd, tycho-integration-test) sees them without any wiring; without a
//! recorder every call is a no-op. Label values are registered venue names or fixed
//! enumerations, never values from the wire.

use metrics::{counter, gauge};

pub(super) const FRAMES_ACCEPTED: &str = "price_level_stream_frames_accepted_total";
pub(super) const FRAMES_REJECTED: &str = "price_level_stream_frames_rejected_total";
pub(super) const LAST_SEEN: &str = "price_level_stream_last_seen_timestamp_seconds";
pub(super) const SERVED_COMPONENTS: &str = "price_level_stream_served_components";
pub(super) const STALE_REMOVALS: &str = "price_level_stream_stale_removals_total";
pub(super) const SOURCE_STATE: &str = "price_level_stream_source_state";
pub(super) const RECONNECTS: &str = "price_level_stream_reconnects_total";
pub(super) const WHITELIST_READS: &str = "price_level_stream_whitelist_reads_total";
pub(super) const WHITELISTED_VENUES: &str = "price_level_stream_whitelisted_venues";
pub(super) const UNREGISTERED_PAMM_FRAMES: &str = "price_level_stream_unregistered_pamm_frames_total";

/// The stream's serving state, exported as the numeric value of `SOURCE_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SourceState {
    /// The PropAMMRouter whitelist has not been read yet; nothing is served.
    AwaitingWhitelist = 0,
    /// Whitelist known (or not needed), no component currently served.
    Unserved = 1,
    /// At least one component is served.
    Serving = 2,
}

pub(super) fn frame_accepted() {
    counter!(FRAMES_ACCEPTED).increment(1);
}

pub(super) fn frame_rejected(reason: &'static str) {
    counter!(FRAMES_REJECTED, "reason" => reason).increment(1);
}

pub(super) fn last_seen(pamm: &str, unix_seconds: u64) {
    gauge!(LAST_SEEN, "pamm" => pamm.to_string()).set(unix_seconds as f64);
}

pub(super) fn served_components(pamm: &str, count: usize) {
    gauge!(SERVED_COMPONENTS, "pamm" => pamm.to_string()).set(count as f64);
}

pub(super) fn stale_removal(pamm: &str) {
    counter!(STALE_REMOVALS, "pamm" => pamm.to_string()).increment(1);
}

pub(super) fn source_state(state: SourceState) {
    gauge!(SOURCE_STATE).set(state as u8 as f64);
}

pub(super) fn reconnect(reason: &'static str) {
    counter!(RECONNECTS, "reason" => reason).increment(1);
}

pub(super) fn whitelist_read(outcome: &'static str) {
    counter!(WHITELIST_READS, "outcome" => outcome).increment(1);
}

pub(super) fn whitelisted_venues(count: usize) {
    gauge!(WHITELISTED_VENUES).set(count as f64);
}

pub(super) fn unregistered_pamm() {
    counter!(UNREGISTERED_PAMM_FRAMES).increment(1);
}

/// Reads a `DebuggingRecorder` snapshot back into plain values for assertions.
#[cfg(test)]
pub(super) mod test_support {
    use std::collections::{BTreeMap, HashMap};

    use metrics_util::{
        debugging::{DebugValue, Snapshot},
        MetricKind,
    };

    pub(in super::super) type SnapshotMap =
        HashMap<(MetricKind, String, BTreeMap<String, String>), DebugValue>;

    pub(in super::super) fn snapshot_map(snapshot: Snapshot) -> SnapshotMap {
        snapshot
            .into_vec()
            .into_iter()
            .map(|(composite_key, _unit, _description, value)| {
                let name = composite_key
                    .key()
                    .name()
                    .to_string();
                let labels = composite_key
                    .key()
                    .labels()
                    .map(|label| (label.key().to_string(), label.value().to_string()))
                    .collect();
                ((composite_key.kind(), name, labels), value)
            })
            .collect()
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    pub(in super::super) fn counter_value(
        snapshot: &SnapshotMap,
        name: &str,
        label_pairs: &[(&str, &str)],
    ) -> u64 {
        match snapshot.get(&(MetricKind::Counter, name.to_string(), labels(label_pairs))) {
            Some(DebugValue::Counter(value)) => *value,
            Some(DebugValue::Gauge(_)) | Some(DebugValue::Histogram(_)) | None => 0,
        }
    }

    pub(in super::super) fn gauge_value(
        snapshot: &SnapshotMap,
        name: &str,
        label_pairs: &[(&str, &str)],
    ) -> f64 {
        match snapshot.get(&(MetricKind::Gauge, name.to_string(), labels(label_pairs))) {
            Some(DebugValue::Gauge(value)) => value.into_inner(),
            Some(DebugValue::Counter(_)) | Some(DebugValue::Histogram(_)) | None => f64::NAN,
        }
    }
}
```

Note: `DebugValue::Gauge` wraps an `OrderedFloat<f64>`; `.into_inner()` unwraps it.

- [ ] **Step 5: Run the test**

Run: `cargo test -p tycho-simulation --lib price_level_stream::telemetry`
Expected: `every_helper_emits_its_named_metric` passes.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/Cargo.toml Cargo.lock crates/tycho-simulation/src/price_level_stream/telemetry.rs crates/tycho-simulation/src/price_level_stream/mod.rs
git commit -m "feat(simulation): add price level stream metrics facade"
```

---

### Task 4: Move `SnapshotTracker` into `tracker.rs` and share test helpers

**Files:**
- Create: `crates/tycho-simulation/src/price_level_stream/tracker.rs`
- Create: `crates/tycho-simulation/src/price_level_stream/test_support.rs`
- Modify: `crates/tycho-simulation/src/price_level_stream/stream.rs` (remove `:318-504`, the tracker, `build_component`, `component_id`; move tracker tests out)
- Modify: `crates/tycho-simulation/src/price_level_stream/mod.rs` (add `mod tracker;` and `#[cfg(test)] mod test_support;`)

**Interfaces:**
- Produces: `tracker::SnapshotTracker` (`pub(super)`), `SnapshotTracker::new(registry, denied, tokens, auto_detect, auto_detected_gas_cost, router_venues)` and `process(&mut self, TitanPriceLevelMessage) -> Option<Update>` unchanged in behaviour; `test_support::{PAMM, WBTC, USDC, WETH, token, tokens}`.

This is a mechanical move so that the later diffs stay readable. Behaviour must not change.

- [ ] **Step 1: Create `test_support.rs`**

```rust
//! Helpers shared by the price level stream tests.

use std::{collections::HashMap, str::FromStr};

use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

/// The FermiSwapper router, one of the default venues.
pub(super) const PAMM: &str = "0x5979458912f80b96d30d4220af8e2e4925a33320";
pub(super) const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
pub(super) const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub(super) const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

pub(super) fn token(address: &str, symbol: &str, decimals: u32) -> Token {
    Token::new(
        &Bytes::from_str(address).unwrap(),
        symbol,
        decimals,
        0,
        &[Some(10_000)],
        Chain::Ethereum,
        100,
    )
}

pub(super) fn tokens() -> HashMap<Bytes, Token> {
    [token(WBTC, "WBTC", 8), token(USDC, "USDC", 6), token(WETH, "WETH", 18)]
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect()
}
```

- [ ] **Step 2: Create `tracker.rs` by moving code**

Move verbatim from `stream.rs` into a new `tracker.rs`:

- the `SnapshotTracker` struct and its `impl` (`stream.rs:318-474`), making the struct, `new`, and `process` `pub(super)`;
- `build_component` (`:476-499`) and `component_id` (`:501-504`);
- these tests, verbatim, into `tracker.rs`'s `mod tests`: `first_snapshot_emits_new_pair_with_both_directions`, `repeated_snapshot_is_not_a_new_pair`, `dropped_pair_is_removed`, `out_of_order_frame_is_skipped`, `vanished_pamm_has_its_pairs_removed`, `unregistered_pamm_produces_no_update_without_auto_detection`, `denied_pamm_is_not_auto_detected`, `auto_detected_pamm_is_served_under_its_address`, `auto_detected_gas_cost_override_applies`, `whitelisted_venue_is_served_under_the_fallback_family`, `auto_detected_whitelisted_venue_is_served_under_the_fallback_family`, `unwhitelisted_venue_keeps_the_direct_family`, `unknown_tokens_are_skipped`;
- the test helpers `tracker()`, `level`, `pair_levels`, `message`, `wbtc_usdc_pairs`, `expected_id` (`:538-587`) into `tracker.rs`'s `mod tests`. Delete the `token`, `tokens` helpers and the `PAMM`/`WBTC`/`USDC`/`WETH` constants from `stream.rs` and import them from `test_support` in both files.

Header of the new file:

```rust
//! Turns Titan frames into [`Update`]s.

use std::collections::{hash_map::Entry, HashMap, HashSet};

use chrono::Utc;
use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use super::{
    config::PriceLevelStreamConfig,
    state::{PriceLevelStreamQuote, PriceLevelStreamState},
    stream::PAMM_ADDRESS_ATTRIBUTE,
    titan::{TitanPairLevels, TitanPammLevels, TitanPriceLevel, TitanPriceLevelMessage},
};
use crate::protocol::models::{ProtocolComponent, Update};
```

Test module header in `tracker.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        super::{config::DEFAULT_AUTO_DETECTED_GAS_COST, test_support::*},
        *,
    };
```

In `stream.rs`, replace the removed code with `use super::tracker::SnapshotTracker;` and drop the now-unused imports (`Entry`, `Utc`, `Chain`, `Token`, `ProtocolSim`, `PriceLevelStreamQuote`, `PriceLevelStreamState`, `TitanPairLevels`, `TitanPammLevels`, `TitanPriceLevel`, `TitanPriceLevelMessage`, `ProtocolComponent`). Its remaining tests (`explicit_add_and_deny_are_last_wins`, `defaults_never_override_explicit_calls`, `with_known_pamms_registers_known_venues`, `fallback_router_is_on_unless_opted_out`, `missing_rpc_url_leaves_every_venue_on_the_direct_path`, `failed_whitelist_read_leaves_every_venue_on_the_direct_path`, `families_match_the_execution_side_prefixes`) import `use super::{super::test_support::PAMM, *};` plus `use super::super::config::default_denied_pamms;`.

In `mod.rs` add:

```rust
mod tracker;
#[cfg(test)]
mod test_support;
```

- [ ] **Step 3: Verify nothing changed**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures, the same passing count as after Task 3, one ignored.

Run: `cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings`
Expected: clean (unused imports would fail here).

- [ ] **Step 4: Format and commit**

```bash
cargo +nightly-2026-06-28 fmt
git add crates/tycho-simulation/src/price_level_stream/
git commit -m "refactor(simulation): move the price level snapshot tracker to its own file"
```

---

### Task 5: Frame acceptance with an injected clock

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/tracker.rs`

**Interfaces:**
- Produces: `pub(super) struct Now { pub wall_nanos: u64, pub instant: Instant }` with `Now::current()`; constants `DEFAULT_STALE_AFTER = 24 s`, `MAX_FUTURE_SKEW_NANOS`, `SECONDS_PER_SLOT = 12`, `BLOCK_JUMP_SLACK = 2`, `NANOS_PER_SECOND`; `enum Rejection { TooOld, InFuture, OutOfOrder, BlockRegression, BlockJump }` with `as_str()`; `SnapshotTracker::new(registry, denied, tokens, auto_detect, auto_detected_gas_cost, router_venues, stale_after: Duration)`; `pub(super) fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update>` (replaces `process`); field `rejecting: bool` (rejection streak, for log throttling).
- Consumes: `telemetry::{frame_accepted, frame_rejected}`.

- [ ] **Step 1: Add the test clock helpers and the failing tests**

Replace `message()` and `tracker()` in `tracker.rs` tests and add a clock (test imports gain `use std::time::{Duration, Instant};`):

```rust
    /// 2026-09-05 16:09:18 UTC, the first frame of the live capture.
    const BASE_WALL_NANOS: u64 = 1_788_624_558_000_000_000;

    /// A test clock: wall and monotonic time both start at zero offset from a fixed base. Tests
    /// must only ever ask for non-decreasing seconds, mirroring a monotonic clock.
    struct Clock {
        start: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self { start: Instant::now() }
        }

        fn at(&self, seconds: u64) -> Now {
            Now {
                wall_nanos: BASE_WALL_NANOS + seconds * NANOS_PER_SECOND,
                instant: self.start + Duration::from_secs(seconds),
            }
        }
    }

    /// A frame built `seconds` after the base instant.
    fn message_at(
        block_number: u64,
        seconds: u64,
        pairs: Vec<TitanPairLevels>,
    ) -> TitanPriceLevelMessage {
        TitanPriceLevelMessage {
            block_number,
            timestamp: BASE_WALL_NANOS + seconds * NANOS_PER_SECOND,
            pamms: vec![TitanPammLevels { pamm: Bytes::from_str(PAMM).unwrap(), pairs }],
        }
    }

    fn message(block_number: u64, pairs: Vec<TitanPairLevels>) -> TitanPriceLevelMessage {
        message_at(block_number, 0, pairs)
    }

    fn tracker() -> SnapshotTracker {
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            HashSet::new(),
            DEFAULT_STALE_AFTER,
        )
    }
```

Every existing call `tracker.process(message(...))` becomes `tracker.on_frame(message(...), clock.at(0))` with `let clock = Clock::new();` at the top of the test; tests that send several frames at increasing blocks keep `clock.at(0)` for all of them (same-timestamp frames are legal). The other `SnapshotTracker::new(...)` literals in tests gain the trailing `DEFAULT_STALE_AFTER` argument.

Rewrite `out_of_order_frame_is_skipped` into `block_regression_is_rejected` and add the acceptance tests:

```rust
    #[test]
    fn block_regression_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(101, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let regressed =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        assert!(tracker
            .on_frame(message_at(100, 1, regressed), clock.at(1))
            .is_none());

        let update = tracker
            .on_frame(message_at(102, 2, wbtc_usdc_pairs()), clock.at(2))
            .expect("update expected");
        assert!(update.new_pairs.is_empty());
    }

    #[test]
    fn data_exactly_stale_after_old_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=0, judged at t=24: exactly the window, already stale.
        assert!(tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(24))
            .is_none());
        // One second younger is accepted.
        assert!(tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(24))
            .is_some());
    }

    #[test]
    fn frame_from_the_future_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        assert!(tracker
            .on_frame(message_at(100, 13, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
        assert!(tracker
            .on_frame(message_at(100, 12, wbtc_usdc_pairs()), clock.at(0))
            .is_some());
    }

    #[test]
    fn older_timestamp_frame_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(5))
            .expect("update expected");
        assert!(tracker
            .on_frame(message_at(100, 4, wbtc_usdc_pairs()), clock.at(5))
            .is_none());
    }

    #[test]
    fn equal_timestamp_frame_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(5))
            .expect("update expected");
        // Titan re-emits within a build round with the same timestamp and different content.
        let changed = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 101_000_000_000)]),
            pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
        ];
        let update = tracker
            .on_frame(message_at(100, 5, changed), clock.at(5))
            .expect("update expected");
        assert!(update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn block_jump_beyond_the_elapsed_bound_is_rejected() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // One second later the chain cannot have advanced by more than 0 + 2 blocks.
        assert!(tracker
            .on_frame(message_at(103, 1, wbtc_usdc_pairs()), clock.at(1))
            .is_none());
        assert!(tracker
            .on_frame(message_at(u64::MAX / 2, 1, wbtc_usdc_pairs()), clock.at(1))
            .is_none());
        // The tracker is not frozen: a plausible block is still accepted afterwards.
        assert!(tracker
            .on_frame(message_at(101, 2, wbtc_usdc_pairs()), clock.at(2))
            .is_some());
    }

    #[test]
    fn block_jump_within_the_elapsed_bound_is_accepted() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // 60 s later: 60 / 12 + 2 = 7 blocks allowed.
        assert!(tracker
            .on_frame(message_at(107, 60, wbtc_usdc_pairs()), clock.at(60))
            .is_some());
    }

    #[test]
    fn first_frame_skips_the_block_checks() {
        let clock = Clock::new();
        let mut tracker = tracker();
        assert!(tracker
            .on_frame(message_at(25_912_232, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_some());
    }

    #[test]
    fn rejections_are_counted_by_reason() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{counter_value, snapshot_map},
            FRAMES_ACCEPTED, FRAMES_REJECTED,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(5));
            // Still fresh (16 s old) but stamped before the accepted frame: out_of_order.
            tracker.on_frame(message_at(100, 4, wbtc_usdc_pairs()), clock.at(20));
            tracker.on_frame(message_at(100, 5, wbtc_usdc_pairs()), clock.at(29)); // too_old
            tracker.on_frame(message_at(100, 45, wbtc_usdc_pairs()), clock.at(29)); // in_future
            tracker.on_frame(message_at(99, 6, wbtc_usdc_pairs()), clock.at(29)); // block_regression
            tracker.on_frame(message_at(200, 6, wbtc_usdc_pairs()), clock.at(29)); // block_jump
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, FRAMES_ACCEPTED, &[]), 1);
        for reason in ["too_old", "in_future", "out_of_order", "block_regression", "block_jump"] {
            assert_eq!(
                counter_value(&snapshot, FRAMES_REJECTED, &[("reason", reason)]),
                1,
                "{reason}"
            );
        }
    }
```

Note on the ordering inside `rejections_are_counted_by_reason`: injected time only moves
forward (5, 20, 29, 29, 29, 29). The out-of-order frame is judged at t=20 so that it is still
younger than `stale_after` and reaches the order check; at t=29 the same stamp would be 25 s old
and counted as `too_old` instead. The frame stamped 6 s with block 99 passes age, future, and
order, then fails regression; the one with block 200 fails the jump bound (24 s elapsed allows
2 + 2 = 4 blocks).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tycho-simulation --lib price_level_stream::tracker`
Expected: compile errors (`Now`, `on_frame`, `DEFAULT_STALE_AFTER` undefined).

- [ ] **Step 3: Implement**

Add after the imports in `tracker.rs`:

```rust
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::telemetry;

/// How long the data of one frame may be served without a fresher frame carrying it: two block
/// times. Titan streams at 1 Hz and its frames arrive within about 3 s of being built, so this
/// leaves ample margin for jitter while bounding stale exposure.
pub(super) const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(24);

pub(super) const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Furthest a frame's `timestamp` may lie ahead of the local wall clock: one slot. Titan's
/// frames were observed at least 120 ms behind the local clock; anything ahead of it by more
/// than clock skew is implausible.
const MAX_FUTURE_SKEW_NANOS: u64 = 12 * NANOS_PER_SECOND;

/// Post-merge Ethereum slot duration in seconds.
const SECONDS_PER_SLOT: u64 = 12;

/// Extra blocks allowed beyond what elapsed time explains. Titan builds at chain head + 1 and
/// sometimes + 2, so a frame right after a block boundary may jump by two.
const BLOCK_JUMP_SLACK: u64 = 2;

/// The instants a tracker event is judged against.
#[derive(Clone, Copy, Debug)]
pub(super) struct Now {
    /// Wall clock, nanoseconds since the Unix epoch. Only ever compared against Titan's
    /// `timestamp`.
    pub wall_nanos: u64,
    /// Monotonic clock; drives every deadline, so a wall-clock jump cannot expire or freeze
    /// anything.
    pub instant: Instant,
}

impl Now {
    pub(super) fn current() -> Self {
        let wall_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|since_epoch| u64::try_from(since_epoch.as_nanos()).ok())
            .unwrap_or(0);
        Self { wall_nanos, instant: Instant::now() }
    }
}

/// Why a parsed frame was dropped before touching the served set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    /// Built `stale_after` ago or earlier.
    TooOld,
    /// Built more than one slot ahead of the local clock.
    InFuture,
    /// Older than the newest accepted frame.
    OutOfOrder,
    /// Targets a block below the newest accepted one.
    BlockRegression,
    /// Targets a block further ahead than elapsed time allows.
    BlockJump,
}

impl Rejection {
    fn as_str(self) -> &'static str {
        match self {
            Rejection::TooOld => "too_old",
            Rejection::InFuture => "in_future",
            Rejection::OutOfOrder => "out_of_order",
            Rejection::BlockRegression => "block_regression",
            Rejection::BlockJump => "block_jump",
        }
    }
}
```

Add fields to `SnapshotTracker` (replace the `newest_block` field and its doc):

```rust
    /// The data freshness window; see [`DEFAULT_STALE_AFTER`].
    stale_after: Duration,
    /// The `timestamp` of the newest accepted frame. Frames older than it are out of order;
    /// equal ones are re-emissions within a build round and accepted. Never reset.
    newest_timestamp: u64,
    /// The block of the newest accepted frame. Later frames may not regress below it, nor jump
    /// further ahead than the elapsed time explains. Reset with `last_accepted`.
    newest_block: u64,
    /// When the newest frame was accepted; `None` before the first one and after the source
    /// returns to unserved, which skips the block checks for the next frame.
    last_accepted: Option<Instant>,
    /// Whether the last frame was rejected: the first rejection of a streak logs at WARN, the
    /// rest at DEBUG, and the counter carries the rate.
    rejecting: bool,
```

Extend `new` with a trailing `stale_after: Duration` parameter and initialise
`stale_after, newest_timestamp: 0, newest_block: 0, last_accepted: None, rejecting: false`.

Add the acceptance method and rename `process` to `on_frame`:

```rust
    /// Checks a frame against the freshness and ordering rules, returning its age when it is
    /// acceptable.
    fn accept(&self, frame: &TitanPriceLevelMessage, now: Now) -> Result<Duration, Rejection> {
        let frame_age = Duration::from_nanos(now.wall_nanos.saturating_sub(frame.timestamp));
        if frame_age >= self.stale_after {
            return Err(Rejection::TooOld);
        }
        if frame.timestamp > now.wall_nanos.saturating_add(MAX_FUTURE_SKEW_NANOS) {
            return Err(Rejection::InFuture);
        }
        if frame.timestamp < self.newest_timestamp {
            return Err(Rejection::OutOfOrder);
        }
        let Some(last_accepted) = self.last_accepted else {
            return Ok(frame_age);
        };
        if frame.block_number < self.newest_block {
            return Err(Rejection::BlockRegression);
        }
        let elapsed_slots = now
            .instant
            .saturating_duration_since(last_accepted)
            .as_secs() /
            SECONDS_PER_SLOT;
        let allowed = self
            .newest_block
            .saturating_add(elapsed_slots)
            .saturating_add(BLOCK_JUMP_SLACK);
        if frame.block_number > allowed {
            return Err(Rejection::BlockJump);
        }
        Ok(frame_age)
    }

    /// Processes one frame into an [`Update`], or `None` if the frame is rejected (see
    /// [`Rejection`]) or carries nothing relevant.
    pub(super) fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update> {
        let frame_age = match self.accept(&frame, now) {
            Ok(frame_age) => frame_age,
            Err(rejection) => {
                self.log_rejection(rejection, &frame);
                telemetry::frame_rejected(rejection.as_str());
                return None;
            }
        };
        self.rejecting = false;
        telemetry::frame_accepted();
        self.newest_timestamp = frame.timestamp;
        self.newest_block = frame.block_number;
        self.last_accepted = Some(now.instant);
        let _ = frame_age;

        // ... the existing body of `process` from `let mut states` onward, unchanged ...
    }

    /// WARN for the first rejected frame of a streak, DEBUG for the rest.
    fn log_rejection(&mut self, rejection: Rejection, frame: &TitanPriceLevelMessage) {
        if self.rejecting {
            tracing::debug!(
                reason = rejection.as_str(),
                block_number = frame.block_number,
                timestamp = frame.timestamp,
                "Rejecting price level frame"
            );
            return;
        }
        self.rejecting = true;
        tracing::warn!(
            reason = rejection.as_str(),
            block_number = frame.block_number,
            timestamp = frame.timestamp,
            newest_block = self.newest_block,
            newest_timestamp = self.newest_timestamp,
            "Rejecting price level frame; further rejections logged at debug until a frame is \
             accepted"
        );
    }
```

Keep the rest of the old `process` body (the diff logic) below the new head for now; Task 6 replaces it. Remove the old `if message.block_number < self.newest_block` check and the `self.newest_block = message.block_number;` line, since `accept` now owns both. Rename `message` to `frame` inside the body.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures; the nine new tracker tests pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/tracker.rs
git commit -m "feat(simulation): validate price level frames by timestamp and block"
```

---

### Task 6: Per-component freshness, expiry, no diff-based removal

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/tracker.rs`

**Interfaces:**
- Produces: `struct Served { component: ProtocolComponent, venue: String, address: Bytes, deadline: Instant }`; `enum Source { AwaitingWhitelist, Unserved, Serving { deadline: Instant } }`; `SnapshotTracker::new(registry, denied, tokens, auto_detect, auto_detected_gas_cost, stale_after: Duration, fallback_router: bool)` (the `router_venues` parameter is gone; Task 7 delivers the whitelist through `on_router_venues`); `pub(super) fn on_stale_deadline(&mut self, now: Now) -> Option<Update>`; `pub(super) fn stale_deadline(&self) -> Option<Instant>`; `pub(super) fn set_router_venues_for_test(&mut self, venues: HashSet<Bytes>)` (cfg(test)).
- Consumes: `state::{QUOTE_TTL, PriceLevelStreamState::with_quotable_until}`, `telemetry::{last_seen, served_components, stale_removal, source_state, SourceState}`.

- [ ] **Step 1: Rewrite the removal tests and add the freshness tests**

In `tracker.rs` tests, update `tracker()` to the new constructor (`..., DEFAULT_STALE_AFTER, false)`) and every other `SnapshotTracker::new(...)` literal likewise. The three whitelist-family tests replace their `HashSet::from([...])` argument with a call after construction:

```rust
        let mut tracker = SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            false,
        );
        tracker.set_router_venues_for_test(HashSet::from([Bytes::from_str(PAMM).unwrap()]));
```

Delete `dropped_pair_is_removed` and `vanished_pamm_has_its_pairs_removed`. Add (test imports gain `use super::super::state::QUOTE_TTL;`):

```rust
    fn quotable_until(update: &Update) -> Instant {
        update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state")
            .quotable_until
            .expect("guard set")
    }

    #[test]
    fn quote_guard_is_one_block_after_the_frame_was_built() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=7, accepted at t=9: the data is 2 s old, so 10 s of quotability remain.
        let update = tracker
            .on_frame(message_at(100, 7, wbtc_usdc_pairs()), clock.at(9))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(9).instant + (QUOTE_TTL - Duration::from_secs(2)));
    }

    #[test]
    fn quote_guard_never_exceeds_one_block_for_a_future_stamped_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Maximum accepted skew: stamped 12 s ahead of the local clock.
        let update = tracker
            .on_frame(message_at(100, 12, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(0).instant + QUOTE_TTL);
    }

    #[test]
    fn frame_older_than_one_block_yields_an_unquotable_state() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // 15 s old: inside the 24 s window, past the 12 s quote guard.
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(15))
            .expect("update expected");
        assert_eq!(quotable_until(&update), clock.at(15).instant);
        assert_eq!(
            tracker.stale_deadline().expect("serving"),
            clock.at(24).instant
        );
    }

    #[test]
    fn omitted_pair_is_not_removed_by_the_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        let weth_usdc =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        let update = tracker
            .on_frame(message_at(101, 1, weth_usdc), clock.at(1))
            .expect("update expected");
        assert!(update.removed_pairs.is_empty());
        assert_eq!(update.new_pairs.len(), 1);
        assert_eq!(update.states.len(), 1);
        assert!(!update
            .states
            .contains_key(&expected_id()));
    }

    #[test]
    fn omitted_venue_is_not_removed_by_the_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        let empty = TitanPriceLevelMessage {
            block_number: 101,
            timestamp: BASE_WALL_NANOS + NANOS_PER_SECOND,
            pamms: vec![],
        };
        assert!(tracker
            .on_frame(empty, clock.at(1))
            .is_none());
        assert!(tracker.stale_deadline().is_some());
    }

    #[test]
    fn component_expires_at_its_own_deadline_and_is_re_added_by_the_next_frame() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );

        // One second early: nothing due.
        assert!(tracker
            .on_stale_deadline(clock.at(23))
            .is_none());

        let update = tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert_eq!(update.removed_pairs.len(), 1);
        assert!(update
            .removed_pairs
            .contains_key(&expected_id()));
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update.is_partial);
        assert!(tracker.stale_deadline().is_none());

        // Firing again with nothing served emits nothing.
        assert!(tracker
            .on_stale_deadline(clock.at(25))
            .is_none());

        let update = tracker
            .on_frame(message_at(102, 26, wbtc_usdc_pairs()), clock.at(26))
            .expect("update expected");
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn deadline_is_shortened_by_the_frame_age_at_acceptance() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // Built at t=0, accepted at t=3: the data is already 3 s old.
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(3))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );
    }

    #[test]
    fn replayed_frame_does_not_extend_the_deadline() {
        let clock = Clock::new();
        let mut tracker = tracker();
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // The same frame arriving again 10 s later carries the same timestamp.
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(10))
            .expect("update expected");
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(24).instant
        );
    }

    #[test]
    fn omitted_pair_expires_alone_while_the_rest_stays_served() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
            pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
        ];
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        // Only WETH/USDC keeps being carried, up to t=23; the clock never goes backwards.
        for second in 1..=23 {
            let weth_usdc = vec![pair_levels(
                WETH,
                USDC,
                vec![level(1_000_000_000_000_000_000, 3_000_000_000)],
            )];
            tracker.on_frame(message_at(100 + second / 12, second, weth_usdc), clock.at(second));
        }
        let update = tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        assert_eq!(update.removed_pairs.len(), 1);
        assert!(update
            .removed_pairs
            .contains_key(&expected_id()));
        // WETH/USDC is still served, with a deadline 24 s after its last frame at t=23.
        assert_eq!(
            tracker
                .stale_deadline()
                .expect("serving"),
            clock.at(47).instant
        );
    }

    #[test]
    fn components_from_one_frame_expire_together() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
        ];
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        let update = tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        assert_eq!(update.removed_pairs.len(), 2);
    }

    #[test]
    fn pair_set_oscillation_emits_no_removal() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let wide = || {
            vec![
                pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
                pair_levels(USDC, WBTC, vec![level(100_000_000_000, 99_000_000)]),
                pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
            ]
        };
        let narrow = || vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        tracker
            .on_frame(message_at(100, 0, wide()), clock.at(0))
            .expect("update expected");
        for second in 1..=10u64 {
            let pairs = if second % 2 == 0 { wide() } else { narrow() };
            let update = tracker
                .on_frame(message_at(100, second, pairs), clock.at(second))
                .expect("update expected");
            assert!(update.removed_pairs.is_empty(), "second {second}");
            assert!(update.new_pairs.is_empty(), "second {second}");
        }
    }

    #[test]
    fn poisoned_first_block_recovers_after_expiry() {
        let clock = Clock::new();
        let mut tracker = tracker();
        // A fresh first frame with an absurd block becomes the frontier.
        tracker
            .on_frame(message_at(u64::MAX / 2, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        // Sane frames regress below it and are rejected for the whole window.
        for second in 1..=23 {
            assert!(tracker
                .on_frame(message_at(100, second, wbtc_usdc_pairs()), clock.at(second))
                .is_none());
        }
        // Expiry clears the served set and resets the frontier.
        let removal = tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        assert_eq!(removal.removed_pairs.len(), 1);
        assert_eq!(removal.block_number_or_timestamp, u64::MAX / 2);
        // The next sane frame is accepted as a first frame again, and nothing from the
        // poisoned frame survives: the pair comes back as new.
        let update = tracker
            .on_frame(message_at(100, 25, wbtc_usdc_pairs()), clock.at(25))
            .expect("update expected");
        assert_eq!(update.block_number_or_timestamp, 100);
        assert!(update
            .new_pairs
            .contains_key(&expected_id()));
    }

    #[test]
    fn recovery_re_adds_only_the_pairs_the_frame_carries() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let both = vec![
            pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)]),
            pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)]),
        ];
        tracker
            .on_frame(message_at(100, 0, both), clock.at(0))
            .expect("update expected");
        tracker
            .on_stale_deadline(clock.at(24))
            .expect("removal expected");
        // After the outage only WETH/USDC comes back: WBTC/USDC stays absent.
        let weth_usdc =
            vec![pair_levels(WETH, USDC, vec![level(1_000_000_000_000_000_000, 3_000_000_000)])];
        let update = tracker
            .on_frame(message_at(102, 25, weth_usdc), clock.at(25))
            .expect("update expected");
        assert_eq!(update.new_pairs.len(), 1);
        assert!(!update
            .new_pairs
            .contains_key(&expected_id()));
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn one_direction_frame_re_adds_the_pair_with_the_other_direction_unquotable() {
        let clock = Clock::new();
        let mut tracker = tracker();
        let one_way = vec![pair_levels(WBTC, USDC, vec![level(100_000_000, 100_000_000_000)])];
        let update = tracker
            .on_frame(message_at(100, 0, one_way), clock.at(0))
            .expect("update expected");
        let state = update.states[&expected_id()]
            .as_any()
            .downcast_ref::<PriceLevelStreamState>()
            .expect("price level state");
        assert_eq!(state.quotes_0_to_1.len(), 1);
        assert!(state.quotes_1_to_0.is_empty());
        let usdc = token(USDC, "USDC", 6);
        let wbtc = token(WBTC, "WBTC", 8);
        assert!(state
            .get_amount_out(BigUint::from(1_000_000u64), &usdc, &wbtc)
            .is_err());
    }

    #[test]
    fn served_components_are_gauged_per_venue_from_zero() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{gauge_value, snapshot_map},
            LAST_SEEN, SERVED_COMPONENTS, SOURCE_STATE,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let tracker = tracker();
            drop(tracker);
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("pamm", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, LAST_SEEN, &[("pamm", "fermiswap")]), 0.0);
        assert_eq!(gauge_value(&snapshot, SOURCE_STATE, &[]), 1.0);

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = tracker();
            tracker.on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0));
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("pamm", "fermiswap")]), 1.0);
        assert_eq!(
            gauge_value(&snapshot, LAST_SEEN, &[("pamm", "fermiswap")]),
            (BASE_WALL_NANOS / NANOS_PER_SECOND) as f64
        );
        assert_eq!(gauge_value(&snapshot, SOURCE_STATE, &[]), 2.0);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tycho-simulation --lib price_level_stream::tracker`
Expected: compile errors (`on_stale_deadline`, `stale_deadline`, new constructor shape).

- [ ] **Step 3: Implement**

Add to `tracker.rs` after `Rejection`:

```rust
/// A component the stream currently vouches for.
struct Served {
    component: ProtocolComponent,
    /// The venue name, for logs and metric labels.
    venue: String,
    /// The venue address, for whitelist membership checks.
    address: Bytes,
    /// The instant this component's data turns `stale_after` old.
    deadline: Instant,
}

/// What the stream can currently vouch for.
enum Source {
    /// The PropAMMRouter whitelist has not been read yet: the family of every component is
    /// unknown, so frames are validated but nothing is emitted.
    AwaitingWhitelist,
    /// Whitelist known (or not needed) and nothing served: at start, and whenever the last
    /// served component expired.
    Unserved,
    /// At least one component is served; `deadline` is the earliest component deadline.
    Serving { deadline: Instant },
}

impl Source {
    fn telemetry_state(&self) -> telemetry::SourceState {
        match self {
            Source::AwaitingWhitelist => telemetry::SourceState::AwaitingWhitelist,
            Source::Unserved => telemetry::SourceState::Unserved,
            Source::Serving { deadline: _ } => telemetry::SourceState::Serving,
        }
    }
}
```

Replace the `components: HashMap<String, ProtocolComponent>` field with:

```rust
    /// The components currently served, keyed by component id.
    served: HashMap<String, Served>,
    source: Source,
```

Change `new`:

```rust
    pub(super) fn new(
        registry: HashMap<Bytes, PriceLevelStreamConfig>,
        denied: HashSet<Bytes>,
        tokens: HashMap<Bytes, Token>,
        auto_detect: bool,
        auto_detected_gas_cost: BigUint,
        stale_after: Duration,
        fallback_router: bool,
    ) -> Self {
        let source = if fallback_router { Source::AwaitingWhitelist } else { Source::Unserved };
        telemetry::source_state(source.telemetry_state());
        // Pre-initialise every per-venue series so a venue that never appears is a visible
        // zero, not a missing series.
        for config in registry.values() {
            telemetry::served_components(&config.protocol, 0);
            telemetry::last_seen(&config.protocol, 0);
        }
        Self {
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            router_venues: HashSet::new(),
            served: HashMap::new(),
            source,
            stale_after,
            newest_timestamp: 0,
            newest_block: 0,
            last_accepted: None,
            rejecting: false,
        }
    }

    #[cfg(test)]
    pub(super) fn set_router_venues_for_test(&mut self, venues: HashSet<Bytes>) {
        self.router_venues = venues;
    }
```

Replace the body of `on_frame` after `self.last_accepted = Some(now.instant);` (drop the `let _ = frame_age;` line) with:

```rust
        if let Source::AwaitingWhitelist = self.source {
            return None;
        }

        let deadline = now.instant + self.stale_after.saturating_sub(frame_age);
        let quotable_until = now.instant + QUOTE_TTL.saturating_sub(frame_age);
        let frame_unix_seconds = frame.timestamp / NANOS_PER_SECOND;
        let mut states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();
        let mut new_pairs = HashMap::new();

        for TitanPammLevels { pamm, pairs } in frame.pamms {
            let Some(config) = self.resolve_config(&pamm) else {
                continue;
            };
            telemetry::last_seen(&config.protocol, frame_unix_seconds);

            for ((token0, token1), (quotes_0_to_1, quotes_1_to_0)) in self.merge_pairs(pairs) {
                let id = component_id(&config.address, &token0, &token1);
                let id_string = id.to_string();
                let component = match self.served.get(&id_string) {
                    Some(served) => served.component.clone(),
                    None => {
                        let via_router = self
                            .router_venues
                            .contains(&config.address);
                        let component =
                            build_component(&self.tokens, &config, id, &token0, &token1, via_router);
                        new_pairs.insert(id_string.clone(), component.clone());
                        component
                    }
                };
                let state = PriceLevelStreamState::new(
                    token0,
                    token1,
                    quotes_0_to_1,
                    quotes_1_to_0,
                    config.gas_cost.clone(),
                )
                .with_quotable_until(quotable_until);
                states.insert(id_string.clone(), Box::new(state));
                self.served.insert(
                    id_string,
                    Served {
                        component,
                        venue: config.protocol.clone(),
                        address: config.address.clone(),
                        deadline,
                    },
                );
            }
        }

        if states.is_empty() {
            return None;
        }
        self.refresh_source();
        Some(Update::new(frame.block_number, states, new_pairs).set_is_partial(true))
    }

    /// The configuration a streamed venue is served under, or `None` when it is skipped.
    fn resolve_config(&mut self, pamm: &Bytes) -> Option<PriceLevelStreamConfig> {
        if let Some(config) = self.registry.get(pamm) {
            return Some(config.clone());
        }
        if self.denied.contains(pamm) {
            tracing::debug!(%pamm, "Skipping denied pAMM");
            return None;
        }
        if !self.auto_detect {
            tracing::debug!(%pamm, "Skipping unregistered pAMM");
            return None;
        }
        tracing::info!(%pamm, "Serving auto-detected pAMM");
        let config =
            PriceLevelStreamConfig::auto_detected(pamm.clone(), self.auto_detected_gas_cost.clone());
        telemetry::served_components(&config.protocol, 0);
        telemetry::last_seen(&config.protocol, 0);
        self.registry
            .insert(pamm.clone(), config.clone());
        Some(config)
    }

    /// Merges the frame's per-direction ladders into one entry per unordered token pair,
    /// skipping pairs with unknown tokens. A direction the frame does not carry yields an empty
    /// ladder, so that direction cannot be quoted.
    fn merge_pairs(
        &self,
        pairs: Vec<TitanPairLevels>,
    ) -> HashMap<(Bytes, Bytes), (Vec<PriceLevelStreamQuote>, Vec<PriceLevelStreamQuote>)> {
        let mut merged: HashMap<(Bytes, Bytes), (Vec<_>, Vec<_>)> = HashMap::new();
        for TitanPairLevels { token_in, token_out, order_book } in pairs {
            if !self.tokens.contains_key(&token_in) || !self.tokens.contains_key(&token_out) {
                tracing::debug!(%token_in, %token_out, "Skipping pair with unknown token");
                continue;
            }
            let sells_token0 = token_in < token_out;
            let key = if sells_token0 {
                (token_in.clone(), token_out.clone())
            } else {
                (token_out.clone(), token_in.clone())
            };
            let quotes = order_book
                .into_iter()
                .map(|TitanPriceLevel { amount_in, amount_out }| {
                    PriceLevelStreamQuote::new(amount_in, amount_out)
                })
                .collect();
            let entry = merged.entry(key).or_default();
            if sells_token0 {
                entry.0 = quotes;
            } else {
                entry.1 = quotes;
            }
        }
        merged
    }

    /// Removes every served component whose deadline has passed, as one [`Update`].
    pub(super) fn on_stale_deadline(&mut self, now: Now) -> Option<Update> {
        let due: Vec<String> = self
            .served
            .iter()
            .filter(|(_, served)| served.deadline <= now.instant)
            .map(|(id, _)| id.clone())
            .collect();
        if due.is_empty() {
            return None;
        }
        let mut removed = HashMap::with_capacity(due.len());
        let mut venues = BTreeSet::new();
        for id in due {
            let Some(served) = self.served.remove(&id) else {
                continue;
            };
            telemetry::stale_removal(&served.venue);
            venues.insert(served.venue);
            removed.insert(id, served.component);
        }
        let component_ids: Vec<&String> = removed.keys().collect();
        tracing::warn!(
            removed = removed.len(),
            venues = ?venues,
            components = ?component_ids,
            stale_after_secs = self.stale_after.as_secs(),
            "Removing price level components: no fresh frame carried them within the window"
        );
        self.refresh_source();
        Some(self.removal_update(removed))
    }

    /// The instant the earliest served component turns stale, if anything is served.
    pub(super) fn stale_deadline(&self) -> Option<Instant> {
        match self.source {
            Source::Serving { deadline } => Some(deadline),
            Source::AwaitingWhitelist | Source::Unserved => None,
        }
    }

    fn removal_update(&self, removed: HashMap<String, ProtocolComponent>) -> Update {
        Update::new(self.newest_block, HashMap::new(), HashMap::new())
            .set_is_partial(true)
            .set_removed_pairs(removed)
    }

    /// Recomputes the source state and the per-venue gauges from the served set. Returning to
    /// unserved also resets the block frontier, so one absurd block can never outlive the
    /// window it was served for.
    fn refresh_source(&mut self) {
        if let Source::AwaitingWhitelist = self.source {
            return;
        }
        self.source = match self
            .served
            .values()
            .map(|served| served.deadline)
            .min()
        {
            Some(deadline) => Source::Serving { deadline },
            None => {
                self.newest_block = 0;
                self.last_accepted = None;
                Source::Unserved
            }
        };
        telemetry::source_state(self.source.telemetry_state());
        let mut per_venue: HashMap<&str, usize> = self
            .registry
            .values()
            .map(|config| (config.protocol.as_str(), 0))
            .collect();
        for served in self.served.values() {
            *per_venue
                .entry(served.venue.as_str())
                .or_default() += 1;
        }
        for (venue, count) in per_venue {
            telemetry::served_components(venue, count);
        }
    }
```

Imports to add: `use std::collections::BTreeSet;`, `use super::state::QUOTE_TTL;`. Remove the `Entry` import. Delete the old diff code (`previous`, `removed_pairs`, the three-way `is_empty` guard).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures. Check specifically that `repeated_snapshot_is_not_a_new_pair`, `unknown_tokens_are_skipped`, and the three family tests still pass unchanged in meaning, and that the fifteen new tests pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/tracker.rs
git commit -m "feat(simulation): expire price level components per component"
```

---

### Task 7: Whitelist events and the bounded unregistered-venue signal

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/fallback_router.rs` (add `RouterVenuesRead`)
- Modify: `crates/tycho-simulation/src/price_level_stream/tracker.rs`

**Interfaces:**
- Produces: `pub(super) enum RouterVenuesRead { Ok(HashSet<Bytes>), Failed(FetchVenuesError) }` in `fallback_router.rs`; `SnapshotTracker::on_router_venues(&mut self, read: RouterVenuesRead) -> Option<Update>`; `seen_unregistered: HashSet<Bytes>` bounded by `MAX_UNREGISTERED_LOGGED = 64`.
- Consumes: `telemetry::{whitelisted_venues, unregistered_pamm}`.

- [ ] **Step 1: Write the failing tests in `tracker.rs`**

```rust
    fn read_ok(addresses: &[&str]) -> RouterVenuesRead {
        RouterVenuesRead::Ok(
            addresses
                .iter()
                .map(|address| Bytes::from_str(address).unwrap())
                .collect(),
        )
    }

    fn tracker_awaiting_whitelist() -> SnapshotTracker {
        let config = PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        );
        SnapshotTracker::new(
            HashMap::from([(config.address.clone(), config)]),
            HashSet::new(),
            tokens(),
            false,
            BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
            DEFAULT_STALE_AFTER,
            true,
        )
    }

    #[test]
    fn nothing_is_emitted_until_the_whitelist_is_known() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        assert!(tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
        assert!(tracker.stale_deadline().is_none());

        assert!(tracker
            .on_router_venues(read_ok(&[PAMM]))
            .is_none());
        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "propammfallback:fermiswap");
    }

    #[test]
    fn failed_whitelist_read_keeps_the_previous_set() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(read_ok(&[PAMM]));
        assert!(tracker
            .on_router_venues(RouterVenuesRead::Failed(FetchVenuesError::Call {
                reason: "boom".to_string(),
            }))
            .is_none());
        let update = tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "propammfallback:fermiswap");
    }

    #[test]
    fn failed_first_read_keeps_waiting() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(RouterVenuesRead::Failed(FetchVenuesError::Call {
            reason: "boom".to_string(),
        }));
        assert!(tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .is_none());
    }

    #[test]
    fn family_change_removes_now_and_re_adds_under_the_new_family() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(read_ok(&[PAMM]));
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");

        // The venue gets de-whitelisted.
        let update = tracker
            .on_router_venues(read_ok(&[]))
            .expect("removal expected");
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert_eq!(update.removed_pairs[&expected_id()].protocol_system, "propammfallback:fermiswap");
        assert!(tracker.stale_deadline().is_none());

        let update = tracker
            .on_frame(message_at(100, 1, wbtc_usdc_pairs()), clock.at(1))
            .expect("update expected");
        assert_eq!(update.new_pairs[&expected_id()].protocol_system, "pricelevelstream:fermiswap");
        assert!(update.removed_pairs.is_empty());
    }

    #[test]
    fn unchanged_whitelist_emits_nothing() {
        let clock = Clock::new();
        let mut tracker = tracker_awaiting_whitelist();
        tracker.on_router_venues(read_ok(&[PAMM]));
        tracker
            .on_frame(message_at(100, 0, wbtc_usdc_pairs()), clock.at(0))
            .expect("update expected");
        assert!(tracker
            .on_router_venues(read_ok(&[PAMM]))
            .is_none());
    }

    #[test]
    fn unregistered_venues_are_counted_without_labels_and_logged_boundedly() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{counter_value, snapshot_map},
            UNREGISTERED_PAMM_FRAMES,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let logged = metrics::with_local_recorder(&recorder, || {
            let clock = Clock::new();
            let mut tracker = SnapshotTracker::new(
                HashMap::new(),
                HashSet::new(),
                tokens(),
                false,
                BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST),
                DEFAULT_STALE_AFTER,
                false,
            );
            // 70 distinct unregistered addresses, each seen twice.
            for round in 0..2u64 {
                for index in 0..70u64 {
                    let address = Bytes::from(format!("{index:040x}").as_bytes().to_vec());
                    let frame = TitanPriceLevelMessage {
                        block_number: 100,
                        timestamp: BASE_WALL_NANOS + (round * 70 + index) * 1_000,
                        pamms: vec![TitanPammLevels {
                            pamm: address,
                            pairs: wbtc_usdc_pairs(),
                        }],
                    };
                    tracker.on_frame(frame, clock.at(1));
                }
            }
            tracker.seen_unregistered.len()
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_FRAMES, &[]), 140);
        assert_eq!(logged, MAX_UNREGISTERED_LOGGED);
    }
```

Add to the test imports: `use super::super::fallback_router::{FetchVenuesError, RouterVenuesRead};`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tycho-simulation --lib price_level_stream::tracker`
Expected: compile errors.

- [ ] **Step 3: Implement**

In `fallback_router.rs`, after `FetchVenuesError`:

```rust
/// The outcome of one whitelist read, as delivered to the tracker.
#[derive(Debug)]
pub(super) enum RouterVenuesRead {
    Ok(std::collections::HashSet<Bytes>),
    Failed(FetchVenuesError),
}
```

In `tracker.rs`, add `use super::fallback_router::RouterVenuesRead;`, the constant and field:

```rust
/// How many distinct unregistered venue addresses get a first-sight INFO line per process.
/// Addresses come from an external source; the set is bounded so a misbehaving upstream cannot
/// grow memory, and they never become metric labels.
const MAX_UNREGISTERED_LOGGED: usize = 64;
```

```rust
    /// Unregistered venue addresses already logged, at most [`MAX_UNREGISTERED_LOGGED`].
    seen_unregistered: HashSet<Bytes>,
```

(initialised empty in `new`), and:

```rust
    /// Applies a whitelist read. A failed read keeps the last known set (the reader already
    /// logged it). A successful read that changes a served venue's family removes that venue's
    /// components now; the next accepted frame carrying them re-adds them under the new family.
    pub(super) fn on_router_venues(&mut self, read: RouterVenuesRead) -> Option<Update> {
        let venues = match read {
            RouterVenuesRead::Failed(error) => {
                tracing::debug!(error = %error, "Whitelist read failed; whitelist unchanged");
                return None;
            }
            RouterVenuesRead::Ok(venues) => venues,
        };
        telemetry::whitelisted_venues(venues.len());
        let moved: Vec<String> = self
            .served
            .iter()
            .filter(|(_, served)| {
                self.router_venues
                    .contains(&served.address) !=
                    venues.contains(&served.address)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.router_venues = venues;

        if let Source::AwaitingWhitelist = self.source {
            tracing::info!(
                venues = self.router_venues.len(),
                "PropAMMRouter venue whitelist read; serving pAMMs from the next frame"
            );
            self.source = Source::Unserved;
            telemetry::source_state(self.source.telemetry_state());
            return None;
        }
        if moved.is_empty() {
            return None;
        }
        let mut removed = HashMap::with_capacity(moved.len());
        for id in moved {
            let Some(served) = self.served.remove(&id) else {
                continue;
            };
            tracing::info!(
                venue = %served.venue,
                "pAMM changed PropAMMRouter whitelist membership; re-adding it under its new \
                 family on the next frame"
            );
            removed.insert(id, served.component);
        }
        self.refresh_source();
        Some(self.removal_update(removed))
    }
```

In `resolve_config`, replace the `!self.auto_detect` branch:

```rust
        if !self.auto_detect {
            telemetry::unregistered_pamm();
            if self.seen_unregistered.len() < MAX_UNREGISTERED_LOGGED &&
                self.seen_unregistered.insert(pamm.clone())
            {
                tracing::info!(
                    %pamm,
                    "Skipping unregistered pAMM; register it via add_pamm to serve it"
                );
            }
            return None;
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures; the six new tests pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/tracker.rs crates/tycho-simulation/src/price_level_stream/fallback_router.rs
git commit -m "feat(simulation): move pAMM components between families on whitelist change"
```

---

### Task 8: Whitelist reader with timeout, retry, and refresh

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/fallback_router.rs`
- Modify: `crates/tycho-simulation/src/price_level_stream/titan.rs:56-63` (`backoff` becomes `pub(super)`)

**Interfaces:**
- Produces: `FetchVenuesError::Timeout { after: Duration }`; `pub(super) const WHITELIST_READ_TIMEOUT: Duration = Duration::from_secs(15)`; `pub(super) fn router_venues_reader<F, Fut>(fetch: F, read_timeout: Duration, max_backoff: Duration, refresh_interval: Duration) -> impl Stream<Item = RouterVenuesRead> + Send where F: Fn() -> Fut + Send + 'static, Fut: Future<Output = Result<Vec<Bytes>, FetchVenuesError>> + Send`.
- Consumes: `titan::backoff`, `telemetry::whitelist_read`.

- [ ] **Step 1: Write the failing tests in `fallback_router.rs`**

```rust
    #[tokio::test]
    async fn reader_retries_failures_and_refreshes_after_success() {
        use std::{
            collections::VecDeque,
            sync::{Arc, Mutex},
            time::Duration,
        };

        use futures::StreamExt;

        let venue = Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap();
        let script: Arc<Mutex<VecDeque<Result<Vec<Bytes>, FetchVenuesError>>>> =
            Arc::new(Mutex::new(VecDeque::from([
                Err(FetchVenuesError::Call { reason: "first".to_string() }),
                Err(FetchVenuesError::Call { reason: "second".to_string() }),
                Ok(vec![venue.clone()]),
                Ok(vec![]),
            ])));
        let fetch = {
            let script = script.clone();
            move || {
                let next = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok(vec![]));
                async move { next }
            }
        };
        let reader = router_venues_reader(
            fetch,
            Duration::from_secs(1),
            Duration::from_millis(5),
            Duration::from_millis(20),
        );
        tokio::pin!(reader);

        assert!(matches!(reader.next().await, Some(RouterVenuesRead::Failed(_))));
        assert!(matches!(reader.next().await, Some(RouterVenuesRead::Failed(_))));
        match reader.next().await {
            Some(RouterVenuesRead::Ok(venues)) => assert_eq!(venues.len(), 1),
            other => panic!("expected a successful read, got {other:?}"),
        }
        // Refreshed after the interval.
        match tokio::time::timeout(Duration::from_millis(500), reader.next()).await {
            Ok(Some(RouterVenuesRead::Ok(venues))) => assert!(venues.is_empty()),
            other => panic!("expected a refresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reader_fails_a_read_that_never_resolves() {
        use std::{
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
            time::Duration,
        };

        use futures::StreamExt;

        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<Vec<Bytes>, FetchVenuesError>>()
            }
        };
        let reader = router_venues_reader(
            fetch,
            Duration::from_millis(30),
            Duration::from_millis(5),
            Duration::from_secs(60),
        );
        tokio::pin!(reader);

        // Two consecutive timeouts prove the read is bounded and retried.
        for _ in 0..2 {
            match tokio::time::timeout(Duration::from_millis(500), reader.next()).await {
                Ok(Some(RouterVenuesRead::Failed(FetchVenuesError::Timeout { after }))) => {
                    assert_eq!(after, Duration::from_millis(30));
                }
                other => panic!("expected a timeout, got {other:?}"),
            }
        }
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tycho-simulation --lib price_level_stream::fallback_router`
Expected: compile errors, `router_venues_reader` and `Timeout` undefined.

- [ ] **Step 3: Implement**

In `titan.rs`, change `fn backoff(` to `pub(super) fn backoff(`.

In `fallback_router.rs`, add the error variant:

```rust
    /// The `eth_call` did not resolve within the read timeout.
    #[error("getWhitelistedVenues call to the PropAMMRouter timed out after {after:?}")]
    Timeout {
        /// The bound that elapsed.
        after: Duration,
    },
```

Add imports `use std::{collections::HashSet, future::Future, time::Duration};`, `use async_stream::stream;`, `use futures::Stream;`, `use tokio::time::{sleep, timeout};`, `use super::{telemetry, titan::backoff};`, and:

```rust
/// Longest a single whitelist read may take. A read that hangs past this is a failure like any
/// other, so a node that accepts the connection and never answers cannot keep the stream in
/// its pre-whitelist state, or freeze a stale whitelist, forever.
pub(super) const WHITELIST_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Reads the whitelist through `fetch`, yielding every outcome: a read is bounded by
/// `read_timeout`; failures (including timeouts) are retried with exponential backoff
/// (`2^attempt` seconds, capped at `max_backoff`); successes are repeated every
/// `refresh_interval`. Never ends. Logs one WARN per failure; consumers of the outcomes should
/// not log them again.
pub(super) fn router_venues_reader<F, Fut>(
    fetch: F,
    read_timeout: Duration,
    max_backoff: Duration,
    refresh_interval: Duration,
) -> impl Stream<Item = RouterVenuesRead> + Send
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<Bytes>, FetchVenuesError>> + Send,
{
    stream! {
        let mut attempt: u32 = 0;
        loop {
            let outcome = match timeout(read_timeout, fetch()).await {
                Ok(outcome) => outcome,
                Err(_elapsed) => Err(FetchVenuesError::Timeout { after: read_timeout }),
            };
            match outcome {
                Ok(venues) => {
                    attempt = 0;
                    telemetry::whitelist_read("ok");
                    let venues: HashSet<Bytes> = venues.into_iter().collect();
                    yield RouterVenuesRead::Ok(venues);
                    sleep(refresh_interval).await;
                }
                Err(error) => {
                    attempt = attempt.saturating_add(1);
                    telemetry::whitelist_read("error");
                    let delay = backoff(attempt, max_backoff);
                    tracing::warn!(
                        error = %error,
                        attempt,
                        retry_secs = delay.as_secs_f64(),
                        "PropAMMRouter whitelist read failed; retrying"
                    );
                    yield RouterVenuesRead::Failed(error);
                    sleep(delay).await;
                }
            }
        }
    }
}
```

Update the docstring of `fetch_fallback_router_venues`: replace the "Read once at startup" paragraph with "Read at startup with retries and refreshed periodically by [`router_venues_reader`], each read bounded by [`WHITELIST_READ_TIMEOUT`]."

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures; both reader tests pass.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/fallback_router.rs crates/tycho-simulation/src/price_level_stream/titan.rs
git commit -m "feat(simulation): bound, retry, and refresh the PropAMMRouter whitelist read"
```

---

### Task 9: Transport liveness counts parsed frames only

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/titan.rs` (`ConnectionSettings` default at `:44-51`, `messages` at `:117-207`)
- Modify: `crates/tycho-simulation/src/price_level_stream/test_support.rs` (add `FakeTitan`, `frame_text`, `wall_nanos_now`)

**Interfaces:**
- Produces: `ConnectionSettings::default().read_idle_timeout == 10 s`; `test_support::FakeTitan::spawn(handler).await`, `FakeTitan::url()`, `FakeTitan::shutdown(&mut self)` (stops accepting: later connects are refused), `FakeTitan::connections: Arc<AtomicUsize>`; `test_support::frame_text(block: u64, timestamp: u64) -> String`; `test_support::wall_nanos_now() -> u64`.
- Consumes: `telemetry::{reconnect, frame_rejected}`.

- [ ] **Step 1: Add the fake server to `test_support.rs`**

```rust
use std::{
    future::Future,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use futures::future::BoxFuture;
use tokio::{net::{TcpListener, TcpStream}, task::JoinHandle};
use tokio_tungstenite::WebSocketStream;

pub(super) type FakeConnection = WebSocketStream<TcpStream>;

/// A scripted stand-in for Titan's WebSocket endpoint on `127.0.0.1`. Every accepted
/// connection runs `handler(connection_index, socket)`.
pub(super) struct FakeTitan {
    addr: SocketAddr,
    pub(super) connections: Arc<AtomicUsize>,
    accept_task: Option<JoinHandle<()>>,
}

impl FakeTitan {
    pub(super) async fn spawn<H, Fut>(handler: H) -> Self
    where
        H: Fn(usize, FakeConnection) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        let handler: Arc<dyn Fn(usize, FakeConnection) -> BoxFuture<'static, ()> + Send + Sync> =
            Arc::new(move |index, socket| Box::pin(handler(index, socket)));
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                tokio::spawn(async move {
                    let Ok(socket) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    handler(index, socket).await;
                });
            }
        });
        Self { addr, connections, accept_task: Some(accept_task) }
    }

    pub(super) fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Stops accepting: the listener is dropped, so every later connect is refused at TCP
    /// level. Connections already handed to a handler keep running.
    pub(super) fn shutdown(&mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

impl Drop for FakeTitan {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Nanoseconds since the Unix epoch, for building frames Titan would stamp right now.
pub(super) fn wall_nanos_now() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos(),
    )
    .expect("fits")
}

/// A valid frame carrying the FermiSwap WBTC/USDC ladder in both directions.
pub(super) fn frame_text(block: u64, timestamp: u64) -> String {
    format!(
        r#"{{"slot": 1, "blockNumber": {block}, "timestamp": {timestamp}, "pamms": [{{"pamm": "{PAMM}", "maker": 12, "pairs": [
            {{"tokenIn": "{WBTC}", "tokenOut": "{USDC}", "orderBook": [{{"amountIn": "0x5f5e100", "amountOut": "0x174876e800", "variant": "Simulated"}}]}},
            {{"tokenIn": "{USDC}", "tokenOut": "{WBTC}", "orderBook": [{{"amountIn": "0x174876e800", "amountOut": "0x5e69ec0", "variant": "Simulated"}}]}}
        ]}}]}}"#
    )
}
```

Note: aborting the accept task drops the listener it owns, which is what turns later connects into a refusal.

- [ ] **Step 2: Write the failing liveness tests in `titan.rs`**

```rust
    #[tokio::test]
    async fn ping_only_traffic_does_not_count_as_liveness() {
        use std::{sync::atomic::Ordering, time::Duration};

        use futures::SinkExt;

        use super::super::test_support::{frame_text, wall_nanos_now, FakeTitan};

        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
            loop {
                if socket
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let settings = ConnectionSettings {
            connect_timeout: Duration::from_secs(1),
            read_idle_timeout: Duration::from_millis(100),
            max_backoff: Duration::from_millis(10),
        };
        let stream = messages(fake.url(), settings);
        tokio::pin!(stream);
        assert!(tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("first frame")
            .is_some());
        // Keep polling so the idle watchdog runs; pings must not feed it.
        let _ = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on ping-only traffic");
    }

    #[tokio::test]
    async fn malformed_text_does_not_count_as_liveness() {
        use std::{sync::atomic::Ordering, time::Duration};

        use futures::SinkExt;

        use super::super::test_support::{frame_text, wall_nanos_now, FakeTitan};

        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
            loop {
                if socket
                    .send(Message::Text("nonsense".into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let settings = ConnectionSettings {
            connect_timeout: Duration::from_secs(1),
            read_idle_timeout: Duration::from_millis(100),
            max_backoff: Duration::from_millis(10),
        };
        let stream = messages(fake.url(), settings);
        tokio::pin!(stream);
        assert!(tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("first frame")
            .is_some());
        let _ = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "no reconnect on malformed text");
    }

    #[tokio::test]
    async fn parsed_frames_keep_the_connection_alive() {
        use std::{sync::atomic::Ordering, time::Duration};

        use futures::SinkExt;

        use super::super::test_support::{frame_text, wall_nanos_now, FakeTitan};

        let fake = FakeTitan::spawn(|_, mut socket| async move {
            loop {
                if socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        let settings = ConnectionSettings {
            connect_timeout: Duration::from_secs(1),
            read_idle_timeout: Duration::from_millis(150),
            max_backoff: Duration::from_millis(10),
        };
        let stream = messages(fake.url(), settings);
        tokio::pin!(stream);
        let mut received = 0;
        while received < 10 {
            assert!(tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("frame")
                .is_some());
            received += 1;
        }
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_idle_timeout_is_ten_seconds() {
        assert_eq!(ConnectionSettings::default().read_idle_timeout, Duration::from_secs(10));
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p tycho-simulation --lib price_level_stream::titan`
Expected: `default_idle_timeout_is_ten_seconds` fails (30 s); the two "does not count" tests fail on the connection count.

- [ ] **Step 4: Implement**

Change the default and its doc:

```rust
    /// Longest gap between *parsed* Titan frames tolerated before the socket is treated as dead
    /// and re-established. Titan pushes one frame per second and sends no keepalives, so a
    /// half-open socket is indistinguishable from silence; ten seconds catches it well before
    /// any served component expires. Pings, binary frames, and unparsable text do not count.
    pub read_idle_timeout: Duration,
```

with `read_idle_timeout: Duration::from_secs(10)` in `Default`.

Rewrite the inner read loop of `messages`:

```rust
                Ok(Ok((mut ws_stream, _))) => {
                    info!(%url, "Connected to Titan pAMM price level stream");
                    let mut last_parsed = Instant::now();
                    loop {
                        // Liveness is measured from the last parsed frame, so control frames and
                        // garbage cannot keep a data-silent socket alive.
                        let remaining = settings
                            .read_idle_timeout
                            .saturating_sub(last_parsed.elapsed());
                        if remaining.is_zero() {
                            warn!(
                                idle_secs = settings.read_idle_timeout.as_secs(),
                                "No parsed Titan frame within idle timeout; reconnecting"
                            );
                            telemetry::reconnect("idle_timeout");
                            break;
                        }
                        let message = match timeout(remaining, ws_stream.next()).await {
                            Ok(Some(message)) => message,
                            Ok(None) => {
                                warn!("Titan price level stream ended; reconnecting");
                                telemetry::reconnect("ended");
                                break;
                            }
                            Err(_elapsed) => {
                                warn!(
                                    idle_secs = settings.read_idle_timeout.as_secs(),
                                    "No parsed Titan frame within idle timeout; reconnecting"
                                );
                                telemetry::reconnect("idle_timeout");
                                break;
                            }
                        };

                        match message {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<TitanPriceLevelMessage>(text.as_str())
                                {
                                    // A parsed frame proves the connection is healthy: reset
                                    // both the reconnect backoff and the idle watchdog.
                                    Ok(message) => {
                                        attempt = 0;
                                        last_parsed = Instant::now();
                                        yield message;
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Failed to parse Titan price level message");
                                        telemetry::frame_rejected("parse_error");
                                    }
                                }
                            }
                            Ok(Message::Binary(bytes)) => {
                                warn!(len = bytes.len(), "Ignoring unexpected binary Titan frame");
                            }
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                            Ok(Message::Frame(_)) => {}
                            Ok(Message::Close(frame)) => {
                                warn!(?frame, "Titan price level stream closed by server; reconnecting");
                                telemetry::reconnect("closed");
                                break;
                            }
                            Err(e) => {
                                warn!(error = %e, "Titan price level stream read error; reconnecting");
                                telemetry::reconnect("read_error");
                                break;
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "Failed to connect to Titan price level stream; retrying");
                    telemetry::reconnect("connect_failed");
                }
                Err(_elapsed) => {
                    warn!(
                        timeout_secs = settings.connect_timeout.as_secs(),
                        "Titan price level connect timed out; retrying"
                    );
                    telemetry::reconnect("connect_timeout");
                }
```

Add `use std::time::Instant;` and `use super::telemetry;`. Keep the existing comments on the `Ping`/`Pong`/`Frame` arms.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures. Run the liveness tests three times in a row to check for flakiness: `for i in 1 2 3; do cargo test -p tycho-simulation --lib price_level_stream::titan -- --test-threads=1 || exit 1; done`.

- [ ] **Step 6: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/titan.rs crates/tycho-simulation/src/price_level_stream/test_support.rs
git commit -m "fix(simulation): count only parsed Titan frames as liveness"
```

---

### Task 10: The event loop and the builder knobs

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/stream.rs` (builder struct, `Default`, setters, `build`, `fetch_router_venues` removed, tests)

**Interfaces:**
- Consumes: `tracker::{SnapshotTracker, Now, DEFAULT_STALE_AFTER}`, `fallback_router::{fetch_fallback_router_venues, router_venues_reader, RouterVenuesRead, WHITELIST_READ_TIMEOUT}`, `telemetry::{source_state, SourceState}`, `test_support::{FakeTitan, frame_text, wall_nanos_now, tokens, PAMM}`.
- Produces: builder fields `stale_after: Duration`, `whitelist_refresh_interval: Duration`, `fallback_router_rpc_url: Option<String>`, `env_rpc_url: bool`; public setters `stale_after(Duration)`, `whitelist_refresh_interval(Duration)`, `fallback_router_rpc_url(impl Into<String>)`; `#[cfg(test)] fn without_env_rpc_url(self) -> Self`; `pub(super) const DEFAULT_WHITELIST_REFRESH_INTERVAL: Duration = Duration::from_secs(600)`.

- [ ] **Step 1: Delete the two fail-open tests and write the loop tests**

Delete `missing_rpc_url_leaves_every_venue_on_the_direct_path` and
`failed_whitelist_read_leaves_every_venue_on_the_direct_path`. Add:

```rust
    fn fermiswap() -> PriceLevelStreamConfig {
        PriceLevelStreamConfig::new(
            "fermiswap",
            Bytes::from_str(PAMM).unwrap(),
            BigUint::from(120_000u64),
        )
    }

    /// Short windows: 300 ms of data freshness, a 100 ms idle watchdog, 20 ms backoff cap.
    fn fast_builder(fake: &FakeTitan) -> PriceLevelStreamBuilder {
        PriceLevelStreamBuilder::new()
            .endpoint(fake.url())
            .without_fallback_router()
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .stale_after(Duration::from_millis(300))
            .connect_timeout(Duration::from_secs(1))
            .read_idle_timeout(Duration::from_millis(100))
            .max_backoff(Duration::from_millis(20))
    }

    async fn next_within(
        stream: &mut Pin<&mut impl Stream<Item = Update>>,
        limit: Duration,
    ) -> Option<Update> {
        tokio::time::timeout(limit, stream.next())
            .await
            .ok()
            .flatten()
    }

    fn assert_removal_only(update: &Update, expected_removed: usize) {
        assert!(update.states.is_empty());
        assert!(update.new_pairs.is_empty());
        assert!(update.sync_states.is_empty());
        assert!(update.is_partial);
        assert_eq!(update.removed_pairs.len(), expected_removed);
    }

    /// Sends one fresh frame on the first connection only; later connections stay silent.
    async fn first_connection_sends_one_frame(index: usize, mut socket: FakeConnection) {
        if index == 0 {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
        }
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn silence_past_stale_after_removes_every_served_component() {
        let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        let first = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        assert_eq!(first.new_pairs.len(), 1);
        assert!(first.removed_pairs.is_empty());

        // Idle reconnects happen (silent later connections), but no frame refreshes anything.
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert_eq!(removal.block_number_or_timestamp, 100);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn repeated_immediate_closes_remove_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = socket.close(None).await;
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
    }

    #[tokio::test]
    async fn refused_reconnects_remove_within_stale_after() {
        let mut fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = socket.close(None).await;
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);

        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        // From here every connect is refused at TCP level.
        fake.shutdown();
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn replayed_frames_remove_within_stale_after_and_never_re_add() {
        // One frame, stamped once, replayed every 50 ms forever.
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let replay = frame_text(100, wall_nanos_now());
            loop {
                if socket
                    .send(Message::Text(replay.clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        // Replays are accepted while fresh but cannot extend the deadline; the deadline fires
        // even though a frame is ready on every poll.
        let removal = loop {
            let update = next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("stream stays alive");
            if !update.removed_pairs.is_empty() {
                break update;
            }
            assert!(update.new_pairs.is_empty());
        };
        assert_removal_only(&removal, 1);
        // Every later replay is too old to be accepted: nothing comes back.
        assert!(next_within(&mut stream, Duration::from_millis(500))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn ping_only_traffic_removes_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await;
            }
            loop {
                if socket
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);
        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "idle watchdog did not reconnect");
    }

    #[tokio::test]
    async fn malformed_text_removes_within_stale_after() {
        let fake = FakeTitan::spawn(|index, mut socket| async move {
            if index == 0 {
                let _ = socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await;
            }
            loop {
                if socket
                    .send(Message::Text("nonsense".into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake).build();
        tokio::pin!(stream);
        assert_eq!(
            next_within(&mut stream, Duration::from_secs(2))
                .await
                .expect("first snapshot")
                .new_pairs
                .len(),
            1
        );
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        assert!(fake.connections.load(Ordering::SeqCst) >= 2, "idle watchdog did not reconnect");
    }

    #[tokio::test]
    async fn fresh_frame_after_removal_re_adds_the_component() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            let _ = socket
                .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = socket
                .send(Message::Text(frame_text(101, wall_nanos_now()).into()))
                .await;
            std::future::pending::<()>().await;
        })
        .await;
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::pin!(stream);

        let first = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        assert_eq!(first.new_pairs.len(), 1);
        let removal = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("removal");
        assert_removal_only(&removal, 1);
        let re_added = next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("re-add");
        assert_eq!(re_added.new_pairs.len(), 1);
        assert!(re_added.removed_pairs.is_empty());
        assert_eq!(re_added.block_number_or_timestamp, 101);
    }

    #[tokio::test]
    async fn frames_are_forwarded_without_waiting_on_timers() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            loop {
                if socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let stream = fast_builder(&fake)
            .stale_after(Duration::from_secs(24))
            .build();
        tokio::pin!(stream);
        next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let update = next_within(&mut stream, Duration::from_secs(1))
                .await
                .expect("steady-state frame");
            assert!(started.elapsed() < Duration::from_millis(200));
            assert!(update.removed_pairs.is_empty());
        }
    }

    #[tokio::test]
    async fn no_connection_before_first_poll_and_drop_closes_the_socket() {
        let server_saw_close = Arc::new(AtomicBool::new(false));
        let fake = {
            let server_saw_close = server_saw_close.clone();
            FakeTitan::spawn(move |_, mut socket| {
                let server_saw_close = server_saw_close.clone();
                async move {
                    let _ = socket
                        .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                        .await;
                    // Read until the client goes away.
                    while let Some(Ok(message)) = socket.next().await {
                        if matches!(message, Message::Close(_)) {
                            break;
                        }
                    }
                    server_saw_close.store(true, Ordering::SeqCst);
                }
            })
            .await
        };
        let stream = fast_builder(&fake)
            .read_idle_timeout(Duration::from_secs(5))
            .build();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(fake.connections.load(Ordering::SeqCst), 0, "connected before first poll");

        tokio::pin!(stream);
        next_within(&mut stream, Duration::from_secs(2))
            .await
            .expect("first snapshot");
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);

        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(server_saw_close.load(Ordering::SeqCst), "socket not closed on drop");
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1, "reconnected after drop");
    }

    #[tokio::test]
    async fn unreachable_whitelist_serves_nothing_while_frames_flow() {
        let fake = FakeTitan::spawn(|_, mut socket| async move {
            loop {
                if socket
                    .send(Message::Text(frame_text(100, wall_nanos_now()).into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        // Port 1 refuses connections, so every whitelist read fails fast.
        let stream = PriceLevelStreamBuilder::new()
            .endpoint(fake.url())
            .fallback_router_rpc_url("http://127.0.0.1:1")
            .add_pamm(fermiswap())
            .with_tokens(tokens())
            .connect_timeout(Duration::from_secs(1))
            .max_backoff(Duration::from_millis(20))
            .build();
        tokio::pin!(stream);
        assert!(next_within(&mut stream, Duration::from_millis(700))
            .await
            .is_none());
        assert!(fake.connections.load(Ordering::SeqCst) >= 1, "frames were not consumed");
    }

    // `metrics::with_local_recorder` takes a sync closure, so this test drives its own
    // current-thread runtime instead of using `#[tokio::test]`.
    #[test]
    fn missing_node_url_serves_nothing_and_reports_awaiting_whitelist() {
        use metrics_util::debugging::DebuggingRecorder;

        use super::super::telemetry::{
            test_support::{gauge_value, snapshot_map},
            SOURCE_STATE,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let fake = FakeTitan::spawn(first_connection_sends_one_frame).await;
                let stream = PriceLevelStreamBuilder::new()
                    .endpoint(fake.url())
                    .without_env_rpc_url()
                    .add_pamm(fermiswap())
                    .with_tokens(tokens())
                    .connect_timeout(Duration::from_secs(1))
                    .build();
                tokio::pin!(stream);
                assert!(next_within(&mut stream, Duration::from_millis(500))
                    .await
                    .is_none());
            });
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(gauge_value(&snapshot, SOURCE_STATE, &[]), 0.0);
    }

    #[test]
    fn knob_defaults() {
        let builder = PriceLevelStreamBuilder::new();
        assert_eq!(builder.stale_after, DEFAULT_STALE_AFTER);
        assert_eq!(builder.whitelist_refresh_interval, DEFAULT_WHITELIST_REFRESH_INTERVAL);
        assert!(builder.fallback_router_rpc_url.is_none());
        assert!(builder.env_rpc_url);
    }
```

Test imports to add: `use std::{pin::Pin, str::FromStr, sync::{atomic::{AtomicBool, Ordering}, Arc}, time::Duration};`, `use futures::SinkExt;`, `use num_bigint::BigUint;`, `use tokio_tungstenite::tungstenite::Message;`, `use super::super::{config::{default_denied_pamms, PriceLevelStreamConfig}, test_support::{frame_text, tokens, wall_nanos_now, FakeConnection, FakeTitan, PAMM}, tracker::DEFAULT_STALE_AFTER};`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tycho-simulation --lib price_level_stream::stream`
Expected: compile errors for the knobs and the test seam.

- [ ] **Step 3: Implement the builder changes**

Add to `stream.rs` imports: `use std::pin::Pin;`, `use async_stream::stream;`, `use super::{fallback_router::{fetch_fallback_router_venues, router_venues_reader, RouterVenuesRead, WHITELIST_READ_TIMEOUT}, telemetry::{self, SourceState}, tracker::{Now, SnapshotTracker, DEFAULT_STALE_AFTER}, titan::{self, ConnectionSettings, TITAN_PRICE_LEVEL_URL}};`.

```rust
/// How often the PropAMMRouter whitelist is re-read by default. It is governance-gated and
/// changes rarely; ten minutes bounds how long a de-whitelisted venue keeps its old family.
pub(super) const DEFAULT_WHITELIST_REFRESH_INTERVAL: Duration = Duration::from_secs(600);
```

Add fields to `PriceLevelStreamBuilder` and `Default`:

```rust
    /// See [`stale_after`](Self::stale_after).
    stale_after: Duration,
    /// See [`whitelist_refresh_interval`](Self::whitelist_refresh_interval).
    whitelist_refresh_interval: Duration,
    /// See [`fallback_router_rpc_url`](Self::fallback_router_rpc_url).
    fallback_router_rpc_url: Option<String>,
    /// Whether `RPC_URL` from the environment (or `.env`) is consulted when no explicit
    /// fallback router URL is set. Only tests turn this off, to pin down the no-URL path
    /// without touching process-global state.
    env_rpc_url: bool,
```

with defaults `DEFAULT_STALE_AFTER`, `DEFAULT_WHITELIST_REFRESH_INTERVAL`, `None`, `true`.

Setters:

```rust
    /// Overrides how long the data of one frame may be served without a fresher frame carrying
    /// it (default: 24s, two block times). A component no accepted frame has carried for this
    /// long is emitted in `removed_pairs`; the next accepted frame carrying it re-adds it in
    /// `new_pairs`. Frames whose data is this old or older are rejected outright. Independent
    /// of this setting, a state refuses to quote once its frame is one block time old.
    pub fn stale_after(mut self, duration: Duration) -> Self {
        self.stale_after = duration;
        self
    }

    /// Overrides how often the PropAMMRouter whitelist is re-read (default: 10 minutes). A
    /// venue whose family changes is removed at once and re-added under the new family by the
    /// next frame carrying it.
    pub fn whitelist_refresh_interval(mut self, interval: Duration) -> Self {
        self.whitelist_refresh_interval = interval;
        self
    }

    /// Sets the node URL the PropAMMRouter whitelist is read from, taking precedence over
    /// `RPC_URL` from the environment or `.env`. Consumers that already hold a node URL as
    /// configuration should pass it here rather than depend on process environment for an
    /// execution-affecting decision.
    pub fn fallback_router_rpc_url(mut self, url: impl Into<String>) -> Self {
        self.fallback_router_rpc_url = Some(url.into());
        self
    }

    #[cfg(test)]
    fn without_env_rpc_url(mut self) -> Self {
        self.env_rpc_url = false;
        self
    }
```

Replace `build` and delete `fetch_router_venues`:

```rust
    pub fn build(self) -> impl Stream<Item = Update> + Send {
        let Self {
            registry,
            denied,
            tokens,
            url,
            auto_detect,
            auto_detected_gas_cost,
            connection,
            fallback_router,
            stale_after,
            whitelist_refresh_interval,
            fallback_router_rpc_url,
            env_rpc_url,
        } = self;
        if registry.is_empty() && !auto_detect {
            tracing::warn!(
                "No pAMMs registered and auto-detection is off; the stream will never produce \
                 an update"
            );
        }
        if tokens.is_empty() {
            tracing::warn!(
                "No token metadata provided; every streamed pair will be skipped and the stream \
                 will never produce an update"
            );
        }
        let url = url.unwrap_or_else(|| TITAN_PRICE_LEVEL_URL.to_string());
        let auto_detected_gas_cost =
            auto_detected_gas_cost.unwrap_or_else(|| BigUint::from(DEFAULT_AUTO_DETECTED_GAS_COST));
        let rpc_url = match (fallback_router, fallback_router_rpc_url) {
            (false, _) => None,
            (true, Some(explicit)) => Some(explicit),
            (true, None) => env_rpc_url.then(rpc_url_from_env).flatten(),
        };
        if fallback_router && rpc_url.is_none() {
            tracing::error!(
                "No node URL to read the PropAMMRouter whitelist from: set RPC_URL, call \
                 fallback_router_rpc_url, or opt out with without_fallback_router. No pAMM will \
                 be served"
            );
            telemetry::source_state(SourceState::AwaitingWhitelist);
        }
        let mut tracker = SnapshotTracker::new(
            registry,
            denied,
            tokens,
            auto_detect,
            auto_detected_gas_cost,
            stale_after,
            fallback_router,
        );
        let max_backoff = connection.max_backoff;

        stream! {
            let frames = titan::messages(url, connection);
            tokio::pin!(frames);
            let mut whitelist: Option<WhitelistReader> = rpc_url.map(|rpc_url| {
                let fetch = move || {
                    let rpc_url = rpc_url.clone();
                    async move { fetch_fallback_router_venues(&rpc_url).await }
                };
                Box::pin(router_venues_reader(
                    fetch,
                    WHITELIST_READ_TIMEOUT,
                    max_backoff,
                    whitelist_refresh_interval,
                )) as WhitelistReader
            });
            loop {
                let deadline = tracker.stale_deadline();
                let sleep_until_deadline = tokio::time::sleep_until(
                    deadline.map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std),
                );
                let update = tokio::select! {
                    Some(frame) = frames.next() => tracker.on_frame(frame, Now::current()),
                    () = sleep_until_deadline, if deadline.is_some() => {
                        tracker.on_stale_deadline(Now::current())
                    }
                    read = next_whitelist_read(&mut whitelist) => tracker.on_router_venues(read),
                };
                if let Some(update) = update {
                    yield update;
                }
            }
        }
    }
}

type WhitelistReader = Pin<Box<dyn Stream<Item = RouterVenuesRead> + Send>>;

/// The next whitelist read, or a future that never resolves when the whitelist is not read at
/// all, so the `select!` arm simply never fires.
async fn next_whitelist_read(reader: &mut Option<WhitelistReader>) -> RouterVenuesRead {
    let Some(reader) = reader else {
        return std::future::pending().await;
    };
    match reader.next().await {
        Some(read) => read,
        None => std::future::pending().await,
    }
}
```

Keep `rpc_url_from_env` as is. The `stream!` block is an `async move` block: the socket and the whitelist reader are owned by it, so dropping the returned stream drops both.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tycho-simulation --lib price_level_stream`
Expected: zero failures. Run the loop tests three times: `for i in 1 2 3; do cargo test -p tycho-simulation --lib price_level_stream::stream || exit 1; done`.

Also confirm the example and integration-test binary still compile:
`cargo check -p tycho-simulation --examples && cargo check -p tycho-integration-test`

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/stream.rs
git commit -m "feat(simulation): expire stale price level components on a deadline timer"
```

---

### Task 11: Captured-frame fixture

**Files:**
- Create: `crates/tycho-simulation/src/price_level_stream/test_responses/pamm_price_levels_1788624558231060482.json`
- Modify: `crates/tycho-simulation/src/price_level_stream/titan.rs` (tests)

- [ ] **Step 1: Copy the first raw frame of the 2026-09-05 capture**

```bash
cp /home/dev/projects/propellerheads/investigations/pricelevelstream-stability/reports/live-characterization-2026-09-05/eu/raw_frame_0.json \
   crates/tycho-simulation/src/price_level_stream/test_responses/pamm_price_levels_1788624558231060482.json
```

The file is 47,114 bytes: block 25912232, slot 15150045, seven venues, every venue entry carries an undocumented `maker` integer, every level is `"Simulated"`.

- [ ] **Step 2: Write the test**

```rust
    /// A frame captured verbatim from the live stream (2026-09-05). It carries `timestamp`,
    /// `slot`, and an undocumented per-venue `maker` field the parser must ignore.
    const CAPTURED_MESSAGE_2026_09_05: &str =
        include_str!("test_responses/pamm_price_levels_1788624558231060482.json");

    #[test]
    fn parses_captured_live_message_with_timestamp_and_extra_fields() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE_2026_09_05).expect("valid JSON");
        assert_eq!(message.timestamp, 1788624558231060482);
        assert_eq!(message.block_number, 25912232);
        assert_eq!(message.pamms.len(), 7);
        let fermiswap = message
            .pamms
            .iter()
            .find(|pamm| {
                pamm.pamm == Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap()
            })
            .expect("fermiswap present");
        assert_eq!(fermiswap.pairs.len(), 12);
    }
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p tycho-simulation --lib price_level_stream::titan::tests::parses_captured_live_message_with_timestamp_and_extra_fields`
Expected: PASS.

- [ ] **Step 4: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/test_responses/pamm_price_levels_1788624558231060482.json crates/tycho-simulation/src/price_level_stream/titan.rs
git commit -m "test(simulation): add a 2026-09-05 Titan price level frame fixture"
```

---

### Task 12: Documentation

**Files:**
- Modify: `crates/tycho-simulation/src/price_level_stream/mod.rs:1-37` (module docs)
- Modify: `crates/tycho-simulation/src/price_level_stream/stream.rs` (`build()` docstring, `read_idle_timeout` docstring)
- Modify: `crates/tycho-simulation/CLAUDE.md:24-40`

- [ ] **Step 1: Rewrite the module docs in `mod.rs`**

```rust
//! Titan pAMM price level stream integration.
//!
//! Titan Builder exposes a WebSocket stream of per-pair quote ladders (simulated price levels)
//! for the pAMMs it builds blocks with (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>). This module turns those
//! frames directly into [`Update`](crate::protocol::models::Update)s ready for consumption.
//!
//! # Freshness contract
//!
//! Frames are best effort, not complete snapshots: a venue or a pair can be absent from one
//! frame and present in the next, so absence is never read as retirement. Instead every served
//! component is vouched for only as long as an accepted frame carried it within `stale_after`
//! (default 24 s, two block times, see
//! [`stale_after`](stream::PriceLevelStreamBuilder::stale_after)):
//!
//! - A frame is accepted only if its wire `timestamp` is younger than `stale_after`, not more
//!   than one slot in the future, not older than the newest accepted frame (equal is fine:
//!   Titan re-emits within a build round), and its block neither regresses nor jumps further
//!   than elapsed time allows. Rejected frames change nothing.
//! - A component no accepted frame has carried for `stale_after` is emitted in
//!   `removed_pairs`, together with every other component that expired at the same instant.
//!   The next accepted frame carrying it re-adds it in `new_pairs`. Silence, disconnects,
//!   keepalive-only or unparsable traffic, and replayed frames all end in this removal.
//! - Every emitted state also carries a one-block-time quote guard
//!   ([`QUOTE_TTL`](state::QUOTE_TTL)) anchored on its frame's `timestamp`, so a ladder cannot
//!   be quoted past the block it targeted even before the removal arrives. The guard is a live
//!   property and is never serialized.
//!
//! Recovery is per component: a frame carrying a pair re-adds that pair, nothing more, and a
//! frame carrying one direction re-adds the pair with the other direction unquotable.
//! Consumers cannot tell an expiry from a retired venue; both mean the component must not be
//! routed until it reappears in `new_pairs`. Removal and re-add are always separate updates.
//!
//! Quotes target the block currently being built, so every emitted update is marked as partial
//! and supersedes the previous one for the pairs it contains.
//!
//! # Identity and families
//!
//! Components are identified as `pricelevelstream:{pamm}`, where `{pamm}` is the configured
//! venue name (e.g. `pricelevelstream:fermiswap`) or, for auto-detected venues, the venue
//! address (e.g. `pricelevelstream:0x5979…`). The prefix keeps these components distinct from
//! those any other integration path may produce for the same venue (e.g. `vm:fermiswap`).
//!
//! Venues on Titan's PropAMMRouter whitelist are emitted under `propammfallback:{pamm}` instead:
//! tycho-execution routes their swaps through the router, which falls back to a single-hop
//! Uniswap V3 pool when the venue reverts. The whitelist is read through the node at
//! [`fallback_router_rpc_url`](stream::PriceLevelStreamBuilder::fallback_router_rpc_url) or
//! `RPC_URL`, each read bounded by a timeout, retried with backoff until it succeeds, and
//! re-read every
//! [`whitelist_refresh_interval`](stream::PriceLevelStreamBuilder::whitelist_refresh_interval).
//! Nothing is served until the first read succeeds, and without a node URL nothing is ever
//! served, so a misconfigured deployment never silently lands on the unfloored direct family.
//! A venue whose membership changes is removed at once and re-added under its new family by
//! the next frame carrying it.
//! [`without_fallback_router`](stream::PriceLevelStreamBuilder::without_fallback_router) skips
//! the read and keeps every venue on the direct path unconditionally.
//!
//! Distinct identifiers do not imply distinct liquidity, though: a venue served here may also be
//! integrated through another path, in which case the components of both paths price the same
//! underlying inventory. Consumers subscribing to multiple paths must expect such overlaps and
//! deduplicate by venue — e.g. via the
//! [`PAMM_ADDRESS_ATTRIBUTE`](stream::PAMM_ADDRESS_ATTRIBUTE) — wherever double-counting
//! matters, such as routing over the combined liquidity.
//!
//! # Observability
//!
//! The stream emits `price_level_stream_*` metrics through the `metrics` facade (frames
//! accepted and rejected by reason, last seen timestamp and served components per registered
//! venue, stale removals, source state, reconnects, whitelist reads); a consumer with a
//! recorder installed sees them without wiring. Per-venue series start at zero for every
//! registered venue, and no label ever carries a value from the wire.
//!
//! Entry point: [`PriceLevelStreamBuilder`](stream::PriceLevelStreamBuilder). Register the pAMMs
//! to serve — the known venues via
//! [`with_known_pamms`](stream::PriceLevelStreamBuilder::with_known_pamms), individual
//! [`PriceLevelStreamConfig`](config::PriceLevelStreamConfig)s via
//! [`add_pamm`](stream::PriceLevelStreamBuilder::add_pamm), or any streamed venue via
//! auto-detection — provide token metadata, and consume the resulting stream of
//! [`Update`](crate::protocol::models::Update)s.
```

- [ ] **Step 2: Rewrite the `build()` docstring in `stream.rs`**

```rust
    /// Consumes the builder and opens the stream.
    ///
    /// The connection is established lazily on first poll and maintained (with reconnects) for as
    /// long as the stream is polled; it never terminates on its own, and dropping the stream
    /// closes the connection and stops the whitelist reader.
    ///
    /// Every accepted frame yields an update with the states of the served pairs it carries,
    /// with `new_pairs` for pairs not currently served. Pairs the frame does not carry keep their
    /// previous state downstream. A component no accepted frame has carried for
    /// [`stale_after`](Self::stale_after) is emitted in `removed_pairs`, together with every
    /// other component expiring at that instant, and re-added by the next accepted frame
    /// carrying it. Frames that are too old, from the future, out of order, or whose block
    /// regresses or jumps implausibly are rejected without effect. Frames that contain no served
    /// pAMM produce no update. Pairs whose tokens are missing from the provided token metadata
    /// are skipped.
    ///
    /// With the fallback router enabled (the default), nothing is emitted until the
    /// PropAMMRouter whitelist has been read from the node at
    /// [`fallback_router_rpc_url`](Self::fallback_router_rpc_url) or `RPC_URL`; each read is
    /// bounded by a timeout, retried with backoff, and refreshed periodically. Without a node URL
    /// an error is logged and nothing is ever served. See the [module documentation](super) for
    /// the full contract.
```

Update the `read_idle_timeout` setter docstring:

```rust
    /// Overrides the longest gap between parsed Titan frames tolerated before the connection is
    /// treated as dead and re-established (default: 10s). Titan pushes one frame per second and
    /// sends no keepalives, so a multi-second silence means a stalled or half-open connection.
    /// Control frames and unparsable text do not count as liveness.
```

- [ ] **Step 3: Update `crates/tycho-simulation/CLAUDE.md`**

Replace the `price_level_stream/` bullet with:

```markdown
- **`price_level_stream/`**: Titan pAMM price level stream — `PriceLevelStreamBuilder` turns the
  Titan WebSocket's per-pair quote-ladder frames directly into `Update`s (no indexer feed
  round-trip); `PriceLevelStreamState` quotes by interpolating the ladder and refuses to quote
  once its frame is one block time old (`quotable_until`, monotonic, never serialized). Frames
  are best-effort, not complete snapshots: `tracker.rs` keys freshness per component, never
  removes on frame diff, and emits `removed_pairs` only when a component's data is `stale_after`
  (default 24 s) old or its PropAMMRouter family changes; the next accepted frame carrying it
  re-adds it. Frames are accepted only if their wire `timestamp` is younger than `stale_after`,
  not in the future, not older than the newest accepted one (equal allowed), and their block
  neither regresses nor jumps implausibly; the block frontier resets whenever nothing is served.
  `build()` is an `async_stream` loop selecting over frames, an earliest-deadline timer, and the
  whitelist reader (`fallback_router.rs`, each read bounded by 15 s, retried with backoff,
  refreshed every 10 min); nothing is served until the whitelist is known, and without
  `fallback_router_rpc_url` or `RPC_URL` nothing is ever served. `titan.rs` counts only parsed
  frames as liveness (idle timeout 10 s). `telemetry.rs` emits `price_level_stream_*` metrics via
  the `metrics` facade, per-venue series pre-initialised to zero, no wire values as labels.
  Components are identified as `pricelevelstream:{pamm}` or `propammfallback:{pamm}` for
  whitelisted venues; a stale expiry and a retired venue look the same downstream. Registration
  precedence (`with_known_pamms`, `add_pamm`, `deny_pamm`, auto-detection) is unchanged: later
  explicit calls win, defaults never override them.
```

- [ ] **Step 4: Build the docs to catch broken intra-doc links**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tycho-simulation --no-deps --all-features`
Expected: exit 0.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo +nightly-2026-06-28 fmt
cargo +nightly-2026-06-28 clippy -p tycho-simulation --all-targets --all-features -- -D warnings
git add crates/tycho-simulation/src/price_level_stream/mod.rs crates/tycho-simulation/src/price_level_stream/stream.rs crates/tycho-simulation/CLAUDE.md
git commit -m "docs(simulation): document the price level stream freshness contract"
```

---

### Task 13: Verification and pull request

**Files:** none new.

- [ ] **Step 1: Full crate tests, three times for the timing-sensitive ones**

```bash
cargo test -p tycho-simulation --lib price_level_stream
for i in 1 2 3; do cargo test -p tycho-simulation --lib price_level_stream -- --test-threads=4 || exit 1; done
cargo test -p tycho-simulation
```

Expected: every run has zero failures; the one ignored mainnet whitelist test stays ignored.

- [ ] **Step 2: Workspace lint and format**

```bash
cargo +nightly-2026-06-28 fmt --check
cargo +nightly-2026-06-28 clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: both exit 0. These are the exact commands and toolchain CI runs.

- [ ] **Step 3: Run the repository CI skill and the docs sync**

Invoke `/run-ci`, then `/sync-docs` (documentation only; it must not touch source). Commit anything `/sync-docs` changes with `docs: sync price level stream docs`.

- [ ] **Step 4: Check acceptance criteria against the spec**

Walk `.claude/specs/2026-09-04-price-level-stream-freshness-design.md` section 12 and name the test that proves each criterion:

| Criterion | Test |
|---|---|
| 1 silence, disconnect, refused, keepalive-only, malformed, replay → removal at deadline | `silence_past_stale_after_removes_every_served_component`, `repeated_immediate_closes_remove_within_stale_after`, `refused_reconnects_remove_within_stale_after`, `ping_only_traffic_removes_within_stale_after`, `malformed_text_removes_within_stale_after`, `replayed_frames_remove_within_stale_after_and_never_re_add` |
| 2 component absent for `stale_after` expires alone | `omitted_pair_expires_alone_while_the_rest_stays_served` |
| 3 omission emits no removal | `omitted_pair_is_not_removed_by_the_frame`, `omitted_venue_is_not_removed_by_the_frame`, `pair_set_oscillation_emits_no_removal` |
| 4 re-add in `new_pairs` and `states`, subset only, reconnection alone re-adds nothing | `component_expires_at_its_own_deadline_and_is_re_added_by_the_next_frame`, `fresh_frame_after_removal_re_adds_the_component`, `recovery_re_adds_only_the_pairs_the_frame_carries`, `one_direction_frame_re_adds_the_pair_with_the_other_direction_unquotable`, `silence_past_stale_after_removes_every_served_component` (silent reconnects re-add nothing) |
| 5 no id in both maps | `family_change_removes_now_and_re_adds_under_the_new_family`, `removal_update` never sets `new_pairs` |
| 6 rejection reasons, boundary, equal accepted | `data_exactly_stale_after_old_is_rejected`, `frame_from_the_future_is_rejected`, `older_timestamp_frame_is_rejected`, `equal_timestamp_frame_is_accepted`, `block_regression_is_rejected`, `block_jump_beyond_the_elapsed_bound_is_rejected`, `rejections_are_counted_by_reason` |
| 7 poisoned block cannot freeze beyond `stale_after` | `block_jump_beyond_the_elapsed_bound_is_rejected`, `poisoned_first_block_recovers_after_expiry` |
| 8 nothing until whitelist, pending read bounded, family change | `nothing_is_emitted_until_the_whitelist_is_known`, `failed_first_read_keeps_waiting`, `missing_node_url_serves_nothing_and_reports_awaiting_whitelist`, `unreachable_whitelist_serves_nothing_while_frames_flow`, `reader_fails_a_read_that_never_resolves`, `family_change_removes_now_and_re_adds_under_the_new_family`, `reader_retries_failures_and_refreshes_after_success` |
| 9 expiry error on every method, guard bounded, never serialized | `expired_state_refuses_every_query`, `ensure_quotable_is_strict_at_the_boundary`, `quote_guard_never_exceeds_one_block_for_a_future_stamped_frame`, `frame_older_than_one_block_yields_an_unquotable_state`, `fresh_state_quotes_and_its_successor_keeps_the_guard`, `guard_is_never_serialized_and_deserializes_as_none` |
| 10 every metric covered, no wire labels | `every_helper_emits_its_named_metric`, `rejections_are_counted_by_reason`, `served_components_are_gauged_per_venue_from_zero`, `unregistered_venues_are_counted_without_labels_and_logged_boundedly` |
| 11 CI | steps 1 and 2 |
| lifecycle | `no_connection_before_first_poll_and_drop_closes_the_socket`, `frames_are_forwarded_without_waiting_on_timers` |

- [ ] **Step 5: Push and open the PR**

Push with the repository's configured authentication. Authentication setup is executor and environment specific and is not part of this plan; the plan never mutates git configuration.

```bash
git push -u origin tl/price-level-stream-freshness
gh pr create --title "feat(simulation): fail closed on stale Titan price levels" --body "$(cat <<'EOF'
## Summary
- Parse Titan's frame `timestamp` and accept frames only when younger than `stale_after`, not in the future, not older than the newest accepted one, and with a plausible block; the block frontier resets whenever nothing is served.
- Track freshness per component: a component no accepted frame carried for `stale_after` (24 s) is emitted in `removed_pairs`; the next accepted frame carrying it re-adds it. Frame omission no longer removes anything, since Titan frames are best-effort (7.7% partial in a 15 min capture).
- `PriceLevelStreamState` carries a monotonic, live-only one-block quote guard and refuses to quote past it.
- The PropAMMRouter whitelist read is bounded by a 15 s timeout, retries with backoff, serves nothing until it succeeds, refreshes every 10 min, and moves venues between families via removal and re-add. No node URL is an error, not a silent downgrade. New `fallback_router_rpc_url` knob.
- Only parsed frames count as liveness; idle timeout default is now 10 s.
- `price_level_stream_*` metrics through the `metrics` facade, per-venue series pre-initialised, no wire values as labels.

Spec: `.claude/specs/2026-09-04-price-level-stream-freshness-design.md`

## Test plan
- [ ] `cargo test -p tycho-simulation --lib price_level_stream` green three runs in a row
- [ ] `cargo +nightly-2026-06-28 clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo +nightly-2026-06-28 fmt --check`
- [ ] Fynd canary on `ethereum-fynd-spot` after the release, watching `price_level_stream_source_state`, `price_level_stream_served_components`, `price_level_stream_frames_rejected_total`, `price_level_stream_stale_removals_total`
EOF
)"
```

The PR body above is complete as written. No model or assistant attribution goes into commit messages, commit trailers, or the PR body, whoever executes this plan.

- [ ] **Step 6: Record the operational follow-ups**

These are outside this repository and do not gate merging. Open one tracking task each:

1. Alerts on the new metrics, to be configured against the canary before the baseline rollout:

```yaml
# A registered venue not seen for two block times. The gauge starts at 0, so the expression is
# true from the first scrape; the `for` clause is what grants a new pod its grace period.
- alert: PriceLevelStreamVenueNotSeen
  expr: time() - price_level_stream_last_seen_timestamp_seconds > 24
  for: 24s

# Partial, persistent coverage loss against the venue's own recent baseline.
- alert: PriceLevelStreamVenueCoverageDrop
  expr: >
    price_level_stream_served_components
      < 0.75 * max_over_time(price_level_stream_served_components[6h])
  for: 5m

# Stream not serving. The `for` clause is the startup grace: whitelist read plus first frame.
- alert: PriceLevelStreamNotServing
  expr: price_level_stream_source_state != 2
  for: 1m
```

2. Fynd consumer-contract test, tracked as a follow-up in the Fynd repository: add → update → removal → graph absence → re-add through `handle_tycho_message` and a worker graph, plus a quote attempt on an expired state being refused. Not a gate: Fynd's removal path is already covered by `test_handle_message_removes_components`, and `Refusal::of` maps the expiry error to a refusal.

---

## Self-review

**Spec coverage.** Section 5.1 architecture → Task 10. 5.2 state machine and block frontier reset → Tasks 6 and 7. 5.3 acceptance, `>=` boundary, poisoned first block, rejection streak logging → Tasks 5 and 6. 5.4 per-component freshness, `quotable_until`, and sweep → Task 6. 5.5 whitelist lifecycle with read timeout and single WARN → Tasks 7, 8, 10. 5.6 transport liveness → Task 9. 5.7 monotonic quote guard, never serialized → Task 2. 5.8 kept/changed/ignored behaviour → Tasks 6 (empty ladders, one-direction ladders via `merge_pairs`), 7 (bounded unregistered signal), 1 (`maker` ignored by serde). 5.9 builder API, three knobs → Task 10. 5.10 Update shape → Tasks 6 and 10. 5.11 metrics with per-venue initialisation and no wire labels → Task 3 plus emit sites in 5, 6, 7, 8, 9. 5.12 logging → inline in each task. Section 7 tests → Tasks 2, 5, 6, 7, 8, 9, 10, 11. Section 8 docs → Task 12. Section 9 rollout → Task 13 PR body. Sections 10 and 11 are out of repo.

**Placeholder scan.** No TBD/TODO. Task 4 is a verbatim move and names every moved item. Task 5 keeps the old diff body for one commit and Task 6 replaces it; both show the exact code. No exact test totals are asserted anywhere.

**Type consistency.** `Now { wall_nanos, instant }` used identically in Tasks 5, 6, 10. `SnapshotTracker::new` has the seven-argument shape from Task 6 onward (Task 5 has the intermediate shape and says so). `RouterVenuesRead` is defined in `fallback_router.rs` (Task 7) and consumed by Tasks 7, 8, 10. `router_venues_reader(fetch, read_timeout, max_backoff, refresh_interval)` matches between Task 8 and Task 10. `telemetry` function names match between Task 3 and their call sites, including the label-free `unregistered_pamm()` and per-venue `served_components(pamm, count)`. `QUOTE_TTL` and `with_quotable_until` are defined in Task 2 and used in Task 6. `DEFAULT_STALE_AFTER` lives in `tracker.rs` and is imported by `stream.rs` in Task 10. `FakeTitan::shutdown`, `frame_text`, `wall_nanos_now`, `FakeConnection` are defined once in Task 9 and used in Tasks 9 and 10.

