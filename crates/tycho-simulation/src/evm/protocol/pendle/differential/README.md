# Differential fixtures for the Pendle math port

A standalone Foundry project that evaluates Pendle's own Solidity over a grid of inputs and writes
the results to `../tests/fixtures/`. The Rust port asserts bit-equality against those files.

Nothing here reimplements anything. The contracts under `test/` call the upstream libraries and
record what they return; a port that is merely close is a port that disagrees with the contract on
some input, and a quote that disagrees with the contract is a wrong quote.

## Where the assertions live

The `.t.sol` files only *write*. Every comparison happens on the Rust side, in the `#[cfg(test)]`
modules of the ported modules, which `include_str!` the committed JSON and replay each row:

| Fixture | Written by | Asserted in |
|---|---|---|
| `exp.json`, `ln.json` | `test/LogExpMathFixtures.t.sol` | `math/log_exp_math.rs` |
| `pmath.json`, `pmath_reverts.json` | `test/PMathFixtures.t.sol` | `math/pmath.rs` |
| `sy_utils.json`, `sy_utils_signed.json`, `sy_utils_zero_index.json` | `test/SYUtilsFixtures.t.sol` | `math/sy_utils.rs` |
| `market.json`, `market_reverts.json`, `implied_rate.json` | `test/MarketMathFixtures.t.sol` | `math/market.rs` |
| `approx.json`, `approx_boundary.json`, `limits.json` | `test/ApproxFixtures.t.sol` | `math/approx.rs` |

Comparison is exact, with no tolerance. Recorded reverts are matched by error variant, so the port
has to fail for the same reason rather than merely fail.

## Regenerating

```bash
./regenerate.sh
```

It clones `pendle-core-v2-public`, `forge-std` and OpenZeppelin (pinned to `v4.9.6`, because an
unpinned major would change the interfaces underneath the fixtures) into `lib/`, then runs the
fixture contracts. `lib/`, `out/` and `cache/` are gitignored — the upstream sources are never
vendored into this tree. See [`../NOTICE.md`](../NOTICE.md) for why, and for the licensing question
behind the port.

CI never runs this harness. It runs `cargo test` against the committed fixtures, so regeneration is
only needed when the grid changes or when Pendle's math does. Re-run the Rust tests afterwards and
commit the fixtures with the change that prompted them.

`via_ir` is on in `foundry.toml`: a fixture row concatenates enough fields of a market state to
exhaust the stack without it.

## What the grids are chosen for

Each contract's doc comments carry the reasoning per case. The themes:

- **Branch boundaries, exactly and either side.** `exp` decomposes its argument into twelve powers
  of two and dispatches on `>=`, so each point is probed at `p - 1`, `p` and `p + 1`. `ln` is
  probed at the strict-inequality edges of its higher-precision `ln_36` window.
- **Block timestamp.** `rateScalar`, `rateAnchor` and `feeRate` move on every quote. A port that
  ignores `blockTime` passes every static test while being wrong all day, so market and approx rows
  sweep several timestamps across a market's life.
- **The decimal gap.** Two markets sit on opposite sides of it: one with SY and accounting asset
  both at 18 decimals, one with SY at 18 and the asset at 6, where the PY index carries the 1e12.
- **The fee configuration.** Both reference markets happen to share one fee root and one 80%
  reserve split, so twenty market rows hold the market fixed and move the fee instead — both ends
  of the split, and three roots including zero, where `feeRate` collapses to exactly one and the
  two branches of `calcTrade` stop being symmetric.
- **Intermediates, not just outputs.** Market rows record the whole `MarketPreCompute`; approx rows
  record the starting estimate and the iteration count. A divergence then localises to a step, and
  the search is pinned to the same route rather than only to the same answer — two implementations
  converging differently agree on the sampled inputs and part company on the rest.
- **The state the trade leaves behind.** Market rows also record the reserves and implied rate
  `executeTradeCore` writes, which is what the *next* quote is priced against. It is invisible in a
  single quote and compounds across a sequence, so a port that got it slightly wrong would pass
  every output assertion above while mispricing every split trade.
- **Both sides of a limit.** The approx boundary sweep records sizes that revert alongside sizes
  that fill; sweeping only what fills would leave the interesting half untested.
- **Operands that do not divide evenly.** The `PMath` and `SYUtils` grids are built out of them on
  purpose: both rounding directions agree on an exact division, so a grid of round numbers would
  pass against either one and prove nothing about the direction.

The Rust tests also assert a minimum row count per fixture, so a regeneration that silently
produced a smaller grid fails instead of passing vacuously. Current sizes: `exp` 80, `ln` 22,
`pmath` 85, `pmath_reverts` 7, `sy_utils` 160, `sy_utils_signed` 165, `sy_utils_zero_index` 4,
`market` 60, `market_reverts` 5, `implied_rate` 24, `approx` 48, `approx_boundary` 24, `limits` 6.
