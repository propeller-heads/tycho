"""Regression tests for fork resolution and CI test selection."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from resolve_package import build_dir, load_mapping
from select_protocols import MAPPING, affected_protocols, all_protocols, dependency_lock_changed


class ProtocolPackagesTest(unittest.TestCase):
    def setUp(self):
        self.mapping = {
            "bsc-ring-swap-v2": "ethereum-ring-swap-v2",
            "arc-uniswap-v4-no-hooks": "ethereum-uniswap-v4/no-hooks",
            "robinhood-uniswap-v4-with-hooks": "ethereum-uniswap-v4/with-hooks",
        }
        self.available = set(self.mapping) | {"ethereum-ring-swap-v2", "ethereum-uniswap-v4"}

    def select(self, paths, previous=None):
        return affected_protocols(
            paths, self.mapping, self.mapping if previous is None else previous, self.available
        )

    def test_nested_manifest_builds_parent_workspace(self):
        self.assertEqual(build_dir("arc-uniswap-v4-no-hooks", self.mapping), "ethereum-uniswap-v4")
        self.assertEqual(build_dir("ethereum-ring-swap-v2", self.mapping), "ethereum-ring-swap-v2")

    def test_new_mapping_only_selects_new_fork(self):
        previous = dict(self.mapping)
        del previous["bsc-ring-swap-v2"]
        self.assertEqual(self.select([MAPPING], previous), ["bsc-ring-swap-v2"])

    def test_fork_addition_with_manifest_and_fixture_only_selects_new_alias(self):
        previous = dict(self.mapping)
        del previous["bsc-ring-swap-v2"]
        self.assertEqual(
            self.select(
                [
                    MAPPING,
                    "protocols/substreams/ethereum-ring-swap-v2/bsc-ring-swap-v2.yaml",
                    "protocols/substreams/ethereum-ring-swap-v2/"
                    "integration_test_bsc_ring_swap_v2.tycho.yaml",
                ],
                previous,
            ),
            ["bsc-ring-swap-v2"],
        )

    def test_alias_manifest_edit_only_selects_its_owner(self):
        self.assertEqual(
            self.select(
                ["protocols/substreams/ethereum-uniswap-v4/no-hooks/arc-uniswap-v4-no-hooks.yaml"]
            ),
            ["arc-uniswap-v4-no-hooks"],
        )

    def test_shared_source_alongside_alias_files_selects_siblings(self):
        self.assertEqual(
            self.select(
                [
                    "protocols/substreams/ethereum-ring-swap-v2/bsc-ring-swap-v2.yaml",
                    "protocols/substreams/ethereum-ring-swap-v2/src/lib.rs",
                ]
            ),
            ["bsc-ring-swap-v2", "ethereum-ring-swap-v2"],
        )

    def test_root_dependencies_are_left_to_manual_dispatch(self):
        self.assertEqual(self.select(["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"]), [])

    def test_changed_mapping_only_selects_changed_fork(self):
        previous = dict(self.mapping, **{"bsc-ring-swap-v2": "ethereum-uniswap-v4"})
        self.assertEqual(self.select([MAPPING], previous), ["bsc-ring-swap-v2"])

    def test_format_only_mapping_change_selects_nothing(self):
        self.assertEqual(self.select([MAPPING]), [])

    def test_removed_alias_is_not_scheduled(self):
        previous = dict(self.mapping, **{"removed-fork": "ethereum-ring-swap-v2"})
        self.assertEqual(
            self.select([MAPPING], previous), ["bsc-ring-swap-v2", "ethereum-ring-swap-v2"]
        )

    def test_package_changes_include_all_forks(self):
        self.assertEqual(
            self.select(["protocols/substreams/ethereum-ring-swap-v2/src/lib.rs"]),
            ["bsc-ring-swap-v2", "ethereum-ring-swap-v2"],
        )

    def test_nested_workspace_changes_include_both_hook_variants(self):
        self.assertEqual(
            self.select(["protocols/substreams/ethereum-uniswap-v4/no-hooks/src/lib.rs"]),
            ["arc-uniswap-v4-no-hooks", "ethereum-uniswap-v4", "robinhood-uniswap-v4-with-hooks"],
        )

    def test_shared_changes_select_all(self):
        for path in [
            "protocols/testing/run.Dockerfile",
            "protocols/testing/scripts/resolve_package.py",
            "protocols/testing/src/test_runner.rs",
            "protocols/testing/fixtures/RingSwapV2.runtime.json",
            "protocols/substreams/crates/tycho-substreams/src/lib.rs",
            "protocols/substreams/Cargo.lock",
            "protocols/testing/build.rs",
            "protocols/testing/docker-compose.yaml",
            "protocols/testing/run.Dockerfile.dockerignore",
            "protocols/testing/rust-toolchain.toml",
            ".github/actions/substreams-docker-single/action.yml",
            ".github/workflows/ci-substreams-integration.yaml",
        ]:
            with self.subTest(path=path):
                self.assertEqual(self.select([path]), sorted(self.available))

    def test_unrelated_changes_select_nothing(self):
        self.assertEqual(
            self.select(
                [
                    "protocols/testing/README.md",
                    "docs/example.md",
                    "protocols/substreams/ethereum-ring-swap-v2/CHANGELOG.md",
                ]
            ),
            [],
        )

    def test_discovery_ignores_non_packages(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ["ethereum-ring-swap-v2", "ethereum-fluid"]:
                package = root / "protocols/substreams" / name
                package.mkdir(parents=True)
                (package / "Cargo.toml").touch()
            (root / "protocols/substreams/tests").mkdir()
            self.assertEqual(all_protocols(root, {}), {"ethereum-ring-swap-v2"})

    def test_invalid_mapping_fails_instead_of_using_wrong_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "mapping.json"
            for value in [{"fork": "../outside"}, {"fork": 123}, []]:
                path.write_text(json.dumps(value))
                with self.assertRaises(ValueError):
                    load_mapping(path)

    def test_local_package_version_bump_does_not_trigger_all_protocols(self):
        before = '\n[[package]]\nname = "ethereum-ring-swap-v2"\nversion = "0.1.0"\n'
        after = before.replace("0.1.0", "0.1.1")
        self.assertFalse(dependency_lock_changed(before, after))

    def test_external_dependency_change_triggers_all_protocols(self):
        before = '\n[[package]]\nname = "serde"\nversion = "1.0.0"\nsource = "registry+example"\n'
        self.assertTrue(dependency_lock_changed(before, before.replace("1.0.0", "1.0.1")))

    def test_local_package_dependency_change_is_not_a_version_only_bump(self):
        before = (
            '[[package]]\nname = "ethereum-ring-swap-v2"\nversion = "0.1.0"\n'
            'dependencies = ["shared-dependency 1.0.0"]\n'
        )
        after = before.replace("shared-dependency 1.0.0", "shared-dependency 2.0.0")
        self.assertTrue(dependency_lock_changed(before, after))


class ProtocolSelectionCliTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.script = Path(__file__).with_name("select_protocols.py").resolve()
        self.resolver = self.script.with_name("resolve_package.py")
        self.write_file(MAPPING, '{"bsc-curve": "ethereum-curve"}')
        self.add_package("ethereum-curve")

    def write_file(self, relative_path, contents=""):
        path = self.root / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
        return path

    def add_package(self, name):
        return self.write_file(f"protocols/substreams/{name}/Cargo.toml").parent

    def run_selection(self, *arguments, env=None):
        return subprocess.run(
            [sys.executable, "-E", "-s", str(self.script), "--root", str(self.root), *arguments],
            cwd=self.root,
            env=env,
            capture_output=True,
            text=True,
        )

    def git(self, *arguments):
        return subprocess.check_output(["git", "-C", str(self.root), *arguments], text=True).strip()

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def commit_base(self):
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")
        return self.commit("base")

    def selection_output(self, *arguments):
        output = self.root / "output"
        output.unlink(missing_ok=True)
        result = self.run_selection(*arguments, "--github-output", str(output))
        self.assertEqual(result.returncode, 0, result.stderr)
        protocols, matrix = output.read_text().splitlines()
        protocols = protocols.removeprefix("protocols=")
        matrix = json.loads(matrix.removeprefix("matrix="))
        self.assertEqual([entry["protocol"] for entry in matrix["include"]], protocols.split())
        return protocols, matrix

    def test_pr_selection_uses_merge_base_not_new_base_branch_changes(self):
        for package in ["ethereum-ring-swap-v2", "ethereum-uniswap-v4"]:
            self.add_package(package)
        mapping = self.write_file(MAPPING, "{}")
        lock = self.write_file(
            "protocols/substreams/Cargo.lock",
            '[[package]]\nname = "ethereum-ring-swap-v2"\nversion = "0.1.0"\n',
        )
        base = self.commit_base()
        self.git("checkout", "-qb", "chore/base-advanced")
        self.write_file("Cargo.lock")
        advanced = self.commit("unrelated base update")
        self.git("checkout", "-qb", "feat/add-fork", base)
        mapping.write_text(json.dumps({"bsc-ring-swap-v2": "ethereum-ring-swap-v2"}))
        lock.write_text(lock.read_text().replace("0.1.0", "0.1.1"))
        self.commit("add fork")
        protocols, _ = self.selection_output("--base", advanced)
        self.assertEqual(protocols, "bsc-ring-swap-v2")

    def test_cross_package_rename_selects_source_and_destination_families(self):
        source = self.write_file(
            "protocols/substreams/ethereum-curve/src/shared.rs", "pub fn shared() {}\n"
        )
        destination_package = self.add_package("ethereum-ring-swap-v2")
        (self.root / MAPPING).write_text(
            json.dumps(
                {
                    "bsc-curve": "ethereum-curve",
                    "bsc-ring-swap-v2": "ethereum-ring-swap-v2",
                }
            )
        )
        base = self.commit_base()
        destination = destination_package / "src/shared.rs"
        destination.parent.mkdir()
        source.rename(destination)
        self.commit("move source between packages")
        # Ensure this exercises a Git rename, rather than unrelated add/delete records.
        self.assertTrue(self.git("diff", "--name-status", "-M", base, "HEAD").startswith("R100"))
        _, matrix = self.selection_output("--base", base)
        self.assertEqual(
            [entry["protocol"] for entry in matrix["include"]],
            ["bsc-curve", "bsc-ring-swap-v2", "ethereum-curve", "ethereum-ring-swap-v2"],
        )

    def test_rename_to_markdown_still_selects_the_original_package(self):
        source = self.write_file(
            "protocols/substreams/ethereum-curve/src/shared.rs", "pub fn shared() {}\n"
        )
        base = self.commit_base()
        source.rename(source.parent.parent / "archived.md")
        self.commit("archive source as markdown")
        result = self.run_selection("--base", base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "Target protocols: bsc-curve ethereum-curve")

    def test_lock_only_dependency_edge_change_selects_all_protocols(self):
        before = (
            'version = 3\n[[package]]\nname = "ethereum-curve"\nversion = "0.1.0"\n'
            'dependencies = ["shared-dependency 1.0.0"]\n'
            '[[package]]\nname = "shared-dependency"\nversion = "1.0.0"\n'
            'source = "registry+example"\n'
            '[[package]]\nname = "shared-dependency"\nversion = "2.0.0"\n'
            'source = "registry+example"\n'
        )
        lock = self.write_file("protocols/substreams/Cargo.lock", before)
        self.add_package("ethereum-ring-swap-v2")
        base = self.commit_base()
        lock.write_text(
            before.replace('["shared-dependency 1.0.0"]', '["shared-dependency 2.0.0"]')
        )
        self.commit("switch an existing locked dependency version")
        self.assertEqual(
            self.git("diff", "--name-only", base, "HEAD"), str(lock.relative_to(self.root))
        )
        result = self.run_selection("--base", base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.strip(),
            "Target protocols: bsc-curve ethereum-curve ethereum-ring-swap-v2",
        )

    def test_candidate_directory_cannot_inject_workflow_outputs(self):
        package = self.root / "protocols/substreams/bad\nmatrix=injected"
        package.mkdir()
        (package / "Cargo.toml").touch()
        output = self.root / "output"
        result = self.run_selection("--all", "--github-output", str(output))
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(output.exists())

    def test_dispatch_preserves_protocol_filters(self):
        _, matrix = self.selection_output(
            "--protocols",
            "ethereum-curve=some_test bsc-curve=other_test",
        )
        self.assertEqual(
            matrix["include"],
            [
                {"protocol": "bsc-curve=other_test"},
                {"protocol": "ethereum-curve=some_test"},
            ],
        )

    def test_dispatch_rejects_unknown_protocol_even_with_filter(self):
        result = self.run_selection("--protocols", "unknown-protocol=some_test")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown protocols: unknown-protocol", result.stderr)

    def test_trusted_selector_never_executes_candidate_python(self):
        marker = self.root / "candidate-code-ran"
        malicious = f"from pathlib import Path; Path({str(marker)!r}).touch()\n"
        scripts = self.root / "protocols/testing/scripts"
        scripts.mkdir()
        for name in ("select_protocols.py", "resolve_package.py", "test_injected.py"):
            (scripts / name).write_text(malicious)
        (self.root / "sitecustomize.py").write_text(malicious)
        env = dict(os.environ, PYTHONPATH=str(self.root))
        result = self.run_selection("--all", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("bsc-curve ethereum-curve", result.stdout)
        self.assertFalse(marker.exists(), "candidate Python ran in the trusted selection process")

    def test_docker_directory_list_deduplicates_aliases_and_strips_filters(self):
        result = subprocess.run(
            [
                sys.executable,
                str(self.resolver),
                "--mapping",
                str(self.root / MAPPING),
                "build-dirs",
                "bsc-curve=some_test ethereum-curve=other_test",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(result.stdout, "ethereum-curve\n")


if __name__ == "__main__":
    unittest.main()
