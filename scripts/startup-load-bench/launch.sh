#!/bin/bash
# Starts the benchmark in the cluster against the idle prod -1 indexers.
#   TYCHO_API_KEY=... GRAFANA_API_KEY=... ./launch.sh
# Both keys may also come from ~/.env. Needs a kubectl context that can create namespaces,
# secrets, volumes and pods.
# Follow the run:      kubectl -n prod-tycho-bench logs -f tycho-bench
# Pull the results:    python3 harness/pull.py <local dir>      (works with read-only access)
# Resume after a pod death:  delete the pod, run ./launch.sh again; finished waves are skipped.
# Remove everything:   kubectl delete namespace prod-tycho-bench   (pull first: the volume goes too)
set -euo pipefail
cd "$(dirname "$0")"
NS=prod-tycho-bench
val() {
  if [ -n "${!1:-}" ]; then printf '%s' "${!1}"; return; fi
  [ -f ~/.env ] && grep -E "^\s*(export\s+)?$1=" ~/.env | head -1 | sed -E "s/^[^=]*=//; s/^[\"']//; s/[\"']\$//" || true
}
TYCHO="$(val TYCHO_API_KEY)"; GRAF="$(val GRAFANA_API_KEY)"
[ -n "$TYCHO" ] && [ -n "$GRAF" ] || { echo "set TYCHO_API_KEY and GRAFANA_API_KEY (env or ~/.env)"; exit 1; }

tar czf bundle.tgz --exclude=./loadgen/target --exclude=./harness/load --exclude='__pycache__' --exclude=./bundle.tgz .
kubectl get namespace $NS >/dev/null 2>&1 || kubectl create namespace $NS
kubectl -n $NS create secret generic bench-keys --from-literal=TYCHO_API_KEY="$TYCHO" --from-literal=GRAFANA_API_KEY="$GRAF" \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -f k8s/pvc.yaml
kubectl apply -f k8s/runner-pod.yaml
kubectl -n $NS wait --for=condition=Ready pod/tycho-bench --timeout=600s
kubectl cp bundle.tgz $NS/tycho-bench:/bench/bundle.tgz
kubectl -n $NS exec tycho-bench -- touch /bench/go
rm bundle.tgz
echo "started: kubectl -n $NS logs -f tycho-bench"
