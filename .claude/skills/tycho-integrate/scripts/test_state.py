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
import final_gates  # noqa: E402
import layer_checks  # noqa: E402
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
        """A review sidecar of the latest round's commit, at a path of its own (as each review has)."""
        self.sidecars = getattr(self, "sidecars", 0) + 1
        rounds = [e for e in ledger.read_events(self.run) if e["t"] == "round"]
        data = {"head": rounds[-1]["head"] if rounds else "", **data}
        for f in data.get("findings", []):
            f.setdefault("severity", "blocking")
        path = Path(self.tmp.name) / f"s{self.sidecars}.findings.json"
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

    def test_a_line_written_around_append_stops_the_replay(self):
        with open(ledger.ledger_path(self.run), "a") as f:
            f.write(json.dumps({"t": "gate", "gate": "1"}) + "\n")
        with self.assertRaises(SystemExit):
            ledger.read_events(self.run)

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
        # The executor table as protocols/testing has it, so the hook judges whole files.
        (self.tree / "protocols/testing/src").mkdir(parents=True)
        (self.tree / self.EXECUTION_RS).write_text(
            "static EXECUTOR_MAPPING: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {\n    let mut map = HashMap::new();\n    map\n});\n"
            "pub const TOLERANCE = 0;\n\n#[cfg(test)]\nmod tests {\n}\n"
        )
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

    REGISTRY_RS = "protocols/testing/src/state_registry.rs"
    REGISTRY = (
        "use tycho_simulation::{\n    evm::protocol::{sky::state::SkyState, vm::state::EVMPoolState},\n};\n\n"
        "pub fn register_protocol() {\n    match protocol_system {\n"
        '        "sky" => stream_builder.exchange_with_decoder_context::<SkyState>(\n'
        "            protocol_system,\n            tvl_filter,\n            None,\n            decoder_context,\n        ),\n"
        "        // Default to EVMPoolState\n        _ => default(),\n    }\n}\n"
    )
    ARM = (
        '        "foo" => stream_builder.exchange_with_decoder_context::<FooState>(\n'
        "            protocol_system,\n            tvl_filter,\n            None,\n            decoder_context,\n        ),\n"
    )

    def test_a_new_decoder_arm_and_its_import_are_allowed_in_the_registry(self):
        self.commit(self.REGISTRY_RS, self.REGISTRY)
        base = self.git("rev-parse", "HEAD").strip()
        added = self.REGISTRY.replace("sky::state::SkyState,", "foo::state::FooState, sky::state::SkyState,").replace("        // Default", self.ARM + "        // Default")
        self.commit(self.REGISTRY_RS, added)
        self.assertEqual(ledger.check_guards(self.tree, base, "HEAD"), [])
        # an arm that also claims an existing system shadows its decoder
        self.commit(self.REGISTRY_RS, added.replace('"foo" =>', '"foo" | "sky" =>'))
        self.assertEqual(len(ledger.check_guards(self.tree, base, "HEAD")), 1)

    def test_only_a_new_package_may_be_mapped_to_a_base_package(self):
        runner = "protocols/testing/src/test_runner.rs"
        text = 'static CLONE_TO_BASE_PROTOCOL: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {\n    HashMap::from([\n        ("base-a", "ethereum-a"),\n    ])\n});\n'
        self.commit(runner, text)
        base = self.git("rev-parse", "HEAD").strip()
        self.commit(runner, text.replace('("base-a", "ethereum-a"),', '("base-a", "ethereum-a"),\n        ("arc-a", "ethereum-a"),'))
        self.assertEqual(ledger.check_guards(self.tree, base, "HEAD"), [])
        self.commit(runner, text.replace('"ethereum-a"', '"ethereum-b"'))
        self.assertEqual(len(ledger.check_guards(self.tree, base, "HEAD")), 1)

    def test_changing_a_limit_kind_or_declaring_under_cap_is_a_violation(self):
        yaml = "pkg/integration_test.tycho.yaml"
        self.commit(yaml, "harness: strict\nlimits: {kind: hard}\ntests: []\n")
        self.assertEqual(ledger.check_guards(self.tree, self.base, "HEAD"), [])
        base = self.git("rev-parse", "HEAD").strip()
        self.commit(yaml, "harness: strict\nlimits: {kind: soft, reason: x}\ntests: []\n")
        self.assertEqual([v["why"] for v in ledger.check_guards(self.tree, base, "HEAD")], ["changes `limits: {kind: hard}`"])
        self.assertTrue(schema.guarded_yaml_changes([], ["        kind: under_cap"]))
        self.assertEqual(schema.guarded_yaml_changes(["limits: {kind: hard}"], ["limits: {kind: hard}"]), [])

    def hook(self, role, tool, tool_input, env_drop=()):
        # The test tree lives in the temp dir, which a role may write: name another as the temp root.
        env = {**os.environ, "TYCHO_INTEGRATE_GUARD": role, "TYCHO_INTEGRATE_WORKTREE": str(self.tree), "TYCHO_INTEGRATE_RUN": str(Path(self.tmp.name) / "run"),
               "TYCHO_INTEGRATE_REPO": str(self.tree), "TYCHO_INTEGRATE_TMP_ROOTS": str(Path(self.tmp.name) / "scratch")}
        for k in env_drop:
            env.pop(k)
        proc = subprocess.run([sys.executable, "-I", str(GUARD)], input=json.dumps({"tool_name": tool, "tool_input": tool_input, "cwd": str(self.tree)}), capture_output=True, text=True, env=env)
        # A crashing guard lets the call through in Claude Code, so a crash is never "allow" here.
        self.assertEqual((proc.returncode, proc.stderr), (0, ""), "the guard crashed")
        return json.loads(proc.stdout)["hookSpecificOutput"]["permissionDecision"] if proc.stdout.strip() else "allow"

    def test_hook_denies_knowledge_edits_and_test_relaxing_lines(self):
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": str(self.tree / ".claude/knowledge/x.md"), "old_string": "a", "new_string": "b"}), "deny")
        yaml = str(self.tree / "pkg/integration_test.tycho.yaml")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": yaml, "old_string": "tests: []", "new_string": "skip_balance_check: true\ntests: []"}), "deny")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": yaml, "old_string": "tests: []", "new_string": "tests:\n  - name: a"}), "allow")

    def test_hook_allows_only_fixture_lines_in_the_executor_table(self):
        path = str(self.tree / self.EXECUTION_RS)
        anchor = "    let mut map = HashMap::new();"
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": path, "old_string": anchor, "new_string": anchor + "\n    " + self.MAPPING}), "allow")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": path, "old_string": "mod tests {", "new_string": "mod tests {\n    #[test]\n    fn foo_is_mapped() {}"}), "allow")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": path, "old_string": "TOLERANCE = 0", "new_string": "TOLERANCE = 50"}), "deny")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": str(self.tree / "protocols/testing/src/config.rs"), "old_string": "a", "new_string": "b"}), "deny")

    def test_hook_keeps_each_role_in_its_place(self):
        self.assertEqual(self.hook("implementer", "Write", {"file_path": "/etc/passwd", "content": ""}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "git push origin HEAD"}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "sed -i s/a/b/ pkg/integration_test.tycho.yaml"}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "cargo test -p pkg"}), "allow")
        self.assertEqual(self.hook("researcher", "Write", {"file_path": str(self.tree / "src/x.rs"), "content": ""}), "deny")
        self.assertEqual(self.hook("researcher", "Write", {"file_path": str(Path(self.tmp.name) / "run/spec.json"), "content": "{}"}), "allow")


