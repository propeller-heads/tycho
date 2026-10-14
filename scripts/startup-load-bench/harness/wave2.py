#!/usr/bin/env python3
"""Realistic and worst-case startup scenarios on one chain with startup-loadgen.

usage: wave2.py <chain> <set> [tag]     set = realistic | worst | all | smoke
Every scenario writes load/waves2/<chain>_<tag>_<scenario>.json and one JSON summary line to stdout.
"""
import base64, gzip, json, os, subprocess, sys, threading, time, urllib.parse, urllib.request
from chain import GRAFANA, L, LOADGEN, S, SIDE, USER_PLAN, VM_UID, env, host, tips
from wave import NS, POD_RE, pod_metrics, poll_health, series, wait_idle, broke

EPS = ["tokens", "protocol_components", "traced_entry_points", "protocol_state", "contract_state"]
RDS = ({"ethereum": "tycho-dev", "base": "dev-base", "bsc": "dev-bsc"} if SIDE == "dev"
       else {"ethereum": "prod-ethereum", "robinhood": "prod-robinhood"})   # each -1 pod has its own RDS
POD = lambda c: f'namespace="{NS}", pod=~"{POD_RE(c)}"'


def vm_scalar_by(q, label):
    url = f"{GRAFANA}/api/datasources/proxy/uid/{VM_UID}/api/v1/query?" + urllib.parse.urlencode({"query": q})
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {env('GRAFANA_API_KEY')}"})
    return {r["metric"].get(label, "?"): float(r["value"][1]) for r in json.load(urllib.request.urlopen(req, timeout=60))["data"]["result"]}


def counters(chain):
    return {"db_path": vm_scalar_by(f'sum by (reason) (db_path_requests{{{POD(chain)}}})', "reason"),
            "cache_hits": vm_scalar_by(f'sum by (exported_cache) (rpc_cache_hits{{{POD(chain)}}})', "exported_cache"),
            "cache_misses": vm_scalar_by(f'sum by (exported_cache) (rpc_cache_misses{{{POD(chain)}}})', "exported_cache")}


def delta(a, b):
    return {k: b.get(k, 0) - a.get(k, 0) for k in set(a) | set(b)}


def block_time(chain, minutes=10):
    """Seconds per block, measured from the extractors over the last `minutes`. bsc moved from
    0.75 s to 0.45 s blocks; a constant pins late starters hundreds of blocks behind the tip."""
    q = (f'max(extractor_current_block_number{{{POD(chain)}}}) - max(extractor_current_block_number{{{POD(chain)}}} offset {minutes}m)')
    url = f"{GRAFANA}/api/datasources/proxy/uid/{VM_UID}/api/v1/query?" + urllib.parse.urlencode({"query": q})
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {env('GRAFANA_API_KEY')}"})
    r = json.load(urllib.request.urlopen(req, timeout=60))["data"]["result"]
    blocks = float(r[0]["value"][1]) if r else 0
    return minutes * 60 / blocks if blocks > 0 else None


def rds_cpu(chain, start, end):
    if chain not in RDS:
        return None
    out = subprocess.run(["python3", S + "/cw.py", RDS[chain],
                          str(int(start) - 60), str(int(end) + 60), "60", "CPUUtilization:Maximum"], capture_output=True, text=True).stdout
    vals = [float(l.split("\t")[1]) for l in out.splitlines()[1:] if l.strip() and l.split("\t")[1]]
    return max(vals) if vals else None


def p95(v):
    v = sorted(v); return v[min(len(v) - 1, int(len(v) * 0.95))] if v else None


