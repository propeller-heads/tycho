#!/usr/bin/env python3
"""Count top-level venue addresses on the Titan pAMM quote stream.

Subscribes to the pamm_quote_stream WebSocket for a fixed duration and prints, per top-level
venue address: frame count and which accounts received storage overrides (stateDiff). Useful to
detect venue deployments appearing/disappearing (they historically change every few weeks) and to
check whether a venue is registry-backed (overrides 0xda7afeed...) or self-storage.

Usage:
    python3 scripts/titan-venue-census.py [--seconds 60] [--url wss://...]

Requires the `websockets` package (pip install websockets).
"""

import argparse
import asyncio
import hashlib
import itertools
import json
from collections import Counter, defaultdict

DEFAULT_URL = "wss://eu.rpc.titanbuilder.xyz/ws/pamm_quote_stream"
META_KEYS = {"slot", "blockNumber", "timestamp"}


def payload_hash(state_override: dict) -> str:
    """Canonical hash of a venue's override payload, used to detect duplicate venue keys."""
    return hashlib.sha256(
        json.dumps(state_override, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


async def census(url: str, seconds: float) -> None:
    import websockets

    venues: Counter = Counter()
    diff_accounts: dict = defaultdict(Counter)
    payloads: dict = defaultdict(set)
    frames = 0
    blocks = set()

    async with websockets.connect(url, max_size=2**24) as ws:
        loop = asyncio.get_event_loop()
        deadline = loop.time() + seconds
        while True:
            remaining = deadline - loop.time()
            if remaining <= 0:
                break
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
            except asyncio.TimeoutError:
                break
            frame = json.loads(raw)
            frames += 1
            if "blockNumber" in frame:
                blocks.add(frame["blockNumber"])
            for key, value in frame.items():
                if key in META_KEYS:
                    continue
                venues[key] += 1
                payloads[key].add(payload_hash(value.get("stateOverride", {})))
                for account, override in value.get("stateOverride", {}).items():
                    if override.get("stateDiff"):
                        diff_accounts[key][account] += 1

    print(f"{frames} frames over {seconds:.0f}s, {len(blocks)} blocks, {len(venues)} venues\n")
    for venue, count in venues.most_common():
        print(f"{venue}  {count} frames  ({len(payloads[venue])} unique payloads)")
        for account, n in diff_accounts[venue].most_common():
            print(f"    stateDiff -> {account}  ({n} frames)")
    if not venues:
        print("No venue frames received.")
        return

    # Venues whose payload streams overlap are aliases of the same maker feed (e.g. a maker's
    # oracle and router addresses both carrying the identical registry diff).
    print("\nDuplicate analysis (payload overlap between venue pairs):")
    duplicates_found = False
    for a, b in itertools.combinations(sorted(payloads), 2):
        intersection = len(payloads[a] & payloads[b])
        union = len(payloads[a] | payloads[b])
        if union == 0 or intersection == 0:
            continue
        duplicates_found = True
        print(
            f"  {a} == {b}: {intersection}/{union} shared payloads"
            f" ({100 * intersection / union:.0f}% overlap)"
        )
    if not duplicates_found:
        print("  none — every venue streams distinct payloads")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seconds", type=float, default=60, help="capture duration (default 60)")
    parser.add_argument("--url", default=DEFAULT_URL, help=f"stream endpoint (default {DEFAULT_URL})")
    args = parser.parse_args()
    asyncio.run(census(args.url, args.seconds))


if __name__ == "__main__":
    main()
