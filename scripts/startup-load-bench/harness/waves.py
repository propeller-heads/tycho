"""Runs one scenario: waits for an idle target, drives startup-loadgen, collects the metrics around it."""
import json, os, subprocess, threading, time
import metrics, tycho

EPS = ["tokens", "protocol_components", "traced_entry_points", "protocol_state", "contract_state"]


def p95(v):
    v = sorted(v)
    return v[min(len(v) - 1, int(len(v) * 0.95))] if v else None


def delta(a, b):
    return {k: b.get(k, 0) - a.get(k, 0) for k in set(a) | set(b)}


def poll_health(cfg, stop, rows):
    while not stop.is_set():
        t = time.time()
        took, code = tycho.health(cfg)
        rows.append([t, took, code])
        time.sleep(0.15)


def run(cfg, plan, sc, loadgen, out, max_wait=600, log=print):
    """Runs scenario `sc` and writes the result to `out`; returns the summary."""
    plan_path = out + ".plan.json"
    with open(plan_path, "w") as f:
        json.dump(plan, f)
    idle = metrics.wait_idle(cfg, max_wait=max_wait) if sc["wait_idle"] else None
    t = metrics.tips(cfg)
    systems = {x["system"] for pr in plan["profiles"] for x in pr["protocols"]}
    block = min(t[s] for s in systems if s in t) - 5
    bt = metrics.block_time(cfg) or 12.0
    before = metrics.counters(cfg)
    stop, health = threading.Event(), []
    th = threading.Thread(target=poll_health, args=(cfg, stop, health), daemon=True)
    th.start()
    time.sleep(1)
    age = sc["version_age"]
    if age == "max":
        # As old as the target's plan allows, minus a minute of slack for the tip moving while the
        # requests are in flight, minus the 5 blocks the base block already sits behind the tip.
        age_blocks = max(0, int((cfg["rules"]["max_version_age_s"] - 60) / bt) - 5)
    else:
        age_blocks = int(age / bt)
    env = {**os.environ, "SPREAD_S": str(sc["spread_s"]), "BLOCK_TIME_S": f"{bt:.3f}", "VERSION_AGE_BLOCKS": str(age_blocks),
           "SINGLE_ID_CALLS": str(sc["single_id_calls"])}
    if sc["profile"]:
        env["PROFILE"] = sc["profile"]
    start = time.time()
    r = subprocess.run([loadgen, plan_path, str(sc["startups"]), str(block), out + ".lg"], capture_output=True, text=True, env=env)
    end = time.time()
    time.sleep(2)
    stop.set()
    th.join()
    time.sleep(45)                     # let the last scrapes land
    after = metrics.counters(cfg)
    m = metrics.pod_metrics(cfg, start - 60, end + 45)
    lg = json.load(open(out + ".lg")) if os.path.exists(out + ".lg") else {}
    recs, sts = lg.get("records", []), lg.get("startups", [])
    bad = [x for x in recs if x["status"] != 200 or x["error"]]
    win = lambda k: [v for ts, v in m[k] if start - 5 <= ts <= end + 30]
    pre = lambda k: [v for ts, v in m[k] if ts < start]
    by_profile = {}
    for s in sts:
        by_profile.setdefault(s["profile"], []).append(s["ready_s"])
    bad_kind = {}
    for x in bad:
        k = f"{x['status'] or 'err'} {x['endpoint']}"
        bad_kind[k] = bad_kind.get(k, 0) + 1
    ready = sorted(s["ready_s"] for s in sts)
    summary = {
        "target": cfg["target"]["name"], "chain": cfg["target"]["chain"], "scenario": sc["name"], "set": sc["set"], "n": sc["startups"],
        "spread_s": sc["spread_s"], "profile": sc["profile"], "version_age_blocks": age_blocks, "single_id_calls": sc["single_id_calls"],
        "block": block, "block_time_s": bt, "idle_cpu_before": idle, "start": start, "end": end,
        "loadgen_exit": r.returncode, "loadgen_stderr": r.stderr[-600:], "mix": lg.get("mix"),
        "requests": len(recs), "bad": len(bad), "bad_kind": bad_kind,
        "ready_p50": ready[len(ready) // 2] if ready else None, "ready_max": ready[-1] if ready else None,
        "ready_by_profile": {k: {"p50": sorted(v)[len(v) // 2], "max": max(v), "n": len(v)} for k, v in by_profile.items()},
        "ep_p95": {ep: v for ep in EPS if (v := p95([x["total_s"] for x in recs if x["endpoint"] == ep])) is not None},
        "mb": sum(x["bytes"] for x in recs) / 1e6, "wall_s": lg.get("wall_s"),
        "cpu_peak": max(win("cpu"), default=None), "mem_peak_gib": max(win("mem_gib"), default=None),
        "mem_before_gib": pre("mem_gib")[-1] if pre("mem_gib") else None,
        "mem_limit_gib": max(m["mem_limit_gib"], default=(0, None))[1] if m["mem_limit_gib"] else None,
        "restarts_delta": max(win("restarts"), default=0) - (pre("restarts")[-1] if pre("restarts") else 0),
        "ready_min": min(win("ready"), default=None), "lag_peak": max(win("lag_blocks"), default=None),
        "health_n": len(health), "health_max": max((h[1] for h in health), default=None), "health_bad": sum(1 for h in health if h[2] != 200),
        "db_path_delta": delta(before["db_path"], after["db_path"]),
        "cache_hits_delta": delta(before["cache_hits"], after["cache_hits"]), "cache_misses_delta": delta(before["cache_misses"], after["cache_misses"]),
        "db_cpu_max": metrics.rds_cpu_max(cfg, start, end),
    }
    with open(out + ".tmp", "w") as f:    # atomic: a concurrent pull never sees a half-written file
        json.dump({"summary": summary, "metrics": m, "health": health, "startups": sts, "records": recs}, f)
    os.replace(out + ".tmp", out)
    for p in (out + ".lg", plan_path):
        if os.path.exists(p):
            os.remove(p)
    log(json.dumps({k: (round(v, 2) if isinstance(v, float) else v) for k, v in summary.items() if k not in ("loadgen_stderr", "start", "end")}))
    return summary


def broke(s):
    reasons = []
    if s["restarts_delta"] and s["restarts_delta"] > 0:
        reasons.append("pod restarted")
    if s["ready_min"] is not None and s["ready_min"] < 1:
        reasons.append("pod not ready")
    if s["requests"] and s["bad"] / s["requests"] >= 0.05:
        reasons.append(f"{s['bad']}/{s['requests']} requests failed")
    if s["loadgen_exit"] != 0:
        reasons.append("loadgen failed")
    return reasons
