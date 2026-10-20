# Copyright (c) 2026 Everlong Labs Limited
"""Extends a schema snapshot (schema/snapshot.py output) with the on-chain answers the ProtocolSim tests assert
against, all `eth_call` at the snapshot block (block.timestamp = the snapshot's):

  - `feed:<f>:kind` on both components (ocr2 / ocr2 / uptime / dual), the attribute the base-flamm substreams
    emits beside the schema's (README "Attributes"); the schema snapshot predates it;
  - `grids.swap_sell` / `grids.swap_buy`: `pool.previewSwap(poolAssetIn, amountIn)` on a log-spaced grid of
    sizes (sats for a sell, USDC base units for a buy), each row the return words or the revert data;
  - `grids.sell_edge` / `grids.buy_edge`: the largest fully consumed size per direction, located by the same
    procedure the Rust `get_limits` runs (doubling from 1 until a size is refused or clipped, then bisection),
    with every probe recorded as a grid row;
  - `grids.lever_up` / `grids.lever_down`: `pool.previewLever(up, amountIn)` on a few sizes (LevPaused on chain
    at all three blocks this script snapshots, so every row is a refusal and there is no lever edge to locate;
    the end-to-end recorder, fetch.py, also runs `lever_edge` at its pinned blocks);
  - `grids.spot`: `EverlongHook.spot(PoolContext)` (the argument is ignored on chain).

Usage: python3 grids.py <block>...  (reads out/schema/<block>.json, writes out/grids/<block>.json)
"""
import ast
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import rpc as R  # noqa: E402

POOL = "0xc0fdCB1799cCc2CEBaA1fe247157b0dF33D57572"
HOOK = "0x65CBD227cBC61248ae77a5fC813A29C54C092134"
KINDS = {"asset": "ocr2", "loan0": "ocr2", "seq": "uptime", "mo0": "dual"}

SEL_PREVIEW_SWAP = R.selector("previewSwap(bool,uint256)")
SEL_PREVIEW_LEVER = R.selector("previewLever(bool,uint256)")
SEL_SPOT = R.selector("spot((uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint48,uint8))")

U256_MAX = (1 << 256) - 1


def preview_swap_data(pool_asset_in, amount):
    return SEL_PREVIEW_SWAP + R.w256(1 if pool_asset_in else 0).hex() + R.w256(amount).hex()


def preview_lever_data(up, amount):
    return SEL_PREVIEW_LEVER + R.w256(1 if up else 0).hex() + R.w256(amount).hex()


def revert_data(err):
    """The revert data of an eth_call error string as rpc.py formats it: `<method> <params> -> <error dict>`,
    the error dict `{'code': 3, 'message': 'execution reverted', 'data': '0x..'}` (the params carry the
    calldata under the same key, hence the split on the arrow)."""
    i = err.rfind(" -> ")
    if i < 0:
        return None
    try:
        e = ast.literal_eval(err[i + 4:])
    except (ValueError, SyntaxError):
        return None
    if not isinstance(e, dict) or not isinstance(e.get("data"), str) or not e["data"].startswith("0x"):
        return None
    return e["data"]


