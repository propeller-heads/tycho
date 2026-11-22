# Research {{protocol}} and write the spec

You are the research agent of the tycho-integrate run for **{{protocol}}** on {{chains}}. Write
`{{run}}/spec.json`: every fact the three layers depend on, each with on-chain evidence. A person
signs the spec at Gate 1, and nothing downstream can catch a wrong model of the venue, so the spec
has to let a reviewer check the protocol without reading its contracts.

Run directory: `{{run}}` (write here only). Worktree (read only, `origin/main`): `{{worktree}}`.
Route: `{{route}}`. For a fork or partial route, research what differs from the sibling and what
the missing layers need; facts the sibling's integration already establishes can cite it.

Intake (leads to verify, never evidence):

```json
{{intake}}
```

Notes from the orchestrator (corrections, refuted facts to redo):

{{extra}}

## How to establish facts

Follow `.claude/knowledge/integration.md` § Baseline (inventory every contract on each supported
path, pin it by address, implementation and block, reconcile slots, events, selectors and units one
by one, mark each dependency) and the methods in `.claude/knowledge/verification.md` § Tycho and
chain methods: among them `state-reads-at-exact-blocks` (block-pinned `cast` reads at every block
that matters), `on-chain-history-reconstruction` (implementation and pointer slot writes, creation
txs, first-set blocks), `decompile-unverified-bytecode`, `resolve-unknown-selectors` (hash
candidate signatures, confirm with an `eth_call`, not 4byte alone), `amount-bisection-ground-truth`
(the largest input that settles, plus one raw unit more) and `state-override-experiments`.

Start from real transactions: pull live swaps and trace them (`debug_traceTransaction`, or a
fork). A full call and storage trace per swap answers half the questions below.

Pitfalls that burned earlier integrations:
- Two quantities matching at one block is a coincidence until another block or a fork shows it is
  an on-chain check.
- A tool that returns empty (some providers' `trace_filter`) is not evidence that something never
  happened; confirm with a method you have seen return a positive result.
- Config changes often emit no event; storage diffs are the ground truth.
- Behaviour in one direction says nothing about the other; check both.
- Check every proxy's implementation slot before reading a storage layout.

## The questionnaire

Each answer is one or more facts (below). Mark the facts every layer depends on `critical`.

1. **Contract topology**: every contract a swap touches and its role (entrypoint, settlement,
   math, config or registry, oracle, custody); proxies and implementations; verified source.
2. **Interfaces**: selectors and signatures of swap and quote entrypoints, config getters, admin
   setters, pause controls; return layouts decoded from raw words.
3. **Pricing determinism and the simulation kind**: is the executable price a pure function of
   on-chain state, and of which contracts' storage? External calls, oracle reads, signature checks,
   `block.timestamp` or `block.number` on the pricing path? (Not a pure function of chain state at
   all rules the venue out: RFQ is not this pipeline's.) Then choose the kind with
   `crates/tycho-simulation/CLAUDE.md` "Choosing an approach" and `simulation-prefer-native` in
   `.claude/knowledge/simulation.md`, weighing:
   - **native**: the swap math and all the state it reads can be ported and indexed as attributes;
     the fastest, and what solvers use first;
   - **hybrid**: the math ports, but the state it reads is too complex to track independently, so
     indexing stays VM-shaped and the quote reads that state through the VM (Fluid V1, Balancer
     V3, Curve); name that state in `vm_reads`;
   - **vm**: neither ports (many contracts, upgradeable math, assembly). A VM quote runs the
     venue's bytecode in revm for every call, orders of magnitude slower than native math, and some
     solvers skip VM venues: say what that costs this venue;
   - **not worth integrating**: the venue's reachable volume or TVL does not justify the kind it
     needs. Say so with the numbers; the person decides at Gate 1.
   Write the choice as one fact (section `summary`, `critical`) with these reasons and the
   alternatives' costs, set `kind`, and point `kind_fact` at that fact.
4. **How a swap works**: the call path, who holds the tokens, the pricing formula, fees and where
   they apply, rounding direction, exact-in and exact-out support.
5. **Limits**: per direction, what happens above capacity and the exact bound with its
   derivation. Record the kind the strict harness tests (`protocols/testing/README.md`, "Limits"):
   `hard` (the contract rejects any input above a bound: name the check), `soft` (it rejects no
   input by its amount: name the code that shows it), and for a hard bound derived from the output
   side, the rounding division and how many units it can exceed the limit by. Where the exact bound
   cannot be derived, say why with a chain read; that is an `under_cap` for a person. What the
   quote path checks versus what settlement checks; they can differ.
6. **Freshness gates**: anything that expires (quote timestamps, deadlines, max-age getters); read
   the constants. A zero window means exact-match gating.
7. **What a quote depends on**, from the chain, not only the code: run
   `python3 -I {{skill}}/scripts/onchain.py --chain <chain> read-set --to <quote contract> --data
   <calldata> --block <block>` on a real quote, and list every account and slot it read in
   `read_set`, each with what tracks it (`tracked_by`: an attribute, VM storage) or why nothing
   needs to (`why_untracked`: an immutable, a constant). Past integrations missed a signer, a
   router list and an implementation that sat on the quote path untracked. Then each piece of
   state: where it lives (slot or getter) and what changes it. Put each contract on the quote path
   in `watch` (its code), and each config getter the quote depends on (`data`: the getter's
   calldata), at the spec's block: the final gates and `--recheck` re-read them.
7a. **Who is asking**: does the price or the call's success depend on the caller or the
   transaction (`msg.sender` or `tx.origin` screening, a whitelist or router list, gasprice or
   priority fee, `block.coinbase`)? In production the Tycho router is the caller
   (`crates/tycho-execution/config/router_addresses.json`): record each venue quote anonymously,
   and `onchain.py quotes` replays it from the router. A difference is a critical fact
   (`caller_fact` on the quote), and usually a conversation with the protocol team (a person's
   decision, class `outside-repo`).
8. **Mutation surfaces**: every way that state changes (swaps, liquidity, fee or config writes,
   pause, upgrade, admin keys): who can trigger it, its signal (event or storage write only), and
   the blocks where it fired.
9. **Components**: what one component is, how components are discovered (factory event, config
   write, registry), the id format, tokens, attributes, and the directions each trades. Pairs that
   trade only through a hub token. **Dependencies**: every contract a quote calls that the package
   never sees created (deployed before the start block, or re-pointable: oracles, rate providers,
   base pools, resolvers), whether its address sits in storage the call reads or comes in calldata
   or an event, and what its values are (slots, events) if they had to be decoded into attributes.
   These need DCI on chains that have it, and another design on chains that do not (below).
10. **Balances**: where each component's tokens sit (`balance-counts-only-payable-inventory`: the
    balance is what the component alone can pay out; `vm-balance-is-real-balanceof`: a VM
    component's balance is the holder's real `balanceOf`), the source per component kind, and
    whether simulation reads balances. The strict harness checks each component against a balance
    lens that reads those amounts on chain: say per component kind whether `ComponentIdBalances`
    (it holds its own tokens) or `BalanceOwnerBalances` (they sit at one address holding only its
    own) reads them, or what a lens of the venue's own must call (a vault's per-pool view, protocol
    accounting kept as a number), and which tokens the protocol does not account per component.
