"""Shapes of a tycho-integrate run: paths, intake, spec, decisions and ledger events.

Every other script imports these definitions instead of restating them. Validation returns a list
of error strings; an empty list means the document is valid.
"""

import difflib
import json
import os
import re
import subprocess
from collections import Counter
from pathlib import Path

LAYERS = ("indexing", "simulation", "execution")

# How a venue is simulated (`crates/tycho-simulation/CLAUDE.md`, "Choosing an approach"): native
# Rust math; hybrid, native math over VM-indexed state; or a Solidity adapter run in the VM.
SIM_KINDS = ("native", "hybrid", "vm")

# Intake routes (assess-protocol's four cases). `integrated` stops the run.
ROUTES = ("new", "fork", "partial", "integrated")

# The closed list of what a person decides (SKILL.md "What a person decides").
DECISION_CLASSES = (
    "rule-conflict",  # two rules contradict, or a rule contradicts a precedent on main
    "no-precedent",  # a design question with no precedent
    "quantified-tradeoff",  # a conservative limit, a known simulation inaccuracy
    "spec-unknown",  # a fact the chain cannot answer
    "outside-repo",  # deployment, secrets, permissions, other teams' registries
    "relax-test",  # a skip flag, a wider tolerance, a removed assertion
    "not-converged",  # a loop that stopped without converging
)

# Why a human review comment escaped the pipeline (shadow mode); each is fixed at its cause.
ESCAPE_CAUSES = (
    "rule-judged-ok",  # a rule was loaded but its verdict was ok
    "missing-knowledge",  # project knowledge the knowledge base does not have
    "no-rule",  # no rule covers it
    "test-gap",  # a test that should have failed did not exist
    "spec-error",  # the spec was wrong
    "decision-not-escalated",  # a decision the pipeline made that was the person's
    "rule-disagreement",  # the person disagrees with a rule
)
ESCAPE_SEVERITIES = ("blocking", "suggestion", "nit")

# How the implementer answers a finding. There is no `skip` in v1: knowledge records carry no
# rating that would allow one.
ANSWER_KINDS = ("fix", "dispute", "decision")

# Statuses a finding moves through in the ledger. `waived`: a person accepted a failing check.
FINDING_STATUSES = ("open", "fixed", "refuted", "decision", "waived")

# Review severities, most severe first; every severity must be answered (no skips in v1).
SEVERITIES = ("blocking", "suggestion", "question", "nit")

# The ledger events the orchestrator may append by hand (`state.py append`). Every other event is
# written by the script that owns its rule: rounds, reviews, findings, verdicts, stops.
APPENDABLE = ("stage", "gate", "escape")

SPEC_SECTIONS = (
    "summary",
    "contracts",
    "swap",
    "state",
    "components",
    "balances",
    "quotes",
    "differences",
)

EVIDENCE_KINDS = ("tx", "block", "command", "source", "trace")

# Stages in run order. `build:<layer>` runs only for the layers the route builds.
STAGES = (
    "envcheck",
    "intake",
    "worktree",
    "research",
    "falsify",
    "gate1",
    "decide",
    "decide-review",
    "gate2",
    "build:indexing",
    "build:simulation",
    "build:execution",
    "final",
    "gate3",
    "pr",
)

# What the implementer may not change during a run (guard.py denies the edit; state.py
# check-guards rejects a round whose commits change it anyway). Globs are relative to the worktree.
GUARDED_PATHS = (
    ".claude/**",
    "protocols/testing/src/**",
    "crates/tycho-test/src/**",
    "scripts/checks/**",
    ".github/**",
)

# In protocols/testing's executor table, the only lines the implementer may add (it removes none): a
# new executor's runtime bytecode fixture, and its `EXECUTOR_MAPPING` or `CHAIN_SPECIFIC_EXECUTORS`
# entry, which let protocol-testing execute through it before it is deployed
# (`protocols/testing/README.md`, "Runtime Bytecode Fixtures").
EXECUTOR_TABLE_LINES = re.compile(
        r"\s*("
        r"const [A-Z0-9_]+_BYTECODE_JSON: &str =( include_str!\(\"\.\./fixtures/[A-Za-z0-9_]+\.runtime\.json\"\);)?"
        r"|include_str!\(\"\.\./fixtures/[A-Za-z0-9_]+\.runtime\.json\"\);"
        r"|map\.insert\(\"[a-z0-9_:]+\", [A-Z0-9_]+_BYTECODE_JSON\);"
        r"|\(\(Chain::[A-Za-z]+, \"[a-z0-9_:]+\"\), [A-Z0-9_]+_BYTECODE_JSON\),"
        # the same entry as rustfmt splits it: `(`, `(Chain::X, "system"),`, `CONST,`, `),`
        r"|\(|\),|\(Chain::[A-Za-z]+, \"[a-z0-9_:]+\"\),|[A-Z0-9_]+_BYTECODE_JSON,"
        r")?\s*"
)

