---
name: gas-compare
description: >
  Compare per-test gas between the current branch and a base branch (default: main).
  Uses `forge test --json` to extract gas per test, then cross-references with
  test_metadata.json to produce per-test comparison tables grouped by source file.
  Router gas includes executor delegatecall cost, so it represents the full swap cost.
  Use when user says "gas compare", "gas report", "gas diff", "compare gas", "gas regression",
  "gas benchmark", or invokes /gas-compare.
allowed-tools: Bash(forge:*), Bash(git:*), Bash(uv:*), Bash(python3:*), Bash(cat:*), Bash(mkdir:*), Read, Write, Glob, Grep
argument-hint: "[--base-branch BRANCH] [--save]"
---

# Gas Compare

Compare per-test gas between the current branch and a base branch.

## When to Use

- Before opening a PR, to check gas impact of your changes
- After refactoring router/executor code
- To benchmark gas improvements across protocol integrations
- To catch gas regressions early

## Prerequisites

- `forge` installed (Foundry)
- Repo must be a git repository with a `crates/tycho-execution/contracts/` directory containing `foundry.toml`
- `RPC_URL` environment variable or `crates/tycho-execution/contracts/.env` file (auto-loaded). Required — most tests fork mainnet. The script will error if missing.
- **Run from the repo root** (not from `crates/tycho-execution/contracts/`). The script auto-detects the foundry dir.

## Workflow

### Step 1: Determine parameters

Parse `$ARGUMENTS`:
- `--base-branch <branch>`: base branch to compare against (default: `main`)
- `--save`: save the report to `crates/tycho-execution/.gas-compare/` in the repo

If no arguments, use defaults.

### Step 2: Ensure test_metadata.json exists

Check whether `crates/tycho-execution/.gas-compare/test_metadata.json` exists in the repo root.

**If it exists**: proceed to Step 3.

**If it does not exist**: build it now by reading each high-level router test file in `crates/tycho-execution/contracts/test/`:
- `TychoRouterSingleSwap.t.sol`
- `TychoRouterSequentialSwap.t.sol`
- `TychoRouterSplitSwap.t.sol`
- `TychoRouterFees.t.sol`
- `TychoRouterChecks.t.sol`
- `TychoRouterProtocolIntegration.t.sol`

For each `function test...` in these files, record:
- `"test"`: the function name
- `"file"`: the filename (not the full path)
- `"router_function"`: the TychoRouter entry-point called — one of `singleSwap`, `singleSwapPermit2`, `sequentialSwap`, `sequentialSwapPermit2`, `splitSwap`, `splitSwapPermit2`, `exposedSplitSwap`, `exposedSequentialSwap`
- `"protocols"`: sorted, deduplicated list of protocol names used — infer from executor variable names (`usv2Executor` → `UniswapV2`, `usv3Executor` → `UniswapV3`, `usv4Executor` → `UniswapV4`, `balancerv2Executor` → `BalancerV2`, `balancerv3Executor` → `BalancerV3`, `curveExecutor` → `Curve`, `ekuboExecutor` → `Ekubo`, `rocketpoolExecutor` → `Rocketpool`, `bebopExecutor` → `Bebop`, `erc4626Executor` → `ERC4626`, `wethExecutor` → `WETH`) or from calldata file names loaded via `loadCallDataFromFile`
- `"skipped"`: `true` if the test uses `vm.skip(true)` (omit otherwise)

Exclude:
- Tests that contain `vm.expectRevert` — error-path tests, not benchmarks
- Tests that do not call any router entry-point (pure admin/config tests and tests in `TychoRouter.t.sol` that only test access control)

Write the result to `crates/tycho-execution/.gas-compare/test_metadata.json` in this format:

```json
{
  "last_updated": "<today's date>",
  "note": "High-level TychoRouter swap tests only. Excluded: tests that expect a revert, admin/config tests, and GasTest.t.sol.",
  "tests": [
    {
      "test": "testSingleSwapPermit2",
      "file": "TychoRouterSingleSwap.t.sol",
      "router_function": "singleSwapPermit2",
      "protocols": ["UniswapV2"]
    },
    ...
  ]
}
```

We assume protocols have not changed between branches, so there is no need to regenerate this file when switching to the base branch for comparison.

### Step 3: Run the comparison

The script runs `forge test --gas-report --json --match-test <testName>` **one test at a time**. For each test, it extracts the gas for the specific router function that test calls. This gives the exact gas cost of the swap — no setUp or test body overhead (deal, approve, assertions).

This is slower than running all tests at once (~5-10 minutes per branch for ~40 tests), but produces accurate per-test gas.

