#!/usr/bin/env python3
"""Record BaiBai quotes from the deployed contracts for the native simulator's parity tests.

All reads are at one fixed Base block, against the live CurveBook and entrypoint proxies.

- `scenarios`: synthetic curves. Each overrides only the CurveBook's 30 pair words for WETH and
  records `CurveBook.quote` for a spread of sizes in both directions, so the deployed bytecode
  prices shapes, cursors, depths, the 36-knot maximum and expiry that live state may not show.
- `live`: the block's real state, unmodified. It holds all 32 indexed words, the custodian's
  balances and `Entrypoint.quoteFor(base, tokenIn, amountIn, router)` for the Tycho Base router.
  `eth_call` has no priority fee, so `quoteFor` prices the no-surcharge path, the one the
  simulator models.

A quote that reverts is recorded as "0". Integers are decimal strings.

Usage:
    BASE_RPC_URL=... ./fetch_quotes.py <block> > quotes.json

Requires `cast` (Foundry) on PATH and an archive RPC. The URL is passed to `cast` through its
environment, never on a command line.
"""

from __future__ import annotations

import concurrent.futures
import functools
import json
import os
import subprocess
import sys

CURVE_BOOK = "0x604d9b9eB1e1571C78661a6C1088427EC9c8c6E5"
ENTRYPOINT = "0x98c1D9E102Eb2806D902b13186BDc7892aC4fFBa"
CUSTODIAN = "0xAaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3"
# tycho-execution's Base TychoRouter, the simulator's default caller.
ROUTER = "0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67"
BASE = "0x4200000000000000000000000000000000000006"
QUOTE = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"

SELL_INPUTS = [
    1,
    10**10,
    10**16,
    10**17 - 1,
    10**17,
    10**17 + 1,
    2 * 10**17,
    4 * 10**17,
    10**19,
]
BUY_INPUTS = [1, 2, 10, 10**6, 100_000_003, 250_000_000, 500_000_000, 10**9, 10**10]
LIVE_SELL_INPUTS = [10**12, 10**15, 10**16, 10**17, 10**18, 3 * 10**18, 10**21]
LIVE_BUY_INPUTS = [1, 10**4, 10**6, 10**8, 10**9, 5 * 10**9, 10**12]

ENV = {**os.environ, "ETH_RPC_URL": os.environ["BASE_RPC_URL"]}


def cast(*args: str) -> str:
    return subprocess.run(
        ["cast", *args], env=ENV, check=True, capture_output=True, text=True
    ).stdout.strip()


def keccak(data: str) -> int:
    return int(cast("keccak", data), 16)


def word(value: int) -> str:
    return f"0x{value:064x}"


def namespace(name: str) -> int:
    """ERC-7201: keccak256(keccak256(name) - 1) & ~0xff."""
    return keccak(word(keccak(name) - 1)) & ~0xFF


def mapping(key: int, slot: int) -> int:
    return keccak(word(key) + f"{slot:064x}")


@functools.cache
def slots() -> list[tuple[str, int]]:
    """The simulator's word_0..word_31, as in the substreams package's Config::slots."""
    book = namespace("baibai.storage.CurveBook.v3")
    pair = mapping(int(BASE, 16), book + 2)
    out = [(CURVE_BOOK, book)] + [(CURVE_BOOK, pair + i) for i in range(5)]
    for side in (5, 6):
        out += [(CURVE_BOOK, mapping(i, pair + side)) for i in range(12)]
    claims = namespace("baibai.storage.Custodian") + 2
    out += [(CUSTODIAN, mapping(int(token, 16), claims)) for token in (BASE, QUOTE)]
    return out


