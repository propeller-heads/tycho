# balancer_v3 Substreams modules

This package was initialized via `substreams init`, using the `evm-events-calls` template.

## Usage

```bash
substreams build
substreams auth
substreams gui       			  # Get streaming!
substreams registry login         # Login to substreams.dev
substreams registry publish       # Publish your Substreams to substreams.dev
```

## Modules

All of these modules produce data filtered by these contracts:
- _vault_ at **0xba1333333333a1ba1108e8412f11850a5c319ba9**
- _stable_pool_factory_ at **0xb9d01ca61b9c181da1051bfdd28e1097e920ab14**
- _weighted_pool_factory_ at **0x201efd508c8dfe9de1a13c2452863a78cb2a86cc**
- _reclamm_pool_factory_ at **0x3ccd78683effffddc1a16f5553c896ac6d3ab7ff**
- stable_pool contracts created from _stable_pool_factory_
- weighted_pool contracts created from _weighted_pool_factory_
- reclamm_pool contracts created from _reclamm_pool_factory_
### `map_events_calls`

This module gets you events _and_ calls


### `map_events`

This module gets you only events that matched.



### `map_calls`

This module gets you only calls that matched.