# Keys of a strict integration test yaml (`protocols/testing/README.md`, "Strict mode") that relax
# a test. Adding, changing or removing a line with one of them is the person's decision (class
# relax-test, or quantified-tradeoff for a tolerance). `harness` is here so `harness: strict`
# cannot be dropped.
GUARDED_YAML_KEYS = (
    "harness",
    "skip_simulation",
    "skip_execution",
    "skip_balance_check",
    "execution_tolerance",
    "tvl_only",
)
# Guarded lines the implementer may add: a new package starts strict.
ALLOWED_NEW_YAML_LINES = ("harness: strict",)
GUARDED_YAML_RE = re.compile(r"^\s*-?\s*(" + "|".join(GUARDED_YAML_KEYS) + r")\s*:")
# An `under_cap` limit gives up capacity below the venue's bound: a quantified trade-off.
UNDER_CAP_RE = re.compile(r"\bunder_cap\b")
# A declared limit kind. Writing one is the implementer's work, from the contract; changing or
# dropping one is guarded, since that is how a failing limit check is made to pass
# (`protocols/testing/CLAUDE.md`, "Declaring limits").
LIMIT_KINDS = ("hard", "soft", "under_cap")
LIMIT_KIND_RE = re.compile(r"\bkind\s*:\s*\"?(hard|soft|under_cap)\b")
INTEGRATION_YAML_RE = re.compile(r"(^|/)integration_test[^/]*\.tycho\.ya?ml$")


def glob_match(path: str, pattern: str) -> bool:
    """`**` matches across directories, `*` within one."""
    rx = re.escape(pattern).replace(r"\*\*", "\0").replace(r"\*", "[^/]*").replace("\0", ".*")
    return re.fullmatch(rx, path) is not None


def guarded_path(rel: str) -> bool:
    return any(glob_match(rel, p) for p in GUARDED_PATHS)


def guarded_yaml_line(line: str, removed: bool) -> bool:
    """A yaml line the implementer may not add, change or remove: a test-relaxing key, an
    `under_cap` limit, or (removed or changed) a declared limit kind."""
    if GUARDED_YAML_RE.match(line) or UNDER_CAP_RE.search(line):
        return True
    return removed and LIMIT_KIND_RE.search(line) is not None


def guarded_yaml_changes(removed: list, added: list) -> list:
    """The guarded lines among an edit's or a commit's removed and added yaml lines. A limit kind
    that is removed and added back unchanged (an edit that only moves it) is not a change."""
    removed = [l.strip() for l in removed]
    added = [l.strip() for l in added]
    out = [l for l in removed if guarded_yaml_line(l, removed=l not in added)]
    out += [l for l in added if l not in removed and l not in ALLOWED_NEW_YAML_LINES and guarded_yaml_line(l, removed=False)]
    return sorted(set(out))


def line_diff(old: str, new: str) -> tuple:
    """(removed, added) lines between two texts, as multisets: lines in both cancel out."""
    o, n = Counter(old.splitlines()), Counter(new.splitlines())
    return list((o - n).elements()), list((n - o).elements())


def _executor_table_change(old: str, new: str) -> list:
    """protocols/testing's executor table: fixture and mapping lines may be added, and its
    `#[cfg(test)]` region may grow (a test beside a fixture registration is the implementer's work).
    Anything removed, and any added line outside those, is reported."""
    old_l, new_l = old.splitlines(), new.splitlines()
    test_start = next((i for i, l in enumerate(new_l) if l.strip() == "#[cfg(test)]"), None)
    bad = []
    for op, i1, i2, j1, j2 in difflib.SequenceMatcher(None, old_l, new_l, autojunk=False).get_opcodes():
        if op in ("replace", "delete"):
            bad += [f"- {l.strip()}" for l in old_l[i1:i2] if l.strip()]
        if op in ("replace", "insert"):
            bad += [f"+ {new_l[j].strip()}" for j in range(j1, j2)
                    if not EXECUTOR_TABLE_LINES.fullmatch(new_l[j]) and not (test_start is not None and j > test_start)]
    return bad


