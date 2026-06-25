---
description: Run the Tycho Indexer on an EVM chain with no hosted Substreams endpoint
---

# Self-Hosted EVM Chain

## Overview

Tycho indexes a chain by streaming blocks from a <a href="https://substreams.streamingfast.io/" target="_blank" rel="noopener noreferrer">Substreams</a> endpoint. For most chains you point the indexer at a hosted StreamingFast or Pinax endpoint and never run any Firehose infrastructure yourself — see [Hosted Endpoints](hosted-endpoints.md).

Some EVM chains have no hosted Substreams endpoint. To index one of those, run your own Firehose + Substreams stack next to the indexer. The `substreams-endpoint` Docker Compose profile does exactly that: it spins up a single-container Firehose that polls blocks from an EVM JSON-RPC node and serves them over the same gRPC interface the indexer expects.

Choose self-hosted when **no hosted Substreams endpoint exists for your chain**. If a hosted endpoint exists, prefer it — it needs no extra infrastructure.

{% hint style="warning" %}
The `substreams-endpoint` profile is a single-machine deployment: every Firehose component runs in one container. It suits development and testing, not production workloads that need high availability. For a distributed, fault-tolerant setup see the <a href="https://firehose.streamingfast.io/firehose/overview/deployment-and-scaling" target="_blank" rel="noopener noreferrer">Firehose deployment and scaling guide</a>.
{% endhint %}

## Prerequisites

