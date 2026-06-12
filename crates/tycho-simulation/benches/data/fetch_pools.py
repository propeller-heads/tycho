# /// script
# requires-python = ">=3.10"
# dependencies = ["requests"]
# ///
"""Fetch realistic Uniswap V3 pool snapshots (slot0 + liquidity + a window of
initialized ticks) from a live Ethereum RPC, for use by the CLMM swap benchmark.

The candidate pool universe is `pool_universe.json` (top mainnet V3 pools by 30d
USD volume, from a Dune query). Ticks are read via the canonical TickLens
contract (0xbfd8137f7d1516D3ea5cA83523914859ec47F573), one bitmap word at a time,
over a window of words centred on the current tick. The window starts at roughly
+/-20% in price (ln(1.2)/ln(1.0001) ~= 1823 ticks each side, widened to whole
bitmap words) and is then EXPANDED per-pool until at least one initialized tick
exists on BOTH sides of the current tick, so neither swap direction is a
one-sided truncation artifact. A pool that cannot satisfy this within a hard cap
is dropped.

`valid_limit` (the safe per-direction max swap amount that stays strictly inside
the fetched window) is NOT computed here: it is derived in Rust at load time
from the simulation's own `get_limits`, which already bounds itself to the
loaded ticks. The snapshot is therefore self-contained and deterministic.

Usage:
    RPC_URL=https://... uv run fetch_pools.py [--out-dir DIR] [--block N] [--max-pools N]

Output: one <symbol0>_<symbol1>_<fee>.json per pool plus an index.json listing
all snapshots and the fetch block.
"""

import argparse
import json
import math
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import requests

TICKLENS = "0xbfd8137f7d1516D3ea5cA83523914859ec47F573"

# Function selectors (keccak256 prefix) for the calls we issue.
SEL_SLOT0 = "0x3850c7bd"
SEL_LIQUIDITY = "0x1a686502"
SEL_FEE = "0xddca3f43"
SEL_TICK_SPACING = "0xd0c93a7c"
SEL_TOKEN0 = "0x0dfe1681"
SEL_TOKEN1 = "0xd21220a7"
SEL_DECIMALS = "0x313ce567"
SEL_SYMBOL = "0x95d89b41"
# getPopulatedTicksInWord(address,int16)
SEL_POPULATED_TICKS = "0x351fb478"

# Hard cap on how far the per-pool window may expand while hunting for ticks on
# both sides of the current tick, in bitmap words each side.
MAX_WORDS_EACH_SIDE = 60


def load_universe(out_dir):
    path = os.path.join(out_dir, "pool_universe.json")
    with open(path) as fh:
        return json.load(fh)["pools"]


def hex_call(rpc, to, data, block):
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [{"to": to, "data": data}, block],
    }
    resp = requests.post(rpc, json=payload, timeout=30)
    resp.raise_for_status()
    body = resp.json()
    if "error" in body:
        raise RuntimeError(f"{to} {data[:10]}: {body['error']}")
    return body["result"]


# Some RPC providers reject or rate-limit large JSON-RPC batch arrays as a
# whole (returning a single top-level error). Cap the batch and degrade to
# per-call requests on any anomaly so the fetch stays robust.
MAX_BATCH = 20


def batch_call(rpc, calls, block):
    """calls: list of (to, data). Returns list of raw hex results, same order."""
    out = []
    for start in range(0, len(calls), MAX_BATCH):
        chunk = calls[start : start + MAX_BATCH]
        out.extend(_batch_chunk(rpc, chunk, block))
    return out


def _batch_chunk(rpc, chunk, block):
    payload = [
        {
            "jsonrpc": "2.0",
            "id": i,
            "method": "eth_call",
            "params": [{"to": to, "data": data}, block],
        }
        for i, (to, data) in enumerate(chunk)
    ]
    resp = requests.post(rpc, json=payload, timeout=60)
    resp.raise_for_status()
    body = resp.json()
    if not isinstance(body, list) or len(body) != len(chunk):
        # Provider did not honour the batch; fall back to sequential calls.
        return [hex_call(rpc, to, data, block) for to, data in chunk]
    by_id = {item["id"]: item for item in body}
    out = []
    for i, (to, data) in enumerate(chunk):
        item = by_id.get(i)
        if item is None or "error" in item:
            out.append(hex_call(rpc, to, data, block))
        else:
            out.append(item["result"])
    return out


def decode_int(raw, signed=False, bits=256):
    val = int(raw, 16)
    if signed and val >= (1 << (bits - 1)):
        val -= 1 << bits
    return val


def decode_address(raw):
    return "0x" + raw[-40:]


