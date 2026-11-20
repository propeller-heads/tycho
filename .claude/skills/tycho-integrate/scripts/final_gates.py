#!/usr/bin/env python3
"""The final gates of a tycho-integrate run, and the proxy re-check.

    final_gates.py --protocol P                  run every gate, write <run>/final_gates.json
    final_gates.py --protocol P --recheck        proxies only: implementation slots vs the spec

Gates: every layer stopped clean; all checks once more (checks.toml, layer `final`, which adds the
mutation score when G10 has landed); one test per spec mutation surface (G5); every proxy's
implementation at the latest block equals the spec's; the branch contains the base's tip (the run's
base moves while it builds); open sibling PRs that append an executor to
TychoRouterTestSetup (they predict the same test address); the executor's runtime bytecode fixture
registered for the protocol system, so protocol-testing executes through it before deployment,
and no `skip_execution`; the package's test yamls strict, with a balance lens and declared limits.
A gate whose harness support has not landed is `off`.
The fresh full review is not run here: the skill runs it through run_agent.py.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envcheck  # noqa: E402
import harness_caps  # noqa: E402
import schema  # noqa: E402
import state as ledger  # noqa: E402

EIP1967_IMPL_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
EIP1967_BEACON_SLOT = "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50"
IMPLEMENTATION_SELECTOR = "0x5c60da1b"  # implementation(): what a beacon answers
ROUTER_TEST_SETUP = "crates/tycho-execution/contracts/test/TychoRouterTestSetup.sol"
EXECUTION_RS = "protocols/testing/src/execution.rs"


def git(tree: Path, *args) -> str:
    return subprocess.run(["git", "-C", str(tree), *args], capture_output=True, text=True, check=True).stdout


def integration_yamls(tree: Path, package: str) -> list:
    """The package's integration test yamls. A package is not always a top-level directory: a nested
    one (`ethereum-uniswap-v4/with-hooks`, cargo package `ethereum-uniswap-v4-with-hooks`) keeps its
    yaml beside its manifest, and a clone's yaml sits in its base package's directory. So the
    candidates are the package's own directory, every directory whose path with `/` as `-` is the
    package, and, when none of those holds a yaml, the yamls the branch itself adds or changes."""
    root = tree / "protocols/substreams"
    found = set()
    if package:
        found.update((root / package).glob("integration_test*.tycho.yaml"))
        for path in root.rglob("integration_test*.tycho.yaml"):
            rel = path.parent.relative_to(root)
            if "target" not in rel.parts and str(rel).replace("/", "-") == package:
                found.add(path)
    if found:
        return sorted(found)
    try:
        changed = git(tree, "diff", "--name-only", f"origin/{schema.BASE}...HEAD").split()
    except subprocess.CalledProcessError:
        changed = []
    for rel in changed:
        if rel.startswith("protocols/substreams/") and schema.INTEGRATION_YAML_RE.search(rel) and (tree / rel).exists():
            found.add(tree / rel)
    return sorted(found)


def gate(name: str, status: str, detail: str, **extra) -> dict:
    return {"name": name, "status": status, "detail": detail, **extra}


# ---------------------------------------------------------------------------------------------


def gate_layers_clean(run: Path) -> dict:
    """Each layer's stop rules now, not its last recorded stop: a finding from the final review, an
    amendment or a red round after a clean stop reopens the layer."""
    st = ledger.State(ledger.read_events(run))
    layers = ledger.route_layers(run)
    now = {l: ledger.next_step(st, l) for l in layers}
    # A layer a person closed (a cap or no-progress answer that grants nothing) counts as done.
    not_clean = {l: r["reason"] for l, r in now.items() if r["action"] not in ("clean", "closed")}
    if not_clean:
        return gate("layers-clean", "fail", f"layers not clean: {not_clean}")
    closed = {l: st.closed[l] for l, r in now.items() if r["action"] == "closed"}
    note = f"; closed by a person's decision: {closed}" if closed else ""
    return gate("layers-clean", "pass", f"{', '.join(layers)} clean{note}")


def gate_checks(protocol: str, n: int) -> dict:
    proc = subprocess.run([sys.executable, "-I", str(Path(__file__).parent / "layer_checks.py"), "--protocol", protocol, "--layer", "final", "--round", str(n)], capture_output=True, text=True)
    last = proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else "{}"
    try:
        info = json.loads(last)
    except json.JSONDecodeError:
        info = {"green": False, "file": None}
    green = bool(info.get("green"))
    lines = [l for l in proc.stdout.strip().splitlines()[:-1] if l]
    failing = [l for l in lines if not l.startswith(("pass", "n/a", "off"))]
    # Lead with what failed, so a one-line summary never shows only the first passing step.
    detail = "; ".join(failing) if failing else ("every step passed" if green else proc.stdout.strip()[-2000:])
    return gate("checks", "pass" if green else "fail", detail, steps=lines, file=info.get("file"))


def gate_mutation_surfaces(spec: dict, yamls: list, caps: dict) -> dict:
    """Each spec mutation surface has a checkpoint in the integration yaml, marked `# surface: <id>`."""
    if not caps["G5"]["on"]:
        return gate("mutation-surface-tests", "off", caps["G5"]["reason"])
    text = "\n".join(p.read_text() for p in yamls)
    marked = set(re.findall(r"#\s*surface:\s*([A-Za-z0-9_-]+)", text))
    missing = [m["id"] for m in spec.get("mutation_surfaces", []) if m["id"] not in marked]
    if missing:
        return gate("mutation-surface-tests", "fail", f"no checkpoint marked `# surface: <id>` for {missing}")
    return gate("mutation-surface-tests", "pass", f"{len(spec.get('mutation_surfaces', []))} surfaces each have a checkpoint")


def proxy_drift(spec: dict) -> list:
    """Each spec proxy's implementation at the latest block, compared with the spec."""
    rows = []
    for p in spec.get("proxies", []):
        names = envcheck.rpc_var(p["chain"])
        url = next((v for v in (os.environ.get(n) for n in names) if v), None)
        if not url:
            rows.append({**p, "status": "unread", "detail": f"no {' or '.join(names)}"})
            continue
        try:
            slot = (p.get("slot") or EIP1967_IMPL_SLOT).lower()
            word = envcheck.rpc(url, "eth_getStorageAt", [p["proxy"], slot, "latest"])
            if slot == EIP1967_BEACON_SLOT or p.get("beacon"):
                # A beacon proxy's slot holds the beacon, not the implementation: ask the beacon.
                beacon = "0x" + word[-40:]
                word = envcheck.rpc(url, "eth_call", [{"to": beacon, "data": IMPLEMENTATION_SELECTOR}, "latest"])
            now = "0x" + word[-40:]
            same = now.lower() == p["implementation"].lower()
            rows.append({**p, "now": now, "status": "same" if same else "upgraded"})
        except Exception as e:  # noqa: BLE001 - reported per proxy
            rows.append({**p, "status": "unread", "detail": str(e)})
    return rows


