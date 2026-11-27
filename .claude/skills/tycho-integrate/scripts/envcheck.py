#!/usr/bin/env python3
"""Checks what a tycho-integrate run needs before any work starts, and lists what is missing.

    envcheck.py --chains ethereum,base [--contract 0x...] [--credentials FOO_API_KEY,...]
    envcheck.py --protocol <name>        reads chains, anchor contracts and credentials from intake.json

Prints one line per check and, last, a JSON summary. Exits 1 when a required check fails; the
skill then asks the person for exactly the failing items.

`debug_storageRangeAt` is not required: it decides whether DCI is available for the integration on
each chain (`dci-only-where-rpc-serves-storage-range` in `.claude/knowledge/indexing.md`: DCI
fetches every contract it finds with it, and so does the harness for `initialized_accounts`).
The summary's `chains` gives the result per chain; with `--protocol`, it is also written to
`<run>/env.json`, which the briefs read.
"""

import argparse
import base64
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

# A run's worktree builds the indexer, the substreams WASM and the test crates in release mode,
# about 30 GB, and the harness's Postgres grows with the range.
MIN_FREE_GB = 40
HARNESS_PORTS = (5431, 4242)  # protocol-testing's Postgres and indexer defaults
TOOLS = ("cargo", "forge", "cast", "anvil", "substreams", "docker", "gh", "claude", "git")


def rpc(url: str, method: str, params: list, timeout=20):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        data = json.loads(resp.read())
    if "error" in data:
        raise RuntimeError(data["error"].get("message", str(data["error"])))
    return data.get("result")


def dci_check_name(chain: str) -> str:
    """The name of the chain's DCI line in the results (`debug_storageRangeAt` served or not)."""
    return f"dci:{chain}"


def check_rpc(chain: str, contract) -> list:
    """An archive RPC: a historical state read and a trace call (required), and
    `debug_storageRangeAt` (informational: whether DCI is available on the chain; reth returns null,
    some Geth and Nitro nodes fail)."""
    results = []
    url = schema.rpc_url(chain)
    if not url:
        return [("rpc:" + chain, False, schema.no_rpc(chain))]
    try:
        head = int(rpc(url, "eth_blockNumber", []), 16)
        # About a year back on Ethereum (12 s blocks): a full node prunes state long before that, so
        # an answer means the node keeps history.
        old = max(head - 2_000_000, 1)
        rpc(url, "eth_getBalance", ["0x0000000000000000000000000000000000000000", hex(old)])
        results.append(("rpc:" + chain + ":historical", True, f"state at block {old} served"))
    except Exception as e:  # noqa: BLE001 - every failure is reported, none is fatal here
        return [("rpc:" + chain + ":historical", False, f"historical eth_getBalance failed: {e}")]
    try:
        rpc(url, "debug_traceCall", [{"to": "0x0000000000000000000000000000000000000000", "data": "0x"}, hex(head), {"tracer": "callTracer"}])
        results.append(("rpc:" + chain + ":trace", True, "debug_traceCall served"))
    except Exception as e:  # noqa: BLE001
        results.append(("rpc:" + chain + ":trace", False, f"debug_traceCall failed: {e}"))
    try:
        block = rpc(url, "eth_getBlockByNumber", [hex(head - 10), False])
        target = contract or "0x0000000000000000000000000000000000000000"
        res = rpc(url, "debug_storageRangeAt", [block["hash"], 0, target, "0x" + "00" * 32, 1])
        ok = res is not None
        results.append((dci_check_name(chain), ok, "debug_storageRangeAt served: DCI available" if ok else "debug_storageRangeAt returned null: no DCI on this chain"))
    except Exception as e:  # noqa: BLE001
        results.append((dci_check_name(chain), False, f"debug_storageRangeAt failed ({e}): no DCI on this chain"))
    return results


def check_substreams_token(hours: float = 6) -> tuple:
    """Whether SUBSTREAMS_API_TOKEN is set and, when it is a JWT with an expiry, still valid for
    `hours` (a run's start asks for 6; a harness run for its own timeout). An opaque token is
    taken as valid: the server is the only judge of it."""
    tok = os.environ.get("SUBSTREAMS_API_TOKEN", "")
    if not tok:
        return ("substreams-token", False, "set SUBSTREAMS_API_TOKEN")
    try:
        payload = tok.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        exp = json.loads(base64.urlsafe_b64decode(payload)).get("exp")
        if exp and exp < time.time() + hours * 3600:
            return ("substreams-token", False, f"SUBSTREAMS_API_TOKEN expires within {hours:g} hours; renew it")
    except Exception:  # noqa: BLE001 - an opaque token is fine
        pass
    return ("substreams-token", True, "set")


def port_free(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.5)
        return s.connect_ex(("127.0.0.1", port)) != 0


def run_ok(cmd: list) -> tuple:
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
        return p.returncode == 0, (p.stdout or p.stderr).strip().splitlines()[:1]
    except (OSError, subprocess.TimeoutExpired) as e:
        return False, [str(e)]


