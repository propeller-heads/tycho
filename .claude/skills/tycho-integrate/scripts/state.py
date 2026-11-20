#!/usr/bin/env python3
"""The run's ledger: append, replay, findings, budgets, stop rules, guards and amendments.

The ledger (`.claude/worktrees/integrations/<protocol>/ledger.jsonl`) is append-only and is the single
source of the run's state: resuming a run, `status` and the stop rules all replay it. The stop
rules are code here, not a judgement the model makes.

    state.py <protocol> init                      create the run directory and ledger
    state.py <protocol> append '<event json>'     validate and append one event
    state.py <protocol> status                    stage, rounds, spend, open items
    state.py <protocol> resume                    the stage to resume at, as JSON
    state.py <protocol> round --layer L --head H --checks green|red
    state.py <protocol> answers --layer L --round N --file answers.json
    state.py <protocol> ingest-review --layer L|auto --round N --sidecar <report>.findings.json
    state.py <protocol> open-findings --layer L   open findings as JSON
    state.py <protocol> answers-file --layer L --out F   the `--existing` file for the next review
    state.py <protocol> next --layer L            continue | clean | escalate:<reason>, as JSON
    state.py <protocol> remaining [--layer L]     USD left for the next agent call
    state.py <protocol> check-guards --worktree W --from A --to B [--layer L --round N --record]
    state.py <protocol> ask --class C --question '...' [--record D3] [--layer L]   open a question for a person
    state.py <protocol> decide --id Q1 --choice '...' [--grant-rounds N --grant-usd X] [--close]
    state.py <protocol> amend --fact S3 --evidence '...' --new '...' [--source person|chain]
"""

import argparse
import datetime
import json
import re
import subprocess
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

# Severity order for status output; every severity must be answered (no skips in v1).
SEVERITIES = ("blocking", "suggestion", "question", "nit")


def ledger_path(run: Path) -> Path:
    return run / "ledger.jsonl"


def read_events(run: Path) -> list:
    path = ledger_path(run)
    if not path.exists():
        return []
    events = []
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError as e:
                raise SystemExit(f"{path}:{n}: not JSON ({e}); the ledger is append-only, fix the line by hand")
            # Replay trusts nothing it would not have appended: a line written around `append`
            # (a gate approval, a closed finding) stops the run instead of changing its state.
            errors = schema.validate_event(event) if isinstance(event, dict) else ["not an object"]
            if errors:
                raise SystemExit(f"{path}:{n}: invalid event ({'; '.join(errors)}); the ledger is append-only, fix the line by hand")
            events.append(event)
    return events


def append(run: Path, event: dict) -> dict:
    errors = schema.validate_event(event)
    if errors:
        raise SystemExit("invalid event: " + "; ".join(errors))
    event = {"ts": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), **event}
    with open(ledger_path(run), "a") as f:
        f.write(json.dumps(event, separators=(",", ":")) + "\n")
    return event


def load_budgets() -> dict:
    with open(schema.skill_dir() / "budgets.toml", "rb") as f:
        return tomllib.load(f)


CAP_ESCALATIONS = ("round-cap", "usd-cap")


