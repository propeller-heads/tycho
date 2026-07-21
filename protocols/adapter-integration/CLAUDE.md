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
  template)
- `test/{Protocol}Adapter.t.sol` — fork tests validating swap encoding and on-chain execution
