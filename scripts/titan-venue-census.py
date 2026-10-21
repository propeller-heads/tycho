#!/usr/bin/env python3
"""Count the venues on the Titan pAMM quote stream.

Subscribes to the pamm_quote_stream_v2 WebSocket for a fixed duration and prints, per venue
`quoteAddress`: entry count, the makers and `pricedAddresses` it was seen with, and which accounts
received storage overrides (stateDiff). Useful to detect venue deployments appearing/disappearing
(they historically change every few weeks), to find the aliases a channel can match on, and to
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
import os
from collections import Counter, defaultdict

DEFAULT_URL = os.environ.get(
    "TITAN_PAMM_QUOTE_STREAM_URL", "wss://eu.data.titanbuilder.xyz/ws/pamm_quote_stream_v2"
)


def payload_hash(state_override: dict) -> str:
    """Canonical hash of a venue's override payload, used to detect duplicate venue keys."""
    return hashlib.sha256(
        json.dumps(state_override, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


async def census(url: str, seconds: float) -> None:
    import websockets

    venues: Counter = Counter()
    makers: dict = defaultdict(set)
    priced: dict = defaultdict(set)
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
            for entry in frame.get("venues", []):
                key = str(entry.get("quoteAddress")).lower()
                if "blockNumber" in entry:
                    blocks.add(entry["blockNumber"])
                venues[key] += 1
                makers[key].add(str(entry.get("maker")))
                priced[key].update(a.lower() for a in entry.get("pricedAddresses", []))
                state_override = entry.get("stateOverride") or {}
                payloads[key].add(payload_hash(state_override))
                for account, override in state_override.items():
                    if override.get("stateDiff"):
                        diff_accounts[key][account] += 1

    print(f"{frames} messages over {seconds:.0f}s, {len(blocks)} blocks, {len(venues)} venues\n")
    for venue, count in venues.most_common():
        print(f"{venue}  {count} entries  ({len(payloads[venue])} unique payloads)")
        print(f"    makers: {', '.join(sorted(makers[venue]))}")
        print(f"    pricedAddresses: {' '.join(sorted(priced[venue]))}")
        for account, n in diff_accounts[venue].most_common():
            print(f"    stateDiff -> {account}  ({n} entries)")
    if not venues:
        print("No venue entries received.")
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
    parser.add_argument("--url", default=DEFAULT_URL, help=f"stream endpoint (default $TITAN_PAMM_QUOTE_STREAM_URL or {DEFAULT_URL})")
    args = parser.parse_args()
    asyncio.run(census(args.url, args.seconds))


if __name__ == "__main__":
    main()
