---
name: reverse-engineering-pamm-contracts
description: Use when a pAMM, propAMM or oracle-priced market maker must be simulated natively but its pricing contract is unverified, when a decompiler returns empty or `require(arg0 == address(arg0))`-only function bodies, when a quote reverts with a staleness error such as StaleUpdate, or when asked which storage slots a contract's quote reads or how often a market maker updates its prices.
---

# Reverse-Engineering pAMM Contracts

## Overview

The bytecode is the specification, and a static reading of it is the only thing that covers
every path. Map the swap path from one real swap, read every contract on it (verified source,
else gigahorse's TAC), and write a model in which every formula and branch cites the lines it
came from. Then check the reading against `eth_call`: a differential test, a read-set check
and branch coverage. Traces and samples confirm a reading or choose between two; they never
replace one. Model the call that executes (`swap`), not only the quote view.

## When to Use

- Porting a pAMM's pricing to native Rust (`ProtocolSim`)
- Finding which storage slots a quote reads, to index or decode them
- Finding a venue's live contracts, and how fresh its on-chain price is at any block

"Verified" is per contract, never per venue: a verified router or proxy usually fronts
unverified pricing code.

## Tools

Scripts are in `scripts/` next to this file: `python3 -I scripts/<tool>.py`, and for
`evmole_layout.py` and `coverage.py classify|report`
`uv run -q --with evmole==0.9.4 python -I`. Install gigahorse once with
`bash scripts/gigahorse_setup.sh DIR`, DIR outside this skill. Set `ETH_RPC_URL` to an archive
node of the right chain; run steps 4-5 against an anvil fork of it
(`anvil --fork-url URL --fork-block-number N`). Read `reference.md` before step 1: tool limits,
TAC idioms, chain differences (L2s), shell and RPC pitfalls.

| Tool | Answers |
|------|---------|
| `calltree.py` | call tree of a mined tx or a call: contracts, selectors, DELEGATECALL hops, return data, reverts |
| `inspect_contract.py ADDR` | code hash, Sourcify status and sources, proxies and their targets, linked addresses, selectors |
| `gigahorse_run.py`, `tac.py` | decompiled TAC; per function: call tree, revert strings, storage |
| `evmole_layout.py` | functions, argument types, storage slots and their readers/writers (inferred, often wrong) |
| `trace_call.py`, `trace_tx.py`, `storage_access.py`, `symreplay.py` | one path: struct-log trace, its storage and env reads, its symbolic replay |
| `coverage.py classify\|trace\|report` | every JUMPI as fork, guard or ABI check; edges a set of cases took; edges never taken |
| `difftest.py` (library) | batched `eth_call` with state, code, block and gas-price overrides; swap storage writes; funding |
| `update_cadence.py`, `liveness.py` | update rate and ordering against swaps; is the deployment still in use |

## Process

1. **Find the live contracts.** Start from whatever lists the venue: a router repository and
   its changelog, an aggregator's adapter list, the venue's docs. These are hints; the chain
   decides. Follow pointer getters (`engine()`) and list getters keyed by pair
   (`poolsForPair`). Prove liveness with recent swaps and price updates: venues redeploy at
   new addresses and leave the old contracts deployed and verified.
2. **Map the swap path.** Run `calltree.py` on one recent mined swap: every address in the tree
   is on the path, and every DELEGATECALL callee is code to read, whatever the proxy kind.
   Find swaps that skip the router (40% of one venue's fills called its engine directly):
   every entry point that reaches the pricing code is in scope. `inspect_contract.py` each
   address, fetch every verified source, `gigahorse_run.py` the unverified ones.
3. **Read every contract, completely.** Verified source first; else `tac.py tree` on the swap
   and quote. List every JUMPI on the path as a fork or a guard with its condition and revert
   reason before writing the model: a 2,000-line tree is an hour of reading, not a reason to
   skip it (a skipped 2,000-line contract hid a live pricing branch). Record, with TAC line
   numbers:
   - every formula, with its rounding;
   - every storage slot read and written, as a formula an indexer can compute; the price may
     live in another contract (a registry shared by many venues, an oracle) or arrive in the
     swap's calldata, in which case there is nothing to index;
   - every external call: a standard view (`balanceOf`) is a model input, a pricing
     dependency gets this whole process, a state change belongs in the swap diff;
   - every read of `TIMESTAMP`, `NUMBER`, `ORIGIN`, `CALLER`, `GASPRICE`, `BASEFEE`, and
     every check an admin can flip (pause, active flag, cap 0, pointer) for `watch.json`;
   - test hooks: entry points that take prices as arguments, gates a `from` can satisfy.

   Use `symreplay.py` on a direct call to the contract only where the TAC is imprecise:
   recursive or looped private functions (PHIs listing the function's own arguments) and
   helper calls with ambiguous arguments. Fit formulas against samples only to choose
   between rounding variants of operands you have read.
4. **Make the call succeed.** Fork at the last block with a price update, or override the
   price word itself, which also makes each of its fields a test input. Take the freshness
   rule from the code. Call as the real caller; fund the input
   (`difftest.funding_overrides`, the router's balance for push routers, a payer contract via
   `code` override for callbacks) and the output side (vault balance and allowance). Test
   every `ORIGIN` gate through a payer contract: in a direct call `tx.origin` is the sender.
5. **Prove the reading.** Write a model that reads storage through `difftest.Chain`, returns
   `(amountIn, amountOut)` and the storage words the swap writes, and cites its sources. Take
   one pair to the end of this step before adding others.
   - **Read set:** run one case through `debug_traceCall` with `prestateTracer` (no
     `diffMode`) and the case's overrides: it lists every account and slot the contract read.
     Diff that against the keys of the model's `difftest.Chain` cache. Every slot or address
     the contract reads and the model does not is an unmodelled input or a constant; name
     each.
   - **Difftest:** each layer alone (price source, pricing contract, router), then end to
     end, through every entry point: random amounts over 8 orders of magnitude, both
     directions, both exact modes, every config field at 0, max and sign flip, and the
     swap's storage writes (`difftest.storage_writes`). One sequence test per `NUMBER` or
     `TIMESTAMP` read: swap, advance the fork, quote. Re-run at a second update block. Break
     the model once and watch the diff fail.
   - **Coverage:** `coverage.py trace` the cases on one contract per code family and
     `coverage.py report` until no fork or non-panic guard edge is unseen; run the model
     under Python `coverage` in branch mode until every model branch was taken. An unseen
     edge is explained only by the invariant that makes it unreachable (name the earlier
     check that enforces it), or a case you add, or a mock callee via `code` override.
     Anything else is a gap: the README says "not proven" for that layer, and it blocks the
     port of every function that reaches it.

   On a mismatch, re-read the code at that branch; trace the case only to locate it.
6. **Measure update cadence** with `update_cadence.py`: share of blocks (and seconds) with an
   update, the median and p90 gap, whether updates sit just before the venue's swaps, who
   sends them and at what priority fee. Say what that means for a quote from indexed state,
   from this chain's data (`reference.md`, "Update delivery").
7. **Write the output**, then port with `port.md` when asked to.

## Output

One directory per venue, `research/pamm/<venue>/` unless the user names another: a README
(contracts, entry points, storage, behaviour, proof, freshness, ProtocolSim hints, gaps), the
model and its difftest, the case fixture, the coverage reports and a `watch.json` that passes
`liveness.py`. Read `output.md` for the exact contents.

## Common Mistakes

| Mistake | Fix |
|---------|-----|
| Trusting a "verified" venue | Check each contract on the path; a verified proxy or router says nothing about the code behind it. |
| Modelling from traces or samples | A trace is one path; samples never reach a branch the live state does not take. Read the code. |
| Calling a gap "explained" | Only an invariant, an added case or a mock explains an unseen edge. |
| Breadth before depth | One pair proven end to end beats twenty pairs half proven. |
| A difftest that never failed | Break the model once; a diff that cannot fail proves nothing. |
| Modelling only the quote view | Execution runs `swap`; its output and its writes are what the native state must match. |
| Reversing a dormant engine | Check the last price update and last swap before starting. |
| Quietly matching partial fills | Clamps that keep the full input belong in `get_limits` and as errors. |
