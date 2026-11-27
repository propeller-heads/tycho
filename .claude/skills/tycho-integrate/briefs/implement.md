# Implement round {{round}} of the {{layer}} layer of {{protocol}}

You are the implementer of the tycho-integrate run for **{{protocol}}** ({{kind}}). This call is
round {{round}} of the **{{layer}}** layer. Work in the worktree `{{worktree}}` on branch
`{{branch}}`; read the run directory `{{run}}`; write nowhere else.

## Requirements

- `{{run}}/SPEC.md` (and `spec.json`): the signed facts about the venue.
- `{{run}}/decisions.json`: the decision record. Its `agreements` (the `protocol_system` string,
  the component id and `poolId` encoding, attribute names and formats) are fixed: use them exactly,
  in every file of every layer. Its `paths` name your package, modules and contracts.
- Route: `{{route}}`. Build only what the route's layers need; a fork follows its sibling.

## Read first

The knowledge files for this layer; their rules are what the review checks you against:

{{knowledge}}

Also the CLAUDE.md of each crate or package you change (`protocols/substreams/CLAUDE.md`,
`protocols/adapter-integration/CLAUDE.md`, `crates/tycho-simulation/CLAUDE.md`,
`crates/tycho-execution/CLAUDE.md`, `protocols/testing/README.md`), and the nearest sibling
integration on main that the decision record names. Before writing a helper, type or loop, search
for an existing one (`.claude/knowledge/libraries-tycho.md`).

## Open findings

Answer every one. A fix is reviewed as new code next round. A dispute needs evidence the
verifier can check: test output, a chain read with its block, a file and line; an argument is not
evidence. A decision is for a person and only for these classes: {{decision_classes}}. There is no
skip.

{{findings}}

The previous round's failed checks, for detail. Each is also a finding above (rule
`check:<step>[:<check>]`), answered the same way: fix it; dispute it with evidence that the failure
is not the code's (the review verifies it, and a refuted check finding no longer holds the layer);
or make it a decision for a person, who may accept the failure.

{{checks}}

## What the layer needs

**Indexing** (`protocols/substreams/<package>/`): the Substreams package, from the nearest sibling
or the templates; added to the workspace members. Its `integration_test.tycho.yaml` is strict.
`protocols/testing/README.md`, "Strict mode", owns the format, and `protocols/testing/CLAUDE.md`
the rules for writing it; read both. In short:
- `harness: strict` first; the range is the spec's `test_range`; every test lists its expected
  components (strict mode rejects an empty list), with tokens, static attributes and creation tx,
  addresses at full width (`full-width-test-addresses`: 40 hex characters, since a dropped digit
  still parses); no key the runner does not read (strict mode rejects it);
- `initialized_accounts` lists every tracked contract deployed before the range's start block (an
  indexer otherwise fails with `Account NotFound` when it applies their storage updates), but no
  contract the package reaches through an entrypoint (`dci-test-proves-discovery`: a seeded
  contract is indexed whether or not the trace finds it, so the test would prove nothing). The
  harness seeds them with `debug_storageRangeAt`: on a chain without it (below), a test that needs
  them is a person's decision (`outside-repo`: an RPC that serves the method);
- entrypoints only as the decision record's per-chain dependency design says: emitted only from
  manifests of chains with DCI, never reachable from another chain's manifest without a param that
  turns them off (`dci-only-where-rpc-serves-storage-range`), with block storage changes on every
  `BlockChanges` (`dci-emits-block-storage-changes`), and a test swap on a component that loads
  each contract DCI must find;
- `balance_lens`: the lens that reads, on chain, the amounts the spec's `balance_source` names.
  `ComponentIdBalances` when a component holds its own tokens, `BalanceOwnerBalances` when they sit
  at the address in its `balance_owner` attribute, else a lens of the venue's own under
  `protocols/adapter-integration/evm/src/` inheriting `BalanceLens`. A lens reads what the protocol
  accounts, never whatever matches the indexed number; it returns 0 only for a token the protocol
  by design does not account per component;
- `limits` for every direction of every expected component, the kind (`hard`, `soft`) chosen from
  the venue's contract as the spec records it, never from a test result: `soft` only where the
  contract rejects no input by its amount, with the reason naming that code; `hard` with
  `rounding` only for the rounding division the spec names. A component with a cap declares its
  own limits, never under a package-level `soft`;
- when G13 is on, components that do not trade both ways declare their directions; when G5 is on,
  one checkpoint per spec mutation surface, each with the comment `# surface: <id>` (the final
  gates look for it); when G6 is on, `replay_swaps` from the spec's golden vectors.

The guard rejects, and you answer as a person's decision instead: any `skip_*` (`relax-test`), an
`execution_tolerance` or a `tvl_only` (`quantified-tradeoff`, with the run's measured `drift` or
deviation and the artifact that explains it), an `under_cap` limit (`quantified-tradeoff`, with
the capacity it gives up), and changing or dropping a limit kind once written (`relax-test`). Fix
`get_limits`, `get_amount_out` or the lens instead, wherever the chain has the answer.

