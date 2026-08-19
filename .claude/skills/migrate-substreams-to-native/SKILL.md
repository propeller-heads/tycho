---
description: "Use when migrating or refactoring a substreams package into a native Rust processor crate — e.g. the user says 'migrate <protocol> to native', 'create a native processor', 'port the substreams to a core crate', or 'migrate <protocol> the same way uniswap-v3/v4 was migrated'. Covers the core crate, TxDeltaIndexer parity semantics, and the substreams parity integration test."
user-invocable: true
---

# Migrate a Substreams Package to a Native Processor Crate

Extract a protocol's substreams transform logic into a native crate at
`protocols/crates/<protocol>` whose processor implements `TxDeltaIndexer`
(`tycho_common::traits`), so pending-block simulation can decode raw EVM logs without a
Substreams runtime. The existing substreams package stays untouched and serves as ground
truth for a parity integration test.

**Reference implementations — mirror these, do not redesign:**
- `protocols/crates/uniswap-v4` (canonical: pool registry keyed by id, sqrt-price tracking,
  spkg-from-main test, `scripts/build_main_spkg.sh`)
- `protocols/crates/uniswap-v3` (factory-pattern pools keyed by contract address)
- `protocols/crates/uniswap-v2` (simplest: one `Sync` event carrying absolute reserves —
  no ticks/liquidity, no running accumulators, processor state is just the pool registry)

## Step 1: Understand the substreams package

Read every module under `protocols/substreams/<package>/src/modules/` (and `shared/` if the
package has variants). For each map/store module note: handled events, store update policies
(`add`, `set`, `set_sum`, `set_if_not_exists`), attribute names emitted, balance-delta
semantics, and any math helpers. The native crate must reproduce these byte-for-byte.

## Step 2: Create the crate

Copy the structure of `protocols/crates/uniswap-v4`:

| File | Content |
|---|---|
| `Cargo.toml` | Copy from uniswap-v4, rename. Crate name `<protocol>-core` |
| `src/abi/` | Copy the generated ABI file(s) from the substreams package verbatim; `mod.rs` gets `#![allow(clippy::all, clippy::pedantic, clippy::nursery)]` |
| `src/events.rs` | `Pool`, `TxRef`, `PoolEventKind`, decode fn using `match_and_decode` |
| `src/balance.rs`, `src/ticks.rs`, `src/liquidity.rs`, `src/output.rs` | Pure `event → delta` transforms ported 1:1 from the substreams modules |
| `src/math.rs` | Port math helpers (they already use `num_bigint`) including their unit tests; add `rstest` to dev-deps if the tests use it |
| `src/processor.rs` | `<Protocol>Processor` implementing `TxDeltaIndexer` — copy uniswap-v4's and adapt the tracked state |
| `tests/integration.rs` | Copy uniswap-v4's parity test and adapt |
| `scripts/build_main_spkg.sh` | Copy from uniswap-v4, swap package paths |

Add the crate to root `Cargo.toml` `[workspace.members]`, then run
`cargo check --workspace` to update `Cargo.lock` (never `cargo generate-lockfile`).

## Step 3: Parity semantics (where mismatches come from)

The substreams pipeline reads stores with `get_at(log_ordinal)`. The native equivalent is
**applying events sequentially in log order and mutating running state as you go** — state
consumed by an event must reflect all prior Initialize/Swap-type events in the same block.

- Ordinal: `tx.index() * 100_000 + log_index` (matches cross-tx event ordering)
- Balances: usually running `BigInt` accumulators per `(pool, token)`, emitting the
  absolute value **clamped to zero** (mirrors `tycho_substreams::balances`). But some
  protocols report absolute balances directly — uniswap-v2's `Sync` carries the reserves,
  so there is no accumulation and the balance is emitted as `reserve.to_signed_bytes_be()`
  (match the substreams' exact encoding: signed vs unsigned magnitude differs by a leading
  zero byte). Check what the substreams module actually emits.
- Tick net-liquidity change type, checked in this order: Creation if the previous running
  value was missing **or zero**; Deletion if the new value is zero; else Update
- Component ids: normalize incoming ids at the `apply_block` boundary (trim `0x`,
  lowercase) for internal registry keys, but emit ids from `generate_deltas` exactly as
  the substreams do (`0x`-prefixed lower-case hex) so they match decoder state keys —
  copy `normalize_id`/`emitted_id` from uniswap-v4's `processor.rs`
