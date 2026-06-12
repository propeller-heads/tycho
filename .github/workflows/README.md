# CI/CD Workflows

This monorepo contains three language ecosystems — **Rust crates**, **Solidity/Foundry contracts**, and **Substreams** — each with their own CI workflows, plus a shared CD pipeline for Docker image delivery.

Reusable build/deploy steps are delegated to [propeller-heads/ci-cd-templates](https://github.com/propeller-heads/ci-cd-templates).

---

## Workflow Overview

| Workflow | Trigger | Purpose |
|---|---|---|
| `main-workflow.yaml` | push to `main` | Orchestrates full CI → release → Docker build → dev deploy |
| `ci-rust.yaml` | PR (Rust paths), push to `main`, `workflow_call` | Rust lint, test, and doc checks |
| `ci-foundry.yaml` | PR/push (`foundry/`), manual | Solidity fmt, tests, gas snapshot, static analysis |
| `ci-substreams.yaml` | PR (`substreams/`), manual | Lint and unit tests for changed substreams packages |
| `ci-substreams-integration.yaml` | PR (`protocols/substreams/`, `protocols/testing/`), manual | Full protocol integration tests against a live DB |
| `cd-deploy-dev.yaml` | Manual | Build a branch image and deploy it to dev |
| `promote-to-prod.yaml` | Manual | Promote a tagged dev image to production |
| `release.yaml` | GitHub Release published, manual | Upload binaries to GitHub + S3, publish crates to crates.io |
| `release-substreams.yaml` | Tag `substreams/v*`, manual | Publish substreams crates, build and release substreams packages |

---

## Workflows in Detail

### `main-workflow.yaml` — Main pipeline

The entry point for all automated delivery. Runs only on pushes to `main`.

**Jobs and order:**

1. **`ci`** — Calls `ci-rust.yaml` via `workflow_call`.
2. **`check-release`** — Runs semantic-release in dry-run mode to determine whether a new release is warranted.
3. **`release`** — Runs only if `check-release` produced a version. Creates the GitHub Release via semantic-release.
4. **`build-and-push-tycho-indexer`** / **`build-and-push-tycho-integration-test`** — Build and push Docker images tagged with the release version to ECR.
5. **`promote-tycho-indexer-to-dev`** / **`promote-tycho-integration-test-to-dev`** — Roll out the new images to the dev environment.

Steps 4 and 5 are gated on `release` completing, so no image is built on dry-run commits.

---

### `ci-rust.yaml` — Rust CI

Covers all Rust crates under `crates/` and `protocols/testing/`. Triggered on PRs touching Rust paths and on every push to `main`. Also callable as a reusable workflow (used by `main-workflow.yaml`).

**Jobs (all run in parallel):**

- **`lint`** — `cargo fmt` and `cargo clippy` on nightly toolchain.
- **`test-unit`** — Unit tests excluding anything requiring a database, RPC, or anvil.
- **`test-db`** — Tests requiring a live Postgres instance. Spins up a custom Postgres Docker image (built from `docker/postgres.Dockerfile`) with pg_cron. Runs parallel and serial DB test suites separately.
- **`doc`** — `cargo doc` with broken intra-doc link detection.
- **`check-no-default-features`** — Ensures the workspace compiles without default features enabled.

All jobs use `cargo nextest` and `--locked` to enforce lockfile consistency.

---

### `ci-rust-evm.yaml` — Rust EVM CI

Runs the Rust tests that need live RPC access. Uses `pull_request_target` so approved fork PR workflow runs can access `ETH_RPC_URL`.

**Jobs:**

- **`test-evm`** — EVM and fork tests requiring a live Ethereum RPC.

---

### `ci-foundry.yaml` — Foundry CI

Covers Solidity contracts under `crates/tycho-execution/contracts/` and `protocols/adapter-integration/evm/`. Triggered on PRs and pushes touching those paths.

**Jobs:**

- **`forge-test`** — Formatting check, test suite (with RPC), and gas snapshot posted to the workflow summary.
- **`slither`** — Static analysis via Slither, ignoring `lib/` dependencies.

---

### `ci-substreams.yaml` — Substreams CI

Covers Rust-based Substreams packages under `protocols/substreams/`. Only processes packages whose files actually changed in the PR.

**Jobs:**

- **`lint`** — Detects changed packages via a custom action, then runs `cargo fmt` and `cargo clippy` (nightly) for each.
- **`test`** — Builds each changed package targeting `wasm32-unknown-unknown` and runs its unit tests.

---

### `ci-substreams-integration.yaml` — Substreams Integration Tests

Full end-to-end protocol tests. Triggered on PRs touching `protocols/substreams/` or `protocols/testing/`, and can be run manually against specific protocols.

**Jobs:**

1. **`detect-changes`** — Computes the list of protocols to test. If `protocols/testing/run.Dockerfile` or `Cargo.toml` changed, all protocols are tested; otherwise only the ones with changed substreams files. Manual runs accept an explicit space-separated list.
2. **`build-images`** — Builds the Postgres and test-runner Docker images and uploads them as artifacts (retained 1 day).
3. **`test-protocols`** — Fan-out matrix job (max 4 parallel). Each protocol gets its own isolated database instance via `docker compose`.
4. **`skip-tests`** — No-op job that runs when no protocols need testing, to satisfy required status checks.

**Custom actions used:**
- `.github/actions/substreams-check` — Maps changed file paths to substreams package names.
- `.github/actions/substreams-docker-single` — Runs a single protocol test with an isolated Postgres container.

---

### `cd-deploy-dev.yaml` — Dev deploy (manual)

Manually triggered workflow for deploying feature branches to dev without going through the release process.

Inputs: `application` (choice: `tycho-indexer` or `tycho-integration-test`), and for `tycho-indexer` an optional `app_name` to target a specific ECS service instead of all indexer services.

**Flow:** Build Docker image tagged with `github.sha` → deploy to the selected ECS/EKS service in dev.

---

### `promote-to-prod.yaml` — Promote to production (manual)

Promotes an already-built and dev-verified image to production. Requires `application` and `image_tag` (e.g. `v0.255.1`) as inputs. Does not rebuild anything — purely a promotion step.

---

### `release.yaml` — Release artifacts

Triggered automatically when a GitHub Release is published (by `main-workflow.yaml` via semantic-release). Also supports manual runs.

**Jobs:**

- **`upload-assets`** — Matrix job building `tycho-client` (macOS arm64, macOS x86, Linux x86) and `tycho-indexer` (Linux x86). Uploads `.tar.gz` binaries as GitHub Release assets and to S3.
- **`publish`** — Publishes all workspace crates to crates.io in dependency-wave order, with delays between waves to allow crates.io indexing.

---

### `release-substreams.yaml` — Substreams release

Triggered on `protocols/substreams/v*` tags or manually.

**Jobs:**

- **`publish-crates`** — Publishes `substreams-helper` then `tycho-substreams` to crates.io.
- **`release-substreams-package`** — Manual-only. Builds a named substreams package for `wasm32-unknown-unknown` and runs `release.sh` to publish it.