**Simulation**: for a native integration, the `ProtocolSim` and its decoder registration in
`crates/tycho-simulation` (ported math reproduces the contract: `ported-math-matches-contract`;
the new state applies every change the swap makes: `simulation-new-state-applies-every-change`;
limits exact: `simulation-limits-are-settleable`). A hybrid is the same native `ProtocolSim` over
VM-indexed state: its quote reads the spec's `vm_reads` through the VM (`SimulationDB`), as Fluid
V1, Balancer V3 and Curve do. For a VM integration, the adapter under
`protocols/adapter-integration/evm/src/<protocol>/` with its manifest and fork tests (the
constructor never calls the protocol: the harness deploys the adapter where the protocol's code is
injected only at call time; capabilities match the manifest: `adapter-manifest-matches-capabilities`).

Every kind adds a **venue-quote test**: from the state at each of the spec's `venue_quotes` blocks
(a native or hybrid state built from that block's chain reads, as literals; a VM adapter in a fork
test at that block), quote the same amount and assert the venue's `result` to the wei, in each
direction the spec records. Name the block in the test (the `venue-quote-test` gate looks for it).
Exact simulation-vs-execution in protocol-testing cannot show an under-quote: the router keeps the
surplus and the harness expects what was simulated. A difference you cannot remove is a finding to
explain to the wei, never a tolerance.

**Execution**: the swap encoder, the executor, their registration and config, and the four test
layers (`router-integration-test-layers`: encoder byte layout per direction, executor Foundry
tests, the cross-language decode test, the TychoRouter integration test). Regenerate the runtime
bytecode fixtures (`regenerate-runtime-bytecode`), and register the new executor's fixture so
protocol-testing executes through it before it is deployed: its entry in
`RuntimeBytecodeFixtures.sol`, the generated `protocols/testing/fixtures/<Name>.runtime.json`, and
in `protocols/testing/src/execution.rs` its `const …_BYTECODE_JSON` and its `EXECUTOR_MAPPING`
line (`CHAIN_SPECIFIC_EXECUTORS` when it differs per chain). The guard allows three changes under
`protocols/testing/src/`, and nothing else there: these executor lines; in the simulation layer,
a native or hybrid state's decoder arm in `state_registry.rs` `register_protocol` (with its
import), without which protocol-testing simulates the system as `EVMPoolState`; and for a
config-only fork, its package's `CLONE_TO_BASE_PROTOCOL` entry in `test_runner.rs`. A missing deployment is never a reason to skip
execution (`integration-test-skip-flags`: a skip states its reason and comes off once the
executor is in `EXECUTOR_MAPPING` with its fixture). `executor_addresses.json` stays untouched until
deployment (`deployment-artifacts-after-launch`). A change to `TychoRouterTestSetup`'s deployment
order re-derives the test executor addresses and reruns one existing router suite
(`router-test-setup-addresses`).

**Gas** (execution): report the router test's measured gas for one swap through the new executor
(`forge test --match-contract <paths.router_test> --gas-report`) beside the spec's `gas.swap_gas_used`, and
make the simulation's `gas` estimate match the measurement, not a sibling's constant; put the
three numbers (spec, measured, estimate) in your summary. The review compares them.

**Every layer**: test expectations come from outside the code under test
(`expectations-from-outside-code-under-test`: a literal, real chain data, hand-worked arithmetic),
and every assertion can fail for a plausible bug (`every-assertion-can-fail`). Copied code lands as
a verbatim-copy commit before the adaptation (`copy-commit-before-adaptation`). Remaining work goes
in your answers, never in a committed handover file (`docs-at-the-right-scope`).

## Checks

Run the layer's checks until green before you finish, with the same script and steps
(`checks.toml`) the orchestrator records afterwards, into the temp dir:

```bash
python3 -I {{skill}}/scripts/layer_checks.py --protocol "{{protocol}}" --layer {{layer}} --scratch [--only <step>,...]
```

It builds what the harness needs, starts its own Postgres and indexer, and judges the
protocol-testing report as the round will. The steps it runs for this layer:

{{commands}}

Run single commands yourself as they are written there, from their directory; a command prefixed
with variable assignments (`FOO=1 cmd`), `export` or `set` is refused in this session, so leave the
harness to the script. Forge fork tests run with `-j 1`: rate-limited RPCs fail parallel fork tests
with timeouts that look like logic errors.

Harness gates on this tree:

{{caps}}

DCI on this run's chains:

{{dci}}

## Notes from the orchestrator

A person's decisions on this layer are in the findings above. Anything else:

{{extra}}

## Finish

Commit the round on `{{branch}}`, usually as one commit: a conventional, imperative subject for
what it did (e.g. `feat(ethereum-foo): index pool creation and fee changes`), rule ids in the body
where a rule shaped a choice, and the trailer `Tycho-Integrate-Round: {{layer}}/{{round}}` on every
commit of the round. Code copied from elsewhere is the exception: its verbatim copy is a commit of
its own, before the commit that adapts it (`copy-commit-before-adaptation`); the PR keeps that
history. Do not push.

End your final message with this JSON, covering every open finding above:

```json
{
  "answers": [
    {"id": "F3", "kind": "fix", "note": "what changed, file:line"},
    {"id": "F4", "kind": "dispute", "evidence": "the command and its output, or file:line", "note": "…"},
    {"id": "F5", "kind": "decision", "class": "no-precedent", "question": "…", "options": ["…", "…"]}
  ],
  "checks": "green | red",
  "summary": "what this round built",
  "facts_learned": ["anything about the venue the spec does not say, with evidence"]
}
```
