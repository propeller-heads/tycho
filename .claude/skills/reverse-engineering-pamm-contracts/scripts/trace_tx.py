"""Struct-log trace of a mined transaction, in the format storage_access.py and symreplay.py read.

usage: trace_tx.py OUT.json TX_HASH

The trace covers the whole transaction, so the contract of interest is usually not at depth 1:
pass --depth N to storage_access.py. Needs ETH_RPC_URL with debug_traceTransaction.
"""

import json
import os
import subprocess
import sys


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    out, tx_hash = sys.argv[1], sys.argv[2]
    url = os.environ.get("ETH_RPC_URL") or os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("set ETH_RPC_URL to the chain the transaction is on (foundry convention)")
    raw = subprocess.check_output(
        [
            "cast",
            "rpc",
            "debug_traceTransaction",
            tx_hash,
            json.dumps({"enableMemory": True}),
            "-r",
            url,
        ]
    )
    with open(out, "wb") as f:
        f.write(raw)
    res = json.loads(raw)
    res = res.get("result", res)
    print(f"failed={res.get('failed')} steps={len(res.get('structLogs', []))}")


if __name__ == "__main__":
    main()
