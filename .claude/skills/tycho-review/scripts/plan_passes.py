#!/usr/bin/env python3
"""Plan the chunk passes of a tycho-review run.

Reads the knowledge files and the path table (`.claude/knowledge/paths.toml`) from the base
(`git show <base>:<path>`), so a change cannot edit its own checklist; gives every changed file its
file kinds and path-table layers; cuts the files into chunks
of at most --max-lines diff lines and --max-records records, by layer and directory, never splitting
a file; splits each chunk's applicable records into balanced groups of at most --group-records; and
writes, per group, a pass checklist: the chunk's files and the group's records.
It also writes `simplify.md` for the simplification pass: the records about removing and reusing
code, two counterweights, the library rows that apply, and an inventory of every item the change
adds (functions, types, constants, errors, events) with its uses at the head.
A record that names another record in backticks gets that record's text quoted under it, unless
the other record is already in the same checklist, so a rule that points elsewhere for part of its
requirement reaches the pass whole.
A `Checked by:` tool that is not set up at the base is marked `(not at base)` in the checklist, so a
record never implies a tool check that cannot run; plan.json's `tools_missing` lists those tools.

With --since-head <old head> (a follow-up review of a PR), only the files whose PR diff differs from
their PR diff at the old head get passes; plan.json's `since` lists the unchanged and dropped files,
or the reason for falling back to a full review (the delta passes a third of the diff lines).

With --stack (a stack of PRs reviewed as one change), the change is the base..head diff as for one
PR; the script also checks that the PRs' heads form one line ending at --head, and gives every file
the PRs whose own diff changes it, in plan.json's `stack` and `files` and in the checklists.

usage:
  plan_passes.py --base <rev> --head <rev> --out <dir> [--pr]       committed change
  plan_passes.py --base <rev> --head <rev> --pr --since-head <rev> --out <dir>   follow-up of a PR
  plan_passes.py --base <rev> --head <rev> --stack <n>=<rev>... --out <dir>       a stack, bottom PR first
  plan_passes.py --base <rev> --worktree --out <dir>                 working changes (incl. untracked)
  plan_passes.py --base <rev> --paths <path>... --out <dir>          files reviewed whole

Writes <out>/plan.json, <out>/pass-<n>.md and <out>/files.txt, and prints a summary. Problems
(a knowledge file or a pointer's source section missing at the base) go to plan.json's `problems`
and are reported as findings against the skill, never skipped silently.
"""

import argparse
import json
import re
import subprocess
import sys
import tomllib
from collections import Counter, defaultdict
from pathlib import Path

KNOWLEDGE = ".claude/knowledge"
PATHS = f"{KNOWLEDGE}/paths.toml"  # the path table
MARKER = "<!-- pieces -->"

TEST_MARK = re.compile(r"#\[(tokio::)?test|#\[rstest|mod tests")
CHECKED = re.compile(r"^Checked by: (.+)$", re.M)

# The simplification pass: records about what a change can remove, reuse or do once, and the
# counterweights that keep it from removing a type that carries an invariant.
SIMPLIFY = [
    "remove-what-the-change-made-redundant", "remove-single-use-indirection", "no-single-use-interfaces",
    "single-definition-of-logic", "fact-lives-once",
    "repeat-the-data-not-the-code", "typed-deserialization-once", "single-pass-over-collection",
    "data-clumps-become-a-type", "error-names-its-cause", "minimal-trait-surface",
    "no-twin-methods-for-api-change", "remove-redundant-tests", "reuse-fixture-builders", "store-graph-minimal",
]
COUNTERWEIGHTS = ["invalid-states-unrepresentable", "invariants-at-construction"]
DEFINITION = [
    (r"\.rs$", re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|const\s+|unsafe\s+)*"
                         r"(fn|struct|enum|trait|type|const|static|mod)\s+([A-Za-z_]\w*)")),
    (r"\.sol$", re.compile(r"^\s*(function|event|error|struct|enum|modifier|contract|library|interface)\s+(\w+)"
                          r"|^\s*[\w\[\]]+\s+(?:(?:public|private|internal)\s+)?(constant|immutable)\s+(\w+)")),
    (r"\.py$", re.compile(r"^\s*(def|class)\s+(\w+)")),
]