class GuardHoles(Guards):
    """The ways past the guard the review of #1852 found."""

    def run_dir(self):
        return Path(self.tmp.name) / "run"

    def test_a_guard_that_cannot_decide_denies(self):
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": str(self.tree / "src/a.rs")}, env_drop=("TYCHO_INTEGRATE_WORKTREE",)), "deny")
        binary = self.tree / ".claude/knowledge/x.md"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"\xff\xfe rule")
        self.assertEqual(self.hook("implementer", "Write", {"file_path": str(binary), "content": "x"}), "deny")

    def test_researcher_and_reviewer_bash_writes_stay_in_their_directories(self):
        target = str(self.tree / self.EXECUTION_RS)
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "echo x > /etc/foo"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": f"sed -i s/a/b/ {target}"}), "deny")
        self.assertEqual(self.hook("researcher", "Bash", {"command": f"python3 -c \"open('{target}','w').write('x')\""}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": f"python3 -c \"open('{self.tree}/.claude/reviews/r.md','w').write('x')\""}), "allow")
        self.assertEqual(self.hook("researcher", "Bash", {"command": f"echo '{{}}' > {self.run_dir()}/spec.json"}), "allow")

    def test_the_implementer_cannot_write_guarded_paths_through_bash(self):
        self.assertEqual(self.hook("implementer", "Bash", {"command": "python3 -c \"open('.claude/knowledge/x.md','w').write('x')\""}), "deny")
        (self.tree / "pkg/sub").mkdir()
        self.assertEqual(self.hook("implementer", "Bash", {"command": "cd pkg/sub && echo x >> ../../.claude/knowledge/general.md"}), "deny")
        self.assertEqual(self.hook("implementer", "Bash", {"command": "cargo test -p pkg 2>&1 | tail -5"}), "allow")

    def test_no_role_writes_the_ledger(self):
        ledger_file = self.run_dir() / "ledger.jsonl"
        for role in ("researcher", "reviewer"):
            self.assertEqual(self.hook(role, "Write", {"file_path": str(ledger_file), "content": ""}), "deny")
            self.assertEqual(self.hook(role, "Bash", {"command": f"echo '{{}}' >> {ledger_file}"}), "deny")

    def test_no_role_writes_to_github_or_pushes(self):
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "gh api -X POST repos/o/r/pulls/1/comments -f body=x"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "gh api repos/o/r/issues/1/comments -f body=x"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "gh api --paginate repos/o/r/pulls/1/comments"}), "allow")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "env git -C . push origin HEAD"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "git commit -am x"}), "deny")

    def test_uncommitted_and_deleted_guarded_files_are_violations(self):
        (self.tree / "protocols/testing/src/lib.rs").write_text("weaker harness")
        self.assertEqual([v["commit"] for v in ledger.check_guards(self.tree, self.base, "HEAD")], ["uncommitted"])
        (self.tree / "protocols/testing/src/lib.rs").unlink()
        self.git("rm", "-q", self.EXECUTION_RS)
        self.git("commit", "-qm", "drop the executor table")
        self.assertEqual([v["why"] for v in ledger.check_guards(self.tree, self.base, "HEAD")], ["deletes a guarded file"])

    def test_decision_values_cannot_carry_a_command(self):
        dec = {"agreements": {"protocol_system": "foo", "component_id": "x"}, "paths": {"package": "x; touch /tmp/pwned; echo"}}
        self.assertTrue(any("paths.package" in e for e in schema.validate_decisions(dec, {})))
        dec["paths"]["package"] = "ethereum-foo"
        self.assertEqual(schema.validate_decisions(dec, {}), [])