class State:
    """The run as the ledger describes it."""

    def __init__(self, events: list):
        self.events = events
        self.stages = {}
        self.gates = {}
        self.rounds = defaultdict(list)
        self.reviews = defaultdict(list)
        self.spend_total = 0.0
        self.spend_layer = defaultdict(float)
        self.spend_label = defaultdict(float)
        self.findings = {}
        self.decisions = {}  # D<n> -> {class, question, finding, layer, resolved, choice}
        self.guard_violations = defaultdict(list)
        self.accepted_guards = set()  # paths whose guarded change a person accepted
        self.stops = {}  # layer or "run" -> reason
        # layer -> the decision id by which a person closed a layer that stopped without converging
        # (an escalation answered without more rounds or dollars, and no round since).
        self.closed = {}
        self.signed_facts = set()
        self.amendments = []
        self.escapes = []
        # A person's answer to an escalation resets the layer's stop rules from that round on and
        # may grant more rounds or dollars.
        self.reset_round = defaultdict(int)
        self.grant_rounds = defaultdict(int)
        self.grant_usd = defaultdict(float)
        self.grant_usd_total = 0.0
        for e in events:
            self._apply(e)

    def _apply(self, e: dict) -> None:
        t = e["t"]
        if t == "stage":
            self.stages[e["stage"]] = e["status"]
        elif t == "gate":
            self.gates[str(e["gate"])] = e["status"]
            if str(e["gate"]) == "1" and e["status"] == "approved":
                self.signed_facts.update(e.get("facts", []))
        elif t == "round":
            self.rounds[e["layer"]].append(e)
            self.closed.pop(e["layer"], None)
            self.stops.pop(e["layer"], None)  # a new round supersedes the layer's last stop
        elif t == "review":
            self.reviews[e["layer"]].append(e)
        elif t == "spend":
            usd = float(e["usd"])
            self.spend_total += usd
            self.spend_label[e["label"]] += usd
            if e.get("layer"):
                self.spend_layer[e["layer"]] += usd
        elif t == "finding":
            self.stops.pop(e["layer"], None)  # a new or reopened finding supersedes a clean stop
            f = self.findings.get(e["id"])
            if f is None:
                self.findings[e["id"]] = {
                    "id": e["id"],
                    "layer": e["layer"],
                    "rule": e["rule"],
                    "severity": e["severity"],
                    "file": e["file"],
                    "lines": e.get("lines"),
                    "quote": e.get("quote", ""),
                    "claim": e["claim"],
                    "evidence": e.get("evidence", ""),
                    "status": "open",
                    "reopened": 0,
                    "reopen_rounds": [],
                    "answers": [],
                    "verdicts": [],
                    "fingerprint": fingerprint(e),
                }
            else:
                if e.get("reopened"):
                    f["reopened"] += 1
                    f["reopen_rounds"].append(e.get("round", 0))
                f["status"] = "open"
                f["claim"] = e["claim"]
                f["severity"] = e["severity"]
        elif t == "answer":
            f = self.findings.get(e["id"])
            if f is not None:
                f["answers"].append(e)
        elif t == "verdict":
            f = self.findings.get(e["id"])
            if f is not None:
                f["verdicts"].append(e)
                f["status"] = e["status"]
        elif t == "decision_needed":
            self.decisions[e["id"]] = {
                "id": e["id"],
                "class": e["class"],
                "question": e["question"],
                "options": e.get("options", []),
                "finding": e.get("finding"),
                "layer": e.get("layer"),
                "escalation": e.get("escalation"),
                "resolved": False,
                "choice": None,
            }
            if e.get("finding") in self.findings:
                self.findings[e["finding"]]["status"] = "decision"
        elif t == "decision":
            d = self.decisions.setdefault(e["id"], {"id": e["id"], "class": None, "question": ""})
            d["resolved"] = True
            d["choice"] = e["choice"]
            d["by"] = e["by"]
            if e.get("accept_guard") and d.get("layer"):
                self.accepted_guards.update(v["path"] for g in self.guard_violations[d["layer"]] for v in g["paths"])
            # The implementer applies the decision next round; the next review statuses the finding.
            if d.get("finding") in self.findings:
                self.findings[d["finding"]]["status"] = "open"
            layer = d.get("layer")
            if d.get("escalation") and layer:
                self.reset_round[layer] = len(self.rounds[layer])
                self.grant_rounds[layer] += int(e.get("grant_rounds", 0))
                self.grant_usd[layer] += float(e.get("grant_usd", 0))
                self.grant_usd_total += float(e.get("grant_usd", 0))
                grants = int(e.get("grant_rounds", 0)) or float(e.get("grant_usd", 0))
                # A cap answered with nothing more can only end the layer; any other escalation
                # (a guard change accepted, findings decided) continues the loop unless the person
                # said `--close`.
                if not grants and (e.get("close") or d["escalation"] in CAP_ESCALATIONS):
                    self.closed[layer] = e["id"]
                else:
                    self.closed.pop(layer, None)
            elif d.get("escalation"):
                self.grant_usd_total += float(e.get("grant_usd", 0))
        elif t == "guard_violation":
            self.guard_violations[e["layer"]].append(e)
        elif t == "stop":
            self.stops[e.get("layer", "run")] = e["reason"]
            if e["reason"] == "clean":
                self.closed.pop(e.get("layer", "run"), None)
        elif t == "amendment":
            self.amendments.append(e)
        elif t == "escape":
            self.escapes.append(e)

    def open_findings(self, layer: str) -> list:
        return [f for f in self.findings.values() if f["layer"] == layer and f["status"] in ("open", "decision")]

    def open_decisions(self, layer=None) -> list:
        return [d for d in self.decisions.values() if not d["resolved"] and (layer is None or d.get("layer") == layer)]

    def next_finding_id(self) -> str:
        return f"F{_next_number(self.findings, 'F')}"

    def next_decision_id(self) -> str:
        # Q, not D: decisions.json numbers its design decisions D<n>.
        return f"Q{_next_number(self.decisions, 'Q')}"


