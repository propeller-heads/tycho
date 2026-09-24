#!/usr/bin/env python3
"""Compare balances emitted by `substreams run -o jsonl` with the closing treasury.

Usage: RPC_BASE=... python3 scripts/verify_balances.py replay.jsonl
Only token/component balances present in the supplied output are checked. To
cover every component, include discovery or treasury-rotation output.
"""

import json
import os
import sys
import urllib.request

SWAP = "0x55555522005bcae1c2424d474bfd5ed477749e3e"


def rpc(method, params):
    request = urllib.request.Request(
        os.environ["RPC_BASE"],
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(f"{method} failed: {result['error'].get('message', 'unknown error')}")
    return result["result"]


def verify(path):
    balances = {}
    closing_block = None
    with open(path) as source:
        for line in source:
            row = json.loads(line)
            if row.get("@module") != "map_protocol_changes":
                continue
            closing_block = int(row["@block"])
            for transaction in row.get("@data", {}).get("changes", []):
                for change in transaction.get("balanceChanges", []):
                    component = bytes.fromhex(change["componentId"][2:]).decode()
                    value = change.get("balance", "0x")
                    balances[(change["token"], component)] = int(value[2:] or "0", 16)
    if closing_block is None or not balances:
        raise RuntimeError("No emitted component balances to verify")
    block = hex(closing_block)
    owner = "0x" + rpc("eth_getStorageAt", [SWAP, "0x1", block])[-40:]
    expected = {}
    for token, _ in balances:
        if token not in expected:
            calldata = "0x70a08231" + owner[2:].zfill(64)
            expected[token] = int(rpc("eth_call", [{"to": token, "data": calldata}, block]), 16)
    failures = []
    for (token, component), actual in sorted(balances.items()):
        if actual != expected[token]:
            failures.append((component, token, actual, expected[token]))
    print(f"Block {closing_block}, treasury {owner}: {len(balances)} observed component balances")
    for token, value in sorted(expected.items()):
        print(f"  {token}: {value}")
    if failures:
        raise RuntimeError(f"Balance mismatches (component, token, indexed, RPC): {failures}")
    print("All observed balances match exactly")


if __name__ == "__main__":
    verify(sys.argv[1])
