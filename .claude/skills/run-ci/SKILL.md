---
allowed-tools: Bash(cargo:*), Bash(env:*), Bash(diesel:*), Bash(psql:*), Bash(bash .claude/scripts/run-nextest.sh:*), Bash(printenv:*), Bash(git diff:*), Bash(git branch:*), Bash(git status:*), Bash(git log:*), Read
description: "Run the full CI pipeline locally to catch failures before pushing. Use this skill before creating a PR, before pushing commits, or whenever you want to verify that CI will pass. Also use it when the user says 'run ci', 'check ci', 'run tests', 'lint', or 'will ci pass'."
user-invocable: true
---

# Run CI Locally

Run the same checks that GitHub Actions CI runs, locally, to catch failures before they hit the
remote pipeline. The canonical commands live in `.github/workflows/ci-rust.yaml`.

## Environment

Environment variables needed for DB tests can be configured in `.claude/settings.local.json`:

```json
{
  "env": {
    "DATABASE_URL": "postgres://postgres:mypassword@localhost:5431/tycho_indexer_0",
    "RPC_URL": "https://your-ethereum-archive-node"
  }
}
```

DB tests require both `DATABASE_URL` set AND a running Postgres instance (see
`crates/tycho-storage/README.md` for setup). Tests marked `#[ignore]` require `RPC_URL` pointing to an
Ethereum archive node with **debug APIs enabled** (Erigon, Reth with `--http.api debug`) —
standard providers like Alchemy/Infura do not support `debug_storageRangeAt` and will cause
failures. These tests are skipped unless `RPC_URL` is set.

## Context

- Current branch: !`git branch --show-current`
- Working tree status: !`git status --short`

## Workflow

### Phase 0: Check environment and run migrations

Check that `DATABASE_URL` and `RPC_URL` are set:

```bash
printenv DATABASE_URL
```
```bash
printenv RPC_URL
```

If `RPC_URL` is set, pass `--include-ignored` to all `run-nextest.sh` calls in Phase 3.
If `RPC_URL` is not set, omit the flag (ignored tests will be skipped).

If `DATABASE_URL` is not set, mark DB as unavailable:
- All `serial_db` tests will be **skipped**
- Unit tests will exclude `tycho-storage` and `diesel` tests
- Set a `DB_SKIPPED` flag for the final report

If `DATABASE_URL` IS set, run migrations to ensure the schema is up to date:

```bash
diesel migration run --migration-dir ./crates/tycho-storage/migrations
```

If migrations fail with "already exists" errors (stale schema), reset the database:

```bash
diesel database reset --migration-dir ./crates/tycho-storage/migrations
```

If the reset fails because other connections are active, terminate them first using `psql` then
retry the reset. If migrations still fail after reset (e.g. Postgres not running), mark DB as
unavailable with the same behavior as above.

### Phase 0.5: Scope detection

Determine which areas changed to avoid running unnecessary checks. Compare against the merge base
with `main`:

```bash
git diff --name-only $(git merge-base HEAD origin/main)..HEAD
```

If the branch IS `main` (no feature branch), or the command fails, default to **full scope** (all
checks run). Otherwise, map changed file patterns to check categories:

| File pattern | Category |
|---|---|
| `crates/**`, `protocols/testing/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` | `rust` |
| `crates/tycho-client-py/**` | `python` |
| `.github/workflows/ci-rust.yaml` | `ci` (always run full) |

If only `python` files changed, skip Rust format/clippy/tests entirely and only run Python checks.
If only `rust` files changed, skip Python checks. If both changed (or `ci`), run everything.

Report the detected scope before proceeding: `Scope: rust`, `Scope: python`, `Scope: rust + python`,
or `Scope: full`.

### Phase 1: Format (sequential) — skip if scope is `python` only

Run formatting first because it modifies source files that all subsequent checks depend on.

```bash
cargo +nightly fmt --all
```

Check `git diff --stat -- '*.rs'` and report whether any files were reformatted.

### Phase 2: Clippy (sequential, gate for tests) — skip if scope is `python` only

Run clippy next. If clippy fails, tests won't compile either, so there's no point running them.

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Report pass/fail. If there are warnings or errors, list them.

**If clippy fails, stop here.** Report the errors and skip Phase 3.

### Phase 2.5: Rustdoc and no-default-features checks