def gate_proxies(spec: dict) -> dict:
    rows = proxy_drift(spec)
    upgraded = [r for r in rows if r["status"] == "upgraded"]
    unread = [r for r in rows if r["status"] == "unread"]
    if upgraded:
        return gate("proxies", "fail", "upgraded since the spec: a spec amendment reruns the affected layers", proxies=rows)
    if unread:
        return gate("proxies", "fail", "could not read every implementation slot", proxies=rows)
    return gate("proxies", "pass", f"{len(rows)} proxies unchanged", proxies=rows)


def gate_base_current(tree: Path) -> dict:
    """The run pins its base at the worktree's creation; main moves
    on while the layers are built, and a squash-merge of the pinned base leaves it unreachable. The
    gates before Gate 3 must run on a branch that contains the base's tip, or what merges is
    untested: stale router slots and removed helpers only show after a rebase."""
    base = f"origin/{schema.BASE}"
    try:
        git(tree, "fetch", "origin", schema.BASE)
        behind = int(git(tree, "rev-list", "--count", f"HEAD..{base}").strip())
        if behind == 0:
            return gate("base-current", "pass", f"the branch contains {base}")
        mine = set(git(tree, "diff", "--name-only", f"{base}...HEAD").split())
        theirs = set(git(tree, "diff", "--name-only", f"HEAD...{base}").split())
    except (subprocess.CalledProcessError, ValueError) as e:
        return gate("base-current", "warn", f"could not compare with {base}: {e}")
    overlap = sorted(mine & theirs)
    detail = (f"{base} is {behind} commits ahead of the branch; "
              + (f"{len(overlap)} files changed on both sides (e.g. {overlap[0]})" if overlap else "no file changed on both sides")
              + ": rebase onto it, rerun the layer checks and these gates before Gate 3")
    return gate("base-current", "fail" if overlap else "warn", detail, behind=behind, overlap=overlap[:20])


def gate_sibling_executors(tree: Path) -> dict:
    changed = git(tree, "diff", "--name-only", f"origin/{schema.BASE}...HEAD").split()
    if ROUTER_TEST_SETUP not in changed:
        return gate("sibling-executor-address", "n/a", "TychoRouterTestSetup unchanged")
    try:
        out = subprocess.run(["gh", "pr", "list", "--state", "open", "--limit", "200", "--json", "number,title,files"], capture_output=True, text=True, check=True, cwd=tree).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        return gate("sibling-executor-address", "warn", f"could not list open PRs: {e}")
    clashes = [f"#{p['number']} {p['title']}" for p in json.loads(out) if any(f.get("path") == ROUTER_TEST_SETUP for f in p.get("files", []))]
    if clashes:
        return gate("sibling-executor-address", "warn", "open PRs also append to TychoRouterTestSetup and may predict the same test executor address; whichever merges second re-derives it and regenerates calldata.txt", prs=clashes)
    return gate("sibling-executor-address", "pass", "no open PR changes TychoRouterTestSetup")


