# VM Sim Lazy Spot Prices + Limit Caching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `EVMPoolState::get_amount_out` stop eagerly recomputing all spot prices and stop recomputing block-stable limits, by adding two thread-safe read-through caches — without changing any simulation result.

**Architecture:** Replace the plain `spot_prices` field with `spot_price_cache: RwLock<HashMap<…>>`, add `limit_cache: RwLock<HashMap<…>>`, and a hand-written `Clone` that deep-copies both. `spot_price()` and `get_amount_limits()` become lazy read-through (compute-on-miss, cache, return). `get_amount_out` reuses cached limits and invalidates the child state's caches instead of eager recompute. `update_pool_state` (per block) keeps eager warming. Both caches are invalidated exactly when pool state changes.

**Tech Stack:** Rust, `std::sync::RwLock`, revm, criterion (existing baselines). All work in one file: `crates/tycho-simulation/src/evm/protocol/vm/state.rs`.

---

## Conventions

- Work in the worktree `/home/dev/projects/propellerheads/tycho-indexer-vm-sim-bench` (branch `feat/vm-sim-bench`). Run cargo from there.
- After each task: `cargo +nightly fmt`, `cargo clippy -p tycho-simulation --all-targets 2>&1 | tail`, and the named tests.
- All edits are in `crates/tycho-simulation/src/evm/protocol/vm/state.rs` unless stated.
- The file starts with `#![allow(deprecated)]`, so referencing the deprecated `balance_owner` field is fine.

## File Structure

Single file: `crates/tycho-simulation/src/evm/protocol/vm/state.rs`.
- Struct field change + manual `Clone` (Task 1)
- Per-pair spot-price compute helper extraction (Task 2)
- Lazy `spot_price` (Task 3)
- Cache-through `get_amount_limits` (Task 4)
- `get_amount_out` invalidation + cached limits (Task 5)
- `update_pool_state` invalidation + eager warm (Task 6)
- Concurrency test extension in `crates/tycho-simulation/tests/vm_concurrency.rs` (Task 7)
- Benchmark comparison, no code (Task 8)

---

## Task 1: Add caches + manual Clone (no behaviour change)

Convert `spot_prices` to an interior-mutable cache and add `limit_cache`. Keep every existing
behaviour identical: the eager `set_spot_prices` still fills the cache; `spot_price()` still only
reads. This task must leave all existing tests green (after mechanical field-access updates).

**Files:** Modify `crates/tycho-simulation/src/evm/protocol/vm/state.rs`

- [ ] **Step 1: Add the RwLock import**

At the top of the file, add `std::sync::RwLock` to imports (the `use std::{…}` block). Add this line inside it:
```rust
    sync::RwLock,
```
(So the block includes `any::Any`, `collections::{HashMap, HashSet}`, `fmt::{self, Debug}`, `str::FromStr`, `sync::RwLock`.)

- [ ] **Step 2: Replace the `spot_prices` field and add `limit_cache`; drop `#[derive(Clone)]`**

Change the struct attribute line `#[derive(Clone)]` (state.rs:38) to nothing (delete it). Replace the field:
```rust
    /// Spot prices of the pool by token pair
    spot_prices: HashMap<(Address, Address), f64>,
```
with:
```rust
    /// Read-through cache of spot prices by `(sell, buy)`. Lazily populated by `spot_price`,
    /// eagerly warmed by `update_pool_state`, and cleared whenever pool state changes.
    spot_price_cache: RwLock<HashMap<(Address, Address), f64>>,
    /// Read-through cache of `(sell_limit, buy_limit)` by `(sell, buy)`. Stable for a given
    /// pool-state-version; cleared whenever pool state changes.
    limit_cache: RwLock<HashMap<(Address, Address), (U256, U256)>>,
```

- [ ] **Step 3: Add a manual `Clone` impl (deep-copies both caches)**