11. **How quotes are checked**: the venue's own quote function (the harness calls it through
    `eth_call` as an independent oracle), checked against a real swap; golden vectors: real swaps
    in both directions, near capacity, and around mutation events. Record `venue_quotes` on every
    chain: the quote function's raw `eth_call` (`to`, `data`, the returned `result` word for word)
    at a block, for small, medium and near-limit amounts in each direction. The simulation layer
    tests the implementation against exactly these, to the wei: an under-quote is invisible to
    exact simulation-vs-execution, because the router keeps the surplus.
11a. **Gas**: `gas.swap_gas_used` of a real swap on the venue (its receipt's `gasUsed`, the tx as
    `evidence`), and what the router hop adds if a sibling shows it. Past estimates were off by 2x
    to 10x; the execution layer measures the router test against this.
11b. **Coverage**: what the integration serves against the venue's whole, as one number per
    chain: `coverage.served` and `coverage.total` in `tvl_usd` (or `volume_usd_30d`), with the
    `source` (DefiLlama, the venue's subgraph, a chain query). Pools or hooks left out are named in
    `out_of_scope` with their share. A person sees it at Gate 1.
12. **Test range**: a block range on one chain that meets the streamed test's coverage floor: at
    least 20 blocks in which an expected component changes, every expected component changing in
    at least 2 blocks, at least 100 execution checks in total and 10 per component, every tradable
    direction quoted after a delta. Count the venue's events on chain to pick it. For a venue with
    too little activity, write a fork setup script under `{{run}}/fork/` that creates state on an
    anvil fork of a recent block (create a pool, add liquidity, swap, fire each mutation surface)
    and record that the floor cannot be met on history.
13. **Nearest integrated sibling** on main and the differences from it.
14. **Other implementations**: search open-source aggregator code (KyberSwap's
    `kyberswap-dex-lib`, ParaSwap's `paraswap-dex-lib`, and others) for this venue. What another
    implementation does is a lead the falsifier tests, not evidence. Record each repo's license;
    reading code for the math is fine, reusing it needs a compatible license.
15. **Open risks** and **out of scope**.

The harness gates on this run's tree (an OFF gate's facts are still recorded; they turn on later):

{{caps}}

DCI on this run's chains:

{{dci}}

## spec.json

```json
{
  "protocol": "{{protocol}}",
  "kind": "native | hybrid | vm",
  "kind_fact": "S2",
  "vm_reads": ["hybrid only: the state the quote reads through the VM, and from which contract"],
  "chains": ["ethereum"],
  "scope": "which markets, which sides",
  "facts": [
    {"id": "S1", "section": "summary | contracts | swap | state | components | balances | quotes | differences",
     "claim": "one fact, stated exactly",
     "critical": true,
     "status": "verified | unknown | contested",
     "evidence": [{"kind": "tx | block | command | source | trace", "ref": "0x… | 21000000 | cast call … -b 21000000 | repo@commit:path:line", "chain": "ethereum", "note": "what it shows"}]}
  ],
  "contracts": [{"chain": "ethereum", "address": "0x…", "role": "…", "proxy": true, "implementation": "0x…", "verified_source": "etherscan | sourcify | none", "deployment_block": 0}],
  "dependencies": [{"component_kind": "pool", "contract": "0x… or a role", "role": "rate provider", "address_in": "traced storage | calldata | event", "repointable": true, "values": "what an attribute would need: slots or events", "fact_ids": ["S11"]}],
  "mutation_surfaces": [{"id": "M1", "name": "fee change", "who": "owner", "signal": "event FeeSet | slot 5 write only", "fired_at_blocks": [0], "fact_ids": ["S7"]}],
  "golden_vectors": [{"tx": "0x…", "component": "0x…", "token_in": "0x…", "token_out": "0x…", "amount_in": "…", "amount_out": "…", "note": "near capacity"}],
  "quote_function": {"chain": "ethereum", "address": "0x…", "signature": "quote(address,address,uint256)(uint256)", "checked_against_tx": "0x…"},
  "directions": [{"component": "0x… or a kind", "only": ["0xTokenA", "0xTokenB"], "reason": "…"}],
  "balance_source": [{"component_kind": "pool", "source": "self | holder:<attribute or address> | min_of:[…]", "simulation_reads": false, "lens": "ComponentIdBalances | BalanceOwnerBalances | own: <what it calls>", "unaccounted_tokens": []}],
  "limits": [{"component": "0x… or a kind", "direction": "0xTokenIn->0xTokenOut or default", "kind": "hard | soft | under_cap", "rounding": 0, "why": "the contract code or chain read that shows it", "fact_ids": ["S9"]}],
  "venue_quotes": [{"id": "V1", "chain": "ethereum", "block": 21000000, "to": "0x… (quote contract)", "data": "0x… (cast calldata …)", "result": "0x… (the raw return)", "direction": "0xIn->0xOut", "amount_in": "…", "caller_fact": "S14 (only when the router gets another answer)"}],
  "watch": [{"chain": "ethereum", "address": "0x…", "block": 21000000, "what": "implementation code"}, {"chain": "ethereum", "address": "0x…", "block": 21000000, "data": "0x… (getter calldata)", "what": "fee()"}],
  "read_set": [{"chain": "ethereum", "address": "0x…", "slots": ["0x…"], "tracked_by": "attribute `reserves` | VM storage", "why_untracked": "immutable after deploy"}],
  "gas": {"swap_gas_used": 0, "evidence": "0x… (tx)", "router_overhead": "from the sibling's router test, if known"},
  "coverage": {"served": 0, "total": 0, "unit": "tvl_usd | volume_usd_30d", "source": "…", "per_chain": {"ethereum": [0, 0]}},
  "proxies": [{"chain": "ethereum", "proxy": "0x…", "implementation": "0x…", "slot": "0x3608…bbc (EIP-1967; a beacon proxy: 0xa3f0…d50, the beacon slot, and the gate asks the beacon for implementation())"}],
  "test_range": {"chain": "ethereum", "start_block": 0, "stop_block": 0, "why": "counts per floor item"},
  "fork_setup": {"script": null, "why": ""},
  "sibling": {"protocol_system": "…", "why": "…", "differences": ["…"]},
  "templates": [{"repo": "…", "path": "…", "license": "MIT", "use": "lead for the swap math"}],
  "open_risks": ["…"],
  "out_of_scope": ["…"]
}
```

Every fact with `status: verified` has evidence. A fact you cannot establish is `unknown`; a
critical unknown stops the run, so say in its claim what would settle it. A question only a person
can answer (the team's intent, off-chain behaviour) goes in your summary.

End your final message with:

```json
{"facts": 0, "critical_unknown": ["S…"], "contested": ["S…"], "questions_for_person": ["…"]}
```