def _next_number(ids, prefix: str) -> int:
    nums = [int(i[len(prefix):]) for i in ids if re.fullmatch(prefix + r"\d+", i)]
    return max(nums, default=0) + 1


def fingerprint(f: dict) -> str:
    """Identity of a finding across rounds when a review does not carry the ledger id: the rule,
    the file and the quoted code (or the claim, when nothing is quoted)."""
    text = f.get("quote") or f.get("claim", "")[:120]
    text = re.sub(r"\s+", " ", text).strip()[:200]
    return f"{f.get('rule')}|{f.get('file')}|{text}"


# ---------------------------------------------------------------------------------------------
# Budgets and stop rules


def integration_cap(state: State) -> float:
    return load_budgets()["integration"]["usd"] + state.grant_usd_total


def layer_cap(state: State, layer: str) -> float:
    return load_budgets()["layer"]["usd"] + state.grant_usd[layer]


# Below this, no agent call starts (`run_agent.py` exits 3) and the stop rules escalate: one
# threshold for both, so a run never sits between "no budget" and "continue".
MIN_CALL_USD = 0.01


def remaining_usd(state: State, layer=None) -> float:
    left = integration_cap(state) - state.spend_total
    if layer:
        left = min(left, layer_cap(state, layer) - state.spend_layer[layer])
    return max(left, 0.0)


def next_step(state: State, layer: str, extra_rounds: int = 0) -> dict:
    """The stop rules. Returns {"action": "continue" | "clean" | "closed" | "escalate", "reason": ...}.

    `closed` means a person ended the layer's loop: treat it as `clean` for the stage, and the PR
    lists the findings still open.

    Order matters: an open decision and a guard violation go to a person before anything else;
    caps are checked before progress so a run never spends past them. Progress, oscillation and
    disputes count only the rounds after the person's last answer to an escalation of this layer.
    """
    budgets = load_budgets()
    since = state.reset_round[layer]
    rounds = state.rounds[layer]
    reviews = state.reviews[layer]
    open_now = state.open_findings(layer)

    # A person closed the layer after it stopped (an escalation answered with no more rounds or
    # dollars) and nothing ran since: it stays closed, `next` never re-escalates it.
    if layer in state.closed:
        return {"action": "closed", "reason": "closed", "decision": state.closed[layer], "open_findings": [f["id"] for f in open_now]}

    if state.open_decisions(layer):
        return {"action": "escalate", "reason": "decision-needed", "decisions": [d["id"] for d in state.open_decisions(layer)]}
    guard = [g for g in state.guard_violations[layer] if g["round"] > since]
    if guard:
        return {"action": "escalate", "reason": "guard", "class": "relax-test", "paths": guard[-1]["paths"]}

    last_green = bool(rounds) and rounds[-1]["checks"] == "green"
    reviewed_last = bool(reviews) and bool(rounds) and reviews[-1]["n"] == rounds[-1]["n"]
    if rounds and last_green and reviewed_last and not open_now:
        return {"action": "clean", "reason": "clean"}

    if integration_cap(state) - state.spend_total < MIN_CALL_USD:
        return {"action": "escalate", "reason": "integration-usd-cap", "class": "not-converged"}
    if layer_cap(state, layer) - state.spend_layer[layer] < MIN_CALL_USD:
        return {"action": "escalate", "reason": "usd-cap", "class": "not-converged"}
    if len(rounds) >= budgets["layer"]["rounds"] + extra_rounds + state.grant_rounds[layer]:
        return {"action": "escalate", "reason": "round-cap", "class": "not-converged"}

    # The specific causes first: an oscillation or a deadlock also stalls progress, and naming
    # the cause tells the person what to decide.
    reopened = [f["id"] for f in state.findings.values() if f["layer"] == layer and any(r > since for r in f["reopen_rounds"])]
    if reopened:
        return {"action": "escalate", "reason": "oscillation", "class": "not-converged", "findings": reopened}

    deadlocked = [f["id"] for f in state.findings.values() if f["layer"] == layer and dispute_deadlock(f, since)]
    if deadlocked:
        return {"action": "escalate", "reason": "dispute-deadlock", "class": "rule-conflict", "findings": deadlocked}

    counts = [r["open"] for r in reviews if r["n"] > since]
    if len(counts) >= 3 and counts[-1] >= counts[-3]:
        return {"action": "escalate", "reason": "no-progress", "class": "not-converged", "open": counts[-3:]}

    return {"action": "continue", "reason": "open findings" if open_now else "checks red or not reviewed"}