def executor_fixture(tree: Path, protocol_system: str, chain: str):
    """The fixture file protocol-testing loads for the protocol system on the chain, from
    `EXECUTOR_MAPPING` or `CHAIN_SPECIFIC_EXECUTORS`, or None when it has none."""
    path = tree / EXECUTION_RS
    if not path.exists():
        return None
    text = path.read_text()
    variant = "".join(w.capitalize() for w in chain.split("_"))
    m = (re.search(rf'\(Chain::{variant}, "{re.escape(protocol_system)}"\),\s*([A-Z0-9_]+)', text)
         or re.search(rf'map\.insert\("{re.escape(protocol_system)}",\s*([A-Z0-9_]+)\)', text))
    if not m:
        return None
    const = re.search(rf'const {m.group(1)}: &str\s*=\s*include_str!\("\.\./fixtures/([^"]+)"\)', text)
    return f"protocols/testing/fixtures/{const.group(1)}" if const else None


def gate_executor_fixture(tree: Path, values: dict, yamls: list) -> dict:
    """protocol-testing executes through the executor's runtime bytecode, planted at the test
    address, so execution is tested before the executor is deployed; a missing deployment is never
    a reason to skip it (`integration-test-skip-flags`)."""
    system, chain = values.get("protocol_system", ""), values.get("chain", "ethereum")
    if not system:
        return gate("executor-fixture", "fail", "decisions.json names no protocol_system")
    skips = [str(p.relative_to(tree)) for p in yamls if re.search(r"^\s*skip_execution\s*:", p.read_text(), re.M)]
    if skips:
        return gate("executor-fixture", "fail", f"skip_execution in {skips}: execution runs through the fixture; a skip needs a person's decision with hard evidence")
    fixture = executor_fixture(tree, system, chain)
    if fixture is None:
        return gate("executor-fixture", "fail", f"{system} on {chain} has no executor fixture in {EXECUTION_RS} (EXECUTOR_MAPPING or CHAIN_SPECIFIC_EXECUTORS)")
    if not (tree / fixture).exists():
        return gate("executor-fixture", "fail", f"{EXECUTION_RS} maps {system} to {fixture}, which does not exist")
    return gate("executor-fixture", "pass", f"{system} on {chain} executes through {fixture}")


def gate_strict_yamls(yamls: list, tree: Path) -> dict:
    """Every test yaml of the package is strict, reads balances through a lens and declares limits
    (`protocols/testing/README.md`, "Strict mode"); the protocol-testing run enforces the rest."""
    if not yamls:
        return gate("strict-yamls", "fail", "the package has no integration test yaml")
    problems = []
    for p in yamls:
        text = p.read_text()
        rel = str(p.relative_to(tree))
        if not re.search(r"^harness:\s*strict\s*$", text, re.M):
            problems.append(f"{rel}: not `harness: strict`")
        if not re.search(r"^balance_lens:", text, re.M):
            problems.append(f"{rel}: no `balance_lens`")
        if not re.search(r"^\s*limits:", text, re.M):
            problems.append(f"{rel}: no `limits` declared")
    if problems:
        return gate("strict-yamls", "fail", "; ".join(problems))
    return gate("strict-yamls", "pass", f"{len(yamls)} strict yamls with a balance lens and declared limits")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--recheck", action="store_true")
    p.add_argument("--round", type=int, default=1, help="the final-gates attempt, for the checks' output directory")
    args = p.parse_args()

    envcheck.load_env_files()  # the proxy re-check reads the chain's RPC
    run = schema.run_dir(args.protocol)
    tree = schema.worktree_dir(args.protocol)
    spec = schema.load_json(run / "spec.json")
    dec = schema.load_json(run / "decisions.json") if (run / "decisions.json").exists() else {}
    values = {**(dec.get("paths") or {}), "protocol_system": (dec.get("agreements") or {}).get("protocol_system", ""), "chain": (spec.get("chains") or ["ethereum"])[0]}
    yamls = integration_yamls(tree, values.get("package", ""))

    if args.recheck:
        rows = proxy_drift(spec)
        print(json.dumps(rows, indent=2))
        sys.exit(1 if any(r["status"] != "same" for r in rows) else 0)

    caps = harness_caps.detect(tree)

    gates = [
        gate_layers_clean(run),
        gate_checks(args.protocol, args.round),
        gate_mutation_surfaces(spec, yamls, caps),
        gate_proxies(spec),
        gate_base_current(tree),
        gate_sibling_executors(tree),
        gate_executor_fixture(tree, values, yamls),
        gate_strict_yamls(yamls, tree),
    ]
    off = [{"gap": g, **info} for g, info in caps.items() if not info["on"]]
    report = {"head": git(tree, "rev-parse", "HEAD").strip(), "gates": gates, "off": off, "passed": all(g["status"] in ("pass", "n/a", "off", "warn") for g in gates)}
    schema.write_json(run / "final_gates.json", report)
    for g in gates:
        print(f"{g['status']:5} {g['name']}: {g['detail'][:300]}")
    print(json.dumps({"passed": report["passed"], "file": str(run / "final_gates.json")}))
    sys.exit(0 if report["passed"] else 1)


if __name__ == "__main__":
    main()
