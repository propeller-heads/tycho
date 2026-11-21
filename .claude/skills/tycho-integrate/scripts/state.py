#!/usr/bin/env python3
"""The run's ledger: append, replay, findings, budgets, stop rules, guards and amendments.

The ledger (`.claude/worktrees/integrations/<protocol>/ledger.jsonl`) is append-only and is the single
source of the run's state: resuming a run, `status` and the stop rules all replay it. The stop
rules are code here, not a judgement the model makes.

    state.py <protocol> init                      create the run directory and ledger
    state.py <protocol> append '<event json>'     validate and append one event
    state.py <protocol> status                    stage, rounds, spend, open items
    state.py <protocol> resume                    the stage to resume at, as JSON
    state.py <protocol> round --layer L --head H --checks-file F
    state.py <protocol> answers --layer L --round N --file answers.json
    state.py <protocol> ingest-review --layer L|auto --round N --sidecar <report>.findings.json
    state.py <protocol> open-findings --layer L   open findings as JSON
    state.py <protocol> answers-file --layer L --out F   the `--existing` file for the next review
    state.py <protocol> next --layer L            continue | clean | closed | stopped | escalate, as JSON
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
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

SEVERITIES = schema.SEVERITIES


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
            # Every line must be an event `append` would have written. This catches a malformed
            # line, not a well-formed forged one: agents cannot write the ledger (guard.py), and
            # run_agent.py restores it if an agent session changed it anyway.
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


CAP_ESCALATIONS = ("round-cap", "usd-cap", "integration-usd-cap")
# The run's own stop, not a layer's: the integration's dollar cap answered without more dollars.
RUN = "run"


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
        self.accepted_guards = set()  # (path, what changed) of guarded changes a person accepted
        self.stops = {}  # layer or "run" -> reason
        # layer -> the decision id by which a person closed a layer that stopped without converging
        # (an escalation answered without more rounds or dollars, and no round since).
        self.closed = {}
        self.signed_facts = set()
        self.amendments = []
        self.escapes = []
        self.tampers = defaultdict(list)  # layer (or "run") -> sessions that changed the run's records
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
            # A round recorded again at the same number (a rerun at the same head) replaces it.
            rounds = self.rounds[e["layer"]]
            if rounds and rounds[-1]["n"] == e["n"]:
                rounds[-1] = e
            else:
                rounds.append(e)
            self.closed.pop(e["layer"], None)
            self.stops.pop(e["layer"], None)  # a new round supersedes the layer's last stop
        elif t == "review":
            # A second review of the same round (a re-ingest, or the final review of the last
            # round) replaces the first, so the stop rules count each round once.
            reviews = self.reviews[e["layer"]]
            same = [i for i, r in enumerate(reviews) if r["n"] == e["n"]]
            if same:
                reviews[same[-1]] = e
            else:
                reviews.append(e)
        elif t == "spend":
            usd = float(e["usd"])
            self.spend_total += usd
            self.spend_label[e["label"]] += usd
            if e.get("layer"):
                self.spend_layer[e["layer"]] += usd
        elif t == "finding":
            # A new or reopened finding supersedes a clean stop, and reopens a layer a person
            # closed: they closed it on the findings they saw, not on this one.
            self.stops.pop(e["layer"], None)
            self.closed.pop(e["layer"], None)
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
                self.accepted_guards.update(guard_key(v) for g in self.guard_violations[d["layer"]] for v in g["paths"])
            # The implementer applies the decision next round; the next review statuses the finding.
            if d.get("finding") in self.findings:
                self.findings[d["finding"]]["status"] = "open"
            if d.get("escalation") == "tamper":
                self.tampers.pop(d.get("layer") or RUN, None)  # the person has heard of it
            if e.get("waive") and d.get("finding") in self.findings:
                self.findings[d["finding"]]["status"] = "waived"
            layer = d.get("layer")
            if d.get("escalation") == "integration-usd-cap":
                self.grant_usd_total += float(e.get("grant_usd", 0))
                if not float(e.get("grant_usd", 0)):
                    self.closed[RUN] = e["id"]
            elif d.get("escalation") and layer:
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
        elif t == "tamper":
            self.tampers[e.get("layer", RUN)].append(e)

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
    """The stop rules. Returns {"action": "continue" | "clean" | "closed" | "stopped" | "escalate",
    "reason": ...}. `stopped`: a person stopped the whole run.

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
    if RUN in state.closed:
        return {"action": "stopped", "reason": "run-stopped", "decision": state.closed[RUN]}
    if layer in state.closed:
        return {"action": "closed", "reason": "closed", "decision": state.closed[layer], "open_findings": [f["id"] for f in open_now]}

    if state.open_decisions(layer):
        return {"action": "escalate", "reason": "decision-needed", "decisions": [d["id"] for d in state.open_decisions(layer)]}
    tampered = state.tampers[layer]
    if tampered:
        return {"action": "escalate", "reason": "tamper", "class": "outside-repo", "sessions": [t["label"] for t in tampered],
                "question": f"An agent session ({', '.join(t['label'] for t in tampered)}) changed the run's records "
                            f"({', '.join(p for t in tampered for p in t['paths'])}); they were restored and its output was not used. Rerun it, or stop?"}
    guard = [g for g in state.guard_violations[layer] if g["round"] > since]
    if guard:
        return {"action": "escalate", "reason": "guard", "class": "relax-test", "paths": guard[-1]["paths"]}
    # The environment failed, not the code: the person fixes it, then the round's checks run again
    # at the same head (which replaces the round). Nothing is handed to the implementer.
    if rounds and rounds[-1].get("infra"):
        infra = rounds[-1]["infra"]
        return {"action": "escalate", "reason": "infra", "class": "outside-repo", "steps": infra,
                "question": "The environment failed, not the code: " + "; ".join(f"{i['step']}: {i['cause']} ({i['detail']})" for i in infra)
                + ". Fix it (renew the token, free the disk, restore the RPC or Docker) and rerun the round's checks, or stop?"}

    last_green = bool(rounds) and checks_accepted(state, layer, rounds[-1])
    # A review counts for the round only if it reviewed that round's commit with its core passes run.
    reviewed_last = bool(reviews) and bool(rounds) and reviews[-1]["n"] == rounds[-1]["n"] and not reviews[-1].get("incomplete")
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

    if not last_green:
        why = "checks red"
    elif not reviewed_last:
        why = "the last round's review left out a core pass" if reviews and reviews[-1].get("incomplete") and reviews[-1]["n"] == rounds[-1]["n"] else "not reviewed"
    else:
        why = "open findings"
    return {"action": "continue", "reason": why}