ESCALATION_QUESTIONS = {
    "guard": "The implementer changed a guarded path or a test-relaxing yaml line. Accept the change, or revert it?",
    "integration-usd-cap": "The run reached its dollar cap. Grant more, or stop the run?",
    "usd-cap": "The layer reached its dollar cap. Grant more, or stop?",
    "round-cap": "The layer reached its round cap without converging. Grant more rounds, or stop?",
    "no-progress": "Open findings did not go down in two rounds. Decide the open findings, grant rounds, or stop?",
    "oscillation": "A closed finding reopened (a fix undid an earlier fix). Decide which way the code goes.",
    "dispute-deadlock": "A finding was disputed twice and both sides' evidence holds; two rules may conflict. Which holds?",
}


def escalate(run: Path, state: State, layer, result: dict) -> str:
    """Opens a decision for an escalation, once per reason, so the person answers it with `decide`."""
    reason = result["reason"]
    for d in state.open_decisions(layer):
        if d.get("escalation") == reason:
            return d["id"]
    did = state.next_decision_id()
    append(run, {
        "t": "decision_needed",
        "id": did,
        "class": result.get("class", "not-converged"),
        "question": ESCALATION_QUESTIONS.get(reason, reason),
        "layer": layer,
        "escalation": reason,
        "detail": {k: v for k, v in result.items() if k not in ("action", "reason", "class")},
    })
    return did


def dispute_deadlock(f: dict, since: int = 0) -> bool:
    """The same finding disputed twice, and the review kept it open after each dispute: both sides'
    evidence holds, which usually means two rules conflict."""
    held = 0
    for answer in f["answers"]:
        if answer["kind"] != "dispute" or answer["round"] <= since:
            continue
        later = [v for v in f["verdicts"] if v["round"] >= answer["round"]]
        if later and later[0]["status"] == "open":
            held += 1
    return held >= 2


# ---------------------------------------------------------------------------------------------
# Review ingestion


def layer_of(path: str) -> str:
    """The layer a file belongs to, from `.claude/knowledge/paths.toml`. Files outside the three
    integration layers (shared types, registries, docs) count toward execution when they are
    execution config, otherwise toward the layer named by their directory, else indexing."""
    with open(schema.repo_root() / ".claude/knowledge/paths.toml", "rb") as f:
        table = tomllib.load(f)
    for layer in table.get("layer", []):
        pats = layer.get("paths", [])
        inc = [p for p in pats if not p.startswith("!")]
        exc = [p[1:] for p in pats if p.startswith("!")]
        if any(schema.glob_match(path, p) for p in inc) and not any(schema.glob_match(path, p) for p in exc):
            name = layer["name"]
            if name in schema.LAYERS:
                return name
            if "simulation" in name:
                return "simulation"
            if "execution" in name:
                return "execution"
    if path.startswith("crates/tycho-execution/"):
        return "execution"
    if path.startswith(("crates/tycho-simulation/", "protocols/adapter-integration/")):
        return "simulation"
    return "indexing"


EARLIER_STATUS = {"fixed": "fixed", "disproven": "refuted", "open": "open", "deferred": "open"}