Immediately after the struct definition (before the `impl<D> Debug` block at state.rs:76), add:
```rust
impl<D> Clone for EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            tokens: self.tokens.clone(),
            balances: self.balances.clone(),
            balance_owner: self.balance_owner,
            spot_price_cache: RwLock::new(
                self.spot_price_cache
                    .read()
                    .expect("spot_price_cache poisoned")
                    .clone(),
            ),
            limit_cache: RwLock::new(
                self.limit_cache
                    .read()
                    .expect("limit_cache poisoned")
                    .clone(),
            ),
            capabilities: self.capabilities.clone(),
            block_lasting_overwrites: self.block_lasting_overwrites.clone(),
            involved_contracts: self.involved_contracts.clone(),
            contract_balances: self.contract_balances.clone(),
            manual_updates: self.manual_updates,
            adapter_contract: self.adapter_contract.clone(),
            disable_overwrite_tokens: self.disable_overwrite_tokens.clone(),
        }
    }
}
```

- [ ] **Step 4: Update the `new()` constructor body**

In `pub fn new(...)`, the body builds `Self { … spot_prices, … }`. Replace the `spot_prices` field
initializer with both cache initializers. Find:
```rust
            spot_prices,
```
in the `Self { … }` of `new()` and replace with:
```rust
            spot_price_cache: RwLock::new(spot_prices),
            limit_cache: RwLock::new(HashMap::new()),
```
(The `new()` signature keeps its `spot_prices: HashMap<(Address, Address), f64>` parameter, so the
`EVMPoolStateBuilder` call site that passes `HashMap::new()` needs no change.)

- [ ] **Step 5: Update `set_spot_prices` to write through the cache**

In `set_spot_prices` (state.rs:188) there are two `self.spot_prices.insert(...)` calls (one in the
`PriceFunction` branch, one in the fallback branch). Replace each:
```rust
                    self.spot_prices
                        .insert((sell_token_address, buy_token_address), price);
```
and
```rust
                    self.spot_prices
                        .insert((t0, t1), marginal_price);
```
with a write-lock insert, e.g.:
```rust
                    self.spot_price_cache
                        .write()
                        .expect("spot_price_cache poisoned")
                        .insert((sell_token_address, buy_token_address), price);
```
and
```rust
                    self.spot_price_cache
                        .write()
                        .expect("spot_price_cache poisoned")
                        .insert((t0, t1), marginal_price);
```
`set_spot_prices` remains `&mut self` for now (unchanged signature).

- [ ] **Step 6: Update `spot_price()` to read through the cache (still read-only)**

Replace the body of `fn spot_price` (state.rs:597) which currently does
`self.spot_prices.get(&(base_address, quote_address)).cloned().ok_or(...)`:
```rust
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let base_address = bytes_to_address(&base.address)?;
        let quote_address = bytes_to_address(&quote.address)?;
        self.spot_price_cache
            .read()
            .expect("spot_price_cache poisoned")
            .get(&(base_address, quote_address))
            .cloned()
            .ok_or(SimulationError::FatalError(format!(
                "Spot price not found for base token {base_address} and quote token {quote_address}"
            )))
    }
```

- [ ] **Step 7: Update tests that read the `spot_prices` field directly**

These tests in the `#[cfg(test)] mod tests` block access the now-renamed field. Update them to read
the cache map:
- `test_get_amount_out` and `test_sequential_get_amount_outs`: replace each
  `new_state.spot_prices` / `pool_state.spot_prices` in `assert_ne!(...)` with
  `*new_state.spot_price_cache.read().unwrap()` and `*pool_state.spot_price_cache.read().unwrap()`
  (deref the guard to compare the `HashMap`). Example:
  ```rust
  assert_ne!(
      *new_state.spot_price_cache.read().unwrap(),
      *pool_state.spot_price_cache.read().unwrap()
  );
  ```
  (These assertions still hold at this task because `get_amount_out` still calls the eager
  `set_spot_prices` on `new_state` — that call is removed in Task 5, which rewrites these assertions.)
- `test_set_spot_prices` and `test_set_spot_prices_without_capability`: replace
  `pool_state.spot_prices.get(&(…))` with
  `pool_state.spot_price_cache.read().unwrap().get(&(…))`. The exact expected numeric values
  (`0.137_778_914_319_047_9`, `7.071_503_245_428_246`, `0.13736685496467538`, `7.050354297665408`)
  are unchanged.

- [ ] **Step 8: Build, test, fmt, clippy**

Run:
```
cargo test -p tycho-simulation --lib evm::protocol::vm::state 2>&1 | tail -20
cargo test -p tycho-simulation --test fixture_replay --test vm_concurrency 2>&1 | tail -8
cargo +nightly fmt
cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
```
Expected: all the `vm::state` unit tests pass (still eager behaviour), fixture_replay 3 + vm_concurrency 1 pass, clippy clean.

