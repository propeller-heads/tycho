# Startup load benchmark

Replays realistic client startups against one Tycho RPC host and records what the pod, its
database and the clients see. Used to compare indexer releases on the idle (blue/green) prod
pods before switching traffic to them.

One config file is the whole run. `bench/<target>.toml` names the target (URL, plan header,
which pod to watch, its RDS instance), the plan rules the target applies, the client profiles
(TVL threshold, protocol set, page and chunk sizes, with a comment on which kind of client each
one stands for) and the scenarios, in order. The command is always the same; only the config
changes. Changing a run means editing a config file in a PR.

## Run it in the cluster

Against the idle prod pods the harness runs as a pod, so the numbers do not depend on a laptop's
uplink. Two objects are created in `prod-tycho`: a ConfigMap with this directory's source and
config files, and the pod. Keys come from the environment or a `.env` next to this README
(see `.env.example`).

```sh
./launch.sh bench/ethereum-1.toml
kubectl -n prod-tycho logs -f tycho-bench                 # the resolved run, then one line per wave
python3 harness/bench.py pull ./results                   # copies results out every 2 minutes
kubectl -n prod-tycho delete pod/tycho-bench configmap/tycho-bench
```

The pod builds `startup-loadgen` itself (`rust:1.91-bookworm`), runs every scenario of the
config, and then serves its results directory on a port. `bench.py pull` reaches it through a
port-forward, which needs only get and port-forward rights on pods. If the pod dies, launch it
again: the plan is rebuilt (about ten minutes) and finished waves are skipped.

The targets `ethereum-1` and `robinhood-1` are reached through the `-1` deployments' own
Services with `X-User-Plan: default`, so the indexer applies the prod plan rules exactly as for
real clients. There is no auth-proxy on that path.

## Run it from anywhere

```sh
cd harness
python3 bench.py show ../bench/dev-bsc.toml                       # what would run
python3 bench.py run  ../bench/dev-bsc.toml --only mix_n1,mix_n8  # a subset; --sets realistic for a whole set
```

`run` builds the binary if needed, prints the resolved run and asks for confirmation (`--yes`
skips it). Python 3.11+; the `zstandard` module lets the plan builder use pages of 2,550, without
it the plan takes longer to build but is the same.

## What a run produces

`results/<target>/`:

- `run.json`: the config path and hash, the resolved scenarios, the target's image tag, per
  profile the number of protocols, components, contracts and requests one startup makes, and
  the git commit of the harness.
- `plan.json`: the component and contract ids every profile fetches, as found on the target that day.
- `waves/<scenario>.json`: the summary (ready time p50/max per profile, p95 per endpoint, failed
  requests, bytes, pod CPU and memory peaks, restarts, lag, RDS CPU peak, health-probe max,
  `db_path_requests` and cache deltas), the metric series, the probe samples, the startups and
  every request record.

## Pieces

| Path | What |
|---|---|
| `bench/*.toml` | One config per target. |
| `harness/bench.py` | `show`, `plan`, `run`, `pull`. |
| `harness/config.py`, `tycho.py`, `metrics.py`, `plan.py`, `waves.py` | Config loading, API calls, Grafana queries, plan building, one scenario. |
| `loadgen/` | `startup-loadgen`: a standalone Rust binary that runs `n` client startups (tokens, components pages, entry points, protocol state, contract state) against one host and records every request. |
| `k8s/pod.yaml`, `launch.sh` | The runner pod and the two kubectl commands that start it. |
