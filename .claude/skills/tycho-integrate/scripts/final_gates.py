#!/usr/bin/env python3
"""The final gates of a tycho-integrate run, and the proxy re-check.

    final_gates.py --protocol P                  run every gate, write <run>/final_gates.json
    final_gates.py --protocol P --recheck        the venue against the spec: proxies, watched code and
                                                 getters, venue quotes (`onchain.py`)

Gates: every layer clean by its stop rules now; no guarded change a person did not accept; all
checks once more (checks.toml, layer `final`: protocol-testing on every chain, and the mutation score
when G10 has landed); one test per spec mutation surface (G5); every proxy's implementation at the
latest block equals the spec's; the venue's quotes as recorded and from the Tycho router; a
simulation test naming each venue quote's block; the watched code and getters unchanged; the
branch contains the base's tip; open sibling PRs that append an executor to TychoRouterTestSetup
(they predict the same test address); the executor's runtime bytecode fixture registered for the
protocol system on every chain, and no `skip_execution`; the package's test yamls strict, with a
balance lens and declared limits. A gate whose harness support has not landed is `off`.
The fresh full review is not run here: the skill runs it through run_agent.py.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envcheck  # noqa: E402
import harness_caps  # noqa: E402
import onchain  # noqa: E402
import schema  # noqa: E402
import state as ledger  # noqa: E402

EIP1967_IMPL_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
EIP1967_BEACON_SLOT = "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50"
IMPLEMENTATION_SELECTOR = "0x5c60da1b"  # implementation(): what a beacon answers
ROUTER_TEST_SETUP = "crates/tycho-execution/contracts/test/TychoRouterTestSetup.sol"
EXECUTION_RS = "protocols/testing/src/execution.rs"
SKIP_EXECUTION = schema.yaml_key_re("skip_execution")


def git(tree: Path, *args) -> str:
    return schema.git("-C", str(tree), *args).stdout


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


def gate_guards(run: Path, tree: Path) -> dict:
    """Every guarded change on the branch, and in the worktree, is one a person accepted. A round's
    guard check covers that round's commits; this covers the whole branch, so an answer of "revert
    it" is checked to have happened."""
    try:
        base = git(tree, "merge-base", f"origin/{schema.BASE}", "HEAD").strip()
    except subprocess.CalledProcessError as e:
        return gate("guards", "fail", f"no merge base with origin/{schema.BASE}: {e}")
    st = ledger.State(ledger.read_events(run))
    open_ = [v for v in ledger.check_guards(tree, base, "HEAD") if ledger.guard_key(v) not in st.accepted_guards]
    if open_:
        return gate("guards", "fail", "guarded changes no person accepted: " + "; ".join(f"{v['path']} ({v['commit']}): {v['why']}" for v in open_[:10]), violations=open_)
    accepted = sorted({p for p, _ in st.accepted_guards})
    return gate("guards", "pass", "no guarded change" + (f" beyond those a person accepted: {accepted}" if accepted else ""))


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


def address_word(word) -> str:
    """The address in a 32-byte word, or None for an empty or zero word."""
    hexpart = (word or "")[2:] if (word or "").startswith("0x") else (word or "")
    if len(hexpart) < 40 or int(hexpart, 16) == 0:
        return None
    return "0x" + hexpart[-40:]


def proxy_drift(spec: dict) -> list:
    """Each spec proxy's implementation at the latest block, compared with the spec."""
    rows = []
    for p in spec.get("proxies", []):
        url = schema.rpc_url(p["chain"])
        if not url:
            rows.append({**p, "status": "unread", "detail": schema.no_rpc(p["chain"])})
            continue
        try:
            slot = (p.get("slot") or (EIP1967_BEACON_SLOT if p.get("beacon") else EIP1967_IMPL_SLOT)).lower()
            word = envcheck.rpc(url, "eth_getStorageAt", [p["proxy"], slot, "latest"])
            if slot == EIP1967_BEACON_SLOT or p.get("beacon"):
                # A beacon proxy's slot holds the beacon, not the implementation: ask the beacon.
                beacon = address_word(word)
                if beacon is None:
                    rows.append({**p, "status": "unread", "detail": f"slot {slot} holds no beacon"})
                    continue
                word = envcheck.rpc(url, "eth_call", [{"to": beacon, "data": IMPLEMENTATION_SELECTOR}, "latest"])
            now = address_word(word)
            if now is None:
                rows.append({**p, "status": "unread", "detail": f"no implementation address read ({word!r})"})
                continue
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


