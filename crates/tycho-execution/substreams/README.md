# TychoRouter trades substreams

Substreams package that extracts every trade routed through the deployed TychoRouter contracts
and sinks it into Postgres with [`substreams-sink-sql`](https://github.com/streamingfast/substreams-sink-sql).
It is a standalone WASM workspace (like `protocols/substreams/`) and is excluded from the root
Cargo workspace.

## What it extracts

The router emits no swap event, so trades are recovered from **EVM call traces**: every call
(top-level or internal, successful or reverted) whose target is a configured router and whose
selector is one of the swap entry points. This requires Extended (Firehose-instrumented) blocks.

Per trade (`trades` table): router + ABI generation, strategy (`single`/`sequential`/`split`),
funding mode (`transfer_from`/`permit2`/`vault`/`none`), EOA (`tx.from`), `msg.sender`, receiver,
token in/out, amount in, expected/min amount out and the implied slippage tolerance, settled
amount out (decoded from the call return value), gross amount out and positive slippage,
`ClientFeeParams`, the router fee configuration in effect, fees actually taken (`FeesTaken`),
executors and protocol systems per hop, splits, watermark (trailing calldata), revert selector,
decoded reason, and gas used.

Related tables: `trade_hops` (one row per executor hop), `fees_taken` (one row per fee
recipient), `fee_config_events` (FeeCalculator admin events and fee-calculator rotations), and
`router_call_errors` (selector-matched calls that could not be decoded safely).

### Router generations

| Version | ABI | Deployments |
|---|---|---|
| `v2` | `singleSwap(amountIn, tokenIn, tokenOut, minAmountOut, wrapEth, unwrapEth, receiver, transferFromNeeded, swap)` (+ `Permit2`, `sequentialSwap`, `splitSwap`) | ethereum, base, unichain (2025) |
| `v3_0` | `ClientFeeParams` (`uint16` bps), `minAmountOut` only, `UsingVault` variants | all chains, Mar–Jul 2026 |
| `v3_1` | adds `expectedAmountOut`; `ClientFeeParams` uses `uint32` fee units (`MAX_BPS = 100_000_000`) | all chains, from Jul 31 2026 |

Addresses and the FeeCalculator each router was constructed with live in the per-chain
manifests under `tycho-router-trades/chains/`. The FeeCalculator constructor emits no event, so
the initial pairing is a parameter; rotations (`FeeCalculatorActivated` / `FeeCalculatorUpdated`)
and all fee-rate changes are replayed from events into `store_fee_config`.

### Volume in USD

Pricing is a post-ingestion step, not part of the substreams. Tycho prices every token in the
**native token of its chain**, so nothing is comparable across chains until it passes through a
USD anchor. On bsc that native token is BNB and on polygon POL, which is why an ETH-denominated
column was not merely mislabelled there but wrong.

`pricing/preferred_tokens.sql` pins a short list of liquid tokens per chain, by address:

* the rows marked `is_stable` carry the anchor. `price / 10^decimals` of a stablecoin is the USD
  price of the native token, and the median over several stables survives one thin or stale row.
* any pinned row makes its side of a trade usable for pricing.

`pricing/price_trades.sql` values a trade from one side only, preferring a side that holds a
pinned token (lowest `priority` first: stablecoins, then the native token and its wrapper, then
BTC wrappers) and otherwise any side Tycho happens to price. The unit price of the token on the
other side is then implied from the trade itself. That is what gives the long tail a price at all:
Tycho prices 7852 tokens on ethereum but only 29 on unichain and 126 on arbitrum, while nearly
every trade has a stablecoin, WETH or the native token on one side.

Pinning is by address, never by symbol. Tycho holds many tokens with a copied symbol — base has
about 40 `cbBTC` rows and ethereum a second 18-decimal `USDC` — and their prices are nonsense.
`min_usd`/`max_usd` bound the resulting unit price so a pinned row that has drifted is dropped
instead of trusted, and `decimals` is kept in the list rather than read from Tycho, whose token
rows carry 18 for anything it has not analysed.

Age is the reason `price_source` exists. Tycho keeps one current price per token, not a history.
A stablecoin is worth 1 USD whenever the trade happened, so a stable-anchored trade is valued at
any age and a backfill can be priced correctly. Every other basis would stamp today's price on an
old trade, so those are limited to trades younger than `MAX_AGE`. `price_source` records the basis
as `<in|out>_<stable|preferred|tycho>`; filter on it before summing `volume_usd`.

The trades live in their own database (all chains in one table). Each chain has its own Tycho
database, reached through `postgres_fdw`: one server and one `tycho_<chain>` schema per chain,
priced in independent passes. An unavailable source pauses only its chain; other chains continue.

```bash
for chain in ethereum base ...; do                              # once per chain
  psql "$DSN" -v chain=$chain -v tycho_host=... -v tycho_port=5432 -v tycho_db=... \
       -v tycho_user=... -v tycho_password=... -f pricing/tycho_foreign_tables.sql
done
make price-setup                # migration + preferred-token list
make price-once CHAIN=ethereum  # one chain; MAX_AGE='2 hours' widens the window
make price-loop                 # every chain, independently, every 60 seconds
```

The `price` container does `price-setup` itself on every start, so the list in the repository is
the list in the database.

For the local docker database, `psql "$DSN" -v chain=ethereum -f pricing/dev_stub.sql` creates
stand-in tables in `tycho_ethereum` instead of the foreign tables.

## Container

`Dockerfile` (build from the repository root) ships `substreams-sink-sql` with `psql`, the AWS CLI
and the SQL. It builds no wasm and carries no `.spkg`: `release.sh` publishes those separately and
each container fetches the one it is pinned to, so rebuilding the image cannot change what a sink
reads. The image has two modes:

```bash
docker build -f crates/tycho-execution/substreams/Dockerfile -t tycho-router-trades .
# one container per chain, all writing to the same database
docker run -e CHAIN=ethereum -e SPKG=substreams/tycho-router-trades/ethereum-v0.1.0.spkg \
  -e DSN='psql://...' -e SUBSTREAMS_API_TOKEN=... tycho-router-trades sink
# one container running the pricing loop
docker run -e DSN='postgres://...' tycho-router-trades price
```

`SPKG` is a local path when that file exists and otherwise a key in the release bucket
(`TYCHO_S3_BUCKET`, default `repo.propellerheads-propellerheads`), cached under `SPKG_CACHE_DIR`.
That is the same rule the indexer uses for its own packages, so a pinned release and a
bind-mounted local build both work.

Both modes wait until the database accepts connections. `sink` runs `substreams-sink-sql setup`
(idempotent) and then `run` with `--batch-block-flush-interval 100`; optional
`SUBSTREAMS_ENDPOINT`, `START_BLOCK`, `STOP_BLOCK`, `FLUSH_INTERVAL`, `METRICS_ADDR` (Prometheus,
default `:9102`). `price` registers one `postgres_fdw` server per `TYCHO_<CHAIN>_DATABASE_URL`
variable (`scripts/fdw_setup.sh`, re-run on every start). It runs `price_trades.sql` separately
for each chain every `INTERVAL` seconds (default 60,
`MAX_AGE` default `1 hour`). A failed chain is logged without blocking the others.
`docker-compose.yaml` wires the database, the eight sinks and the pricer for a local run: run
`make pack-all` first, which writes the packages to `target/spkg/` where the compose file mounts
them (`docker compose --profile sinks up`).

### Kubernetes

`main-workflow.yaml` builds the image as `tycho-router-trades:<release version>` on every
monorepo release and promotes the tag to `helmwave/dev/versions.yml` in `helm-configuration`. The
image only carries the sink and the SQL, so that promotion is routine: what each chain indexes is
decided by the package pinned under `spkgs`, released separately. The
release `router-trades-db` there (namespace `dev-tycho`) is a Postgres 16 StatefulSet; release
`router-trades` is one pod with a `sink` container per chain and one `price` container, all
writing to `router-trades-db:5432`. Grafana reaches the database through the same service with
the read-only `grafana` role.

## Updating a deployed sink

Two properties of `substreams-sink-sql` decide what an update costs, and both bite silently.

**The cursor is keyed by the output module hash.** That hash covers every module definition —
including `initialBlock` and `params` — and the compiled wasm. The sinks run with the default
`--on-module-hash-mismatch=error`, so when the hash changes the container exits at startup with a
mismatch against the row in `cursors_<chain>`, and it keeps crashlooping until that row is
deleted.

Nothing changes that hash by accident, because it belongs to a released `.spkg` rather than to the
image. A chain's hash moves only when someone pins a different package for that chain under
`spkgs` in `helmwave/<env>/values/tycho/router-trades/router-trades.yml`, and it moves for that
chain alone. Rebuilding or promoting the image — from a change here or from an unrelated monorepo
release — changes nothing. A wasm build is in any case not byte-identical across environments, so
a hash is only ever known from the package that CI published.

**The sink does not upsert.** `db_out` uses `create_row`, which is `OPERATION_CREATE`, and the
postgres dialect turns that into a plain `INSERT` with no `ON CONFLICT`. Any block the sink reads
a second time fails on the `TEXT PRIMARY KEY` and takes the container down. So whenever a chain
restarts below its previous cursor, delete that chain's rows in the range that will be re-read —
in practice, all of them.

### Procedure

1. Merge the change, release the packages (see "Releasing a package") and pin the new keys for
   the affected chains under `spkgs` in
   `helmwave/<env>/values/tycho/router-trades/router-trades.yml`. The dev deployment rolls the pod.
2. Chains whose package changed exit on the mismatch; the others resume from their cursors. This
   is expected, and it is contained: the sinks have no readiness probe and Postgres is a separate
   release.
3. Clear the state of each affected chain, now that its sink is down and not flushing:

   ```sql
   DELETE FROM cursors_<chain>;
   DELETE FROM substreams_history_<chain>;
   -- only when the chain will re-read blocks it has already written
   DELETE FROM trades             WHERE chain = '<chain>';
   DELETE FROM trade_hops         WHERE chain = '<chain>';
   DELETE FROM fees_taken         WHERE chain = '<chain>';
   DELETE FROM fee_config_events  WHERE chain = '<chain>';
   DELETE FROM router_call_errors WHERE chain = '<chain>';
   ```

4. The container recovers on its next backoff restart. Confirm the new range in its log:
   `restarting_at: None` together with the expected `resolved_start_block`.

Never reach for `--on-module-hash-mismatch=ignore` to skip step 3: it resumes from the highest
cursor in the table, which is block 0 for a chain that has never produced data.

`cursors_<chain>` holds one bookmark row and `substreams_history_<chain>` is the reorg-undo
journal. Neither holds trade data, so clearing them loses no trade.

## Releasing a package

`release.sh` builds the wasm once and packs one `.spkg` per chain to
`s3://repo.propellerheads-propellerheads/substreams/tycho-router-trades/<chain>-<version>.spkg`.
It mirrors `protocols/substreams/release.sh`, including its rules:

* the version comes from `tycho-router-trades/Cargo.toml`, so bump it in the PR that changes the
  package;
* a release needs a `tycho-router-trades-<semver>` tag on HEAD whose version matches, and a clean
  tree. **Releases are immutable** — S3 rejects a key that already exists, so a changed package
  takes a new version and never an overwrite;
* without such a tag the run publishes `pre.<sha>`, which may be overwritten and is meant for
  testing.

Run it from CI with the **Release Router Trades Substreams** workflow (`workflow_dispatch`, with
an optional space-separated `chains` input). It prints the `db_out` module hash of every package.
That hash is the identity the cursors are keyed by, so compare it with the running
`cursors_<chain>.id` before pinning, to know whether a chain will resume or restart.

Then pin the published keys per chain in helm:

```yaml
spkgs:
  ethereum: substreams/tycho-router-trades/ethereum-v0.2.0.spkg
  bsc:      substreams/tycho-router-trades/bsc-v0.3.0.spkg
```

Chains are independent: each runs the version it is pinned to, and a rollback is a pin back to the
previous key.

## Adding a chain

Six places, and the deployment fails quietly if any of the last three is missed.

1. **Collect the routers.** Every TychoRouter deployed on the chain, the ABI generation of each
   (`v2`, `v3_0`, `v3_1`), and the FeeCalculator each router was constructed with. The constructor
   emits no event, so that pairing is a manifest parameter; later rotations are replayed from
   `FeeCalculatorActivated` / `FeeCalculatorUpdated`.

2. **Find the deployment block of the earliest router**, with a binary search over `eth_getCode`
   against the chain's node. This is `initialBlock`, and it goes on all four modules. Everything in
   the graph, `store_fee_config` included, is built from there before the first row appears, so a
   round number below the real block is pure cost — bsc started at 50,000,000 against a first
   router at 98,458,505 and produced nothing for hours. A `START_BLOCK` on the container is not a
   substitute: an existing cursor overrides the requested start block, and the store still builds
   from its own `initialBlock`.

3. **Write the manifest**, `tycho-router-trades/chains/<chain>.yaml`, copied from an existing one.
   Set `network:` to the Substreams network name, the four `initialBlock` values, and the `params:`
   of `map_fee_config_events` and `map_trades`:

   ```
   chain=<chain>&routers=<router>:<generation>,...&fee_calculators=<router>:<calculator>,...
   ```

   The chain must serve Extended (Firehose) blocks. Trades are recovered from call traces, so a
   chain without them yields nothing at all rather than an error.

4. **Pin the pricing tokens** in `pricing/preferred_tokens.sql`: the native sentinel row for the
   chain with its native symbol and price band, at least one stablecoin, and the majors. Without a
   pinned stablecoin the chain has no USD anchor and every trade stays unpriced. Verify each
   address before trusting it — `price / 10^decimals` of each pinned stablecoin should agree with
   the others, and that agreed value is the USD price of the native token. Never pick a token by
   symbol: Tycho holds about 40 tokens called `cbBTC` on base alone.

5. **Regenerate the lookup tables** with `scripts/gen_tables.py`, so `src/executors_table.rs`
   covers the new chain's executors. Without it a hop carries the executor address but no protocol
   name.

6. **Wire the deployment.** Add a `sink-<chain>` service to `docker-compose.yaml` for local runs.
   Release the chain's package (see "Releasing a package"). In `helm-configuration`, add the chain
   to the `$chains` list of `helmwave/dev/values/tycho/router-trades/router-trades.yml`, which
   gives it a sink container, a metrics port and a scrape entry, pin its package under `spkgs`,
   and add a `TYCHO_<CHAIN>_DATABASE_URL` entry pointing at that chain's Tycho database so the
   pricer can reach its prices.

Nothing has to be cleared in the database: a chain with no cursor row starts at its
`initialBlock`.

## Module graph

```
map_fee_config_events ─▶ store_fee_config ─▶ map_trades ─▶ db_out (DatabaseChanges)
        └──────────────────────────────────────────────────▶
```

## Running locally

```bash
rustup target add wasm32-unknown-unknown
brew install streamingfast/tap/substreams            # or see substreams.dev
# substreams-sink-sql: download the release binary for your platform from
# https://github.com/streamingfast/substreams-sink-sql/releases (tested with v4.13.1)
export SUBSTREAMS_API_TOKEN=$(curl -s https://auth.streamingfast.io/v1/auth/issue \
  -d "{\"api_key\":\"$STREAMINGFAST_KEY\"}" | jq -r .token)

docker compose up -d
make setup CHAIN=ethereum                              # applies schema.sql (idempotent)
make run   CHAIN=ethereum START=25654569 STOP=+50      # print decoded trades
make sink  CHAIN=ethereum                              # stream into Postgres
```

Run one `make sink` per chain against the same database. The `chain` column is part of every
business-table primary key. Deployed sinks use per-chain cursor and history tables so their block
positions and reorg bookkeeping remain independent.

`make sink` passes `--batch-block-flush-interval 100`: with the default (1000) the sink
v4.13.1 did not flush the trailing partial batch when a bounded run reached its stop block.
Live mode flushes every block regardless.

Backfilling from the router deployment blocks needs `substreams-sink-sql`'s production mode
(the `substreams run` CLI caps ad-hoc requests at 10k blocks because of the fee store).

Endpoints are resolved from the manifest `network` field; override with
`-e <host>:443` or `SUBSTREAMS_ENDPOINTS_CONFIG_<NETWORK>`.

## Development

```bash
make test    # unit tests (decoding fixtures from contracts/test/assets/calldata.txt)
make lint    # fmt + clippy
make build   # wasm
make test-pricing  # disposable two-chain PostgreSQL/FDW integration test
./scripts/test_pricing.sh --outage-check  # verifies one failed source does not block another
```

Lookup tables (`src/executors_table.rs`, `src/decode/error_table.rs`) are generated by
`scripts/gen_tables.py` from the docs/config git history and the ABIs under `abi/`; re-run it
after new executor deployments. ABIs come from the verified sources on Sourcify
(`TychoRouterV2` = `0xfD0b31d2…`, `TychoRouterV3_0` = `0x1f8dB310…`, `TychoRouterV3_1` =
`0xea290cE3…`, `FeeCalculator` = `0xA236E1F0…`); `FeeCalculatorV3_0.json` holds the `uint16`
event signatures of the earlier FeeCalculator.

Regenerate protobuf bindings after editing `proto/` with `make protogen`. The abigen output under
`src/abi/` is not committed; `build.rs` regenerates it from the minified JSON ABIs on every build.
