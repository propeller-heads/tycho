#!/usr/bin/env python3
"""Runs a layer's checks (checks.toml) in the run's worktree and writes the results.

    layer_checks.py --protocol P --layer indexing|simulation|execution|final --round N [--chains a,b]

Results go to `<run>/rounds/<layer>-<n>/checks.json` (one entry per step: pass, fail, infra, off
with the gap that turns it on, or n/a) with a log per step beside it. Exit 0 when every step that
ran passed, 1 when one failed, 3 when only the environment did (`infra`). The implementer runs the
same commands while it works; this run is the one the ledger records.

Chains: a round runs the protocol-testing harness on the run's primary chain (`schema.run_chains`),
plus any chain `--chains` names (a chain the final gates found red); `--layer final` runs it on
every chain. The other steps run once.

Infrastructure: a failed step whose log shows the environment failing (RPC limits or TLS, an
expired Substreams token, Docker, a full disk, a timeout) is run once more after the environment is
checked again. Still failing that way, it is `infra`: never handed to the implementer as a code
failure, and the stop rules ask the person about the named cause. It is not rerun at the base
branch: the integration's package and tests do not exist there.
"""

import argparse
import json
import os
import re
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

# A failed step's log naming one of these is the environment failing, not the code (most specific
# first). Each is (cause, pattern).
INFRA_CAUSES = (
    ("disk full", re.compile(r"No space left on device|ENOSPC", re.I)),
    ("docker", re.compile(r"Cannot connect to the Docker daemon|docker: .*(daemon|not found)|error during connect", re.I)),
    ("substreams token", re.compile(r"(substreams|firehose)[^\n]*(\b401\b|unauthenticated|unauthorized|invalid (api )?token|token (is )?expired)"
                                    r"|(\b401\b|unauthenticated|unauthorized|invalid (api )?token|token (is )?expired)[^\n]*(substreams|firehose)|SUBSTREAMS_API_TOKEN", re.I)),
    ("etherscan", re.compile(r"etherscan[^\n]*(rate limit|max rate|timed? ?out|429|503)", re.I)),
    ("rpc rate limit", re.compile(r"\b429\b|Too Many Requests|rate.?limit(ed)?|exceeded (the )?(compute units|capacity)", re.I)),
    ("network", re.compile(r"tls handshake|certificate verify failed|InvalidCertificate|Connection reset by peer|connection refused|Could not resolve host|dns error|error sending request|network is unreachable|broken pipe|operation timed out", re.I)),
)


def infra_cause(text: str):
    """The infrastructure cause a log shows, or None."""
    return next((cause for cause, rx in INFRA_CAUSES if rx.search(text)), None)


def placeholders(run: Path, tree: Path, chain: str) -> dict:
    """The checks' placeholders for one chain. A `paths` value may be one string, or a map from
    chain to string (a package or test package per chain)."""
    values = {"tree": str(tree), "chain": chain}
    dec_path, spec_path = run / "decisions.json", run / "spec.json"
    if dec_path.exists():
        dec = schema.load_json(dec_path)
        # Checked again here: decisions.json may change after it was validated.
        unsafe = schema.unsafe_values(dec)
        if unsafe:
            raise SystemExit(f"decisions.json has values that cannot go into a command: {unsafe}")
        for k, v in (dec.get("paths") or {}).items():
            v = v.get(chain) if isinstance(v, dict) else v
            if isinstance(v, str):
                values[k] = v
        values.setdefault("protocol_system", (dec.get("agreements") or {}).get("protocol_system", ""))
    values.setdefault("test_package", values.get("package", ""))
    if spec_path.exists():
        values["kind"] = schema.load_json(spec_path).get("kind", "")
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
                return False, f"building tycho-indexer failed (see {log}): {infra_cause(log.read_text(errors='replace')) or 'a build error'}"
            env = {**os.environ, "DB_HOST_PORT": str(self.db_port)}
            db = subprocess.run(["docker", "compose", "-p", self.project, "up", "db", "-d", "--wait"], cwd=self.tree / "protocols/testing", env=env, stdout=out, stderr=subprocess.STDOUT)
            if db.returncode != 0:
                return False, f"docker: starting the harness database failed (see {log}): {infra_cause(log.read_text(errors='replace')) or 'docker compose up'}"
        schema.load_env_files()
        rpc = schema.rpc_url(self.chain)
        if not rpc:
            return False, f"no RPC for {self.chain}: set {' or '.join(schema.rpc_vars(self.chain))}"
        name, ok, detail = envcheck.check_substreams_token(hours=2)
        if not ok:
            return False, f"{name}: {detail}"
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


