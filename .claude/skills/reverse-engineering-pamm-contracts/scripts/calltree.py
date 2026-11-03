"""Call tree of a mined transaction or of one eth_call: every contract on the path at once.

usage:
  calltree.py TX_HASH
  calltree.py --to ADDR [--from ADDR] [--block N|latest] [--time T] [--number N]
              [--ov ADDR:SLOT:VALUE ...] (--data 0x.. | --sig 'f(address,uint256)' ARG...)

Runs debug_traceTransaction or debug_traceCall with geth's callTracer and prints one line per
call frame, indented by depth: call type, caller -> callee, selector with its openchain name,
ETH value, gas used, the first argument and return words (leading zeros dropped, 4 words at
most), and for a frame that reverted its reason or the head of its revert data. Every address
in the tree of a real swap is on the swap path. Needs ETH_RPC_URL pointing at a node with the
debug API; `cast` for --sig. --sig must come last: everything after it is its argument list.
--time/--number override block.timestamp/block.number; --ov overrides one storage slot (repeat
the flag).
"""

import argparse
import json
import os
import subprocess
import sys
import urllib.request

WORDS_SHOWN = 4


def rpc(method, params):
    url = os.environ.get("ETH_RPC_URL") or os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("set ETH_RPC_URL to a node with the debug API (foundry convention)")
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    reply = json.load(urllib.request.urlopen(req, timeout=300))
    if "error" in reply:
        sys.exit(f"{method}: {reply['error']}")
    return reply["result"]


def selector_names(selectors):
    """{selector: name} from openchain; empty when the lookup fails (names are advisory)."""
    url = "https://api.openchain.xyz/signature-database/v1/lookup?function=" + ",".join(selectors)
    try:
        found = json.load(urllib.request.urlopen(url, timeout=20))["result"]["function"]
    except Exception:  # noqa: BLE001 - names are a convenience, the tree is printed without them
        return {}
    return {s: entries[0]["name"].split("(")[0] for s, entries in found.items() if entries}


def short_words(hex_data):
    """`0x..` data as up to WORDS_SHOWN 32-byte words without leading zeros."""
    body = hex_data.removeprefix("0x")
    words = [body[i : i + 64].lstrip("0") or "0" for i in range(0, len(body), 64)]
    text = ",".join(words[:WORDS_SHOWN])
    return text + (f",..({len(words)} words)" if len(words) > WORDS_SHOWN else "")


def frame_line(frame, names):
    data = frame.get("input", "0x")
    selector = data[:10] if len(data) >= 10 else "-"
    name = names.get(selector, "")
    value = int(frame.get("value") or "0x0", 16)
    parts = [
        frame["type"].lower(),
        f"{frame['from']} -> {frame.get('to', '?')}",
        selector + (f" {name}" if name else ""),
        f"value={value / 1e18:g}ETH" if value else "",
        f"gas={int(frame.get('gasUsed', '0x0'), 16)}",
        f"args=[{short_words(data[10:])}]" if len(data) > 10 else "",
    ]
    output = frame.get("output") or ""
    if frame.get("error"):
        reason = frame.get("revertReason") or output[:138]
        parts.append(f"REVERT {frame['error']}" + (f": {reason}" if reason else ""))
    elif output not in ("", "0x"):
        parts.append(f"out=[{short_words(output)}]")
    return " ".join(p for p in parts if p)


def walk(frame, depth=0):
    yield depth, frame
    for child in frame.get("calls", []):
        yield from walk(child, depth + 1)


def trace_call_params(args):
    if bool(args.sig) == bool(args.data):
        sys.exit("pass exactly one of --sig (with args) or --data")
    data = args.data or subprocess.check_output(["cast", "calldata", *args.sig]).decode().strip()
    config = {"tracer": "callTracer"}
    state = {}
    for override in args.ov:
        address, slot, value = override.split(":")
        diff = state.setdefault(address, {"stateDiff": {}})["stateDiff"]
        diff[f"0x{int(slot, 16):064x}"] = f"0x{int(value, 16) % (1 << 256):064x}"
    if state:
        config["stateOverrides"] = state
    block = {k: hex(int(v, 0)) for k, v in (("time", args.time), ("number", args.number)) if v}
    if block:
        config["blockOverrides"] = block
    tag = args.block if args.block == "latest" else hex(int(args.block, 0))
    call = {"from": args.sender, "to": args.to, "data": data}
    return [call, tag, config]


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("tx", nargs="?", help="transaction hash")
    p.add_argument("--to")
    p.add_argument("--from", dest="sender", default="0x0000000000000000000000000000000000000001")
    p.add_argument("--data")
    p.add_argument("--block", default="latest")
    p.add_argument("--time")
    p.add_argument("--number")
    p.add_argument("--ov", action="append", default=[], help="ADDR:SLOT:VALUE, repeat the flag")
    p.add_argument("--sig", nargs=argparse.REMAINDER, help="signature then arguments; must be last")
    args = p.parse_args()
    if bool(args.tx) == bool(args.to):
        sys.exit("pass a TX_HASH or --to (with --data or --sig), not both")
    if args.tx:
        root = rpc("debug_traceTransaction", [args.tx, {"tracer": "callTracer"}])
    else:
        root = rpc("debug_traceCall", trace_call_params(args))
    frames = list(walk(root))
    selectors = sorted(
        {f.get("input", "")[:10] for _, f in frames if len(f.get("input", "")) >= 10}
    )
    names = selector_names(selectors) if selectors else {}
    for depth, frame in frames:
        print("  " * depth + frame_line(frame, names))


if __name__ == "__main__":
    main()
