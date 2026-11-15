# Gas Compare Learnings

## Why Per-Test `--gas-report`, Not `forge test --json`

`forge test --json` reports gas for the entire test body — including deal(), approve(), assertions, and other test overhead. For `testSingleSwapTransferFrom`:
- `forge test --json` → 303,363 gas (full test body)
- `forge test --gas-report` → 139,900 gas (just the singleSwap call)

The ~163k difference is test overhead, NOT swap cost. We want the swap cost only.

**Solution**: Run `forge test --gas-report --json --match-test <testName>` per test. When only one test runs, the gas report stats for the router function reflect exactly that test's call.

## Forge Gas Report Output

- `forge test --gas-report --json` outputs a JSON **array** of `{contract, deployment, functions}` objects
- Each `functions` entry has per-function stats: `{min, mean, median, max, calls}`
- When run with `--match-test` for a single test, calls=1 and min=mean=median=max
- Exit code 1 does NOT mean no output — some tests may fail while others succeed
- JSON may have non-JSON prefix text; find the first `[` to parse

## Why NOT Bulk `--gas-report`

Running all tests at once with `--gas-report` aggregates ALL invocations of a function (e.g., `singleSwap`) across ALL tests into one set of stats. You can't tell which test contributed which gas value. Running per-test is slower but gives exact per-test gas.

## Test Matching

- `forge test --match-test <name>` uses substring/regex matching (no `^...$` anchors needed)
- Add `--match-path "test/TychoRouter*"` to avoid matching protocol tests in `test/protocols/`
- Router test names are unique enough to match without anchors

## tycho-execution Test Structure

- Router tests: `TychoRouterSingleSwap.t.sol`, `TychoRouterSequentialSwap.t.sol`, `TychoRouterSplitSwap.t.sol`
- Fee tests: `TychoRouterFees.t.sol`
- Router entry-check tests: `TychoRouterChecks.t.sol`
- Integration tests: `TychoRouterProtocolIntegration.t.sol`
- Protocol tests in `protocols/` dir: `*ExecutorTest` (direct) vs `TychoRouterFor*Test` (integration through router)
- Fork tests need `RPC_URL` env var — check `crates/tycho-execution/contracts/.env` for it
- ~40 tracked tests, ~5-10 minutes per branch when run individually

## Git Worktree for Base Branch

- Worktrees work well for running tests on another branch without stashing
- Must run `git submodule update --init --recursive` in the worktree — contracts lib/ depends on submodules
- Clean up with `git worktree remove <path> --force`

## Important: Run from Repo Root

- Run `gas_compare.py` from the repo root, not from `crates/tycho-execution/contracts/`
- `--foundry-dir ./crates/tycho-execution/contracts` resolves relative to cwd
- Running from `crates/tycho-execution/contracts/` causes the path to resolve incorrectly

## Saved JSON Format

- Current format: `{testName: gasValue}` simple dict
- Old formats are incompatible: arrays from `--gas-report` bulk runs, objects from `forge test --json`
- Script does not auto-detect old formats — user must delete old files and re-run