def run_step(step: dict, values: dict, tree: Path, outdir: Path, name: str, env, layer: str) -> dict:
    """Runs one step once: its result, with the infrastructure cause when the log shows one."""
    report_path = outdir / f"{name}.report.json"
    report_path.unlink(missing_ok=True)
    cmd = step["cmd"].format(**values, report=report_path)
    log = outdir / f"{name}.log"
    started = time.time()
    timeout = step.get("timeout_min", 20)
    proc = None
    try:
        with open(log, "w") as out:
            proc = subprocess.run(["bash", "-o", "pipefail", "-c", cmd], cwd=tree / step.get("cwd", "."), env=env, stdout=out, stderr=subprocess.STDOUT, timeout=timeout * 60)
        status, detail = ("pass" if proc.returncode == 0 else "fail"), f"exit {proc.returncode}"
    except subprocess.TimeoutExpired:
        status, detail = "fail", f"timed out after {timeout} min"
    result = {"name": name, "cmd": cmd, "log": str(log), "seconds": round(time.time() - started)}
    if "{report}" in step["cmd"] and proc is not None:
        status, detail, checks, later = judge_report(report_path, layer_patterns(step, layer), proc.returncode)
        result.update(report=str(report_path), checks=checks, later=later)
    text = log.read_text(errors="replace")
    tail = text.splitlines()[-30:] if status == "fail" else []
    result.update(status=status, detail=detail, tail=tail)
    if status == "fail":
        cause = "timeout" if proc is None else infra_cause("\n".join(text.splitlines()[-400:]))
        if cause:
            result["infra"] = cause
    return result


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--layer", required=True, choices=(*schema.LAYERS, "final"))
    p.add_argument("--round", type=int, required=True)
    p.add_argument("--only", help="comma-separated step names")
    p.add_argument("--chains", help="comma-separated chains to run the harness on besides the primary; `all` for every chain")
    args = p.parse_args()

    run = schema.run_dir(args.protocol)
    tree = schema.worktree_dir(args.protocol)
    outdir = run / "rounds" / f"{args.layer}-{args.round}"
    outdir.mkdir(parents=True, exist_ok=True)
    with open(schema.skill_dir() / "checks.toml", "rb") as f:
        steps = tomllib.load(f)["step"]
    primary, every = schema.run_chains(run)
    extra = every if args.layer == "final" or args.chains == "all" else [c for c in (args.chains or "").split(",") if c]
    unknown = [c for c in extra if c not in every]
    if unknown:
        raise SystemExit(f"chains {unknown} are not the run's ({every})")
    harness_chains = [primary] + [c for c in every if c in extra and c != primary]
    caps = harness_caps.detect(tree)
    only = set(args.only.split(",")) if args.only else None

    results = []
    for step in steps:
        if only and step["name"] not in only:
            continue
        for chain in (harness_chains if step.get("harness") else [primary]):
            values = placeholders(run, tree, chain)
            name = step["name"] if len(harness_chains) == 1 or not step.get("harness") else f"{step['name']}@{chain}"
            runs, why = applies(step, args.layer, values, caps, tree)
            if not runs:
                if why:
                    results.append({"name": name, "status": why.split(":", 1)[0], "detail": why})
                continue
            result = None
            for attempt in (1, 2):
                harness = None
                try:
                    env = None
                    if step.get("harness"):
                        harness = Harness(tree, f"tycho-integrate-{schema.slug(args.protocol)}-{chain}", outdir, chain)
                        ok, detail = harness.up()
                        if not ok:
                            result = {"name": name, "status": "fail", "detail": detail}
                            if infra_cause(detail):
                                result["infra"] = infra_cause(detail)
                            continue
                        env = harness.env
                    result = run_step(step, values, tree, outdir, name, env, args.layer)
                finally:
                    if harness is not None:
                        harness.down()
                if result["status"] != "fail" or not result.get("infra"):
                    break
                # The environment failed: check it again, and try once more.
                schema.load_env_files()
                time.sleep(30)
            if result["status"] == "fail" and result.get("infra"):
                result["status"] = "infra"
                result["detail"] = f"{result['infra']}: {result['detail']} (twice)"
            results.append(result)

    green = all(r["status"] in ("pass", "n/a", "off") for r in results)
    only_infra = not green and all(r["status"] in ("pass", "n/a", "off", "infra") for r in results)
    head = subprocess.run(["git", "-C", str(tree), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    report = {"layer": args.layer, "round": args.round, "head": head, "chains": harness_chains, "green": green, "steps": results, "caps": caps}
    schema.write_json(outdir / "checks.json", report)
    for r in results:
        print(f"{r['status']:5} {r['name']}: {r['detail']}")
    print(json.dumps({"green": green, "infra": [r["name"] for r in results if r["status"] == "infra"], "file": str(outdir / "checks.json")}))
    sys.exit(0 if green else 3 if only_infra else 1)


if __name__ == "__main__":
    main()