def gate_venue_quotes(spec: dict, tree: Path, facts: set) -> dict:
    """The spec's venue quotes hold at their blocks, and the venue quotes the Tycho router the same
    as anyone, unless the spec states the difference as a fact (`caller_fact`)."""
    rows = onchain.check_quotes(spec, tree)
    if not rows:
        return gate("venue-quotes", "fail", "the spec records no venue quote (`venue_quotes`)")
    bad = [r for r in rows if r["status"] != "same" and not (r["status"] == "caller-dependent" and next(
        (q.get("caller_fact") for q in spec["venue_quotes"] if q.get("id") == r.get("id")), None) in facts)]
    if bad:
        return gate("venue-quotes", "fail", "; ".join(f"{r.get('id')}: {r['status']}: {r.get('detail', '')}" for r in bad[:5]), quotes=rows)
    return gate("venue-quotes", "pass", f"{len(rows)} venue quotes hold at their blocks, from the router too", quotes=rows)


def quote_words(result: str) -> set:
    """The ways a test may write a venue quote's result: each nonzero 32-byte word of the raw
    return, in decimal (as Rust and Solidity tests assert amounts) or as hex digits."""
    hexpart = (result or "")[2:] if (result or "").startswith("0x") else (result or "")
    out = set()
    for i in range(0, len(hexpart), 64):
        word = hexpart[i:i + 64]
        if word and int(word, 16):
            out |= {str(int(word, 16)), f"{int(word, 16):_}", word.lstrip("0").lower()}
    return out


def gate_venue_quote_test(spec: dict, tree: Path, layers: list) -> dict:
    """The simulation layer tests the implementation against the venue's own quotes, to the wei
    (`implement.md`, venue-quote test): a test file the branch changes names each quote's block and
    asserts its result. Exact simulation-vs-execution cannot show an under-quote: the router keeps
    the surplus. A textual check: it finds the test, a reviewer reads it."""
    if "simulation" not in layers:
        return gate("venue-quote-test", "n/a", "the route builds no simulation layer")
    quotes = spec.get("venue_quotes", [])
    if not quotes:
        return gate("venue-quote-test", "fail", "the spec records no venue quote to test against")
    try:
        changed = git(tree, "diff", "--name-only", f"origin/{schema.BASE}...HEAD").split()
    except subprocess.CalledProcessError as e:
        return gate("venue-quote-test", "fail", f"cannot diff against origin/{schema.BASE}: {e}")
    texts = [(tree / p).read_text(errors="replace") for p in changed
             if p.startswith(("crates/tycho-simulation/", "protocols/adapter-integration/")) and (tree / p).is_file()]
    tests = "\n".join(t for t in texts if re.search(r"#\[(tokio::)?test\b|function\s+test", t))
    missing = []
    for q in quotes:
        block = str(q["block"])
        if not (block in tests or f"{int(block):_}" in tests) or not any(w in tests.lower() for w in quote_words(q.get("result", ""))):
            missing.append(q.get("id") or block)
    if missing:
        return gate("venue-quote-test", "fail", f"no simulation test names the block and asserts the result of venue quotes {missing}")
    return gate("venue-quote-test", "pass", f"simulation tests name the block and assert the result of all {len(quotes)} venue quotes")


def gate_watch(spec: dict) -> dict:
    """Each watched contract's code and getter on the quote path is as it was at the spec's block."""
    rows = onchain.check_watch(spec)
    if not rows:
        return gate("watch", "fail", "the spec watches nothing (`watch`): list the quote path's contracts and config getters")
    bad = [r for r in rows if r["status"] != "same"]
    if bad:
        return gate("watch", "fail", "changed or unread since the spec: a spec amendment reruns the affected layers; "
                    + "; ".join(f"{r['address']} {r.get('what') or r.get('data') or 'code'}: {r['status']} {r.get('detail', '')}" for r in bad[:5]), watch=rows)
    return gate("watch", "pass", f"{len(rows)} watched contracts and getters unchanged since the spec", watch=rows)


