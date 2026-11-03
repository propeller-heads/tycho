"""List what one frame of a struct-log trace hashed, read, wrote and called, in execution order.

usage: storage_access.py TRACE_JSON [--depth N]

KECCAK256 lines show the preimage as 32-byte words, which gives mapping layouts directly:
`keccak(key . slot)` is `mapping(slot)[key]`, `keccak(slot)` is the data of a dynamic array at
`slot`, and a later SLOAD at `result + k` is field/element k. Pair every hash with the SLOADs
that follow it.
"""

import argparse
import json


def memory_bytes(step, offset, size):
    words = "".join(w.removeprefix("0x") for w in step.get("memory") or [])
    return words[offset * 2 : (offset + size) * 2]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("trace")
    p.add_argument("--depth", type=int, default=1)
    a = p.parse_args()
    with open(a.trace) as f:
        data = json.load(f)
    res = data.get("result", data)
    logs = res["structLogs"]
    print(f"steps={len(logs)} failed={res.get('failed')}")
    for i, step in enumerate(logs):
        if step["depth"] != a.depth:
            continue
        op, stack = step["op"], step["stack"]
        following = logs[i + 1]["stack"] if i + 1 < len(logs) and logs[i + 1]["stack"] else ["0x0"]
        pc = step["pc"]
        if op in ("KECCAK256", "SHA3"):
            offset, size = int(stack[-1], 16), int(stack[-2], 16)
            pre = memory_bytes(step, offset, size)
            words = " . ".join(pre[j : j + 64] for j in range(0, len(pre), 64))
            print(f"pc={pc:5d} KECCAK {int(following[-1], 16):#066x} <- {words}")
        elif op == "SLOAD":
            print(f"pc={pc:5d} SLOAD  {int(stack[-1], 16):#066x} = {int(following[-1], 16):#066x}")
        elif op == "SSTORE":
            print(f"pc={pc:5d} SSTORE {int(stack[-1], 16):#066x} = {int(stack[-2], 16):#066x}")
        elif op in ("TLOAD", "TSTORE"):
            print(f"pc={pc:5d} {op} {int(stack[-1], 16):#x}")
        elif op in ("CALL", "STATICCALL", "DELEGATECALL", "CALLCODE"):
            print(f"pc={pc:5d} {op} to={int(stack[-2], 16):#042x}")
        elif op in ("TIMESTAMP", "NUMBER", "CALLER", "ORIGIN", "BASEFEE", "GASPRICE"):
            print(f"pc={pc:5d} {op} = {int(following[-1], 16)}")
        elif op in ("RETURN", "REVERT", "STOP"):
            print(f"pc={pc:5d} {op}")


if __name__ == "__main__":
    main()
