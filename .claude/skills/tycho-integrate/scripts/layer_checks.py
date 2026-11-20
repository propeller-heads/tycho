#!/usr/bin/env python3
"""Runs a layer's checks (checks.toml) in the run's worktree and writes the results.

    layer_checks.py --protocol P --layer indexing|simulation|execution|final --round N

Results go to `<run>/rounds/<layer>-<n>/checks.json` (one entry per step: pass, fail, off with the
gap that turns it on, or n/a) with a log per step beside it. Exit 0 when every step that ran
passed, 1 otherwise. The implementer runs the same commands while it works; this run is the one
the ledger records.
"""

import argparse
import json
import os
import socket
import subprocess
import sys
import time
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envcheck  # noqa: E402
import harness_caps  # noqa: E402
import schema  # noqa: E402


def placeholders(run: Path, tree: Path) -> dict:
    values = {"tree": str(tree)}
    dec_path, spec_path = run / "decisions.json", run / "spec.json"
    if dec_path.exists():
        dec = schema.load_json(dec_path)
        values.update({k: v for k, v in (dec.get("paths") or {}).items() if isinstance(v, str)})
        values.setdefault("protocol_system", (dec.get("agreements") or {}).get("protocol_system", ""))
    values.setdefault("test_package", values.get("package", ""))
    if spec_path.exists():
        spec = schema.load_json(spec_path)
        values["kind"] = spec.get("kind", "")
        values.setdefault("chain", (spec.get("chains") or [""])[0])
    return values


def layer_patterns(step: dict, layer: str):
    """The report checks `layer` answers for: its own `report_checks` and the earlier layers'. None
    when the step lists none, or for the final run: every check."""
    table = step.get("report_checks")
    if not table or layer == "final":
        return None
    out = []
    for l in schema.LAYERS:
        out += table.get(l, [])
        if l == layer:
            break
    return None if "*" in out else out


def judge_report(path: Path, patterns, exit_code: int) -> tuple:
    """(status, detail, failing checks, later-layer checks) of a protocol-testing `--report` file
    (`protocols/testing/README.md`, "Test report").

    The layer's checks (`patterns`; None is all) decide the step: one that failed, did not run, or
    was skipped without a reason fails it. A skip with a reason is structural (a direction capped
    at the int128 limit, a zero-limit dust band): it is listed, not failed. A missing or empty
    report fails, since the runner exits 0 having tested nothing when a package has no test yaml or
    manifest; so does a report with none of the layer's checks. A failed exit with none of the
    layer's checks failing passes only when a later layer's check explains it."""
    if not path.exists():
        return "fail", f"exit {exit_code}, and the runner wrote no report: nothing was tested", [], []
    try:
        results = schema.load_json(path).get("results") or []
    except (OSError, ValueError, AttributeError):
        return "fail", f"exit {exit_code}, and the report {path} is unreadable", [], []
    keep = ("test", "check", "component", "direction", "outcome", "detail", "reason", "block", "limit")
    mine = lambda c: patterns is None or c in patterns
    own = [r for r in results if mine(r.get("check", ""))]
    later = [{k: r[k] for k in keep if k in r} for r in results if not mine(r.get("check", "")) and r.get("outcome") != "pass"]
    bad = [{k: r[k] for k in keep if k in r} for r in own
           if r.get("outcome") in ("fail", "not_run") or (r.get("outcome") == "skipped" and not r.get("reason"))]
    skipped = [{k: r[k] for k in keep if k in r} for r in own if r.get("outcome") == "skipped" and r.get("reason")]
    if not own:
        return "fail", f"exit {exit_code}, and the report has none of this layer's checks ({len(results)} results)", [], later
    if bad:
        return "fail", f"exit {exit_code}; {len(bad)} of this layer's checks failed", bad + skipped, later
    if exit_code != 0 and not later:
        return "fail", f"exit {exit_code} with every check passing: see the log", skipped, later
    note = f"; {len(later)} later-layer checks not counted" if later else ""
    return "pass", f"{len(own)} checks of this layer passed{note}", skipped, later


def free_port(start: int) -> int:
    for port in range(start, start + 200):
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) != 0:
                return port
    raise SystemExit(f"no free port from {start}")


