#!/usr/bin/env python3
"""Regenerate tycho-router-trades/src/decode/error_table.rs.

Custom error signatures come from the `error` entries of the ABIs under
tycho-router-trades/abi/, and resolve the revert selector a failed router call returns.

The executor table is not generated. It is `executors.sql`, kept by hand.

Run from the repository root:
python3 crates/tycho-execution/substreams/scripts/gen_error_table.py
"""
from __future__ import annotations

import json
import pathlib

PKG = pathlib.Path(__file__).resolve().parents[1] / "tycho-router-trades"


def write_errors() -> None:
    errors: dict[str, list[str]] = {"Error": ["string"], "Panic": ["uint256"]}
    for abi_file in sorted((PKG / "abi").glob("*.json")):
        for entry in json.loads(abi_file.read_text()):
            if entry["type"] == "error":
                errors[entry["name"]] = [i["type"] for i in entry["inputs"]]
    lines = [
        "// Generated from the `error` entries of the ABIs under abi/ plus the Solidity built-in",
        "// `Error(string)` and `Panic(uint256)`. Regenerate with scripts/gen_error_table.py.",
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
    write_errors()
