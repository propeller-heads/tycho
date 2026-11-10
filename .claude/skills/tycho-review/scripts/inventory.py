#!/usr/bin/env python3
"""The contract inventory of an integration (integration.md § Baseline, steps 1-2), built without a
model: for each address, its code at a block, the proxy it is (EIP-1967 implementation or beacon,
EIP-1167 clone) and the implementation behind it, and for each contract the verified source, ABI and
storage layout from Sourcify, with every event's topic0 and every function's selector. Chunk passes,
the integration pass and verifiers read it instead of fetching the same contracts each.

usage:
  inventory.py --chain base --out <run>/inventory [--block N] 0xADDR[=label] ...
  inventory.py --chain base --out <run>/inventory --scan <manifest.yaml> <integration_test.tycho.yaml> ...

--scan takes every address in the files, labelled with its file:line. Writes
<out>/<chain>/<address>/{meta.json, abi.json, events.md, functions.md, storage.md, sources/...} and
appends to <out>/inventory.md. Reads the chain's RPC from the environment (SKILL.md step 6); the URL
is never printed. A contract Sourcify does not have is listed as unverified: find its source by hand
(an explorer, the protocol's repo at the deployed commit) or decompile it.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

CHAINS = {  # name: (chain id, RPC variables in the repo's spellings)
    "ethereum": (1, ["RPC_URL", "ETH_RPC_URL"]), "base": (8453, ["BASE_RPC_URL"]),
    "bsc": (56, ["BSC_RPC_URL"]), "arbitrum": (42161, ["ARBITRUM_RPC_URL", "ARB_RPC_URL"]),
    "polygon": (137, ["POLYGON_RPC_URL"]), "unichain": (130, ["UNICHAIN_RPC_URL"]),
    "robinhood": (4663, ["ROBINHOOD_RPC_URL"]),
}
SLOTS = {  # EIP-1967
    "implementation": "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
    "beacon": "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50",
    "admin": "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103",
}
ADDR = re.compile(r"0x[0-9a-fA-F]{40}(?![0-9a-fA-F])")
ZERO = "0x" + "0" * 40


def mask(s: str) -> str:
    return re.sub(r"https?://\S+", "<rpc-url>", s)


def cast(*args) -> str:
    r = subprocess.run(["cast", *args], capture_output=True, text=True,
                       env=dict(os.environ, FOUNDRY_DISABLE_NIGHTLY_WARNING="1"))
    if r.returncode:
        raise RuntimeError(mask(r.stderr.strip().splitlines()[-1] if r.stderr.strip() else "cast failed"))
    return r.stdout.strip()


def keccak(text: str) -> str:
    return cast("keccak", text)


def canonical(param) -> str:
    t = param["type"]
    if t.startswith("tuple"):
        return "(" + ",".join(canonical(c) for c in param.get("components", [])) + ")" + t[5:]
    return t


def sourcify(chain_id: int, address: str):
    url = (f"https://sourcify.dev/server/v2/contract/{chain_id}/{address}"
           "?fields=abi,compilation,storageLayout,proxyResolution,sources")
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            return json.load(r)
    except Exception as e:  # 404 = not verified there
        return {"error": str(e)}


def word_address(word: str):
    a = "0x" + word[-40:]
    return None if int(word, 16) == 0 else a


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--chain", required=True, choices=sorted(CHAINS))
    ap.add_argument("--out", required=True)
    ap.add_argument("--block", default="latest")
    ap.add_argument("--scan", nargs="+", default=[])
    ap.add_argument("--max", type=int, default=60, help="at most this many addresses")
    ap.add_argument("addresses", nargs="*")
    a = ap.parse_args()
    chain_id, names = CHAINS[a.chain]
    rpc = next((os.environ[n] for n in names if os.environ.get(n)), None)

    todo = {}  # address -> labels
    for spec in a.addresses:
        addr, _, label = spec.partition("=")
        todo.setdefault(addr.lower(), []).append(label or "given")
    for f in a.scan:
        for i, line in enumerate(Path(f).read_text(errors="replace").splitlines(), 1):
            for m in ADDR.finditer(line):
                if m.group().lower() != ZERO:
                    todo.setdefault(m.group().lower(), []).append(f"{f}:{i}")
    if len(todo) > a.max:
        print(f"{len(todo)} addresses, more than --max {a.max}: only the first {a.max} are inventoried")
    out = Path(a.out) / a.chain
    out.mkdir(parents=True, exist_ok=True)
    rows = []
    seen = set()
    queue = [(addr, labels, None) for addr, labels in list(todo.items())[:a.max]]
    while queue:
        addr, labels, proxy_of = queue.pop(0)
        if addr in seen:
            continue
        seen.add(addr)
        d = out / addr
        d.mkdir(exist_ok=True)
        meta = dict(address=addr, chain=a.chain, block=a.block, labels=labels, implementation_of=proxy_of)
        impls = []
        if rpc:
            try:
                code = cast("code", addr, "--block", str(a.block), "--rpc-url", rpc)
                meta["code_bytes"] = (len(code) - 2) // 2
                if code.startswith("0x363d3d373d3d3d363d73"):  # EIP-1167 clone
                    meta["proxy"] = "eip1167"
                    impls.append("0x" + code[22:62])
                for kind, slot in SLOTS.items():
                    w = cast("storage", addr, slot, "--block", str(a.block), "--rpc-url", rpc)
                    if (v := word_address(w)):
                        meta[f"eip1967_{kind}"] = v
                if meta.get("eip1967_implementation"):
                    meta["proxy"] = "eip1967"
                    impls.append(meta["eip1967_implementation"])
                if meta.get("eip1967_beacon"):
                    meta["proxy"] = "eip1967-beacon"
                    impls.append(cast("call", meta["eip1967_beacon"], "implementation()(address)",
                                      "--block", str(a.block), "--rpc-url", rpc).lower())
            except RuntimeError as e:
                meta["rpc_error"] = str(e)
        else:
            meta["rpc_error"] = f"no RPC: set {' or '.join(names)}"
        s = sourcify(chain_id, addr)
        if "error" in s:
            meta["verified"] = False
            meta["sourcify_error"] = s["error"]
        else:
            comp = s.get("compilation") or {}
            meta.update(verified=True, match=s.get("match"), name=comp.get("name"),
                        compiler=comp.get("compilerVersion"), qualified=comp.get("fullyQualifiedName"),
                        sourcify_proxy=s.get("proxyResolution"))
            for impl in (s.get("proxyResolution") or {}).get("implementations") or []:
                impls.append(impl.get("address", "").lower())
                meta.setdefault("proxy", (s.get("proxyResolution") or {}).get("proxyType") or "proxy")
            abi = s.get("abi") or []
            (d / "abi.json").write_text(json.dumps(abi, indent=1))
            for path, src in (s.get("sources") or {}).items():
                p = d / "sources" / path.lstrip("/").replace("..", "_")
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(src.get("content", "") if isinstance(src, dict) else str(src))
            ev = ["| Event | topic0 | Indexed |", "|---|---|---|"]
            fn = ["| Function | Selector | Mutability |", "|---|---|---|"]
            for item in abi:
                if item.get("type") == "event":
                    sig = f"{item['name']}({','.join(canonical(i) for i in item.get('inputs', []))})"
                    idx = ", ".join(i.get("name") or "?" for i in item.get("inputs", []) if i.get("indexed"))
                    ev.append(f"| `{sig}` | `{keccak(sig)}` | {idx or '-'} |")
                elif item.get("type") == "function":
                    sig = f"{item['name']}({','.join(canonical(i) for i in item.get('inputs', []))})"
                    fn.append(f"| `{sig}` | `{keccak(sig)[:10]}` | {item.get('stateMutability', '')} |")
            (d / "events.md").write_text("\n".join(ev) + "\n")
            (d / "functions.md").write_text("\n".join(fn) + "\n")
            layout = s.get("storageLayout") or {}
            types = layout.get("types") or {}
            st = ["| Slot | Offset | Bytes | Name | Type | Declared in |", "|---|---|---|---|---|---|"]
            for v in layout.get("storage") or []:
                t = types.get(v.get("type"), {})
                st.append(f"| {v.get('slot')} | {v.get('offset')} | {t.get('numberOfBytes', '?')} | `{v.get('label')}` | "
                          f"`{t.get('label', v.get('type'))}` | {v.get('contract', '')} |")
            (d / "storage.md").write_text("\n".join(st) + "\n" if len(st) > 2 else "No storage layout on Sourcify.\n")
        impls = sorted({i for i in impls if i and i != ZERO and i != addr})
        meta["implementations"] = impls
        (d / "meta.json").write_text(json.dumps(meta, indent=1))
        rows.append(meta)
        queue += [(i, [f"implementation of {addr}"], addr) for i in impls]

    lines = [f"## {a.chain} at block {a.block}", "",
             "| Address | Labels | Code | Proxy → implementation | Verified (Sourcify) |", "|---|---|---|---|---|"]
    for m in rows:
        ver = (f"{m.get('name')} ({m.get('match')}, {m.get('compiler')})" if m.get("verified")
               else "**unverified**: find the source by hand")
        prox = (f"{m.get('proxy', 'proxy')} → " + ", ".join(f"`{i}`" for i in m["implementations"])) if m["implementations"] else "-"
        code = m.get("code_bytes", m.get("rpc_error", "?"))
        if code == 0:
            code = "**no code**"
        lines.append(f"| `{m['address']}` | {'; '.join(m['labels'][:3])}{' …' if len(m['labels']) > 3 else ''} | {code} | {prox} | {ver} |")
    with open(Path(a.out) / "inventory.md", "a") as f:
        f.write("\n".join(lines) + "\n\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