_STRING_OR_COMMENT = re.compile(r'"(?:[^"\\]|\\.)*"|//[^\n]*|/\*.*?\*/', re.S)
_STRING_OR_SPACE = re.compile(r'"(?:[^"\\]|\\.)*"|\s+')


def rust_compact(text: str) -> str:
    """Rust source without comments and whitespace (string literals kept), so two texts that differ
    only in rustfmt's layout compare equal: rustfmt reflows a whole import block or call when one
    item is added."""
    text = _STRING_OR_COMMENT.sub(lambda m: m.group(0) if m.group(0).startswith('"') else "", text)
    return _STRING_OR_SPACE.sub(lambda m: m.group(0) if m.group(0).startswith('"') else "", text)


def _strip_new(new: str, old: str, rx: re.Pattern, span: tuple = None, key_only: bool = False) -> str:
    """`new` without the matches of `rx` (within `span`) that name only systems or packages absent
    from `old`: every string literal of the match, or with `key_only` its first."""
    lo, hi = span or (0, len(new))
    out, last = [], lo
    for m in rx.finditer(new, lo, hi):
        lits = re.findall(r'"[^"]*"', m.group(0))
        if all(lit not in old for lit in (lits[:1] if key_only else lits)):
            out.append(new[last:m.start()])
            last = m.end()
    return new[:lo] + "".join(out) + new[last:]


# A new decoder arm in `register_protocol`, compacted.
_REGISTRY_ARM = re.compile(
    r'"[a-z0-9_:]+"(?:\|"[a-z0-9_:]+")*=>stream_builder\.exchange_with_decoder_context::<[A-Za-z0-9_:<>]+>'
    r"\(protocol_system,tvl_filter,None,decoder_context,?\),"
)
_SIM_IMPORT = re.compile(r"usetycho_simulation::\{.*?\};", re.S)


def _state_registry_change(old: str, new: str) -> list:
    """protocols/testing's decoder registry: a new protocol system gets a decoder arm, and its state
    type may be imported. Nothing else may change: existing arms, the default arm and the TVL filter
    stay as they are."""
    o, n = rust_compact(old), rust_compact(new)
    n = _strip_new(n, o, _REGISTRY_ARM)
    o_imp, n_imp = _SIM_IMPORT.search(o), _SIM_IMPORT.search(n)
    if o_imp and n_imp:
        # An import only names a type; its tokens may grow, none may go.
        tok = lambda s: Counter(re.findall(r"[A-Za-z_][A-Za-z0-9_]*", s))
        if tok(o_imp.group(0)) - tok(n_imp.group(0)):
            return ["an existing import was removed"]
        o, n = o[:o_imp.start()] + o[o_imp.end():], n[:n_imp.start()] + n[n_imp.end():]
    return [] if o == n else ["only a new protocol system's decoder arm (and its state type's import) may be added"]


_CLONE_MAP = re.compile(r"staticCLONE_TO_BASE_PROTOCOL:.*?HashMap::from\(\[(.*?)\]\)", re.S)
_CLONE_ENTRY = re.compile(r'\("[a-z0-9_/-]+","[a-z0-9_/-]+"\),')


def _test_runner_change(old: str, new: str) -> list:
    """protocols/testing's runner: a config-only fork's package may be mapped to its base package in
    `CLONE_TO_BASE_PROTOCOL`. Nothing else may change."""
    o, n = rust_compact(old), rust_compact(new)
    m = _CLONE_MAP.search(n)
    if m:
        n = _strip_new(n, o, _CLONE_ENTRY, m.span(1), key_only=True)
    return [] if o == n else ["only a new package's CLONE_TO_BASE_PROTOCOL entry may be added"]


# Guarded files the implementer may still extend, each with the check of a change to it: (old text,
# new text) -> what is not allowed (empty when the change is). These are the harness wiring a new
# integration needs: its executor's fixture, its decoder, and a config-only fork's base package.
GUARDED_PATH_EDITS = {
    "protocols/testing/src/execution.rs": _executor_table_change,
    "protocols/testing/src/state_registry.rs": _state_registry_change,
    "protocols/testing/src/test_runner.rs": _test_runner_change,
}


def disallowed_change(rel: str, old: str, new: str):
    """What a change to a guarded file may not do (empty when allowed); None for a guarded path
    that may not change at all. `old` and `new` are whole files, or an edit's old and new strings."""
    check = GUARDED_PATH_EDITS.get(rel)
    return None if check is None else check(old, new)


