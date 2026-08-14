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

- `vault` — Balancer V3 Vault; also emitted on every component as the `vault` static attribute
- `vault_extension` — Vault extension contract
- `batch_router` — Batch router for swaps
- `permit2` — Permit2 authorization contract
- `weighted_factories` — Weighted pool factories, keyed by version label
- `stable_factories` — Stable pool factories, keyed by version label
- `reclamm_factories` — ReClamm pool factories, keyed by version label
- `skip_rate_provider_pools` — When `true`, pools whose factory `Create` call includes any
  `WITH_RATE` token are not emitted as protocol components (optional, default `false`). Set to
  `true` on L2 deployments where RPC nodes lack DCI/tracing support; yield-bearing pools with rate
  providers will not be indexed.

Each factory family takes any number of generations, written as a query-string map from a version
label to an address:

```
weighted_factories[v1]=201efd508c8dfe9de1a13c2452863a78cb2a86cc&weighted_factories[v2]=…
```

Balancer keeps the `create` signature and the `PoolCreated` event stable across generations, so one
decoder handles every version of a family. The label names the generation for whoever reads the
manifest and goes no further: the `pool_type` static attribute carries the family alone — for
example `WeightedPoolFactory` — because a family prices the same whichever generation built the
pool. Where a generation does differ, consumers probe the pool for the feature rather than read a
label, and reCLAMM's earlier maths is kept out by configuring only its newest factory.

Omit a family entirely when it is not deployed on a chain. `map_components` fails when no family is
configured at all, since it would have nothing left to match.

Pools created with an external hooks contract are not indexed: hooks run arbitrary code on swaps,
which the native maths in `tycho-simulation` does not model. The reCLAMM factory takes no hooks
parameter, so only the weighted and stable families are filtered.

See `substreams.yaml` (Ethereum mainnet) and the network-specific manifests:

- `base-balancer-v3.yaml`
- `arbitrum-balancer-v3.yaml`
- `gnosis-balancer-v3.yaml`

Addresses are sourced from the [Balancer deployments repo](https://github.com/balancer/balancer-deployments).

## Modules

- `map_components` — discovers pools from factory create calls
- `map_protocol_changes` — aggregates components, contract changes, and pool token balances
  read as absolute values from Vault storage writes
