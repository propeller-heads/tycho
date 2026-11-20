#!/usr/bin/env python3
"""PreToolUse hook for the agents of a tycho-integrate run: denies the edits a role may not make.

Reads the hook input (JSON on stdin) and, for a denied call, prints a `permissionDecision: deny`
with the reason. The role and the run come from the environment `run_agent.py` sets:

- implementer: writes only inside the run's worktree, never to a guarded path (the knowledge
  base, the test harness, the check scripts, CI) and never a line that relaxes an integration
  test (a skip flag, a tolerance, `tvl_only`, `harness`, an `under_cap` limit, a changed limit
  kind); no push and no GitHub. The one guarded file it may extend is protocols/testing's
  executor table, with a new executor's fixture and mapping lines only.
- researcher: writes only under the run directory (spec, fork scripts, cloned references).
- reviewer: writes only under `.claude/reviews/` and the run directory.

This guard sees Edit, Write and Bash calls; a determined Bash command can still get past a
heuristic, so `state.py check-guards` checks every round's commits as well.
"""

import json
import os
import re
import shlex
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

EDIT_TOOLS = ("Edit", "Write", "MultiEdit", "NotebookEdit")
WRITE_VERBS = re.compile(r"(^|[\s;&|(])(sed\s+-i|tee|mv|cp|rm|truncate|dd|install|ln)\b|>>?")
FORBIDDEN_BASH = (
    (re.compile(r"\bgit\s+push\b"), "pushing is the person's step (Gate 3)"),
    (re.compile(r"(^|[\s;&|(])gh\s"), "no GitHub calls from an agent of the run"),
    (re.compile(r"\bgit\s+(rebase|reset\s+--hard|checkout\s+-B|branch\s+-[dD]|worktree)\b"), "history and worktrees belong to the orchestrator"),
)
RESEARCHER_BASH = (
    (re.compile(r"\bgit\s+push\b"), "researchers do not push"),
    (re.compile(r"\bgh\s+(api|pr\s+(create|merge|comment|edit|review)|issue\s+(create|comment|edit))\b"), "researchers only read GitHub"),
)


def deny(reason: str) -> None:
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": f"tycho-integrate guard: {reason}",
        }
    }))
    sys.exit(0)


def inside(path: Path, root: Path) -> bool:
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


def edited_text(tool: str, inp: dict) -> tuple:
    """(old, new) text of an edit, for the yaml line check."""
    if tool == "Edit":
        return inp.get("old_string", ""), inp.get("new_string", "")
    if tool == "MultiEdit":
        edits = inp.get("edits", [])
        return "\n".join(e.get("old_string", "") for e in edits), "\n".join(e.get("new_string", "") for e in edits)
    if tool == "Write":
        path = Path(inp.get("file_path", ""))
        old = path.read_text() if path.exists() else ""
        return old, inp.get("content", "")
    return "", ""


def resulting_file(tool: str, inp: dict):
    """(old file text, new file text) after an Edit, MultiEdit or Write; None when it cannot be told."""
    path = Path(inp.get("file_path", ""))
    old = path.read_text() if path.exists() else ""
    if tool == "Write":
        return old, inp.get("content", "")
    edits = [inp] if tool == "Edit" else inp.get("edits", []) if tool == "MultiEdit" else None
    if edits is None:
        return None
    new = old
    for e in edits:
        if e.get("old_string", "") not in new:
            return None
        new = new.replace(e["old_string"], e.get("new_string", ""), -1 if e.get("replace_all") else 1)
    return old, new


def guarded_yaml_lines(old: str, new: str) -> list:
    """Guarded yaml lines the edit adds, changes or removes."""
    return schema.guarded_yaml_changes(*schema.line_diff(old, new))


def check_edit(role: str, tool: str, inp: dict, worktree: Path, run: Path, repo: Path) -> None:
    raw = inp.get("file_path") or inp.get("notebook_path") or ""
    if not raw:
        return
    path = Path(raw)
    if role == "researcher":
        if not inside(path, run):
            deny(f"researchers write only under {run}; {path} is outside")
        return
    if role == "reviewer":
        if not (inside(path, repo / ".claude" / "reviews") or inside(path, run)):
            deny(f"reviews write only under .claude/reviews/ or {run}; {path} is outside")
        return
    if not inside(path, worktree):
        deny(f"the implementer writes only inside the worktree {worktree}; {path} is outside")
    rel = str(path.resolve().relative_to(worktree.resolve()))
    if schema.guarded_path(rel):
        texts = resulting_file(tool, inp)
        bad = schema.disallowed_additions_text(rel, *texts) if texts else schema.disallowed_additions(rel, *schema.line_diff(*edited_text(tool, inp)))
        if bad is None:
            deny(f"{rel} is guarded during a run (knowledge, test harness, check scripts, CI); answer the finding as a decision instead")
        if bad:
            deny(f"in {rel} only a new executor's fixture and mapping lines may be added, nothing removed; these are not: {bad}")
        return
    if schema.INTEGRATION_YAML_RE.search(rel):
        lines = guarded_yaml_lines(*edited_text(tool, inp))
        if lines:
            deny(f"the edit changes test-relaxing lines {lines} in {rel}; relaxing a test is a person's decision (class relax-test)")


def check_bash(role: str, command: str, worktree: Path, run: Path, repo: Path) -> None:
    rules = RESEARCHER_BASH if role == "researcher" else FORBIDDEN_BASH if role == "implementer" else ()
    for rx, why in rules:
        if rx.search(command):
            deny(why)
    if role != "implementer" or not WRITE_VERBS.search(command):
        return
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError:
        tokens = command.split()
    for tok in tokens:
        tok = tok.lstrip(">").strip()
        if not tok or tok.startswith("-"):
            continue
        if tok.startswith("/"):
            p = Path(tok)
            if not (inside(p, worktree) or inside(p, Path("/tmp")) or inside(p, Path(os.environ.get("TMPDIR", "/tmp"))) or str(p).startswith("/dev/")):
                deny(f"a writing command names {tok}, outside the worktree")
            if inside(p, worktree) and schema.guarded_path(str(p.resolve().relative_to(worktree.resolve()))):
                deny(f"a writing command names the guarded path {tok}")
        elif schema.guarded_path(tok.removeprefix("./")) or schema.INTEGRATION_YAML_RE.search(tok):
            deny(f"a writing command names {tok}; edit integration test yamls with Edit so the guard can see the lines, and guarded paths not at all")


def main() -> None:
    role = os.environ.get("TYCHO_INTEGRATE_GUARD")
    if not role:
        sys.exit(0)  # not inside a run
    data = json.load(sys.stdin)
    tool = data.get("tool_name", "")
    inp = data.get("tool_input", {}) or {}
    worktree = Path(os.environ["TYCHO_INTEGRATE_WORKTREE"])
    run = Path(os.environ["TYCHO_INTEGRATE_RUN"])
    repo = Path(os.environ.get("TYCHO_INTEGRATE_REPO", "."))
    if tool in EDIT_TOOLS:
        check_edit(role, tool, inp, worktree, run, repo)
    elif tool == "Bash":
        check_bash(role, inp.get("command", ""), worktree, run, repo)
    sys.exit(0)


if __name__ == "__main__":
    main()
