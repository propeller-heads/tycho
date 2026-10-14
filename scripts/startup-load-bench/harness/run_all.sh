#!/bin/sh
# Runs the whole benchmark: plan, realistic set, worst-case set, per chain. Used as the main
# process of the runner pod (see ../k8s/runner-pod.yaml), but works anywhere with python3, cargo,
# TYCHO_API_KEY and GRAFANA_API_KEY in the environment.
# One JSON summary line per wave goes to stdout. The result files land in load/ and are served
# on port 8000 (plain http.server) so pull.py can copy them out through a port-forward while the
# run is still going. Re-running continues where it stopped: existing plans and results are kept.
set -u
cd "$(dirname "$0")"
export BENCH_SIDE="${BENCH_SIDE:-bench}"
CHAINS="${BENCH_CHAINS-ethereum robinhood}"     # unset = both; empty = none
SETS="${BENCH_SETS-realistic worst}"
echo "BENCH start $(date -u +%FT%TZ) side=$BENCH_SIDE chains=[$CHAINS] sets=[$SETS]"
mkdir -p load
python3 -m http.server 8000 --bind 0.0.0.0 --directory load >/dev/null 2>&1 &
if [ ! -x ../loadgen/target/release/startup-loadgen ] && [ -z "${LOADGEN_BIN:-}" ]; then
  echo "BENCH building startup-loadgen"
  (cd ../loadgen && cargo build --release 2>&1 | tail -3) || { echo "BENCH build FAILED"; exit 1; }
fi
python3 -c "import zstandard" 2>/dev/null || echo "BENCH no python zstandard module: plan build uses pages of 500"
for chain in $CHAINS; do
  echo "BENCH $chain target: $(python3 -c "import chain as c; print(c.host('$chain', c.SIDE))") plan header: $(python3 -c "import chain as c; print(c.auth_headers().get('X-User-Plan', 'none (auth-proxy sets it)'))")"
  echo "BENCH $chain health: $(python3 -c "import urllib.request,chain as c; r=urllib.request.urlopen(urllib.request.Request(c.host('$chain', c.SIDE)+'/v1/health', headers=c.auth_headers()), timeout=30); print(r.status)" 2>&1 | tail -1)"
  echo "BENCH $chain plan $(date -u +%FT%TZ)"
  [ -s "load/plan2_$chain.json" ] || python3 plan2.py "$chain" || { echo "BENCH $chain plan FAILED"; continue; }
  for set in $SETS; do
    echo "BENCH $chain $set $(date -u +%FT%TZ)"
    python3 wave2.py "$chain" "$set" "$BENCH_SIDE"
  done
done
echo "BENCH done $(date -u +%FT%TZ)"
if [ "${BENCH_KEEP_ALIVE:-1}" = "1" ]; then
  # Keep the pod (and the file server) around until the results are pulled.
  sleep infinity
fi