- [ ] **Step 9: Commit**

```bash
git add crates/tycho-simulation/src/evm/protocol/vm/state.rs
git commit -m "refactor(sim): make spot_prices an interior-mutable cache, add limit cache"
```

---

## Task 2: Extract a per-pair spot-price compute helper

`set_spot_prices` contains the per-pair price computation inline (twice: PriceFunction branch and
fallback branch). Extract a single `&self` helper that computes ONE pair's price, so both the eager
loop (Task 1) and the lazy path (Task 3) call the same code. No behaviour change.

**Files:** Modify `crates/tycho-simulation/src/evm/protocol/vm/state.rs`

- [ ] **Step 1: Add the helper method**

Add this private method in the `impl<D> EVMPoolState<D>` block (near `set_spot_prices`). It reproduces
the existing per-pair logic for a single `(sell, buy)`, returning the scaled price:
```rust
    /// Computes the spot price for a single `(sell, buy)` pair using the same logic as the eager
    /// `set_spot_prices` loop: the adapter `price` function when `PriceFunction` is supported,
    /// otherwise a two-swap finite-difference. Does not touch the cache.
    fn compute_spot_price(
        &self,
        tokens: &HashMap<Bytes, Token>,
        sell_token_address: Address,
        buy_token_address: Address,
    ) -> Result<f64, SimulationError> {
        if self
            .capabilities
            .contains(&Capability::PriceFunction)
        {
            let overwrites = Some(self.get_overwrites(
                vec![sell_token_address, buy_token_address],
                *MAX_BALANCE / U256::from(100),
            )?);
            let (sell_amount_limit, _) = self.get_amount_limits(
                vec![sell_token_address, buy_token_address],
                overwrites.clone(),
            )?;
            let price_result = self.adapter_contract.price(
                &self.id,
                sell_token_address,
                buy_token_address,
                vec![sell_amount_limit / U256::from(100)],
                overwrites,
            )?;
            if self.capabilities.contains(&Capability::ScaledPrice) {
                price_result.first().copied().ok_or_else(|| {
                    SimulationError::FatalError("Calculated price array is empty".to_string())
                })
            } else {
                let unscaled_price = price_result.first().ok_or_else(|| {
                    SimulationError::FatalError("Calculated price array is empty".to_string())
                })?;
                let sell_token_decimals = self.get_decimals(tokens, &sell_token_address)?;
                let buy_token_decimals = self.get_decimals(tokens, &buy_token_address)?;
                Ok(*unscaled_price * 10f64.powi(sell_token_decimals as i32)
                    / 10f64.powi(buy_token_decimals as i32))
            }
        } else {
            let overwrites = Some(self.get_overwrites(
                vec![sell_token_address, buy_token_address],
                *MAX_BALANCE / U256::from(100),
            )?);
            let x1 = self
                .get_amount_limits(
                    vec![sell_token_address, buy_token_address],
                    overwrites.clone(),
                )?
                .0
                / U256::from(100);
            let x2 = x1 + (x1 / U256::from(100));
            let y1 = self
                .adapter_contract
                .swap(&self.id, sell_token_address, buy_token_address, false, x1, overwrites.clone())?
                .0
                .received_amount;
            let y2 = self
                .adapter_contract
                .swap(&self.id, sell_token_address, buy_token_address, false, x2, overwrites)?
                .0
                .received_amount;
            let sell_token_decimals = self.get_decimals(tokens, &sell_token_address)?;
            let buy_token_decimals = self.get_decimals(tokens, &buy_token_address)?;
            let num = y2 - y1;
            let den = x2 - x1;
            let token_correction =
                10f64.powi(sell_token_decimals as i32 - buy_token_decimals as i32);
            let num_f64 = u256_to_f64(num)?;
            let den_f64 = u256_to_f64(den)?;
            if den_f64 == 0.0 {
                return Err(SimulationError::FatalError(
                    "Failed to compute marginal price: denominator converted to 0".into(),
                ));
            }
            Ok(num_f64 / den_f64 * token_correction)
        }
    }
```

- [ ] **Step 2: Rewrite `set_spot_prices` to use the helper**

