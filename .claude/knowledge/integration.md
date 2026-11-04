# Protocol integrations

What a review of a protocol integration does beyond the layer rules: compare the integration with
the deployed contracts, and check that its tests exercise what it claims. `tycho-review` runs this
as the integration pass, once per protocol a change touches, at the `full` level. The layer rules
are in `indexing.md`, `simulation.md` and `execution.md`; the contract checks in `architecture.md`.

## Baseline

A new integration (its `protocol_system` absent at the base) gets the full baseline; a change
inside an existing one gets the scoped baseline: the same steps, limited to the slots, events and
functions the changed code consumes and the branches it affects.

1. **Inventory.** List every contract on each supported path: entrypoints, proxies and
   implementations, inherited math and storage code, wrappers, and the reachable permission, limit,
   fee, pause and upgrade logic. Pin each by address, implementation and block.
2. **Start from what the package consumes:** manifest parameters, addresses, tracked storage,
   decoded events, called functions and getters, ABIs and upgrade signals. Map each to the contract
   that owns the storage or code, and get that implementation's verified source (or decompile it,
   `verification.md`).
3. **Reconcile one by one:** slots and packing, event signatures and indexed fields, function
   selectors, return types, units and inherited overrides. Never infer a contract's model from its
   interface or names.
4. **Mark each dependency** source reviewed, behaviour verified only, not reviewed, or out of
   scope, with block or version and evidence. Interfaces and a successful fork transaction are not
   a source review. Report reachable code that was not reviewed (a kernel or proxy, permission
   logic) instead of expanding into an audit of the whole protocol. A layer the change does not
   touch yet is a dependency in the inventory, not a defect.

## Comparison with the deployed contracts

- **Custom simulation:** compare the Rust with the Solidity control flow branch by branch, for
  every supported direction: arithmetic order, rounding, checks, state mutations and the quantities
  returned or received.
- **VM simulation:** compare every interface the adapter uses with the pinned deployed source
  (signatures, mutability, tuple layout, revert assumptions), and confirm the adapter reaches the
  intended implementation path.
- Expand each finding to its siblings: one direction checked does not make all directions correct.

## Test scenarios

Check the integration's tests against the scenarios its failure model calls for; each entry needs
a test or yaml entry that exercises it and an assertion that would fail if it broke. Choose the
combinations that apply, not a Cartesian product:

- zero input, dust, normal size, the advertised limit and just beyond a hard cap;
- non-unit exchange rates, non-zero fees, partially consumed buckets and external shares;
- each independent constraint binding on its own, with positive non-zero capacity (tests where
  every constraint collapses to zero capacity prove no boundary trade);
- a snapshot at creation and at a busy later block; idle blocks that accrue; several trades on the
  same pool in sequence;
- writes whose call-tree order differs from ordinal order, several transactions, unrelated storage
  owners, reverted calls and repeated implementation changes;
- missing or oversized inputs, malformed configuration, unknown extra attributes.

A green run proves only what it ran: read the integration test yaml's component assertions,
`skip_*` flags, initialized accounts and block ranges, and the run's executed and skipped counts,
before counting it as coverage. A fork with patched state proves the guard it patched, not untouched
mainnet execution.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds).

<!-- pieces: rendered from candidates.json by candidates/knowledge.py; edit the pieces there, not below -->

### integration-test-skip-flags
Every `skip_*` flag in an integration test yaml (`skip_simulation`, `skip_execution`, `skip_balance_check`) states its reason and is removed once the blocker is gone. Execution is tested during development: `skip_execution` comes off once the executor is in `EXECUTOR_MAPPING` with its runtime-bytecode fixture, and a skip that remains needs hard evidence that execution cannot be tested (a block range before an upgrade the router needs); an executor not deployed yet is not such evidence.
Why: Each skip disables exactly the check that would catch a class of bug — `skip_balance_check` hides balance drift, `skip_execution` hides encoding errors — so unexplained skips leave whole layers untested.
Spot: New or copied `skip_*: true` entries without a comment; `skip_execution: true` for a protocol whose executor exists; `skip_balance_check: true` on a package with a delta-based balance path. `skip_execution: true` justified by a missing deployment ('no executor deployed yet').
Applies: integration-test, substreams · look first at lines matching `skip|EXECUTOR_MAPPING|executor`
Checked by: repo-script `protocol-system-registry`

### test-range-covers-change
Integration test ranges exercise the conditions the change is about, including the non-default branch of a component flag.
Why: A test that only covers the default branch or a pre-upgrade layout passes while the code is wrong at tip.
Spot: A new boolean/static attribute (e.g. `reserves_inverted`) with every expected component using one value; test `start_block`/snapshot far behind the latest upgrade of an upgradeable contract.
Applies: integration-test, substreams
Checked by: repo-script `flag-attribute-one-value`

### full-width-test-addresses
Addresses and ids in integration-test expectations are written at full width (40 hex characters for an address), because Tycho's Bytes deserializer left-pads odd-length hex and silently accepts a literal that dropped a digit.
Why: A mistyped literal still parses into some value, so the test either checks the wrong address or passes only because padding happened to repair it, and the yaml misleads readers.
Spot: An address in an integration_test.tycho.yaml or fixture whose hex length after 0x is not 40 (or an id whose length is odd).
Applies: integration-test, substreams · look first at lines matching `0x`

### test-env-satisfies-sim-assumptions
When simulation assumes a caller-dependent venue policy (e.g. the router is whitelisted), the integration-test range or fork environment satisfies that assumption at its blocks, verified with a block-pinned quote, and the execution comparison stays enabled.
Why: Otherwise the execution comparison checks a model the simulator no longer uses and either fails or is disabled.
Spot: An integration_test yaml whose comment says the range predates a whitelist/permission the simulator now assumes.
Applies: integration-test, substreams, rust

### integration-lands-as-one-change
An integration is one change. Written by the implementation skill, it lands as one PR carrying every layer the integration needs (Substreams, adapter, execution) with its tests and wiring. An external contribution may arrive as several PRs; they are reviewed together as one change, and any merge-order coupling between them is stated in each PR's description.
Why: The layers share contracts (the component id and the adapter's poolId, the protocol_system name, attribute names and formats) that only show when they are read together: a layer reviewed alone passes while the integration is broken across layers.
Spot: A PR from the implementation skill that leaves out a layer the integration needs (an executor without its encoder, a VM package without its adapter, a package without its integration test); a PR of an external integration reviewed while its other layers' open PRs are left out; several PRs of one integration with no stated merge order.
Applies: substreams, solidity, rust, pr
