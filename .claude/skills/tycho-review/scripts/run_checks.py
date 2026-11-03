#!/usr/bin/env python3
"""Run a trusted change's builds and tests (SKILL.md step 12, and the Substreams harness of an
integration pass) in the run's worktree, without a model: each step with a timeout, every log masked,
and one results file the orchestrator and the passes read instead of watching the runs.

Steps, from the plan's files: `fmt --check`; clippy (`-D warnings`), both on CI's pinned nightly
(`RUST_NIGHTLY` from the environment, else from the worktree's `mise.toml`), and nextest
(`--no-fail-fast`) on each crate with a changed file; `forge build`, `forge test`, `forge fmt
--check` and `forge lint` when the contracts change; `cargo test` on each changed Substreams package; and the Docker
harness for each `--harness <protocol>[=<test>]`. The harness checks the Substreams credential before
it starts (a StreamingFast API key is exchanged for a JWT, kept in memory only), shows the indexer's
own logs, and stops a case at once when the endpoint rejects the credential, when its log shows no
indexing progress for --stall minutes, or when it passes --harness-timeout: a rejected credential otherwise loops silently for hours.

With --compare-base, every failed cargo or forge test is run again at the base, in a worktree of its
own, so each failure is marked `also fails at the base` or `only at the head`.

usage:
  run_checks.py --plan <run>/plan --worktree <run>/worktree --out <run>/checks [--compare-base]
                [--harness base-foo bsc-foo=case_name ...] [--harness-timeout 180] [--stall 20]
                [--harness-patch <patch>] [--skip fmt clippy ...] [--detach]
  run_checks.py --out <run>/checks --wait [--max 540]

Never for an untrusted change: it builds and runs the change. Writes <out>/checks.json, checks.md and
one log per step.
"""

import argparse
import base64
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import tomllib
import urllib.request
from pathlib import Path

SECRET_VARS = ("RPC_URL", "SUBSTREAMS_API_TOKEN", "SUBSTREAMS_API_KEY", "TYCHO_API_KEY")


def masker():
    secrets = [v for k, v in os.environ.items() if v and len(v) >= 12 and (k.endswith(SECRET_VARS) or "KEY" in k or "TOKEN" in k)]

    def mask(s: str) -> str:
        s = re.sub(r"https?://[^\s\"']+", "<rpc-url>", s)
        s = re.sub(r"eyJ[A-Za-z0-9._-]{20,}", "<token>", s)
        for v in secrets:
            s = s.replace(v, "<secret>")
        return s
    return mask


MASK = masker()


def jwt_exp(tok: str):
    try:
        p = tok.split(".")[1]
        return json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4))).get("exp")
    except Exception:
        return None


def substreams_token():
    """A JWT for the harness, or (None, reason)."""
    tok = os.environ.get("SUBSTREAMS_API_TOKEN", "")
    if tok.count(".") == 2 and (jwt_exp(tok) or 0) > time.time() + 3600:
        return tok, None
    key = os.environ.get("SUBSTREAMS_API_KEY") or (tok if tok and tok.count(".") != 2 else "")
    if not key:
        return None, "no SUBSTREAMS_API_TOKEN (a JWT) or SUBSTREAMS_API_KEY set" + (" (the JWT has expired)" if tok else "")
    try:
        req = urllib.request.Request("https://auth.streamingfast.io/v1/auth/issue",
                                     data=json.dumps({"api_key": key}).encode(), method="POST")
        with urllib.request.urlopen(req, timeout=30) as r:
            jwt = json.load(r).get("token", "")
    except Exception as e:
        return None, f"StreamingFast rejected the API key: {MASK(str(e))}"
    return (jwt, None) if jwt.count(".") == 2 else (None, "StreamingFast returned no JWT for the API key")


