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
import envcheck  # noqa: E402
import final_gates  # noqa: E402
import layer_checks  # noqa: E402
import onchain  # noqa: E402
import pr_body  # noqa: E402
import prompt  # noqa: E402
import run_agent  # noqa: E402
import schema  # noqa: E402
import split  # noqa: E402
import state as ledger  # noqa: E402
from unittest import mock  # noqa: E402

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
        """A review of round `n` at that round's commit, as ingest-review records one."""
        head = next(r["head"] for r in self.state().rounds[layer] if r["n"] == n)
        self.add(t="review", layer=layer, n=n, report="r.md", head=head, open=len(self.state().open_findings(layer)))

    def answers(self, items):
        path = Path(self.tmp.name) / "answers.json"
        path.write_text(json.dumps(items))
        return path

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
        self.assertEqual(result["errors"], ["F1: event answer: a dispute carries `evidence`"])
        self.assertEqual(self.state().findings["F1"]["answers"], [])

    def test_skip_is_not_an_answer(self):
        self.finding("F1")
        answers = Path(self.tmp.name) / "answers.json"
        answers.write_text(json.dumps([{"id": "F1", "kind": "skip"}]))
        self.assertIn("must be one of", ledger.record_answers(self.run, "indexing", 1, answers)["errors"][0])
        self.assertEqual(self.state().findings["F1"]["answers"], [])

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
        # classify has a stage of its own: a crash in it resumes there, not at research with no route
        self.assertEqual(ledger.resume_point(self.run, self.state())["stage"], "classify")
        self.add(t="stage", stage="classify", status="done")
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


class GitTree(unittest.TestCase):
    """A temporary git repository with a strict test yaml and protocols/testing's executor table,
    and the guard hook run against it."""

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

    EXECUTION_RS = "protocols/testing/src/execution.rs"
    MAPPING = 'const FOO_BYTECODE_JSON: &str = include_str!("../fixtures/Foo.runtime.json");\n    map.insert("foo", FOO_BYTECODE_JSON);\n'

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

    def hook(self, role, tool, tool_input, env_drop=(), label=""):
        # The test tree lives in the temp dir, which a role may write: name another as the temp root.
        env = {**os.environ, "TYCHO_INTEGRATE_GUARD": role, "TYCHO_INTEGRATE_WORKTREE": str(self.tree), "TYCHO_INTEGRATE_RUN": str(Path(self.tmp.name) / "run"),
               "TYCHO_INTEGRATE_REPO": str(self.tree), "TYCHO_INTEGRATE_TMP_ROOTS": str(Path(self.tmp.name) / "scratch"),
               "TYCHO_INTEGRATE_LABEL": label}
        for k in env_drop:
            env.pop(k)
        proc = subprocess.run([sys.executable, "-I", str(GUARD)], input=json.dumps({"tool_name": tool, "tool_input": tool_input, "cwd": str(self.tree)}), capture_output=True, text=True, env=env)
        # A crashing guard lets the call through in Claude Code, so a crash is never "allow" here.
        self.assertEqual((proc.returncode, proc.stderr), (0, ""), "the guard crashed")
        return json.loads(proc.stdout)["hookSpecificOutput"]["permissionDecision"] if proc.stdout.strip() else "allow"


class Guards(GitTree):
    def test_guarded_path_and_relaxed_yaml_line_are_violations(self):
        self.commit(".claude/knowledge/indexing.md", "weaker rule")
        self.commit("pkg/integration_test.tycho.yaml", "harness: strict\nskip_balance_check: true\ntests: []\n")
        self.commit("pkg/src/lib.rs", "fn main() {}")
        found = ledger.check_guards(self.tree, self.base, "HEAD")
        self.assertEqual(sorted(v["path"] for v in found), [".claude/knowledge/indexing.md", "pkg/integration_test.tycho.yaml"])

    def test_executor_fixture_lines_are_the_one_allowed_harness_change(self):
        self.commit(self.EXECUTION_RS, "fn mapping() {\n}\n")
        base = self.git("rev-parse", "HEAD").strip()
        self.commit(self.EXECUTION_RS, "fn mapping() {\n" + self.MAPPING + "}\n")
        self.assertEqual(ledger.check_guards(self.tree, base, "HEAD"), [])
        self.commit(self.EXECUTION_RS, "fn mapping() {\n" + self.MAPPING + "    let strict = false;\n}\n")
        found = ledger.check_guards(self.tree, base, "HEAD")
        self.assertEqual(len(found), 1)
        self.assertIn("let strict = false", found[0]["why"])

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
        self.assertEqual(self.hook("researcher", "Write", {"file_path": str(Path(self.tmp.name) / "run/spec.json"), "content": "{}"}, label="research"), "allow")


