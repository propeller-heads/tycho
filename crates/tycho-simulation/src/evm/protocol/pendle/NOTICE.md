# Provenance and licensing of the Pendle math port

`math/log_exp_math.rs`, `math/pmath.rs`, `math/sy_utils.rs`, `math/market.rs` and `math/approx.rs`
are ports of Solidity from
[`pendle-finance/pendle-core-v2-public`](https://github.com/pendle-finance/pendle-core-v2-public):

| Rust module | Solidity source |
|---|---|
| `math/log_exp_math.rs` | `contracts/core/libraries/math/LogExpMath.sol` |
| `math/pmath.rs` | `contracts/core/libraries/math/PMath.sol` |
| `math/sy_utils.rs` | `contracts/core/libraries/SYUtils.sol`, `contracts/core/StandardizedYield/PYIndex.sol` |
| `math/market.rs` | `contracts/core/Market/MarketMathCore.sol` |
| `math/approx.rs` | `contracts/router/math/MarketApproxLibOnchain.sol`, `ApproxStateLib.sol`, `MarketApproxEstimateLib.sol` |

They are ports rather than reimplementations on purpose. The quote has to agree with the contract
bit for bit, including its rounding directions and its fixed-point intermediates, and the surest
way to get that is to follow the source line by line.

## The licence question, and the decision

This repository is **MIT** (`LICENSE`, `Cargo.toml`). The Pendle sources are not, and they are not
consistent with each other either:

- `PMath.sol` carries a plain `SPDX-License-Identifier: GPL-3.0-or-later` header with no other
  grant.
- `LogExpMath.sol` carries `SPDX-License-Identifier: GPL-3.0-or-later` **and**, immediately below
  it, the verbatim MIT permission notice. The two contradict each other. (The file is itself
  adapted from Balancer's `LogExpMath`, which is GPL-3.0.)

A line-by-line port of `LogExpMath` — its constants, its series decomposition, its 20-decimal
intermediate scheme — is a derivative work of the original by any reading, so this is a real
question rather than a formality.

**Decision: proceed with the port.** Raised with and confirmed by the repository owner on
2026-08-23, before any of the ported code was written. Recorded here so that a reviewer meets the
question at the same time as the code, rather than discovering the header later.

Elsewhere in this repository the same tension is resolved the other way: Balancer, also GPL, is
integrated as a precompiled VM adapter (`evm/protocol/vm/constants.rs`) rather than as a Rust port.
Two alternatives were on the table and were not taken — reimplementing `ln`/`exp` independently and
pinning it to the differential fixtures, or running the math in the VM engine. Both were rejected
in favour of the port; the first is not a true clean room once the source has been read, and the
second contradicts this being a native integration.

## Differential fixtures

`differential/` holds a Foundry harness that evaluates the Solidity originals over a grid and
writes JSON, which the Rust tests assert bit-equality against. It clones
`pendle-core-v2-public` into `differential/lib/` at run time; that directory is gitignored, so the
upstream sources are never vendored into this tree. The committed fixtures under `tests/fixtures/`
are what CI checks — the harness is only needed to regenerate them.
