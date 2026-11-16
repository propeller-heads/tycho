# Workflow Directives for CLAUDE

When responding to user input, always log this information to the user which knowledge docs you have read. List only
the documents you actually read. Respond like this:

> Knowledge docs: [the `.claude/knowledge/` files read, and which sections if not whole]
> Files loaded in context: [list of loaded documents]

All commands in this repo assume mise is activated and its tools are installed (`mise install`), so the
tools and environment variables defined in `mise.toml` are available. In a shell without it, prefix
commands with `mise exec --`.

By default, don't use the Explore agent as a first step in a conversation. The first step should always be to identify
and read the relevant documentation files.

Everytime a developer asks something new, you should check the list of available knowledge and inform
the user about it. If the user asks a question that requires exploring the project, always
check [.claude/CODEBASE.md](.claude/CODEBASE.md) and all the related linked documents first. Only go through files when
you've exhausted the path of documentation.

## Knowledge Documents

**MANDATORY**: Before responding to any input, scan this table for trigger matches. If ANY trigger matches, you MUST
`Read` the document BEFORE doing anything else. Tell the user which documents you loaded.

| Document        | Trigger                                                                 | Path                                   |
|-----------------|-------------------------------------------------------------------------|----------------------------------------|
| Architecture    | Changing code in a protocol layer (Substreams, simulation, off-chain, execution) or a shared type or registry | `.claude/knowledge/architecture.md` |
| Indexing        | Changing a Substreams package under `protocols/substreams/`             | `.claude/knowledge/indexing.md`        |
| Simulation      | Changing protocol states in `tycho-simulation` or VM adapters under `protocols/adapter-integration/` | `.claude/knowledge/simulation.md` |
| Off-chain       | Changing `tycho-simulation`'s `snapshot_feed`, `book`, `rfq`, `pamm` or `price_level_stream` (read Simulation too) | `.claude/knowledge/offchain.md` |
| Execution       | Changing encoders, executors or the router in `tycho-execution`         | `.claude/knowledge/execution.md`       |
| Tycho helpers   | Adding a helper, type or loop in a layer: read that layer's section     | `.claude/knowledge/libraries-tycho.md` |
| Rust            | Building, testing or formatting Rust: the Commands section              | `.claude/knowledge/rust.md`            |
| Solidity        | Building, testing or formatting Solidity: the Commands, Testing and Project Config sections | `.claude/knowledge/solidity.md` |
| Version control | git, branch, commit, PR, push, rebase, merge, cherry-pick, tag, release | `.claude/knowledge/version_control.md` |
| Python          | Python code, tycho-client-py, changes to dto.rs or rpc.rs               | `.claude/knowledge/python.md`          |
| GitBook         | Writing, editing, or reviewing docs under `docs/`                       | `.claude/knowledge/gitbook.md`         |
| Router trades   | Adding a chain to, or changing, the router trades substreams or its pricing | `crates/tycho-execution/substreams/README.md` |

The rest of the knowledge base loads on request or for a review, not for every task: the general
rules (`general.md`), the rules sections of `rust.md` and `solidity.md`, the crate tables
(`libraries-rust.md`), the integration review (`integration.md`) and the evidence methods
(`verification.md`). Lints and formatters check what they can; `/tycho-review` on your own changes
checks them against the rest.

When spawning subagents, pass the relevant knowledge document contents to them.

## Skills

- `tycho-review`: Review a PR, a stack of PRs as one change, a branch, a range, the working changes or a path against
  the knowledge base, with verified findings written to `.claude/reviews/`. Posts nothing. Trigger words: "review",
  "review this PR", "check my changes before a PR".
- `plan`: Guided feature planning with iterative user input. Gathers requirements, validates assumptions, proposes a
  solution. **Always use this skill** when the user wants to plan, design, or architect a feature before coding. Trigger
  words: "plan", "design", "architect", "think through", "figure out how to".
- `run-ci`: Run the full CI pipeline locally (format, clippy, tests). Optionally DB and node RPC dependent checks. Use it
  for PRs and workspace-wide changes, not for routine pushes — those need only a format run and the tests covering the
  change. Trigger words: "run ci", "check ci", "full ci", "will ci pass".
- `sync-docs`: Review all codebase documentation files under `.claude/` and per-crate `CLAUDE.md` files, fix any that
  have drifted from the actual code. Documentation-only — does not modify source code.
- `gas-compare`: Compare per-test gas between the current branch and a base branch (default: main). Produces per-test
  comparison tables grouped by source file. Trigger words: "gas compare", "gas report", "gas diff", "gas regression".
- `update-gas-test-metadata`: Keep `crates/tycho-execution/.gas-compare/test_metadata.json` in sync with TychoRouter
  test files. Use after adding, removing, or renaming tests. Trigger words: "update test metadata", "sync test metadata".
- `migrate-pr`: Migrate an open PR from a related repository (tycho-protocol-sdk, tycho-simulation, tycho-execution)
  into this monorepo. Rewrites paths, strips unmapped diffs, handles conflicts. Trigger words: "migrate PR", "port PR".
