#!/usr/bin/env python3
"""Writes the PR description of a tycho-integrate run from its ledger, decisions and final gates.

    pr_body.py --protocol P [--out <file>]     (default <run>/PR.md)

What it carries: the layers, the decision record and cross-layer agreements, a ledger summary per
layer, the final gates (with the gates that were off and what turns them on), and
what is left for a person, rollout included. It follows `.claude/knowledge/version_control.md`
§ Pull requests: what the code does now, plain language, no narrated alternatives. The session
appends its attribution lines when it opens the PR. The per-call spend goes to <run>/PR-spend.md, not
the description, and `STALE:` lines on stderr name what the text refers to that the branch no longer has.
"""

import argparse
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402
import state as ledger  # noqa: E402

LAYER_TITLES = {"indexing": "Indexing (Substreams)", "simulation": "Simulation", "execution": "Execution"}


def maybe(path: Path, default=None):
    return schema.load_json(path) if path.exists() else default


def layer_summary(st: ledger.State, layer: str) -> str:
    fs = [f for f in st.findings.values() if f["layer"] == layer]
    by_status = Counter(f["status"] for f in fs)
    rounds = len(st.rounds[layer])
    return (
        f"| {LAYER_TITLES[layer]} | {rounds} | {st.stops.get(layer, '-')} | {len(fs)} | "
        f"{by_status.get('fixed', 0)} | {by_status.get('refuted', 0)} | "
        f"{sum(1 for d in st.decisions.values() if d.get('layer') == layer)} | ${st.spend_layer[layer]:.2f} |"
    )


def rollout(values: dict, spec: dict, new_executor: bool) -> list:
    chains = ", ".join(spec.get("chains", []))
    items = [
        f"Release the `{values.get('package', '<package>')}` spkg (`protocols/substreams/release.sh`) and add its `extractors.yaml` entry.",
    ]
    if new_executor:
        items += [
            f"Deploy the executor on {chains} and register it with TychoRouter (timelocked role). Until then protocol-testing executes through its runtime bytecode fixture.",
            "Add the deployed address to `crates/tycho-execution/config/executor_addresses.json` after the deployment.",
        ]
    return items