# Required fields per ledger event type. Events may carry more.
EVENT_FIELDS = {
    "stage": ("stage", "status"),  # status: started | done | stopped
    "gate": ("gate", "status"),  # gate: 1 | 2 | 3; status: approved | corrected | rejected
    "round": ("layer", "n", "head", "checks"),  # checks: green | red
    "review": ("layer", "n", "report", "open"),  # open: open findings of the layer after ingest
    "finding": ("id", "layer", "rule", "severity", "file", "claim"),
    "answer": ("id", "round", "kind"),
    "verdict": ("id", "status", "by", "round"),
    "decision_needed": ("id", "class", "question"),
    "decision": ("id", "choice", "by"),
    "spend": ("label", "usd"),
    "amendment": ("fact", "evidence"),
    "guard_violation": ("layer", "round", "paths"),
    "stop": ("reason",),
    # A comment from the person's blind review of a shadow-mode PR: what the pipeline let through.
    "escape": ("comment", "severity", "cause"),  # layer-scoped when it carries `layer`, run-scoped otherwise
}


# The git-ignored env files the tests read, from the person's checkout. worktree.py links them into
# the run's worktree; the scripts load them (the shell's own variables win). protocols/testing's
# runner reads its own `.env` from its directory, so the scripts must see it too.
ENV_FILES = ("docker/.env", ".env", "protocols/testing/.env")


def load_env_files() -> list:
    """Loads ENV_FILES into the environment; the names it set."""
    loaded = []
    for rel in ENV_FILES:
        path = repo_root() / rel
        if not path.exists():
            continue
        for line in path.read_text(errors="replace").splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.removeprefix("export ").partition("=")
            key, value = key.strip(), value.strip().strip('"').strip("'")
            if key and key not in os.environ:
                os.environ[key] = value
                loaded.append(key)
    return loaded


def rpc_vars(chain: str) -> list:
    """The variable names the repo uses for a chain's RPC, most specific first."""
    if chain == "ethereum":
        return ["RPC_URL", "ETH_RPC_URL"]
    names = {"arbitrum": ["ARBITRUM_RPC_URL", "ARB_RPC_URL"]}
    return names.get(chain, [f"{chain.upper()}_RPC_URL"])


def rpc_url(chain: str):
    """The chain's RPC URL from the environment (after `load_env_files`), or None."""
    return next((os.environ[n] for n in rpc_vars(chain) if os.environ.get(n)), None)


def git(*args, cwd=None, check=True) -> subprocess.CompletedProcess:
    """One git command, its output captured as text."""
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=check)


def repo_root() -> Path:
    """The main checkout's root, also when called from inside a worktree."""
    out = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    return Path(out).parent


def skill_dir() -> Path:
    return Path(__file__).resolve().parent.parent


def slug(protocol: str) -> str:
    """`Foo Swap v2` -> `foo-swap-v2`: the run directory, branch and worktree name."""
    s = re.sub(r"[^a-z0-9]+", "-", protocol.lower()).strip("-")
    if not s:
        raise ValueError(f"protocol name {protocol!r} has no usable characters")
    return s


def run_dir(protocol: str) -> Path:
    """The run's state. It sits under `.claude/worktrees/`, the one part of `.claude/` that Claude
    Code does not treat as protected: a headless agent's Write anywhere else under `.claude/` is
    refused as a sensitive file."""
    return repo_root() / ".claude" / "worktrees" / "integrations" / slug(protocol)


def worktree_dir(protocol: str) -> Path:
    return repo_root() / ".claude" / "worktrees" / f"integrate-{slug(protocol)}"


def branch(protocol: str) -> str:
    return f"integrate/{slug(protocol)}"


# The branch a run builds on and its PR targets.
BASE = "main"


def run_chains(run: Path) -> tuple:
    """(primary chain, every chain) of a run, from its intake (else its spec). The rounds test the
    primary; the final gates test every chain."""
    doc = {}
    for name in ("intake.json", "spec.json"):
        if (run / name).exists():
            doc = load_json(run / name)
            if doc.get("chains"):
                break
    chains = list(doc.get("chains") or ["ethereum"])
    primary = doc.get("primary_chain") or chains[0]
    return primary, [primary] + [c for c in chains if c != primary]


def load_json(path: Path):
    with open(path) as f:
        return json.load(f)


def write_json(path: Path, data) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
    tmp.replace(path)


