---
allowed-tools: Read, Edit, Write, Bash(git log *), Bash(git diff *), Bash(git rev-parse *), Bash(git branch *), Bash(git status *), Bash(ls *), Glob, Grep, LSP, Agent
description: "Review all codebase documentation files under `.claude/` and per-crate `CLAUDE.md` files, and fix any that have drifted from the actual code. This is a documentation-only task -- do not modify any source code."
user-invocable: true
---

Review all codebase documentation files under `.claude/` and per-crate `CLAUDE.md` files, and fix any that have drifted
from the actual code. This is a documentation-only task -- do not modify any source code.

## Purpose of these docs

These docs exist to help AI agents quickly understand the codebase so they can solve tasks. Every piece of information
must serve that goal. Do NOT:

- Add exhaustive field-by-field listings unless a field is surprising or critical to understand
- Document obvious things (e.g. "mod.rs contains module declarations")
- Be pedantic about minor wording differences that don't affect understanding
- Add information that an agent could trivially find by reading the source file

DO focus on:

- **What each module does** and **how it relates to other modules**
- **Key types, traits, and their roles** -- names and purpose, not exhaustive field lists
- **Data flow** -- how information moves through the system
- **Non-obvious behavior** -- reorg handling, partial blocks, temporal versioning, DCI, etc.
- **Correct interface signatures** for core traits (`ProtocolGateway`, `ContractStateGateway`,
  `AccountExtractor`, `TokenPreProcessor`, `TokenAnalyzer`, `EntryPointTracer`, `SwapQuoter`,
  `ProtocolSim`) -- agents need these to implement or call them

## What counts as a real discrepancy

Only fix things that would **mislead an agent** trying to work with the code:

- A module/file listed in docs that no longer exists, or a new module not listed
- A trait signature in docs that doesn't match the actual trait (wrong method names, wrong parameters, wrong return
  types)
- An endpoint path, method, or handler name that's wrong
- A struct or enum documented with fields/variants that don't exist (or missing ones that are important)
- A data flow step that's no longer accurate
- A feature flag that was added or removed

Do NOT "fix" things like:

- Minor differences in how a concept is described vs how it reads in code
- Adding more detail to something that's already clear enough

## Documentation layout

This repo's documentation lives in two places:

### 1. `.claude/` directory

| File | Purpose |
|------|---------|
| `.claude/CODEBASE.md` | Workspace-level overview: module map, data flow, architecture patterns, config, testing |
| `.claude/knowledge/rust.md` | Rust coding conventions for this project |
| `.claude/knowledge/version_control.md` | Git/PR workflow for this project |
| `.claude/knowledge/python.md` | Python conventions (tycho-client-py, dto/rpc changes) |

### 2. Per-crate `CLAUDE.md` files

| File | Crate |
|------|-------|
| `crates/tycho-indexer/CLAUDE.md` | Main indexer: extractors, services, RPC endpoints |
| `crates/tycho-common/CLAUDE.md` | Shared domain types, traits, simulation abstractions |
| `crates/tycho-storage/CLAUDE.md` | Postgres backend, temporal versioning, gateway structs |
| `crates/tycho-ethereum/CLAUDE.md` | Ethereum RPC, token analysis, entrypoint tracing |
| `crates/tycho-client/CLAUDE.md` | Consumer library: snapshot+delta sync, feed alignment |
| `crates/tycho-execution/CLAUDE.md` | TychoRouter contracts + Rust encoding library |
| `crates/tycho-simulation/CLAUDE.md` | DEX simulation library: native/VM/RFQ approaches, protocol implementations |
| `crates/tycho-integration-test/CLAUDE.md` | Live integration validator: CLI binary, env vars, validation loop |

| `protocols/CLAUDE.md` | Index of sub-directories |
| `protocols/substreams/CLAUDE.md` | WASM Substreams modules: layout, templates, release process |
| `protocols/testing/CLAUDE.md` | Integration test runner: CLI, env vars |
| `protocols/adapter-integration/CLAUDE.md` | Foundry VM adapter tests |

## Process

This skill runs 3 agents in parallel, then applies changes based on their combined findings.

### Agent 1: Code Reader (subagent_type: Explore)

Reads every doc file and compares it against the actual source code. This agent does NOT make edits --
it only reports discrepancies.

**Prompt for Agent 1:**

