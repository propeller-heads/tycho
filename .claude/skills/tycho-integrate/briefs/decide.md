# Write the decision record for {{protocol}}

You are the research agent of the tycho-integrate run for **{{protocol}}**, now writing
`{{run}}/decisions.json`: every design decision the layers need, before any code exists, when
changing course is cheapest. The spec at `{{run}}/SPEC.md` is signed; build only on it.

Run directory: `{{run}}` (write here only). Worktree (read only, `origin/main`): `{{worktree}}`.
Route: `{{route}}`. Kind: {{kind}}.

Notes from the orchestrator (review findings on an earlier draft, a person's answers):

{{extra}}

## Decisions

For each decision: the question, the options, the choice, the spec facts it rests on (by id; an
amendment of one of them reopens the decision), the nearest merged sibling integration that made
the same choice (with its path on main), and the knowledge rules the choice relies on. Name each
rule by its id **and** a one-line gloss of what it says, so a person can follow without knowing
the ids, e.g. `{"id": "contract-vm-component-id-hex", "gloss": "a VM component id is hex of at most
32 bytes the adapter can decode into its poolId"}`. Read the rule in the knowledge base before you
cite it; never paraphrase one you have not read.

Questions every integration answers (more as the venue needs):

- **Implementation type**: not yours to decide. The spec's `kind` (native, hybrid or vm) was set
  by research and signed at Gate 1; cite its `kind_fact` in every decision that depends on it. If
  something you find contradicts the kind, say so in your summary as a spec amendment of that fact,
  not as a decision.
- **Component model**: what one component is, and its id (`component-id-matches-pool-identity`
  in `.claude/knowledge/indexing.md`: an id identifies exactly one pool chain-wide).
- **Discovery and refresh**: which events or storage writes create components and change their
  state; every writer of a refreshed value is covered (`refresh-covers-every-writer`).
- **Dependencies, per chain**: for each of the spec's `dependencies`, how it is indexed on each
  chain. Where the chain has DCI (below): an entrypoint per component with params that reach the
  call (`entrypoint-linked-per-component`, `trace-params-exercise-swap-path`), block storage
  changes emitted (`dci-emits-block-storage-changes`), and a new entrypoint when an address outside
  traced storage changes (`dependency-change-emits-new-entrypoint`). Where it has none
  (`dci-only-where-rpc-serves-storage-range`): the component is not created on that chain (a
  manifest param), or the dependency's values are decoded into attributes and it is priced
  natively. Which of the two, when it drops markets or changes the kind, is a person's decision
  (`no-precedent` or `quantified-tradeoff`, with the markets it drops). All of these rules are in
  `.claude/knowledge/indexing.md` § DCI.

DCI on this run's chains:

{{dci}}
- **Balances**: the source per component kind, from the spec's `balance_source`, and the
  `balance_lens` that reads it (a stock lens, or the venue's own and what it calls).
- **Re-simulation**: which changes mark a component updated (`contract-manual-updates-mark-every-change`
  in `.claude/knowledge/architecture.md`).
- **Limits**: per direction, the kind the yaml declares (`hard`, with any `rounding`, or `soft`)
  from the spec's limit facts, and `get_limits` exact (`simulation-limits-are-settleable`: a limit
  the venue would reject is never accepted; `simulation-quotes-and-limits-agree`: quotes and
  limits read the same state, both in `.claude/knowledge/simulation.md`). An `under_cap` limit,
  where no exact bound can be derived, is a person's decision (class `quantified-tradeoff`) with
  the capacity it gives up.
- **Execution**: the token flow (`TransferType`), where output lands, callbacks, protocol min-out
  and deadline values (`crates/tycho-execution/CLAUDE.md` § Adding a New Executor, and
  `.claude/knowledge/execution.md`).
- **Package**: a new package, or a new module in a live one (`fork-gets-own-package`).

A decision is **open** (a person decides at Gate 2) only when it falls in the closed list:
{{decision_classes}}. Set `"open": true`, its `class`, and the options with their cost; leave
`choice` empty. Everything else you decide.

## Cross-layer agreements

The strings and encodings every layer must use identically, fixed here and nowhere else:

- `protocol_system`: identical and registered everywhere it is dispatched on
  (`contract-protocol-system-registered-everywhere`);
- the component id encoding and, for a VM integration, the adapter `poolId` decoding
  (`contract-vm-component-id-hex`);
- each attribute's name, byte format, writer and readers (`contract-attribute-name-and-format`;
  reuse an attribute the simulation already understands).

## Paths

The names the layer checks run with (`checks.toml` placeholders): `package` (Substreams package
directory, `<chain>-<protocol>`), `sim_module` (nextest filter for a native module), `adapter`
(VM adapter contract), `executor` (executor contract), `encoder` (nextest filter for the swap
encoder), `router_test` (TychoRouter integration test contract). Follow the naming of the nearest
sibling on main. Leave out what this route does not build.

## decisions.json

```json
{
  "decisions": [
    {"id": "D1", "question": "…", "options": ["…", "…"], "choice": "…", "facts": ["S3"],
     "sibling": "protocols/substreams/ethereum-…", "rules": [{"id": "…", "gloss": "…"}],
     "layers": ["indexing", "simulation"], "open": false, "class": null}
  ],
  "agreements": {
    "protocol_system": "…",
    "component_id": "how the id is formed, with an example",
    "adapter_pool_id": "how the adapter decodes it (VM only)",
    "attributes": [{"name": "…", "format": "8-byte big-endian u64", "written_by": "indexing", "read_by": "simulation decoder and delta_transition"}]
  },
  "paths": {"package": "ethereum-…", "sim_module": "…", "adapter": "…", "executor": "…", "encoder": "…", "router_test": "…"}
}
```

End your final message with `{"decisions": <n>, "open": ["D…"]}`.
