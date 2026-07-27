---
description: Run the Tycho Indexer on an EVM chain with no hosted Substreams endpoint
---

# Self-Hosted EVM Chain

## Overview

Tycho indexes a chain by streaming blocks from a <a href="https://substreams.streamingfast.io/" target="_blank" rel="noopener noreferrer">Substreams</a> endpoint. For most chains you point the indexer at a hosted StreamingFast or Pinax endpoint and never run any Firehose infrastructure yourself — see [Hosted Endpoints](hosted-endpoints.md).

Some EVM chains have no hosted Substreams endpoint. To index one of those, run your own Firehose + Substreams stack next to the indexer. The `substreams-endpoint` Docker Compose profile does exactly that: it spins up a single-container Firehose that polls blocks from an EVM JSON-RPC node and serves them over the same gRPC interface the indexer expects.

Choose self-hosted when **no hosted Substreams endpoint exists for your chain**. If a hosted endpoint exists, prefer it — it needs no extra infrastructure.

{% hint style="warning" %}
The RPC poller produces **base blocks** (no contract storage — VM protocols and DCI do not work). Read [Limitations](#limitations) first.
{% endhint %}

## Limitations

### Base block model — VM protocols and DCI do not work

The poller reads blocks over plain JSON-RPC, which exposes logs and receipts but not storage writes or internal calls. It therefore produces <a href="https://docs.substreams.dev/reference-material/chain-support/chains-and-endpoints#evm-extended-vs-base-block-model" target="_blank" rel="noopener noreferrer">base blocks</a> — the protobuf fields for storage and call data arrive empty. Hosted endpoints backed by instrumented nodes serve **extended** blocks that carry all of it.

To overcome it, run your own instrumented node: a <a href="https://github.com/streamingfast/go-ethereum/releases" target="_blank" rel="noopener noreferrer">Firehose-patched build</a> of the chain's execution client (available for geth, op-geth, BNB, Polygon, Arbitrum) under `fireeth start reader-node`, in place of the poller.

### Single machine — meant for dev/test usage

Every component runs in one container against one RPC endpoint — no high availability, no failover. For production, consider a distributed topology following the <a href="https://firehose.streamingfast.io/firehose/overview/distributed-deployment" target="_blank" rel="noopener noreferrer">Firehose deployment and scaling guide</a>.

## Prerequisites

- <a href="https://docs.docker.com/get-docker/" target="_blank" rel="noopener noreferrer">Docker</a> and Docker Compose.
- The `tycho-indexer` image, built locally. From the repo root, build it and tag it to match `TYCHO_IMAGE`:

  ```bash
  docker build -f docker/tycho-indexer.Dockerfile -t tycho-indexer:local .
  ```

- An EVM JSON-RPC endpoint for your chain. The poller fetches every block from it, so prefer a low-latency node — RPC latency directly caps the fetch rate.
- A compiled Substreams package (`.spkg`) for **each protocol you want to index, built for your chain**. A chain Tycho has never indexed has no ready-made package, so you build one — see [Building a Substreams package for your chain](#building-a-substreams-package). Place each `.spkg` under `docker/substreams/`; the compose file mounts that directory into the indexer at `/opt/tycho-indexer/substreams/`.

## Configuration

Configure the stack through `docker/.env`. The compose file reads it for both the indexer and the Firehose service.

### Environment variable reference

<table><thead><tr><th width="220">Variable</th><th width="120">Service</th><th width="90">Required</th><th width="180">Default</th><th>Purpose</th></tr></thead><tbody>
<tr><td><code>RPC_URL</code></td><td>poller + indexer</td><td>Yes</td><td>—</td><td>EVM JSON-RPC endpoint. The poller fetches blocks from it; the indexer reads token metadata from it.</td></tr>
<tr><td><code>SUBSTREAMS_ENDPOINT</code></td><td>indexer</td><td>Yes</td><td><code>https://mainnet.eth.streamingfast.io:443</code></td><td>Substreams tier1 gRPC. Self-hosted: <code>http://substreams-endpoint:10016</code>.</td></tr>
<tr><td><code>TYCHO_IMAGE</code></td><td>indexer</td><td>Yes</td><td>—</td><td>tycho-indexer image tag.</td></tr>
<tr><td><code>START_BLOCK</code></td><td>poller</td><td>No</td><td><code>0</code></td><td>First block the poller fetches.</td></tr>
<tr><td><code>CHAIN_NAME</code></td><td>poller</td><td>No</td><td><code>mainnet</code></td><td>Chain name the Firehose advertises (<code>--advertise-chain-name</code>).</td></tr>
<tr><td><code>CHAINS</code></td><td>indexer</td><td>No</td><td><code>ethereum</code></td><td>Active chain to index. The indexer uses only the first value (multichain is not yet supported). Name a built-in chain, or a custom chain you declare in <code>chains.yaml</code> (see below).</td></tr>
<tr><td><code>RETENTION_HORIZON</code></td><td>indexer</td><td>No</td><td><code>2000-01-01T00:00:00</code></td><td>Earliest version history the indexer keeps. Use a <strong>future</strong> date to keep no historical state (recommended) — see <a href="#retention-horizon">the note below</a>. The <code>2000-01-01</code> default keeps all history and fails a from-scratch historical backfill.</td></tr>
<tr><td><code>EXTRACTORS_CONFIG</code></td><td>indexer</td><td>No</td><td><code>/opt/tycho-indexer/extractors.yaml</code></td><td>Path to the extractors config inside the container.</td></tr>
<tr><td><code>TYCHO_CHAINS_CONFIG</code></td><td>indexer + consumers</td><td>No</td><td><code>/opt/tycho-indexer/chains.yaml</code></td><td>Path to the custom-chains config. The indexer and every consumer (tycho-simulation, tycho-client, tycho-execution) read the same variable. Only needed for a non-built-in chain.</td></tr>
<tr><td><code>SUBSTREAMS_API_TOKEN</code></td><td>indexer</td><td>No</td><td><code>readme</code></td><td>Auth token for a hosted Substreams endpoint; unused self-hosted.</td></tr>
<tr><td><code>TRACE_RPC_URL</code></td><td>indexer</td><td>For DCI</td><td><code>readme</code> (placeholder)</td><td>Trace-capable RPC for dynamic contract indexing.</td></tr>
<tr><td><code>OTLP_EXPORTER_ENDPOINT</code></td><td>indexer</td><td>No</td><td>empty (disabled)</td><td>OpenTelemetry collector. Set <code>http://lgtm:4317</code> with the <code>observability</code> profile.</td></tr>
<tr><td><code>AUTH_API_KEY</code></td><td>indexer</td><td>No</td><td><code>local-dev-key</code></td><td>Tycho RPC API key.</td></tr>
<tr><td><code>RUST_LOG</code></td><td>indexer</td><td>No</td><td><code>info</code></td><td>Log level.</td></tr>
</tbody></table>

A self-hosted `docker/.env` for the Tempo chain looks like this:

```bash
TYCHO_IMAGE=tycho-indexer:local

RPC_URL=https://rpc.tempo.xyz
TRACE_RPC_URL=https://rpc.tempo.xyz
CHAIN_NAME=tempo
START_BLOCK=6455886

# Self-hosted Firehose, reachable by its docker service name
SUBSTREAMS_ENDPOINT=http://substreams-endpoint:10016

# Custom chain — defined in chains.yaml (see below)
CHAINS=tempo

# Keep no historical state (recommended); the 2000-01-01 default keeps all history and breaks a from-scratch backfill (see "Retention horizon")
RETENTION_HORIZON=2100-01-01T00:00:00
SUBSTREAMS_API_TOKEN=local
```

### Writing your extractors.yaml

Edit <a href="https://github.com/propeller-heads/tycho/blob/main/crates/tycho-indexer/extractors.yaml" target="_blank" rel="noopener noreferrer"><code>crates/tycho-indexer/extractors.yaml</code></a> — the compose file mounts it into the container at `/opt/tycho-indexer/extractors.yaml`. Each entry under `extractors:` configures one protocol:

{% hint style="warning" %}
The indexer builds **every** entry in this file at startup and fails if any referenced `.spkg` is missing. The shipped file lists Ethereum protocols whose packages are not in `docker/substreams/`. Replace them with only the extractors for the chain and protocols you are indexing, each pointing at a `.spkg` you have placed under `docker/substreams/`.
{% endhint %}


```yaml
extractors:
  uniswap_v3:
    name: "uniswap_v3"
    chain: "tempo"
    implementation_type: "Custom"
    sync_batch_size: 1000
    start_block: 6455886
    protocol_types:
      - name: "uniswap_v3_pool"
        financial_type: "Swap"
    spkg: "substreams/tempo-uniswap-v3-v0.1.0.spkg"
    module_name: "map_protocol_changes"
```

<table><thead><tr><th width="220">Field</th><th>Purpose</th></tr></thead><tbody>
<tr><td><code>name</code></td><td>Unique extractor name; also the protocol system name exposed over the RPC.</td></tr>
<tr><td><code>chain</code></td><td>Chain this extractor runs on — a built-in chain name, or a custom chain defined in <code>chains.yaml</code> (see below). The indexer rejects an unknown chain name at startup.</td></tr>
<tr><td><code>implementation_type</code></td><td><code>Custom</code> for natively integrated protocols — the Substreams emits protocol attributes and the pool maths are implemented natively in tycho-simulation. <code>Vm</code> for protocols whose full contract state is indexed and whose logic runs in a local VM inside tycho-simulation. The indexer stores this as metadata; downstream consumers (e.g. tycho-simulation) act on it.</td></tr>
<tr><td><code>sync_batch_size</code></td><td>How many blocks the indexer buffers before flushing them to Postgres in a single write, during the initial catch-up sync. Substreams delivers messages as a continuous stream — this only tunes DB write batching while syncing.</td></tr>
<tr><td><code>start_block</code></td><td>Block at which the protocol was deployed; the indexer starts streaming here.</td></tr>
<tr><td><code>spkg</code></td><td>Path to the compiled <code>.spkg</code>, relative to <code>/opt/tycho-indexer/</code> (i.e. under <code>docker/substreams/</code>).</td></tr>
<tr><td><code>module_name</code></td><td>Substreams output module to consume (e.g. <code>map_protocol_changes</code>).</td></tr>
<tr><td><code>protocol_types</code></td><td>Protocol component types this extractor produces, each with a <code>name</code> and a <code>financial_type</code> (<code>Swap</code>, <code>Psm</code>, <code>Debt</code>, or <code>Leverage</code>).</td></tr>
</tbody></table>

### Declaring a custom chain

Built-in chains (`ethereum`, `base`, `unichain`, …) need no extra config. To index a chain Tycho does not know, define it in a separate `chains.yaml` file. Copy <a href="https://github.com/propeller-heads/tycho/blob/main/crates/tycho-indexer/chains.example.yaml" target="_blank" rel="noopener noreferrer"><code>crates/tycho-indexer/chains.example.yaml</code></a> to `crates/tycho-indexer/chains.yaml` and edit it — the compose file mounts it into the container at `/opt/tycho-indexer/chains.yaml`, and the indexer reads it via `TYCHO_CHAINS_CONFIG` (the `--chain-config` flag). Each extractor's `chain:` field and `CHAINS` resolve against these entries; the indexer fails fast at startup if an extractor references a chain that is neither built-in nor defined here.

```yaml
chains:
  - name: tempo                       # the value used as `chain:` on extractors and in CHAINS
    chain_id: 12345                   # EVM chain id
    block_time_secs: 1
    native:                           # native gas token
      address: "0x0000000000000000000000000000000000000000"
      symbol: "ETH"
      decimals: 18
    wrapped_native:                   # wrapped native token (e.g. WETH)
      address: "0x0000000000000000000000000000000000000000"
      symbol: "WETH"
      decimals: 18
    default_tvl_thresholds:           # TVL gates (in native token units) for component tracking
      low: 1000
      medium: 10000
```

<table><thead><tr><th width="220">Field</th><th>Purpose</th></tr></thead><tbody>
<tr><td><code>name</code></td><td>Chain identifier; reference it from an extractor's <code>chain:</code> field and from <code>CHAINS</code>.</td></tr>
<tr><td><code>chain_id</code></td><td>EVM chain id.</td></tr>
<tr><td><code>block_time_secs</code></td><td>Average block time in seconds.</td></tr>
<tr><td><code>native</code></td><td>Native gas token: <code>address</code>, <code>symbol</code>, <code>decimals</code>.</td></tr>
<tr><td><code>wrapped_native</code></td><td>Wrapped native token (e.g. WETH): <code>address</code>, <code>symbol</code>, <code>decimals</code>.</td></tr>
<tr><td><code>default_tvl_thresholds</code></td><td>Liquidity gates in <strong>native-token units</strong> (e.g. ETH), read by downstream consumers (solvers, tycho-simulation) to decide which components to track — not by the indexer. Size <code>low</code>/<code>medium</code> to the USD floor you want at the native token's price; the Ethereum defaults target roughly $20k / $200k.</td></tr>
</tbody></table>

### Building a Substreams package

Tycho ships the Substreams module sources in <a href="https://github.com/propeller-heads/tycho/tree/main/protocols/substreams" target="_blank" rel="noopener noreferrer"><code>protocols/substreams/</code></a>. Each directory holds one WASM module plus a manifest per chain and fork that reuses it — for example <code>ethereum-uniswap-v2/</code> also carries the PancakeSwap manifests and the Arbitrum, BSC, Base, and Unichain variants. The WASM logic is chain-agnostic; each manifest pins the chain-specific factory address and start block. A chain Tycho has never indexed has no prebuilt `.spkg`, so you build one — from a logs-only module such as `ethereum-uniswap-v2` or `ethereum-uniswap-v3-logs-only` (see [Limitations](#limitations)).

To index a Uniswap-V2-style DEX on a new chain, reuse the `ethereum-uniswap-v2` module and add a manifest for your chain:

1. Copy an existing manifest, e.g. `protocols/substreams/ethereum-uniswap-v2/ethereum-uniswap-v2.yaml`, to `<chain>-<dex>.yaml` in the same directory.
2. Edit the copy: set the package `name`/`version`, set every module's `initialBlock` to the DEX factory's deployment block, and set the `params` line to `factory_address=<your factory>&protocol_type_name=<your pool type>`.
3. Build the WASM and pack the package (some protocols build with `--profile substreams` instead of `--release` — check that protocol's `Makefile` or `README`):

   ```bash
   cd protocols/substreams/ethereum-uniswap-v2
   cargo build --target wasm32-unknown-unknown --release
   substreams pack <chain>-<dex>.yaml
   ```

4. Copy the resulting `.spkg` into `docker/substreams/`, then point your extractor's `spkg:` field at it.

{% hint style="info" %}
`substreams pack` may warn that `network` is not set. This is harmless for the self-hosted stack — the Firehose advertises the chain through `CHAIN_NAME`, not the package.
{% endhint %}

## Running the stack

Start everything — Postgres, the self-hosted Firehose, and the indexer — with the `substreams-endpoint` profile:

```bash
cd docker
docker compose --profile substreams-endpoint up
```

On a cold start the poller begins at `START_BLOCK` and streams forward. The indexer only commits a block once it sits behind the finality horizon, so expect a delay before committed state appears — on a fresh chain the first cold start takes a while to reach the deployment block of your protocols.

### Retention horizon

{% hint style="danger" %}
Set `RETENTION_HORIZON` to a **future** date (the example uses `2100-01-01T00:00:00`) so the indexer keeps no historical state — only the current state of every component. This is the simplest setup and sidesteps a partition crash during backfill.

The `2000-01-01T00:00:00` default keeps the full version history. When you then backfill a chain from a deployment block more than about a month old, the indexer crashes on the first historical state update it writes:

```
duplicate key value violates unique constraint "component_balance_default_unique_pk"
```

pg_partman partitions the `component_balance`, `protocol_state`, and `contract_storage` tables by day on `valid_to`, keeping a one-month retention window. A superseded historical version carries a `valid_to` older than any existing partition, so it lands in the default partition and violates its uniqueness constraint. A future `RETENTION_HORIZON` drops those old versions before they are written. Use a recent date instead only if you genuinely need a short window of version history.
{% endhint %}

### Resuming after a restart

Both halves of the stack resume on their own:

- The poller auto-detects the highest stored one-block file under `/data/storage/one-blocks/` and resumes from there, so it skips already-fetched blocks.
- The indexer resumes from the cursor stored in its Postgres database.

`docker compose --profile substreams-endpoint down` followed by `up` **without `-v`** preserves both the Firehose data volume and the database, so the stack picks up where it left off. Passing `-v` deletes the volumes and forces a full cold start.

{% hint style="warning" %}
When you switch the chain you index (change `CHAIN_NAME`, `START_BLOCK`, or `CHAINS`), bring the stack down **with `-v`** first:

```bash
docker compose --profile substreams-endpoint down -v
```

The Firehose data volume is chain-specific. Reusing it for a different chain makes the Firehose advertise a first-streamable block from the previous chain, and the indexer fails with `initial block N smaller than first streamable block M`.
{% endhint %}

### Connecting to a hosted endpoint instead

To use a hosted Substreams endpoint rather than the self-hosted Firehose, omit the profile and set the endpoint and token:

```bash
cd docker
# in .env:
#   SUBSTREAMS_ENDPOINT=https://<hosted-endpoint>:443
#   SUBSTREAMS_API_TOKEN=<your-token>
docker compose up
```

Without `--profile substreams-endpoint`, the `substreams-endpoint` service never starts and the indexer streams straight from the hosted endpoint.

## Consuming the custom chain

The indexer serves your custom chain over RPC and WebSocket, but a consumer (tycho-simulation, tycho-client, tycho-execution) resolves a chain name against its own copy of the chain config. Point it at the same `chains.yaml` through the `TYCHO_CHAINS_CONFIG` environment variable — the same variable the indexer uses:

```bash
export TYCHO_CHAINS_CONFIG=crates/tycho-indexer/chains.yaml
```

With the variable set, a chain name like `tempo` resolves to its full config on first use. Leave it unset and the consumer resolves only built-in chains and rejects the custom name. When the variable points at a missing or malformed file, the stream builder rejects it at startup and returns a set-up error naming the config — the consumer never starts against a half-configured chain.

## Monitoring sync progress

Follow the logs to watch the stack catch up:

```bash
docker compose --profile substreams-endpoint logs -f
```

Signals to watch, in the order blocks flow through the stack:

- **Poller** — block-fetch rate. A healthy poller logs a steady stream of fetched blocks; a stalled or slow rate points at the RPC.
- **Merger** — bundles one-block files into merged-block segments.
- **Substreams tier2** — produces the state segments tier1 serves to the indexer.
- **tycho-indexer** — the committed block height climbs as finalized blocks reach the database.

Both services expose TCP healthchecks you can probe:

<table><thead><tr><th width="320">Endpoint</th><th width="120">Port</th><th>Service</th></tr></thead><tbody>
<tr><td>Firehose / Substreams tier1 gRPC</td><td><code>10016</code></td><td><code>substreams-endpoint</code></td></tr>
<tr><td>Tycho RPC</td><td><code>4242</code></td><td><code>tycho-indexer</code></td></tr>
</tbody></table>

For dashboards, logs, and traces, enable the `observability` profile alongside `substreams-endpoint` and set `OTLP_EXPORTER_ENDPOINT=http://lgtm:4317`. See [Observability](../OBSERVABILITY.md).

## Troubleshooting

- **Cold start takes a long time.** `START_BLOCK` is the Firehose poller's start block (where it begins fetching from the RPC), separate from each extractor's `start_block`. A fresh chain must fetch and merge every block from `START_BLOCK` before your protocols' deployment blocks appear. Set `START_BLOCK` at or just below your earliest protocol `start_block` so the poller skips irrelevant history.

## Performance tuning

- **`--interval-between-fetch`** (poller) — delay between RPC fetches. The compose file sets `0ms` (no delay) for maximum throughput. Raise it to throttle a rate-limited RPC.
- **RPC latency** — the poller fetches blocks sequentially, so round-trip latency directly bounds the fetch rate. A nearby, low-latency node is the single biggest lever on cold-start speed.
- **Chain block time vs fetch rate** — once caught up, the poller only needs to keep pace with the chain's block time. Cold start is fetch-bound; steady state is block-time-bound.
- **`--substreams-state-bundle-size`** (tier1/tier2) — number of blocks per state segment, `1000` by default. Larger bundles cut per-segment overhead at the cost of coarser caching granularity.