Replace the body of `set_spot_prices` (the whole `match self.ensure_capability(...) { ... }` that
loops permutations) with a single loop that calls `compute_spot_price` per permutation and writes the
cache. This preserves behaviour (same pairs, same values), removing the duplicated inline logic:
```rust
    pub fn set_spot_prices(
        &mut self,
        tokens: &HashMap<Bytes, Token>,
    ) -> Result<(), SimulationError> {
        for tokens_pair in self.tokens.iter().permutations(2) {
            let sell = bytes_to_address(tokens_pair[0])?;
            let buy = bytes_to_address(tokens_pair[1])?;
            let price = self.compute_spot_price(tokens, sell, buy)?;
            self.spot_price_cache
                .write()
                .expect("spot_price_cache poisoned")
                .insert((sell, buy), price);
        }
        Ok(())
    }
```
NOTE: the original distinguished `PriceFunction` vs no-capability via `ensure_capability`. The helper
encodes that same branch via `self.capabilities.contains(&Capability::PriceFunction)`, so the loop no
longer needs the outer match. Keep the `set_spot_prices` doc comment.

- [ ] **Step 3: Test, fmt, clippy**

Run:
```
cargo test -p tycho-simulation --lib evm::protocol::vm::state::tests::test_set_spot_prices 2>&1 | tail
cargo test -p tycho-simulation --lib evm::protocol::vm::state::tests::test_set_spot_prices_without_capability 2>&1 | tail
cargo +nightly fmt && cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
```
Expected: both spot-price tests pass with the SAME expected values; clippy clean.

- [ ] **Step 4: Commit**

```bash
git add crates/tycho-simulation/src/evm/protocol/vm/state.rs
git commit -m "refactor(sim): extract per-pair compute_spot_price helper"
```

---

## Task 3: Make `spot_price` lazy (compute-on-miss + cache)

**Files:** Modify `state.rs`; tests in the same file.

- [ ] **Step 1: Write a failing test**

Add to the `#[cfg(test)] mod tests` block:
```rust
    #[tokio::test]
    async fn test_spot_price_lazy_computes_on_miss() {
        let pool_state = setup_pool_state().await;
        // Cache starts empty (builder does not warm it).
        assert!(pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .is_empty());

        // Reading a pair computes it on demand and caches it.
        let price = pool_state.spot_price(&dai(), &bal()).unwrap();
        assert!(price > 0.0);
        assert_eq!(
            pool_state
                .spot_price_cache
                .read()
                .unwrap()
                .get(&(dai_addr(), bal_addr()))
                .copied(),
            Some(price)
        );
    }
```

- [ ] **Step 2: Run it, confirm it fails**

Run: `cargo test -p tycho-simulation --lib test_spot_price_lazy_computes_on_miss 2>&1 | tail`
Expected: FAIL — current `spot_price` returns `FatalError("Spot price not found …")` on a cold cache.

- [ ] **Step 3: Implement lazy `spot_price`**

`spot_price` (trait method) currently has only `base`/`quote` (no token map). The lazy compute needs a
`HashMap<Bytes, Token>`; build a 2-entry map from `base`/`quote` (they carry decimals). Replace the
`fn spot_price` body with:
```rust
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let base_address = bytes_to_address(&base.address)?;
        let quote_address = bytes_to_address(&quote.address)?;

        if let Some(price) = self
            .spot_price_cache
            .read()
            .expect("spot_price_cache poisoned")
            .get(&(base_address, quote_address))
            .copied()
        {
            return Ok(price);
        }

        let tokens = HashMap::from([
            (base.address.clone(), base.clone()),
            (quote.address.clone(), quote.clone()),
        ]);
        let price = self.compute_spot_price(&tokens, base_address, quote_address)?;
        self.spot_price_cache
            .write()
            .expect("spot_price_cache poisoned")
            .insert((base_address, quote_address), price);
        Ok(price)
    }
```

- [ ] **Step 4: Run the test + existing spot-price tests**

