#!/usr/bin/env python3
"""On-chain checks of a tycho-integrate run that read the venue itself, not the implementation.

    onchain.py --protocol P quotes        the spec's venue quotes again, as recorded and with the
                                          Tycho router as the caller: exit 1 on any difference
    onchain.py --protocol P watch         each watched contract's code and getters at the spec's
                                          block against the latest block: exit 1 on any change
    onchain.py --chain C read-set --to A --data D [--block B] [--from F]
                                          the accounts and storage slots one call reads
                                          (`debug_traceCall` with `prestateTracer`), as JSON

What each guards against, from past integrations:

- `quotes`: a spec quote that does not hold at its block (a wrong `quote_function` or golden
  vector), and pricing that depends on the caller (`msg.sender`/`tx.origin` screening, a router
  whitelist): the router is the caller in production, so a quote that differs with it is a fact the
  spec must state. Exactness against the implementation is the simulation layer's venue-quote test
  (implement.md; the `venue-quote-test` final gate), which these recorded quotes feed.
- `watch`: an upgrade or redeploy the EIP-1967 slot does not show (a new address behind a custom
  pointer, a config swap with no event): any code or getter on the quote path that changed since
  the spec's block reopens the spec.
- `read-set`: the state a quote reads, so research lists every mutable parameter on the quote
  path from the chain, not only from reading code. Each read slot must be tracked or explained in
  the spec's `read_set`.

Every RPC is the chain's from the env files (`schema.rpc_url`); the block reads need an archive
node.
"""

import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import envcheck  # noqa: E402
import schema  # noqa: E402

ROUTER_ADDRESSES = "crates/tycho-execution/config/router_addresses.json"
REVERTED = re.compile(r"execution reverted|\brevert(ed)?\b", re.I)


def router(chain: str, tree: Path):
    path = tree / ROUTER_ADDRESSES
    if not path.exists():
        path = schema.repo_root() / ROUTER_ADDRESSES
    return (schema.load_json(path) if path.exists() else {}).get(chain)


def block_tag(block) -> str:
    return hex(block) if isinstance(block, int) else block


def tx(to: str, data: str, sender=None) -> dict:
    return {"to": to, "data": data, **({"from": sender} if sender else {})}


def call(url: str, to: str, data: str, block, sender=None) -> str:
    return envcheck.rpc(url, "eth_call", [tx(to, data, sender), block_tag(block)], timeout=60)


def check_quotes(spec: dict, tree: Path) -> list:
    """Each `venue_quotes` entry ({chain, block, to, data, result}) again at its block, anonymous
    and from the chain's Tycho router."""
    rows = []
    for q in spec.get("venue_quotes", []):
        row = {k: q.get(k) for k in ("id", "chain", "block", "to", "note")}
        url = schema.rpc_url(q["chain"])
        if not url:
            rows.append({**row, "status": "unread", "detail": schema.no_rpc(q["chain"])})
            continue
        try:
            now = call(url, q["to"], q["data"], q["block"])
            row["result"] = now
            if now.lower() != (q.get("result") or "").lower():
                rows.append({**row, "status": "differs", "detail": f"the spec records {q.get('result')}, the venue returns {now} at block {q['block']}"})
                continue
            addr = router(q["chain"], tree)
            if not addr:
                rows.append({**row, "status": "unread", "detail": f"no Tycho router address for {q['chain']} in {ROUTER_ADDRESSES}"})
                continue
        except Exception as e:  # noqa: BLE001 - reported per quote
            rows.append({**row, "status": "unread", "detail": str(e)})
            continue
        try:
            as_router = call(url, q["to"], q["data"], q["block"], sender=addr)
        except Exception as e:  # noqa: BLE001 - reported per quote
            # Only a revert says the venue refused the router; a node's error (a quota, a timeout,
            # missing state) says nothing about the venue.
            reverted = REVERTED.search(str(e))
            rows.append({**row, "status": "caller-dependent" if reverted else "unread",
                         "detail": f"from the router {addr} the call {'reverts' if reverted else 'fails'} ({e}); anonymously it returns {now}"})
            continue
        if as_router.lower() != now.lower():
            rows.append({**row, "status": "caller-dependent", "detail": f"from the router {addr} the venue returns {as_router}, anonymously {now}"})
        else:
            rows.append({**row, "status": "same", "detail": "as recorded, and the same from the router"})
    return rows


def check_watch(spec: dict) -> list:
    """Each `watch` entry ({chain, address, block, data?}): its code (no `data`) or the getter's
    return (`data`) at the spec's block against the latest block."""
    rows, heads = [], {}
    for w in spec.get("watch", []):
        row = {k: w.get(k) for k in ("chain", "address", "block", "data", "what")}
        url = schema.rpc_url(w["chain"])
        if not url:
            rows.append({**row, "status": "unread", "detail": schema.no_rpc(w["chain"])})
            continue
        try:
            # One block per chain, so every reading of a chain is at the same, named block.
            if w["chain"] not in heads:
                heads[w["chain"]] = int(envcheck.rpc(url, "eth_blockNumber", []), 16)
            now_block = heads[w["chain"]]
            row["now_block"] = now_block
            if w.get("data"):
                then, now = (call(url, w["address"], w["data"], b) for b in (w["block"], now_block))
            else:
                then, now = (envcheck.rpc(url, "eth_getCode", [w["address"], block_tag(b)], timeout=60) for b in (w["block"], now_block))
            if then.lower() == now.lower():
                rows.append({**row, "status": "same"})
            else:
                shown = (then, now) if w.get("data") else (f"{len(then) // 2 - 1} bytes", f"{len(now) // 2 - 1} bytes")
                rows.append({**row, "status": "changed", "detail": f"at block {w['block']}: {shown[0]}; now: {shown[1]}"})
        except Exception as e:  # noqa: BLE001 - reported per entry
            rows.append({**row, "status": "unread", "detail": str(e)})
    return rows


def read_set(chain: str, to: str, data: str, block, sender=None) -> dict:
    url = schema.rpc_url(chain)
    if not url:
        raise SystemExit(schema.no_rpc(chain))
    trace = envcheck.rpc(url, "debug_traceCall", [tx(to, data, sender), block_tag(block), {"tracer": "prestateTracer"}], timeout=120)
    return {addr: sorted((acct.get("storage") or {}).keys()) for addr, acct in trace.items()}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol")
    p.add_argument("--chain")
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("quotes")
    sub.add_parser("watch")
    r = sub.add_parser("read-set")
    r.add_argument("--to", required=True)
    r.add_argument("--data", required=True)
    r.add_argument("--block", default="latest")
    r.add_argument("--from", dest="sender")
    args = p.parse_args()

    schema.load_env_files()
    if args.cmd == "read-set":
        if not args.chain:
            raise SystemExit("read-set needs --chain")
        block = int(args.block) if args.block.isdigit() else args.block
        print(json.dumps(read_set(args.chain, args.to, args.data, block, args.sender), indent=2))
        return
    if not args.protocol:
        raise SystemExit(f"{args.cmd} needs --protocol")
    spec = schema.load_json(schema.run_dir(args.protocol) / "spec.json")
    rows = check_quotes(spec, schema.worktree_dir(args.protocol)) if args.cmd == "quotes" else check_watch(spec)
    print(json.dumps(rows, indent=2))
    sys.exit(0 if all(r["status"] == "same" for r in rows) else 1)


if __name__ == "__main__":
    main()