def decode_string(raw):
    # Symbols are usually ABI-encoded dynamic strings; some legacy tokens
    # return a fixed bytes32. Handle both.
    data = bytes.fromhex(raw[2:])
    if len(data) == 32:
        return data.rstrip(b"\x00").decode("utf-8", "replace")
    offset = int.from_bytes(data[0:32], "big")
    length = int.from_bytes(data[offset : offset + 32], "big")
    return data[offset + 32 : offset + 32 + length].decode("utf-8", "replace")


def encode_word_call(pool, word):
    # int16 is ABI-encoded as a sign-extended 256-bit two's-complement word, so
    # negative words become 0xffff..ffXX rather than a 16-bit-masked value.
    pool_arg = pool[2:].lower().rjust(64, "0")
    word_arg = (word & ((1 << 256) - 1)).to_bytes(32, "big").hex()
    return TICKLENS, SEL_POPULATED_TICKS + pool_arg + word_arg


def decode_populated_ticks(raw):
    """Decode TickLens.PopulatedTick[] = (int24 tick, int128 net, uint128 gross)."""
    data = bytes.fromhex(raw[2:])
    if len(data) < 64:
        return []
    count = int.from_bytes(data[32:64], "big")
    base = 64
    ticks = []
    for i in range(count):
        off = base + i * 96
        tick = int.from_bytes(data[off : off + 32], "big")
        if tick >= (1 << 255):
            tick -= 1 << 256
        net = int.from_bytes(data[off + 32 : off + 64], "big")
        if net >= (1 << 255):
            net -= 1 << 256
        ticks.append((tick, net))
    return ticks


def fetch_pool(rpc, pool, block):
    base = [
        (pool, SEL_SLOT0),
        (pool, SEL_LIQUIDITY),
        (pool, SEL_FEE),
        (pool, SEL_TICK_SPACING),
        (pool, SEL_TOKEN0),
        (pool, SEL_TOKEN1),
    ]
    slot0, liq, fee, spacing, t0, t1 = batch_call(rpc, base, block)

    # int24 fields (tick, tickSpacing) are ABI-encoded as full sign-extended
    # 256-bit words, so they must be decoded as signed 256-bit integers.
    sqrt_price = decode_int(slot0[2 : 2 + 64])
    current_tick = decode_int("0x" + slot0[2 + 64 : 2 + 128], signed=True)
    liquidity = decode_int(liq)
    fee_val = decode_int(fee)
    tick_spacing = decode_int(spacing, signed=True)
    token0 = decode_address(t0)
    token1 = decode_address(t1)

    meta = batch_call(
        rpc,
        [
            (token0, SEL_DECIMALS),
            (token1, SEL_DECIMALS),
            (token0, SEL_SYMBOL),
            (token1, SEL_SYMBOL),
        ],
        block,
    )
    dec0 = decode_int(meta[0])
    dec1 = decode_int(meta[1])
    sym0 = decode_string(meta[2])
    sym1 = decode_string(meta[3])

    # Window starts at the ~+/-20% price band (widened to whole bitmap words),
    # then expands until there is an initialized tick on both sides of the
    # current tick so neither swap direction is a one-sided truncation artifact.
    band_ticks = math.ceil(math.log(1.20) / math.log(1.0001))  # ~1823
    compressed = current_tick // tick_spacing
    current_word = compressed >> 8
    base_words = max(2, math.ceil(band_ticks / tick_spacing / 256) + 1)

    words_each_side = base_words
    ticks = []
    while True:
        word_calls = [
            encode_word_call(pool, current_word + w)
            for w in range(-words_each_side, words_each_side + 1)
        ]
        raw_words = batch_call(rpc, word_calls, block)
        ticks = []
        for raw in raw_words:
            ticks.extend(decode_populated_ticks(raw))
        ticks.sort(key=lambda t: t[0])

        has_below = any(idx <= current_tick and net != 0 for idx, net in ticks)
        has_above = any(idx > current_tick and net != 0 for idx, net in ticks)
        if has_below and has_above:
            break
        if words_each_side >= MAX_WORDS_EACH_SIDE:
            raise RuntimeError(
                f"no initialized ticks on both sides within {words_each_side} words "
                f"(below={has_below}, above={has_above}); one-sided pool"
            )
        words_each_side = min(words_each_side * 2, MAX_WORDS_EACH_SIDE)

    return {
        "pool_address": pool.lower(),
        "fee_tier": fee_val,
        "tick_spacing": tick_spacing,
        "token0": {"address": token0, "symbol": sym0, "decimals": dec0},
        "token1": {"address": token1, "symbol": sym1, "decimals": dec1},
        "liquidity": str(liquidity),
        "sqrt_price_x96": str(sqrt_price),
        "current_tick": current_tick,
        "window_words_each_side": words_each_side,
        "window_band_ticks": band_ticks,
        "ticks_below_current": sum(1 for idx, net in ticks if idx <= current_tick and net != 0),
        "ticks_above_current": sum(1 for idx, net in ticks if idx > current_tick and net != 0),
        "truncated": True,
        "synthetic": False,
        "ticks": [{"index": idx, "net_liquidity": str(net)} for idx, net in ticks],
    }


