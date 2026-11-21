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

In the run directory a session writes only what its brief produces (`schema.AGENT_WRITES`: the
spec for research, the decisions for decide, ...) and the scratch directories; the rest are the
orchestrator's and the person's records. No role runs the scripts that change them. What this hook cannot see (a script file, a heredoc), run_agent.py
catches by comparing the records before and after the session.

Any error in the guard denies the call: Claude Code lets a call through when a hook fails with an
exit code other than 2 (https://docs.claude.com/en/docs/claude-code/hooks, "Hook output"), so a
crash must never be the way past it. Bash writes are recognised by
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

# The run's scripts that change its state are the orchestrator's: an agent never records a round, a
# review, a decision or a gate. Agents may read the ledger (these `state.py` commands) and run the
# layer's checks into a scratch directory (`layer_checks.py --scratch`).
ORCHESTRATOR_SCRIPTS = ("state.py", "layer_checks.py", "final_gates.py", "run_agent.py", "split.py", "worktree.py", "pr_body.py")
STATE_READS = ("status", "resume", "open-findings", "remaining")

GH_WRITE = re.compile(r"\bgh\s+(pr|issue|release|repo|label|workflow|run|gist|secret|variable)\s+(create|merge|comment|edit|review|close|reopen|delete|ready|lock|unlock|transfer|fork|archive|rename|set|cancel|rerun|enable|disable)\b")
GH_API = re.compile(r"\bgh\s+api\b(?P<args>[^;&|\n]*)")
# `gh api` sends a POST with any of these, unless a method is named.
GH_API_BODY = re.compile(r"(^|\s)(-[A-Za-z]*[fF]\S*|--field|--raw-field|--input)(\s|=|$)")
GH_API_METHOD = re.compile(r"(^|\s)(-X|--method)(\s+|=)?(?P<m>[A-Za-z]+)")

ROLE_BASH = {
    "implementer": ((re.compile(r"(^|[\s;&|(])gh\s"), "no GitHub calls from an agent of the run"),),
    "researcher": ((GH_API, "researchers read GitHub with `gh pr view|list|diff`, `gh search` and `gh repo view`"),),
}

# git subcommands a role may not run (the subcommand, whatever global options precede it).
GIT_DENIED = {
    "implementer": {"push", "rebase", "update-ref", "worktree", "filter-branch", "replace"},
    "researcher": {"push", "commit", "rebase", "reset", "merge", "cherry-pick", "revert", "update-ref", "am", "apply", "stash", "checkout", "switch", "restore", "worktree", "filter-branch", "replace", "notes"},
    "reviewer": {"push", "commit", "rebase", "reset", "merge", "cherry-pick", "revert", "update-ref", "am", "apply", "checkout", "switch", "restore", "filter-branch", "replace", "notes"},
}
GIT_GLOBAL_WITH_VALUE = ("-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path")
# `git branch` and `git tag` options that only list.
GIT_LIST_FLAGS = {"-a", "-r", "-l", "--list", "-v", "-vv", "--show-current", "--all", "--remotes", "--merged", "--no-merged", "--contains", "--no-contains", "--sort", "--format", "--points-at", "-n", "--column", "--no-column"}
SHELLS = ("bash", "sh", "zsh", "dash")
WRAPPERS = ("env", "sudo", "command", "nice", "time", "xargs", "exec", "nohup", "stdbuf", "timeout")

# Commands whose arguments are written. `last` writes only its last argument (the destination).
WRITERS = {"tee": "all", "rm": "all", "truncate": "all", "touch": "all", "mkdir": "all", "rmdir": "all", "shred": "all",
           "mv": "last", "cp": "last", "install": "last", "ln": "last", "rsync": "last"}
INTERPRETERS = ("python", "python3", "perl", "ruby", "node")
WRITE_CALL = re.compile(r"""open\s*\([^)]*['"][wax+]|\.write_(text|bytes)\s*\(|\b(shutil|os)\.(copy\w*|move|rename|replace|remove|unlink)\s*\(|>\s*['"]|writeFileSync|appendFileSync|File\.write""")
# Inline interpreter code that runs another command: the guard cannot see that command.
RUNS_COMMANDS = re.compile(r"\b(os\.(system|popen|exec\w*|spawn\w*)|subprocess|pty\.spawn|child_process|execSync|spawnSync|`)")
STRING_LITERAL = re.compile(r"""(['"])((?:(?!\1)[^\\]|\\.)*)\1""")
# The calls in inline interpreter code whose string arguments are paths it writes (or names a path
# it writes to, as `Path('x').write_text`): only those literals are write targets, so a sidecar
# that merely mentions a file path is not one.
PATH_CALL = re.compile(r"""\b(open|Path|copy\w*|move|rename|replace|unlink|remove|writeFileSync|appendFileSync|File\.write)\s*\(([^()]*)\)""")


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
    """Where a role writes. Every role may use the temp dirs (`/tmp`, `$TMPDIR`; the tests name
    another with TYCHO_INTEGRATE_TMP_ROOTS, since their trees live in the temp dir)."""
    tmp = [Path(p) for p in os.environ.get("TYCHO_INTEGRATE_TMP_ROOTS", f"/tmp:{os.environ.get('TMPDIR', '/tmp')}").split(":") if p]
    if role == "implementer":
        return [worktree, *tmp]
    if role == "researcher":
        return [run, *tmp]
    return [run, repo / ".claude" / "reviews", *tmp]


def check_target(role: str, path: Path, worktree: Path, run: Path, repo: Path, how: str, label: str = "") -> None:
    """Denies a write to `path` the role may not make."""
    if str(path).startswith("/dev/"):
        return
    if inside(path, run):
        rel = str(path.resolve().relative_to(run.resolve()))
        if rel != "." and not schema.agent_may_write(label, rel):
            deny(f"{how} writes {path}: in the run directory a `{label.split(':')[0] or role}` session writes only "
                 f"{', '.join(schema.AGENT_WRITES.get(label.split(':')[0], ())) or 'nothing'} and {', '.join(schema.RUN_SCRATCH)}/; the rest are the orchestrator's records")
        return
    roots = write_roots(role, worktree, run, repo)
    if not any(inside(path, r) for r in roots):
        deny(f"{how} writes {path}; a {role} writes only under {', '.join(str(r) for r in roots)}")
    if role == "implementer" and inside(path, worktree):
        rel = str(path.resolve().relative_to(worktree.resolve()))
        # A directory that holds guarded paths counts as one (`rm -r .claude`, `find .claude -delete`).
        if schema.guarded_path(rel) or schema.guarded_path(rel.rstrip("/") + "/x") or rel == ".":
            deny(f"{how} writes the guarded path {rel}; change guarded files with Edit, and only the integration's wiring")
        if schema.INTEGRATION_YAML_RE.search(rel):
            deny(f"{how} writes {rel}; edit integration test yamls with Edit, so the guard sees the lines")


def check_edit(role: str, tool: str, inp: dict, worktree: Path, run: Path, repo: Path, label: str = "") -> None:
    raw = inp.get("file_path") or inp.get("notebook_path") or ""
    if not raw:
        deny(f"{tool} names no file")
    path = Path(raw)
    if role != "implementer" or not inside(path, worktree):
        check_target(role, path, worktree, run, repo, tool, label)
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
        # The whole file's lines, before and after: an edit of part of a line (`strict` to
        # `legacy`) changes a guarded line its own strings do not show.
        lines = guarded_yaml_lines(*(resulting_file(tool, inp) or edited_text(tool, inp)))
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


def split_redirections(tokens: list) -> tuple:
    """(the command's own words, (target, how) for each file a redirection writes)."""
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
    return args, out


def unwrap(args: list) -> list:
    """The command a wrapper runs: past `VAR=x`, `env`/`sudo`/`timeout 5`/..., and `mise exec -- `."""
    while args:
        head = Path(args[0]).name
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", args[0]):
            args = args[1:]
        elif head in WRAPPERS:
            args = args[1:]
            while args and (args[0].startswith("-") or re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=.*", args[0])):
                args = args[1:]
            if head == "timeout" and args:
                args = args[1:]  # the duration
        elif head == "mise" and len(args) > 1 and args[1] in ("exec", "x"):
            args = args[args.index("--") + 1:] if "--" in args else []
        else:
            return args
    return args


def git_subcommand(args: list) -> tuple:
    """(subcommand, its arguments) of a `git ...` word list."""
    i = 1
    while i < len(args) and args[i].startswith("-"):
        i += 2 if args[i] in GIT_GLOBAL_WITH_VALUE else 1
    return (args[i], args[i + 1:]) if i < len(args) else ("", [])


def check_git(role: str, args: list, cwd: Path, worktree: Path, run: Path, repo: Path, label: str) -> list:
    """Denies a git command the role may not run; the paths it writes, as (target, how)."""
    sub, rest = git_subcommand(args)
    if sub == "push":
        deny("pushing is the person's step (Gate 3)")
    if sub in GIT_DENIED.get(role, ()):
        deny(f"`git {sub}` is not a {role}'s: history, branches and worktrees belong to the orchestrator")
    flags = [a for a in rest if a.startswith("-")]
    positional = [a for a in rest if not a.startswith("-")]
    if sub == "submodule" and "foreach" in rest:
        deny("`git submodule foreach` runs commands the guard cannot see")
    if sub in ("branch", "tag") and role != "implementer":
        listing = set(flags) <= GIT_LIST_FLAGS | {"--"} and (not positional or flags)
        if not listing:
            deny(f"a {role} only lists with `git {sub}`")
    if sub == "branch" and role == "implementer" and set(flags) & {"-d", "-D", "-f", "-m", "-M", "--delete", "--force", "--move"}:
        deny("an implementer does not move or delete branches")
    if sub == "fetch" and any(":" in a for a in positional):
        deny("a fetch into a local branch moves it; fetch into the remote-tracking refs")
    if sub == "worktree":
        action = positional[0] if positional else ""
        if action not in ("add", "remove", "list"):
            deny(f"`git worktree {action}` is the orchestrator's")
        if action in ("add", "remove"):
            if action == "add" and set(flags) & {"-b", "-B"}:
                deny("a review's worktree is detached: no branch")
            target = Path(positional[1]).expanduser() if len(positional) > 1 else None
            if target is None or not inside(target if target.is_absolute() else cwd / target, repo / ".claude" / "reviews"):
                deny(f"a review adds and removes only its own worktree under .claude/reviews/, not {positional[1:2]}")
    if sub == "clone":
        urlish = [a for a in positional if a]
        if urlish:
            dest = urlish[1] if len(urlish) > 1 else re.sub(r"\.git$", "", urlish[0].rstrip("/").rsplit("/", 1)[-1].rsplit(":", 1)[-1])
            return [(dest, "git clone")]
    return []


def write_targets(tokens: list, role: str = "", cwd: Path = Path("."), worktree: Path = Path("/"), run: Path = Path("/"), repo: Path = Path("/"), label: str = "", depth: int = 0) -> list:
    """(target, how) for each path one simple command writes; denies a command whose writes the
    guard cannot see (a shell or interpreter running another command, `find -exec`, awk output)."""
    args, out = split_redirections(tokens)
    args = unwrap(args)
    if not args:
        return out
    verb, rest = Path(args[0]).name, [a for a in args[1:] if not a.startswith("-")]
    nested = lambda command: [t for seg in segments(command) for t in write_targets(seg, role, cwd, worktree, run, repo, label, depth + 1)]
    if depth > 3:
        deny("a command nested this deep is not checked")
    if verb in ORCHESTRATOR_SCRIPTS:
        check_script(verb, args)
    script = next((Path(a).name for a in args[1:3] if Path(a).name in ORCHESTRATOR_SCRIPTS), None)
    if verb.rstrip("0123456789.") in ("python", "python3") and script:
        check_script(script, args[args.index(next(a for a in args if Path(a).name == script)):])
    if verb in SHELLS and "-c" in args[1:]:
        return out + nested(args[args.index("-c") + 1] if args.index("-c") + 1 < len(args) else "")
    if verb == "git":
        return out + check_git(role, args, cwd, worktree, run, repo, label)
    if verb == "find":
        starts = [a for a in args[1:] if not a.startswith(("-", "(", "!"))][:1] or ["."]
        if "-delete" in args:
            out += [(s, "find -delete") for s in starts]
        for flag in ("-exec", "-execdir", "-ok", "-okdir"):
            if flag in args:
                i = args.index(flag) + 1
                j = next((k for k in range(i, len(args)) if args[k] in (";", "+", "\\;")), len(args))
                out += write_targets([starts[0] if a == "{}" else a for a in args[i:j]], role, cwd, worktree, run, repo, label, depth + 1)
        return out
    if verb == "sed":
        if any(a == "-i" or a.startswith(("-i", "--in-place")) for a in args[1:]):
            out += [(a, "sed -i") for a in rest[1:]]
        script_text = rest[0] if rest else ""
        out += [(m, "sed w") for m in re.findall(r"(?:^|[;/}\s])w\s+(\S+)", script_text)]
        return out
    if verb in ("awk", "gawk", "mawk", "nawk"):
        program = " ".join(rest[:1])
        if re.search(r"print[f]?[^;}]*>|system\s*\(|\|\s*\"", program) or "inplace" in args:
            deny("awk that writes files or runs commands is not checked; write with a file tool")
        return out
    if verb in ("curl",):
        for flag in ("-o", "--output"):
            if flag in args and args.index(flag) + 1 < len(args):
                out.append((args[args.index(flag) + 1], "curl"))
        if "-O" in args or "--remote-name" in args:
            out.append((".", "curl -O"))
        return out
    if verb == "wget":
        target = next((args[i + 1] for i, a in enumerate(args[:-1]) if a in ("-O", "--output-document", "-P", "--directory-prefix")), ".")
        return out + [(target, "wget")]
    if verb == "dd":
        return out + [(a[3:], "dd") for a in args[1:] if a.startswith("of=")]
    if WRITERS.get(verb) == "all":
        return out + [(a, verb) for a in rest]
    if WRITERS.get(verb) == "last" and rest:
        return out + [(rest[-1], verb)]
    if verb.rstrip("0123456789.") in INTERPRETERS:
        code = next((args[j + 1] for j, a in enumerate(args[:-1]) if a in ("-c", "-e", "-E")), "")
        if code and RUNS_COMMANDS.search(code):
            deny(f"{verb} code that runs commands is not checked; run the command itself")
        if code and WRITE_CALL.search(code):
            for call in PATH_CALL.finditer(code):
                out += [(lit, f"{verb} code") for _, lit in STRING_LITERAL.findall(call.group(2)) if lit and " " not in lit and ("/" in lit or "." in lit)]
    return out


def check_script(name: str, args: list) -> None:
    """Agents read the ledger and run the layer's checks into a scratch directory; every other use of
    the run's scripts is the orchestrator's."""
    rest = [a for a in args[1:] if not a.startswith("-")]
    if name == "state.py" and len(rest) >= 2 and rest[1] in STATE_READS:
        return
    if name == "layer_checks.py" and "--scratch" in args:
        return
    deny(f"`{name}` with these arguments is the orchestrator's: report what you found instead")


def check_bash(role: str, command: str, cwd: Path, worktree: Path, run: Path, repo: Path, label: str = "") -> None:
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
        for target, how in write_targets(tokens, role, cwd, worktree, run, repo, label):
            path = Path(target).expanduser()
            check_target(role, path if path.is_absolute() else cwd / path, worktree, run, repo, how, label)


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
        label = os.environ.get("TYCHO_INTEGRATE_LABEL", "")
        if tool in EDIT_TOOLS:
            check_edit(role, tool, inp, worktree, run, repo, label)
        elif tool == "Bash":
            check_bash(role, inp.get("command", ""), cwd, worktree, run, repo, label)
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001 - a guard that cannot decide denies
        deny(f"the guard could not check this call ({type(e).__name__}: {e}); nothing was run")
    sys.exit(0)


if __name__ == "__main__":
    main()
