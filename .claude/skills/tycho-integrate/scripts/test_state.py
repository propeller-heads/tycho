#!/usr/bin/env python3
"""Tests of the run's ledger, stop rules, review ingestion, guards and spec validation.

    python3 -I .claude/skills/tycho-integrate/scripts/test_state.py
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402
import state as ledger  # noqa: E402

GUARD = Path(__file__).resolve().parent / "guard.py"
BUDGETS = ledger.load_budgets()


class RunCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.run = Path(self.tmp.name) / "run"
        self.run.mkdir()
        ledger.ledger_path(self.run).touch()

    def tearDown(self):
        self.tmp.cleanup()

    def add(self, **event):
        return ledger.append(self.run, event)

    def state(self):
        return ledger.State(ledger.read_events(self.run))

    def next(self, layer="indexing"):
        return ledger.next_step(self.state(), layer)

    def finding(self, fid, layer="indexing", rule="rule-a", file="src/a.rs", quote="let x = 1;", n=1):
        self.add(t="finding", id=fid, layer=layer, rule=rule, severity="blocking", file=file, claim=f"claim {fid}", quote=quote, round=n)

    def round(self, layer="indexing", checks="green"):
        n = len(self.state().rounds[layer]) + 1
        self.add(t="round", layer=layer, n=n, head=f"h{n}", checks=checks)
        return n

    def review(self, n, layer="indexing"):
        self.add(t="review", layer=layer, n=n, report="r.md", open=len(self.state().open_findings(layer)))

    def sidecar(self, data):
        path = Path(self.tmp.name) / "s.findings.json"
        path.write_text(json.dumps(data))
        return path


class StopRules(RunCase):
    def test_clean_needs_green_checks_a_review_of_the_last_round_and_no_open_findings(self):
        n = self.round()
        self.assertEqual(self.next()["action"], "continue")
        self.review(n)
        self.assertEqual(self.next(), {"action": "clean", "reason": "clean"})

    def test_red_checks_keep_the_layer_going(self):
        n = self.round(checks="red")
        self.review(n)
        self.assertEqual(self.next()["action"], "continue")

    def test_open_finding_keeps_the_layer_going(self):
        n = self.round()
        self.finding("F1", n=n)
        self.review(n)
        self.assertEqual(self.next()["action"], "continue")

    def test_round_cap_escalates(self):
        for _ in range(BUDGETS["layer"]["rounds"]):
            n = self.round()
            self.finding(f"F{n}", n=n, quote=f"q{n}")
            self.review(n)
        # Open counts rise every round, so no-progress would also fire; the cap is checked first.
        self.assertEqual(self.next()["reason"], "round-cap")

    def test_layer_dollar_cap_escalates(self):
        self.round(checks="red")
        self.add(t="spend", label="implement:indexing:1", usd=BUDGETS["layer"]["usd"], layer="indexing")
        self.assertEqual(self.next()["reason"], "usd-cap")
        self.assertEqual(ledger.remaining_usd(self.state(), "indexing"), 0)

    def test_integration_dollar_cap_escalates_before_the_layer_cap(self):
        self.round(checks="red")
        self.add(t="spend", label="research", usd=BUDGETS["integration"]["usd"])
        self.assertEqual(self.next()["reason"], "integration-usd-cap")

    def test_no_progress_after_two_rounds_without_fewer_open_findings(self):
        self.finding("F1")
        self.finding("F2", quote="other")
        for _ in range(3):
            n = self.round()
            self.review(n)
        self.assertEqual(self.next()["reason"], "no-progress")

    def test_progress_resets_the_no_progress_rule(self):
        self.finding("F1")
        self.finding("F2", quote="other")
        n = self.round()
        self.review(n)
        n = self.round()
        self.review(n)
        self.add(t="verdict", id="F1", status="fixed", by="review", round=n)
        n = self.round()
        self.review(n)
        self.assertEqual(self.next()["action"], "continue")

    def test_reopened_finding_is_oscillation(self):
        n = self.round()
        ledger.ingest_review(self.run, "indexing", n, self.sidecar({"findings": [{"rule": "rule-a", "file": "src/a.rs", "quote": "let x = 1;", "claim": "c", "severity": "blocking"}]}))
        n = self.round()
        ledger.ingest_review(self.run, "indexing", n, self.sidecar({"earlier": [{"id": "F1", "status": "fixed"}], "findings": []}))
        self.assertEqual(self.next()["action"], "clean")
        n = self.round()
        result = ledger.ingest_review(self.run, "indexing", n, self.sidecar({"findings": [{"rule": "rule-a", "file": "src/a.rs", "quote": "let  x = 1;", "claim": "back", "severity": "blocking"}]}))
        self.assertEqual(result["reopened"], ["F1"])
        self.assertEqual(self.next()["reason"], "oscillation")

    def test_two_held_disputes_are_a_deadlock(self):
        self.finding("F1")
        for _ in range(2):
            n = self.round()
            self.add(t="answer", id="F1", round=n, kind="dispute", evidence="cast call ... returns 0")
            self.add(t="verdict", id="F1", status="open", by="review", round=n)
            self.review(n)
        self.assertEqual(self.next()["reason"], "dispute-deadlock")

    def test_refuted_dispute_closes_the_finding(self):
        self.finding("F1")
        n = self.round()
        self.add(t="answer", id="F1", round=n, kind="dispute", evidence="test output")
        ledger.ingest_review(self.run, "indexing", n, self.sidecar({"earlier": [{"id": "F1", "status": "disproven"}]}))
        self.assertEqual(self.state().findings["F1"]["status"], "refuted")
        self.assertEqual(self.next()["action"], "clean")

    def test_guard_violation_escalates_until_a_person_answers(self):
        n = self.round()
        self.review(n)
        self.add(t="guard_violation", layer="indexing", round=n, paths=[{"path": ".claude/knowledge/indexing.md"}])
        result = self.next()
        self.assertEqual(result["reason"], "guard")
        did = ledger.escalate(self.run, self.state(), "indexing", result)
        self.assertEqual(self.next()["reason"], "decision-needed")
        self.add(t="decision", id=did, choice="revert it", by="person")
        self.assertEqual(self.next()["action"], "clean")

    def test_escalation_opens_one_decision_per_reason(self):
        for _ in range(BUDGETS["layer"]["rounds"]):
            n = self.round(checks="red")
            self.review(n)
        result = self.next()
        first = ledger.escalate(self.run, self.state(), "indexing", result)
        self.assertEqual(ledger.escalate(self.run, self.state(), "indexing", {**result}), first)

    def test_granted_rounds_lift_the_round_cap(self):
        for _ in range(BUDGETS["layer"]["rounds"]):
            n = self.round(checks="red")
            self.review(n)
        did = ledger.escalate(self.run, self.state(), "indexing", self.next())
        self.add(t="decision", id=did, choice="two more rounds", by="person", grant_rounds=2)
        self.assertEqual(self.next()["action"], "continue")

    def test_decision_answer_blocks_the_layer_until_decided(self):
        self.finding("F1")
        n = self.round()
        answers = Path(self.tmp.name) / "answers.json"
        answers.write_text(json.dumps({"answers": [{"id": "F1", "kind": "decision", "class": "no-precedent", "question": "Which encoding?"}]}))
        result = ledger.record_answers(self.run, "indexing", n, answers)
        self.assertEqual(result["missing"], [])
        self.review(n)
        self.assertEqual(self.next()["reason"], "decision-needed")
        self.add(t="decision", id="Q1", choice="pack address and index", by="person")
        self.assertEqual(self.next()["action"], "continue")


class Ingestion(RunCase):
    def test_new_findings_get_ids_and_matching_ones_are_not_duplicated(self):
        n = self.round()
        f = {"rule": "rule-a", "file": "src/a.rs", "quote": "x", "claim": "c", "severity": "nit"}
        self.assertEqual(ledger.ingest_review(self.run, "indexing", n, self.sidecar({"findings": [f]}))["added"], ["F1"])
        n = self.round()
        self.assertEqual(ledger.ingest_review(self.run, "indexing", n, self.sidecar({"findings": [f]}))["added"], [])
        self.assertEqual(len(self.state().findings), 1)

    def test_pre_existing_findings_stay_out_of_the_ledger(self):
        n = self.round()
        result = ledger.ingest_review(self.run, "indexing", n, self.sidecar({"findings": [{"rule": "r", "file": "f", "claim": "c", "pre_existing": True}]}))
        self.assertEqual(result["pre_existing_skipped"], 1)
        self.assertEqual(self.state().findings, {})

    def test_every_open_finding_needs_an_answer(self):
        self.finding("F1")
        self.finding("F2", quote="y")
        answers = Path(self.tmp.name) / "answers.json"
        answers.write_text(json.dumps([{"id": "F1", "kind": "fix"}]))
        result = ledger.record_answers(self.run, "indexing", 1, answers)
        self.assertEqual(result["missing"], ["F2"])

    def test_dispute_without_evidence_is_rejected(self):
        self.finding("F1")
        answers = Path(self.tmp.name) / "answers.json"
        answers.write_text(json.dumps([{"id": "F1", "kind": "dispute"}]))
        result = ledger.record_answers(self.run, "indexing", 1, answers)
        self.assertTrue(result["errors"])
        self.assertEqual(self.state().findings["F1"]["answers"], [])

    def test_skip_is_not_an_answer(self):
        self.finding("F1")
        answers = Path(self.tmp.name) / "answers.json"
        answers.write_text(json.dumps([{"id": "F1", "kind": "skip"}]))
        self.assertTrue(ledger.record_answers(self.run, "indexing", 1, answers)["errors"])

    def test_answers_file_keeps_ledger_ids_and_the_latest_answer(self):
        self.finding("F1")
        self.add(t="answer", id="F1", round=1, kind="dispute", evidence="forge test -vvv shows the revert")
        text = ledger.answers_file(self.state(), "indexing")
        self.assertIn("## F1 · blocking · rule-a", text)
        self.assertIn("forge test -vvv shows the revert", text)


class Replay(RunCase):
    def test_resume_starts_at_the_first_stage_not_done(self):
        for stage in ("envcheck", "intake", "worktree"):
            self.add(t="stage", stage=stage, status="done")
        self.assertEqual(ledger.resume_point(self.run, self.state())["stage"], "research")

    def test_resume_skips_layers_the_route_does_not_build(self):
        (self.run / "intake.json").write_text(json.dumps({"route": {"kind": "fork", "layers": ["indexing"], "sibling": "uniswap_v2"}}))
        for stage in schema.STAGES:
            if stage in ("build:indexing", "final", "gate3", "pr"):
                break
            self.add(t="stage", stage=stage, status="done")
        self.add(t="stage", stage="build:indexing", status="done")
        self.assertEqual(ledger.resume_point(self.run, self.state())["stage"], "final")

    def test_a_malformed_event_is_refused(self):
        with self.assertRaises(SystemExit):
            ledger.append(self.run, {"t": "round", "layer": "indexing"})


class Guards(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tree = Path(self.tmp.name) / "tree"
        self.tree.mkdir()
        self.git("init", "-q")
        self.git("config", "user.email", "t@example.com")
        self.git("config", "user.name", "t")
        (self.tree / "pkg").mkdir()
        (self.tree / "pkg/integration_test.tycho.yaml").write_text("harness: strict\ntests: []\n")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD").strip()

    def tearDown(self):
        self.tmp.cleanup()

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.tree), *args], capture_output=True, text=True, check=True).stdout

    def commit(self, rel, text, msg="change"):
        path = self.tree / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        self.git("add", rel)
        self.git("commit", "-qm", msg)

    def test_guarded_path_and_relaxed_yaml_line_are_violations(self):
        self.commit(".claude/knowledge/indexing.md", "weaker rule")
        self.commit("pkg/integration_test.tycho.yaml", "harness: strict\nskip_balance_check: true\ntests: []\n")
        self.commit("pkg/src/lib.rs", "fn main() {}")
        found = ledger.check_guards(self.tree, self.base, "HEAD")
        self.assertEqual(sorted(v["path"] for v in found), [".claude/knowledge/indexing.md", "pkg/integration_test.tycho.yaml"])

    EXECUTION_RS = "protocols/testing/src/execution.rs"
    MAPPING = 'const FOO_BYTECODE_JSON: &str = include_str!("../fixtures/Foo.runtime.json");\n    map.insert("foo", FOO_BYTECODE_JSON);\n'

    def test_executor_fixture_lines_are_the_one_allowed_harness_change(self):
        self.commit(self.EXECUTION_RS, "fn mapping() {\n}\n")
        base = self.git("rev-parse", "HEAD").strip()
        self.commit(self.EXECUTION_RS, "fn mapping() {\n" + self.MAPPING + "}\n")
        self.assertEqual(ledger.check_guards(self.tree, base, "HEAD"), [])
        self.commit(self.EXECUTION_RS, "fn mapping() {\n" + self.MAPPING + "    let strict = false;\n}\n")
        found = ledger.check_guards(self.tree, base, "HEAD")
        self.assertEqual(len(found), 1)
        self.assertIn("let strict = false", found[0]["why"])

    def test_changing_a_limit_kind_or_declaring_under_cap_is_a_violation(self):
        yaml = "pkg/integration_test.tycho.yaml"
        self.commit(yaml, "harness: strict\nlimits: {kind: hard}\ntests: []\n")
        self.assertEqual(ledger.check_guards(self.tree, self.base, "HEAD"), [])
        base = self.git("rev-parse", "HEAD").strip()
        self.commit(yaml, "harness: strict\nlimits: {kind: soft, reason: x}\ntests: []\n")
        self.assertEqual([v["why"] for v in ledger.check_guards(self.tree, base, "HEAD")], ["changes `limits: {kind: hard}`"])
        self.assertTrue(schema.guarded_yaml_changes([], ["        kind: under_cap"]))
        self.assertEqual(schema.guarded_yaml_changes(["limits: {kind: hard}"], ["limits: {kind: hard}"]), [])

    def hook(self, role, tool, tool_input):
        env = {**os.environ, "TYCHO_INTEGRATE_GUARD": role, "TYCHO_INTEGRATE_WORKTREE": str(self.tree), "TYCHO_INTEGRATE_RUN": str(Path(self.tmp.name) / "run"), "TYCHO_INTEGRATE_REPO": str(self.tree)}
        out = subprocess.run([sys.executable, "-I", str(GUARD)], input=json.dumps({"tool_name": tool, "tool_input": tool_input}), capture_output=True, text=True, env=env).stdout
        return json.loads(out)["hookSpecificOutput"]["permissionDecision"] if out.strip() else "allow"

    def test_hook_denies_knowledge_edits_and_test_relaxing_lines(self):
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": str(self.tree / ".claude/knowledge/x.md"), "old_string": "a", "new_string": "b"}), "deny")
        yaml = str(self.tree / "pkg/integration_test.tycho.yaml")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": yaml, "old_string": "tests: []", "new_string": "skip_balance_check: true\ntests: []"}), "deny")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": yaml, "old_string": "tests: []", "new_string": "tests:\n  - name: a"}), "allow")

    def test_hook_allows_only_fixture_lines_in_the_executor_table(self):
        path = str(self.tree / self.EXECUTION_RS)
        anchor = "static EXECUTOR_MAPPING"
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": path, "old_string": anchor, "new_string": anchor + "\n" + self.MAPPING}), "allow")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": path, "old_string": "TOLERANCE = 0", "new_string": "TOLERANCE = 50"}), "deny")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": str(self.tree / "protocols/testing/src/config.rs"), "old_string": "a", "new_string": "b"}), "deny")

    def test_hook_keeps_each_role_in_its_place(self):
        self.assertEqual(self.hook("implementer", "Write", {"file_path": "/etc/passwd", "content": ""}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "git push origin HEAD"}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "sed -i s/a/b/ pkg/integration_test.tycho.yaml"}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "cargo test -p pkg"}), "allow")
        self.assertEqual(self.hook("researcher", "Write", {"file_path": str(self.tree / "src/x.rs"), "content": ""}), "deny")
        self.assertEqual(self.hook("researcher", "Write", {"file_path": str(Path(self.tmp.name) / "run/spec.json"), "content": "{}"}), "allow")


class SpecValidation(unittest.TestCase):
    def spec(self, **fact):
        return {"protocol": "Foo", "kind": "native", "facts": [{"id": "S1", "section": "swap", "claim": "fee is 30 bps", **fact}]}

    def test_critical_fact_without_evidence_stops_the_run(self):
        errors = schema.validate_spec(self.spec(critical=True, status="unknown"))
        self.assertTrue(any("critical" in e for e in errors))

    def test_verified_fact_needs_evidence(self):
        self.assertTrue(schema.validate_spec(self.spec()))
        self.assertEqual(schema.validate_spec(self.spec(evidence=[{"kind": "command", "ref": "cast call 0x.. 'fee()' -b 21000000"}])), [])

    def test_unknown_non_critical_fact_is_allowed(self):
        self.assertEqual(schema.validate_spec(self.spec(status="unknown")), [])


class ClosedLayers(RunCase):
    def stopped_at_round_cap(self):
        for _ in range(BUDGETS["layer"]["rounds"]):
            self.round(checks="red")
        result = self.next()
        self.assertEqual(result["reason"], "round-cap")
        ledger.append(self.run, {"t": "stop", "layer": "indexing", "reason": "round-cap"})
        return ledger.escalate(self.run, self.state(), "indexing", result)

    def test_a_layer_a_person_closed_is_closed_until_it_runs_again(self):
        did = self.stopped_at_round_cap()
        self.assertNotIn("indexing", self.state().closed)
        self.add(t="decision", id=did, choice="stop, accept as is", by="person")
        self.assertEqual(self.state().closed["indexing"], did)
        self.round()
        self.assertNotIn("indexing", self.state().closed)

    def test_next_does_not_re_escalate_a_closed_layer(self):
        did = self.stopped_at_round_cap()
        self.add(t="decision", id=did, choice="stop, accept as is", by="person")
        result = self.next()
        self.assertEqual((result["action"], result["decision"]), ("closed", did))
        self.assertEqual(len(self.state().open_decisions()), 0)

    def test_an_answer_that_grants_rounds_does_not_close_the_layer(self):
        did = self.stopped_at_round_cap()
        self.add(t="decision", id=did, choice="two more rounds", by="person", grant_rounds=2)
        self.assertNotIn("indexing", self.state().closed)

    def test_earlier_findings_with_unknown_ids_match_by_file_and_claim(self):
        self.finding("F1")
        sidecar = Path(self.tmp.name) / "s.json"
        sidecar.write_text(json.dumps({"earlier": [{"id": "E7", "status": "fixed", "file": "src/a.rs", "claim": "claim F1"}], "findings": []}))
        self.add(t="round", layer="indexing", n=1, head="h1", checks="green")
        result = ledger.ingest_review(self.run, "indexing", 1, sidecar)
        self.assertEqual(result["unmatched_earlier"], [])
        self.assertEqual(self.state().findings["F1"]["status"], "fixed")


if __name__ == "__main__":
    unittest.main()
