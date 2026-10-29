#!/usr/bin/env python3
"""Record a BaiBai fixture for the simulator's parity tests.

Everything is read at one Base block through the same getters the feed polls: each listed
pair's `pair`, `validUntil` and knots, the CurveBook's `ttl` and `cUnit`, the custodian's
`availableLiquidity`, and the entrypoint's effective fee for the taker. Each case records
`Entrypoint.quoteFor(base, tokenIn, amountIn, taker)` (net output and fee) and the raw
`CurveBook.quote`, so the simulator can be checked wei for wei against both. A failed quote is
recorded as zero output, which is what both functions return for it.

Usage:
    BASE_RPC_URL=... ./record_fixture.py [block] > live_<block>.json

Requires `cast` (Foundry) on PATH. The RPC URL reaches `cast` through its environment.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time

ENTRYPOINT = "0x98c1D9E102Eb2806D902b13186BDc7892aC4fFBa"
CURVE_BOOK = "0x604d9b9eB1e1571C78661a6C1088427EC9c8c6E5"
CUSTODIAN = "0xAaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3"
QUOTE = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
# tycho-execution's Base TychoRouter: the caller whose fee `quoteFor` charges.
TAKER = "0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67"
# Unverified implementation; identified by its storage reads matching quoteFor's fee lookup.
EFFECTIVE_FEE_SELECTOR = "0xa5cbd1b7"
BASES = [
    ("0x4200000000000000000000000000000000000006", 18),
    ("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf", 8),
]

ENV = {**os.environ, "ETH_RPC_URL": os.environ.get("BASE_RPC_URL", "https://mainnet.base.org")}


def cast(*args: str) -> str:
    for attempt in range(8):
        result = subprocess.run(["cast", *args], env=ENV, capture_output=True, text=True)
        if result.returncode == 0:
            return result.stdout.strip()
        time.sleep(1.5 * (attempt + 1))
    raise RuntimeError(result.stderr.strip())


def call(block: int, *args: str) -> list[int]:
    out = cast("call", "--block", str(block), *args)
    return [int(line.split()[0], 0) for line in out.splitlines()]


def pair_view(block: int, base: str) -> dict:
    raw = cast("call", "--block", str(block), CURVE_BOOK, "pair(address)", base)[2:]
    words = [int(raw[64 * i : 64 * (i + 1)], 16) for i in range(len(raw) // 64)]

    def side(w: list[int], index: int) -> dict:
        spread = w[0] - (1 << 256) if w[0] >= 1 << 255 else w[0]
        signature = "knot(address,uint8,uint256)(uint48,uint48)"
        knots = [
            call(block, CURVE_BOOK, signature, base, str(index), str(k)) for k in range(w[3])
        ]
        return {"spread_bps": spread, "depth_bps": w[1], "filled": str(w[2]), "knots": knots}

    return {
        "seq": words[0],
        "fill_seq": words[1],
        "last_update_at": words[2],
        "mid": str(words[3]),
        "q_unit": str(words[4]),
        "ask": side(words[5:9], 0),
        "bid": side(words[9:13], 1),
    }


def amounts(decimals: int, sell_base: bool) -> list[int]:
    if sell_base:
        return [1, 10 ** max(decimals - 6, 0), 10 ** (decimals - 4), 10 ** (decimals - 2),
                10 ** (decimals - 1), 3 * 10 ** (decimals - 1), 10**decimals, 2 * 10**decimals,
                5 * 10**decimals, 10 ** (decimals + 2)]
    return [1, 10, 10**3, 10**5, 10**6, 10**8, 10**9, 3 * 10**9, 10**10, 2 * 10**10, 5 * 10**10,
            10**12]


def main() -> None:
    block = int(sys.argv[1]) if len(sys.argv) > 1 else int(cast("block-number")) - 5
    timestamp = int(cast("block", str(block), "-f", "timestamp"))
    fixture = {
        "block": block,
        "timestamp": timestamp,
        "taker": TAKER,
        "quote": QUOTE,
        "ttl": call(block, CURVE_BOOK, "ttl()(uint64)")[0],
        "c_unit": str(call(block, CURVE_BOOK, "cUnit()(uint256)")[0]),
        "available_quote": str(
            call(block, CUSTODIAN, "availableLiquidity(address)(uint256)", QUOTE)[0]
        ),
        "pairs": [],
    }
    for base, decimals in BASES:
        fee_input = cast("abi-encode", "f(address,address)", TAKER, base)[2:]
        fee_call = EFFECTIVE_FEE_SELECTOR + fee_input
        fee = int(cast("call", "--block", str(block), ENTRYPOINT, fee_call), 16)
        cases = []
        for sell_base in (True, False):
            token_in = base if sell_base else QUOTE
            for amount in amounts(decimals, sell_base):
                out, charged = call(
                    block, ENTRYPOINT, "quoteFor(address,address,uint256,address)(uint256,uint256)",
                    base, token_in, str(amount), TAKER,
                )
                curve = call(
                    block, CURVE_BOOK, "quote(address,address,uint256)(uint256)",
                    base, token_in, str(amount),
                )[0]
                cases.append({
                    "sell_base": sell_base, "amount_in": str(amount), "amount_out": str(out),
                    "fee": str(charged), "curve_out": str(curve),
                })
        fixture["pairs"].append({
            "base": base,
            "decimals": decimals,
            "valid_until": call(block, CURVE_BOOK, "validUntil(address)(uint64)", base)[0],
            "available_base": str(
                call(block, CUSTODIAN, "availableLiquidity(address)(uint256)", base)[0]
            ),
            "fee_pips": fee,
            "pair": pair_view(block, base),
            "cases": cases,
        })
    json.dump(fixture, sys.stdout, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