def gate_base_current(tree: Path) -> dict:
    """The run pins its base at the worktree's creation; main moves on while the layers are built.
    The gates before Gate 3 must run on a branch that contains the base's tip, or what merges is
    untested: stale router slots and removed helpers only show after a rebase. Behind by any
    commit, or not comparable (a failed fetch), fails: the files both sides changed only say where
    to look first."""
    base = f"origin/{schema.BASE}"
    try:
        git(tree, "fetch", "origin", schema.BASE)
        behind = int(git(tree, "rev-list", "--count", f"HEAD..{base}").strip())
        if behind == 0:
            return gate("base-current", "pass", f"the branch contains {base}")
        mine = set(git(tree, "diff", "--name-only", f"{base}...HEAD").split())
        theirs = set(git(tree, "diff", "--name-only", f"HEAD...{base}").split())
    except (subprocess.CalledProcessError, ValueError) as e:
        return gate("base-current", "fail", f"could not compare with {base}: {e}")
    overlap = sorted(mine & theirs)
    detail = (f"{base} is {behind} commits ahead of the branch; "
              + (f"{len(overlap)} files changed on both sides (e.g. {overlap[0]})" if overlap else "no file changed on both sides")
              + ": rebase onto it, rerun the layer checks and these gates before Gate 3")
    return gate("base-current", "fail", detail, behind=behind, overlap=overlap[:20])


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


def gate_executor_fixture(tree: Path, system: str, chains: list, yamls: list) -> dict:
    """On every chain of the run, protocol-testing executes through the executor's runtime
    bytecode, planted at the test address, so execution is tested before the executor is deployed;
    a missing deployment is never a reason to skip it (`integration-test-skip-flags`)."""
    if not system:
        return gate("executor-fixture", "fail", "decisions.json names no protocol_system")
    skips = [str(p.relative_to(tree)) for p in yamls if SKIP_EXECUTION.search(p.read_text())]
    if skips:
        return gate("executor-fixture", "fail", f"skip_execution in {skips}: execution runs through the fixture; a skip needs a person's decision with hard evidence")
    problems, found = [], []
    for chain in chains:
        fixture = executor_fixture(tree, system, chain)
        if fixture is None:
            problems.append(f"{system} on {chain} has no executor fixture in {EXECUTION_RS} (EXECUTOR_MAPPING or CHAIN_SPECIFIC_EXECUTORS)")
        elif not (tree / fixture).exists():
            problems.append(f"{EXECUTION_RS} maps {system} on {chain} to {fixture}, which does not exist")
        else:
            found.append(f"{chain}: {fixture}")
    if problems:
        return gate("executor-fixture", "fail", "; ".join(problems))
    return gate("executor-fixture", "pass", f"{system} executes through its fixture on " + ", ".join(found))


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

    schema.load_env_files()  # the proxy re-check reads the chain's RPC
    run = schema.run_dir(args.protocol)
    tree = schema.worktree_dir(args.protocol)
    spec = schema.load_json(run / "spec.json")
    dec = schema.load_json(run / "decisions.json") if (run / "decisions.json").exists() else {}
    _, chains = schema.run_chains(run)
    system = (dec.get("agreements") or {}).get("protocol_system", "")
    # Every chain's package and test package: a clone's yaml sits under its own name.
    names = set()
    for key in ("package", "test_package"):
        v = (dec.get("paths") or {}).get(key)
        names.update(v.values() if isinstance(v, dict) else [v] if v else [])
    yamls = sorted({y for name in names for y in integration_yamls(tree, name)} or set(integration_yamls(tree, "")))

    if args.recheck:
        # After the PR opens (and after merge, until the integration is retired): the venue as the
        # spec describes it, re-read.
        rows = {"proxies": proxy_drift(spec), "watch": onchain.check_watch(spec), "quotes": onchain.check_quotes(spec, tree)}
        print(json.dumps(rows, indent=2))
        sys.exit(1 if any(r["status"] != "same" for group in rows.values() for r in group) else 0)

    caps = harness_caps.detect(tree)

    gates = [
        gate_layers_clean(run),
        gate_guards(run, tree),
        gate_checks(args.protocol, args.round),
        gate_mutation_surfaces(spec, yamls, caps),
        gate_proxies(spec),
        gate_venue_quotes(spec, tree, {f["id"] for f in spec.get("facts", [])}),
        gate_venue_quote_test(spec, tree, ledger.route_layers(run)),
        gate_watch(spec),
        gate_base_current(tree),
        gate_sibling_executors(tree),
        gate_executor_fixture(tree, system, chains, yamls),
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