class SpecValidation(unittest.TestCase):
    def spec(self, **fact):
        return {"protocol": "Foo", "kind": "native", "kind_fact": "S1", "facts": [{"id": "S1", "section": "swap", "claim": "fee is 30 bps", **fact}]}

    def test_the_kind_is_one_of_three_and_rests_on_a_fact(self):
        ok = self.spec(status="unknown")
        self.assertTrue(schema.validate_spec({**ok, "kind": "rfq"}))
        self.assertTrue(schema.validate_spec({**ok, "kind_fact": "S9"}))
        self.assertTrue(schema.validate_spec({k: v for k, v in ok.items() if k != "kind_fact"}))

    def test_a_hybrid_names_what_it_reads_through_the_vm(self):
        hybrid = {**self.spec(status="unknown"), "kind": "hybrid"}
        self.assertTrue(any("vm_reads" in e for e in schema.validate_spec(hybrid)))
        self.assertEqual(schema.validate_spec({**hybrid, "vm_reads": ["pool storage of the vault"]}), [])

    def test_critical_fact_without_evidence_stops_the_run(self):
        errors = schema.validate_spec(self.spec(critical=True, status="unknown"))
        self.assertTrue(any("critical" in e for e in errors))

    def test_verified_fact_needs_evidence(self):
        self.assertTrue(schema.validate_spec(self.spec()))
        self.assertEqual(schema.validate_spec(self.spec(evidence=[{"kind": "command", "ref": "cast call 0x.. 'fee()' -b 21000000"}])), [])

    def test_unknown_non_critical_fact_is_allowed(self):
        self.assertEqual(schema.validate_spec(self.spec(status="unknown")), [])


