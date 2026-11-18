# Classify {{protocol}} before any work

You are the research agent of the tycho-integrate run for **{{protocol}}** on {{chains}}. Before
research starts, decide which of four cases this venue is, so the run builds only what is missing.

Run directory: `{{run}}`. Worktree (read only, cut from `origin/main`): `{{worktree}}`.

Intake, as the person gave it (leads to verify, not facts):

```json
{{intake}}
```

## Read main, not a checkout

Answer for `origin/main` only. The worktree is cut from it; read files there or with
`git show origin/main:<path>`. A branch or an open PR is not main: list open PRs that touch this
venue (`gh pr list --search <name or address>`), but they do not change the case.

## The four cases

1. **integrated**: every layer the venue needs is on main for these chains: its `protocol_system`
   in a Substreams integration test yaml, in `crates/tycho-indexer/extractors.yaml` where
   applicable, in the simulation decoder registration, and in the swap encoder registry and
   `crates/tycho-execution/config/executor_deployments.json`.
2. **partial**: some of those layers are on main and others are not. Name the missing layers.
3. **fork**: the venue is a fork of a venue integrated on main. Decide it from a fingerprint of the
   pool, not a bytecode diff: the swap and quote selectors, the event topics, the storage layout of
   the state a quote reads, the factory or registry pattern, and the fee and rounding behaviour on
   one real swap (`cast` with the block pinned). A fork whose differences touch only configuration
   (addresses, fee tiers, chain) may need no new code in some layers; a fork with changed math or
   storage needs the layers those touch. The rule `fork-gets-own-package` (a new kind of component
   in a live package gets its own package) and `fork-executor-follows-sibling` (a fork's executor
   follows the sibling's settlement flow) in `.claude/knowledge/indexing.md` and
   `.claude/knowledge/execution.md` say what a fork needs.
4. **new**: none of the above. All three layers are built.

## Output

Write `{{run}}/route.json`:

```json
{
  "kind": "new | fork | partial | integrated",
  "layers": ["indexing", "simulation", "execution"],
  "sibling": "<protocol_system on main, for fork and partial>",
  "why": "<one paragraph: what you compared and what you found>",
  "evidence": [{"kind": "command | tx | source", "ref": "...", "note": "..."}],
  "open_prs": ["#1234 <title>"]
}
```

`layers` lists only the layers this run must build. End your final message with the same JSON.
