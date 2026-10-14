# Startup load benchmark

Replays realistic client startups against one Tycho RPC host and records what the pod, its
database and the clients see. Used to compare indexer releases on the idle (blue/green) prod
pods before switching traffic to them.

## Pieces

| Path | What |
|---|---|
| `loadgen/` | `startup-loadgen`: a standalone Rust binary that runs `n` client startups (tokens, components pages, entry points, protocol state, contract state) against one host, each with a client profile from the plan, and records every request. |
| `harness/plan2.py` | Builds the plan for a chain: client profiles (TVL threshold, protocol set, page and chunk sizes) with the component and contract ids they would fetch, kept inside the prod plan rules (`tvl_gt >= 0.1`, versions at most 10 minutes old). |
| `harness/wave2.py` | Runs scenario sets: `realistic` (mixed profiles, 1 to 64 startups, at once and staggered) and `worst` (broadest profile at the TVL floor, oldest allowed versions, crash loops, single-id pollers). Collects pod CPU/memory/restarts, RDS CPU, health-probe latency, `db_path_requests` and cache counters around every wave. |
| `harness/run_all.sh` | Plan, realistic set, worst set, per chain. The runner pod's main process. |
| `k8s/`, `launch.sh`, `harness/pull.py` | In-cluster run against the idle prod `-1` pods through their own Services, with `X-User-Plan: default` so the indexer applies the prod plan rules; results on a persistent volume, pulled through a port-forward. |

## Run it in the cluster

```sh
TYCHO_API_KEY=<any valid key> GRAFANA_API_KEY=<read-only Grafana key> ./launch.sh
kubectl -n prod-tycho-bench logs -f tycho-bench
```

`launch.sh` creates the namespace `prod-tycho-bench`, a secret with the two keys, a 20 GiB
`gp3` volume and the pod; then copies this directory in and starts the run. The pod builds
`startup-loadgen` itself (`rust:1.91-bookworm`), so nothing is compiled on the laptop. The run
takes a few hours for two chains and prints one JSON summary line per wave.

Plans and results live on the volume under `load/`, so they survive a pod death. The pod serves
that directory on port 8000; pull it from anywhere with get and port-forward rights on pods
(the read-only debug role is enough), during the run or after it:

```sh
python3 harness/pull.py ./results          # every 2 minutes until interrupted; --once for a single pass
```

If the pod dies, delete it and run `./launch.sh` again: existing plans are reused and finished
waves are skipped. When everything is pulled:

```sh
kubectl delete namespace prod-tycho-bench      # removes the pod, the secret and the volume
```

Chains and sets come from `BENCH_CHAINS` and `BENCH_SETS` in the pod manifest. The pod needs
to reach `<chain>-tycho-indexer-1.prod-tycho.svc:8080` (the `-1` deployment's own Service) and
Grafana.

## Run it against dev from anywhere

```sh
cargo build --release --manifest-path loadgen/Cargo.toml
cd harness
BENCH_SIDE=dev python3 plan2.py bsc            # needs the `zstandard` module for pages > 500
BENCH_SIDE=dev python3 wave2.py bsc smoke      # or realistic | worst | all
```

Results land in `harness/load/waves2/<chain>_<tag>_<scenario>.json`.