def _need(doc: dict, key: str, kind, where: str, errors: list) -> None:
    if key not in doc or doc[key] in (None, "", [], {}):
        errors.append(f"{where}: `{key}` is required")
    elif not isinstance(doc[key], kind):
        errors.append(f"{where}: `{key}` must be {kind.__name__}")


def validate_intake(doc: dict) -> list:
    errors = []
    _need(doc, "protocol", str, "intake", errors)
    _need(doc, "chains", list, "intake", errors)
    if doc.get("primary_chain") and doc["primary_chain"] not in (doc.get("chains") or []):
        errors.append("intake: `primary_chain` must be one of `chains`")
    anchor = doc.get("anchor") or {}
    if not any(anchor.get(k) for k in ("contracts", "docs", "swap_txs")):
        errors.append("intake: one anchor is required (a contract, docs or a swap tx)")
    route = doc.get("route")
    if route is not None:
        if route.get("kind") not in ROUTES:
            errors.append(f"intake.route.kind must be one of {ROUTES}")
        layers = route.get("layers", [])
        bad = [l for l in layers if l not in LAYERS]
        if bad:
            errors.append(f"intake.route.layers has unknown layers {bad}")
        if route.get("kind") in ("fork", "partial") and not route.get("sibling"):
            errors.append("intake.route: a fork or partial route names its `sibling`")
    return errors


def validate_spec(doc: dict) -> list:
    """Checks the spec's shape and its evidence rule: a critical fact without evidence stops the
    run; an unknown is written as unknown, never as a claim."""
    errors = []
    _need(doc, "protocol", str, "spec", errors)
    if doc.get("kind") not in SIM_KINDS:
        errors.append(f"spec: `kind` must be one of {SIM_KINDS}")
    # The kind is one fact, so a later change to it is an amendment that reopens what rests on it.
    _need(doc, "kind_fact", str, "spec", errors)
    if doc.get("kind") == "hybrid" and not doc.get("vm_reads"):
        errors.append("spec: a hybrid names its `vm_reads`, the state the quote reads through the VM")
    _need(doc, "facts", list, "spec", errors)
    ids = set()
    for i, fact in enumerate(doc.get("facts", [])):
        where = f"spec.facts[{i}]"
        fid = fact.get("id")
        if not fid:
            errors.append(f"{where}: `id` is required")
        elif fid in ids:
            errors.append(f"{where}: duplicate id {fid}")
        ids.add(fid)
        if fact.get("section") not in SPEC_SECTIONS:
            errors.append(f"{where} ({fid}): `section` must be one of {SPEC_SECTIONS}")
        if not fact.get("claim"):
            errors.append(f"{where} ({fid}): `claim` is required")
        status = fact.get("status", "verified")
        evidence = fact.get("evidence") or []
        for ev in evidence:
            if ev.get("kind") not in EVIDENCE_KINDS or not ev.get("ref"):
                errors.append(f"{where} ({fid}): evidence needs a `kind` in {EVIDENCE_KINDS} and a `ref`")
        if status == "verified" and not evidence:
            errors.append(f"{where} ({fid}): a verified fact needs evidence; otherwise mark it unknown")
        if fact.get("critical") and status != "verified":
            errors.append(f"{where} ({fid}): critical fact is {status}; the run stops until it has evidence")
    for i, surface in enumerate(doc.get("mutation_surfaces", [])):
        if not surface.get("id") or not surface.get("name"):
            errors.append(f"spec.mutation_surfaces[{i}]: `id` and `name` are required")
    for i, proxy in enumerate(doc.get("proxies", [])):
        for key in ("chain", "proxy", "implementation"):
            if not proxy.get(key):
                errors.append(f"spec.proxies[{i}]: `{key}` is required")
    for i, limit in enumerate(doc.get("limits", [])):
        if limit.get("kind") not in LIMIT_KINDS:
            errors.append(f"spec.limits[{i}]: `kind` must be one of {LIMIT_KINDS}")
        if not limit.get("why") or not limit.get("fact_ids"):
            errors.append(f"spec.limits[{i}]: a limit kind needs its `why` and the `fact_ids` that show it")
    if doc.get("kind_fact") and doc["kind_fact"] not in ids:
        errors.append(f"spec: `kind_fact` {doc['kind_fact']} is not a fact")
    for i, ref in enumerate(_fact_refs(doc)):
        if ref not in ids:
            errors.append(f"spec: reference to unknown fact {ref}")
    return errors