def ingest_review(run: Path, layer: str, n: int, sidecar_path: Path) -> dict:
    """Maps a tycho-review sidecar onto the ledger: new findings get ids, findings the review
    matched to ledger ids get verdicts, a closed finding that comes back is reopened."""
    sidecar = schema.load_json(sidecar_path)
    state = State(read_events(run))
    by_print = {f["fingerprint"]: f for f in state.findings.values()}
    added, reopened, skipped_pre = [], [], 0

    def norm(text):
        return re.sub(r"\s+", " ", text or "").strip().lower()[:60]

    unmatched = []
    for e in sidecar.get("earlier", []):
        fid = e.get("id")
        if fid not in state.findings:
            # An id the ledger does not know (E<n> from an --existing file without ids): match by
            # file and claim, never by position.
            fid = next((f["id"] for f in state.findings.values()
                        if e.get("file") and f["file"] == e.get("file") and norm(f["claim"]) == norm(e.get("claim"))), None)
            if fid is None:
                unmatched.append(e.get("id"))
                continue
        status = EARLIER_STATUS.get(e.get("status"), "open")
        append(run, {"t": "verdict", "id": fid, "status": status, "by": "review", "round": n, "evidence": e.get("evidence", "")})

    state = State(read_events(run))
    for f in sidecar.get("findings", []):
        if f.get("pre_existing"):
            skipped_pre += 1
            continue
        flayer = layer if layer != "auto" else layer_of(f.get("file", ""))
        event = {
            "t": "finding",
            "layer": flayer,
            "rule": f.get("rule", "unknown"),
            "severity": f.get("severity", "question"),
            "file": f.get("file", ""),
            "lines": f.get("lines"),
            "quote": f.get("quote", ""),
            "claim": f.get("claim", ""),
            "evidence": f.get("evidence", ""),
            "round": n,
        }
        known = state.findings.get(f.get("earlier_id")) or by_print.get(fingerprint(event))
        if known is None:
            event["id"] = state.next_finding_id()
            append(run, event)
            added.append(event["id"])
        elif known["status"] in ("fixed", "refuted"):
            event["id"] = known["id"]
            event["reopened"] = True
            append(run, event)
            reopened.append(known["id"])
        # A finding the ledger already holds open needs no new event: its verdict came above.
        state = State(read_events(run))

    layers = schema.LAYERS if layer == "auto" else (layer,)
    for lay in layers:
        if layer == "auto" and not state.rounds[lay]:
            continue
        append(run, {"t": "review", "layer": lay, "n": n, "report": sidecar.get("report", str(sidecar_path)), "open": len(state.open_findings(lay))})
    return {"added": added, "reopened": reopened, "pre_existing_skipped": skipped_pre, "unmatched_earlier": unmatched}


def record_answers(run: Path, layer: str, n: int, answers_path: Path) -> dict:
    """Records the implementer's answers. Every open finding of the layer must have one; a missing
    answer is reported, and the finding stays open (never silently dropped)."""
    data = schema.load_json(answers_path)
    answers = data.get("answers", data) if isinstance(data, dict) else data
    state = State(read_events(run))
    open_ids = {f["id"] for f in state.open_findings(layer)}
    errors, recorded = [], []
    for a in answers:
        fid = a.get("id")
        if fid not in open_ids:
            errors.append(f"{fid}: not an open finding of {layer}")
            continue
        event = {"t": "answer", "id": fid, "round": n, "kind": a.get("kind"), "evidence": a.get("evidence", ""), "note": a.get("note", "")}
        if a.get("kind") == "decision":
            event["class"] = a.get("class")
        problems = schema.validate_event(event)
        if problems:
            errors.extend(f"{fid}: {p}" for p in problems)
            continue
        append(run, event)
        recorded.append(fid)
        if a.get("kind") == "decision":
            state = State(read_events(run))
            append(run, {
                "t": "decision_needed",
                "id": state.next_decision_id(),
                "class": a["class"],
                "question": a.get("question") or a.get("note") or state.findings[fid]["claim"],
                "options": a.get("options", []),
                "finding": fid,
                "layer": layer,
            })
    missing = sorted(open_ids - set(recorded) - {e.split(":")[0] for e in errors})
    return {"recorded": recorded, "missing": missing, "errors": errors}


def answers_file(state: State, layer: str) -> str:
    """The `--existing` file for the next review: each open finding with its ledger id and the
    implementer's latest answer, so the review statuses it under the same id and its verifier
    judges a dispute's evidence."""
    lines = [
        f"# Open findings of the {layer} layer, with the implementer's answers",
        "",
        "Each entry keeps its ledger id. Report every entry's status under that id (fixed, open, "
        "disproven): a fix is reviewed as new code; a dispute is settled by verifying its evidence.",
        "",
    ]
    for f in state.open_findings(layer):
        lines.append(f"## {f['id']} · {f['severity']} · {f['rule']}")
        lines.append(f"- at: `{f['file']}`" + (f":{f['lines']}" if f.get("lines") else ""))
        lines.append(f"- claim: {f['claim']}")
        if f.get("quote"):
            lines.append("- quoted code:")
            lines.append("  ```")
            lines.extend("  " + q for q in f["quote"].splitlines())
            lines.append("  ```")
        if f["answers"]:
            a = f["answers"][-1]
            lines.append(f"- answer (round {a['round']}): {a['kind']}" + (f" — {a.get('note')}" if a.get("note") else ""))
            if a.get("evidence"):
                lines.append(f"- evidence: {a['evidence']}")
        decision = next((d for d in state.decisions.values() if d.get("finding") == f["id"] and d["resolved"]), None)
        if decision:
            lines.append(f"- decided by a person ({decision['id']}): {decision['choice']}")
        lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------------------------