def check_permissions() -> tuple:
    """The main session runs the scripts unattended: auto mode, or an allowlist entry for them."""
    paths = [Path.home() / ".claude/settings.json", schema.repo_root() / ".claude/settings.json", schema.repo_root() / ".claude/settings.local.json"]
    for path in paths:
        if not path.exists():
            continue
        try:
            data = json.loads(path.read_text())
        except json.JSONDecodeError:
            continue
        perms = data.get("permissions", {})
        if perms.get("defaultMode") in ("auto", "bypassPermissions"):
            return ("permissions", True, f"{perms['defaultMode']} in {path}")
        if any("tycho-integrate" in a for a in perms.get("allow", [])):
            return ("permissions", True, f"allowlist in {path}")
    return ("permissions", False, "run the session in auto mode, or allow `Bash(python3 -I .claude/skills/tycho-integrate/scripts/*)` and `Bash(claude -p:*)`")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol")
    p.add_argument("--chains", default="")
    p.add_argument("--contract", help="an anchor contract, for the storageRangeAt probe")
    p.add_argument("--credentials", default="", help="comma-separated env vars the venue needs")
    args = p.parse_args()

    chains = [c for c in args.chains.split(",") if c]
    contracts = [args.contract] if args.contract else []
    creds = [c for c in args.credentials.split(",") if c]
    if args.protocol:
        intake_path = schema.run_dir(args.protocol) / "intake.json"
        if intake_path.exists():
            intake = schema.load_json(intake_path)
            errors = schema.validate_intake(intake)
            if errors:
                raise SystemExit("intake.json: " + "; ".join(errors))
            chains = chains or intake.get("chains", [])
            contracts = contracts or (intake.get("anchor", {}).get("contracts") or [])
            creds = creds or (intake.get("offchain", {}).get("credentials_env") or [])
    if not chains:
        raise SystemExit("no chains: pass --chains or write intake.json first")

    schema.load_env_files()
    checks = []
    for chain in chains:
        checks += check_rpc(chain, contracts[0] if contracts else None)
    checks.append(check_substreams_token())
    for tool in TOOLS:
        checks.append(("tool:" + tool, shutil.which(tool) is not None, shutil.which(tool) or f"install {tool} (`mise install` covers most)"))
    if (schema.repo_root() / "sgconfig.yml").exists():
        checks.append(("tool:ast-grep", shutil.which("ast-grep") is not None, shutil.which("ast-grep") or "install ast-grep: the repo has ast-grep rules"))
    nightly = os.environ.get("RUST_NIGHTLY")
    if nightly:
        ok, out = run_ok(["cargo", f"+{nightly}", "--version"])
        checks.append(("rust-nightly", ok, out[0] if out else f"rustup toolchain install {nightly}"))
    else:
        checks.append(("rust-nightly", False, "RUST_NIGHTLY is unset; activate mise (`mise install`, then a mise shell)"))
    ok, out = run_ok(["docker", "info", "--format", "{{.ServerVersion}}"])
    checks.append(("docker", ok, f"daemon {out[0]}" if ok and out else "start the Docker daemon"))
    for port in HARNESS_PORTS:
        free = port_free(port)
        checks.append((f"port:{port}", free, "free" if free else f"port {port} is in use; the harness run picks another, but stop stray tycho stacks first"))
    ok, out = run_ok(["gh", "auth", "status"])
    checks.append(("gh-auth", ok, "authenticated" if ok else "run `gh auth login` (needed for the final push and PR)"))
    free_gb = shutil.disk_usage(schema.repo_root()).free / 1e9
    checks.append(("disk", free_gb >= MIN_FREE_GB, f"{free_gb:.0f} GB free (need {MIN_FREE_GB})"))
    for cred in creds:
        checks.append(("credential:" + cred, bool(os.environ.get(cred)), "set" if os.environ.get(cred) else f"set {cred}"))
    checks.append(check_permissions())

    # A busy port is not fatal: the harness runs pick free ports themselves. A node without
    # `debug_storageRangeAt` is not either: it means no DCI on that chain.
    advisory = {f"port:{p}" for p in HARNESS_PORTS}
    info = {dci_check_name(c) for c in chains}
    for name, ok, detail in checks:
        mark = "ok  " if ok else ("info" if name in info else "warn" if name in advisory else "MISS")
        print(f"{mark} {name}: {detail}")
    missing = [{"check": n, "fix": d} for n, ok, d in checks if not ok and n not in advisory | info]
    by_name = {n: (ok, d) for n, ok, d in checks}
    chain_caps = {
        c: {"dci": by_name[dci_check_name(c)][0], "detail": by_name[dci_check_name(c)][1]}
        for c in chains if dci_check_name(c) in by_name
    }
    if args.protocol and schema.run_dir(args.protocol).exists():
        schema.write_json(schema.run_dir(args.protocol) / "env.json", {"chains": chain_caps})
    print(json.dumps({"ok": not missing, "missing": missing, "chains": chain_caps}))
    sys.exit(0 if not missing else 1)


if __name__ == "__main__":
    main()
