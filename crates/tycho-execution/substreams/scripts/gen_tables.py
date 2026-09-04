#!/usr/bin/env python3
"""Regenerate the lookup tables compiled into tycho-router-trades.

- src/executors_table.rs: executor address -> protocol names, from the full git history of
  docs/for-solvers/execution/contract-addresses.md and config/executor_addresses.json.
- src/decode/error_table.rs: custom error signatures from the ABIs under abi/.

Run from the repository root: python3 crates/tycho-execution/substreams/scripts/gen_tables.py
"""
from __future__ import annotations

import collections
import json
import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[4]
PKG = ROOT / "crates/tycho-execution/substreams/tycho-router-trades"
DOCS = "docs/for-solvers/execution/contract-addresses.md"
CONFIG = "crates/tycho-execution/config/executor_addresses.json"


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def snake(name: str) -> str:
    name = re.sub(r"Executor$", "", name)
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def executor_rows() -> dict[str, set[str]]:
    by_addr: dict[str, set[str]] = collections.defaultdict(set)
    for commit in git("log", "--format=%h", "--", DOCS).split():
        text = git("show", f"{commit}:{DOCS}")
        for name, addr in re.findall(
            r">([A-Za-z0-9_]+Executor)</a></td><td><a href=\"[^\"]*(0x[0-9a-fA-F]{40})", text
        ):
            by_addr[addr.lower()].add(snake(name))
    # Executor addresses are unique per deployment, so the chain section is irrelevant here.
    for line in git("log", "-p", "--format=", "--", CONFIG).splitlines():
        if m := re.match(r"^[+\- ]\s*\"([a-z_:0-9]+)\": \"(0x[0-9a-fA-F]{40})\"", line):
            by_addr[m.group(2).lower()].add(m.group(1))
    by_addr.pop("0x" + "0" * 40, None)
    for addr, protocols in by_addr.items():
        normalised = {"native_wrapper" if p in ("native_wrap", "wrap") else p for p in protocols}
        prefixed = {p.split(":", 1)[1] for p in normalised if ":" in p}
        by_addr[addr] = {p for p in normalised if ":" in p or p not in prefixed}
    return by_addr


def write_executors(rows: dict[str, set[str]]) -> None:
    lines = [
        "// Generated from the executor tables in docs/for-solvers/execution/contract-addresses.md and",
        "// crates/tycho-execution/config/executor_addresses.json (full git history). Regenerate with",
        "// scripts/gen_tables.py; do not edit by hand.",
        "",
        "/// Executor address (lowercase hex) and the protocol names it serves.",
        "pub(crate) const EXECUTORS: &[(&str, &[&str])] = &[",
    ]
    for addr in sorted(rows):
        protocols = ", ".join(f'"{p}"' for p in sorted(rows[addr]))
        lines.append(f'    ("{addr}", &[{protocols}]),')
    lines.append("];")
    (PKG / "src/executors_table.rs").write_text("\n".join(lines) + "\n")


def write_errors() -> None:
    errors: dict[str, list[str]] = {"Error": ["string"], "Panic": ["uint256"]}
    for abi_file in sorted((PKG / "abi").glob("*.json")):
        for entry in json.loads(abi_file.read_text()):
            if entry["type"] == "error":
                errors[entry["name"]] = [i["type"] for i in entry["inputs"]]
    lines = [
        "// Generated from the `error` entries of the ABIs under abi/ plus the Solidity built-in",
        "// `Error(string)` and `Panic(uint256)`. Regenerate with scripts/gen_tables.py.",
        "",
        "/// Custom error name and its parameter types, used to resolve revert selectors.",
        "pub(crate) const ERRORS: &[(&str, &[&str])] = &[",
    ]
    for name in sorted(errors):
        params = ", ".join(f'"{t}"' for t in errors[name])
        lines.append(f'    ("{name}", &[{params}]),')
    lines.append("];")
    (PKG / "src/decode/error_table.rs").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    write_executors(executor_rows())
    write_errors()