class Harness:
    """A protocol-testing environment of the run's own: a Postgres on a free port, an indexer port,
    and `tycho-indexer` on PATH (the harness runs it by name)."""

    def __init__(self, tree: Path, project: str, logdir: Path, chain: str):
        self.tree, self.project, self.logdir, self.chain = tree, project, logdir, chain
        self.db_port = free_port(5440)
        self.server_port = free_port(4250)
        self.env = None

    def up(self) -> tuple:
        log = self.logdir / "harness-setup.log"
        with open(log, "w") as out:
            build = subprocess.run(["cargo", "build", "--release", "-p", "tycho-indexer"], cwd=self.tree, stdout=out, stderr=subprocess.STDOUT)
            if build.returncode != 0:
                return False, f"building tycho-indexer failed (see {log})"
            env = {**os.environ, "DB_HOST_PORT": str(self.db_port)}
            db = subprocess.run(["docker", "compose", "-p", self.project, "up", "db", "-d", "--wait"], cwd=self.tree / "protocols/testing", env=env, stdout=out, stderr=subprocess.STDOUT)
            if db.returncode != 0:
                return False, f"starting the harness database failed (see {log})"
        envcheck.load_env_files()
        rpc = next((os.environ[n] for n in envcheck.rpc_var(self.chain) if os.environ.get(n)), None)
        if not rpc:
            return False, f"no RPC for {self.chain}: set {' or '.join(envcheck.rpc_var(self.chain))}"
        self.env = {
            **os.environ,
            "RPC_URL": rpc,  # the runner reads the chain's RPC from RPC_URL
            "DATABASE_URL": f"postgres://postgres:mypassword@localhost:{self.db_port}/tycho_indexer_0",
            "TYCHO_SERVER_PORT": str(self.server_port),
            "PATH": f"{self.tree / 'target/release'}:{os.environ.get('PATH', '')}",
        }
        return True, f"db on {self.db_port}, indexer on {self.server_port}"

    def down(self) -> None:
        subprocess.run(["docker", "compose", "-p", self.project, "down"], cwd=self.tree / "protocols/testing", capture_output=True)


def applies(step: dict, layer: str, values: dict, caps: dict, tree: Path) -> tuple:
    """(runs, why not)."""
    layers = step.get("layers", [])
    if layer != "final" and layer not in layers:
        return False, None
    if layer == "final" and not layers:
        return False, None
    when = step.get("when")
    if isinstance(when, list) and values.get("kind") not in when:
        return False, f"n/a: {values.get('kind') or 'unknown'} integration"
    if isinstance(when, str) and when.startswith("file:") and not (tree / when[5:]).exists():
        return False, f"n/a: no {when[5:]} in the tree"
    gap = step.get("requires")
    if gap and not caps.get(gap, {}).get("on"):
        return False, f"off: {caps.get(gap, {}).get('reason', gap + ' unknown')}"
    missing = [k for k in step.get("needs", []) if not values.get(k)]
    if missing:
        return False, f"fail: decisions.json paths lack {missing}"
    return True, None


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--layer", required=True, choices=(*schema.LAYERS, "final"))
    p.add_argument("--round", type=int, required=True)
    p.add_argument("--only", help="comma-separated step names")
    args = p.parse_args()

    run = schema.run_dir(args.protocol)
    tree = schema.worktree_dir(args.protocol)
    outdir = run / "rounds" / f"{args.layer}-{args.round}"
    outdir.mkdir(parents=True, exist_ok=True)
    with open(schema.skill_dir() / "checks.toml", "rb") as f:
        steps = tomllib.load(f)["step"]
    values = placeholders(run, tree)
    caps = harness_caps.detect(tree)
    only = set(args.only.split(",")) if args.only else None

    results, harness = [], None
    try:
        for step in steps:
            if only and step["name"] not in only:
                continue
            runs, why = applies(step, args.layer, values, caps, tree)
            if not runs:
                if why:
                    status = why.split(":", 1)[0]
                    results.append({"name": step["name"], "status": status, "detail": why})
                continue
            env = None
            if step.get("harness"):
                if harness is None:
                    harness = Harness(tree, f"tycho-integrate-{schema.slug(args.protocol)}", outdir, values.get("chain") or "ethereum")
                    ok, detail = harness.up()
                    if not ok:
                        results.append({"name": step["name"], "status": "fail", "detail": detail})
                        continue
                env = harness.env
            report_path = outdir / f"{step['name']}.report.json"
            report_path.unlink(missing_ok=True)
            cmd = step["cmd"].format(**values, report=report_path)
            log = outdir / f"{step['name']}.log"
            started = time.time()
            try:
                with open(log, "w") as out:
                    proc = subprocess.run(["bash", "-o", "pipefail", "-c", cmd], cwd=tree / step.get("cwd", "."), env=env, stdout=out, stderr=subprocess.STDOUT, timeout=step.get("timeout_min", 20) * 60)
                status = "pass" if proc.returncode == 0 else "fail"
                detail = f"exit {proc.returncode}"
            except subprocess.TimeoutExpired:
                status, detail = "fail", f"timed out after {step.get('timeout_min', 20)} min"
            result = {"name": step["name"], "cmd": cmd, "log": str(log), "seconds": round(time.time() - started)}
            if "{report}" in step["cmd"] and not detail.startswith("timed out"):
                status, detail, checks, later = judge_report(report_path, layer_patterns(step, args.layer), proc.returncode)
                result.update(report=str(report_path), checks=checks, later=later)
            tail = log.read_text(errors="replace").splitlines()[-30:] if status == "fail" else []
            result.update(status=status, detail=detail, tail=tail)
            results.append(result)
    finally:
        if harness is not None:
            harness.down()

    green = all(r["status"] in ("pass", "n/a", "off") for r in results)
    head = subprocess.run(["git", "-C", str(tree), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    report = {"layer": args.layer, "round": args.round, "head": head, "green": green, "steps": results, "caps": caps}
    schema.write_json(outdir / "checks.json", report)
    for r in results:
        print(f"{r['status']:5} {r['name']}: {r['detail']}")
    print(json.dumps({"green": green, "file": str(outdir / "checks.json")}))
    sys.exit(0 if green else 1)


if __name__ == "__main__":
    main()