# Per-pool tick cap to keep total committed JSON small. Ticks are kept nearest
# the current tick, balanced across both sides; `valid_limit` (computed in Rust
# from get_limits over the loaded ticks) keeps swap amounts inside whatever is
# retained, so trimming the far tail is safe.
MAX_TICKS_PER_SIDE = 250


def trim_ticks(ticks, current_tick):
    below = [t for t in ticks if t["index"] <= current_tick]
    above = [t for t in ticks if t["index"] > current_tick]
    below = below[-MAX_TICKS_PER_SIDE:]
    above = above[:MAX_TICKS_PER_SIDE]
    return below + above


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", default=os.path.dirname(os.path.abspath(__file__)))
    parser.add_argument(
        "--block",
        default="latest",
        help="block tag ('latest') or decimal number; non-archive nodes prune "
        "state, so pinning an old number may revert on slot0 reads",
    )
    parser.add_argument(
        "--max-pools",
        type=int,
        default=0,
        help="cap the number of successfully fetched pools (0 = no cap)",
    )
    args = parser.parse_args()

    rpc = os.environ.get("RPC_URL") or os.environ.get("ETH_RPC_URL")
    if not rpc:
        print("RPC_URL or ETH_RPC_URL must be set", file=sys.stderr)
        sys.exit(1)

    if args.block == "latest":
        block = "latest"
        bn = hex_call_block_number(rpc)
    else:
        bn = int(args.block)
        block = hex(bn)

    universe = load_universe(args.out_dir)
    index = {"fetch_block": bn, "rpc_chain": "ethereum", "pools": []}

    def work(pool):
        for attempt in range(5):
            try:
                return fetch_pool(rpc, pool, block)
            except RuntimeError as exc:
                # One-sided / window-cap failures are deterministic; do not retry.
                if "one-sided" in str(exc):
                    print(f"  DROP {pool}: {exc}", file=sys.stderr)
                    return None
                if attempt == 4:
                    print(f"  FAILED {pool}: {exc}", file=sys.stderr)
                    return None
                time.sleep(2.0 * (attempt + 1))
            except Exception as exc:  # noqa: BLE001 - fetcher is best-effort
                if attempt == 4:
                    print(f"  FAILED {pool}: {exc}", file=sys.stderr)
                    return None
                time.sleep(2.0 * (attempt + 1))

    with ThreadPoolExecutor(max_workers=4) as pool_exec:
        results = list(pool_exec.map(work, universe))

    seen_names = {}
    kept = 0
    for snap in results:
        if snap is None:
            continue
        if args.max_pools and kept >= args.max_pools:
            break
        snap["ticks"] = trim_ticks(snap["ticks"], snap["current_tick"])

        base = f"{snap['token0']['symbol']}_{snap['token1']['symbol']}_{snap['fee_tier']}"
        base = "".join(c if c.isalnum() or c in "_-" else "_" for c in base)
        # Disambiguate pools that share pair+fee (e.g. duplicate deployments).
        count = seen_names.get(base, 0)
        seen_names[base] = count + 1
        name = base if count == 0 else f"{base}_{snap['pool_address'][2:8]}"

        path = os.path.join(args.out_dir, f"{name}.json")
        with open(path, "w") as fh:
            json.dump(snap, fh, separators=(",", ":"))
        index["pools"].append(
            {
                "file": f"{name}.json",
                "pool_address": snap["pool_address"],
                "pair": f"{snap['token0']['symbol']}/{snap['token1']['symbol']}",
                "fee_tier": snap["fee_tier"],
                "tick_count": len(snap["ticks"]),
            }
        )
        kept += 1
        print(f"  {name}: {len(snap['ticks'])} ticks")

    with open(os.path.join(args.out_dir, "index.json"), "w") as fh:
        json.dump(index, fh, indent=2)
    print(f"Wrote {len(index['pools'])} pools at block {bn}")


def hex_call_block_number(rpc):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []}
    resp = requests.post(rpc, json=payload, timeout=30)
    resp.raise_for_status()
    return int(resp.json()["result"], 16)


if __name__ == "__main__":
    main()