class Run:
    def __init__(self, out: Path, tree: Path):
        self.out, self.tree, self.steps = out, tree, []
        out.mkdir(parents=True, exist_ok=True)

    def save(self, done=False):
        (self.out / "checks.json").write_text(json.dumps(dict(done=done, steps=self.steps), indent=1))
        lines = ["| Step | Result | Minutes | Failures | Log |", "|---|---|---|---|---|"]
        for s in self.steps:
            fl = s.get("failures") or []
            shown = "; ".join(f"`{f['test']}`" + (f" ({f['base']})" if f.get("base") else "") for f in fl[:8])
            note = "; ".join(x for x in (s.get("note"), "rewrote " + ", ".join(f"`{m}`" for m in s["modified"]) + f" (`{s['name']}.diff`)"
                                         if s.get("modified") else "") if x)
            lines.append(f"| {s['name']} | {s['status']}{': ' + note if note else ''} | {s.get('minutes', '')} | "
                         f"{len(fl) and f'{len(fl)}: ' + shown + (' …' if len(fl) > 8 else '') or ''} | `{s.get('log', '')}` |")
        (self.out / "checks.md").write_text("\n".join(lines) + "\n")

    def step(self, name, cmd, cwd, timeout_min=90, env=None, parse=None):
        log = self.out / f"{name}.log"
        t0 = time.time()
        s = dict(name=name, cmd=" ".join(cmd) if isinstance(cmd, list) else cmd, log=log.name, status="running")
        self.steps.append(s)
        self.save()
        try:
            r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout_min * 60,
                               env=env or os.environ, shell=isinstance(cmd, str))
            text, rc = r.stdout + r.stderr, r.returncode
            s["status"] = "pass" if rc == 0 else "fail"
        except subprocess.TimeoutExpired as e:
            text = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
            rc, s["status"], s["note"] = None, "timeout", f"stopped after {timeout_min} min"
        log.write_text(MASK(text))
        s.update(rc=rc, minutes=round((time.time() - t0) / 60, 1))
        changed = [l[3:] for l in subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=self.tree,
                                                capture_output=True, text=True).stdout.splitlines()]
        if changed:  # a step that rewrites tracked files (a test regenerating a fixture) is evidence, then undone
            (self.out / f"{name}.diff").write_text(MASK(subprocess.run(["git", "diff"], cwd=self.tree, capture_output=True,
                                                                        text=True).stdout))
            subprocess.run(["git", "checkout", "--", *changed], cwd=self.tree, capture_output=True)
            s["modified"] = changed
        if parse:
            s["failures"] = parse(text)
            if s["status"] == "fail" and not s["failures"]:
                s["note"] = "failed before any test ran: see the log"
        self.save()
        return s


def nextest_failures(text: str):
    seen, out = set(), []
    for m in re.finditer(r"^\s+FAIL \[[^\]]*\] (?:\(\d+/\d+\) )?(\S+) (\S+)\s*$", text, re.M):
        key = (m.group(1), m.group(2))
        if key not in seen:
            seen.add(key)
            out.append(dict(package=m.group(1), test=m.group(2)))
    return out


ENVIRONMENT = re.compile(r"createSelectFork|forked environment|missing trie node|environment variable `\w+` not found|"
                         r"error sending request|tls handshake|connection (refused|reset)|timed out|rate limit|429")


def forge_failures(text: str):
    out, suite = [], None
    for line in text.splitlines():
        m = re.match(r"Encountered \d+ failing tests? in (\S+)", line)
        if m:
            suite = m.group(1)
            continue
        m = re.match(r"\[FAIL(?:: (.*))?\] (\w+)\(", line)
        if m and suite:
            out.append(dict(suite=suite, test=m.group(2), reason=MASK((m.group(1) or "")[:200])))
    return out


def substreams_failures(pkg):
    return lambda t: [dict(package=pkg, test=m) for m in re.findall(r"^test (\S+) \.\.\. FAILED", t, re.M)]


def init_submodules(tree: Path, path: str):
    """`git submodule update --init` for `path`, then a checkout of any submodule that came back with
    only its `.git` (a shared module dir can leave one empty); returns what is still missing."""
    subprocess.run(["git", "submodule", "update", "--init", "--recursive", path], cwd=tree, capture_output=True)
    st = subprocess.run(["git", "submodule", "status", "--recursive", path], cwd=tree, capture_output=True, text=True).stdout
    missing = []
    for line in st.splitlines():
        sub = tree / line[1:].split()[1]
        if line.startswith("-") or not sub.is_dir():
            missing.append(str(sub.relative_to(tree)))
        elif [x.name for x in sub.iterdir()] == [".git"]:
            subprocess.run(["git", "-C", str(sub), "checkout", "-f", "-q", "HEAD"], capture_output=True)
            if [x.name for x in sub.iterdir()] == [".git"]:
                missing.append(str(sub.relative_to(tree)))
    return missing


def package_of(worktree: Path, path: str):
    """The Cargo package a changed file belongs to, and its workspace root."""
    p = (worktree / path).parent
    while p != worktree and p != p.parent:
        ct = p / "Cargo.toml"
        if ct.exists():
            pkg = tomllib.loads(ct.read_text()).get("package", {}).get("name")
            if pkg:
                ws = worktree / "protocols/substreams" if "protocols/substreams/" in path else worktree
                return pkg, ws
        p = p.parent
    return None, None