Run the remaining compile-time CI gates after clippy:

```bash
env RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc --workspace --no-deps --all-features --locked
env RUSTFLAGS="-Dwarnings" cargo check --workspace --no-default-features --locked
```

Report each command separately. If either fails, continue to tests so the final report includes all
independent failures.

### Phase 3: Parallel test checks

Only run this phase if clippy passed. Launch all test commands as **parallel foreground Bash calls
in a single message**. Do NOT use `run_in_background` — multiple Bash tool calls in one message
already execute concurrently.

**IMPORTANT**: Do NOT use `cargo nextest run` with `-E` / `--filter-expr` directly — the
parentheses in filter expressions break allowed-tools pattern matching and trigger permission
prompts. Use the wrapper script `.claude/scripts/run-nextest.sh` instead, which encapsulates
the filter expressions. Also do NOT pipe commands through `grep`, `tail`, or other commands.

#### Unit tests (parallel)

If DB is available:
```bash
bash .claude/scripts/run-nextest.sh unit                    # RPC_URL not set
bash .claude/scripts/run-nextest.sh unit --include-ignored  # RPC_URL set
```

If DB is NOT available (exclude tycho-storage and diesel round-trip tests):
```bash
bash .claude/scripts/run-nextest.sh no-db                    # RPC_URL not set
bash .claude/scripts/run-nextest.sh no-db --include-ignored  # RPC_URL set
```

Report pass/fail with test count summary (passed, failed, ignored).

#### DB tests (serial) — only if DB is available

```bash
bash .claude/scripts/run-nextest.sh serial-db                    # RPC_URL not set
bash .claude/scripts/run-nextest.sh serial-db --include-ignored  # RPC_URL set
```

Report pass/fail with test count summary. If DB is not available, skip entirely.

## Report

After all steps complete, provide a summary table. Combine results from both test runs (unit +
serial-db) to compute totals. Only report a test as "skipped" if it was genuinely not run at all
(e.g. DB unavailable, clippy failed). Do NOT count nextest filter exclusions as skipped — those
tests run in the other phase.

**How to compute the totals:**
- `passed` = unit passed + serial-db passed
- `failed` = unit failed + serial-db failed
- `skipped` = only tests that were NOT run in ANY phase (e.g. DB tests when DB is unavailable,
  or `#[ignore]`-d tests when `RPC_URL` is not set)

| Step     | Status            | Details                              |
|----------|-------------------|--------------------------------------|
| Scope    | detected          | rust / python / rust + python / full |
| Format   | pass/fail/skipped | files reformatted or clean           |
| Clippy   | pass/fail/skipped | warning/error count                  |
| Rustdoc  | pass/fail/skipped | broken intra-doc links               |
| No-default check | pass/fail/skipped | workspace builds without default features |
| Tests    | pass/fail/skipped | X passed, Y failed, Z skipped        |

If clippy failed, mark tests as "skipped (clippy failed)".

**If DB was not available**, add a warning above the table:

```
WARNING: DATABASE NOT AVAILABLE — serial_db, tycho-storage, and diesel round-trip tests were skipped.
```

Add a "skipped" count for the DB-dependent tests and tell the user how to enable them:

```
To run the full suite including DB tests:

1. Start Postgres:  docker-compose up -d db
2. Set DATABASE_URL (pick one):
   a) Per-session:    export DATABASE_URL="postgres://postgres:mypassword@localhost:5431/tycho_indexer_0"
   b) Persistent:     Add to .claude/settings.local.json:
                      { "env": { "DATABASE_URL": "postgres://postgres:mypassword@localhost:5431/tycho_indexer_0" } }
3. Run migrations:  diesel migration run --migration-dir ./crates/tycho-storage/migrations
4. Re-run:          /run-ci
```

**If `RPC_URL` was not set** and ignored tests were skipped, add a hint after the table:

```
To also run #[ignore]-d tests (archive node integration tests), set RPC_URL.
Requires a debug-enabled archive node (Erigon, Reth with --http.api debug).
Standard providers (Alchemy, Infura) will NOT work — they don't support debug_storageRangeAt.
  a) Per-session:    export RPC_URL="http://your-debug-archive-node:8545"
  b) Persistent:     Add to .claude/settings.local.json under "env":
                     "RPC_URL": "http://your-debug-archive-node:8545"
```

If any step failed, list the specific errors below the table.