def _fact_refs(doc: dict):
    for surface in doc.get("mutation_surfaces", []):
        yield from surface.get("fact_ids", [])
    for limit in doc.get("limits", []):
        yield from limit.get("fact_ids", [])
    for dep in doc.get("dependencies", []):
        yield from dep.get("fact_ids", [])


# A value from decisions.json that checks.toml formats into a shell command (`layer_checks.py`):
# names and paths only, so an agent-written value can never be a command of its own.
SAFE_VALUE = re.compile(r"[A-Za-z0-9_./:-]+")


def unsafe_values(doc: dict) -> list:
    """The decisions' `paths` and `agreements.protocol_system` values that are not SAFE_VALUE."""
    values = [("agreements.protocol_system", (doc.get("agreements") or {}).get("protocol_system"))]
    for k, v in (doc.get("paths") or {}).items():
        values += [(f"paths.{k}.{c}", x) for c, x in v.items()] if isinstance(v, dict) else [(f"paths.{k}", v)]
    return [f"{where} = {v!r}" for where, v in values if v is not None and not (isinstance(v, str) and SAFE_VALUE.fullmatch(v))]


def validate_decisions(doc: dict, spec: dict) -> list:
    errors = [f"decisions.{u}: only letters, digits and `_./:-`, since it is formatted into a check's command" for u in unsafe_values(doc)]
    fact_ids = {f.get("id") for f in spec.get("facts", [])}
    for i, d in enumerate(doc.get("decisions", [])):
        where = f"decisions[{i}] ({d.get('id')})"
        for key in ("id", "question", "choice"):
            if not d.get(key) and not (key == "choice" and d.get("open")):
                errors.append(f"{where}: `{key}` is required")
        for fid in d.get("facts", []):
            if fid not in fact_ids:
                errors.append(f"{where}: cites unknown spec fact {fid}")
        for rule in d.get("rules", []):
            if not rule.get("id") or not rule.get("gloss"):
                errors.append(f"{where}: each rule needs an `id` and a one-line `gloss`")
        if d.get("open") and d.get("class") not in DECISION_CLASSES:
            errors.append(f"{where}: an open decision needs a `class` in {DECISION_CLASSES}")
        bad = [l for l in d.get("layers", []) if l not in LAYERS]
        if bad:
            errors.append(f"{where}: unknown layers {bad}")
    agreements = doc.get("agreements") or {}
    for key in ("protocol_system", "component_id"):
        if not agreements.get(key):
            errors.append(f"decisions.agreements: `{key}` is required")
    paths = doc.get("paths") or {}
    if not paths.get("package"):
        errors.append("decisions.paths: `package` (the Substreams package directory name) is required")
    return errors


def validate_event(event: dict) -> list:
    kind = event.get("t")
    if kind not in EVENT_FIELDS:
        return [f"event: unknown type {kind!r}"]
    errors = [f"event {kind}: `{k}` is required" for k in EVENT_FIELDS[kind] if k not in event]
    if kind == "answer" and event.get("kind") not in ANSWER_KINDS:
        errors.append(f"event answer: `kind` must be one of {ANSWER_KINDS}")
    if kind == "answer" and event.get("kind") == "dispute" and not event.get("evidence"):
        errors.append("event answer: a dispute carries `evidence`")
    if kind == "answer" and event.get("kind") == "decision" and event.get("class") not in DECISION_CLASSES:
        errors.append(f"event answer: a decision answer carries a `class` in {DECISION_CLASSES}")
    if kind == "escape" and (event.get("cause") not in ESCAPE_CAUSES or event.get("severity") not in ESCAPE_SEVERITIES):
        errors.append(f"event escape: `cause` in {ESCAPE_CAUSES} and `severity` in {ESCAPE_SEVERITIES}")
    if kind == "verdict" and event.get("status") not in FINDING_STATUSES:
        errors.append(f"event verdict: `status` must be one of {FINDING_STATUSES}")
    if kind == "finding" and event.get("severity") not in SEVERITIES:
        errors.append(f"event finding: `severity` must be one of {SEVERITIES}")
    if kind == "decision_needed" and event.get("class") not in DECISION_CLASSES:
        errors.append(f"event decision_needed: `class` must be one of {DECISION_CLASSES}")
    if kind in ("round", "review", "finding", "guard_violation") and event.get("layer") not in LAYERS:
        errors.append(f"event {kind}: `layer` must be one of {LAYERS}")
    return errors
