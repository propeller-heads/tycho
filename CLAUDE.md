# Workflow Directives for CLAUDE

When responding to user input, always log this information to the user which knowledge docs you have read. Only say yes
if you actually read them. Respond like this:

> Knowledge docs: Rust: [yes/no], Python: [yes/no], Solidity: [yes/no], Version-control: [yes/no], GitBook: [yes/no]
> Files loaded in context: [list of loaded documents]

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
| Rust            | Writing, reviewing, or debugging Rust code                              | `.claude/knowledge/rust.md`            |
| Version control | git, branch, commit, PR, push, rebase, merge, cherry-pick, tag, release | `.claude/knowledge/version_control.md` |
| Python          | Python code, tycho-client-py, changes to dto.rs or rpc.rs               | `.claude/knowledge/python.md`          |
| Solidity        | Solidity contracts, Foundry, forge, executors, tycho-execution          | `.claude/knowledge/solidity.md`        |
| GitBook         | Writing, editing, or reviewing docs under `docs/`                       | `.claude/knowledge/gitbook.md`         |

When spawning subagents, pass the relevant knowledge document contents to them.

## Skills

- `plan`: Guided feature planning with iterative user input. Gathers requirements, validates assumptions, proposes a
  solution. **Always use this skill** when the user wants to plan, design, or architect a feature before coding. Trigger
  words: "plan", "design", "architect", "think through", "figure out how to".
- `run-ci`: Run the full CI pipeline locally (format, clippy, tests). Optionally DB and node RPC dependent checks.
  Trigger words: "run ci", "check ci", "run tests", "lint", "will ci pass".
- `sync-docs`: Review all codebase documentation files under `.claude/` and per-crate `CLAUDE.md` files, fix any that
  have drifted from the actual code. Documentation-only — does not modify source code.
- `gas-compare`: Compare per-test gas between the current branch and a base branch (default: main). Produces per-test
  comparison tables grouped by source file. Trigger words: "gas compare", "gas report", "gas diff", "gas regression".
- `update-gas-test-metadata`: Keep `crates/tycho-execution/.gas-compare/test_metadata.json` in sync with TychoRouter
  test files. Use after adding, removing, or renaming tests. Trigger words: "update test metadata", "sync test metadata".
- `migrate-pr`: Migrate an open PR from a related repository (tycho-protocol-sdk, tycho-simulation, tycho-execution)
  into this monorepo. Rewrites paths, strips unmapped diffs, handles conflicts. Trigger words: "migrate PR", "port PR".