# Guards


def check_guards(worktree: Path, rev_from: str, rev_to: str) -> list:
    """Paths and yaml lines the implementer's commits changed but may not; with `rev_to` HEAD, the
    worktree's uncommitted changes too, since the layer checks run on the working tree."""
    def git(*args):
        return subprocess.run(["git", "-C", str(worktree), *args], capture_output=True, text=True, check=True).stdout

    def show(rev, path):
        """A file at a revision; None where it does not exist there (added or deleted)."""
        proc = subprocess.run(["git", "-C", str(worktree), "show", f"{rev}:{path}"], capture_output=True, text=True)
        return proc.stdout if proc.returncode == 0 else None

    def changed_lines(sha, path):
        removed, added = [], []
        for line in git("show", "--format=", "-U0", sha, "--", path).splitlines():
            if line.startswith(("+++", "---")):
                continue
            if line[:1] == "-":
                removed.append(line[1:])
            elif line[:1] == "+":
                added.append(line[1:])
        return removed, added

    violations = []
    for sha in git("rev-list", f"{rev_from}..{rev_to}").split():
        for path in git("diff-tree", "--no-commit-id", "--name-only", "-r", sha).split("\n"):
            if not path:
                continue
            violations += guarded_change(sha[:10], path, show(f"{sha}^", path), show(sha, path), lambda: changed_lines(sha, path))
    if rev_to == "HEAD":
        for line in git("status", "--porcelain", "--untracked-files=all", "--no-renames").splitlines():
            path = line[3:].strip().strip('"')
            if not (schema.guarded_path(path) or schema.INTEGRATION_YAML_RE.search(path)):
                continue
            file = worktree / path
            new = file.read_text(errors="replace") if file.is_file() else None
            old = show("HEAD", path)
            violations += guarded_change("uncommitted", path, old, new, lambda: schema.line_diff(old or "", new or ""))
    return violations


def guarded_change(where: str, path: str, old, new, yaml_lines) -> list:
    """The violations of one file's change from `old` to `new` (None: the file does not exist)."""
    if schema.guarded_path(path):
        if new is None:
            return [{"commit": where, "path": path, "why": "deletes a guarded file"}]
        bad = schema.disallowed_change(path, old or "", new)
        if bad is None:
            return [{"commit": where, "path": path, "why": "guarded path"}]
        return [{"commit": where, "path": path, "why": f"changes beyond the integration's wiring: {bad}"}] if bad else []
    if schema.INTEGRATION_YAML_RE.search(path):
        return [{"commit": where, "path": path, "why": f"changes `{line}`"} for line in schema.guarded_yaml_changes(*yaml_lines())]
    return []


# ---------------------------------------------------------------------------------------------
# Amendments


def amend(run: Path, fact: str, evidence: str, new: str, source: str) -> dict:
    """Records a spec amendment and says what it reopens: the decisions that cite the fact and the
    layers those decisions shaped. A fact signed at Gate 1 goes back to a person first."""
    state = State(read_events(run))
    decisions_path = run / "decisions.json"
    reopened, layers = [], set()
    if decisions_path.exists():
        for d in schema.load_json(decisions_path).get("decisions", []):
            if fact in d.get("facts", []):
                reopened.append(d["id"])
                layers.update(d.get("layers", []))
    needs_person = fact in state.signed_facts
    append(run, {
        "t": "amendment",
        "fact": fact,
        "evidence": evidence,
        "new": new,
        "source": source,
        "reopens": reopened,
        "layers": sorted(layers),
        "needs_person": needs_person,
    })
    return {"reopens": reopened, "layers": sorted(layers), "needs_person": needs_person}


# ---------------------------------------------------------------------------------------------
# Status and resume


def route_layers(run: Path) -> list:
    intake = run / "intake.json"
    if intake.exists():
        route = schema.load_json(intake).get("route") or {}
        if route.get("layers"):
            return [l for l in schema.LAYERS if l in route["layers"]]
    return list(schema.LAYERS)