> You are auditing documentation against the actual Rust source code in a Cargo workspace. Do NOT edit any files. Only
> report discrepancies.
>
> **Prefer LSP over Glob/Grep for code exploration.** Use `documentSymbol` to list module contents, `goToDefinition`
> and `findReferences` for trait/type verification, `workspaceSymbol` for locating types across crates. Fall back to
> Glob/Grep/Read for docs, config files, and non-Rust files.
>
> For each doc file below, read it, then read the corresponding source files and report any concrete discrepancy that
> would mislead an agent. Focus on: wrong/missing modules, wrong trait signatures, wrong struct fields/variants, wrong
> endpoint paths, wrong data flow, wrong feature flags.
>
> **Workspace entrypoint** (`.claude/CODEBASE.md`):
> - Compare "Workspace Module Map" against actual crates in `Cargo.toml` `[workspace.members]`
> - Compare feature flags table against each crate's `Cargo.toml` `[features]`
> - Compare "End-to-End Data Flow" diagram against `crates/tycho-indexer/src/extractor/protocol_extractor.rs`,
>   `crates/tycho-indexer/src/services/`, and `crates/tycho-client/src/feed/`
> - Compare CLI commands table against `crates/tycho-indexer/src/cli/`
> - Compare env vars table against actual usage (search for `env::var` / `std::env`)
> - Compare testing section against CI config (`.github/workflows/`)
>
> **tycho-indexer** (`crates/tycho-indexer/CLAUDE.md`):
> - Compare module map against `crates/tycho-indexer/src/` directory tree
> - Compare ProtocolExtractor description against `crates/tycho-indexer/src/extractor/protocol_extractor.rs`
> - Compare RPC endpoints table against `crates/tycho-indexer/src/services/rpc.rs`
> - Compare services/middleware listing against `crates/tycho-indexer/src/services/middleware/`
> - Compare DCI description against `crates/tycho-indexer/src/extractor/dynamic_contract_indexer/`
>
> **tycho-common** (`crates/tycho-common/CLAUDE.md`):
> - Compare module organisation against `crates/tycho-common/src/` directory tree
> - Compare trait abstractions against `crates/tycho-common/src/storage.rs` and `crates/tycho-common/src/traits.rs`
> - Compare simulation module against `crates/tycho-common/src/simulation/`
> - Compare data flow diagram against actual inter-crate dependencies
>
> **tycho-storage** (`crates/tycho-storage/CLAUDE.md`):
> - Compare module map against `crates/tycho-storage/src/postgres/` directory tree
> - Compare write order against `DBCacheWriteExecutor` in `crates/tycho-storage/src/postgres/cache.rs`
> - Compare gateway descriptions against `crates/tycho-storage/src/postgres/cache.rs` and `direct.rs`
>
> **tycho-ethereum** (`crates/tycho-ethereum/CLAUDE.md`):
> - Compare module map against `crates/tycho-ethereum/src/` directory tree
> - Compare trait implementations table against actual `impl` blocks
> - Compare entrypoint_tracer contents against `crates/tycho-ethereum/src/services/entrypoint_tracer/`
>
> **tycho-client** (`crates/tycho-client/CLAUDE.md`):
> - Compare module map against `crates/tycho-client/src/` directory tree
> - Compare connections diagram against actual struct relationships
> - Compare sync lifecycle against `crates/tycho-client/src/feed/synchronizer.rs`
>
> **tycho-execution** (`crates/tycho-execution/CLAUDE.md`):
> - Compare Solidity architecture against `crates/tycho-execution/contracts/`
> - Compare Rust encoding module map against `crates/tycho-execution/src/` directory tree
> - Compare swap flow description against actual contract entry points
>
> **protocols** (`protocols/CLAUDE.md`, `protocols/substreams/CLAUDE.md`,
> `protocols/testing/CLAUDE.md`, `protocols/adapter-integration/CLAUDE.md`):
> - Compare substreams protocol list against `protocols/substreams/` subdirectories
> - Compare adapter-integration protocol list against `protocols/adapter-integration/evm/src/` and `test/`
> - Verify release tagging instructions still match `protocols/substreams/Readme.md`
>
> **tycho-integration-test** (`crates/tycho-integration-test/CLAUDE.md`):
> - Compare module map against `crates/tycho-integration-test/src/` directory tree
> - Compare env vars / CLI args against `Cli` struct in `crates/tycho-integration-test/src/main.rs`
> - Compare validation steps against actual logic in `stream_processor/`
>
> **tycho-simulation** (`crates/tycho-simulation/CLAUDE.md`):
> - Compare module map against `crates/tycho-simulation/src/` directory tree
> - Compare native protocol list against `crates/tycho-simulation/src/evm/protocol/` subdirectories
> - Compare VM adapter description against `crates/tycho-simulation/src/evm/protocol/vm/`
> - Compare features table against `crates/tycho-simulation/Cargo.toml` `[features]`
>
> **Skill file paths**: Verify every source path referenced in `.claude/skills/sync-docs/SKILL.md` and
> `.claude/skills/run-ci/SKILL.md` still exists.
>
> Output format: For each discrepancy, report:
> 1. Which doc file is wrong
> 2. What the doc says vs what the code says (with file:line references)
> 3. Severity: HIGH (would cause agent to write wrong code) or MEDIUM (misleading but recoverable)

### Agent 2: Commit Differ (subagent_type: general-purpose)

Reads the `docs-synced-at` commit hash from the first line of `.claude/CODEBASE.md`, then examines all commits between
that hash and HEAD to find changes that demand documentation updates.

