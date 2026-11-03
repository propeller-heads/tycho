"""Is a pAMM deployment still the one in use? Checks a venue's watch.json against the chain.

usage: liveness.py WATCH_JSON [--block B]

WATCH_JSON lists what the analysis assumed (see SKILL.md, Output). Every entry has a "name".
- "code":       {"address", "hash"}: code still there and unchanged; the hash is keccak256 of
                eth_getCode (the account's codeHash).
- "slots":      {"address", "slot", "expect", "mask"?, "op"?}: pointers (EIP-1967
                implementation, router -> engine), pause or active flags, pricer and vault
                settings. "op" is "eq" (default) or "ne": "ne" with expect 0 watches "cap != 0".
- "calls":      {"to", "data", "expect", "op"?}: eth_call `data` and compare the whole return
                data, read as one integer, with "expect" (so a multi-word or array return is
                written out in full, as returned). "op" as for slots.
- "activity":   {"to", "selector"?, "args"?, "max_age_blocks"}: the last successful call into
                `to` must be at most this old. "selector" is matched as a calldata prefix, so a
                4-byte selector or a shorter prefix such as "0x00" for selector-less pushes.
                "args" maps an ABI word index (0 = first word after the selector) to a value,
                compared as integers: {"0": "0x2086..."} picks `updateState(target, ...)` calls
                for one venue out of a registry shared by several. Price updates stop first when
                a venue moves.
- "operators":  {"addresses", "known", "blocks", "include_internal"?}: calls and contract
                creations by the venue's own accounts (owner, pricers) in the last `blocks`
                blocks; any destination outside `known` is printed, as it is where the flow
                went. Only top-level calls count unless "include_internal" is true, which also
                counts internal calls (not static calls) the addresses make: a Safe owner acts
                through internal calls under `execTransaction`, and its delegatecall to the Safe
                singleton then belongs in `known`.

Prints one line per check: OK, FAIL, or ERROR when an RPC call failed (the remaining checks
still run). Exits 1 when any check fails or errors. Needs trace_filter for "activity" and
"operators".
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections import Counter

CHUNK = 250


class RpcError(Exception):
    """A JSON-RPC call that returned an error object."""


def rpc(method, params):
    url = os.environ.get("ETH_RPC_URL") or os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("set ETH_RPC_URL to the chain the venue lives on (foundry convention)")
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    request = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    for attempt in range(8):
        try:
            reply = json.load(urllib.request.urlopen(request, timeout=120))
        except urllib.error.HTTPError as err:
            if err.code not in (429, 503) or attempt == 7:
                raise
        else:
            # -32005: the node's "limit exceeded / overloaded, retry later"
            if reply.get("error", {}).get("code") != -32005 or attempt == 7:
                break
        time.sleep(0.5 * 2**attempt)
    if "error" in reply:
        raise RpcError(f"{method}: {reply['error']}")
    return reply["result"]


def word(hex_value):
    return int(hex_value or "0x0", 16)


def keccak_hex(hex_data):
    """keccak256 of hex bytes, with difftest.py's stdlib implementation."""
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from difftest import keccak

    return "0x" + keccak(bytes.fromhex(hex_data.removeprefix("0x"))).hex()


def check_code(entry, block):
    code = rpc("eth_getCode", [entry["address"], hex(block)])
    if code in ("0x", ""):
        return False, "no code (self-destructed or wrong chain)"
    code_hash = keccak_hex(code)
    return code_hash == entry["hash"].lower(), f"code hash {code_hash}"


def compare(entry, value):
    """`value` against entry["expect"] with entry["op"] ("eq" or "ne")."""
    op = entry.get("op", "eq")
    if op not in ("eq", "ne"):
        raise ValueError(f"op must be 'eq' or 'ne', not {op!r}")
    return (value == word(entry["expect"])) == (op == "eq")


def check_slot(entry, block):
    value = word(rpc("eth_getStorageAt", [entry["address"], entry["slot"], hex(block)]))
    value &= word(entry.get("mask", hex((1 << 256) - 1)))
    return compare(entry, value), f"value {value:#x}"


def check_call(entry, block):
    value = word(rpc("eth_call", [{"to": entry["to"], "data": entry["data"]}, hex(block)]))
    return compare(entry, value), f"returns {value:#x}"


def matches_calldata(entry, data):
    """The trace's calldata starts with entry["selector"] and has every entry["args"] word."""
    if not data.lower().startswith(entry.get("selector", "").lower()):
        return False
    for index, expected in entry.get("args", {}).items():
        start = 10 + 64 * int(index)
        if word(data[start : start + 64] or "0x0") != word(expected):
            return False
    return True


def check_activity(entry, block):
    oldest = block - entry["max_age_blocks"]
    end = block
    while end > oldest:
        start = max(end - CHUNK + 1, oldest + 1)
        query = {"fromBlock": hex(start), "toBlock": hex(end), "toAddress": [entry["to"]]}
        traces = rpc("trace_filter", [query])
        for trace in reversed(traces):
            if trace.get("error") or trace.get("type") != "call":
                continue
            if not matches_calldata(entry, trace["action"]["input"]):
                continue
            return True, f"last call {block - trace['blockNumber']} blocks ago"
        end = start - 1
    return False, f"no call in the last {entry['max_age_blocks']} blocks"


def check_operators(entry, block):
    known = {address.lower() for address in entry["known"]}
    destinations = Counter()
    internal = entry.get("include_internal", False)
    start_block = block - entry["blocks"] + 1
    for start in range(start_block, block + 1, CHUNK):
        end = min(start + CHUNK - 1, block)
        query = {"fromBlock": hex(start), "toBlock": hex(end), "fromAddress": entry["addresses"]}
        for trace in rpc("trace_filter", [query]):
            if trace.get("error") or (trace["traceAddress"] and not internal):
                continue
            if trace["action"].get("callType") == "staticcall":
                continue
            if trace["type"] == "create":
                destinations["created " + trace["result"]["address"]] += 1
            elif trace["type"] == "call":
                destinations[trace["action"]["to"].lower()] += 1
    unknown = {d: n for d, n in destinations.items() if d not in known}
    if not unknown:
        return True, f"{sum(destinations.values())} txs, all to known contracts"
    listing = ", ".join(f"{d} x{n}" for d, n in Counter(unknown).most_common(5))
    return False, f"new destinations: {listing}"


CHECKS = {
    "code": check_code,
    "slots": check_slot,
    "calls": check_call,
    "activity": check_activity,
    "operators": check_operators,
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("watch")
    parser.add_argument("--block", type=int)
    args = parser.parse_args()
    with open(args.watch) as handle:
        watch = json.load(handle)
    block = args.block or word(rpc("eth_blockNumber", []))
    print(f"chain id {word(rpc('eth_chainId', []))}, block {block}")
    failed = errors = 0
    for kind, check in CHECKS.items():
        for entry in watch.get(kind, []):
            try:
                ok, detail = check(entry, block)
                status = "OK" if ok else "FAIL"
            except (RpcError, OSError) as err:
                ok, status, detail = False, "ERROR", f"RPC failed, check not run: {err}"
                errors += 1
            failed += not ok
            print(f"{status:<5} {kind:<9} {entry['name']}: {detail}")
    print(f"{failed - errors} failed, {errors} errors")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