def git(*args, check=True):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=check).stdout


def kinds(path: str, text: str) -> set:
    """File kinds of candidates/applicability.md; `text` is the diff (or the file) of `path`."""
    k = {"any"}
    name = path.rsplit("/", 1)[-1]
    if path.startswith("protocols/substreams/"):
        k.add("substreams")
    elif path.endswith(".rs"):
        k.add("rust")
    if path.endswith(".rs") and ("/tests/" in f"/{path}" or TEST_MARK.search(text)):
        k.add("rust-test")
    if path.endswith(".sol"):
        k.add("solidity")
        if name.endswith((".t.sol", ".s.sol")) or re.search(r"(^|/)(test|script)/", path):
            k.add("solidity-test")
    if re.match(r"integration_test.*\.tycho\.yaml$", name) or path.startswith(
            ("protocols/testing/", "crates/tycho-integration-test/")):
        k.add("integration-test")
    if path.endswith(".md") or path.startswith("docs/"):
        k.add("docs")
    if path.endswith((".yaml", ".yml", ".json", ".toml")) and not path.startswith(".github/") \
            and name != "Cargo.toml":
        k.add("config")
    if name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
        k.add("cargo")
    if path.startswith(".github/"):
        k.add("ci")
    if path.endswith(".sql") or "/migrations/" in f"/{path}":
        k.add("sql")
    if path.startswith("crates/tycho-client-py/"):
        k.add("python")
    elif path.endswith((".sh", ".js", ".ts", ".py")):
        k.add("script")
    return k


def glob_rx(pattern: str):
    """A path glob as a regex: `**` matches any number of segments, `*` and `?` stay in one."""
    parts = [re.escape(p).replace(r"\*", "[^/]*").replace(r"\?", "[^/]") for p in pattern.split("**")]
    return re.compile(".*".join(parts))


def layer_patterns(paths: list):
    """A layer's `paths` as (included, excluded) regexes; a `!` pattern excludes."""
    return ([glob_rx(p) for p in paths if not p.startswith("!")],
            [glob_rx(p[1:]) for p in paths if p.startswith("!")])


def in_layer(path: str, patterns) -> bool:
    included, excluded = patterns
    return any(rx.fullmatch(path) for rx in included) and not any(rx.fullmatch(path) for rx in excluded)


def load_table(text: str):
    """The path table as (layer, patterns, knowledge files, contract), in review order."""
    return [(l["name"], layer_patterns(l["paths"]), l["knowledge"], l.get("contract", False))
            for l in tomllib.loads(text)["layer"]]


def rows(path: str, table):
    hit = [(layer, files) for layer, patterns, files, _ in table if in_layer(path, patterns)]
    return hit or [("general", [])]


def uncovered(table: dict, info: dict) -> dict:
    """The changed files in the `uncovered` layer, by the table entry they match."""
    out = defaultdict(list)
    for layer in table["layer"]:
        if layer["name"] != "uncovered":
            continue
        for p in layer["paths"]:
            if p.startswith("!"):
                continue
            rx = glob_rx(p)
            for f, i in info.items():
                if "uncovered" in i["layers"] and rx.fullmatch(f):
                    out[p.removesuffix("/**")].append(f)
    return dict(out)


def language_files(k: set) -> list:
    out = []
    if k & {"rust", "rust-test", "substreams"}:
        out += ["rust", "libraries-rust"]
    if "solidity" in k:
        out.append("solidity")
    if "python" in k:
        out.append("python")
    return out


# ------------------------------------------------------------------ records