def render(protocol: str, shadow: bool = False) -> tuple:
    """(description, held back): in shadow mode the decisions and the review ledger are held back
    until the person's blind review is done, then posted as a comment."""
    run = schema.run_dir(protocol)
    st = ledger.State(ledger.read_events(run))
    intake = maybe(run / "intake.json", {})
    spec = maybe(run / "spec.json", {})
    dec = maybe(run / "decisions.json", {})
    final = maybe(run / "final_gates.json", {"gates": [], "off": []})
    layers = ledger.route_layers(run)
    agreements = dec.get("agreements") or {}
    values = {**(dec.get("paths") or {}), "protocol_system": agreements.get("protocol_system")}
    route = intake.get("route") or {"kind": "new"}
    new_executor = "execution" in layers

    kind = spec.get("kind", "?")
    open_findings = [f for l in layers for f in st.open_findings(l)]
    blocking = [f for f in open_findings if f["severity"] == "blocking"]
    closed = {l: st.closed[l] for l in layers if l in st.closed}
    out = [
        f"Integrates {spec.get('protocol', protocol)} ({kind}) on {', '.join(spec.get('chains', []))} as "
        f"`{agreements.get('protocol_system', '?')}`. "
        + ("This PR carries every layer: " if route.get("kind") == "new" else f"A {route['kind']} of `{route.get('sibling')}`; this PR carries: ")
        + ", ".join(LAYER_TITLES[l].lower() for l in layers) + ".",
        "",
        ("Written by the tycho-integrate pipeline: the implementer and `/tycho-review` resolved every "
         "review finding before this PR was opened." if not open_findings else
         f"Written by the tycho-integrate pipeline. {len(open_findings)} review findings are still open"
         + (f", {len(blocking)} of them blocking" if blocking else "")
         + (f" (layers a person closed at a cap: {', '.join(closed)})" if closed else "")
         + "; they are listed under \"For a person\"."),
        "",
    ]
    out += [
        "## Cross-layer agreements",
        "",
        f"- `protocol_system`: `{agreements.get('protocol_system')}`",
        f"- component id: {agreements.get('component_id')}",
    ]
    if agreements.get("adapter_pool_id"):
        out.append(f"- adapter `poolId`: {agreements['adapter_pool_id']}")
    for a in agreements.get("attributes", []):
        out.append(f"- attribute `{a.get('name')}`: {a.get('format')} (written by {a.get('written_by')}, read by {a.get('read_by')})")
    body_end = len(out)
    out += ["", "## Decisions", ""]
    for d in dec.get("decisions", []):
        rules = ", ".join(f"`{r['id']}` ({r['gloss']})" for r in d.get("rules", []))
        facts = ", ".join(d.get("facts", []))
        out.append(f"- **{d['id']}** {d['question']} → {d.get('choice') or 'decided by a person, see below'}"
                   + (f". Facts: {facts}." if facts else "") + (f" Rules: {rules}." if rules else "")
                   + (f" Sibling: `{d['sibling']}`." if d.get("sibling") else ""))
    resolved = [d for d in st.decisions.values() if d["resolved"]]
    if resolved:
        out += ["", "Decided by a person during the run:", ""]
        out += [f"- **{d['id']}** [{d.get('class')}] {d.get('question')} → {d.get('choice')}" for d in resolved]

    out += [
        "",
        "## Review ledger",
        "",
        "| Layer | Rounds | Stop | Findings | Fixed | Refuted | Decisions | Spend |",
        "|---|---|---|---|---|---|---|---|",
    ]
    out += [layer_summary(st, l) for l in layers]

    held = out[body_end:]
    if shadow:
        out = out[:body_end]
    out += ["", "## Tests", ""]
    for g in final.get("gates", []):
        # A passing gate is one word; the gate's detail (a command's output, a step list) belongs in
        # final_gates.json. Only a gate that did not pass says why.
        why = f" — {g['detail'].splitlines()[0][:200]}" if g["status"] not in ("pass", "n/a") and g.get("detail") else ""
        out.append(f"- {g['name']}: **{g['status']}**{why}")
        for pr in g.get("prs", []):
            out.append(f"  - {pr}")
    if final.get("off"):
        out += ["", "Checks off because the harness does not support them yet:", ""]
        out += [f"- {o['gap']}: {o['what']} — {o['reason']}" for o in final["off"]]

    # The per-call spend is the run's bookkeeping, not what the PR does: it goes beside the PR text.
    spend = ["# Spend", ""] + [f"- {label}: ${usd:.2f}" for label, usd in sorted(st.spend_label.items())] + [f"- total: ${st.spend_total:.2f}"]
    (run / "PR-spend.md").write_text("\n".join(spend) + "\n")

    person = [f"**{d['id']}** [{d['class']}] {d['question']}" for d in st.open_decisions()]
    person += [f"Blocking finding {f['id']} still open in {f['layer']}"
               + (f" (closed by {closed[f['layer']]} without converging)" if f["layer"] in closed else "")
               + f": `{f['rule']}` at `{f['file']}`: {f['claim']}" for f in blocking]
    person += [f"Open risk: {r}" for r in spec.get("open_risks", [])]
    for g in final.get("gates", []):
        if g["status"] in ("fail", "warn"):
            person.append(f"Final gate `{g['name']}` is {g['status']}: {g['detail'].splitlines()[0][:200]}")
    out += ["", "## For a person", ""]
    out += [f"- {p}" for p in person] or ["- Nothing to decide."]
    out += ["", "Rollout, outside this PR:", ""]
    out += [f"- {r}" for r in rollout(values, spec, new_executor)]
    lesser = [f for f in open_findings if f["severity"] != "blocking"]
    if lesser:
        out += ["", "Review findings still open, below blocking:", ""]
        out += [f"- [{f['severity']}] `{f['rule']}` at `{f['file']}`: {f['claim']}" for f in lesser]
    if shadow:
        out += ["", "Shadow mode: the decision record and the review ledger are held back until a person has reviewed this PR blind."]
    held_text = "# Decision record and review ledger\n" + "\n".join(held).rstrip() + "\n" if shadow else ""
    return "\n".join(out).rstrip() + "\n", held_text


def stale_references(protocol: str, body: str) -> list:
    """What the description names that the branch no longer has: the Substreams packages of the
    decision record that are not in the worktree, any `protocols/substreams/<x>` path in the text that
    is not there, and final gates run at another commit than the branch's head. A description is
    generated from the run's records, so a package renamed or removed after them (or gates run
    before the last commit) leaves it stale."""
    run, tree = schema.run_dir(protocol), schema.worktree_dir(protocol)
    problems = []
    if not tree.exists():
        return problems
    dec = maybe(run / "decisions.json", {})
    for key in ("package", "test_package"):
        pkg = (dec.get("paths") or {}).get(key)
        if pkg and not (tree / "protocols/substreams" / pkg).exists() and not (tree / "protocols/substreams" / pkg.replace("-", "/", 1)).exists():
            problems.append(f"decisions.json {key} `{pkg}` is not under protocols/substreams in the worktree")
    for rel in sorted(set(re.findall(r"protocols/substreams/[A-Za-z0-9_./-]+", body))):
        if "<" not in rel and not (tree / rel.rstrip(".")).exists():
            problems.append(f"the text names `{rel}`, which the branch does not have")
    final = maybe(run / "final_gates.json", {})
    head = subprocess.run(["git", "-C", str(tree), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    if final.get("head") and head and final["head"] != head:
        problems.append(f"final gates ran at {final['head'][:10]}, the branch is at {head[:10]}: rerun them")
    return problems


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--out")
    p.add_argument("--shadow", action="store_true", help="hold the decisions and ledger back for after the blind review (PR-held.md)")
    args = p.parse_args()
    run = schema.run_dir(args.protocol)
    out = Path(args.out) if args.out else run / "PR.md"
    body, held = render(args.protocol, args.shadow)
    out.write_text(body)
    print(out)
    for problem in stale_references(args.protocol, body):
        print(f"STALE: {problem}", file=sys.stderr)
    if held:
        (run / "PR-held.md").write_text(held)
        print(run / "PR-held.md")


if __name__ == "__main__":
    main()