class Prober:
    def __init__(self, rpc, block):
        self.rpc = rpc
        self.block = block
        self.rows = {}  # (kind, amount) -> row

    def call(self, to, data):
        ok, res = self.rpc.try_call(to, data, self.block)
        if ok:
            return {"ok": True, "ret": res}
        rd = revert_data(res)
        if rd is None:
            raise RuntimeError("eth_call failed without revert data: %s" % res)
        return {"ok": False, "revert": rd}

    def swap(self, pool_asset_in, amount):
        key = ("sell" if pool_asset_in else "buy", amount)
        if key not in self.rows:
            row = {"amount_in": str(amount)}
            row.update(self.call(POOL, preview_swap_data(pool_asset_in, amount)))
            self.rows[key] = row
        return self.rows[key]

    def lever(self, up, amount):
        key = ("lever_up" if up else "lever_down", amount)
        if key not in self.rows:
            row = {"amount_in": str(amount)}
            row.update(self.call(POOL, preview_lever_data(up, amount)))
            self.rows[key] = row
        return self.rows[key]

    def full(self, pool_asset_in, amount):
        """The size fills in full: previewSwap answered and amountInUsed == amountIn."""
        row = self.swap(pool_asset_in, amount)
        if not row["ok"]:
            return False
        words = R.dec_words(row["ret"])
        return len(words) == 3 and words[0] == amount

    def lever_full(self, up, amount):
        """The size fills in full: previewLever answered and amountInUsed == amountIn."""
        row = self.lever(up, amount)
        if not row["ok"]:
            return False
        words = R.dec_words(row["ret"])
        return len(words) == 4 and words[0] == amount

    def _edge(self, full):
        """Doubling from 1 to the first size that fills in full, doubling on to the first that does not, then
        bisection; every probe is a recorded row. None when no size up to 2^128 fills. This is the procedure
        the Rust `get_limits` runs (`sim.rs::limit`), over whichever preview `full` asks."""
        a = 1
        while a < (1 << 128) and not full(a):
            a <<= 1
        if a >= (1 << 128):
            return None
        lo = a
        hi = a << 1
        while hi < (1 << 200) and full(hi):
            lo, hi = hi, hi << 1
        while hi - lo > 1:
            mid = (lo + hi) >> 1
            if full(mid):
                lo = mid
            else:
                hi = mid
        return lo

    def edge(self, pool_asset_in):
        """The largest fully consumed `previewSwap` size in a direction (`_edge`)."""
        return self._edge(lambda a: self.full(pool_asset_in, a))

    def lever_edge(self, up=True):
        """The largest fully consumed `previewLever` size in a direction (`_edge`): what the lever-up venue's
        `get_limits` answers, the pool asset in."""
        return self._edge(lambda a: self.lever_full(up, a))


def log_grid(max_exp):
    out = []
    for e in range(max_exp + 1):
        for m in (1, 2, 3, 5, 7):
            out.append(m * 10 ** e)
    return out


def main(block):
    src = os.path.join(HERE, "out", "schema", "%d.json" % block)
    snap = json.load(open(src))
    rpc = R.RPC(verbose=True)
    p = Prober(rpc, block)
    for c in snap["components"]:
        for f, k in KINDS.items():
            c["state"]["attributes"]["feed:%s:kind" % f] = "0x" + k.encode().hex()
    sells = log_grid(9) + [15000, 15001, 14999]
    buys = log_grid(12)
    for a in sells:
        p.swap(True, a)
    for a in buys:
        p.swap(False, a)
    sell_edge = p.edge(True)
    buy_edge = p.edge(False)
    for a in (1, 1000, 15000, 10 ** 6, 10 ** 8):
        p.lever(True, a)
    for a in (1, 10 ** 6, 10 ** 9):
        p.lever(False, a)
    spot = p.call(HOOK, SEL_SPOT + (R.w256(0).hex() * 9))
    grids = {
        "note": "eth_call at block %d (timestamp %d): pool.previewSwap / previewLever on the sizes below, "
                "EverlongHook.spot; the edges are the largest fully consumed sizes located by doubling from 1 and "
                "bisection, every probe recorded as a row" % (block, snap["block"]["timestamp"]),
        "swap_sell": [p.rows[k] for k in sorted(p.rows) if k[0] == "sell"],
        "swap_buy": [p.rows[k] for k in sorted(p.rows) if k[0] == "buy"],
        "sell_edge": None if sell_edge is None else str(sell_edge),
        "buy_edge": None if buy_edge is None else str(buy_edge),
        "lever_up": [p.rows[k] for k in sorted(p.rows) if k[0] == "lever_up"],
        "lever_down": [p.rows[k] for k in sorted(p.rows) if k[0] == "lever_down"],
        "spot": spot,
    }
    snap["grids"] = grids
    out = os.path.join(HERE, "out", "grids", "%d.json" % block)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        json.dump(snap, f, indent=1, sort_keys=True)
    print("wrote %s: %d sell rows, %d buy rows, sell edge %s, buy edge %s, %d rpc calls" % (
        out, len(grids["swap_sell"]), len(grids["swap_buy"]), sell_edge, buy_edge, rpc.calls), flush=True)


if __name__ == "__main__":
    for b in sys.argv[1:]:
        main(int(b))
