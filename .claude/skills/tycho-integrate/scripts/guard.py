#!/usr/bin/env python3
"""PreToolUse hook for the agents of a tycho-integrate run: denies the edits a role may not make.

Reads the hook input (JSON on stdin) and, for a denied call, prints a `permissionDecision: deny`
with the reason. The role and the run come from the environment `run_agent.py` sets:

- implementer: writes only inside the run's worktree (and temp dirs), never to a guarded path
  (the knowledge base, the test harness, the check scripts, CI) and never a line that relaxes an
  integration test (a skip flag, a tolerance, `tvl_only`, `harness`, an `under_cap` limit, a
  changed limit kind); no push and no GitHub. The guarded files it may extend are
  protocols/testing's wiring for a new integration (`schema.GUARDED_PATH_EDITS`): the executor's
  fixture and mapping, the decoder arm, and a config-only fork's base package.
- researcher: writes only under the run directory (spec, fork scripts, cloned references); reads
  GitHub, never writes to it.
- reviewer: writes only under `.claude/reviews/` and the run directory; reads GitHub (`gh api`
  GET only), never writes to it.

No role writes the run's own records (`PROTECTED_RUN_FILES`: the ledger, the intake, the role
settings, the layer checks' results): those are the orchestrator's and the person's.

Any error in the guard denies the call: Claude Code lets a call through when a hook fails with an
exit code other than 2, so a crash must never be the way past it. Bash writes are recognised by
their shape (redirections, writing commands, an inline interpreter that opens a file for writing);
a determined command can still get past that, so `state.py check-guards` checks every round's
commits and the worktree's uncommitted changes as well.
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

# The run directory's records no agent writes.
PROTECTED_RUN_FILES = ("ledger.jsonl", "intake.json", "settings", "rounds", "final_gates.json", "env.json", "prompts")

# A push, however the git command is spelled (`git -C x push`, `env git push`).
GIT_PUSH = re.compile(r"\bgit\b[^;&|\n]*\bpush\b")
GH_WRITE = re.compile(r"\bgh\s+(pr|issue|release|repo|label|workflow|run|gist|secret|variable)\s+(create|merge|comment|edit|review|close|reopen|delete|ready|lock|unlock|transfer|fork|archive|rename|set|cancel|rerun|enable|disable)\b")
GH_API = re.compile(r"\bgh\s+api\b(?P<args>[^;&|\n]*)")
# `gh api` sends a POST with any of these, unless a method is named.
GH_API_BODY = re.compile(r"(^|\s)(-f|-F|--field|--raw-field|--input)(\s|=|$)")
GH_API_METHOD = re.compile(r"(^|\s)(-X|--method)(\s+|=)?(?P<m>[A-Za-z]+)")

ROLE_BASH = {
    "implementer": (
        (re.compile(r"(^|[\s;&|(])gh\s"), "no GitHub calls from an agent of the run"),
        (re.compile(r"\bgit\s+(rebase|reset\s+--hard|checkout\s+-B|branch\s+-[dD]|worktree)\b"), "history and worktrees belong to the orchestrator"),
    ),
    "researcher": (
        (GH_API, "researchers read GitHub with `gh pr view|list|diff`, `gh search` and `gh repo view`"),
    ),
    "reviewer": (
        (re.compile(r"\bgit\s+(commit|rebase|reset|checkout\s+-[bB]|branch\s+-[dD]|tag|merge|cherry-pick|revert)\b"), "a review changes no history"),
    ),
}

# Commands whose arguments are written. `last` writes only its last argument (the destination).
WRITERS = {"tee": "all", "rm": "all", "truncate": "all", "touch": "all", "mkdir": "all", "rmdir": "all", "shred": "all",
           "mv": "last", "cp": "last", "install": "last", "ln": "last", "rsync": "last"}
REDIRECTS = (">", ">>", "&>", "&>>", ">|", "1>", "2>", "1>>", "2>>")
INTERPRETERS = ("python", "python3", "perl", "ruby", "node")
WRITE_CALL = re.compile(r"""open\s*\([^)]*['"][wax+]|\.write_(text|bytes)\s*\(|\b(shutil|os)\.(copy\w*|move|rename|replace|remove|unlink)\s*\(|>\s*['"]|writeFileSync|appendFileSync|File\.write""")
STRING_LITERAL = re.compile(r"""(['"])((?:(?!\1)[^\\]|\\.)*)\1""")


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