def run(chain, name, n, spread=0.0, tag="v2", envx=None, idle_limit=0.4, max_wait=600):
    plan = json.load(open(f"{L}/plan2_{chain}.json"))
    idle = wait_idle(chain, limit=idle_limit, max_wait=max_wait)
    t = tips(chain)
    systems = {p["system"] for pr in plan["profiles"] for p in pr["protocols"]}
    block = min(t[s][SIDE] for s in systems if s in t and SIDE in t[s]) - 5
    os.makedirs(f"{L}/waves2", exist_ok=True)
    out = f"{L}/waves2/{chain}_{tag}_{name}.json"
    before = counters(chain)
    stop, health = threading.Event(), []
    th = threading.Thread(target=poll_health, args=(chain, stop, health), daemon=True); th.start()
    time.sleep(1)
    bt = block_time(chain) or plan["block_time_s"]
    envx = dict(envx or {})
    if envx.get("VERSION_AGE_BLOCKS") == "auto":
        envx["VERSION_AGE_BLOCKS"] = str(int(plan["max_version_age_s"] / bt))
    e = {**os.environ, "TYCHO_API_KEY": env("TYCHO_API_KEY"), "SPREAD_S": str(spread), "BLOCK_TIME_S": f"{bt:.3f}", **envx}
    if SIDE == "bench":
        e["USER_PLAN"] = USER_PLAN     # sent as X-User-Plan: there is no auth-proxy on the in-cluster path
    start = time.time()
    r = subprocess.run([LOADGEN, f"{L}/plan2_{chain}.json", str(n), str(block), out + ".lg"], capture_output=True, text=True, env=e)
    end = time.time()
    time.sleep(2); stop.set(); th.join()
    time.sleep(45)
    after = counters(chain)
    m = pod_metrics(chain, start - 60, end + 45)
    lg = json.load(open(out + ".lg")) if os.path.exists(out + ".lg") else {}
    recs = lg.get("records", []); sts = lg.get("startups", [])
    bad = [x for x in recs if x["status"] != 200 or x["error"]]
    win = lambda k: [v for ts, v in m[k] if start - 5 <= ts <= end + 30]
    pre = lambda k: [v for ts, v in m[k] if ts < start]
    by_profile = {}
    for s in sts:
        by_profile.setdefault(s["profile"], []).append(s["ready_s"])
    ep_p95 = {ep: p95([x["total_s"] for x in recs if x["endpoint"] == ep]) for ep in EPS}
    ep_p95 = {k: v for k, v in ep_p95.items() if v is not None}
    bad_kind = {}
    for x in bad:
        k = f"{x['status'] or 'err'} {x['endpoint']}"; bad_kind[k] = bad_kind.get(k, 0) + 1
    ready = sorted(s["ready_s"] for s in sts)
    summary = {
        "chain": chain, "tag": tag, "scenario": name, "n": n, "spread_s": spread, "env": envx, "block": block, "block_time_s": bt, "idle_cpu_before": idle,
        "start": start, "end": end, "loadgen_exit": r.returncode, "loadgen_stderr": r.stderr[-600:], "mix": lg.get("mix"),
        "requests": len(recs), "bad": len(bad), "bad_kind": bad_kind,
        "ready_p50": ready[len(ready) // 2] if ready else None, "ready_max": ready[-1] if ready else None,
        "ready_by_profile": {k: {"p50": sorted(v)[len(v) // 2], "max": max(v), "n": len(v)} for k, v in by_profile.items()},
        "ep_p95": ep_p95, "mb": sum(x["bytes"] for x in recs) / 1e6, "wall_s": lg.get("wall_s"),
        "cpu_peak": max(win("cpu"), default=None), "mem_peak_gib": max(win("mem_gib"), default=None),
        "mem_before_gib": pre("mem_gib")[-1] if pre("mem_gib") else None,
        "mem_limit_gib": max(m["mem_limit_gib"], default=(0, None))[1] if m["mem_limit_gib"] else None,
        "restarts_delta": (max(win("restarts"), default=0) - (pre("restarts")[-1] if pre("restarts") else 0)),
        "ready_min": min(win("ready"), default=None), "lag_peak": max(win("lag_blocks"), default=None),
        "health_n": len(health), "health_max": max((h[1] for h in health), default=None), "health_bad": sum(1 for h in health if h[2] != 200),
        "db_path_delta": delta(before["db_path"], after["db_path"]),
        "cache_hits_delta": delta(before["cache_hits"], after["cache_hits"]), "cache_misses_delta": delta(before["cache_misses"], after["cache_misses"]),
        "db_cpu_max": rds_cpu(chain, start, end),
    }
    json.dump({"summary": summary, "metrics": m, "health": health, "startups": sts, "records": recs}, open(out, "w"))
    if os.path.exists(out + ".lg"):
        os.remove(out + ".lg")
    print(json.dumps({k: (round(v, 2) if isinstance(v, float) else v) for k, v in summary.items() if k not in ("loadgen_stderr", "start", "end")}), flush=True)
    if SIDE != "dev":
        # The pod's stdout is the only channel back: one gzip+base64 line per wave (metrics are left out,
        # they can be queried again from Grafana with the start/end stamps in the summary).
        payload = gzip.compress(json.dumps({"summary": summary, "health": health, "startups": sts, "records": recs}).encode())
        print(f"RESULT {os.path.basename(out)} {base64.b64encode(payload).decode()}", flush=True)
    return summary


def scenarios(chain, which):
    eth = chain == "ethereum"
    broad = "low_tvl_broad" if chain != "bsc" else "low_tvl_small_pages"
    # Dev ethereum (22 GiB) died at 32 real-mix startups; the prod pod has 44 GiB, so the 64-wave runs
    # there too, last, after the staggered waves (wave2 stops the set if the pod restarts).
    n64 = [("mix_n64", 64, 0, {})]
    real = ([("mix_n1", 1, 0, {}), ("mix_n8", 8, 0, {}), ("mix_n16", 16, 0, {}), ("mix_n32", 32, 0, {})]
            + ([] if eth else n64)
            + [("staggered_n32_60s", 32, 60, {}), ("staggered_n64_300s", 64, 300, {})]
            + (n64 if eth and SIDE != "dev" else []))
    worst = [
        # 1. The broadest client the plan allows, many at once: every protocol at the TVL floor.
        ("broad_floor_n4", 4, 0, {"PROFILE": broad}),
        ("broad_floor_n8", 8, 0, {"PROFILE": broad}),
        ("broad_floor_n16", 16, 0, {"PROFILE": broad}),
        # 2. Versions as old as the plan allows: below the delta window on fast chains.
        ("old_version_n16", 16, 0, {"VERSION_AGE_BLOCKS": "auto"}),
        # 3. Crash loop: the same wave three times with 20 s between them.
        ("crashloop_a_n16", 16, 0, {}), ("crashloop_b_n16", 16, 0, {}), ("crashloop_c_n16", 16, 0, {}),
        # 4. Pollers: 50 one-id components + one-id state requests per protocol per startup.
        ("hammer_n16", 16, 0, {"SINGLE_ID_CALLS": "50"}),
    ] + ([("tiny_pages_n16", 16, 0, {"PROFILE": "tiny_pages"})] if chain == "bsc" else [])
    smoke = [("mix_n2", 2, 0, {}), ("staggered_n4_10s", 4, 10, {})]
    redo = [("staggered_n32_60s", 32, 60, {}), ("staggered_n64_300s", 64, 300, {}), ("old_version_n16", 16, 0, {"VERSION_AGE_BLOCKS": "auto"})]
    # ethereum after the 32-wave restart: the staggered waves it did not reach and the worst cases that fit the pod.
    worst_eth = [("staggered_n32_60s", 32, 60, {}), ("staggered_n64_300s", 64, 300, {}),
                 ("broad_floor_n4", 4, 0, {"PROFILE": "low_tvl_broad"}), ("broad_floor_n8", 8, 0, {"PROFILE": "low_tvl_broad"}),
                 ("old_version_n16", 16, 0, {"VERSION_AGE_BLOCKS": "auto"}),
                 ("crashloop_a_n16", 16, 0, {}), ("crashloop_b_n16", 16, 0, {}), ("crashloop_c_n16", 16, 0, {}),
                 ("hammer_n16", 16, 0, {"SINGLE_ID_CALLS": "50"})]
    return {"realistic": real, "worst": worst, "all": real + worst, "smoke": smoke, "redo": redo, "worst_eth": worst_eth}[which]


if __name__ == "__main__":
    chain, which = sys.argv[1], sys.argv[2]
    tag = sys.argv[3] if len(sys.argv) > 3 else "v2"
    for i, (name, n, spread, envx) in enumerate(scenarios(chain, which)):
        if name.startswith("crashloop_b") or name.startswith("crashloop_c"):
            time.sleep(20)
            s = run(chain, name, n, spread, tag, envx, idle_limit=99)   # do not wait for idle: that is the point
        else:
            # The first scenario may have to wait for a fresh pod to finish loading its snapshot.
            s = run(chain, name, n, spread, tag, envx, max_wait=3600 if i == 0 else 600)
        why = broke(s)
        if why:
            print(f"BROKE at {name}: {why}", flush=True)
            if s["restarts_delta"] or (s["ready_min"] is not None and s["ready_min"] < 1):
                print("pod restarted or not ready: stopping this set", flush=True)
                break
