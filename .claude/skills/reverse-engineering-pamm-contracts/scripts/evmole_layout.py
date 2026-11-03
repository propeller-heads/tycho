"""Static map of a contract with evmole: functions, arguments, mutability and storage layout.

usage: uv run -q --with evmole==0.9.4 python -I evmole_layout.py ADDRESS|HEXFILE

An address is fetched with eth_getCode from ETH_RPC_URL; a file holds runtime bytecode as hex.
Everything here is inferred from bytecode: confirm slots and mutability with a trace
(storage_access.py). Known limits: a mapping to a struct shows only the struct field that
evmole could type, and mutability can be wrong (a state-writing function reported as `view`).
Selector names come from the openchain signature database and are guesses.
"""

import argparse
import json
import os
import sys
import urllib.request

import evmole


def rpc_url():
    for name in ("ETH_RPC_URL", "MAINNET_RPC_URL"):
        if os.environ.get(name):
            return os.environ[name]
    sys.exit("set ETH_RPC_URL to the chain the contract lives on (foundry convention)")


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(rpc_url(), body, {"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=60))["result"]


def load_code(target):
    if target.startswith("0x") and len(target) == 42:
        print(f"chain id: {int(rpc('eth_chainId', []), 16)}")
        code_hex = rpc("eth_getCode", [target, "latest"])
    else:
        with open(target) as handle:
            code_hex = handle.read().strip()
    code = bytes.fromhex(code_hex.removeprefix("0x"))
    if not code:
        sys.exit("no code: EOA, self-destructed, or wrong chain")
    return code


def lookup_names(selectors):
    url = "https://api.openchain.xyz/signature-database/v1/lookup?function=" + ",".join(
        "0x" + selector for selector in selectors
    )
    try:
        found = json.load(urllib.request.urlopen(url, timeout=30))["result"]["function"]
    except Exception as err:  # noqa: BLE001 - names are advisory, the layout does not need them
        print(f"selector names unavailable: {err}")
        return {}
    return {
        key.removeprefix("0x"): entries[0]["name"].split("(")[0]
        for key, entries in found.items()
        if entries
    }


def print_metadata(metadata):
    if metadata is None:
        print("metadata: none (no CBOR trailer)")
        return
    entries = []
    for entry in metadata.entries:
        value = entry.value.value
        if entry.key == "solc" and isinstance(value, bytes) and len(value) == 3:
            value = ".".join(str(part) for part in value)
        elif isinstance(value, bytes):
            value = value.hex()
        entries.append(f"{entry.key}={value}")
    print("metadata:", " ".join(entries))


def print_storage(title, records, names):
    print(f"{title} ({len(records)} records): slot | offset | type | reads | writes")
    for record in sorted(records, key=lambda r: (int(r.slot, 16), r.offset)):
        slot = int(record.slot, 16)
        slot_text = str(slot) if slot < 2**32 else "0x" + record.slot
        reads = ",".join(names.get(s, s) for s in sorted(record.reads)) or "-"
        writes = ",".join(names.get(s, s) for s in sorted(record.writes)) or "-"
        print(f"  {slot_text:>6} | {record.offset:>2} | {record.type} | R {reads} | W {writes}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("target", help="contract address or file with runtime bytecode hex")
    args = parser.parse_args()
    code = load_code(args.target)
    print(f"code size: {len(code)} bytes")
    info = evmole.contract_info(
        code,
        selectors=True,
        arguments=True,
        state_mutability=True,
        storage=True,
        metadata=True,
    )
    print_metadata(info.metadata)
    functions = sorted(info.functions or [], key=lambda f: f.selector)
    names = lookup_names([f.selector for f in functions]) if functions else {}
    print(f"functions ({len(functions)}): selector | mutability | name(arguments)")
    for function in functions:
        name = names.get(function.selector, "?")
        print(
            f"  {function.selector} | {function.state_mutability:<10} | "
            f"{name}({function.arguments})"
        )
    print_storage("storage", info.storage or [], names)
    if info.transient_storage:
        print_storage("transient storage", info.transient_storage, names)


if __name__ == "__main__":
    main()