def free_port(start=5440):
    for port in range(start, start + 50):
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)):
                return port
    return start


def harness(run: Run, worktree: Path, spec: str, a, short: str):
    protocol, _, case = spec.partition("=")
    name = f"harness-{protocol}" + (f"-{case}" if case else "")
    tok, why = substreams_token()
    if not tok:
        run.steps.append(dict(name=name, status="not run", note=why))
        run.save()
        return
    ctx = worktree
    if a.harness_patch:  # build from a patched copy, leaving the worktree at the head
        ctx = run.out / "harness-context"
        if not ctx.exists():
            subprocess.run(["rsync", "-a", "--exclude=.git", "--exclude=target", "--exclude=.claude", f"{worktree}/", f"{ctx}/"], check=True)
            subprocess.run(["patch", "-p1", "-s", "-i", str(Path(a.harness_patch).resolve())], cwd=ctx, check=True)
    testing = ctx / "protocols/testing"
    project = f"review{short}"
    port = free_port()
    env = dict(os.environ, SUBSTREAMS_API_TOKEN=tok, PROTOCOLS=f"{protocol}={case}" if case else protocol,
               DB_HOST_PORT=str(port), RUST_LOG="protocol_testing=info,tycho_indexer=info,warn")
    override = run.out / "compose.override.yaml"  # this run's own image tags: never the shared `:latest`
    override.write_text(f"services:\n  db:\n    image: protocol-testing-db:{project}\n"
                        f"  test-runner:\n    image: protocol-testing-test-runner:{project}\n")
    compose = ["docker", "compose", "-p", project, "-f", str(testing / "docker-compose.yaml"), "-f", str(override)]
    if not any(s["name"] == "harness-build" for s in run.steps):
        build = run.step("harness-build", compose + ["build"], testing, timeout_min=90,
                         env=dict(env, PROTOCOLS=" ".join(h.partition("=")[0] for h in a.harness)))
        if build["status"] != "pass":
            return
    elif next(s for s in run.steps if s["name"] == "harness-build")["status"] != "pass":
        run.steps.append(dict(name=name, status="not run", note="the harness image did not build"))
        return
    log = run.out / f"{name}.log"
    s = dict(name=name, log=log.name, status="running", cmd=f"PROTOCOLS={env['PROTOCOLS']} docker compose up")
    run.steps.append(s)
    run.save()
    t0 = time.time()
    with open(log, "w") as f:
        p = subprocess.Popen(compose + ["up", "--no-build", "--exit-code-from", "test-runner", "--abort-on-container-exit"],
                             cwd=testing, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        progress = dict(at=time.time(), last=None, fatal=None)

        def pump():  # the log is the watchdog's signal: the indexer's per-minute metrics, then the checks
            for line in p.stdout:
                f.write(MASK(line))
                f.flush()
                m = re.search(r"blocks_processed=(\d+)|Simulated amount|Execution returned|Running Tycho indexer", line)
                if m:
                    progress.update(at=time.time(), last=MASK(line.strip())[-160:])
                if re.search(r"rejected the credential|Unauthenticated|invalid JWT", line):
                    progress["fatal"] = "the Substreams endpoint rejected the credential"
        th = threading.Thread(target=pump, daemon=True)
        th.start()
        while p.poll() is None:
            time.sleep(15)
            if progress["fatal"]:
                s.update(status="not run", note=progress["fatal"])
                break
            if time.time() - progress["at"] > a.stall * 60:
                s.update(status="stalled", note=f"no indexing progress in the log for {a.stall} min")
                break
            if time.time() - t0 > a.harness_timeout * 60:
                s.update(status="timeout", note=f"stopped after {a.harness_timeout} min")
                break
        if p.poll() is None:
            subprocess.run(compose + ["stop"], cwd=testing, env=env, capture_output=True)
            p.wait(timeout=120)
        th.join(timeout=30)
    subprocess.run(compose + ["down", "-v"], cwd=testing, env=env, capture_output=True)
    text = log.read_text(errors="replace")
    if s["status"] == "running":
        s["status"] = "pass" if p.returncode == 0 else "fail"
    passed = re.findall(r"✅ (\S+) passed", text)
    failed = re.findall(r"❌ (\S+)", text)
    s.update(minutes=round((time.time() - t0) / 60, 1), rc=p.returncode, cases_passed=passed,
             failures=[dict(test=t) for t in failed], last_progress=progress["last"])
    run.save()


def compare_base(run: Run, worktree: Path, base: str, cargo_env):
    failing = [s for s in run.steps if s.get("failures") and s["name"].startswith(("nextest", "forge-test", "substreams-test"))]
    if not failing:
        return
    bw = run.out / "base-worktree"
    if not bw.exists():
        subprocess.run(["git", "-C", str(worktree), "worktree", "add", "-q", "--detach", str(bw), base], check=True)
        init_submodules(bw, "crates/tycho-execution/contracts")
    for s in failing:
        names = [f["test"] for f in s["failures"]]
        if s["name"].startswith("forge-test"):
            suites = sorted({f["suite"].split(":")[-1] for f in s["failures"]})
            b = run.step(f"base-{s['name']}", ["forge", "test", "--match-contract", "|".join(f"^{x}$" for x in suites)],
                         bw / "crates/tycho-execution/contracts", parse=forge_failures)
            fails = {(f["suite"].split(":")[-1], f["test"]) for f in b["failures"] or []}
            for f in s["failures"]:
                f["base"] = "also fails at the base" if (f["suite"].split(":")[-1], f["test"]) in fails else "only at the head"
            only = [f for f in s["failures"] if f["base"] == "only at the head"]
            if only:  # once more at the head: a failure that does not repeat is a flake, not evidence
                again = run.step(f"recheck-{s['name']}", ["forge", "test", "--match-contract",
                                 "|".join(sorted({f"^{f['suite'].split(':')[-1]}$" for f in only}))],
                                 worktree / "crates/tycho-execution/contracts", parse=forge_failures)
                still = {(f["suite"].split(":")[-1], f["test"]) for f in again["failures"] or []}
                for f in only:
                    if (f["suite"].split(":")[-1], f["test"]) not in still:
                        f["base"] = "flaky: passed when run again at the head"
            for f in s["failures"]:
                if f["base"] != "also fails at the base" and ENVIRONMENT.search(f.get("reason", "")):
                    f["base"] += "; the error is the environment's (RPC, fork or a missing variable)"
        elif s["name"].startswith("substreams-test"):
            pkg = s["failures"][0]["package"]
            b = run.step(f"base-{s['name']}", ["cargo", "test", "--locked", "-p", pkg], bw / "protocols/substreams",
                         env=cargo_env, parse=substreams_failures(pkg))
            fails = {f["test"] for f in b["failures"] or []}
            for f in s["failures"]:
                f["base"] = ("not at the base, or the base did not build" if b["status"] == "fail" and not fails else
                             "also fails at the base" if f["test"] in fails else "only at the head")
        else:
            pkgs = sorted({f["package"].split("::")[0] for f in s["failures"]})
            filt = " | ".join(f"test(={n})" for n in names)
            b = run.step(f"base-{s['name']}", ["cargo", "nextest", "run", "--no-fail-fast", *sum((["-p", p] for p in pkgs), []), "-E", filt],
                         bw, env=cargo_env, parse=nextest_failures)
            fails = {f["test"] for f in b["failures"] or []}
            for f in s["failures"]:
                f["base"] = ("not at the base, or the base did not build" if b["status"] == "fail" and not fails else
                             "also fails at the base" if f["test"] in fails else "only at the head")
    run.save()
    subprocess.run(["git", "-C", str(worktree), "worktree", "remove", "--force", str(bw)], capture_output=True)



def rust_nightly(worktree):
    """CI's nightly for fmt and clippy: `RUST_NIGHTLY` as mise sets it, else the pin in the worktree's
    `mise.toml`, else plain `nightly`."""
    if os.environ.get("RUST_NIGHTLY"):
        return os.environ["RUST_NIGHTLY"]
    m = re.search(r'^\s*RUST_NIGHTLY\s*=\s*"([^"]+)"', (worktree / "mise.toml").read_text(), re.M) \
        if (worktree / "mise.toml").exists() else None
    return m.group(1) if m else "nightly"

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--plan")
    ap.add_argument("--worktree")
    ap.add_argument("--out", required=True)
    ap.add_argument("--compare-base", action="store_true")
    ap.add_argument("--harness", nargs="+", default=[])
    ap.add_argument("--harness-timeout", type=int, default=180, help="minutes per harness case")
    ap.add_argument("--stall", type=int, default=20, help="minutes without a newly indexed block")
    ap.add_argument("--harness-patch")
    ap.add_argument("--skip", nargs="+", default=[])
    ap.add_argument("--detach", action="store_true")
    ap.add_argument("--wait", action="store_true")
    ap.add_argument("--max", type=int, default=540)
    a = ap.parse_args()
    out = Path(a.out).resolve()
    if a.wait:
        end, f = time.time() + a.max, out / "checks.json"
        while time.time() < end and not (f.exists() and json.loads(f.read_text()).get("done")):
            time.sleep(10)
        state = json.loads(f.read_text()) if f.exists() else {"done": False}
        print(f"done: {state.get('done')}")
        print((out / "checks.md").read_text() if (out / "checks.md").exists() else "no step has started")
        return
    if not (a.plan and a.worktree):
        sys.exit("--plan and --worktree are required")
    if a.detach:
        out.mkdir(parents=True, exist_ok=True)
        subprocess.Popen([sys.executable, "-I", __file__, *[x for x in sys.argv[1:] if x != "--detach"]],
                         stdout=open(out / "runner.log", "w"), stderr=subprocess.STDOUT, start_new_session=True)
        print(f"started; wait with: {Path(__file__).name} --out {a.out} --wait")
        return
    plan = json.loads((Path(a.plan) / "plan.json").read_text())
    worktree = Path(a.worktree).resolve()
    files = list(plan["files"])
    run = Run(out, worktree)
    cargo_env = dict(os.environ, FOUNDRY_DISABLE_NIGHTLY_WARNING="1")
    main_pkgs, sub_pkgs = set(), set()
    for f in files:
        if f.endswith((".rs", "Cargo.toml")) or "/src/" in f:
            pkg, ws = package_of(worktree, f)
            if pkg:
                (sub_pkgs if ws != worktree else main_pkgs).add(pkg)
    sol = [f for f in files if f.startswith("crates/tycho-execution/contracts/") and f.endswith(".sol")]
    contracts = worktree / "crates/tycho-execution/contracts"
    if any(f.startswith("crates/tycho-execution/contracts/") for f in files):
        missing = init_submodules(worktree, "crates/tycho-execution/contracts")
        run.steps.append(dict(name="submodules", status="fail" if missing else "pass",
                              note=("not checked out: " + ", ".join(missing)) if missing else ""))
        run.save()
    want = lambda n: n not in a.skip
    nightly = "+" + rust_nightly(worktree)
    if main_pkgs and want("fmt"):
        run.step("fmt", ["cargo", nightly, "fmt", "--all", "--check"], worktree, timeout_min=10)
    pk = sum((["-p", p] for p in sorted(main_pkgs)), [])
    if main_pkgs and want("clippy"):
        run.step("clippy", ["cargo", nightly, "clippy", "--locked", "--all-features", *pk, "--all-targets", "--", "-D", "warnings"],
                 worktree, env=cargo_env)
    if main_pkgs and want("nextest"):
        run.step("nextest", ["cargo", "nextest", "run", "--locked", "--no-fail-fast", *pk], worktree, env=cargo_env,
                 parse=nextest_failures)
    if sol and want("forge"):
        run.step("forge-fmt", ["forge", "fmt", "--check", *[str(worktree / f) for f in sol]], contracts, timeout_min=5)
        run.step("forge-lint", ["forge", "lint", *[str(worktree / f) for f in sol]], contracts, timeout_min=10)
        if run.step("forge-build", ["forge", "build"], contracts, env=cargo_env)["status"] == "pass":
            run.step("forge-test", ["forge", "test"], contracts, env=cargo_env, parse=forge_failures)
    for pkg in sorted(sub_pkgs):
        if want("substreams"):
            run.step(f"substreams-test-{pkg}", ["cargo", "test", "--locked", "-p", pkg], worktree / "protocols/substreams",
                     env=cargo_env, parse=substreams_failures(pkg))
    if a.compare_base:
        compare_base(run, worktree, plan["base"], cargo_env)
    short = subprocess.run(["git", "-C", str(worktree), "rev-parse", "--short=8", "HEAD"], capture_output=True, text=True).stdout.strip()
    for spec in a.harness:
        harness(run, worktree, spec, a, short)
    if (out / "harness-context").exists():
        shutil.rmtree(out / "harness-context", ignore_errors=True)
    run.save(done=True)
    print((out / "checks.md").read_text())


if __name__ == "__main__":
    main()