def read(path: Path) -> str:
    return path.read_text(errors="replace") if path.is_file() else ""


def edited_text(tool: str, inp: dict) -> tuple:
    """(old, new) text of an edit: the edit's own strings, for the yaml line check."""
    if tool == "Edit":
        return inp.get("old_string", ""), inp.get("new_string", "")
    if tool == "MultiEdit":
        edits = inp.get("edits", [])
        return "\n".join(e.get("old_string", "") for e in edits), "\n".join(e.get("new_string", "") for e in edits)
    if tool == "Write":
        return read(Path(inp.get("file_path", ""))), inp.get("content", "")
    return "", ""


def resulting_file(tool: str, inp: dict):
    """(old file text, new file text) after an Edit, MultiEdit or Write; None when it cannot be told."""
    path = Path(inp.get("file_path") or inp.get("notebook_path") or "")
    old = read(path)
    if tool == "Write":
        return old, inp.get("content", "")
    edits = [inp] if tool == "Edit" else inp.get("edits", []) if tool == "MultiEdit" else None
    if edits is None:
        return None
    new = old
    for e in edits:
        if e.get("old_string", "") not in new:
            return None
        new = new.replace(e.get("old_string", ""), e.get("new_string", ""), -1 if e.get("replace_all") else 1)
    return old, new


def guarded_yaml_lines(old: str, new: str) -> list:
    """Guarded yaml lines the edit adds, changes or removes."""
    return schema.guarded_yaml_changes(*schema.line_diff(old, new))


def write_roots(role: str, worktree: Path, run: Path, repo: Path) -> list:
    tmp = [Path(p) for p in os.environ.get("TYCHO_INTEGRATE_TMP_ROOTS", f"/tmp:{os.environ.get('TMPDIR', '/tmp')}").split(":") if p]
    if role == "implementer":
        return [worktree, *tmp]
    if role == "researcher":
        return [run, *tmp]
    return [run, repo / ".claude" / "reviews", *tmp]


def check_target(role: str, path: Path, worktree: Path, run: Path, repo: Path, how: str) -> None:
    """Denies a write to `path` the role may not make."""
    if str(path).startswith("/dev/"):
        return
    if inside(path, run):
        rel = path.resolve().relative_to(run.resolve()).parts
        if rel and rel[0] in PROTECTED_RUN_FILES:
            deny(f"{how} writes {path}: the run's {rel[0]} is the orchestrator's record")
    roots = write_roots(role, worktree, run, repo)
    if not any(inside(path, r) for r in roots):
        deny(f"{how} writes {path}; a {role} writes only under {', '.join(str(r) for r in roots)}")
    if role == "implementer" and inside(path, worktree):
        rel = str(path.resolve().relative_to(worktree.resolve()))
        if schema.guarded_path(rel):
            deny(f"{how} writes the guarded path {rel}; change guarded files with Edit, and only the integration's wiring")
        if schema.INTEGRATION_YAML_RE.search(rel):
            deny(f"{how} writes {rel}; edit integration test yamls with Edit, so the guard sees the lines")


def check_edit(role: str, tool: str, inp: dict, worktree: Path, run: Path, repo: Path) -> None:
    raw = inp.get("file_path") or inp.get("notebook_path") or ""
    if not raw:
        deny(f"{tool} names no file")
    path = Path(raw)
    if role != "implementer" or not inside(path, worktree):
        check_target(role, path, worktree, run, repo, tool)
        return
    rel = str(path.resolve().relative_to(worktree.resolve()))
    if schema.guarded_path(rel):
        texts = resulting_file(tool, inp) or edited_text(tool, inp)
        bad = schema.disallowed_change(rel, *texts)
        if bad is None:
            deny(f"{rel} is guarded during a run (knowledge, test harness, check scripts, CI); answer the finding as a decision instead")
        if bad:
            deny(f"{rel} is guarded; only the new integration's wiring may be added to it, nothing else changed: {bad}")
        return
    if schema.INTEGRATION_YAML_RE.search(rel):
        lines = guarded_yaml_lines(*edited_text(tool, inp))
        if lines:
            deny(f"the edit changes test-relaxing lines {lines} in {rel}; relaxing a test is a person's decision (class relax-test)")


