---
name: update-gas-test-metadata
description: >
  Keep crates/tycho-execution/.gas-compare/test_metadata.json in sync with the high-level TychoRouter test files.
  Scans crates/tycho-execution/contracts/test/TychoRouter*.t.sol files, diffs against the existing JSON, and
  adds/removes/updates entries. Use when tests have been added, removed, or renamed.
  Trigger on: "update test metadata", "sync test metadata", "update gas test list",
  "new tests added", or invokes /update-gas-test-metadata.
allowed-tools: Read, Write, Edit, Glob, Grep
---

# Update Gas Test Metadata

Keep `crates/tycho-execution/.gas-compare/test_metadata.json` in sync with the current high-level TychoRouter test files.

## When to Use

- After adding a new test to any `TychoRouter*.t.sol` file
- After removing or renaming a test
- After adding a new executor or protocol to an existing test
- When the gas-compare skill reports that test_metadata.json is missing

## Workflow

### Step 1: Read the current metadata

Read `crates/tycho-execution/.gas-compare/test_metadata.json`. Note the `last_updated` date and the current set of test names.

### Step 2: Scan the test files

Read each of these files in parallel:
- `crates/tycho-execution/contracts/test/TychoRouterSingleSwap.t.sol`
- `crates/tycho-execution/contracts/test/TychoRouterSequentialSwap.t.sol`
- `crates/tycho-execution/contracts/test/TychoRouterSplitSwap.t.sol`
- `crates/tycho-execution/contracts/test/TychoRouterFees.t.sol`
- `crates/tycho-execution/contracts/test/TychoRouterChecks.t.sol`
- `crates/tycho-execution/contracts/test/TychoRouterProtocolIntegration.t.sol`

For each `function test...` found, determine:

**`router_function`** — the TychoRouter entry-point called in the test body. One of:
`singleSwap`, `singleSwapPermit2`,
`sequentialSwap`, `sequentialSwapPermit2`,
`splitSwap`, `splitSwapPermit2`,
`exposedSplitSwap`, `exposedSequentialSwap`

**`protocols`** — sorted, deduplicated list. Infer from executor variables and encode helpers used in the test body:

| Variable / helper | Protocol |
|---|---|
| `usv2Executor` | `UniswapV2` |
| `usv3Executor` | `UniswapV3` |
| `usv4Executor` | `UniswapV4` |
| `balancerv2Executor` | `BalancerV2` |
| `balancerv3Executor` | `BalancerV3` |
| `curveExecutor` | `Curve` |
| `ekuboExecutor` | `Ekubo` |
| `rocketpoolExecutor` | `Rocketpool` |
| `bebopExecutor` | `Bebop` |
| `hashflowExecutor` | `Hashflow` |
| `erc4626Executor` | `ERC4626` |
| `wethExecutor` | `WETH` |
| `fluidExecutor` | `FluidV1` |
| `maverickExecutor` | `MaverickV2` |
| `slipstreamsExecutor` | `Slipstreams` |

For tests that load calldata from a file (`loadCallDataFromFile("test_...")`), infer protocols from the filename:
- `test_uniswap_v3_curve` → `[Curve, UniswapV3]`
- `test_balancer_v2_uniswap_v2` → `[BalancerV2, UniswapV2]`
- `test_multi_protocol` → `[BalancerV2, Curve, Ekubo, UniswapV2, UniswapV4]`
- single-protocol filenames → that protocol only

**`skipped`** — `true` if the test body contains `vm.skip(true)` (omit field if false).

**Exclude**:
- Tests that contain `vm.expectRevert` — error-path tests, not benchmarks
- Tests that do not call any router entry-point: pure admin/config tests and tests in `TychoRouter.t.sol` that only test access control

### Step 3: Diff and report

Compare the scanned tests against the existing JSON:

- **Added**: tests in the files but not in the JSON
- **Removed**: tests in the JSON but no longer in the files
- **Changed**: tests where `router_function` or `protocols` differ

Print a summary of changes before writing, e.g.:

```
Added (2):
  testNewFeatureSwap  [TychoRouterSingleSwap.t.sol]  singleSwap  [UniswapV2]
  testAnotherTest     [TychoRouterFees.t.sol]         singleSwap  [UniswapV2]

Removed (1):
  testOldTest  [TychoRouterSingleSwap.t.sol]

Changed (1):
  testUSV3CurveIntegration: protocols [UniswapV3] → [Curve, UniswapV3]
```

If there are no changes, say so and stop — do not write the file.

### Step 4: Write the updated metadata

If there are changes, write the updated JSON to `crates/tycho-execution/.gas-compare/test_metadata.json`. Update `last_updated` to today's date. Preserve all existing entries that are unchanged; add new ones at the end of their file's group; remove deleted ones.

Keep entries sorted: by file (in the order the source files are listed in Step 2), then alphabetically by test name within each file.

## Notes

- Protocols are always sorted alphabetically and deduplicated per test
- We assume protocols do not change between branches — no need to re-run when switching to the base branch for gas comparison
- The `note` field in the JSON explains what is excluded; do not change it
