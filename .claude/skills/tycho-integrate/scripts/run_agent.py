#!/usr/bin/env python3
"""Runs one agent of a tycho-integrate run as a headless `claude -p`, and records what it cost.

Every agent of the run goes through here: research, falsify, decide, each implement round, each
review and the final review. The call gets the smaller of the layer's and the integration's
remaining budget as `--max-budget-usd`, so no call spends past a cap, and its `total_cost_usd`
lands in the ledger as a `spend` event.

    run_agent.py --protocol P --label implement:indexing:2 --role implementer \
        --agent tycho-implementer --layer indexing --cwd <worktree> --prompt-file F --out R

Exit codes: 0 done; 2 the agent ended with an error; 3 no budget left, nothing was started; 4 the
agent changed what it may not (the run's records, the repository's branches: restored; the person's
checkout: named), and its output is not used; 5 the session hit the budget part way (its commits,
if any, are in the worktree). Every session that ran has its spend in the ledger.
"""

import argparse
import json
import math
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402
import state as ledger  # noqa: E402

ROLES = ("implementer", "researcher", "reviewer")
PERMISSION_MODE = "acceptEdits"


def agent_definitions(names: list) -> dict:
    """Agent files under `.claude/agents/` as the `--agents` JSON. Passed explicitly because the
    worktree is cut from `origin/main`, which may not carry this skill's agents yet."""
    out = {}
    for name in names:
        path = schema.repo_root() / ".claude" / "agents" / f"{name}.md"
        text = path.read_text()
        m = re.match(r"^---\n(.*?)\n---\n(.*)$", text, re.S)
        if not m:
            raise SystemExit(f"{path}: no frontmatter")
        meta = {}
        for line in m.group(1).splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                meta[k.strip()] = v.strip().strip('"')
        entry = {"description": meta.get("description", name), "prompt": m.group(2).strip()}
        if meta.get("tools"):
            entry["tools"] = [t.strip() for t in meta["tools"].split(",") if t.strip()]
        if meta.get("model"):
            entry["model"] = meta["model"]
        out[meta.get("name", name)] = entry
    return out


def render_settings(role: str, run: Path) -> Path:
    """The role's settings with the guard hook's absolute path filled in."""
    template = (schema.skill_dir() / "settings" / f"{role}.json").read_text()
    guard = schema.skill_dir() / "scripts" / "guard.py"
    # Quoted, and escaped for the JSON string it lands in: a repo path with a space would otherwise
    # break the hook command on every call.
    rendered = template.replace("{GUARD}", json.dumps(shlex.quote(str(guard)))[1:-1])
    path = run / "settings" / f"{role}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(rendered)
    return path


def restore_records(run: Path, saved: Path, before: dict, label: str) -> list:
    """Puts back every run record a session changed, added or removed; their paths."""
    after = schema.records_digest(run, label)
    changed = sorted(p for p in set(before) | set(after) if before.get(p) != after.get(p))
    for rel in changed:
        target = run / rel
        if rel in before:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(saved / rel, target)
        else:
            target.unlink(missing_ok=True)
    return changed