def resume_point(run: Path, state: State) -> dict:
    layers = route_layers(run)
    for stage in schema.STAGES:
        if stage.startswith("build:") and stage.split(":", 1)[1] not in layers:
            continue
        if state.stages.get(stage) != "done":
            out = {"stage": stage, "status": state.stages.get(stage, "not started")}
            if stage.startswith("build:"):
                layer = stage.split(":", 1)[1]
                out["layer"] = layer
                out["rounds"] = len(state.rounds[layer])
                out["next"] = next_step(state, layer)
            return out
    return {"stage": "done", "status": "done"}


def status_text(run: Path, state: State) -> str:
    budgets = load_budgets()
    out = [f"Run: {run}", f"Resume at: {json.dumps(resume_point(run, state))}"]
    out.append("Stages: " + ", ".join(f"{s}={state.stages[s]}" for s in schema.STAGES if s in state.stages))
    out.append("Gates: " + (", ".join(f"{g}={s}" for g, s in sorted(state.gates.items())) or "none yet"))
    out.append(f"Spend: ${state.spend_total:.2f} of ${budgets['integration']['usd']}")
    for label, usd in sorted(state.spend_label.items()):
        out.append(f"  {label}: ${usd:.2f}")
    for layer in route_layers(run):
        rounds = state.rounds[layer]
        counts = [r["open"] for r in state.reviews[layer]]
        opened = state.open_findings(layer)
        by_sev = {s: sum(1 for f in opened if f["severity"] == s) for s in SEVERITIES}
        out.append(
            f"{layer}: {len(rounds)}/{budgets['layer']['rounds']} rounds, ${state.spend_layer[layer]:.2f}/${budgets['layer']['usd']}, "
            f"open {len(opened)} {by_sev}, open after each review {counts}, stop: {state.stops.get(layer, '-')}"
        )
    if state.open_decisions():
        out.append("Open decisions:")
        for d in state.open_decisions():
            out.append(f"  {d['id']} [{d['class']}] {d['question']}")
    if state.escapes:
        by = {}
        for e in state.escapes:
            by[(e["severity"], e["cause"])] = by.get((e["severity"], e["cause"]), 0) + 1
        out.append("Escapes from the blind review: " + ", ".join(f"{s}/{c}: {n}" for (s, c), n in sorted(by.items())))
    for a in state.amendments:
        out.append(f"Amendment of {a['fact']}: reopens {a.get('reopens')} layers {a.get('layers')} needs person: {a.get('needs_person')}")
    return "\n".join(out)


