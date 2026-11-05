# balancer_v3 Substreams modules

This package indexes Balancer V3 pools across multiple networks using manifest-level
configuration. The same WASM module graph is reused; chain-specific contract addresses are
provided via substreams `params`.

## Usage

```bash
substreams build
substreams auth
substreams gui
substreams registry login
substreams registry publish
```

## Configuration

Contract addresses are configured per manifest via query-string params on map/store modules:

- `vault` — Balancer V3 Vault
- `vault_extension` — Vault extension contract
- `batch_router` — Batch router for swaps
- `permit2` — Permit2 authorization contract
- `weighted_factory` — Weighted pool factory
- `stable_factory` — Stable pool factory
- `reclamm_factory` — ReClamm pool factory
- `skip_rate_provider_pools` — When `true`, pools whose factory `Create` call includes any
  `WITH_RATE` token are not emitted as protocol components (optional, default `false`). Set to
  `true` on L2 deployments where RPC nodes lack DCI/tracing support; yield-bearing pools with rate
  providers will not be indexed.

See `substreams.yaml` (Ethereum mainnet) and the network-specific manifests:

- `base-balancer-v3.yaml`
- `arbitrum-balancer-v3.yaml`
- `gnosis-balancer-v3.yaml`

Addresses are sourced from the [Balancer deployments repo](https://github.com/balancer/balancer-deployments).

## Modules

- `map_components` — discovers pools from factory create calls
- `map_protocol_changes` — aggregates components, contract changes, and pool token balances
  read as absolute values from Vault storage writes
