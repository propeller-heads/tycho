#!/usr/bin/env python3
"""Resolve fork aliases to runtime package paths or their build workspaces."""

import argparse
import json
from pathlib import Path
import re

MAPPING_PATH = Path(__file__).resolve().parents[1] / "protocol_packages.json"


def load_mapping(path=MAPPING_PATH):
    mapping = json.loads(Path(path).read_text())
    if not isinstance(mapping, dict):
        raise ValueError("Protocol package mapping must be a JSON object")
    for name, package in mapping.items():
        if not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", name):
            raise ValueError(f"Invalid protocol name: {name!r}")
        if not isinstance(package, str) or not re.fullmatch(
            r"[a-z0-9]+(?:[-/][a-z0-9]+)*", package
        ):
            raise ValueError(f"Invalid package directory for {name}: {package!r}")
    return mapping


def build_dir(protocol, mapping):
    # Nested manifests share their parent's Cargo workspace and target directory.
    return mapping.get(protocol, protocol).split("/", 1)[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mapping", type=Path, default=MAPPING_PATH)
    parser.add_argument("mode", choices=["build-dir", "runtime-dir"])
    parser.add_argument("protocol")
    args = parser.parse_args()
    mapping = load_mapping(args.mapping)
    if args.mode == "build-dir":
        print(build_dir(args.protocol, mapping))
    else:
        print(mapping.get(args.protocol, args.protocol))


if __name__ == "__main__":
    main()