# ---------------------------------------------------------------------------------------------


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("protocol")
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("init")
    a = sub.add_parser("append")
    a.add_argument("event")
    sub.add_parser("status")
    sub.add_parser("resume")
    a = sub.add_parser("round")
    a.add_argument("--layer", required=True, choices=schema.LAYERS)
    a.add_argument("--head", required=True)
    a.add_argument("--checks", required=True, choices=("green", "red"))
    a.add_argument("--checks-file")
    a = sub.add_parser("answers")
    a.add_argument("--layer", required=True, choices=schema.LAYERS)
    a.add_argument("--round", type=int, required=True)
    a.add_argument("--file", required=True)
    a = sub.add_parser("ingest-review")
    a.add_argument("--layer", required=True, choices=(*schema.LAYERS, "auto"))
    a.add_argument("--round", type=int, required=True)
    a.add_argument("--sidecar", required=True)
    a = sub.add_parser("open-findings")
    a.add_argument("--layer", required=True, choices=schema.LAYERS)
    a = sub.add_parser("answers-file")
    a.add_argument("--layer", required=True, choices=schema.LAYERS)
    a.add_argument("--out", required=True)
    a = sub.add_parser("next")
    a.add_argument("--layer", required=True, choices=schema.LAYERS)
    a.add_argument("--extra-rounds", type=int, default=0)
    a = sub.add_parser("remaining")
    a.add_argument("--layer", choices=schema.LAYERS)
    a = sub.add_parser("check-guards")
    a.add_argument("--worktree", required=True)
    a.add_argument("--from", dest="rev_from", required=True)
    a.add_argument("--to", dest="rev_to", default="HEAD")
    a.add_argument("--layer", choices=schema.LAYERS)
    a.add_argument("--round", type=int)
    a.add_argument("--record", action="store_true")
    a = sub.add_parser("ask")
    a.add_argument("--class", dest="klass", required=True, choices=schema.DECISION_CLASSES)
    a.add_argument("--question", required=True)
    a.add_argument("--record", help="the decisions.json id this question decides")
    a.add_argument("--layer", choices=schema.LAYERS)
    a.add_argument("--option", action="append", default=[])
    a = sub.add_parser("decide")
    a.add_argument("--id", required=True)
    a.add_argument("--choice", required=True)
    a.add_argument("--by", default="person")
    a.add_argument("--grant-rounds", type=int, default=0)
    a.add_argument("--grant-usd", type=float, default=0.0)
    a.add_argument("--close", action="store_true", help="the answer ends the layer's loop (a cap answered without grants closes it already)")
    a.add_argument("--accept-guard", action="store_true", help="the answer to a guard escalation accepts the guarded change")
    a = sub.add_parser("amend")
    a.add_argument("--fact", required=True)
    a.add_argument("--evidence", required=True)
    a.add_argument("--new", required=True)
    a.add_argument("--source", default="chain", choices=("chain", "person"))
    args = p.parse_args()

    run = schema.run_dir(args.protocol)
    if args.cmd == "init":
        run.mkdir(parents=True, exist_ok=True)
        (run / "reviews").mkdir(exist_ok=True)
        (run / "rounds").mkdir(exist_ok=True)
        ledger_path(run).touch()
        print(run)
        return
    if not ledger_path(run).exists():
        raise SystemExit(f"no run for {args.protocol!r} at {run}; start one with `init`")

    state = State(read_events(run))
    if args.cmd == "append":
        print(json.dumps(append(run, json.loads(args.event))))
    elif args.cmd == "status":
        print(status_text(run, state))
    elif args.cmd == "resume":
        print(json.dumps(resume_point(run, state), indent=2))
    elif args.cmd == "round":
        n = len(state.rounds[args.layer]) + 1
        event = {"t": "round", "layer": args.layer, "n": n, "head": args.head, "checks": args.checks}
        if args.checks_file:
            event["checks_file"] = args.checks_file
        print(json.dumps(append(run, event)))
    elif args.cmd == "answers":
        result = record_answers(run, args.layer, args.round, Path(args.file))
        print(json.dumps(result, indent=2))
        if result["missing"] or result["errors"]:
            sys.exit(1)
    elif args.cmd == "ingest-review":
        print(json.dumps(ingest_review(run, args.layer, args.round, Path(args.sidecar)), indent=2))
    elif args.cmd == "open-findings":
        print(json.dumps(state.open_findings(args.layer), indent=2))
    elif args.cmd == "answers-file":
        Path(args.out).write_text(answers_file(state, args.layer))
        print(args.out)
    elif args.cmd == "next":
        result = next_step(state, args.layer, args.extra_rounds)
        if result["action"] in ("clean", "escalate") and state.stops.get(args.layer) != result["reason"]:
            append(run, {"t": "stop", "layer": args.layer, "reason": result["reason"]})
        if result["action"] == "escalate" and result["reason"] != "decision-needed":
            result["decision"] = escalate(run, State(read_events(run)), args.layer, result)
        print(json.dumps(result))
    elif args.cmd == "remaining":
        print(remaining_usd(state, args.layer))
    elif args.cmd == "check-guards":
        violations = check_guards(Path(args.worktree), args.rev_from, args.rev_to)
        if violations and args.record:
            if not args.layer or args.round is None:
                raise SystemExit("--record needs --layer and --round")
            append(run, {"t": "guard_violation", "layer": args.layer, "round": args.round, "paths": violations})
        print(json.dumps(violations, indent=2))
        if violations:
            sys.exit(1)
    elif args.cmd == "ask":
        event = {"t": "decision_needed", "id": state.next_decision_id(), "class": args.klass, "question": args.question, "options": args.option}
        if args.record:
            event["record"] = args.record
        if args.layer:
            event["layer"] = args.layer
        print(json.dumps(append(run, event)))
    elif args.cmd == "decide":
        if args.id not in state.decisions:
            raise SystemExit(f"no decision {args.id}")
        event = {"t": "decision", "id": args.id, "choice": args.choice, "by": args.by}
        if args.grant_rounds:
            event["grant_rounds"] = args.grant_rounds
        if args.grant_usd:
            event["grant_usd"] = args.grant_usd
        if args.close:
            event["close"] = True
        if args.accept_guard:
            event["accept_guard"] = True
        print(json.dumps(append(run, event)))
    elif args.cmd == "amend":
        print(json.dumps(amend(run, args.fact, args.evidence, args.new, args.source), indent=2))


if __name__ == "__main__":
    main()