- <a href="https://docs.docker.com/get-docker/" target="_blank" rel="noopener noreferrer">Docker</a> and Docker Compose.
- An EVM JSON-RPC endpoint for your chain. The poller fetches every block from it, so prefer a low-latency node — RPC latency directly caps the fetch rate.
- A compiled Substreams package (`.spkg`) for each protocol you want to index. Place it under `docker/substreams/`; the compose file mounts that directory into the indexer at `/opt/tycho-indexer/substreams/`.
- Optionally, a local <a href="https://github.com/propeller-heads/tycho-protocol-sdk" target="_blank" rel="noopener noreferrer">tycho-protocol-sdk</a> checkout if you index VM-based protocols (see [Pointing to a local tycho-protocol-sdk checkout](#pointing-to-a-local-tycho-protocol-sdk-checkout)).

## Configuration

Configure the stack through `docker/.env`. The compose file reads it for both the indexer and the Firehose service.

### Environment variable reference

<table><thead><tr><th width="220">Variable</th><th width="120">Service</th><th width="90">Required</th><th width="180">Default</th><th>Purpose</th></tr></thead><tbody>
<tr><td><code>RPC_URL</code></td><td>poller + indexer</td><td>Yes</td><td>—</td><td>EVM JSON-RPC endpoint. The poller fetches blocks from it; the indexer reads token metadata from it.</td></tr>
<tr><td><code>START_BLOCK</code></td><td>poller</td><td>No</td><td><code>0</code></td><td>First block the poller fetches.</td></tr>
<tr><td><code>CHAIN_NAME</code></td><td>poller</td><td>No</td><td><code>mainnet</code></td><td>Chain name the Firehose advertises (<code>--advertise-chain-name</code>).</td></tr>
<tr><td><code>SUBSTREAMS_ENDPOINT</code></td><td>indexer</td><td>Yes</td><td><code>https://mainnet.eth.streamingfast.io:443</code></td><td>Substreams tier1 gRPC. Self-hosted: <code>http://substreams-endpoint:10016</code>.</td></tr>
<tr><td><code>CHAINS</code></td><td>indexer</td><td>No</td><td><code>ethereum</code></td><td>Comma-separated chains to index; filters which extractors run. Custom-chain workaround until ENG-5738 (see below).</td></tr>
<tr><td><code>RETENTION_HORIZON</code></td><td>indexer</td><td>No</td><td><code>2000-01-01T00:00:00</code></td><td>Earliest block data the indexer retains.</td></tr>
<tr><td><code>TYCHO_IMAGE</code></td><td>indexer</td><td>Yes</td><td>—</td><td>tycho-indexer image tag.</td></tr>
<tr><td><code>EXTRACTORS_CONFIG</code></td><td>indexer</td><td>No</td><td><code>/opt/tycho-indexer/extractors.yaml</code></td><td>Path to the extractors config inside the container.</td></tr>
<tr><td><code>TYCHO_PROTOCOL_SDK_PATH</code></td><td>indexer</td><td>No</td><td><code>../tycho-protocol-sdk</code></td><td>Host tycho-protocol-sdk checkout, mounted read-only.</td></tr>
<tr><td><code>SUBSTREAMS_API_TOKEN</code></td><td>indexer</td><td>No</td><td><code>readme</code></td><td>Auth token for a hosted Substreams endpoint; unused self-hosted.</td></tr>
<tr><td><code>TRACE_RPC_URL</code></td><td>indexer</td><td>For DCI</td><td>—</td><td>Trace-capable RPC for dynamic contract indexing.</td></tr>
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
START_BLOCK=24542000

# Self-hosted Firehose, reachable by its docker service name
SUBSTREAMS_ENDPOINT=http://substreams-endpoint:10016

# Custom-chain workaround: reuse a known chain's extractors (see below)
CHAINS=unichain

RETENTION_HORIZON=2000-01-01T00:00:00
SUBSTREAMS_API_TOKEN=local
AUTH_API_KEY=local-dev-key
RUST_LOG=info
OTLP_EXPORTER_ENDPOINT=
```

### Writing your extractors.yaml

The indexer loads its extractors from `crates/tycho-indexer/extractors.yaml`, mounted into the container at `/opt/tycho-indexer/extractors.yaml`. Use that file as your template. Each entry under `extractors:` configures one protocol:

```yaml
extractors:
  uniswap_v3:
    name: "uniswap_v3"
    chain: "ethereum"
    implementation_type: "Custom"
    sync_batch_size: 1000
    start_block: 12369621
    protocol_types:
      - name: "uniswap_v3_pool"
        financial_type: "Swap"
    spkg: "substreams/ethereum-uniswap-v3/ethereum-uniswap-v3-logs-only-0.1.1.spkg"
    module_name: "map_protocol_changes"
```

<table><thead><tr><th width="220">Field</th><th>Purpose</th></tr></thead><tbody>
<tr><td><code>name</code></td><td>Unique extractor name; also the protocol system name exposed over the RPC.</td></tr>
<tr><td><code>chain</code></td><td>Chain this extractor runs on. Must match an entry in <code>CHAINS</code> for the extractor to start.</td></tr>
<tr><td><code>implementation_type</code></td><td><code>Custom</code> for native substreams that emit Tycho protocol messages directly, or <code>Vm</code> for protocols simulated through the tycho-protocol-sdk.</td></tr>
<tr><td><code>sync_batch_size</code></td><td>Number of blocks the indexer requests per Substreams batch.</td></tr>
<tr><td><code>start_block</code></td><td>Block at which the protocol was deployed; the indexer starts streaming here.</td></tr>
<tr><td><code>spkg</code></td><td>Path to the compiled <code>.spkg</code>, relative to <code>/opt/tycho-indexer/</code> (i.e. under <code>docker/substreams/</code>).</td></tr>
<tr><td><code>module_name</code></td><td>Substreams output module to consume (e.g. <code>map_protocol_changes</code>).</td></tr>
<tr><td><code>protocol_types</code></td><td>Protocol component types this extractor produces, each with a <code>name</code> and a <code>financial_type</code>.</td></tr>
</tbody></table>

{% hint style="info" %}
A top-level `chains:` section that lets you declare a custom chain directly in `extractors.yaml` is pending ENG-5738. Until it ships, point `CHAINS` at a known chain whose extractors you want to reuse — for example `CHAINS=unichain` runs the Unichain extractors against the Tempo RPC.
{% endhint %}

### Pointing to a local tycho-protocol-sdk checkout

`Vm` extractors run protocol logic from the tycho-protocol-sdk. The compose file mounts a host checkout into the container read-only:

```
${TYCHO_PROTOCOL_SDK_PATH:-../tycho-protocol-sdk}/substreams → /opt/tycho-indexer/substreams-sdk:ro
```

Set `TYCHO_PROTOCOL_SDK_PATH` in `docker/.env` to your checkout if it lives somewhere other than `../tycho-protocol-sdk`. If you index only `Custom` extractors, leave the default — the mount stays unused.

## Running the stack

Start everything — Postgres, the self-hosted Firehose, and the indexer — with the `substreams-endpoint` profile:

```bash
cd docker
docker compose --profile substreams-endpoint up
```

On a cold start the poller begins at `START_BLOCK` and streams forward. The indexer only commits a block once it sits behind the finality horizon, so expect a delay before committed state appears — on a fresh chain the first cold start takes a while to reach the deployment block of your protocols.

### Resuming after a restart

Both halves of the stack resume on their own:

- The poller auto-detects the highest stored one-block file under `/data/storage/one-blocks/` and resumes from there, so it skips already-fetched blocks.
- The indexer resumes from the cursor stored in its Postgres database.

`docker compose --profile substreams-endpoint down` followed by `up` **without `-v`** preserves both the Firehose data volume and the database, so the stack picks up where it left off. Passing `-v` deletes the volumes and forces a full cold start.

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

- **Blocks look years old.** If the poller's block `age` field reads years instead of seconds, `RPC_URL` points at the wrong chain or a stale node. Confirm the endpoint serves your chain and is fully synced.
- **Firehose never becomes healthy.** A hub bootstrap deadlock leaves the stack waiting indefinitely. Stop the stack and bring it back up; on restart the poller resumes from stored one-block files instead of bootstrapping from scratch.
- **Corrupt Substreams state cache.** If tier2 fails to produce segments after an unclean shutdown, clear the cached state and restart: remove `/data/substreams-states/` inside the `firehose-data` volume, then `up` again.
- **Cold start takes a long time.** A fresh chain must fetch and merge every block from `START_BLOCK` before your protocols' deployment blocks appear. This is expected — set `START_BLOCK` close to your earliest protocol `start_block` to avoid fetching irrelevant history.

## Performance tuning

- **`--interval-between-fetch`** (poller) — delay between RPC fetches. The compose file sets `0ms` (no delay) for maximum throughput. Raise it to throttle a rate-limited RPC.
- **RPC latency** — the poller fetches blocks sequentially, so round-trip latency directly bounds the fetch rate. A nearby, low-latency node is the single biggest lever on cold-start speed.
- **Chain block time vs fetch rate** — once caught up, the poller only needs to keep pace with the chain's block time. Cold start is fetch-bound; steady state is block-time-bound.
- **`--substreams-state-bundle-size`** (tier1/tier2) — number of blocks per state segment, `1000` by default. Larger bundles cut per-segment overhead at the cost of coarser caching granularity.
