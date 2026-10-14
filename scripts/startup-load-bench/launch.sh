#!/bin/bash
# Runs one config in the cluster:   ./launch.sh bench/ethereum-1.toml [bench.py run arguments]
# e.g. a subset:                     ./launch.sh bench/ethereum-1.toml --only old_version_n16
# Needs TYCHO_API_KEY and GRAFANA_API_KEY (environment or ./.env) and a kubectl context on the
# prod cluster. Creates two objects in prod-tycho: the ConfigMap "tycho-bench" with this
# directory's source and config files, and the pod "tycho-bench".
#   follow:   kubectl -n prod-tycho logs -f tycho-bench
#   results:  python3 harness/bench.py pull ./results        (repeats every 2 minutes)
#   remove:   kubectl -n prod-tycho delete pod/tycho-bench configmap/tycho-bench
set -euo pipefail
cd "$(dirname "$0")"
CONFIG="${1:?usage: launch.sh bench/<target>.toml [bench.py run arguments]}"
shift
export BENCH_ARGS="$*"
NS="${NAMESPACE:-prod-tycho}"
[ -f "$CONFIG" ] || { echo "no such config: $CONFIG"; exit 1; }
if [ -f .env ]; then set -a; . ./.env; set +a; fi
: "${TYCHO_API_KEY:?set TYCHO_API_KEY (environment or .env)}" "${GRAFANA_API_KEY:?set GRAFANA_API_KEY (environment or .env)}"

files=()
for f in harness/*.py bench/*.toml loadgen/Cargo.toml loadgen/Cargo.lock loadgen/src/main.rs; do files+=("--from-file=$f"); done
kubectl -n "$NS" create configmap tycho-bench "${files[@]}" --dry-run=client -o yaml | kubectl apply -f -
export BENCH_CONFIG="$(basename "$CONFIG")"
python3 -c 'import os, sys; print(os.path.expandvars(sys.stdin.read()))' < k8s/pod.yaml | kubectl -n "$NS" apply -f -
echo "started $BENCH_CONFIG $BENCH_ARGS: kubectl -n $NS logs -f tycho-bench"
