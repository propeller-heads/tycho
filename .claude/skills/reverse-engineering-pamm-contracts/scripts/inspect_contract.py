"""First look at a contract: size, verification, proxy slots, linked contracts, selectors.

usage: inspect_contract.py ADDRESS [--fetch-sources DIR] [--history]

Needs ETH_RPC_URL and `cast`. Prints:
- code size and hash, and the Sourcify match with the URL of its sources. --fetch-sources DIR
  writes the verified source tree under DIR/ADDRESS/.
- the EIP-1967 implementation, admin and beacon slots. A proxy's Sourcify status is the
  proxy's own, not its logic's, so when the implementation or beacon slot is set the
  implementation (for a beacon, its `implementation()`) is inspected as well.
- linked contracts: PUSH20 constants, PUSH32 constants whose top 12 bytes are zero (solc stores
  immutable addresses as 32-byte words), and the addresses that zero-argument functions
  (`amm()`, `registry()`, `vault()`) return. Only addresses that have code are listed.
- selectors with cast's argument/mutability guess and openchain names (guesses: an unresolved
  selector is normal for unverified code).
- with --history: the deployment block (binary search on eth_getCode) and every block where
  the EIP-1967 implementation slot changed value. Needs an archive node; assumes a slot value,
  once replaced, is not set again later.
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

PROXY_SLOTS = {
    "eip1967.implementation": 0x360894A13BA1A3210667C828492DB98DCA3E2076CC3735A920A3CA505D382BBC,
    "eip1967.admin": 0xB53127684A568B3173AE13B9F8A6016E243E63B6E8EE1178D6A717850B5D6103,
    "eip1967.beacon": 0xA3F0AD74E5423AEBFD80D3EF4346578335A9A72AEAEE59FF6CB3582B35133D50,
}
IMPLEMENTATION_SELECTOR = "0x5c60da1b"
SOURCIFY = "https://sourcify.dev/server/v2/contract"


def rpc_url():
    for name in ("ETH_RPC_URL", "MAINNET_RPC_URL"):
        if os.environ.get(name):
            return os.environ[name]
    sys.exit("set ETH_RPC_URL to the chain the contract lives on (foundry convention)")


def rpc_batch(calls):
    """Results of [(method, params)]; an eth_call that reverts gives None. Retries 429/503."""
    if not calls:
        return []
    payload = [
        {"jsonrpc": "2.0", "id": i, "method": m, "params": p} for i, (m, p) in enumerate(calls)
    ]
    req = urllib.request.Request(
        rpc_url(), json.dumps(payload).encode(), {"Content-Type": "application/json"}
    )
    for attempt in range(8):
        try:
            replies = json.load(urllib.request.urlopen(req, timeout=120))
            break
        except urllib.error.HTTPError as err:
            if err.code not in (429, 503) or attempt == 7:
                raise
            time.sleep(0.5 * 2**attempt)
    return [reply.get("result") for reply in sorted(replies, key=lambda r: r["id"])]


def rpc(method, params):
    return rpc_batch([(method, params)])[0]


def get_json(url):
    try:
        return json.load(urllib.request.urlopen(url, timeout=30))
    except Exception as err:  # noqa: BLE001 - report and continue, these lookups are advisory
        return {"error": str(err)}


def word_address(value):
    """`0x`-address in a 32-byte word whose top 12 bytes are zero, else None."""
    if value is None or len(value) != 66 or int(value, 16) == 0 or int(value, 16) >> 160:
        return None
    return "0x" + value[-40:]


def push_constants(code):
    """Addresses from PUSH20 operands and from PUSH32 operands whose top 12 bytes are zero."""
    found, i = set(), 0
    while i < len(code):
        op = code[i]
        if 0x60 <= op <= 0x7F:
            width = op - 0x5F
            operand = code[i + 1 : i + 1 + width]
            if width == 20 and len(operand) == 20:
                found.add("0x" + operand.hex())
            elif width == 32 and len(operand) == 32 and not any(operand[:12]):
                found.add("0x" + operand[12:].hex())
            i += width
        i += 1
    return sorted(found)


def with_code(addresses):
    codes = rpc_batch([("eth_getCode", [a, "latest"]) for a in addresses])
    return [a for a, code in zip(addresses, codes) if code not in (None, "0x")]


def getter_addresses(address, selectors):
    """{selector: address} returned by zero-argument functions, for addresses with code."""
    results = rpc_batch([("eth_call", [{"to": address, "data": s}, "latest"]) for s in selectors])
    returned = {s: word_address(r) for s, r in zip(selectors, results)}
    returned = {s: a for s, a in returned.items() if a}
    alive = set(with_code(sorted(set(returned.values()))))
    return {s: a for s, a in returned.items() if a in alive}


def print_selectors(code_hex):
    """Print the selector table; returns the zero-argument selectors."""
    listing = subprocess.check_output(["cast", "selectors", code_hex]).decode().splitlines()
    rows = [line.split("\t") for line in listing if line.strip()]
    selectors = [row[0].strip() for row in rows]
    names = get_json(
        "https://api.openchain.xyz/signature-database/v1/lookup?function=" + ",".join(selectors)
    )
    resolved = names.get("result", {}).get("function", {}) if "result" in names else {}
    print("selectors (cast's argument/mutability guess | openchain names):")
    for line, selector in zip(listing, selectors):
        known = [entry["name"] for entry in (resolved.get(selector) or [])][:2]
        print(f"  {line.strip():<70} | {', '.join(known) or '?'}")
    return [row[0].strip() for row in rows if len(row) > 1 and not row[1].strip()]


def fetch_sources(chain_id, address, out_dir):
    """Write the Sourcify source tree of `address` under out_dir/address/."""
    reply = get_json(f"{SOURCIFY}/{chain_id}/{address}?fields=sources")
    sources = reply.get("sources") or {}
    root = (out_dir / address.lower()).resolve()
    for name, entry in sources.items():
        path = (root / name.lstrip("/")).resolve()
        if not path.is_relative_to(root):
            print(f"  skipped source path outside {root}: {name}")
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(entry.get("content", ""))
    print(f"  wrote {len(sources)} source files under {root}" if sources else "  no sources")


def first_block(predicate, low, high):
    """Lowest block in [low, high] where `predicate` holds, given it holds from there on."""
    while low < high:
        middle = (low + high) // 2
        if predicate(middle):
            high = middle
        else:
            low = middle + 1
    return low


def print_history(address, head):
    deployed = first_block(lambda b: rpc("eth_getCode", [address, hex(b)]) != "0x", 0, head)
    print(f"history: code since block {deployed}")
    slot = hex(PROXY_SLOTS["eip1967.implementation"])

    def implementation(block):
        return int(rpc("eth_getStorageAt", [address, slot, hex(block)]), 16)

    block = head
    while block >= deployed:
        value = implementation(block)
        since = first_block(lambda b, v=value: implementation(b) == v, deployed, block)
        print(f"  eip1967.implementation {value:#042x} from block {since}")
        block = since - 1


def proxy_target(address, slots):
    """The implementation behind an EIP-1967 proxy or beacon proxy, or None."""
    if slots.get("eip1967.implementation"):
        return f"{slots['eip1967.implementation']:#042x}"
    if slots.get("eip1967.beacon"):
        beacon = f"{slots['eip1967.beacon']:#042x}"
        return word_address(
            rpc("eth_call", [{"to": beacon, "data": IMPLEMENTATION_SELECTOR}, "latest"])
        )
    return None


def inspect(address, chain_id, args):
    """Print the inspection of `address`; returns the proxy's implementation or None."""
    code_hex = rpc("eth_getCode", [address, "latest"])
    code = bytes.fromhex(code_hex[2:])
    print(f"code size: {len(code)} bytes")
    if not code:
        sys.exit("no code: EOA, self-destructed, or wrong chain")
    print(f"code hash: {subprocess.check_output(['cast', 'keccak', code_hex]).decode().strip()}")
    verified = get_json(f"{SOURCIFY}/{chain_id}/{address}")
    print(
        f"sourcify match: {verified.get('match')}; "
        f"sources: {SOURCIFY}/{chain_id}/{address}?fields=sources"
    )
    if args.fetch_sources and verified.get("match"):
        fetch_sources(chain_id, address, args.fetch_sources)
    values = rpc_batch(
        [("eth_getStorageAt", [address, hex(s), "latest"]) for s in PROXY_SLOTS.values()]
    )
    slots = {name: int(value, 16) for name, value in zip(PROXY_SLOTS, values)}
    for name, value in slots.items():
        if value:
            print(f"{name}: {value:#042x}")
    target = proxy_target(address, slots)
    if target:
        print(f"proxy: the Sourcify status above is the proxy's; implementation {target} follows")
    print("linked contracts in code (PUSH20, and PUSH32 with 12 zero bytes; with code):")
    for constant in with_code(push_constants(code)):
        print("  ", constant)
    zero_argument = print_selectors(code_hex)
    linked = getter_addresses(address, zero_argument) if zero_argument else {}
    if linked:
        print("addresses returned by zero-argument functions (with code):")
        for selector, returned in sorted(linked.items()):
            print(f"   {selector} -> {returned}")
    if args.history:
        print_history(address, int(rpc("eth_blockNumber", []), 16))
    return target


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("address")
    p.add_argument("--fetch-sources", type=Path, metavar="DIR")
    p.add_argument("--history", action="store_true")
    args = p.parse_args()
    chain_id = int(rpc("eth_chainId", []), 16)
    print(f"chain id: {chain_id}")
    target = inspect(args.address, chain_id, args)
    if target:
        print(f"\n== implementation {target}")
        inspect(target, chain_id, argparse.Namespace(**{**vars(args), "history": False}))


if __name__ == "__main__":
    main()