class ResumeAndReviewIdentity(RunCase):
    """Re-running a step after a crash, and reviews of the wrong commit, change nothing."""

    def test_a_round_recorded_again_at_the_same_head_is_the_same_round(self):
        for _ in range(5):
            ledger.record_round(self.run, self.state(), "indexing", "abc", "red", None)
        self.assertEqual([r["n"] for r in self.state().rounds["indexing"]], [1])
        ledger.record_round(self.run, self.state(), "indexing", "abc", "green", None)
        self.assertEqual(self.state().rounds["indexing"][-1]["checks"], "green")

    def test_ingesting_a_sidecar_twice_counts_one_review(self):
        for _ in range(2):
            n = self.round()
            path = self.sidecar({"findings": [{"rule": "r", "file": "src/a.rs", "claim": "c"}, {"rule": "q", "file": "src/b.rs", "claim": "d"}]})
            ledger.ingest_review(self.run, "indexing", n, path)
        self.assertTrue(ledger.ingest_review(self.run, "indexing", n, path)["duplicate"])
        self.assertEqual([r["n"] for r in self.state().reviews["indexing"]], [1, 2])
        self.assertEqual(self.next()["action"], "continue")

    def test_a_review_of_another_commit_or_with_passes_not_run_does_not_count(self):
        n = self.round()
        with self.assertRaises(SystemExit):
            ledger.ingest_review(self.run, "indexing", n, self.sidecar({"head": "bbbb", "findings": []}))
        ledger.ingest_review(self.run, "indexing", n, self.sidecar({"not_run": ["every pass"], "findings": []}))
        self.assertEqual(self.next(), {"action": "continue", "reason": "the last round's review did not run every pass"})

    def test_a_finding_without_a_severity_is_refused(self):
        n = self.round()
        path = self.sidecar({"findings": [{"rule": "r", "file": "f", "claim": "c"}]})
        data = json.loads(path.read_text())
        del data["findings"][0]["severity"]
        path.write_text(json.dumps(data))
        with self.assertRaises(SystemExit):
            ledger.ingest_review(self.run, "indexing", n, path)

    def test_append_cannot_close_a_finding_with_an_unknown_status(self):
        self.assertTrue(schema.validate_event({"t": "verdict", "id": "F1", "status": "closed", "by": "x", "round": 1}))
        self.assertNotIn("verdict", schema.APPENDABLE)

    def test_auto_reviews_each_layer_at_its_own_last_round(self):
        (self.run / "intake.json").write_text(json.dumps({"route": {"kind": "new", "layers": ["indexing", "simulation"]}}))
        for _ in range(3):
            self.review(self.round("indexing"), "indexing")
        self.review(self.round("simulation"), "simulation")
        ledger.ingest_review(self.run, "auto", None, self.sidecar({"findings": [{"rule": "r", "file": "docs/x.md", "claim": "c"}]}))
        st = self.state()
        self.assertEqual([r["n"] for r in st.reviews["indexing"]], [1, 2, 3])
        self.assertEqual([r["n"] for r in st.reviews["simulation"]], [1])
        # a file of no built layer keeps the route's last layer open
        self.assertEqual([f["layer"] for f in st.findings.values()], ["simulation"])
        self.assertEqual(self.next("indexing")["action"], "clean")


