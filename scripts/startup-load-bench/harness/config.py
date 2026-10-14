"""Loads a target config (bench/<target>.toml) and fills in the defaults."""
import hashlib, os, re, tomllib

METRICS_DEFAULTS = {"grafana_url": "https://grafana.propellerheads.xyz",
                    "vm_uid": "P4169E866C3094E38",     # VictoriaMetrics datasource: tycho and kube metrics
                    "cw_uid": "P034F075C744B399F"}     # CloudWatch datasource: RDS CPU
PROFILE_DEFAULTS = dict(weight=1, protocols="all", exclude=[], page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100,
                        sort_contracts=True, no_dci=False)
SCENARIO_DEFAULTS = dict(set="default", spread_s=0, profile=None, version_age=0, single_id_calls=0, wait_idle=True, delay_s=0)


def load(path):
    with open(path, "rb") as f:
        cfg = tomllib.load(f)
    t = cfg["target"]
    for k in ("name", "chain", "url", "pod_selector"):
        if k not in t:
            raise SystemExit(f"{path}: target.{k} is required")
    t.setdefault("plan_header", None)
    t.setdefault("live_pod_selector", None)
    t.setdefault("rds_instance", None)
    rules = cfg.setdefault("rules", {})
    rules.setdefault("tvl_floor", 0.1)
    rules.setdefault("max_version_age_s", 600)
    m = cfg.setdefault("metrics", {})
    for k, v in METRICS_DEFAULTS.items():
        m.setdefault(k, v)
    names = set()
    for p in cfg.get("profiles", []):
        if "name" not in p or "tvl_gt" not in p:
            raise SystemExit(f"{path}: every profile needs name and tvl_gt")
        for k, v in PROFILE_DEFAULTS.items():
            p.setdefault(k, v)
        names.add(p["name"])
    if not cfg.get("profiles"):
        raise SystemExit(f"{path}: no profiles")
    seen = set()
    for s in cfg.get("scenarios", []):
        if "name" not in s or "startups" not in s:
            raise SystemExit(f"{path}: every scenario needs name and startups")
        if s["name"] in seen:
            raise SystemExit(f"{path}: scenario {s['name']} is listed twice")
        seen.add(s["name"])
        for k, v in SCENARIO_DEFAULTS.items():
            s.setdefault(k, v)
        if s["profile"] and s["profile"] not in names:
            raise SystemExit(f"{path}: scenario {s['name']} uses unknown profile {s['profile']}")
        if s["version_age"] not in ("max", 0) and not isinstance(s["version_age"], (int, float)):
            raise SystemExit(f"{path}: scenario {s['name']}: version_age is a number of seconds or \"max\"")
    if not cfg.get("scenarios"):
        raise SystemExit(f"{path}: no scenarios")
    cfg["path"] = os.path.abspath(path)
    cfg["sha256"] = hashlib.sha256(open(path, "rb").read()).hexdigest()
    return cfg


def load_env(path=None):
    """Puts the variables of a .env file into the environment, without overriding what is set."""
    path = path or os.environ.get("ENV_FILE") or os.path.join(os.getcwd(), ".env")
    if not os.path.exists(path):
        path = os.path.expanduser("~/.env")
    if not os.path.exists(path):
        return
    for line in open(path):
        m = re.match(r"\s*(?:export\s+)?([A-Z_][A-Z0-9_]*)=(.*)", line)
        if m and m.group(1) not in os.environ:
            os.environ[m.group(1)] = m.group(2).strip().strip('"').strip("'")


def key(name):
    v = os.environ.get(name)
    if not v:
        raise SystemExit(f"{name} is not set (environment or .env)")
    return v
