

### Streaming Options

Tycho Client supports several options to customize the data stream. These are available as CLI flags, Rust builder
methods, and Python parameters — refer to each client's documentation for usage details.

| CLI Flag                | Rust Builder               | Python                     | Description                                                         |
|-------------------------|----------------------------|----------------------------|---------------------------------------------------------------------|
| `--partial-blocks`      | `.enable_partial_blocks()` | `partial_blocks=True`      | Enable partial block updates (flash blocks) for sub-block latency   |
| `--no-state`            | `.no_state(true)`          | `include_state=False`      | Only stream components and tokens; omit snapshots and state updates |
| `--include-tvl`         | `.include_tvl(true)`       | `include_tvl=True`         | Include TVL estimates in messages                                   |
| `--disable-compression` | `.disable_compression()`   | `disable_compression=True` | Disable zstd compression for WebSocket messages                     |
| `--no-tls`              | `.no_tls(true)`            | `use_tls=False`            | Use unencrypted transports (http/ws instead of https/wss)           |

{% hint style="info" %}
**Not recommended:** `--disable-compression` and `--no-tls` are intended for debugging only. Compression (zstd)
significantly reduces message sizes and improves throughput, while TLS is required to connect to the hosted endpoint.
Keeping both enabled (the default) is recommended for production use.
{% endhint %}

#### Partial Blocks

Some chains, such as <a href="https://docs.base.org/building-with-base/differences/flashblocks" target="_blank" rel="noopener noreferrer">Base</a>, support _flash blocks_ —
pre-confirmation updates that contain parts of a future block before it is finalized. When `--partial-blocks` is
enabled, Tycho streams these incremental updates as they arrive, giving you sub-block latency. On chains without flash block
support, enabling this flag has no effect — full blocks are delivered as usual.

{% hint style="warning" %}
Block hashes in partial block messages are **unstable** — they change between partial updates and will differ from the
final block hash. Do not use them as persistent identifiers or cache keys. See
the <a href="https://docs.substreams.dev/reference-material/chain-support/flashblocks#developing-for-partial-blocks" target="_blank" rel="noopener noreferrer">Substreams documentation</a>
for details.
{% endhint %}

#### No State

By default, each message includes full component snapshots (on the first sync) and state deltas (reserves, balances,
contract storage) on every subsequent block. If you only need to discover which components exist and which tokens they
involve — for example, to build a pool registry or monitor new deployments — you can disable state with `--no-state`.
This significantly reduces message sizes and processing overhead since snapshots and per-block state updates are omitted
entirely.

#### Include TVL

When enabled, each message includes an approximate TVL estimate for every tracked component. This is useful for building
dashboards or for ranking pools by liquidity. Note that enabling this option increases startup latency: for each
snapshot request, the client makes additional RPC calls to fetch token prices and compute TVL for all tracked
components. This overhead scales with the number of components you're tracking.