class RunStop(RunCase):
    def test_the_integration_cap_answered_without_dollars_stops_the_run(self):
        self.round(checks="red")
        self.add(t="spend", label="x", usd=BUDGETS["integration"]["usd"], layer="indexing")
        result = self.next()
        self.assertEqual(result["reason"], "integration-usd-cap")
        did = ledger.escalate(self.run, self.state(), "indexing", result)
        self.add(t="decision", id=did, choice="stop the run", by="person")
        self.assertEqual(self.next()["action"], "stopped")
        self.assertEqual(self.next("simulation")["action"], "stopped")
        self.assertEqual(ledger.resume_point(self.run, self.state())["stage"], "stopped")


class FailedChecks(RunCase):
    def checks(self, failing):
        path = Path(self.tmp.name) / f"checks{len(self.state().rounds['indexing'])}.json"
        steps = [{"name": "protocol-testing", "status": "fail", "checks": [{"test": "t", "check": c, "outcome": "fail", "detail": "x"} for c in failing]}] if failing else []
        path.write_text(json.dumps({"steps": steps}))
        return path

    def test_a_failed_check_is_a_finding_a_dispute_can_settle(self):
        ledger.record_round(self.run, self.state(), "indexing", "h1", "red", self.checks(["balances"]))
        [f] = self.state().open_findings("indexing")
        self.assertEqual(f["rule"], "check:protocol-testing:balances")
        self.review(1)
        self.assertEqual(self.next()["reason"], "checks red")
        # the review upholds the implementer's dispute: the failure is accepted
        self.add(t="verdict", id=f["id"], status="refuted", by="review", round=1)
        self.assertEqual(self.next()["action"], "clean")

    def test_a_check_that_passes_again_closes_its_finding(self):
        ledger.record_round(self.run, self.state(), "indexing", "h1", "red", self.checks(["balances"]))
        ledger.record_round(self.run, self.state(), "indexing", "h2", "green", self.checks([]))
        self.assertEqual(self.state().open_findings("indexing"), [])


class Environment(RunCase):
    def test_an_infrastructure_failure_goes_to_a_person_not_the_implementer(self):
        checks = Path(self.tmp.name) / "checks.json"
        checks.write_text(json.dumps({"steps": [{"name": "protocol-testing", "status": "infra", "infra": "substreams token", "detail": "x (twice)", "tail": ["401"]}]}))
        ledger.record_round(self.run, self.state(), "indexing", "h1", "red", checks)
        self.assertEqual(self.state().open_findings("indexing"), [])
        result = self.next()
        self.assertEqual((result["reason"], result["class"]), ("infra", "outside-repo"))
        self.assertIn("substreams token", result["question"])
        # fixed and rerun at the same head: the same round, and the layer goes on
        checks.write_text(json.dumps({"steps": []}))
        ledger.record_round(self.run, self.state(), "indexing", "h1", "green", checks)
        self.assertEqual(len(self.state().rounds["indexing"]), 1)
        self.assertEqual(self.next()["reason"], "not reviewed")

    def test_infrastructure_causes_are_told_from_code_failures(self):
        self.assertEqual(layer_checks.infra_cause("HTTP 429 Too Many Requests"), "rpc rate limit")
        self.assertEqual(layer_checks.infra_cause("code = Unauthenticated desc = invalid token from substreams"), "substreams token")
        self.assertIsNone(layer_checks.infra_cause("thread 'main' panicked: assertion `left == right` failed"))