def restore_checkout(repo: Path, before: dict, after: dict) -> list:
    """Branches a session moved go back; a change to the person's checkout (its files) cannot be
    undone safely, so it is named. What changed, as paths."""
    changed = []
    for ref in sorted(set(before["refs"]) | set(after["refs"])):
        if before["refs"].get(ref) != after["refs"].get(ref):
            changed.append(f"branch {ref}")
            if ref in before["refs"]:
                schema.git("-C", str(repo), "update-ref", ref, before["refs"][ref], check=False)
            else:
                schema.git("-C", str(repo), "update-ref", "-d", ref, check=False)
    if (before["status"], before["diff"], before["untracked"]) != (after["status"], after["diff"], after["untracked"]):
        lines = set(before["status"].splitlines()) ^ set(after["status"].splitlines())
        names = sorted({l[3:] for l in lines} | {u for u in set(before["untracked"]) | set(after["untracked"]) if before["untracked"].get(u) != after["untracked"].get(u)})
        changed.append(f"the person's checkout {repo}: " + (", ".join(names[:10]) or "a tracked file changed further"))
    return changed


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--label", required=True, help="what the call is, for the ledger: research, implement:<layer>:<n>, review:<layer>:<n>, final-review")
    p.add_argument("--role", required=True, choices=ROLES)
    p.add_argument("--agent", help="an agent under .claude/agents/; omit to run the prompt in a plain session (reviews)")
    p.add_argument("--layer", choices=schema.LAYERS, help="the layer whose budget the call counts against")
    p.add_argument("--cwd", required=True)
    p.add_argument("--prompt-file", required=True)
    p.add_argument("--out", required=True, help="where the agent's final message goes")
    p.add_argument("--model")
    args = p.parse_args()

    run = schema.run_dir(args.protocol)
    st = ledger.State(ledger.read_events(run))
    budget = math.floor(ledger.remaining_usd(st, args.layer) * 100) / 100
    if budget < ledger.MIN_CALL_USD:
        print(json.dumps({"label": args.label, "error": "no budget left", "remaining": budget}))
        sys.exit(3)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)

    cmd = [
        "claude", "-p",
        "--output-format", "json",
        "--max-budget-usd", f"{budget:.2f}",
        "--permission-mode", PERMISSION_MODE,
        "--settings", str(render_settings(args.role, run)),
        # The role's settings plus the repository's own: the project source is what loads the
        # project's skills (a review runs tycho-review) and CLAUDE.md. The person's user and local
        # settings (their allow lists and hooks) stay out, so a role's policy does not depend on who
        # runs it.
        "--setting-sources", "project",
        "--add-dir", str(run),
    ]
    if args.agent:
        cmd += ["--agents", json.dumps(agent_definitions([args.agent])), "--agent", args.agent]
    if args.model:
        cmd += ["--model", args.model]

    worktree = schema.worktree_dir(args.protocol)
    schema.load_env_files()  # a headless session sees the repo's .env (RPC URLs), not only the shell's
    env = {
        **os.environ,
        "TYCHO_INTEGRATE_GUARD": args.role,
        "TYCHO_INTEGRATE_RUN": str(run),
        "TYCHO_INTEGRATE_WORKTREE": str(worktree),
        "TYCHO_INTEGRATE_REPO": str(schema.repo_root()),
        "TYCHO_INTEGRATE_LABEL": args.label,
        # No agent pushes: whatever runs git (a script, an interpreter), origin's push URL goes
        # nowhere. Fetching is unaffected.
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "remote.origin.pushurl",
        "GIT_CONFIG_VALUE_0": "/dev/null/tycho-integrate-agents-do-not-push",
    }
    prompt = Path(args.prompt_file).read_text()
    repo, own = schema.repo_root(), schema.branch(args.protocol)
    before, checkout_before = schema.records_digest(run, args.label), schema.checkout_digest(repo, own)
    with tempfile.TemporaryDirectory() as saved:
        for rel in before:
            dst = Path(saved) / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(run / rel, dst)
        with tempfile.TemporaryFile("w+") as stderr:
            proc = subprocess.run(cmd, input=prompt, capture_output=False, stdout=subprocess.PIPE, stderr=stderr, text=True, cwd=args.cwd, env=env)
            stderr.seek(0)
            err_text = stderr.read()
        tampered = restore_records(run, Path(saved), before, args.label)
    tampered += restore_checkout(repo, checkout_before, schema.checkout_digest(repo, own))

    try:
        result = json.loads(proc.stdout)
    except json.JSONDecodeError:
        result = None
    # The session's cost reaches the ledger whatever happened in it.
    spend = {"t": "spend", "label": args.label, "session": (result or {}).get("session_id"), "turns": (result or {}).get("num_turns")}
    if args.layer:
        spend["layer"] = args.layer
    usd = float((result or {}).get("total_cost_usd") or 0.0)
    if result is None:
        # A crashed or killed session reports no cost; the call is on record, its cost unknown.
        spend["error"] = f"no JSON result (exit {proc.returncode})"
    ledger.append(run, {**spend, "usd": round(usd, 4)})

    if tampered:
        # The session changed what it may not: the run's records and the repository's branches are
        # back as they were; a change to the person's checkout is named. The person hears of it
        # (`next` escalates), and the session's output is not used.
        ledger.append(run, {"t": "tamper", "label": args.label, "paths": tampered, **({"layer": args.layer} if args.layer else {})})
        print(json.dumps({"label": args.label, "error": "the agent changed what it may not; see paths", "paths": tampered}))
        sys.exit(4)
    if result is None:
        out.write_text(proc.stdout + "\n" + err_text)
        print(json.dumps({"label": args.label, "error": "claude -p did not return JSON", "exit": proc.returncode, "out": args.out}))
        sys.exit(2)

    out.write_text(result.get("result") or "")
    subtype = result.get("subtype", "")
    summary = {"label": args.label, "usd": usd, "subtype": subtype, "is_error": bool(result.get("is_error")), "out": args.out, "session": result.get("session_id")}
    print(json.dumps(summary))
    if "budget" in subtype:
        sys.exit(5)
    if result.get("is_error") or proc.returncode != 0:
        sys.exit(2)


if __name__ == "__main__":
    main()
