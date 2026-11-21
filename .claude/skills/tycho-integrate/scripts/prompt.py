#!/usr/bin/env python3
"""Renders an agent's prompt: a brief from `briefs/` with the run's parameters filled in.

    prompt.py --protocol P --brief implement --layer indexing --round 2 [--extra-file F] --out <file>

Agents run in the worktree, cut from `origin/main`, where this skill may not exist yet; the brief
therefore travels inside the prompt. Placeholders are `{{name}}`; an unknown one stops the render.
"""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import harness_caps  # noqa: E402
import schema  # noqa: E402
import state as ledger  # noqa: E402

BRIEFS = ("classify", "research", "falsify", "amend", "decide", "review-decisions", "implement", "review", "final-review")

# Knowledge every layer's implementer reads beyond the path table's own entries.
ALWAYS = ("general", "integration", "architecture", "version_control")
LANGUAGES = {"indexing": ("rust",), "simulation:native": ("rust",), "simulation:hybrid": ("rust",), "simulation:vm": ("solidity",), "execution": ("rust", "solidity")}


def knowledge_for(layer: str, kind: str) -> list:
    with open(schema.repo_root() / ".claude/knowledge/paths.toml", "rb") as f:
        table = tomllib.load(f)
    names = list(ALWAYS)
    for entry in table.get("layer", []):
        if entry["name"] == layer or (layer == "simulation" and entry["name"] == "shared-simulation"):
            names += entry.get("knowledge", [])
    names += LANGUAGES.get(f"{layer}:{kind}", LANGUAGES.get(layer, ()))
    seen, out = set(), []
    for n in names:
        if n not in seen:
            seen.add(n)
            out.append(f".claude/knowledge/{n}.md")
    return out


def failed_checks(run: Path, layer: str, round_n: int) -> str:
    """The previous round's failed check steps, and each check of its protocol-testing report that
    failed this layer (`layer_checks.judge_report`), for the implementer to fix."""
    path = run / "rounds" / f"{layer}-{round_n - 1}" / "checks.json"
    if round_n <= 1 or not path.exists():
        return "(no earlier round of this layer)"
    data = schema.load_json(path)
    lines = []
    for step in data.get("steps", []):
        if step.get("status") != "fail":
            continue
        lines.append(f"### {step['name']}: {step.get('detail', '')}")
        if step.get("cmd"):
            lines.append(f"`{step['cmd']}` (log: `{step.get('log', '')}`)")
        for c in step.get("checks", []):
            where = " ".join(x for x in (c.get("component"), c.get("direction")) if x)
            why = c.get("detail") or c.get("reason") or ""
            lines.append(f"- {c.get('test', '')} · `{c['check']}` {c['outcome']}" + (f" · {where}" if where else "") + (f": {why}" if why else ""))
        if step.get("tail") and not step.get("checks"):
            lines.append("```")
            lines.extend(step["tail"][-15:])
            lines.append("```")
        lines.append("")
    return "\n".join(lines) or "(every check of the previous round passed)"


def dci_summary(run: Path, chains: list) -> str:
    """DCI per chain, from envcheck's `debug_storageRangeAt` probe of the run's RPC (`env.json`)."""
    caps = (schema.load_json(run / "env.json").get("chains", {}) if (run / "env.json").exists() else {})
    lines = []
    for chain in chains:
        c = caps.get(chain)
        if c is None:
            lines.append(f"- {chain}: not probed yet (run envcheck); assume no DCI")
        elif c.get("dci"):
            lines.append(f"- {chain}: the node serves `debug_storageRangeAt`; DCI is available, within the chains the rule allows")
        else:
            lines.append(f"- {chain}: **no DCI**: {c.get('detail', 'debug_storageRangeAt unavailable')}")
    lines += [
        "",
        "`dci-only-where-rpc-serves-storage-range` (`.claude/knowledge/indexing.md` § DCI): a package emits",
        "entrypoints only for chains whose RPC serves `debug_storageRangeAt`. On a chain without it, a component",
        "that would need DCI is either not created there (a manifest param turns it off) or its dependency's",
        "values are decoded from storage writes or events into attributes and it is priced natively; a",
        "client-side filter is no substitute. Without the method the harness cannot seed `initialized_accounts`",
        "through that node either.",
    ]
    return "\n".join(lines)


def caps_summary(tree: Path) -> str:
    if not tree.exists():
        return "(worktree not created yet)"
    caps = harness_caps.detect(tree)
    return "\n".join(f"- {gap}: {'on' if c['on'] else 'OFF'} — {c['what']}" + ("" if c["on"] else f" ({c['reason']})") for gap, c in caps.items())


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--brief", required=True, choices=BRIEFS)
    p.add_argument("--layer", choices=schema.LAYERS)
    p.add_argument("--round", type=int, default=0)
    p.add_argument("--extra-file", help="text for {{extra}}: corrections, refuted facts, a person's decisions")
    p.add_argument("--set", action="append", default=[], help="name=value for other placeholders")
    p.add_argument("--out", required=True)
    args = p.parse_args()

    run = schema.run_dir(args.protocol)
    tree = schema.worktree_dir(args.protocol)
    intake = schema.load_json(run / "intake.json") if (run / "intake.json").exists() else {}
    spec = schema.load_json(run / "spec.json") if (run / "spec.json").exists() else {}
    st = ledger.State(ledger.read_events(run))
    values = {
        "protocol": intake.get("protocol", args.protocol),
        "slug": schema.slug(args.protocol),
        "run": str(run),
        "skill": str(schema.skill_dir()),
        "worktree": str(tree),
        "branch": schema.branch(args.protocol),
        "repo": str(schema.repo_root()),
        "chains": ", ".join(intake.get("chains", [])),
        "kind": spec.get("kind", "unknown until research"),
        "route": json.dumps(intake.get("route") or {"kind": "new"}),
        "intake": json.dumps(intake, indent=2),
        "layer": args.layer or "",
        "round": str(args.round),
        "knowledge": "\n".join(f"- `{k}`" for k in knowledge_for(args.layer, spec.get("kind", ""))) if args.layer else "",
        "findings": ledger.answers_file(st, args.layer) if args.layer else "",
        "caps": caps_summary(tree),
        "dci": dci_summary(run, intake.get("chains", [])),
        "checks": failed_checks(run, args.layer, args.round) if args.layer else "",
        "decision_classes": ", ".join(schema.DECISION_CLASSES),
        "extra": Path(args.extra_file).read_text() if args.extra_file else "(none)",
    }
    for item in args.set:
        k, _, v = item.partition("=")
        values[k] = v

    text = (schema.skill_dir() / "briefs" / f"{args.brief}.md").read_text()
    unknown = sorted(set(re.findall(r"\{\{(\w+)\}\}", text)) - set(values))
    if unknown:
        raise SystemExit(f"brief {args.brief} uses unknown placeholders {unknown}")
    text = re.sub(r"\{\{(\w+)\}\}", lambda m: values[m.group(1)], text)
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(text)
    print(args.out)


if __name__ == "__main__":
    main()
