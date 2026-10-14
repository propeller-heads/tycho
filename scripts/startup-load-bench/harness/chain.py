#!/usr/bin/env python3
"""Shared helpers for the startup load benchmark: API keys, hosts, metrics queries, block tips."""
import json, os, re, sys, urllib.parse, urllib.request

S = os.path.dirname(os.path.abspath(__file__))   # this directory
L = S + "/load"                                   # plans and wave results
LOADGEN = os.environ.get("LOADGEN_BIN", os.path.normpath(S + "/../loadgen/target/release/startup-loadgen"))
GRAFANA = os.environ.get("GRAFANA_URL", "https://grafana.propellerheads.xyz")
VM_UID = os.environ.get("GRAFANA_VM_UID", "P4169E866C3094E38")          # VictoriaMetrics datasource (tycho metrics)
# dev = public dev hosts (harness runs from the VPS); bench = the idle prod -1 pod's own Service,
# reached from a pod in the cluster. There is no auth-proxy on that path, so the harness sets the
# plan header itself; the indexer applies the plan's restrictions from it (tvl floor, version age).
SIDE = os.environ.get("BENCH_SIDE", "dev")
USER_PLAN = os.environ.get("USER_PLAN", "default")


def auth_headers():
    h = {"Authorization": env("TYCHO_API_KEY")}
    if SIDE == "bench":
        h["X-User-Plan"] = USER_PLAN
    return h


def env(name):
    """An environment variable, or its line in ~/.env."""
    if os.environ.get(name):
        return os.environ[name]
    if not os.path.exists(os.path.expanduser("~/.env")):
        return None
    for line in open(os.path.expanduser("~/.env")):
        m = re.match(rf"\s*(?:export\s+)?{name}=(.*)", line)
        if m:
            return m.group(1).strip().strip('"').strip("'")


def host(chain, side):
    if side == "bench":
        return os.environ.get(f"BENCH_URL_{chain.upper()}", f"http://{chain}-tycho-indexer-1.prod-tycho.svc:8080")
    if chain == "ethereum":
        return "https://tycho-dev.propellerheads.xyz" if side == "dev" else "https://tycho-beta.propellerheads.xyz"
    return f"https://tycho-{chain}-{'dev' if side == 'dev' else 'beta'}.propellerheads.xyz"


def post(chain, side, path, body):
    req = urllib.request.Request(host(chain, side) + path, data=json.dumps(body).encode(), method="POST",
                                 headers={**auth_headers(), "Content-Type": "application/json", "User-Agent": "startup-load-bench/1"})
    return json.load(urllib.request.urlopen(req, timeout=120))


def vm(q):
    """Instant query against the metrics datasource through the Grafana proxy."""
    url = f"{GRAFANA}/api/datasources/proxy/uid/{VM_UID}/api/v1/query?query=" + urllib.parse.quote(q)
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {env('GRAFANA_API_KEY')}"})
    return json.load(urllib.request.urlopen(req, timeout=30))["data"]["result"]


def tips(chain):
    """{extractor: {"dev": max over dev pods, "prod": min over the live prod pods, "bench": the prod -1 pod}}"""
    res = {}
    rows = vm(f'max by (namespace,pod,extractor) (extractor_current_block_number{{namespace=~"dev-tycho|prod-tycho",'
              f' pod=~"{chain}-tycho-indexer-.*"}})')
    for r in rows:
        m = r["metric"]
        if "-new-" in m["pod"] or "staging" in m["pod"]:
            continue
        if m["namespace"] == "dev-tycho":
            side = "dev"
        elif m["pod"].startswith(f"{chain}-tycho-indexer-1-"):
            side = "bench"
        else:
            side = "prod"
        v = int(float(r["value"][1]))
        d = res.setdefault(m["extractor"], {})
        d[side] = max(d.get(side, 0), v) if side in ("dev", "bench") else min(d.get(side, 10**12), v)
    return res
