# Ethereum Pendle Substreams

This Substreams package indexes Pendle V2 on Ethereum. It emits two component types under one
protocol system, and the state each needs to be quoted natively:

- **`pendle_market`** — the SY↔PT AMM. Reserves (`total_pt`, `total_sy`) are replayed from
  `Swap`/`Mint`/`Burn` rather than read from storage, because the events are identical across all
  five market generations and the storage layout is not.
- **`pendle_sy`** — the ERC-5115 wrapper behind a market. Its state carries one attribute per
  quotable `(token, direction)` pair naming the conversion it uses (`token_in_class_0x…`,
  `token_out_class_0x…`), its share supply (`sy_total_supply`), which bounds every redemption, and
  whether it is `paused`.

## Factories

Five factories are indexed. The original (`0x27b1dAcd…`) emits a four-parameter `CreateNewMarket`
and keys its fee config by *router*; V3 through V6 share one ABI and key fee overrides by
`(market, router)`. See `src/consts.rs` for the addresses and `src/fees.rs` for both generations'
resolution rules.

Fee resolution is deliberately *read back* from the factory rather than re-derived in Rust — the
events say which markets moved and when, and `getMarketConfig` says what to.

### Factory upgrades

The V6 factory (`0x6d247b1c…`) is a `TransparentUpgradeableProxy`; the other four are immutable.
The decoders are not version-guarded, so an implementation that changed `CreateNewMarket` or a fee
event would stop matching without an error: new markets would not be discovered and fees would
stop updating. `map_market_components` logs every event from a known factory that neither factory
ABI declares, with its topic0, so an upgrade, or a changed event, leaves a trace in the module
logs. Nothing acts on it yet. How upgrades should be handled (halting on the proxy's `Upgraded`
event, a version check at market creation, or pinning the implementation slot) is still open.

## SY classification

An SY wraps a different protocol per instance, so there is no single deposit/redeem formula. At
component creation each `(SY, token)` pair is probed once with `previewDeposit` / `previewRedeem`
and matched against two closed forms:

- `one_to_one` — one token unit is one SY unit, modulo decimals.
- `index_rate` — converted at `exchangeRate()`, like an ERC-4626 share.

A pair matching neither is left out of the component, so an unquotable edge is **absent** rather
than silently wrong. See `src/sy.rs`.

The classes are state, not static attributes: 251 of the 379 SYs are upgradeable proxies, and 31
have been upgraded after deployment. On an SY's `Upgraded` or `Unpaused` event, `map_sy_state`
re-runs the classification at the end of the block, re-emits the class of every listed token that
answers its probe and deletes those that answer but no longer match. A token whose probe reverts
keeps its class: upgrades usually happen while an SY is paused, and a paused SY reverts its
previews, so the re-run on `Unpaused` is the one that classifies such an upgrade. A token an
upgrade adds stays unquotable, since a component's token list is fixed at creation. `Paused` and
`Unpaused` set the SY's `paused` attribute, and `sy_paused` on every market priced against it: a
paused SY reverts every transfer, so none of those markets can trade either.

## What is read, and when

| Value | Read | Then follows |
|---|---|---|
| Classes, `paused` | Once, at creation | `Upgraded`, `Paused`, `Unpaused` |
| Market `sy_paused` | The SY's `paused()`, at market creation (from the SY profile) | The SY's `Paused`, `Unpaused` |
| SY supply | `totalSupply()` once, at creation | The SY's own mints and burns |
| SY supply cap | `getAbsoluteSupplyCap()` (or `supplyCap()`) once, at creation, kept only when it caps the SY's own supply | `SupplyCapUpdated` |
| Yield token | `yieldToken()` once, at creation | — |
| SY balances | `balanceOf` once per token, at creation | `Transfer`s, and WETH9's `Deposit` / `Withdrawal` |
| Market `pyIndexStored` | `pyIndexStored()` on the YT, once, at market creation | `NewInterestIndex` |
| Market fees | `getMarketConfig` once, at market creation | The factories' fee events |
| Exchange rate | Every refresh block | — |

Creation reads are batched per kind (market, SY units, decimals, probes, seeds). The rate is read
for the SY of each live market and for every SY that has ever had an `index_rate` token; an SY whose
tokens have only ever been `one_to_one` and whose markets have expired is never read again. The
flag is never cleared, because a re-classification cannot see the class of a token whose probe did
not answer.

Balances are never re-read. A rebasing token (stETH, aTokens) moves an SY's balance without a
`Transfer`, so the reported holding of one misses every rebase since creation, and one whose
withdrawals outrun the accounted total is reported as zero. The simulation reads one balance:
an SY's holding of its own yield token, which bounds a redemption to that token. Every other
redemption is bounded by supply. So a missed rebase affects that bound, the reported balances
and anything derived from them, such as TVL.

## The refresh, and its one parameter

`SY.exchangeRate()` has no event stream — it moves with whatever protocol the SY wraps. So it is
read once per refresh block for every SY that needs it (see above), in one batched `eth_call`, and
republished alongside two clocks:

- `rate_sampled_at` dates the rate, and stops advancing when a read stops resolving.
- `block_timestamp` dates the *look*, and advances on every refresh block regardless.

A consumer holding both can tell a rate that is still current from one the chain has moved past.

```yaml
params:
  map_protocol_changes: "sy_rate_refresh_blocks=1"
```

`sy_rate_refresh_blocks=1` reads every block and is what live indexing wants. A backfill from
`initialBlock` (16032059, the original factory's deployment) issues one batched `eth_call` per
refresh block and is dominated by that cost, so raise it there — the trade is index freshness for
backfill time. `0` is rejected rather than reinterpreted. Whatever the interval, a component is
refreshed on the block it is created, and a market on any block its yield token emits
`NewInterestIndex`, so neither waits for the next refresh to be quotable.

The trade is freshness only. Supply and balances follow events whatever the interval, so a coarse
interval leaves the PY index stale without leaving a redemption bound wrong.

## Module graph

One handler per file under `src/modules/`, prefixed with its stage. A module at stage N reads only
from stages below it, so modules sharing a stage run in parallel and the manifest lists them in the
same order.

## Testing

```bash
cargo test                      # unit tests, native target
cargo build --target wasm32-unknown-unknown --release
# range tests, see integration_test.tycho.yaml; from protocols/testing:
cargo run -- range --package ethereum-pendle
```

The range tests cover three windows: the original factory's first markets (2022), the reference
wstETH market's creation (2023), and the current V6 factory (2026). This package's yaml runs indexing
only; the simulation and execution PRs switch the rest on. Execution stays off on the two historical
windows: the planted router needs Cancun (transient storage), Pendle Router V4 has no code before
block 19759272, and no window contains both one of those markets being created and a live router.