Run:
```
cargo test -p tycho-simulation --lib test_spot_price_lazy_computes_on_miss 2>&1 | tail
cargo test -p tycho-simulation --lib evm::protocol::vm::state 2>&1 | tail -20
```
Expected: new test passes; existing tests still pass.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
git add crates/tycho-simulation/src/evm/protocol/vm/state.rs
git commit -m "feat(sim): compute spot price lazily on cache miss"
```

---

## Task 4: Cache-through `get_amount_limits`

**Files:** Modify `state.rs`; tests in the same file.

- [ ] **Step 1: Write a failing test (cached == fresh)**

Add:
```rust
    #[tokio::test]
    async fn test_get_limits_cached_matches_fresh() {
        let pool_state = setup_pool_state().await;
        let overwrites = pool_state
            .get_overwrites(vec![dai_addr(), bal_addr()], *MAX_BALANCE / U256::from(100))
            .unwrap();

        // First call populates the cache.
        let fresh = pool_state
            .get_amount_limits(vec![dai_addr(), bal_addr()], Some(overwrites.clone()))
            .unwrap();
        assert_eq!(
            pool_state
                .limit_cache
                .read()
                .unwrap()
                .get(&(dai_addr(), bal_addr()))
                .copied(),
            Some(fresh)
        );

        // Second call returns the cached value, identical to the first.
        let cached = pool_state
            .get_amount_limits(vec![dai_addr(), bal_addr()], Some(overwrites))
            .unwrap();
        assert_eq!(fresh, cached);
    }
```

- [ ] **Step 2: Run it, confirm it fails**

Run: `cargo test -p tycho-simulation --lib test_get_limits_cached_matches_fresh 2>&1 | tail`
Expected: FAIL — `limit_cache` is empty after the call (no caching yet), so the `assert_eq!` on the
cache contents fails.

- [ ] **Step 3: Implement cache-through in `get_amount_limits`**

`get_amount_limits` (state.rs:342) currently calls the adapter directly. Replace its body:
```rust
    fn get_amount_limits(
        &self,
        tokens: Vec<Address>,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<(U256, U256), SimulationError> {
        let key = (tokens[0], tokens[1]);
        if let Some(limits) = self
            .limit_cache
            .read()
            .expect("limit_cache poisoned")
            .get(&key)
            .copied()
        {
            return Ok(limits);
        }
        let limits = self
            .adapter_contract
            .get_limits(&self.id, tokens[0], tokens[1], overwrites)?;
        self.limit_cache
            .write()
            .expect("limit_cache poisoned")
            .insert(key, limits);
        Ok(limits)
    }
```

- [ ] **Step 4: Run the test + the existing limits test**

Run:
```
cargo test -p tycho-simulation --lib test_get_limits_cached_matches_fresh 2>&1 | tail
cargo test -p tycho-simulation --lib evm::protocol::vm::state::tests::test_get_amount_limits 2>&1 | tail
```
Expected: both pass. (`test_get_amount_limits` asserts exact limits `100279494253364362835` and
`13997408640689987484` — unchanged.)

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
git add crates/tycho-simulation/src/evm/protocol/vm/state.rs
git commit -m "feat(sim): cache get_amount_limits per (sell, buy) pair"
```

---

## Task 5: `get_amount_out` — invalidate child caches instead of eager recompute

**Files:** Modify `state.rs`; tests in the same file.

- [ ] **Step 1: Update the existing get_amount_out tests to lazy semantics**

In `test_get_amount_out`, replace the eager-era assertion:
```rust
        assert_ne!(
            *new_state.spot_price_cache.read().unwrap(),
            *pool_state.spot_price_cache.read().unwrap()
        );
```
with an assertion that the post-swap lazily-computed spot price differs from the pre-swap one (the
swap moved the price), keeping the exact `amount` assertion (`137780051463393923`) intact:
```rust
        // new_state has an invalidated (empty) cache; reading recomputes the post-swap price.
        assert!(new_state
            .spot_price_cache
            .read()
            .unwrap()
            .is_empty());
        let pre = pool_state.spot_price(&dai(), &bal()).unwrap();
        let post = new_state.spot_price(&dai(), &bal()).unwrap();
        assert_ne!(pre, post);
```
In `test_sequential_get_amount_outs`, replace both `assert_ne!(... spot_prices ...)` similarly:
after each swap assert the new state's cache is empty and that `spot_price(&dai(), &bal())` differs
from the prior state's, keeping the exact `amount` assertions (`137780051463393923`,
`136964651490065626`) intact.

- [ ] **Step 2: Run them, confirm they fail**

