"""Regression tests for fork resolution and CI test selection."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

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
            "Cargo.lock",
            ".github/workflows/ci-substreams-integration.yaml",
        ]:
            with self.subTest(path=path):
                self.assertEqual(self.select([path]), sorted(self.available))

    def test_unrelated_changes_select_nothing(self):
        self.assertEqual(self.select(["protocols/testing/README.md", "docs/example.md"]), [])

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
        after = before.replace('0.1.0', '0.1.1')
        self.assertFalse(dependency_lock_changed(before, after))

    def test_external_dependency_change_triggers_all_protocols(self):
        before = '\n[[package]]\nname = "serde"\nversion = "1.0.0"\nsource = "registry+example"\n'
        self.assertTrue(dependency_lock_changed(before, before.replace('1.0.0', '1.0.1')))

    def test_pr_selection_uses_merge_base_not_new_base_branch_changes(self):
        script = Path(__file__).with_name("select_protocols.py")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.check_output(["git", "-C", directory, *args], text=True).strip()

            git("init", "-q")
            git("config", "user.email", "test@example.com")
            git("config", "user.name", "Test")
            for package in ["ethereum-ring-swap-v2", "ethereum-uniswap-v4"]:
                path = root / "protocols/substreams" / package
                path.mkdir(parents=True)
                (path / "Cargo.toml").touch()
            mapping_path = root / MAPPING
            mapping_path.parent.mkdir(parents=True)
            mapping_path.write_text("{}")
            lock_path = root / "protocols/substreams/Cargo.lock"
            lock_path.write_text('[[package]]\nname = "ethereum-ring-swap-v2"\nversion = "0.1.0"\n')
            git("add", ".")
            git("commit", "-qm", "base")
            base = git("rev-parse", "HEAD")
            git("checkout", "-qb", "base-advanced")
            (root / "Cargo.lock").touch()
            git("add", ".")
            git("commit", "-qm", "unrelated base update")
            advanced = git("rev-parse", "HEAD")
            git("checkout", "-qb", "feature", base)
            mapping_path.write_text(json.dumps({"bsc-ring-swap-v2": "ethereum-ring-swap-v2"}))
            lock_path.write_text(lock_path.read_text().replace("0.1.0", "0.1.1"))
            git("add", ".")
            git("commit", "-qm", "add fork")
            output = root / "output"
            subprocess.run(
                [sys.executable, str(script), "--root", directory, "--base", advanced,
                 "--github-output", str(output)], check=True, capture_output=True,
            )
            self.assertEqual(output.read_text().splitlines()[0], "protocols=bsc-ring-swap-v2")


if __name__ == "__main__":
    unittest.main()
