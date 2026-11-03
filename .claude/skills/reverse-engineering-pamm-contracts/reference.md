# Reference: tools, idioms, chains

## Tool limits

- **evmole** infers. It collapses a mapping to a struct into one field, misses functions in
  read/write lists (a swap that writes storage shown as `view`), and guesses types. It found
  none of the pricing slots on a venue whose storage is addressed by hash. Use it as a first
  map only.
- **gigahorse** covers every path. Its storage client and `tac.py storage` miss storage
  addressed by a hash with no slot number in it (`keccak(abi.encode(a, b))`, EIP-7201
  namespaces) and state kept in another contract: follow `SHA3` then `SLOAD` in the tree and
  compute the keys. A private function that calls itself, or is entered from several call
  sites, gets every caller's arguments merged into one PHI (operands from 14 to 19 values):
  read such a function with `symreplay.py` on one direct call per region.
- **symreplay** replays one executed path. A gate on an env value it treats as constant
  disappears from the replay.
- **Formula fitting** against live samples only chooses between rounding variants of a
  formula whose operands you read. It cannot find a branch the sampled state never takes.
- **coverage.py** measures one code address per pass; contracts deployed per pair have
  different bytecode (immutables), so measure one representative.

## TAC idioms

| TAC | Meaning |
|-----|---------|
| 8-15 line helper: `ADD` then `GT`/`LT` then `Panic(0x11)` | checked add (likewise sub, mul; `Panic(0x12)` for div) |
| `MULMOD` with `LT`/`SUB` around a `MUL` | 512-bit `mulDiv` |
| `b ^ ((a ^ b) * (a < b))` | `min(a, b)` (swap the comparison for `max`) |
| `SIGNEXTEND` pairs | int narrowing, noise |
| `SUB(0, x)` used as a condition | `x != 0` |
| a second `JUMPI` on a call's success after `if iszero(success) revert` | compiler re-check, not behaviour |

## Chains

- **Trace APIs.** `trace_filter` and custom JS tracers are missing on many L2 nodes (op-geth,
  Arbitrum Nitro, zkSync), and `update_cadence.py` and `liveness.py` need `trace_filter`.
  Updates are usually top-level transactions and can be found from block bodies
  (`eth_getBlockByNumber` with full transactions) or from their events (`eth_getLogs`);
  internal router swaps need `debug_traceBlockByNumber` with `callTracer`. An anvil fork runs
  JS tracers for any chain.
- **`block.number`** is chain-specific: on Arbitrum it is the L1 block number, shared by many
  L2 blocks (ArbSys at `0x64` gives the L2 one), and an anvil fork returns the L2 number.
  List every `NUMBER`, `TIMESTAMP`, `GASPRICE`, `BASEFEE` read and every precompile call
  before trusting a fork.
- **Sub-second blocks** share a timestamp: "fresh in this second" spans several blocks, and
  timestamp-keyed state aggregates across them. Measure cadence in seconds as well as blocks.
- **`eth_call` at block B** runs with `block.number = B` on the state after B, not as B+1.
- **Proxies** other than EIP-1967: EIP-1167 minimal proxies, EIP-7702 delegated accounts,
  EIP-2535 diamonds, Safe proxies. A DELEGATECALL in the call tree finds all of them.

## Update delivery

What an update pattern means depends on the chain. On a chain with block builders, an update
at zero priority fee that always lands one transaction before the venue's fill is delivered
with the fill: the indexed price is stale between fills. On a sequencer chain read only the
ordering and the sender (same sender, consecutive nonces: the maker sends both); tips may mean
nothing there. Updates every block, independent of fills, keep the indexed price current to
within a block. Report the pattern you measured, not the one you expect.

## Shell and RPC

- `zsh` does not word-split variables: put loops in a `bash` script with `set -euo pipefail`.
- Output-filtering hooks can rewrite `cat`, `rg`, `grep`, `curl`: prefix `command`, or use the
  scripts here.
- RPC 429/503, "overloaded" errors and 1,000-block log limits: chunk ranges and retry, as the
  scripts do. Generic env names such as `RPC_URL` may point at another chain; the scripts
  print the chain id first.