Run: `cargo test -p tycho-simulation --lib test_get_amount_out test_sequential_get_amount_outs 2>&1 | tail`
Expected: FAIL — currently `get_amount_out` eagerly fills `new_state`'s cache, so the
`is_empty()` assertion fails.

- [ ] **Step 3: Replace the eager recompute with cache invalidation**

In `get_amount_out` (state.rs:608), find the block that builds `new_state` and recomputes spot prices
(state.rs:669-674):
```rust
        // Update spot prices
        let tokens = HashMap::from([
            (token_in.address.clone(), token_in.clone()),
            (token_out.address.clone(), token_out.clone()),
        ]);
        let _ = new_state.set_spot_prices(&tokens);
```
Replace it with cache invalidation (the swap changed pool state, so both caches are stale):
```rust
        // Invalidate the derived caches: the swap changed pool state, so spot prices and limits
        // must be recomputed lazily on next read against `new_state`'s post-swap overwrites.
        new_state
            .spot_price_cache
            .write()
            .expect("spot_price_cache poisoned")
            .clear();
        new_state
            .limit_cache
            .write()
            .expect("limit_cache poisoned")
            .clear();
```
Leave the rest of `get_amount_out` unchanged (the `get_amount_limits(self)` HardLimits call earlier in
the function now benefits from the cache automatically).

- [ ] **Step 4: Run the updated tests + dust/limit tests**

Run:
```
cargo test -p tycho-simulation --lib test_get_amount_out test_sequential_get_amount_outs test_get_amount_out_dust test_get_amount_out_sell_limit 2>&1 | tail -20
```
Expected: all pass. Amounts unchanged.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
git add crates/tycho-simulation/src/evm/protocol/vm/state.rs
git commit -m "feat(sim): stop eager spot-price recompute in get_amount_out, invalidate caches"
```

---

## Task 6: `update_pool_state` — clear caches then eager warm

**Files:** Modify `state.rs`; tests in the same file.

- [ ] **Step 1: Add cache-clear before the eager warm**

In `update_pool_state` (state.rs:364), it currently clears `block_lasting_overwrites`, updates
balances, then calls `self.set_spot_prices(tokens)?`. Add an explicit clear of both caches near the
start (right after the existing `self.block_lasting_overwrites.clear();`):
```rust
        self.spot_price_cache
            .write()
            .expect("spot_price_cache poisoned")
            .clear();
        self.limit_cache
            .write()
            .expect("limit_cache poisoned")
            .clear();
```
The existing `self.set_spot_prices(tokens)?` at the end then re-warms `spot_price_cache`, and because
`set_spot_prices` → `compute_spot_price` → `get_amount_limits` is now cache-through, `limit_cache` is
re-warmed as a side effect. No other change.

- [ ] **Step 2: Add a test that delta_transition warms both caches**

Add:
```rust
    #[tokio::test]
    async fn test_update_pool_state_warms_caches() {
        let mut pool_state = setup_pool_state().await;
        let tokens = HashMap::from([
            (dai().address.clone(), dai()),
            (bal().address.clone(), bal()),
        ]);
        let balances = Balances {
            component_balances: HashMap::new(),
            account_balances: HashMap::new(),
        };
        pool_state
            .update_pool_state(&tokens, &balances)
            .unwrap();
        assert!(!pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .is_empty());
        assert!(!pool_state
            .limit_cache
            .read()
            .unwrap()
            .is_empty());
    }
```
(If `Balances` is not already imported in the test module, add
`use tycho_common::simulation::protocol_sim::Balances;` to the test `use` block — check first; the
production code already imports it.)

- [ ] **Step 3: Run it + the existing delta test**

Run:
```
cargo test -p tycho-simulation --lib test_update_pool_state_warms_caches test_balance_merging_during_delta_transition 2>&1 | tail
```
Expected: both pass.

- [ ] **Step 4: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
git add crates/tycho-simulation/src/evm/protocol/vm/state.rs
git commit -m "feat(sim): invalidate and re-warm caches in update_pool_state"
```

---

## Task 7: Extend the concurrency equivalence test to spot_price

**Files:** Modify `crates/tycho-simulation/tests/vm_concurrency.rs`

- [ ] **Step 1: Add a concurrent spot_price equivalence test**

