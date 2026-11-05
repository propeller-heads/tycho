#!/usr/bin/env python3
"""Check that every chain manifest filters on the addresses its params configure.

`map_trades` selects on the routers in its params, and its block filter has to name the same
addresses. `map_vault_transfers` reads logs from the same routers, so it carries the same params
and the same filter. `map_fee_config_events` decodes from any emitter, so it filters on
`fee_cfg`, a key the index sets from the same decoders. A filter that is too narrow drops rows
without an error, so this runs in CI.

Run from anywhere: python3 crates/tycho-execution/substreams/scripts/check_manifests.py
"""
from __future__ import annotations

import pathlib
import re
import sys
import urllib.parse

CHAINS = pathlib.Path(__file__).resolve().parent.parent / "tycho-router-trades/chains"


def configured(params: str) -> tuple[set[str], set[str]]:
    query = urllib.parse.parse_qs(params)
    routers = {e.split(":")[0] for e in query.get("routers", [""])[0].split(",") if e}
    calculators = {e.split(":")[1] for e in query.get("fee_calculators", [""])[0].split(",") if e}
    return routers, calculators


def query(manifest: str, module: str) -> str:
    block = re.search(
        rf"^  - name: {module}\n(?:.*\n)*?    blockFilter:\n(?:.*\n)*?        string: \"(.*)\"$",
        manifest,
        re.M,
    )
    return "" if block is None else block.group(1)


def filtered(manifest: str, module: str) -> set[str]:
    return set(re.findall(r"addr:(0x[0-9a-f]{40})", query(manifest, module)))


def main() -> int:
    failures = []
    for path in sorted(CHAINS.glob("*.yaml")):
        manifest = path.read_text()
        params = re.search(r"^  map_trades: (.+)$", manifest, re.M).group(1)
        routers, _ = configured(params)
        actual = filtered(manifest, "map_trades")
        if actual != routers:
            failures.append(
                f"{path.name} map_trades: filter has {sorted(actual)}, params say {sorted(routers)}"
            )
        vault_params = re.search(r"^  map_vault_transfers: (.+)$", manifest, re.M)
        if vault_params is None:
            failures.append(f"{path.name}: no map_vault_transfers params")
        elif vault_params.group(1) != params:
            failures.append(f"{path.name} map_vault_transfers: params differ from map_trades")
        vault_filter = filtered(manifest, "map_vault_transfers")
        if vault_filter != routers:
            failures.append(
                f"{path.name} map_vault_transfers: filter has {sorted(vault_filter)}, "
                f"params say {sorted(routers)}"
            )
        fee_query = query(manifest, "map_fee_config_events")
        if fee_query != "fee_cfg":
            failures.append(
                f"{path.name} map_fee_config_events: filter is {fee_query!r}, expected 'fee_cfg'"
            )
    for failure in failures:
        print(failure, file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