def read_base(base: str, path: str):
    r = subprocess.run(["git", "show", f"{base}:{path}"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def parse_records(name: str, text: str):
    """Records below the marker: `### id` blocks and library table rows, with their Applies kinds."""
    if MARKER not in text:
        return []
    body = text.split(MARKER, 1)[1]
    out, section = [], ""
    for block in re.split(r"(?m)^(?=## |### )", body):
        if block.startswith("## "):
            section = block.splitlines()[0][3:].strip()
            for line in block.splitlines():
                cells = [c.strip() for c in line.strip().strip("|").split(" | ")]
                if line.startswith("| ") and len(cells) == 6 and cells[0] not in ("Id",) and not set(cells[0]) <= set("-"):
                    out.append(dict(id=cells[0], file=name, section=section, text=line,
                                    applies={x.strip() for x in cells[5].split(",") if x.strip()},
                                    header="| Id | Need | Use | Instead of | Spot | Applies |\n|---|---|---|---|---|---|"))
        elif block.startswith("### "):
            rid = block.splitlines()[0][4:].strip()
            m = re.search(r"^Applies: ([^·\n]+)", block, re.M)
            out.append(dict(id=rid, file=name, section=section, text=block.strip(),
                            applies={x.strip() for x in m.group(1).split(",")} if m else None))
    return out


def section_text(doc: str, section: str):
    """The text under a heading path 'A › B' (prefix match per level), optionally ', step N'."""
    step = None
    m = re.search(r",\s*step (\d+)$", section)
    if m:
        step, section = int(m.group(1)), section[:m.start()]
    parts = [p.strip() for p in section.split("›")]
    lines, start, level = doc.splitlines(), 0, 0
    for part in parts:
        for i in range(start, len(lines)):
            h = re.match(r"^(#+) (.+)$", lines[i])
            if h and len(h.group(1)) > level and h.group(2).strip().startswith(part):
                start, level = i + 1, len(h.group(1))
                break
        else:
            return None
    end = next((i for i in range(start, len(lines))
                if (h := re.match(r"^(#+) ", lines[i])) and len(h.group(1)) <= level), len(lines))
    text = "\n".join(lines[start:end]).strip()
    if step:
        items = re.split(r"(?m)^(?=\d+\. )", text)
        pick = [it for it in items if it.startswith(f"{step}. ")]
        text = pick[0].strip() if pick else None
    return text


def tools_at(base: str):
    """Which `Checked by:` tools the base has: (tool, check) -> bool. Clippy, rustfmt, forge and CI
    always run; ast-grep, dylint and cargo-deny need their config; a script needs a file named
    after its check."""
    tree = git("ls-tree", "-r", "--name-only", base).split()
    stems = {re.sub(r"[-_]", "", Path(t).stem.lower()) for t in tree}
    cargo = read_base(base, "Cargo.toml") or ""
    config = {"ast-grep": "sgconfig.yml" in tree, "dylint": "dylint" in cargo, "cargo-deny": "deny.toml" in tree}

    def present(tool: str, check: str) -> bool:
        if tool in config:
            return config[tool]
        if tool in ("repo-script", "pr-script"):
            return re.sub(r"[-_]", "", check.lower()) in stems
        return True
    return present


def mark_checked(text: str, present, missing: set) -> str:
    """Rewrite a record's `Checked by:` line: tools absent at the base get `(not at base)`."""
    def sub(m):
        parts = []
        for part in m.group(1).split("; "):
            pm = re.match(r"(\S+) `([^`]+)`", part)
            if pm and not present(*pm.groups()):
                missing.add(f"{pm.group(1)} `{pm.group(2)}`")
                part += " (not at base)"
            parts.append(part)
        line = "Checked by: " + "; ".join(parts)
        if all(p.endswith("(not at base)") for p in parts):
            line += " — no tool runs it: the pass checks it in full"
        return line
    return CHECKED.sub(sub, text)


def added_items(path: str, text: str, whole: bool):
    """(kind, name) for each definition on an added line (every line of a whole file)."""
    rx = next((r for pat, r in DEFINITION if re.search(pat, path)), None)
    if rx is None:
        return []
    out = []
    for line in text.splitlines():
        if not whole:
            if not line.startswith("+") or line.startswith("+++"):
                continue
            line = line[1:]
        m = rx.match(line)
        if m:
            kind, name = [g for g in m.groups() if g][:2]
            if kind != "mod" and not re.match(r"test", name, re.I):  # tests have their own records
                out.append((kind, name))
    return out


def uses(path: str, kind: str, name: str, rev):
    """Lines in the item's own package naming `name` at `rev` (the working tree when None), the
    definition included; a function counts only where it is called or named with `::`."""
    parts = path.split("/")
    scope = "/".join(parts[:3] if parts[0] == "protocols" else parts[:2])
    if kind in ("fn", "def", "function", "modifier"):
        pat = ["-E", "-e", rf"(^|[^A-Za-z0-9_]){name}\s*(\(|::<)", "-e", rf"::{name}([^A-Za-z0-9_]|$)"]
    else:
        pat = ["-w", "-e", name]
    args = ["grep", "-c", *pat] + ([rev] if rev else []) + ["--", scope]
    return sum(int(l.rsplit(":", 1)[1]) for l in git(*args, check=False).splitlines() if ":" in l)


def changed_lines(diff: str) -> list:
    """A diff's `+`/`-` lines without the file headers: what the change does to the file, whatever
    the base it is taken against."""
    return [l for l in diff.splitlines() if l[:1] in "+-" and not l.startswith(("+++", "---"))]


def follow_up(a, base: str, files: list, diff_of: dict) -> dict:
    """The delta of a follow-up review: the files whose PR diff differs from their PR diff at the old
    head (`--since-head`), so a file that only picked up a merge of the base branch is not in it.
    `delta_lines` counts the diff lines that differ between the two PR diffs (added or dropped since
    the old head), not the delta files' whole diffs. Falls back to a full review when the old head is
    unknown or `delta_lines` passes a third of the PR's diff lines."""
    old = git("rev-parse", "--verify", "-q", f"{a.since_head}^{{commit}}", check=False).strip()
    res = dict(old_head=old or a.since_head, delta=[], unchanged=[], dropped=[], fallback=None,
               delta_lines=0, pr_lines=sum(len(changed_lines(diff_of[f])) for f in files))
    if not old:
        res.update(delta=files, fallback=f"the old head {a.since_head} is not in this clone")
        return res
    rng = f"{base}...{old}"
    old_files = set(git("diff", "--name-only", rng).split())
    for f in sorted(set(files) | old_files):
        before = Counter(changed_lines(git("diff", rng, "--", f, check=False))) if f in old_files else Counter()
        now = Counter(changed_lines(diff_of[f])) if f in diff_of else Counter()
        if before == now:
            res["unchanged"].append(f)
            continue
        res["delta_lines"] += sum(((now - before) + (before - now)).values())
        res["delta" if f in diff_of else "dropped"].append(f)
    if res["delta_lines"] * 3 > res["pr_lines"]:
        res["fallback"] = (f"the delta has {res['delta_lines']} of the PR's {res['pr_lines']} diff lines, "
                           "more than a third")
        res["delta"] = files
    return res


def stack_of(entries: list, base: str, head: str, files: list) -> list:
    """The stack's PRs, bottom first, each with its head and the change's files its own diff changes
    (against the head of the PR below it; the bottom one against the base). Exits when the heads do
    not form one line from the base to the change's head, so a gap or a PR on another branch is never
    reviewed silently as part of the change."""
    prs, below = [], base
    for entry in entries:
        n, sep, rev = entry.partition("=")
        if not sep:
            sys.exit(f"--stack takes <number>=<head>, not {entry}")
        sha = git("rev-parse", "--verify", "-q", f"{rev}^{{commit}}", check=False).strip()
        if not sha:
            sys.exit(f"--stack: the head {rev} of #{n} is not in this clone")
        if subprocess.run(["git", "merge-base", "--is-ancestor", below, sha]).returncode != 0:
            sys.exit(f"--stack: #{n}'s head {sha[:12]} does not contain "
                     + (f"#{prs[-1]['n']}'s head {below[:12]}" if prs else f"the base {below[:12]}"))
        changed = set(git("diff", "--name-only", f"{base}...{sha}" if not prs else f"{below}..{sha}").split())
        prs.append(dict(n=n, head=sha, files=[f for f in files if f in changed]))
        below = sha
    if below != head:
        sys.exit(f"--stack: the top PR's head {below[:12]} is not --head {head[:12]}")
    return prs


# ------------------------------------------------------------------ main


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--head")
    ap.add_argument("--worktree", action="store_true")
    ap.add_argument("--paths", nargs="+")
    ap.add_argument("--pr", action="store_true", help="the target is a PR: the `pr` kind applies")
    ap.add_argument("--out", required=True)
    ap.add_argument("--max-lines", type=int, default=6000)
    ap.add_argument("--max-records", type=int, default=320,
                    help="cut a chunk before its checklist passes this many records (a file is never split)")
    ap.add_argument("--group-records", type=int, default=50,
                    help="split each chunk's records into balanced groups of at most this many, one pass each")
    ap.add_argument("--since-head",
                    help="follow-up review: plan only the files whose PR diff differs from the one at this old head")
    ap.add_argument("--stack", nargs="+", metavar="N=REV",
                    help="the target is a stack of PRs: each PR's number and head, bottom first; implies --pr")
    a = ap.parse_args()
    if a.since_head and not a.head:
        sys.exit("--since-head needs --head")
    if a.stack and (not a.head or a.paths or a.worktree):
        sys.exit("--stack needs --head, and no --paths or --worktree")
    a.pr = a.pr or bool(a.stack)
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    base = git("rev-parse", a.base).strip()
    table_text = read_base(base, PATHS)
    if table_text is None:
        sys.exit(f"{PATHS} is missing at the base {base[:12]}: no path table to plan by")
    table = load_table(table_text)
    layer_order = [layer for layer, _, _, _ in table]
    if "general" not in layer_order:
        layer_order.append("general")
    contract_layers = {layer for layer, _, _, contract in table if contract}

    # file list and per-file diff text
    if a.paths:
        files = git("ls-files", "--cached", "--others", "--exclude-standard", "--", *a.paths).split()
        diff_of = {f: Path(f).read_text(errors="replace") if Path(f).is_file() else "" for f in files}
    else:
        if a.worktree:
            files = git("diff", "--name-only", base).split()
            files += git("ls-files", "--others", "--exclude-standard").split()
            rng = [base]
        else:
            head = git("rev-parse", a.head).strip()
            files = git("diff", "--name-only", f"{base}...{head}").split()
            rng = [f"{base}...{head}"]
        diff_of = {}
        for f in files:
            d = git("diff", *rng, "--", f, check=False)
            if not d and Path(f).is_file():  # untracked: the whole file is new
                d = "".join("+" + l for l in Path(f).read_text(errors="replace").splitlines(True))
            diff_of[f] = d
    files = sorted(set(files))
    if not files:
        sys.exit("empty file list: nothing to review")
    stack = stack_of(a.stack, base, head, files) if a.stack else []
    since = follow_up(a, base, files, diff_of) if a.since_head else None
    if since and not since["fallback"]:
        files = since["delta"]
    (out / "files.txt").write_text("".join(f + "\n" for f in files))

    info = {}
    for f in files:
        d = diff_of[f]
        if a.paths:  # a whole file: all its lines
            n = len(d.splitlines())
        else:
            n = sum(1 for l in d.splitlines() if l[:1] in "+-" and not l.startswith(("+++", "---")))
        k = kinds(f, d)
        if a.pr:
            k.add("pr")
        rs = rows(f, table)
        know = {"general"} | {x for _, fs in rs for x in fs} | set(language_files(k))
        if a.pr:
            know.add("version_control")
        info[f] = dict(lines=n, kinds=sorted(k), layers=[l for l, _ in rs], knowledge=sorted(know),
                       dir="/".join(f.split("/")[:3]))
        if stack:
            info[f]["prs"] = [p["n"] for p in stack if f in p["files"]]

    # knowledge from the base, with the existence check
    problems, records, docs = [], {}, {}
    for name in sorted({k for f in files for k in info[f]["knowledge"]} | {"architecture"}):
        text = read_base(base, f"{KNOWLEDGE}/{name}.md")
        if text is None:
            if name in ("general",) or any(name in info[f]["knowledge"] for f in files):
                problems.append(f"{KNOWLEDGE}/{name}.md is missing at the base {base[:12]}")
            continue
        records[name] = parse_records(name, text)

    # chunks: ordered by layer then directory, filled up to max-lines and max-records, a file never split
    def n_records(chunk):
        kinds_in = set().union(*(set(info[f]["kinds"]) for f in chunk))
        know = set().union(*(set(info[f]["knowledge"]) for f in chunk))
        return sum(1 for name in know for r in records.get(name, [])
                   if r["applies"] is None or r["applies"] & kinds_in)

    order = sorted(files, key=lambda f: (min(layer_order.index(l) for l in info[f]["layers"]), info[f]["dir"], f))
    chunks, cur, size = [], [], 0
    for f in order:
        n = info[f]["lines"]
        if cur and (size + n > a.max_lines or n_records(cur + [f]) > a.max_records):
            chunks.append(cur)
            cur, size = [], 0
        cur.append(f)
        size += n
    if cur:
        chunks.append(cur)
    # a chunk of a few lines would cost a whole pass: fold it into the chunk it adds fewest records to
    for small in [c for c in chunks if sum(info[f]["lines"] for f in c) < a.max_lines // 20]:
        others = [c for c in chunks if c is not small
                  and sum(info[f]["lines"] for f in c + small) <= a.max_lines]
        if others:
            best = min(others, key=lambda c: n_records(c + small) - n_records(c))
            best += small
            chunks.remove(small)

    def pointer_source(rec):
        m = re.search(r"^Source: (.+)$", rec["text"], re.M)
        if not m:
            return ""
        blocks = []
        for part in m.group(1).split("; "):
            pm = re.match(r"`([^`]+)` § (.+)$", part.strip())
            if not pm:
                continue
            path, sec = pm.groups()
            if path not in docs:
                docs[path] = read_base(base, path)
            body = section_text(docs[path], sec) if docs[path] else None
            if body is None:
                problems.append(f"{rec['id']}: source `{path}` § {sec} is missing at the base")
                continue
            blocks.append(f"Source text ({path} § {sec}):\n\n" + "\n".join("> " + l for l in body.splitlines()))
        return "\n\n".join(blocks)

    # every record at the base, from every knowledge file, so a reference into a file this chunk does
    # not load (another layer's half of a rule) still resolves
    every_record = {}
    for p in git("ls-tree", "--name-only", f"{base}:{KNOWLEDGE}", check=False).split():
        if p.endswith(".md"):
            name = p[:-3]
            recs = records[name] if name in records else parse_records(name, read_base(base, f"{KNOWLEDGE}/{p}") or "")
            for r in recs:
                every_record.setdefault(r["id"], r)

    def referenced_records(rec, shown):
        """The text of each record `rec` names in backticks that is not in `shown`, one level deep:
        a rule that points to another for part of its requirement reaches the pass whole, whatever
        group or file the other one lands in."""
        blocks = []
        for rid in dict.fromkeys(re.findall(r"`([a-z0-9][a-z0-9-]*)`", rec["text"])):
            ref = every_record.get(rid)
            if ref is None or rid == rec["id"] or rid in shown:
                continue
            body = ref["header"] + "\n" + ref["text"] if "header" in ref else ref["text"]
            blocks.append(f"Referenced record ({KNOWLEDGE}/{ref['file']}.md):\n\n"
                          + "\n".join("> " + l for l in body.splitlines()))
        return "\n\n".join(blocks)

    contract = any(l in contract_layers for f in files for l in info[f]["layers"])
    present, missing = tools_at(base), set()
    plan = dict(base=base, head=a.head or ("worktree" if a.worktree else "paths"), pr=a.pr,
                contract_checks=contract, problems=problems, passes=[])
    if stack:
        plan["stack"] = [dict(n=p["n"], head=p["head"], files=len(p["files"])) for p in stack]
    if since:
        plan.update(since={k: v for k, v in since.items() if k != "delta"})
    # each chunk's records, in checklist order, split into balanced groups: one pass per group, every
    # group seeing all of the chunk's files (review-corpus measurements/limits-1671: groups of 50 found 14 of 15 real
    # issues at about 3x the cost of one pass per chunk, which found 10)
    i = 0
    for c, chunk in enumerate(chunks, 1):
        kinds_in = set().union(*(set(info[f]["kinds"]) for f in chunk))
        know = sorted(set().union(*(set(info[f]["knowledge"]) for f in chunk)))
        units = [(name, r) for name in know for r in records.get(name, [])
                 if r["applies"] is None or r["applies"] & kinds_in]
        k = max(1, -(-len(units) // a.group_records))
        size = -(-len(units) // k) if units else 0
        groups = [units[j:j + size] for j in range(0, len(units), size)] if units else [[]]
        for g, group in enumerate(groups, 1):
            i += 1
            title = f"# Pass {i}: checklist" + (f" (chunk {c}, group {g} of {len(groups)})" if len(groups) > 1 else "")
            lines = [title, "",
                     f"Files ({len(chunk)}, {sum(info[f]['lines'] for f in chunk)} diff lines):", ""]
            lines += [f"- `{f}` ({info[f]['lines']} lines; kinds: {', '.join(k for k in info[f]['kinds'] if k != 'any')}"
                      + (f"; PRs: {', '.join('#' + n for n in info[f]['prs'])}" if stack else "") + ")"
                      for f in chunk]
            if len(groups) > 1:
                lines += ["", f"Other passes check the chunk's other {len(units) - len(group)} records against these files."]
            ids, last_name, last_header = [], None, None
            shown = {r["id"] for _, r in group}
            for name, r in group:
                if name != last_name:
                    lines += ["", f"## {name}.md", ""]
                    last_name, last_header = name, None
                ids.append(r["id"])
                if "header" in r:
                    if last_header != r["section"]:
                        lines += ["", f"### {r['section']}", "", r["header"]]
                        last_header = r["section"]
                    lines.append(r["text"])
                else:
                    last_header = None
                    lines += [mark_checked(r["text"], present, missing), ""]
                    src = pointer_source(r)
                    if src:
                        lines += [src, ""]
                    refs = referenced_records(r, shown)
                    if refs:
                        lines += [refs, ""]
            (out / f"pass-{i}.md").write_text("\n".join(lines).rstrip() + "\n")
            plan["passes"].append(dict(n=i, chunk=c, group=g, groups=len(groups), files=chunk,
                                       diff_lines=sum(info[f]["lines"] for f in chunk),
                                       knowledge=sorted({name for name, _ in group}), pieces=len(ids), piece_ids=ids,
                                       layers=sorted({l for f in chunk for l in info[f]["layers"]})))
    # the simplification pass: one checklist for the whole change
    by_id = {r["id"]: r for recs in records.values() for r in recs}
    kinds_all = set().union(*(set(info[f]["kinds"]) for f in files))
    simp = [by_id[k] for k in SIMPLIFY + COUNTERWEIGHTS if k in by_id]
    libs = [r for name in ("libraries-rust", "libraries-tycho") for r in records.get(name, [])
            if "header" in r and (r["applies"] is None or r["applies"] & kinds_all)]
    rev = None if (a.paths or a.worktree) else git("rev-parse", a.head).strip()
    items = [(f, k, n) for f in files for k, n in added_items(f, diff_of[f], bool(a.paths))]
    lines = ["# Simplification pass: checklist", "",
             f"Items the change adds ({len(items)}; uses = lines of the item's own package naming it at the head, "
             "its definition included; a function counts where it is called):",
             "", "| File | Kind | Name | Uses |", "|---|---|---|---|"]
    for f, k, n in items[:300]:
        lines.append(f"| `{f}` | {k} | `{n}` | {uses(f, k, n, rev)} |")
    if len(items) > 300:
        lines.append(f"| … | | {len(items) - 300} more, not listed | |")
    lines += ["", "## Records", ""]
    shown = {r["id"] for r in simp} | {r["id"] for r in libs}
    for r in simp:
        lines += [("Counterweight. " if r["id"] in COUNTERWEIGHTS else "") + mark_checked(r["text"], present, missing), ""]
        refs = referenced_records(r, shown)
        if refs:
            lines += [refs, ""]
    if libs:
        lines += ["## Library rows (an existing helper or crate feature replaces new code)", "", libs[0]["header"]]
        lines += [r["text"] for r in libs]
    (out / "simplify.md").write_text("\n".join(lines).rstrip() + "\n")
    plan["simplify"] = dict(items=len(items), records=[r["id"] for r in simp], library_rows=len(libs))
    every = "".join(read_base(base, f"{KNOWLEDGE}/{p}") or ""
                    for p in git("ls-tree", "--name-only", f"{base}:{KNOWLEDGE}", check=False).split())
    missing_simp = [k for k in SIMPLIFY + COUNTERWEIGHTS if f"### {k}\n" not in every]
    if missing_simp:
        problems.append(f"simplification records missing at the base: {', '.join(missing_simp)}")
    plan["files"] = info
    plan["tools_missing"] = sorted(missing)
    plan["uncovered"] = uncovered(tomllib.loads(table_text), info)
    (out / "plan.json").write_text(json.dumps(plan, indent=1))
    if since:
        s = plan["since"]
        print(f"follow-up since {s['old_head'][:12]}: " + (f"FALLBACK to a full review: {s['fallback']}" if s["fallback"] else
              f"{len(files)} delta files ({s['delta_lines']} of {s['pr_lines']} diff lines), "
              f"{len(s['unchanged'])} unchanged, {len(s['dropped'])} no longer changed"))
    if stack:
        print("stack, bottom first: " + ", ".join(f"#{p['n']} at {p['head'][:12]} ({len(p['files'])} files)" for p in stack))
    print(f"{len(files)} files, {len(chunks)} chunks, {len(plan['passes'])} passes; contract checks: {'yes' if contract else 'no'}")
    for p in plan["passes"]:
        print(f"  pass {p['n']}: {len(p['files'])} files, {p['diff_lines']} diff lines, {p['pieces']} pieces, "
              f"layers {', '.join(p['layers'])}")
    print(f"  simplification pass: {plan['simplify']['items']} added items, "
          f"{len(plan['simplify']['records'])} records, {plan['simplify']['library_rows']} library rows")
    if missing:
        print(f"{len(missing)} `Checked by:` tools not set up at the base (marked in the checklists)")
    if plan["uncovered"]:
        print("uncovered, no layer knowledge: "
              + ", ".join(f"{entry} ({len(fs)} file{'s' * (len(fs) > 1)})" for entry, fs in plan["uncovered"].items()))
    for pr in problems:
        print("PROBLEM:", pr)


if __name__ == "__main__":
    main()
