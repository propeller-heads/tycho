"""What Grafana knows about the target: pod metrics, block heights, cache and DB counters, RDS CPU."""
import json, time, urllib.parse, urllib.request
from config import key


def _get(cfg, path, params):
    url = f"{cfg['metrics']['grafana_url']}/api/datasources/proxy/uid/{cfg['metrics']['vm_uid']}/api/v1/{path}?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {key('GRAFANA_API_KEY')}"})
    return json.load(urllib.request.urlopen(req, timeout=60))["data"]["result"]


def vm(cfg, q):
    return _get(cfg, "query", {"query": q})


def vm_by(cfg, q, label):
    return {r["metric"].get(label, "?"): float(r["value"][1]) for r in vm(cfg, q)}


def series(cfg, q, start, end, step=5):
    """[(t, value)] summed over all returned series per timestamp."""
    acc = {}
    for r in _get(cfg, "query_range", {"query": q, "start": int(start), "end": int(end), "step": step}):
        for t, v in r["values"]:
            acc[float(t)] = acc.get(float(t), 0) + float(v)
    return sorted(acc.items())


def pod_metrics(cfg, start, end):
    sel = cfg["target"]["pod_selector"]
    live = cfg["target"]["live_pod_selector"]
    out = {
        "cpu": series(cfg, f'sum(rate(container_cpu_usage_seconds_total{{{sel}, container!=""}}[30s]))', start, end),
        "mem_gib": series(cfg, f'sum(container_memory_working_set_bytes{{{sel}, container!=""}}) / 2^30', start, end),
        "mem_limit_gib": series(cfg, f'max(kube_pod_container_resource_limits{{{sel}, resource="memory"}}) / 2^30', start, end),
        "restarts": series(cfg, f'sum(kube_pod_container_status_restarts_total{{{sel}}})', start, end),
        "ready": series(cfg, f'sum(kube_pod_status_ready{{{sel}, condition="true"}})', start, end),
    }
    if live:
        # Blocks the target is behind the live pod (max over extractors).
        out["lag_blocks"] = series(cfg, f'max(min by (extractor) (extractor_current_block_number{{{live}}})'
                                        f' - on (extractor) max by (extractor) (extractor_current_block_number{{{sel}}}))', start, end)
    else:
        out["lag_blocks"] = []
    return out


def wait_idle(cfg, limit=0.4, max_wait=600):
    """Waits until the target is ready and under `limit` cores; returns its CPU, or None on timeout."""
    sel = cfg["target"]["pod_selector"]
    t0 = time.time()
    while time.time() - t0 < max_wait:
        now = time.time()
        cpu = series(cfg, f'sum(rate(container_cpu_usage_seconds_total{{{sel}, container!=""}}[1m]))', now - 60, now)
        ready = series(cfg, f'sum(kube_pod_status_ready{{{sel}, condition="true"}})', now - 30, now)
        if cpu and cpu[-1][1] < limit and ready and ready[-1][1] >= 1:
            return cpu[-1][1]
        time.sleep(20)
    return None


def tips(cfg):
    """{extractor: current block} of the target."""
    sel = cfg["target"]["pod_selector"]
    return {r["metric"]["extractor"]: int(float(r["value"][1])) for r in vm(cfg, f'max by (extractor) (extractor_current_block_number{{{sel}}})')}


def block_time(cfg, minutes=10):
    """Seconds per block, measured on the target over the last `minutes`."""
    sel = cfg["target"]["pod_selector"]
    r = vm(cfg, f'max(extractor_current_block_number{{{sel}}}) - max(extractor_current_block_number{{{sel}}} offset {minutes}m)')
    blocks = float(r[0]["value"][1]) if r else 0
    return minutes * 60 / blocks if blocks > 0 else None


def counters(cfg):
    sel = cfg["target"]["pod_selector"]
    return {"db_path": vm_by(cfg, f'sum by (reason) (db_path_requests{{{sel}}})', "reason"),
            "cache_hits": vm_by(cfg, f'sum by (exported_cache) (rpc_cache_hits{{{sel}}})', "exported_cache"),
            "cache_misses": vm_by(cfg, f'sum by (exported_cache) (rpc_cache_misses{{{sel}}})', "exported_cache")}


def image(cfg):
    sel = cfg["target"]["pod_selector"]
    r = vm(cfg, f'kube_pod_container_info{{{sel}, container=~".*tycho-indexer.*"}}')
    return sorted({x["metric"]["image"].split("/")[-1] for x in r})


def rds_cpu_max(cfg, start, end, period=60):
    """Peak CPUUtilization of the target's RDS instance over [start, end] via the CloudWatch datasource."""
    inst = cfg["target"]["rds_instance"]
    if not inst:
        return None
    q = {"refId": "A", "datasource": {"type": "cloudwatch", "uid": cfg["metrics"]["cw_uid"]}, "queryMode": "Metrics",
         "metricQueryType": 0, "metricEditorMode": 0, "region": "eu-central-1", "namespace": "AWS/RDS", "metricName": "CPUUtilization",
         "dimensions": {"DBInstanceIdentifier": [inst]}, "statistic": "Maximum", "period": str(period), "matchExact": True,
         "intervalMs": period * 1000, "maxDataPoints": 2000}
    body = json.dumps({"queries": [q], "from": str(int(start - 60) * 1000), "to": str(int(end + 60) * 1000)}).encode()
    req = urllib.request.Request(f"{cfg['metrics']['grafana_url']}/api/ds/query", data=body,
                                 headers={"Authorization": f"Bearer {key('GRAFANA_API_KEY')}", "Content-Type": "application/json"})
    try:
        frames = json.load(urllib.request.urlopen(req, timeout=60))["results"]["A"].get("frames") or []
        vals = [v for v in frames[0]["data"]["values"][1] if v is not None] if frames else []
        return max(vals) if vals else None
    except Exception:
        return None