class Chains(RunCase):
    def test_the_primary_chain_comes_first_and_paths_resolve_per_chain(self):
        (self.run / "intake.json").write_text(json.dumps({"chains": ["ethereum", "base"], "primary_chain": "base"}))
        self.assertEqual(schema.run_chains(self.run), ("base", ["base", "ethereum"]))
        (self.run / "decisions.json").write_text(json.dumps({"agreements": {"protocol_system": "foo"},
                                                             "paths": {"package": "foo", "test_package": {"ethereum": "ethereum-foo", "base": "base-foo"}}}))
        self.assertEqual(layer_checks.placeholders(self.run, Path("/t"), "base")["test_package"], "base-foo")
        self.assertEqual(layer_checks.placeholders(self.run, Path("/t"), "ethereum")["test_package"], "ethereum-foo")
        self.assertTrue(schema.validate_intake({"protocol": "Foo", "chains": ["ethereum"], "primary_chain": "base", "anchor": {"docs": "x"}}))


class LayersCleanGate(RunCase):
    def test_a_finding_after_a_clean_stop_reopens_the_layer(self):
        (self.run / "intake.json").write_text(json.dumps({"route": {"kind": "new", "layers": ["indexing"]}}))
        self.review(self.round())
        self.assertEqual(self.next()["action"], "clean")
        self.add(t="stop", layer="indexing", reason="clean")
        self.assertEqual(final_gates.gate_layers_clean(self.run)["status"], "pass")
        self.finding("F1")
        self.round(checks="red")
        self.assertEqual(self.state().stops, {})
        self.assertEqual(final_gates.gate_layers_clean(self.run)["status"], "fail")


class ProtocolTestingReport(unittest.TestCase):
    STEP = {"report_checks": {"indexing": ["index", "state.components", "balances"], "simulation": ["decode", "sim.ladder"], "execution": ["*"]}}

    def judge(self, results, layer, exit_code=1):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "r.json"
            if results is not None:
                path.write_text(json.dumps({"results": results}))
            return layer_checks.judge_report(path, layer_checks.layer_patterns(self.STEP, layer), exit_code)[0]

    def test_each_layer_answers_for_its_own_and_earlier_checks(self):
        results = [{"check": "index", "outcome": "pass"}, {"check": "balances", "outcome": "pass"},
                   {"check": "sim.ladder", "outcome": "fail"}, {"check": "exec.ladder", "outcome": "not_run", "reason": "sim.ladder failed"}]
        self.assertEqual(self.judge(results, "indexing"), "pass")
        self.assertEqual(self.judge(results, "simulation"), "fail")
        self.assertEqual(self.judge(results, "final"), "fail")

    def test_a_missing_report_or_an_unexplained_exit_fails(self):
        self.assertEqual(self.judge(None, "indexing", exit_code=0), "fail")
        self.assertEqual(self.judge([], "indexing", exit_code=0), "fail")
        self.assertEqual(self.judge([{"check": "index", "outcome": "pass"}], "indexing", exit_code=101), "fail")
        self.assertEqual(self.judge([{"check": "index", "outcome": "skipped"}], "indexing", exit_code=0), "fail")
        self.assertEqual(self.judge([{"check": "index", "outcome": "skipped", "reason": "dust"}], "indexing", exit_code=0), "pass")


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
        sidecar.write_text(json.dumps({"head": "h1", "earlier": [{"id": "E7", "status": "fixed", "file": "src/a.rs", "claim": "claim F1"}], "findings": []}))
        self.add(t="round", layer="indexing", n=1, head="h1", checks="green")
        result = ledger.ingest_review(self.run, "indexing", 1, sidecar)
        self.assertEqual(result["unmatched_earlier"], [])
        self.assertEqual(self.state().findings["F1"]["status"], "fixed")


if __name__ == "__main__":
    unittest.main()
