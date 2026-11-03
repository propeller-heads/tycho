"""Struct-log trace of one eth_call, with block and storage overrides.

usage:
  trace_call.py OUT.json --to ADDR [--from ADDR] [--block N|latest] [--time T] [--number N]
                [--ov ADDR:SLOT:VALUE ...] (--data 0x.. | --sig 'f(address,int256)' ARG...)

--sig must come last: everything after it, negative numbers included, is its argument list.

Needs `cast` (foundry) and ETH_RPC_URL pointing at a node with debug_traceCall. Prints
whether the call reverted and the first bytes of its return data.
--time/--number override block.timestamp/block.number, e.g. to pass an oracle freshness gate.
--number without --time also sets block.timestamp to block N's own timestamp, so the pair stays
consistent (a number override alone keeps the head's timestamp, which some code rejects).
--ov overrides one storage slot (stateDiff, the rest of the account is kept); repeat the flag.
"""

import argparse
import json
import os
import subprocess
import sys


def rpc_url():
    for name in ("ETH_RPC_URL", "MAINNET_RPC_URL"):
        if os.environ.get(name):
            return os.environ[name]
    sys.exit("set ETH_RPC_URL to the chain the contract lives on (foundry convention)")


def block_timestamp(number):
    """Timestamp of block `number` (decimal or 0x hex), from the node."""
    raw = subprocess.check_output(
        ["cast", "block", to_hex_quantity(number), "--field", "timestamp", "-r", rpc_url()]
    )
    return raw.decode().strip()


def to_hex_quantity(value):
    return value if str(value).startswith("0x") else hex(int(value))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("out")
    p.add_argument("--to", required=True)
    p.add_argument("--from", dest="sender", default="0x0000000000000000000000000000000000000001")
    p.add_argument("--sig", nargs=argparse.REMAINDER, help="signature then arguments; must be last")
    p.add_argument("--data")
    p.add_argument("--block", default="latest")
    p.add_argument("--time")
    p.add_argument("--number")
    p.add_argument("--ov", action="append", default=[], help="ADDR:SLOT:VALUE, repeat the flag")
    a = p.parse_args()
    if bool(a.sig) == bool(a.data):
        sys.exit("pass exactly one of --sig (with args) or --data")
    data = a.data or subprocess.check_output(["cast", "calldata", *a.sig]).decode().strip()

    state = {}
    for ov in a.ov:
        addr, slot, value = ov.split(":")
        diff = state.setdefault(addr, {"stateDiff": {}})["stateDiff"]
        diff[f"0x{int(slot, 16):064x}"] = f"0x{int(value, 16) % (1 << 256):064x}"
    block_overrides = {}
    if a.number and not a.time:
        a.time = block_timestamp(a.number)
        print(f"--number {a.number} without --time: block.timestamp set to {a.time}")
    if a.time:
        block_overrides["time"] = to_hex_quantity(a.time)
    if a.number:
        block_overrides["number"] = to_hex_quantity(a.number)
    config = {"enableMemory": True}
    if block_overrides:
        config["blockOverrides"] = block_overrides
    if state:
        config["stateOverrides"] = state

    block = a.block if a.block == "latest" else to_hex_quantity(a.block)
    call = {"from": a.sender, "to": a.to, "data": data}
    raw = subprocess.check_output(
        [
            "cast",
            "rpc",
            "debug_traceCall",
            json.dumps(call),
            block,
            json.dumps(config),
            "-r",
            rpc_url(),
        ]
    )
    with open(a.out, "wb") as f:
        f.write(raw)
    res = json.loads(raw)
    res = res.get("result", res)
    steps = len(res.get("structLogs", []))
    print(f"failed={res.get('failed')} steps={steps} return={res.get('returnValue', '')[:138]}")


if __name__ == "__main__":
    main()