def pack_knots(knots: list[tuple[int, int]]) -> list[int]:
    """Twelve slots, three 84-bit knots each: q in the high 42 bits, extra in the low 42."""
    out = [0] * 12
    for i, (q, extra) in enumerate(knots):
        out[i // 3] |= ((q << 42) | extra) << (172 - 84 * (i % 3))
    return out


def scenario_words(
    timestamp: int,
    knots: list[tuple[int, int]],
    filled: int = 0,
    depth: int = 10_000,
    expired: bool = False,
) -> list[int]:
    valid = timestamp - 1 if expired else timestamp + 30
    # seq 1, fillSeq 0, lastUpdateAt, validUntil.
    sequencing = 1 | (timestamp << 128) | (valid << 192)
    # mid, ask/bid spread bps (7, -11), depths, knot counts, and an update priority fee.
    # The fee occupies the slot's high 96 bits in v5; quote() ignores it.
    packed = (
        2_500_000_123
        | (7 << 80)
        | ((-11 & 0xFFFF) << 96)
        | (depth << 112)
        | (depth << 128)
        | (len(knots) << 144)
        | (len(knots) << 152)
        | (16_000_000 << 160)
    )
    side = pack_knots(knots)
    return [30, sequencing, packed, 10**10, filled, filled] + side + side + [0, 0]


def linear(n: int) -> list[tuple[int, int]]:
    return [(10**7 * i, 100_003 * i) for i in range(1, n + 1)]


def curved(n: int) -> list[tuple[int, int]]:
    return [(10**7 * i, 100_003 * i * i) for i in range(1, n + 1)]


def call(block: int, *args: str) -> str:
    """The first return value, or "0" when the call reverts. Any other failure aborts."""
    result = subprocess.run(
        ["cast", "call", "--block", str(block), *args], env=ENV, capture_output=True, text=True
    )
    if result.returncode == 0:
        return result.stdout.split()[0]
    if "execution reverted" in result.stderr:
        return "0"
    raise RuntimeError(result.stderr.strip())


def book_quote(block: int, words: list[int], sell_base: bool, amount: int) -> str:
    overrides = ",".join(
        f"{CURVE_BOOK}:{word(slot)}:{word(value)}"
        for (_, slot), value in zip(slots()[:30], words[:30])
    )
    token_in = BASE if sell_base else QUOTE
    return call(
        block,
        "--override-state-diff",
        overrides,
        CURVE_BOOK,
        "quote(address,address,uint256)(uint256)",
        BASE,
        token_in,
        str(amount),
    )


def quote_for(block: int, sell_base: bool, amount: int) -> str:
    token_in = BASE if sell_base else QUOTE
    return call(
        block,
        ENTRYPOINT,
        "quoteFor(address,address,uint256,address)(uint256,uint256)",
        BASE,
        token_in,
        str(amount),
        ROUTER,
    )


def cases(quote, sell: list[int], buy: list[int]) -> list[dict]:
    jobs = [(True, amount) for amount in sell] + [(False, amount) for amount in buy]
    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        outputs = list(pool.map(lambda job: quote(*job), jobs))
    return [
        {"sell_base": sell_base, "input": str(amount), "output": output}
        for (sell_base, amount), output in zip(jobs, outputs)
    ]


def main() -> None:
    block = int(sys.argv[1])
    timestamp = int(cast("block", str(block), "-f", "timestamp"))
    layout = slots()
    definitions = {
        "linear": dict(knots=linear(4)),
        "curved": dict(knots=curved(4)),
        "cursor": dict(knots=curved(4), filled=150_000_000_000_000_123),
        "depth": dict(knots=curved(4), depth=7_500),
        "max_knots": dict(knots=curved(36)),
        "expired": dict(knots=curved(4), expired=True),
    }
    scenarios = []
    for name, definition in definitions.items():
        words = scenario_words(timestamp, **definition)
        scenarios.append(
            {
                "name": name,
                "words": [word(w) for w in words],
                "cases": cases(
                    lambda sell, amount: book_quote(block, words, sell, amount),
                    SELL_INPUTS,
                    BUY_INPUTS,
                ),
            }
        )
    live_words = [
        int(cast("storage", "--block", str(block), address, word(slot)), 16)
        for address, slot in layout
    ]
    balances = [
        call(block, token, "balanceOf(address)(uint256)", CUSTODIAN) for token in (BASE, QUOTE)
    ]
    live = {
        "router": ROUTER,
        "words": [word(w) for w in live_words],
        "balances": balances,
        "cases": cases(
            lambda sell, amount: quote_for(block, sell, amount),
            LIVE_SELL_INPUTS,
            LIVE_BUY_INPUTS,
        ),
    }
    json.dump(
        {
            "block": block,
            "timestamp": timestamp,
            "curve_book": CURVE_BOOK,
            "entrypoint": ENTRYPOINT,
            "base": BASE,
            "quote": QUOTE,
            "slots": [[address, word(slot)] for address, slot in layout],
            "scenarios": scenarios,
            "live": live,
        },
        sys.stdout,
        indent=2,
    )
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
