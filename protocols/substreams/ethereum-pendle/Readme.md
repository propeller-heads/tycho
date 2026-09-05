# Ethereum Pendle Substreams

This Substreams package indexes Pendle V2 on Ethereum. It emits two component types under one
protocol system, and the state each needs to be quoted natively:

- **`pendle_market`** — the SY↔PT AMM. Reserves (`total_pt`, `total_sy`) are replayed from
  `Swap`/`Mint`/`Burn` rather than read from storage, because the events are identical across all
  five market generations and the storage layout is not.
- **`pendle_sy`** — the ERC-5115 wrapper behind a market, with one attribute per quotable
  `(token, direction)` pair naming the conversion it uses.

## Factories

Five factories are indexed. The original (`0x27b1dAcd…`) emits a four-parameter `CreateNewMarket`
and keys its fee config by *router*; V3 through V6 share one ABI and key fee overrides by
`(market, router)`. See `src/consts.rs` for the addresses and `src/fees.rs` for both generations'
resolution rules.

Fee resolution is deliberately *read back* from the factory rather than re-derived in Rust — the
events say which markets moved and when, and `getMarketConfig` says what to.

## SY classification

An SY wraps a different protocol per instance, so there is no single deposit/redeem formula. At
component creation each `(SY, token)` pair is probed once with `previewDeposit` / `previewRedeem`
and matched against two closed forms:

- `one_to_one` — one token unit is one SY unit, modulo decimals.
- `index_rate` — converted at `exchangeRate()`, like an ERC-4626 share.

A pair matching neither is left out of the component, so an unquotable edge is **absent** rather
than silently wrong. See `src/sy.rs`.

## The refresh, and its one parameter

`SY.exchangeRate()` has no event stream — it moves with whatever protocol the SY wraps. So it is
read once per refresh block for every SY behind a live market, in one batched `eth_call`, and
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
backfill time. `0` is rejected rather than reinterpreted.

The trade is freshness only. SY balances are read off chain on any block a `Transfer` moved one,
not only on refresh blocks, so a coarse interval leaves the PY index stale without leaving balances
wrong. What the refresh alone covers is a *rebase*, which moves a balance with no transfer to
notice.

## Module graph

One handler per file under `src/modules/`, prefixed with its stage. A module at stage N reads only
from stages below it, so modules sharing a stage run in parallel and the manifest lists them in the
same order.

## Testing

```bash
cargo test                      # unit tests, native target
cargo build --target wasm32-unknown-unknown --release
substreams-tycho-test           # range tests, see integration_test.tycho.yaml
```

The range tests cover three windows: the original factory's first markets (2022), the reference
wstETH market's creation (2023), and the current V6 factory (2025). Execution is skipped on the two
historical windows — Pendle Router V4 has no code before block 19759272, and no window contains both
one of those markets being created and a live router.
