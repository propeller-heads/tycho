# protocols/adapter-integration/

Foundry project containing Solidity integration tests for VM adapter contracts. Adapters are only
needed for **VM protocol integrations** — they implement the on-chain interface used by
`tycho-simulation` to execute swaps inside `revm`. Native protocol implementations do not require
an adapter. 

**Not a Rust workspace member** — run with `forge test` from `protocols/adapter-integration/evm/`.

## Layout

Organised by protocol under `evm/src/` and `evm/test/`:

- `src/{protocol}/` — adapter contract source (angle, balancer-v2, balancer-v3, bopamm, curve,
  etherfi, fermiswap, integral, liquidityparty, maverick-v2, sfrax, sfraxeth, uniswap-v2,
  template); plus shared `src/interfaces/` and `src/libraries/`
- `test/{Protocol}Adapter.t.sol` or `test/{Protocol}SwapAdapter.t.sol` — fork tests validating swap
  encoding and on-chain execution. The suffix is not consistent, and a test file's name does not
  always match its `src/` directory (e.g. `FraxV3SFraxAdapter.t.sol` covers `src/sfrax/`), so
  grep rather than guess
- `test/executors/`, `test/mocks/`, `test/interfaces/` — shared test scaffolding
