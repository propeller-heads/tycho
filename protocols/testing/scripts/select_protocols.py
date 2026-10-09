#!/usr/bin/env python3
"""Select integration tests from package ownership and a PR's merge-base diff."""

import argparse
import json
import re
import subprocess
from pathlib import Path

import tomllib
from resolve_package import build_dir, load_mapping

MAPPING = "protocols/testing/protocol_packages.json"
EXCLUDED = {"ethereum-template-factory", "ethereum-template-singleton", "ethereum-fluid"}


def all_protocols(root, mapping):
    packages = {
        path.parent.name
        for path in (root / "protocols/substreams").glob("*/Cargo.toml")
        if path.parent.name not in EXCLUDED
    }
    for name in packages:
        if not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", name):
            raise ValueError(f"Invalid protocol package directory: {name!r}")
    return packages | set(mapping)


def affected_protocols(paths, mapping, previous_mapping, available):
    selected = set()
    for path in paths:
        if path.endswith(".md"):
            continue
        if path == MAPPING:
            selected.update(
                name for name in mapping if mapping.get(name) != previous_mapping.get(name)
            )
            # Removed aliases must not become jobs pointing at nonexistent packages. Exercise
            # their former workspace and remaining siblings instead.
            removed_bases = {
                build_dir(name, previous_mapping)
                for name in previous_mapping.keys() - mapping.keys()
            }
            selected.update(name for name in available if build_dir(name, mapping) in removed_bases)
        elif (
            path.startswith("protocols/testing/")
            or path.startswith(".github/actions/substreams-docker-single/")
            or path == ".github/workflows/ci-substreams-integration.yaml"
        ):
            return sorted(available)
        elif path.startswith("protocols/substreams/"):
            relative = path.removeprefix("protocols/substreams/")
            package = relative.split("/", 1)[0]
            if package == "crates" or "/" not in relative:
                return sorted(available)
            # A fork's manifest and fixture belong only to that alias. Common source, ABIs,
            # Cargo metadata and other files still affect every consumer of the workspace.
            owners = set()
            for aliases in (previous_mapping, mapping):
                for name, directory in aliases.items():
                    owned_files = {
                        f"{directory}/{name}.yaml",
                        f"{directory}/integration_test_{name.replace('-', '_')}.tycho.yaml",
                    }
                    if relative in owned_files:
                        owners.add(name)
            if owners:
                selected.update(owners & available)
            else:
                selected.update(name for name in available if build_dir(name, mapping) == package)
    return sorted(selected & available)


def dependency_lock_changed(before, after):
    """Local package version bumps do not change the shared dependency environment."""

    def dependencies(contents):
        lock = tomllib.loads(contents)
        return sorted(
            (package for package in lock.get("package", []) if "source" in package),
            key=lambda package: (package["name"], package["version"], package["source"]),
        )

    return dependencies(before) != dependencies(after)


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args], text=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[3])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--base", help="PR base SHA; changes are compared from the merge base")
    mode.add_argument("--all", action="store_true")
    mode.add_argument("--protocols", help="Space-separated protocols for manual dispatch")
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    mapping = load_mapping(args.root / MAPPING)
    available = all_protocols(args.root, mapping)
    if args.all:
        protocols = sorted(available)
    elif args.protocols is not None:
        protocols = sorted(set(args.protocols.split()))
        unknown = {protocol.split("=", 1)[0] for protocol in protocols} - available
        if unknown:
            parser.error(f"Unknown protocols: {', '.join(sorted(unknown))}")
    else:
        base = git(args.root, "merge-base", args.base, "HEAD").strip()
        paths = git(args.root, "diff", "--name-only", "-z", base, "HEAD").split("\0")
        lock = "protocols/substreams/Cargo.lock"
        if lock in paths and git(args.root, "ls-tree", "--name-only", base, "--", lock).strip():
            current = args.root / lock
            if current.exists() and not dependency_lock_changed(
                git(args.root, "show", f"{base}:{lock}"), current.read_text()
            ):
                paths.remove(lock)
        previous = {}
        if git(args.root, "ls-tree", "--name-only", base, "--", MAPPING).strip():
            previous = json.loads(git(args.root, "show", f"{base}:{MAPPING}"))
        protocols = affected_protocols(paths, mapping, previous, available)
    matrix = {"include": [{"protocol": name} for name in protocols]}
    print("Target protocols: " + " ".join(protocols))
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write("protocols=" + " ".join(protocols) + "\n")
            output.write("matrix=" + json.dumps(matrix, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
