#!/usr/bin/env python3
"""Check that executors.sql names every executor the router config deploys.

A hop through an executor with no row in `executors` resolves to an empty protocol list, so the
dashboards drop it from every per-protocol panel without an error. The router config
(config/executor_addresses.json) is where a new executor is first recorded, so every address it
lists for a chain the package indexes must have a row for that chain in executors.sql. Names are
not compared: a row already in the database keeps its names whatever the file says.

Run from anywhere: python3 crates/tycho-execution/substreams/scripts/check_executors.py
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

SUBSTREAMS = pathlib.Path(__file__).resolve().parent.parent
CONFIG = SUBSTREAMS.parent / "config/executor_addresses.json"
CHAINS = SUBSTREAMS / "tycho-router-trades/chains"
ROW = re.compile(r"^    \('(\w+)', '(0x[0-9a-f]{40})', ARRAY\[[^\]]*\]\),?$", re.M)


def main() -> int:
    config = json.loads(CONFIG.read_text())
    rows = set(ROW.findall((SUBSTREAMS / "executors.sql").read_text()))
    indexed = {path.stem for path in CHAINS.glob("*.yaml")}
    missing = sorted(
        (chain, address.lower(), name)
        for chain in sorted(indexed & config.keys())
        for name, address in config[chain].items()
        if (chain, address.lower()) not in rows
    )
    for chain, address, name in missing:
        print(f"executors.sql has no row for {chain} {address} ({name} in {CONFIG.name})")
    if missing:
        print(f"{len(missing)} executor(s) missing; add them to executors.sql", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
