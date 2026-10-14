#!/usr/bin/env python3
"""Pod metrics, idle detection and the health probe used around every wave."""
import json, time, urllib.error, urllib.parse, urllib.request
from chain import GRAFANA, SIDE, VM_UID, auth_headers, env, host

NS = "dev-tycho" if SIDE == "dev" else "prod-tycho"
POD_RE = lambda c: f"{c}-tycho-indexer-[0-9a-f]+-[a-z0-9]+" if SIDE == "dev" else f"{c}-tycho-indexer-1-[0-9a-f]+-[a-z0-9]+"
PROXY_RE = lambda c: f"{c}-auth-proxy-.*" if SIDE == "dev" else f"{c}-auth-proxy-indexer-1-.*"


def vm_range(q, start, end, step=5):
    url = (f"{GRAFANA}/api/datasources/proxy/uid/{VM_UID}/api/v1/query_range?"
           + urllib.parse.urlencode({"query": q, "start": int(start), "end": int(end), "step": step}))
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {env('GRAFANA_API_KEY')}"})
    return json.load(urllib.request.urlopen(req, timeout=60))["data"]["result"]


def series(q, start, end):
    """[(t, value)] summed over all returned series per timestamp."""
    acc = {}
    for r in vm_range(q, start, end):
        for t, v in r["values"]:
            acc[float(t)] = acc.get(float(t), 0) + float(v)
    return sorted(acc.items())


def pod_metrics(chain, start, end):
    sel = f'namespace="{NS}", pod=~"{POD_RE(chain)}", container!=""'
    proxy = f'namespace="{NS}", pod=~"{PROXY_RE(chain)}", container!=""'
    return {
        "cpu": series(f"sum(rate(container_cpu_usage_seconds_total{{{sel}}}[30s]))", start, end),
        "mem_gib": series(f"sum(container_memory_working_set_bytes{{{sel}}}) / 2^30", start, end),
        "mem_limit_gib": series(f'max(kube_pod_container_resource_limits{{namespace="{NS}", resource="memory", pod=~"{POD_RE(chain)}"}}) / 2^30', start, end),
        "restarts": series(f'sum(kube_pod_container_status_restarts_total{{namespace="{NS}", pod=~"{POD_RE(chain)}"}})', start, end),
        "ready": series(f'sum(kube_pod_status_ready{{namespace="{NS}", condition="true", pod=~"{POD_RE(chain)}"}})', start, end),
        "proxy_cpu": series(f"sum(rate(container_cpu_usage_seconds_total{{{proxy}}}[30s]))", start, end),
        "ingress_cpu": series('sum(rate(container_cpu_usage_seconds_total{namespace="ingress-nginx", container!=""}[30s]))', start, end),
        # Blocks the pod under test is behind the live prod pod (max over extractors).
        "lag_blocks": series(
            f'max(min by (extractor) (extractor_current_block_number{{namespace="prod-tycho", pod=~"{chain}-tycho-indexer-2-.*"}})'
            f' - on (extractor) max by (extractor) (extractor_current_block_number{{namespace="{NS}", pod=~"{POD_RE(chain)}"}}))',
            start, end),
    }


def wait_idle(chain, limit=0.4, max_wait=600):
    t0 = time.time()
    while time.time() - t0 < max_wait:
        now = time.time()
        cpu = series(f'sum(rate(container_cpu_usage_seconds_total{{namespace="{NS}", pod=~"{POD_RE(chain)}", container!=""}}[1m]))', now - 60, now)
        ready = series(f'sum(kube_pod_status_ready{{namespace="{NS}", condition="true", pod=~"{POD_RE(chain)}"}})', now - 30, now)
        if cpu and cpu[-1][1] < limit and ready and ready[-1][1] >= 1:
            return cpu[-1][1]
        time.sleep(20)
    return None


def poll_health(chain, stop, rows):
    url = host(chain, SIDE) + "/v1/health"
    while not stop.is_set():
        start = time.time()
        try:
            req = urllib.request.Request(url, headers={**auth_headers(), "Connection": "close"})
            with urllib.request.urlopen(req, timeout=30) as r:
                r.read()
                rows.append([start, time.time() - start, r.status])
        except urllib.error.HTTPError as e:
            rows.append([start, time.time() - start, e.code])
        except Exception:
            rows.append([start, time.time() - start, 0])
        time.sleep(0.15)


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
