#!/usr/bin/env python3
"""Startup load benchmark: one config file says what runs against which target.

usage: bench.py show <config>                        print the resolved run and exit
       bench.py plan <config> [--out DIR]            build the plan (the ids every profile fetches today)
       bench.py run  <config> [--out DIR] [--only a,b] [--sets realistic,worst] [--yes]
       bench.py pull <local dir> [--namespace NS] [--pod NAME] [--once] [--url URL]

run = show + plan (if missing) + every scenario in config order. Results: <out>/run.json,
<out>/plan.json, <out>/waves/<scenario>.json. A scenario whose file exists is skipped, so a
re-run continues where it stopped; the run stops if the target restarts or stops being ready.
Keys: TYCHO_API_KEY and GRAFANA_API_KEY from the environment or a .env file (cwd or ~).
"""
import html.parser, json, os, subprocess, sys, time, urllib.request
import config as C

HERE = os.path.dirname(os.path.abspath(__file__))


def arg(args, name, default=None):
    return args[args.index(name) + 1] if name in args else default


def loadgen_bin():
    p = os.environ.get("LOADGEN_BIN") or os.path.normpath(os.path.join(HERE, "..", "loadgen", "target", "release", "startup-loadgen"))
    if not os.path.exists(p):
        print("building startup-loadgen", flush=True)
        subprocess.run(["cargo", "build", "--release", "--manifest-path", os.path.join(HERE, "..", "loadgen", "Cargo.toml")], check=True)
    return p


def show(cfg, scenarios, plan=None):
    import plan as P
    t = cfg["target"]
    print(f"target    {t['name']}  chain={t['chain']}")
    print(f"url       {t['url']}")
    print(f"plan hdr  {t['plan_header'] or '(none: the auth-proxy sets it)'}")
    print(f"watching  {t['pod_selector']}" + (f"   rds={t['rds_instance']}" if t['rds_instance'] else ""))
    print(f"rules     tvl >= {cfg['rules']['tvl_floor']}, versions <= {cfg['rules']['max_version_age_s']} s old")
    print(f"config    {cfg['path']}  sha256 {cfg['sha256'][:12]}")
    print("profiles")
    for p in cfg["profiles"]:
        line = f"  {p['name']:<22} weight {p['weight']:<2} tvl>{p['tvl_gt']:<8} pages {p['page_size']:<5} state {p['state_chunk']:<4} contract {p['contract_chunk']:<5} protocols {p['protocols'] if isinstance(p['protocols'], str) else len(p['protocols'])}"
        if plan:
            d = P.describe_profile(next(x for x in plan["profiles"] if x["name"] == p["name"]), plan["token_pages"])
            line += f"  -> {d['components']} components, {d['contracts']} contracts, {d['requests']} requests per startup"
        print(line)
    print("scenarios")
    for s in scenarios:
        extra = [f"over {s['spread_s']} s" if s["spread_s"] else "", f"profile {s['profile']}" if s["profile"] else "",
                 f"versions {s['version_age']} old" if s["version_age"] else "", f"{s['single_id_calls']} single-id calls" if s["single_id_calls"] else "",
                 "no idle wait" if not s["wait_idle"] else "", f"after {s['delay_s']} s" if s["delay_s"] else ""]
        print(f"  {s['set']:<10} {s['name']:<22} {s['startups']:>3} startups  " + "  ".join(x for x in extra if x))


def select(cfg, args):
    sets = arg(args, "--sets")
    only = arg(args, "--only")
    sc = cfg["scenarios"]
    if sets:
        sc = [s for s in sc if s["set"] in sets.split(",")]
    if only:
        sc = [s for s in sc if s["name"] in only.split(",")]
    return sc