Append a second `#[test]` to `tests/vm_concurrency.rs` that hammers `spot_price()` from 8 threads on a
COLD cache and compares to a single-threaded oracle (this specifically guards the read-through cache
races introduced in Task 3):
```rust
#[test]
fn concurrent_spot_price_matches_single_threaded_oracle() {
    let pools = common::load_pools("balancer_v2_2token");
    assert_eq!(pools.len(), 1);
    let state = pools.into_values().next().unwrap();
    let (t_in, t_out) = common::pool_tokens("balancer_v2_2token");

    // Oracle from a fresh (cold-cache) clone, single-threaded.
    let oracle = state
        .spot_price(&t_in, &t_out)
        .expect("oracle spot price");

    // Many threads racing to lazily populate the same pair must all agree with the oracle.
    let state = std::sync::Arc::new(state);
    let t_in = std::sync::Arc::new(t_in);
    let t_out = std::sync::Arc::new(t_out);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let (state, t_in, t_out) = (state.clone(), t_in.clone(), t_out.clone());
        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                let got = state.spot_price(&t_in, &t_out).expect("spot price");
                assert_eq!(got, oracle, "concurrent spot price diverged");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }
}
```
NOTE: computing the oracle on `state` warms its cache before the threads run; since all reads of the
same pair return the cached value, they trivially match. To also exercise the cold-miss race, the
threads read the SAME pair — idempotent inserts guarantee agreement. This is the intended guard.

- [ ] **Step 2: Run it (and the existing test)**

Run: `cargo test -p tycho-simulation --test vm_concurrency 2>&1 | tail`
Expected: 2 tests pass.

- [ ] **Step 3: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy -p tycho-simulation --all-targets 2>&1 | tail -5
git add crates/tycho-simulation/tests/vm_concurrency.rs
git commit -m "test(sim): add concurrent spot_price equivalence test"
```

---

## Task 8: Benchmark against baselines (no code)

**Files:** none (measurement + record results in the spec's baseline doc).

- [ ] **Step 1: tycho-sim benches vs `before`**

Run from the worktree:
```
cargo bench --bench get_amount_out -- --baseline before 2>&1 | tail -30
cargo bench --bench concurrency_throughput -- --baseline before 2>&1 | tail -20
```
Expected: `get_amount_out` shows a large improvement (largest on curve_3token/curve_4token);
`concurrency_throughput` shows no regression. Capture the `change: [-x% .. -y%]` lines.

- [ ] **Step 2: Fynd routing bench vs `before`**

Run from the Fynd worktree:
```
cd /home/dev/projects/propellerheads/fynd-vm-sim-bench
cargo bench -p fynd-core --bench routing_sim -- --baseline before 2>&1 | tail -20
```
Expected: routing path shows improvement. Capture the change lines.

- [ ] **Step 3: Record the deltas**

Append an "## After lazy caching (deltas vs before)" section to
`/home/dev/projects/propellerheads/tycho-indexer-vm-sim-bench/docs/superpowers/specs/2026-06-18-vm-sim-lazy-caching-design.md`
with the captured percentage changes for each bench id. Commit it in the tycho-sim worktree:
```bash
cd /home/dev/projects/propellerheads/tycho-indexer-vm-sim-bench
git add docs/superpowers/specs/2026-06-18-vm-sim-lazy-caching-design.md
git commit -m "docs: record lazy-caching benchmark deltas"
```

---

## Self-Review notes

- **Spec coverage:** caches + Clone (T1), lazy spot_price (T3) + helper (T2), limit caching (T4),
  get_amount_out invalidation (T5), per-block eager warm + invalidation (T6), concurrency test (T7),
  benchmark deltas (T8). The limit-validity assumption is guarded by T4's cached==fresh test; the
  equivalence bar (unchanged amounts/prices) is enforced by keeping every exact-value assertion in the
  existing tests (T1/T2/T4/T5).
- **Future work (V2 background warming)** is intentionally NOT implemented (spec §Future work).
- **Type consistency:** field names `spot_price_cache`/`limit_cache`, helper `compute_spot_price`,
  cache key `(Address, Address)`, limit value `(U256, U256)` are used consistently across T1-T7.
  `set_spot_prices` stays `&mut self`; `spot_price`/`get_amount_limits`/`get_amount_out` stay `&self`
  (interior mutability via the locks).