**Prompt for Agent 2:**

> You are reviewing git history to find commits that may have introduced documentation drift.
>
> 1. Read the first line of `.claude/CODEBASE.md` to extract the commit hash from the `docs-synced-at` HTML comment
>    (format: `<!-- docs-synced-at: <hash> -->`).
> 2. Run `git log --oneline <hash>..HEAD -- crates/tycho-indexer/src/ crates/tycho-common/src/ crates/tycho-storage/src/ crates/tycho-ethereum/src/ crates/tycho-client/src/ crates/tycho-simulation/src/ crates/tycho-execution/src/ protocols/testing/`
>    to list all source-code commits since the last doc sync.
> 3. For each commit (or group of related commits), run `git diff <hash> HEAD` on the relevant paths to understand what
>    changed. Focus on:
>    - New or removed files/modules under any `crates/tycho-*/src/` or `protocols/` directory
>    - Changed trait definitions (added/removed/renamed methods) in `crates/tycho-common/src/storage.rs` or `crates/tycho-common/src/traits.rs`
>    - Changed `ProtocolSim` trait in `crates/tycho-simulation/src/` or executor interfaces in `crates/tycho-execution/src/`
>    - Changed struct/enum definitions (added/removed/renamed fields/variants)
>    - Changed RPC endpoints in `crates/tycho-indexer/src/services/rpc.rs`
>    - Changed feature flags in any crate's `Cargo.toml`
>    - Changed CLI commands in `crates/tycho-indexer/src/cli/`
>    - Changed data flow or extraction pipeline logic
>    - New DEX integrations added to `crates/tycho-simulation/src/evm/protocol/` or `crates/tycho-execution/src/encoding/`
> 4. For each change that affects something documented in `.claude/CODEBASE.md` or any `CLAUDE.md`, report:
>    - The commit(s) that introduced the change
>    - Which doc file is affected
>    - What specifically changed and what the doc should say now
>
> Ignore: test-only changes, comment changes, internal refactors that don't change public interfaces, dependency version
> bumps (unless a new dependency adds a new module).
>
> Output format: A list of "doc update needed" items, each with:
> 1. Commit hash(es) and summary
> 2. Which doc file needs updating
> 3. What the doc currently says (if relevant)
> 4. What it should say based on the code change

### Agent 3: Reviewer (subagent_type: general-purpose)

Waits for Agents 1 and 2 to complete, then synthesizes their findings and applies the actual documentation edits.

**Prompt for Agent 3** (pass the output of Agents 1 and 2 as context):

> You are the final reviewer for a documentation sync. Two agents have produced findings:
>
> **Agent 1 (Code Reader)** compared docs against current source code and found discrepancies.
> **Agent 2 (Commit Differ)** reviewed git history since the last sync and found commits that demand doc updates.
>
> Your job:
>
> 1. **Deduplicate**: If both agents flag the same issue, merge them into one.
> 2. **Filter**: Only keep items that would genuinely mislead an agent. Drop anything that's:
>    - Minor wording differences
>    - Implementation details that don't affect how an agent would use the code
>    - Adding detail to something already clear enough
>    - Field visibility (pub vs private) unless it affects how an agent would call the API
>    - Missing async keywords on methods where async is obvious from context
> 3. **Apply edits**: For each remaining item, use the Edit tool to fix the documentation file. Keep edits minimal --
>    change only what's wrong, don't restructure surrounding text.
> 4. **Update the sync marker**: After all edits, update the `docs-synced-at` hash in `.claude/CODEBASE.md` line 1 to
>    the current HEAD commit hash (run `git rev-parse HEAD` to get it).
> 5. **Verify skill file paths**: Check that every source path referenced in `.claude/skills/sync-docs/SKILL.md` and
>    `.claude/skills/run-ci/SKILL.md` still exists. If any path is stale, fix it.
> 6. **Report** a summary of:
>    - Files that were already accurate
>    - Files that were updated and what changed
>    - Any new modules not yet documented (create docs if needed)
>    - Any stale paths in the skill files that were corrected

## Execution

Use `TaskCreate` to create a task list so the user can track progress. Create these tasks up front:

| Task | Subject                          | Active Form                         |
|------|----------------------------------|-------------------------------------|
| 1    | Audit docs against source code   | Auditing docs against source code   |
| 2    | Review git history for doc drift | Reviewing git history for doc drift |
| 3    | Apply documentation fixes        | Applying documentation fixes        |

Then execute:

1. Mark tasks 1 and 2 as `in_progress`. Launch Agent 1 (Code Reader) and Agent 2 (Commit Differ) **in parallel** using
   the Agent tool with `run_in_background: true`.
2. When each background agent completes, mark its task as `completed`.
3. Once both are done, mark task 3 as `in_progress`. Launch Agent 3 (Reviewer) with the combined output of Agents 1 and
   2.
4. Agent 3 applies all edits and produces the final report. Mark task 3 as `completed`.
5. Present Agent 3's report to the user.
