#!/usr/bin/env python3
"""usage: cw.py <instance> <start_epoch> <end_epoch> <period_s> metric[:stat] ...  -> TSV of UTC time + one column per metric"""
import json, os, re, sys, urllib.request, datetime
from chain import GRAFANA, env
key = env("GRAFANA_API_KEY")
CW_UID = os.environ.get("GRAFANA_CW_UID", "P034F075C744B399F")   # CloudWatch datasource
inst, start, end, period = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
queries, names = [], []
for i, m in enumerate(sys.argv[5:]):
    name, _, stat = m.partition(":")
    names.append(m)
    queries.append({"refId": f"Q{i}", "datasource": {"type": "cloudwatch", "uid": CW_UID},
                    "queryMode": "Metrics", "metricQueryType": 0, "metricEditorMode": 0, "region": "eu-central-1",
                    "namespace": "AWS/RDS", "metricName": name, "dimensions": {"DBInstanceIdentifier": [inst]},
                    "statistic": stat or "Average", "period": period, "matchExact": True,
                    "intervalMs": int(period) * 1000, "maxDataPoints": 2000})
body = json.dumps({"queries": queries, "from": str(start * 1000), "to": str(end * 1000)}).encode()
req = urllib.request.Request(f"{GRAFANA}/api/ds/query", data=body,
                             headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"})
res = json.load(urllib.request.urlopen(req, timeout=60))["results"]
cols = {}
for i, n in enumerate(names):
    fr = res[f"Q{i}"].get("frames") or []
    if fr:
        ts, vs = fr[0]["data"]["values"][:2]
        for t, v in zip(ts, vs):
            cols.setdefault(t, {})[n] = v
print("time\t" + "\t".join(names))
for t in sorted(cols):
    row = [cols[t].get(n) for n in names]
    print(datetime.datetime.utcfromtimestamp(t / 1000).strftime("%H:%M") + "\t" +
          "\t".join("" if v is None else f"{v:.4g}" for v in row))