class GuardWritesByRole(GitTree):
    """Writes, pushes and GitHub calls the guard denies however they are spelled."""

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
        self.assertEqual(self.hook("researcher", "Bash", {"command": f"echo '{{}}' > {self.run_dir()}/spec.json"}, label="research"), "allow")

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

    def test_the_read_set_names_only_agreed_attributes(self):
        spec = {"read_set": [{"address": "0xp", "slots": [], "tracked_by": "attribute `reserves`"}]}
        dec = {"agreements": {"protocol_system": "foo", "component_id": "x", "attributes": [{"name": "fee"}]}, "paths": {"package": "p"}}
        self.assertTrue(any("reserves" in e for e in schema.validate_decisions(dec, spec)))
        dec["agreements"]["attributes"].append({"name": "reserves"})
        self.assertEqual(schema.validate_decisions(dec, spec), [])

    def test_decision_values_cannot_carry_a_command(self):
        dec = {"agreements": {"protocol_system": "foo", "component_id": "x"}, "paths": {"package": "x; touch /tmp/pwned; echo"}}
        self.assertTrue(any("paths.package" in e for e in schema.validate_decisions(dec, {})))
        dec["paths"]["package"] = "ethereum-foo"
        self.assertEqual(schema.validate_decisions(dec, {}), [])


class GuardScriptsAndGitHub(GitTree):
    def test_agents_do_not_run_the_runs_state_changing_scripts(self):
        for role in ("implementer", "researcher", "reviewer"):
            self.assertEqual(self.hook(role, "Bash", {"command": "python3 -I .claude/skills/tycho-integrate/scripts/state.py Foo decide --id Q1 --choice x --waive"}), "deny")
        self.assertEqual(self.hook("researcher", "Bash", {"command": "python3 -I .claude/skills/tycho-integrate/scripts/state.py Foo status"}), "allow")

    def test_attached_field_flags_make_gh_api_a_write(self):
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "gh api repos/a/b/issues/1/comments -fbody=x"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "gh api repos/a/b/pulls/1 --jq .title"}), "allow")

    def test_a_review_touches_no_branch_and_only_its_own_worktree(self):
        self.assertEqual(self.hook("reviewer", "Bash", {"command": "git branch -f main HEAD~3"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": f"git worktree remove --force {self.tree}/../integrate-foo"}), "deny")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": f"git worktree remove --force {self.tree}/.claude/reviews/r/worktree"}), "allow")
        self.assertEqual(self.hook("reviewer", "Bash", {"command": f"python3 -c \"from pathlib import Path; Path('{self.tree}/.claude/reviews/r.json').write_text('scripts/guard.py')\""}), "allow")

    def test_part_of_a_guarded_yaml_line_cannot_be_edited(self):
        yaml = str(self.tree / "pkg/integration_test.tycho.yaml")
        self.assertEqual(self.hook("implementer", "Edit", {"file_path": yaml, "old_string": "strict", "new_string": "legacy"}), "deny")

    def test_a_new_system_may_join_an_existing_decoder_arm(self):
        arm = '"a" | "b" => stream_builder.exchange_with_decoder_context::<S>(protocol_system, tvl_filter, None, decoder_context),'
        self.assertEqual(schema._state_registry_change(arm, arm.replace('"b" =>', '"b" | "fork" =>')), [])
        self.assertTrue(schema._state_registry_change(arm, arm.replace('"b" =>', '"b" | "a" =>')))