def segments(command: str) -> list:
    """The command's simple commands, as token lists, split at `;`, `&&`, `||`, `|` and newlines."""
    lex = shlex.shlex(command.replace("\n", " ; "), posix=True, punctuation_chars=";&|<>()")
    lex.whitespace_split = True
    try:
        tokens = list(lex)
    except ValueError:
        tokens = command.split()
    out, cur = [], []
    for tok in tokens:
        if tok in (";", "&&", "||", "|", "&", "(", ")", "|&", ";;"):
            if cur:
                out.append(cur)
            cur = []
        else:
            cur.append(tok)
    if cur:
        out.append(cur)
    return out


def write_targets(tokens: list) -> list:
    """(target, how) for each path one simple command writes."""
    out, args, i = [], [], 0
    while i < len(tokens):
        tok = tokens[i]
        if tok in ("<", "<<", "<<<"):
            i += 2
            continue
        if ">" in tok and re.fullmatch(r"[<>&|]+", tok):
            # A redirection operator and its target; `>&2` (and `2>&1`) name a descriptor, not a file.
            nxt = tokens[i + 1] if i + 1 < len(tokens) else ""
            if nxt and not (tok.endswith("&") and nxt.isdigit()):
                out.append((nxt, "a redirection"))
            i += 2
            continue
        args.append(tok)
        i += 1
    while args and (re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", args[0]) or args[0] in ("env", "sudo", "command", "nice", "time", "xargs", "exec")):
        args = args[1:]
    if not args:
        return out
    verb, rest = Path(args[0]).name, [a for a in args[1:] if not a.startswith("-")]
    if verb == "sed" and any(a == "-i" or a.startswith(("-i", "--in-place")) for a in args[1:]):
        out += [(a, "sed -i") for a in rest[1:]]
    elif verb == "dd":
        out += [(a[3:], "dd") for a in args[1:] if a.startswith("of=")]
    elif WRITERS.get(verb) == "all":
        out += [(a, verb) for a in rest]
    elif WRITERS.get(verb) == "last" and rest:
        out.append((rest[-1], verb))
    elif verb.rstrip("0123456789.") in INTERPRETERS:
        code = next((args[j + 1] for j, a in enumerate(args[:-1]) if a in ("-c", "-e", "-E")), "")
        if code and WRITE_CALL.search(code):
            out += [(lit, f"{verb} code") for _, lit in STRING_LITERAL.findall(code) if lit and " " not in lit and ("/" in lit or "." in lit)]
    return out


def check_bash(role: str, command: str, cwd: Path, worktree: Path, run: Path, repo: Path) -> None:
    if GIT_PUSH.search(command):
        deny("pushing is the person's step (Gate 3)")
    if GH_WRITE.search(command):
        deny("agents of the run only read GitHub")
    for m in GH_API.finditer(command):
        args = m.group("args")
        method = GH_API_METHOD.search(args)
        if (method and method.group("m").upper() != "GET") or (not method and GH_API_BODY.search(args)):
            deny("`gh api` only reads (GET) in a run")
    for rx, why in ROLE_BASH.get(role, ()):
        if rx.search(command):
            deny(why)
    for tokens in segments(command):
        if tokens[0] == "cd" and len(tokens) > 1:
            # Later relative paths resolve from here (a `cd` in a subshell is followed too: stricter).
            cwd = Path(tokens[1]).expanduser() if Path(tokens[1]).expanduser().is_absolute() else cwd / tokens[1]
            continue
        for target, how in write_targets(tokens):
            path = Path(target).expanduser()
            check_target(role, path if path.is_absolute() else cwd / path, worktree, run, repo, how)


def main() -> None:
    role = os.environ.get("TYCHO_INTEGRATE_GUARD")
    if not role:
        sys.exit(0)  # not inside a run
    try:
        data = json.load(sys.stdin)
        tool = data.get("tool_name", "")
        inp = data.get("tool_input", {}) or {}
        worktree = Path(os.environ["TYCHO_INTEGRATE_WORKTREE"])
        run = Path(os.environ["TYCHO_INTEGRATE_RUN"])
        repo = Path(os.environ["TYCHO_INTEGRATE_REPO"])
        cwd = Path(data.get("cwd") or os.getcwd())
        if tool in EDIT_TOOLS:
            check_edit(role, tool, inp, worktree, run, repo)
        elif tool == "Bash":
            check_bash(role, inp.get("command", ""), cwd, worktree, run, repo)
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001 - a guard that cannot decide denies
        deny(f"the guard could not check this call ({type(e).__name__}: {e}); nothing was run")
    sys.exit(0)


if __name__ == "__main__":
    main()