- `apply_block` must reconstruct ALL state the transforms consume (e.g. uniswap-v4 tracks
  `sqrt_price_x96` because ModifyLiquidity balance math depends on it)
- Pools created in block N are invisible to `generate_deltas` until `apply_block(N)` ran —
  production semantics; the test accounts for this via known-pools filtering

## Step 4: Parity integration test

The test streams the package's final map module from the spkg as ground truth and asserts
byte-identical attributes/balances per block. The module name varies — `map_protocol_changes`
for uniswap-v3/v4, `map_pool_events` for uniswap-v2; check the manifest. The per-block flow
(copy it from uniswap-v4's test, do not reinvent):

1. `generate_deltas(rpc_txs)` — pending deltas from raw `eth_getLogs`/block data
2. Compare against the aggregated substreams output, restricted to known pools
3. `apply_block(substreams ground truth)` — advance state and register new pools

**Aggregate the substreams side in ascending transaction-index order.** When a pool changes
in several transactions within one block, the block-final value is the highest-index
transaction's — which is what the processor produces (it orders by `tx.index()`). Some
substreams emit per-transaction changes in non-deterministic order (uniswap-v2's
`merge_block` collects via `into_values()`), so sort `proto.changes` by `tx.index` before
the last-write-wins aggregation, or the comparison flaps on multi-tx pools. (uniswap-v3/v4
already sort upstream, so their tests didn't need it.)

Two non-obvious setup points:

1. **Build the spkg from `origin/main`, not the working tree** (a branch that refactors the
   substreams to use the core crate would otherwise test the code against itself). Use the
   `scripts/build_main_spkg.sh` pattern: temp `git worktree` of origin/main → wasm build →
   `substreams pack` → `target/spkg/<package>-main.spkg`. The test defaults
   `*_SPKG_PATH` to that output.
2. **Pick an active block window.** Scan candidate 2000-block windows with `eth_getLogs`
   (count creation + activity events) and hard-code a default window that contains pool
   creations *followed by* swaps/liquidity changes — only pools created inside the window
   get compared. The default must be a full ~2000-block window; never reuse the tiny range
   from `integration_test.tycho.yaml`. Keep `*_START_BLOCK`/`*_STOP_BLOCK` env overrides.

Credentials (from repo-root `.env`): `ETH_RPC_URL` (archive node) and `STREAMINGFAST_KEY`.
The endpoint is `https://mainnet.eth.streamingfast.io:443` and `SubstreamsEndpoint` sends
the token raw — a `server_…` API key must first be exchanged for a JWT:

```bash
TOKEN=$(curl -s https://auth.streamingfast.io/v1/auth/issue \
  -d "{\"api_key\":\"$STREAMINGFAST_KEY\"}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')
STREAMINGFAST_KEY="$TOKEN" cargo test -p <protocol>-core --test integration -- --ignored --nocapture
```

## Step 5: Done criteria

- Parity test summary shows **0 attr and 0 balance mismatches** with meaningful coverage
  (tens of pools, hundreds of comparisons — widen the window if coverage is thin)
- `cargo test -p <protocol>-core --lib` green; `cargo clippy -p <protocol>-core
  --all-targets` warning-free; `cargo +nightly fmt`
- Do not modify the substreams package itself in this migration; if it must later consume
  the core crate (like `ethereum-uniswap-v3-logs-only`), that is a separate change with a
  version bump per `protocols/substreams/CLAUDE.md`

## Common mistakes

| Mistake | Fix |
|---|---|
| Testing against a working-tree spkg | Build from origin/main via the worktree script |
| Inventing env vars (`SUBSTREAMS_API_KEY` etc.) | Use `.env`'s `ETH_RPC_URL` + `STREAMINGFAST_KEY` (JWT-exchanged) |
| Default window at protocol genesis with no activity | Scan for an active window first (uniswap-v4 launch had ~0 events for 60k blocks) |
| Reading state "as of block start" for in-block events | Apply events sequentially; update running state between events |
| Deriving static attributes (fee→tick_spacing maps) | Take them from the creation event exactly as the substreams does |
| Test flaps on pools that change in multiple txs per block | Sort the substreams `proto.changes` by `tx.index` before aggregating (last write = highest index, matching the processor) |
| Emitting unsigned-magnitude balances when substreams emit signed | Match the substreams encoding exactly (`to_signed_bytes_be` vs `to_bytes_be().1`) |