def cmd_run(cfg, args, dry):
    import metrics, plan as P, waves
    out = arg(args, "--out", os.path.join("results", cfg["target"]["name"]))
    scenarios = select(cfg, args)
    show(cfg, scenarios)
    if dry:
        return
    if "--yes" not in args and sys.stdin.isatty():
        if input("run this? [y/N] ").strip().lower() != "y":
            return
    os.makedirs(os.path.join(out, "waves"), exist_ok=True)
    loadgen = loadgen_bin()
    plan_path = os.path.join(out, "plan.json")
    if os.path.exists(plan_path):
        plan = json.load(open(plan_path))
        print(f"plan: reusing {plan_path}", flush=True)
    else:
        print("plan: building", flush=True)
        plan = P.build(cfg, plan_path, log=lambda m: print(m, flush=True))
    run = {"target": cfg["target"], "rules": cfg["rules"], "config": cfg["path"], "config_sha256": cfg["sha256"], "started": time.time(),
           "target_image": metrics.image(cfg), "profiles": {p["name"]: P.describe_profile(p, plan["token_pages"]) for p in plan["profiles"]},
           "scenarios": scenarios, "git": git_rev()}
    with open(os.path.join(out, "run.json"), "w") as f:
        json.dump(run, f, indent=1)
    print(f"target image {run['target_image']}", flush=True)
    for i, sc in enumerate(scenarios):
        path = os.path.join(out, "waves", sc["name"] + ".json")
        if os.path.exists(path):
            print(f"skip {sc['name']}: result exists", flush=True)
            continue
        if sc["delay_s"]:
            time.sleep(sc["delay_s"])
        print(f"--- {sc['name']} {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}", flush=True)
        s = waves.run(cfg, plan, sc, loadgen, path, max_wait=3600 if i == 0 else 600, log=lambda m: print(m, flush=True))
        why = waves.broke(s)
        if why:
            print(f"BROKE at {sc['name']}: {why}", flush=True)
            if s["restarts_delta"] or (s["ready_min"] is not None and s["ready_min"] < 1):
                print("target restarted or not ready: stopping", flush=True)
                break
    run["finished"] = time.time()
    with open(os.path.join(out, "run.json"), "w") as f:
        json.dump(run, f, indent=1)
    print("done", flush=True)


def git_rev():
    try:
        return subprocess.run(["git", "rev-parse", "HEAD"], cwd=HERE, capture_output=True, text=True, timeout=10).stdout.strip() or None
    except Exception:
        return None


class _Links(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.hrefs = []

    def handle_starttag(self, tag, attrs):
        if tag == "a":
            self.hrefs += [v for k, v in attrs if k == "href"]


def _walk(base, path=""):
    p = _Links()
    p.feed(urllib.request.urlopen(base + "/" + path, timeout=30).read().decode())
    for h in p.hrefs:
        if h.startswith("?") or h.startswith("/"):
            continue
        if h.endswith("/"):
            yield from _walk(base, path + h)
        elif not h.endswith((".tmp", ".lg", ".part", ".plan.json")):   # per-wave scratch files
            yield path + h


def _pull(base, dest):
    got = 0
    for rel in _walk(base):
        local = os.path.join(dest, rel)
        size = int(urllib.request.urlopen(urllib.request.Request(base + "/" + rel, method="HEAD"), timeout=30).headers["Content-Length"])
        if os.path.exists(local) and os.path.getsize(local) == size:
            continue
        os.makedirs(os.path.dirname(local) or ".", exist_ok=True)
        with urllib.request.urlopen(base + "/" + rel, timeout=600) as r, open(local + ".part", "wb") as f:
            while chunk := r.read(1 << 20):
                f.write(chunk)
        os.replace(local + ".part", local)
        got += 1
        print(f"pulled {rel} ({size / 1e6:.1f} MB)", flush=True)
    return got


def cmd_pull(args):
    """Copies the results out of the runner pod (it serves its results directory on port 8000)
    through a port-forward, which needs only get + portforward on pods."""
    dest, ns, pod, url = args[0], arg(args, "--namespace", "prod-tycho"), arg(args, "--pod", "tycho-bench"), arg(args, "--url")
    while True:
        pf = None
        try:
            if url:
                base = url
            else:
                port = 18000 + os.getpid() % 1000
                pf = subprocess.Popen(["kubectl", "-n", ns, "port-forward", f"pod/{pod}", f"{port}:8000"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                time.sleep(3)
                base = f"http://127.0.0.1:{port}"
            print(time.strftime("%H:%M:%S"), f"{_pull(base, dest)} new file(s)", flush=True)
        except Exception as e:
            print(time.strftime("%H:%M:%S"), "pull failed:", e, flush=True)
        finally:
            if pf:
                pf.terminate()
        if "--once" in args:
            break
        time.sleep(120)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    cmd, args = sys.argv[1], sys.argv[2:]
    C.load_env(arg(args, "--env-file"))
    if cmd == "pull":
        cmd_pull(args)
        return
    cfg = C.load(args[0])
    if cmd == "show":
        show(cfg, select(cfg, args))
    elif cmd == "plan":
        import plan as P
        out = arg(args, "--out", os.path.join("results", cfg["target"]["name"]))
        plan = P.build(cfg, os.path.join(out, "plan.json"), log=lambda m: print(m, flush=True))
        show(cfg, select(cfg, args), plan)
    elif cmd == "run":
        cmd_run(cfg, args, dry=False)
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