```bash
python3 <skill_dir>/scripts/gas_compare.py \
    --foundry-dir ./crates/tycho-execution/contracts \
    --metadata crates/tycho-execution/.gas-compare/test_metadata.json \
    --base-branch main \
    --save-json crates/tycho-execution/.gas-compare \
    --output crates/tycho-execution/.gas-compare/report.md
```

This will:
1. For each test in test_metadata.json, run `forge test --gas-report --json --match-test <testName>` on the current branch
2. Create a temporary git worktree for the base branch
3. Run the same per-test gas collection in the worktree
4. Clean up the worktree
5. Generate a per-test comparison report

If per-test results have already been saved (`crates/tycho-execution/.gas-compare/*.json`), reuse them:

```bash
# Reuse saved results (skip re-running tests)
python3 <skill_dir>/scripts/gas_compare.py \
    --current crates/tycho-execution/.gas-compare/current_branch.json \
    --base crates/tycho-execution/.gas-compare/main.json \
    --metadata crates/tycho-execution/.gas-compare/test_metadata.json \
    --output crates/tycho-execution/.gas-compare/report.md
```

**Important**: Saved JSON files are now `{testName: gas}` dicts. Old format files (arrays from `--gas-report --json` bulk runs, or objects from `forge test --json`) are incompatible — delete them and re-run.

### Step 4: Present the gas report

Read the generated markdown report, then **rewrite it for the user** following the presentation rules below. Do NOT just paste the raw report — curate it.

#### Report structure

The report has two sections:

1. **Summary table** (top): Mean gas per router function, aggregated from per-test data. Columns: `Function | Main (mean) | Branch (mean) | Diff`.

2. **Per-file tables**: One table per test source file. Each row is one test. Columns: `Test | Router Function | Protocols | Main | Branch | Diff`. Sorted alphabetically by test name within each file. Skipped tests omitted.

#### How to present

Show the **summary table first** with actual gas values (not percentages). After the summary table, add a one-line **Pattern** summary describing the trend (e.g., "All entry points increase ~3-10k due to added balance checks.").

Then show the **per-file tables** exactly as generated — one section heading per file, one row per test.

End with a **Summary** paragraph: what the overall gas impact is, what areas improved, what areas regressed, and why (based on the branch name and what changed).

#### Example output format

```
## Gas Comparison: `my-branch` vs `main`

## Summary (mean per router function)

| Function | Main (mean) | Branch (mean) | Diff |
|---|---:|---:|---:|
| singleSwap | 285,753 | 291,543 | +5,790 |
| singleSwapPermit2 | 333,180 | 336,036 | +2,856 |
| sequentialSwap | 369,795 | 374,406 | +4,611 |
...

**Pattern**: All entry points increase ~3-10k. Permit2 variants show smaller increases.

### TychoRouterSingleSwap.t.sol

| Test | Router Function | Protocols | Main | Branch | Diff |
|---|---|---|---:|---:|---:|
| testSingleSwapIntegration | singleSwap | UniswapV2 | 283,456 | 289,246 | +5,790 |
| testSingleSwapPermit2 | singleSwapPermit2 | UniswapV2 | 333,180 | 336,036 | +2,856 |
...

### TychoRouterSequentialSwap.t.sol
...

**Summary**: This branch adds balance checks in the dispatcher, costing ~3-10k additional gas per swap.
```

### Step 5: Sanity-check suggestions

After the report, suggest specific `forge test` commands for the largest changes:

```bash
# Verbose gas trace for the biggest regression
forge test --match-test <testName> -vvvv

# Gas snapshot diff
forge snapshot --diff
```

## Options

- `--foundry-dir PATH`: path to the foundry project (auto-detected from cwd)
- `--base-branch BRANCH`: branch to compare against (default: `main`)
- `--metadata PATH`: path to test_metadata.json (default: `crates/tycho-execution/.gas-compare/test_metadata.json`)
- `--current FILE`: pre-generated forge test --json for current branch (skip running tests)
- `--base FILE`: pre-generated forge test --json for base branch (skip running tests)
- `--output FILE`: write report to file instead of stdout
- `--save-json DIR`: save raw JSON results for reuse
- `--forge-args ...`: extra arguments passed to `forge test`

## Notes

- Runs `forge test --gas-report --json --match-test <testName>` per test to get exact router function gas
- Gas values are the **router function call only** — no setUp, deal, approve, or assertion overhead
- Each test is run individually (~5-10s each, ~5-10 minutes per branch for ~40 tests)
- The worktree approach means the base branch tests run in an isolated copy — no stashing needed
- The script requires no external Python dependencies (stdlib only)
- Saved JSON format is `{testName: gas}` — old bulk-run formats are incompatible