class RunRecords(unittest.TestCase):
    def test_records_a_session_may_not_write_are_put_back(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as saved:
            run = Path(d)
            (run / "ledger.jsonl").write_text("{}\n")
            (run / "spec.json").write_text("{}")
            (run / "decisions.json").write_text("{}")
            before = schema.records_digest(run, "research")
            for rel in before:
                Path(saved, rel).write_bytes((run / rel).read_bytes())
            (run / "ledger.jsonl").write_text("{}\nforged\n")
            (run / "decisions.json").write_text('{"forged": true}')  # the person's answers: not research's
            (run / "spec.json").write_text('{"changed": true}')  # research's own file
            (run / "rounds").mkdir()
            (run / "rounds" / "x.json").write_text("{}")
            self.assertEqual(run_agent.restore_records(run, Path(saved), before, "research"), ["decisions.json", "ledger.jsonl", "rounds/x.json"])
            self.assertEqual((run / "ledger.jsonl").read_text(), "{}\n")
            self.assertEqual((run / "decisions.json").read_text(), "{}")
            self.assertEqual((run / "spec.json").read_text(), '{"changed": true}')
            self.assertFalse((run / "rounds" / "x.json").exists())


class VenueQuotes(unittest.TestCase):
    SPEC = {"venue_quotes": [{"id": "V1", "chain": "ethereum", "block": 1, "to": "0xv", "data": "0x", "result": "0x05"}]}

    def quotes(self, router_error):
        def rpc(url, method, params, timeout=20):
            if params[0].get("from"):
                raise RuntimeError(router_error)
            return "0x05"
        with mock.patch.object(schema, "rpc_url", return_value="http://node"), mock.patch.object(onchain, "router", return_value="0xr"), \
                mock.patch.object(envcheck, "rpc", side_effect=rpc):
            return onchain.check_quotes(self.SPEC, Path("."))[0]["status"]

    def test_only_a_revert_from_the_router_is_caller_dependent(self):
        self.assertEqual(self.quotes("execution reverted: not whitelisted"), "caller-dependent")
        self.assertEqual(self.quotes("daily request count exceeded, request rate limited"), "unread")


class FinalGates(GitTree):
    def setUp(self):
        super().setUp()
        self.git("update-ref", "refs/remotes/origin/main", self.base)

    def test_the_venue_quote_test_names_the_block_and_asserts_the_result(self):
        spec = {"venue_quotes": [{"id": "V1", "block": 21000000, "result": "0x" + f"{12345:064x}"}]}
        test = "crates/tycho-simulation/src/evm/protocol/foo/state.rs"
        self.commit(test, "#[test]\nfn quotes_like_the_venue() { at_block(21_000_000); }\n")
        self.assertEqual(final_gates.gate_venue_quote_test(spec, self.tree, ["simulation"], final_gates.changed_files(self.tree))["status"], "fail")
        self.commit(test, "#[test]\nfn quotes_like_the_venue() { at_block(21_000_000); assert_eq!(out, 12345); }\n")
        self.assertEqual(final_gates.gate_venue_quote_test(spec, self.tree, ["simulation"], final_gates.changed_files(self.tree))["status"], "pass")
        self.assertEqual(final_gates.gate_venue_quote_test(spec, self.tree, ["execution"], final_gates.changed_files(self.tree))["status"], "n/a")

    def test_an_accepted_guarded_change_covers_that_change_only(self):
        run = Path(self.tmp.name) / "run"
        run.mkdir()
        ledger.ledger_path(run).touch()
        self.commit("protocols/testing/src/lib.rs", "weaker")
        found = ledger.check_guards(self.tree, self.base, "HEAD")
        ledger.append(run, {"t": "guard_violation", "layer": "indexing", "round": 1, "paths": found})
        ledger.append(run, {"t": "decision_needed", "id": "Q1", "class": "relax-test", "question": "?", "layer": "indexing", "escalation": "guard"})
        ledger.append(run, {"t": "decision", "id": "Q1", "choice": "accept", "by": "person", "accept_guard": True})
        self.assertEqual(final_gates.gate_guards(run, self.tree)["status"], "pass")
        self.commit(self.EXECUTION_RS, "gone")
        self.assertEqual(final_gates.gate_guards(run, self.tree)["status"], "fail")

    def test_a_split_keeps_the_copy_commit_before_its_adaptation(self):
        self.git("checkout", "-qb", "integrate/foo")
        self.commit("ix/copy.rs", "upstream\n", msg="copy verbatim")
        self.commit("ix/copy.rs", "upstream\nadapted\n", msg="adapt")
        self.commit("sim/s.rs", "s\n", msg="simulation")
        self.git("checkout", "-q", self.base)
        target = Path(self.tmp.name) / "pr-ix"
        self.git("worktree", "add", "-q", "-b", "pr/ix", str(target), self.base)
        split.replay(target, "integrate/foo", self.base, ["ix"])
        log = subprocess.run(["git", "-C", str(target), "log", "--format=%s", f"{self.base}..HEAD"], capture_output=True, text=True).stdout.split("\n")
        self.assertEqual([m for m in log if m], ["adapt", "copy verbatim"])


class ReviewLevel(GitTree):
    def test_full_on_first_rounds_disputes_and_quote_path_changes(self):
        first = self.git("rev-parse", "HEAD").strip()
        self.commit("docs/a.md", "x")
        docs = self.git("rev-parse", "HEAD").strip()
        self.commit("crates/tycho-simulation/src/evm/protocol/foo/state.rs", "x")
        quote = self.git("rev-parse", "HEAD").strip()
        st = ledger.State([{"t": "round", "layer": "simulation", "n": 1, "head": first, "checks": "green"}])
        self.assertEqual(prompt.review_level(st, "simulation", 1, self.tree), "full")
        st._apply({"t": "round", "layer": "simulation", "n": 2, "head": docs, "checks": "green"})
        self.assertEqual(prompt.review_level(st, "simulation", 2, self.tree), "partial")
        st._apply({"t": "round", "layer": "simulation", "n": 3, "head": quote, "checks": "green"})
        self.assertEqual(prompt.review_level(st, "simulation", 3, self.tree), "full")


class ReviewLevelBranches(GitTree):
    def test_full_while_a_dispute_awaits_its_verdict_and_when_attributes_change(self):
        first = self.git("rev-parse", "HEAD").strip()
        self.commit("protocols/substreams/foo/src/modules.rs", "fn x() {}\n")
        plain = self.git("rev-parse", "HEAD").strip()
        self.commit("protocols/substreams/foo/src/modules.rs", "fn x() { attribute(\"fee\") }\n")
        attr = self.git("rev-parse", "HEAD").strip()
        st = ledger.State([{"t": "round", "layer": "indexing", "n": 1, "head": first, "checks": "green"},
                           {"t": "round", "layer": "indexing", "n": 2, "head": plain, "checks": "green"}])
        self.assertEqual(prompt.review_level(st, "indexing", 2, self.tree), "partial")
        st._apply({"t": "finding", "id": "F1", "layer": "indexing", "rule": "r", "severity": "nit", "file": "f", "claim": "c"})
        st._apply({"t": "answer", "id": "F1", "round": 2, "kind": "dispute", "evidence": "e"})
        self.assertEqual(prompt.review_level(st, "indexing", 2, self.tree), "full")
        st = ledger.State([{"t": "round", "layer": "indexing", "n": 1, "head": plain, "checks": "green"},
                           {"t": "round", "layer": "indexing", "n": 2, "head": attr, "checks": "green"}])
        self.assertEqual(prompt.review_level(st, "indexing", 2, self.tree), "full")


class Retry(unittest.TestCase):
    def test_a_step_the_environment_fails_twice_is_infra(self):
        infra = {"name": "s", "status": "fail", "detail": "429", "infra": "rpc rate limit"}
        with mock.patch.object(layer_checks, "RETRY_PAUSE_S", 0), mock.patch.object(layer_checks, "run_once", side_effect=[infra, dict(infra)]):
            self.assertEqual(layer_checks.run_with_retry({}, {}, Path("."), Path("."), "s", "ethereum", None)["status"], "infra")
        with mock.patch.object(layer_checks, "RETRY_PAUSE_S", 0), mock.patch.object(layer_checks, "run_once", side_effect=[infra, {"name": "s", "status": "pass"}]):
            self.assertEqual(layer_checks.run_with_retry({}, {}, Path("."), Path("."), "s", "ethereum", None)["status"], "pass")
        code = {"name": "s", "status": "fail", "detail": "assertion failed"}
        with mock.patch.object(layer_checks, "run_once", side_effect=[code]) as once:
            self.assertEqual(layer_checks.run_with_retry({}, {}, Path("."), Path("."), "s", "ethereum", None)["status"], "fail")
            self.assertEqual(once.call_count, 1)


class SpecRender(unittest.TestCase):
    def test_every_spec_section_has_a_title(self):
        import spec_render
        self.assertEqual(set(spec_render.SECTION_TITLES), set(schema.SPEC_SECTIONS))


class SpecValidation(unittest.TestCase):
    def spec(self, **fact):
        return {"protocol": "Foo", "kind": "native", "kind_fact": "S1", "chains": ["ethereum"], **self.READINGS,
                "facts": [{"id": "S1", "section": "swap", "claim": "fee is 30 bps", **fact}]}

    READINGS = {
        "venue_quotes": [{"id": "V1", "chain": "ethereum", "block": 21000000, "to": "0xpool", "data": "0x12", "result": "0x01"}],
        "watch": [{"chain": "ethereum", "address": "0xpool", "block": 21000000}],
        "read_set": [{"address": "0xpool", "slots": ["0x0"], "tracked_by": "attribute reserves"}],
        "gas": {"swap_gas_used": 120000, "evidence": "0xtx"},
        "coverage": {"served": 8, "total": 10, "unit": "tvl_usd", "source": "defillama"},
    }

    def test_the_spec_carries_the_readings_the_gates_re_read(self):
        spec = self.spec(status="unknown")
        self.assertEqual(schema.validate_spec(spec), [])
        for key in self.READINGS:
            self.assertTrue(schema.validate_spec({k: v for k, v in spec.items() if k != key}), key)
        self.assertTrue(schema.validate_spec({**spec, "chains": ["ethereum", "base"]}))
        self.assertTrue(schema.validate_spec({**spec, "read_set": [{"address": "0xpool", "slots": ["0x0"]}]}))

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


class ResumeAndReviewIdentity(RunCase):
    """Re-running a step after a crash, and reviews of the wrong commit, change nothing."""

    def test_a_round_recorded_again_at_the_same_head_is_the_same_round(self):
        for _ in range(5):
            ledger.record_round(self.run, self.state(), "indexing", "abc", "red", None)
        self.assertEqual([r["n"] for r in self.state().rounds["indexing"]], [1])
        ledger.record_round(self.run, self.state(), "indexing", "abc", "green", None)
        self.assertEqual(self.state().rounds["indexing"][-1]["checks"], "green")

    def test_a_round_after_a_review_at_the_same_head_is_a_new_round(self):
        ledger.record_round(self.run, self.state(), "indexing", "abc", "green", None)
        self.review(1)
        ledger.record_round(self.run, self.state(), "indexing", "abc", "green", None)
        self.assertEqual([r["n"] for r in self.state().rounds["indexing"]], [1, 2])

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
        self.assertEqual(self.next(), {"action": "continue", "reason": "the last round's review left out a core pass"})

    def test_parts_a_review_leaves_out_by_design_do_not_make_it_incomplete(self):
        n = self.round()
        not_run = ["integration pass: level partial", "execution verifier (pass `full`)", "ast-grep: no sgconfig.yml at the base",
                   "chunk passes on 3 files: unchanged since abc123"]
        ledger.ingest_review(self.run, "indexing", n, self.sidecar({"not_run": not_run, "findings": []}))
        self.assertEqual(self.next()["action"], "clean")

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
        steps = [{"name": "protocol-testing", "status": "fail", "checks": [{"test": "t", "check": c, "outcome": "fail", "detail": "x"} for c in failing]}] if failing else [{"name": "protocol-testing", "status": "pass"}]
        path.write_text(json.dumps({"green": not failing, "steps": steps}))
        return path

    def test_a_failed_check_is_a_finding_a_dispute_can_settle(self):
        ledger.record_round(self.run, self.state(), "indexing", "h1", None, self.checks(["balances"]))
        [f] = self.state().open_findings("indexing")
        self.assertEqual(f["rule"], "check:protocol-testing:balances")
        self.review(1)
        self.assertEqual(self.next()["reason"], "checks red")
        # the review upholds the implementer's dispute: the failure is accepted
        self.add(t="verdict", id=f["id"], status="refuted", by="review", round=1)
        self.assertEqual(self.next()["action"], "clean")

    def test_a_check_that_passes_again_closes_its_finding(self):
        ledger.record_round(self.run, self.state(), "indexing", "h1", None, self.checks(["balances"]))
        ledger.record_round(self.run, self.state(), "indexing", "h2", None, self.checks([]))
        self.assertEqual(self.state().open_findings("indexing"), [])

    def test_a_check_that_did_not_run_keeps_its_finding(self):
        ledger.record_round(self.run, self.state(), "indexing", "h1", None, self.checks(["balances"]))
        infra = Path(self.tmp.name) / "infra.json"
        infra.write_text(json.dumps({"green": False, "steps": [{"name": "protocol-testing", "status": "infra", "infra": "rpc", "detail": "x"}]}))
        ledger.record_round(self.run, self.state(), "indexing", "h2", None, infra)
        self.assertEqual(len(self.state().open_findings("indexing")), 1)


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
        checks.write_text(json.dumps({"green": True, "steps": [{"name": "protocol-testing", "status": "pass"}]}))
        ledger.record_round(self.run, self.state(), "indexing", "h1", None, checks)
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
        (self.run / "decisions.json").write_text(json.dumps({"agreements": {"protocol_system": "foo", "component_id": "pool address"},
                                                             "paths": {"package": "foo", "test_package": {"ethereum": "ethereum-foo", "base": "base-foo"}}}))
        self.assertEqual(layer_checks.placeholders(self.run, Path("/t"), "base")["test_package"], "base-foo")
        self.assertEqual(layer_checks.placeholders(self.run, Path("/t"), "ethereum")["test_package"], "ethereum-foo")
        self.assertTrue(schema.validate_intake({"protocol": "Foo", "chains": ["ethereum"], "primary_chain": "base", "anchor": {"docs": "x"}}))


class PullRequestBody(RunCase):
    def test_the_description_carries_the_evidence_and_what_is_not_verified(self):
        report = Path(self.tmp.name) / "pt.report.json"
        report.write_text(json.dumps({"results": [{"check": "index", "outcome": "pass"}, {"check": "exec.ladder", "outcome": "skipped", "component": "0xc", "reason": "dust"}], "drift": []}))
        checks = Path(self.tmp.name) / "checks.json"
        checks.write_text(json.dumps({"steps": [{"name": "protocol-testing@ethereum", "status": "pass", "cmd": "cargo run -- range", "report": str(report)}]}))
        (self.run / "final_gates.json").write_text(json.dumps({"gates": [
            {"name": "checks", "status": "pass", "detail": "", "file": str(checks)},
            {"name": "venue-quotes", "status": "pass", "detail": "", "quotes": [{"id": "V1", "chain": "ethereum", "block": 1, "status": "same"}]}],
            "off": [{"gap": "G6", "what": "golden vectors replayed", "reason": "not landed"}]}))
        (self.run / "spec.json").write_text(json.dumps({"protocol": "Foo", "kind": "native", "chains": ["ethereum"]}))
        self.finding("F1")
        self.add(t="answer", id="F1", round=1, kind="dispute", evidence="cast call at 21000000 returns 5")
        self.add(t="verdict", id="F1", status="refuted", by="review", round=1)
        with mock.patch.object(schema, "run_dir", return_value=self.run):
            body, _ = pr_body.render("Foo")
        for text in ("### Evidence", "1 pass, 1 skipped", "skipped `exec.ladder` 0xc : dust", "venue-quotes V1 on ethereum", "### Not verified",
                     "golden vectors replayed (G6", "→ refuted by review", "executors.sql", "What CI will not guard"):
            self.assertIn(text, body)


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
        self.assertEqual(self.judge([{"check": "index", "outcome": "skipped", "reason": "dust"}], "indexing", exit_code=0), "fail")
        self.assertEqual(self.judge([{"check": "index", "outcome": "pass"}, {"check": "balances", "outcome": "skipped", "reason": "dust"}], "indexing", exit_code=0), "pass")


class ReviewFindings(RunCase):
    """What the skill's own review found in the ledger, each as it should behave."""

    def test_an_amendment_reopens_the_layers_built_on_the_fact(self):
        (self.run / "decisions.json").write_text(json.dumps({"decisions": [{"id": "D1", "facts": ["S3"], "layers": ["indexing"]}]}))
        self.review(self.round())
        self.assertEqual(self.next()["action"], "clean")
        self.add(t="gate", gate=1, status="approved", facts=["S3"])
        result = ledger.amend(self.run, "S3", "cast call", "fee is 25 bps", "chain")
        self.assertEqual((result["layers"], result["needs_person"]), (["indexing"], True))
        # a signed fact: the person first
        self.assertEqual(self.next(), {"action": "escalate", "reason": "decision-needed", "decisions": [result["decision"]]})
        self.add(t="decision", id=result["decision"], choice="accept", by="person")
        self.assertEqual(self.next()["reason"], "a spec amendment reopened the layer: a round rebuilds it on the changed fact")
        self.assertEqual(final_gates.gate_layers_clean(self.run)["status"], "fail")
        self.review(self.round())
        self.assertEqual(self.next()["action"], "clean")

    def test_a_rebase_makes_the_old_reviews_stale_and_the_new_head_reviewable(self):
        self.review(self.round())
        self.add(t="rebase", head="rebased")
        self.assertEqual(self.next()["reason"], "not reviewed")
        ledger.ingest_review(self.run, "auto", None, self.sidecar({"head": "rebased", "findings": []}))
        self.assertEqual(self.next()["action"], "clean")

    def test_a_persons_answer_to_an_escalation_reaches_the_next_round(self):
        self.round(checks="red")
        self.add(t="decision_needed", id="Q1", class_="x", question="Which way does the code go?", layer="indexing", escalation="oscillation") if False else None
        self.add(**{"t": "decision_needed", "id": "Q1", "class": "not-converged", "question": "Which way does the code go?", "layer": "indexing", "escalation": "oscillation"})
        self.add(t="decision", id="Q1", choice="keep the checked math", by="person")
        text = ledger.answers_file(self.state(), "indexing")
        self.assertIn("Which way does the code go? → **keep the checked math**", text)

    def test_an_environment_rerun_after_the_review_replaces_the_round(self):
        checks = Path(self.tmp.name) / "infra.json"
        checks.write_text(json.dumps({"green": False, "steps": [{"name": "protocol-testing", "status": "infra", "infra": "rpc", "detail": "x"}]}))
        ledger.record_round(self.run, self.state(), "indexing", "h1", None, checks)
        self.review(1)
        checks.write_text(json.dumps({"green": True, "steps": [{"name": "protocol-testing", "status": "pass"}]}))
        ledger.record_round(self.run, self.state(), "indexing", "h1", None, checks)
        self.assertEqual([r["n"] for r in self.state().rounds["indexing"]], [1])
        self.assertEqual(self.next()["action"], "clean")

    def test_a_final_review_nit_holds_no_layer_and_a_blocking_finding_adds_rounds(self):
        self.review(self.round())
        ledger.ingest_review(self.run, "auto", None, self.sidecar({"findings": [{"rule": "r", "file": "protocols/substreams/x/src/a.rs", "claim": "c", "severity": "nit"}]}))
        self.assertEqual(self.next()["action"], "clean")
        for _ in range(BUDGETS["layer"]["rounds"]):
            self.review(self.round(checks="red"))
        self.assertEqual(self.next()["reason"], "round-cap")
        ledger.ingest_review(self.run, "auto", None, self.sidecar({"findings": [{"rule": "q", "file": "protocols/substreams/x/src/b.rs", "claim": "d", "severity": "blocking"}]}))
        self.assertNotEqual(self.next().get("reason"), "round-cap")

    def test_core_passes_that_did_not_run_make_a_review_incomplete(self):
        incomplete = ["verification: skipped, the harness could not start", "chunk pass 3: subagent failed, CI budget",
                      "chunk pass 2 (harness and CI files): agent timed out", "simplification pass: not run, ast-grep crashed first"]
        self.assertEqual(ledger.core_not_run(incomplete), incomplete)
        by_design = ["integration pass: level partial", "execution verifier (pass `full`)", "ast-grep: no sgconfig.yml at the base",
                     "chunk passes on 3 files: unchanged since abc123", "tools not at base: dylint"]
        self.assertEqual(ledger.core_not_run(by_design), [])

    def test_two_findings_with_the_same_code_at_two_places_are_two_findings(self):
        n = self.round()
        same = {"rule": "r", "file": "src/a.rs", "quote": ".unwrap_or_default()", "claim": "c"}
        result = ledger.ingest_review(self.run, "indexing", n, self.sidecar({"findings": [{**same, "lines": "10-10"}, {**same, "lines": "80-80"}]}))
        self.assertEqual(result["added"], ["F1", "F2"])

    def test_an_earlier_finding_that_matches_two_is_reported_not_guessed(self):
        self.finding("F1", quote="a")
        self.finding("F2", quote="b")
        self.add(t="finding", id="F2", layer="indexing", rule="rule-a", severity="blocking", file="src/a.rs", claim="claim F1", quote="b", round=1)
        n = self.round()
        result = ledger.ingest_review(self.run, "indexing", n, self.sidecar({"earlier": [{"id": "E1", "status": "fixed", "file": "src/a.rs", "claim": "claim F1"}], "findings": []}))
        self.assertEqual(result["unmatched_earlier"], ["E1"])

    def test_a_session_of_no_layer_that_changed_records_is_escalated(self):
        self.review(self.round())
        self.add(t="tamper", label="final-review", paths=["ledger.jsonl"])
        self.assertEqual(self.next()["reason"], "tamper")
        self.assertEqual(ledger.resume_point(self.run, self.state())["stage"], "tamper")

    def test_the_status_shows_caps_with_grants(self):
        self.round(checks="red")
        self.add(**{"t": "decision_needed", "id": "Q1", "class": "not-converged", "question": "?", "layer": "indexing", "escalation": "round-cap"})
        self.add(t="decision", id="Q1", choice="more", by="person", grant_rounds=2)
        self.assertIn(f"1/{BUDGETS['layer']['rounds'] + 2} rounds", ledger.status_text(self.run, self.state()))

    def test_validate_names_every_documents_errors(self):
        (self.run / "intake.json").write_text(json.dumps({"protocol": "Foo", "chains": ["ethereum"], "anchor": {"docs": "x"}}))
        (self.run / "decisions.json").write_text(json.dumps({"agreements": {"protocol_system": "foo"}, "paths": {"package": "x; rm -rf /"}}))
        errors = ledger.validate_run(self.run)
        self.assertTrue(any("component_id" in e for e in errors) and any("paths.package" in e for e in errors))


class SignedFacts(RunCase):
    def test_a_signed_fact_changed_without_an_amendment_fails_the_gate(self):
        spec = {"facts": [{"id": "S1", "claim": "fee is 30 bps", "status": "verified", "evidence": [{"kind": "tx", "ref": "0x1"}]}]}
        (self.run / "spec.json").write_text(json.dumps(spec))
        self.add(t="gate", gate=1, status="approved", facts=["S1"], digests={"S1": schema.fact_digest(spec["facts"][0])})
        self.assertEqual(final_gates.gate_signed_facts(self.run, spec)["status"], "pass")
        spec["facts"][0]["claim"] = "fee is 5 bps"
        self.assertEqual(final_gates.gate_signed_facts(self.run, spec)["status"], "fail")
        self.add(t="amendment", fact="S1", evidence="cast call", new="fee is 5 bps")
        self.assertEqual(final_gates.gate_signed_facts(self.run, spec)["status"], "pass")


class LayerChecksSteps(unittest.TestCase):
    STEP = {"name": "adapter-test", "layers": ["simulation"], "when": ["vm"], "cmd": "forge test --match-contract {adapter}"}

    def test_a_path_the_route_does_not_build_fails_or_does_not_apply(self):
        values = {"kind": "vm", "tree": "/t", "chain": "ethereum"}
        self.assertEqual(layer_checks.applies(self.STEP, "simulation", values, {}, Path("."), ["simulation"]), ("fail", "decisions.json paths lack ['adapter']"))
        self.assertEqual(layer_checks.applies(self.STEP, "indexing", values, {}, Path("."), ["indexing"])[0], None)
        self.assertEqual(layer_checks.applies(self.STEP, "final", {**values, "kind": "native"}, {}, Path("."), ["indexing", "simulation"])[0], "n/a")
        self.assertEqual(layer_checks.applies(self.STEP, "final", values, {}, Path("."), ["indexing"])[0], None)

    def test_a_missing_kind_fails_the_kind_gated_steps(self):
        self.assertEqual(layer_checks.applies(self.STEP, "simulation", {"kind": ""}, {}, Path("."), ["simulation"])[0], "fail")

    def test_the_implementers_commands_come_from_checks_toml(self):
        text = prompt.layer_commands("execution")
        for name in ("execution-clippy", "executor-fmt", "rust-fmt", "licensing", "protocol-testing"):
            self.assertIn(f"`{name}`", text)
        self.assertIn("tycho-router-model", text)


class GuardBashForms(GitTree):
    """Each command form the guard must read: its writes, its git subcommand, what it wraps."""

    CASES = [
        ("reviewer", "git merge-base HEAD origin/main", "allow", ""),
        ("reviewer", "git merge-base --is-ancestor a b", "allow", ""),
        ("reviewer", "git cat-file commit HEAD", "allow", ""),
        ("reviewer", "git tag --list", "allow", ""),
        ("reviewer", "git tag v1", "deny", ""),
        ("reviewer", "git branch newb", "deny", ""),
        ("reviewer", "git fetch . HEAD~3:integrate/foo", "deny", ""),
        ("reviewer", "git fetch origin pull/1/head", "allow", ""),
        ("reviewer", "git worktree add -B integrate/foo {repo}/.claude/reviews/r/w", "deny", ""),
        ("reviewer", "git worktree add --detach {repo}/.claude/reviews/r/w HEAD", "allow", ""),
        ("researcher", "git clone https://github.com/a/b", "deny", "research"),
        ("researcher", "git clone https://github.com/a/b {run}/refs/b", "allow", "research"),
        ("researcher", "curl -o {tree}/src/x.rs https://x", "deny", "research"),
        ("implementer", "mise exec -- rm {repo}/../CLAUDE.md", "deny", ""),
        ("implementer", "find {tree}/.claude -name x.md -delete", "deny", ""),
        ("implementer", "bash -c 'echo x > {tree}/.claude/knowledge/rust.md'", "deny", ""),
        ("implementer", "python3 -c \"import os; os.system('echo x > y')\"", "deny", ""),
        ("implementer", "git submodule foreach 'rm -f x'", "deny", ""),
        ("implementer", "env git -C . push", "deny", ""),
        ("implementer", "git commit -am x", "allow", ""),
        ("implementer", "python3 -I s/scripts/layer_checks.py --protocol foo --layer indexing --scratch", "allow", ""),
        ("implementer", "python3 -I s/scripts/state.py foo decide --id Q1 --choice resume", "deny", ""),
        ("implementer", "python3 -I s/scripts/state.py foo status", "allow", ""),
        ("reviewer", "awk '{{print > \"x\"}}' f", "deny", ""),
        ("reviewer", "sed -n 'w {tree}/x' f", "deny", ""),
        ("researcher", "echo x > {run}/decisions.json", "deny", "research"),
        ("researcher", "echo x > {run}/decisions.json", "allow", "decide"),
        ("reviewer", "echo x > {run}/spec.json", "deny", "review:indexing:2"),
    ]

    def test_each_form(self):
        run = Path(self.tmp.name) / "run"
        for role, command, want, label in self.CASES:
            with self.subTest(role=role, command=command, label=label):
                cmd = command.format(repo=self.tree, tree=self.tree, run=run)
                self.assertEqual(self.hook(role, "Bash", {"command": cmd}, label=label), want)


class Checkout(GitTree):
    def test_a_moved_branch_is_put_back_and_a_changed_file_named(self):
        before = schema.checkout_digest(self.tree, "integrate/foo")
        self.git("branch", "side")
        self.commit("pkg/x.rs", "x")
        after = schema.checkout_digest(self.tree, "integrate/foo")
        # the default branch moved (the commit) and `side` appeared
        changed = run_agent.restore_checkout(self.tree, before, after)
        self.assertTrue(any("side" in c for c in changed))
        self.assertEqual(self.git("rev-parse", "HEAD").strip(), self.base)
        (self.tree / "pkg/integration_test.tycho.yaml").write_text("changed")
        self.assertTrue(any("checkout" in c for c in run_agent.restore_checkout(self.tree, before, schema.checkout_digest(self.tree, "integrate/foo"))))

    def test_worktree_reattaches_the_runs_branch(self):
        origin = Path(self.tmp.name) / "origin.git"
        self.git("branch", "-M", "main")  # the run builds on origin/main
        subprocess.run(["git", "clone", "-q", "--bare", str(self.tree), str(origin)], check=True)
        clone = Path(self.tmp.name) / "clone"
        subprocess.run(["git", "clone", "-q", str(origin), str(clone)], check=True)
        script = [sys.executable, "-I", str(Path(__file__).resolve().parent / "worktree.py"), "--protocol", "Foo"]
        self.assertEqual(subprocess.run(script, cwd=clone, capture_output=True, text=True).returncode, 0)
        subprocess.run(["rm", "-rf", str(clone / ".claude/worktrees/integrate-foo")], check=True)
        proc = subprocess.run(script, cwd=clone, capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_a_git_failure_says_why(self):
        with self.assertRaises(schema.GitError) as e:
            schema.git("-C", str(self.tree), "rev-parse", "--verify", "no-such-ref")
        self.assertIn("fatal", str(e.exception) + (e.exception.stderr or ""))

    def test_stale_references_know_nested_and_cloned_packages(self):
        self.commit("protocols/substreams/ethereum-uniswap-v4/with-hooks/Cargo.toml", "[package]")
        self.commit("protocols/testing/src/test_runner.rs", 'static CLONE_TO_BASE_PROTOCOL = [("base-foo", "ethereum-uniswap-v4-with-hooks")];')
        run = Path(self.tmp.name) / "run"
        run.mkdir()
        (run / "decisions.json").write_text(json.dumps({"paths": {"package": "ethereum-uniswap-v4-with-hooks", "test_package": "base-foo"}}))
        (run / "final_gates.json").write_text(json.dumps({"passed": True, "gates": []}))
        with mock.patch.object(schema, "run_dir", return_value=run), mock.patch.object(schema, "worktree_dir", return_value=self.tree):
            self.assertEqual([p for p in pr_body.stale_references("Foo", "") if "package" in p], [])
            (run / "final_gates.json").unlink()
            self.assertIn("the final gates have not run: the Tests section is empty", pr_body.stale_references("Foo", ""))

    def test_a_branch_diff_that_cannot_be_taken_fails_the_gates_that_need_it(self):
        self.assertIsNone(final_gates.changed_files(self.tree))  # no origin/main here
        self.assertEqual(final_gates.gate_sibling_executors(self.tree, None)["status"], "fail")
        self.assertEqual(pr_body.first_line(""), "(no detail)")


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