def checks_accepted(state: State, layer: str, rnd: dict) -> bool:
    """The round's checks are green, or each one that failed is a finding the review refuted (the
    implementer's dispute held) or a person waived."""
    if rnd["checks"] == "green":
        return True
    failed = rnd.get("failed_checks")
    if not failed:
        return False
    ok = {check_key(f["rule"], f.get("quote", "")) for f in state.findings.values() if f["layer"] == layer and f["status"] in ("refuted", "waived")}
    return all(c in ok for c in failed)


ESCALATION_QUESTIONS = {
    "guard": "The implementer changed a guarded path or a test-relaxing yaml line. Accept the change, or revert it?",
    "integration-usd-cap": "The run reached its dollar cap. Grant more (`decide --grant-usd`), or stop the run (no grant)?",
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
        "question": result.get("question") or ESCALATION_QUESTIONS.get(reason, reason),
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


def layer_of(path: str, layers=schema.LAYERS) -> str:
    """The layer a file belongs to, among the route's `layers`, from `.claude/knowledge/paths.toml`.
    Files outside the three integration layers (shared types, registries, docs) count toward
    execution when they are execution config, otherwise toward the layer named by their directory.
    A file of no layer the route builds counts toward the route's last layer, so its finding still
    keeps a built layer open."""
    found = _layer_of(path)
    return found if found in layers else layers[-1]


def _layer_of(path: str):
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
    if path.startswith("protocols/substreams/"):
        return "indexing"
    return None


EARLIER_STATUS = {"fixed": "fixed", "disproven": "refuted", "open": "open", "deferred": "open"}

# A review that did not run one of these did not review the round: the chunk passes, the
# simplification pass, or the verification of findings. tycho-review also lists, in `not_run`,
# parts a review may leave out by design (the integration pass and the execution verifier at
# `partial`, tools the base lacks, files unchanged since a `--since` report); those do not make it
# incomplete.
CORE_NOT_RUN = re.compile(r"\b(chunk|pass(es)?\b|simplif|verif|every|all)", re.I)
OPTIONAL_NOT_RUN = re.compile(r"integration pass|execution verifier|not at base|not set up|tools? missing|unchanged since|ast-grep|dylint|review-only lint|harness|ci\b", re.I)


def core_not_run(not_run: list) -> list:
    """The `not_run` entries that leave a review short of reviewing the round."""
    return [x for x in not_run if CORE_NOT_RUN.search(str(x)) and not OPTIONAL_NOT_RUN.search(str(x))]


def ingest_review(run: Path, layer: str, n, sidecar_path: Path) -> dict:
    """Maps a tycho-review sidecar onto the ledger: new findings get ids, findings the review
    matched to ledger ids get verdicts, a closed finding that comes back is reopened.

    The review is of a layer's last round (`n`, which `auto` takes per layer from the ledger): its
    sidecar must name that round's commit, and one whose core passes did not all run is recorded as
    incomplete, which does not count as the round's review. Ingesting the same sidecar again changes
    nothing; a later review of the same round replaces the earlier one."""
    sidecar = schema.load_json(sidecar_path)
    state = State(read_events(run))
    layers = route_layers(run)
    targets = [l for l in layers if state.rounds[l]] if layer == "auto" else [layer]
    if not targets or not all(state.rounds[l] for l in targets):
        raise SystemExit(f"no round of {layer} to review")
    rounds = {l: state.rounds[l][-1] for l in targets}
    if layer != "auto" and n is not None and n != rounds[layer]["n"]:
        raise SystemExit(f"round {n} is not {layer}'s last round ({rounds[layer]['n']}): a review is of the last round")
    # The commit reviewed: the layer's last round, or for `auto` the branch's latest round.
    expect = rounds[layer]["head"] if layer != "auto" else next(e for e in reversed(state.events) if e["t"] == "round" and e["layer"] in targets)["head"]
    head = sidecar.get("head") or ""
    if not head or not (expect.startswith(head) or head.startswith(expect)):
        raise SystemExit(f"the sidecar reviewed {head or 'no recorded commit'}, not the round's {expect}")
    report = sidecar.get("report", str(sidecar_path))
    if all(any(r["n"] == rounds[l]["n"] and r.get("report") == report for r in state.reviews[l]) for l in targets):
        return {"added": [], "reopened": [], "pre_existing_skipped": 0, "unmatched_earlier": [], "duplicate": True}
    bad = [f"{f.get('file', '?')}: severity {f.get('severity')!r}" for f in sidecar.get("findings", [])
           if not f.get("pre_existing") and f.get("severity") not in SEVERITIES]
    if bad:
        raise SystemExit(f"findings without a severity in {SEVERITIES}: {bad}; fix the sidecar, a missing severity is never read as a lesser one")
    n_of = {l: rounds[l]["n"] for l in targets}
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
        append(run, {"t": "verdict", "id": fid, "status": status, "by": "review", "round": n_of.get(state.findings[fid]["layer"], 0), "evidence": e.get("evidence", "")})

    state = State(read_events(run))
    for f in sidecar.get("findings", []):
        if f.get("pre_existing"):
            skipped_pre += 1
            continue
        flayer = layer if layer != "auto" else layer_of(f.get("file", ""), targets)
        event = {
            "t": "finding",
            "layer": flayer,
            "rule": f.get("rule", "unknown"),
            "severity": f["severity"],
            "file": f.get("file", ""),
            "lines": f.get("lines"),
            "quote": f.get("quote", ""),
            "claim": f.get("claim", ""),
            "evidence": f.get("evidence", ""),
            "round": n_of[flayer],
        }
        known = state.findings.get(f.get("earlier_id")) or by_print.get(fingerprint(event))
        if known is None:
            event["id"] = state.next_finding_id()
        elif known["status"] in ("fixed", "refuted"):
            event["id"] = known["id"]
            event["reopened"] = True
        else:
            continue  # a finding the ledger already holds open needs no new event: its verdict came above
        state._apply(append(run, event))
        by_print[fingerprint(event)] = state.findings[event["id"]]
        (reopened if event.get("reopened") else added).append(event["id"])

    not_run = sidecar.get("not_run") or []
    missing = core_not_run(not_run)
    for lay in targets:
        review = {"t": "review", "layer": lay, "n": n_of[lay], "report": report, "head": head, "open": len(state.open_findings(lay))}
        if not_run:
            review["not_run"] = not_run
        if missing:
            review["incomplete"] = missing
        append(run, review)
    return {"added": added, "reopened": reopened, "pre_existing_skipped": skipped_pre, "unmatched_earlier": unmatched, "incomplete": missing}


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


def guard_key(v: dict) -> tuple:
    """A guarded change as a person accepts it: the file and what changed in it. A later, different
    change to the same file is not accepted with it."""
    return v["path"], v["why"]


def check_key(rule: str, quote: str) -> str:
    """The identity of a failed check across rounds: its rule (`check:<step>[:<check>]`) and where."""
    return f"{rule}|{quote}"


def failed_check_ids(checks: dict) -> list:
    """(rule, quote, claim) for each failure of a layer_checks.py result: a failed step, or each
    failing check of its protocol-testing report, with the test, component and direction as its
    identity."""
    out = []
    for step in checks.get("steps", []):
        if step.get("status") != "fail":
            continue
        if step.get("checks"):
            for c in step["checks"]:
                where = " ".join(str(c[k]) for k in ("test", "component", "direction") if c.get(k))
                out.append((f"check:{step['name']}:{c.get('check', '?')}", where,
                            f"{c.get('check')} {c.get('outcome')}" + (f" at {where}" if where else "") + f": {c.get('detail') or c.get('reason') or ''}"))
        else:
            out.append((f"check:{step['name']}", "", f"{step['name']} failed: {step.get('detail', '')}"))
    return out


def record_round(run: Path, state: State, layer: str, head: str, checks, checks_file) -> dict:
    """Records a round. The same head as the layer's last round, before that round was reviewed,
    records it again (a rerun after a crash or an infrastructure failure), so resuming never adds a
    round. After its review, the same head is a new round: an implementer round that only disputed
    findings changes no code, and still counts toward the caps. Each failed check
    is a finding of the layer (rule `check:<step>[:<check>]`), answered like a review's: fixed, or
    disputed with evidence the review verifies, or a decision for a person (`decide --waive`
    accepts the failure). A check that passes again closes its finding."""
    last = state.rounds[layer][-1] if state.rounds[layer] else None
    rerun = last and last["head"] == head and not any(r["n"] == last["n"] for r in state.reviews[layer])
    n = last["n"] if rerun else len(state.rounds[layer]) + 1
    data = schema.load_json(Path(checks_file)) if checks_file else {}
    if checks_file:
        checks = "green" if data.get("green") else "red"  # the file's verdict, never a second one
    if checks not in ("green", "red"):
        raise SystemExit("a round needs --checks-file (or --checks green|red without one)")
    event = {"t": "round", "layer": layer, "n": n, "head": head, "checks": checks}
    failed = []
    passed_steps = {s["name"] for s in data.get("steps", []) if s.get("status") == "pass"}
    if checks_file:
        event["checks_file"] = str(checks_file)
        failed = failed_check_ids(data) if checks == "red" else []
        event["failed_checks"] = [check_key(rule, quote) for rule, quote, _ in failed]
        infra = [{"step": s["name"], "cause": s.get("infra"), "detail": s.get("detail", ""), "tail": s.get("tail", [])[-8:]}
                 for s in data.get("steps", []) if s.get("status") == "infra"]
        if infra:
            event["infra"] = infra
    out = append(run, event)
    failing = set(event.get("failed_checks", []))
    by_id = {check_key(f["rule"], f.get("quote", "")): f for f in state.findings.values() if f["layer"] == layer and f["rule"].startswith("check:")}
    for rule, quote, claim in failed:
        known = by_id.get(check_key(rule, quote))
        if known and known["status"] in ("open", "decision", "refuted", "waived"):
            continue  # still failing: the open finding stands, and a refuted or waived one stays accepted
        finding = {"t": "finding", "id": known["id"] if known else state.next_finding_id(), "layer": layer, "rule": rule, "severity": "blocking",
                   "file": f"checks:{rule.split(':')[1]}", "quote": quote, "claim": claim, "round": n}
        if known:
            finding["reopened"] = True
        state._apply(append(run, finding))
        by_id[check_key(rule, quote)] = state.findings[finding["id"]]
    # A check finding is fixed only when its step ran and passed: a step that did not run this
    # round (`--only`, a chain left out, the environment failing) says nothing about it.
    for key, f in by_id.items():
        if key not in failing and f["status"] in ("open", "decision") and f["rule"].split(":")[1] in passed_steps:
            append(run, {"t": "verdict", "id": f["id"], "status": "fixed", "by": "checks", "round": n})
    return out


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
        return schema.git("-C", str(worktree), *args).stdout

    def show(rev, path):
        """A file at a revision; None where it does not exist there (added or deleted)."""
        proc = schema.git("-C", str(worktree), "show", f"{rev}:{path}", check=False)
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
    if RUN in state.closed:
        return {"stage": "stopped", "status": "stopped", "decision": state.closed[RUN]}
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
    a.add_argument("--checks", choices=("green", "red"), help="only without --checks-file, which carries the verdict")
    a.add_argument("--checks-file")
    a = sub.add_parser("answers")
    a.add_argument("--layer", required=True, choices=schema.LAYERS)
    a.add_argument("--round", type=int, required=True)
    a.add_argument("--file", required=True)
    a = sub.add_parser("ingest-review")
    a.add_argument("--layer", required=True, choices=(*schema.LAYERS, "auto"))
    a.add_argument("--round", type=int, help="the layer's last round (checked); `auto` takes each layer's from the ledger")
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
    a.add_argument("--waive", action="store_true", help="the answer to a failing check's decision accepts the failure")
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
        event = json.loads(args.event)
        if event.get("t") not in schema.APPENDABLE:
            raise SystemExit(f"`append` takes {schema.APPENDABLE} events; the others come from the command that owns their rule")
        print(json.dumps(append(run, event)))
    elif args.cmd == "status":
        print(status_text(run, state))
    elif args.cmd == "resume":
        print(json.dumps(resume_point(run, state), indent=2))
    elif args.cmd == "round":
        print(json.dumps(record_round(run, state, args.layer, args.head, args.checks, args.checks_file)))
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
        if args.waive:
            event["waive"] = True
        print(json.dumps(append(run, event)))
    elif args.cmd == "amend":
        print(json.dumps(amend(run, args.fact, args.evidence, args.new, args.source), indent=2))


if __name__ == "__main__":
    main()
