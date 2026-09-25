# Copyright (c) 2026 Everlong Labs Limited
"""Fetches the real Base data of the end-to-end replay: for every stage block, the tracked words before and after
it (eth_getStorageAt), the transaction of interest (eth_getTransactionByHash / Receipt), the aggregators' logs,
the runtime code of the contracts it created (eth_getCode) and, at the pinned blocks, the pool's previewSwap /
previewLever grids and the hook's spot (eth_call). One JSON per stage under out/stages/, resumable.

Stage kinds: `catchup` (no transaction of interest: the net storage diff since the previous stage, which the
replay feeds the package as one synthetic transaction at the stage block), `deploy` (a creator transaction that
created registered contracts), `real` (a transaction of interest whose block's diffs are replayed as its own).
"""
import json, os, sys
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rpc import RPC, hexword, keccak256
import universe as U
import grids as G

AGG_ADDRS = list(U.AGGREGATORS)
POOL_FAMILY = ["0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572", "0x65cbd227cbc61248ae77a5fc813a29c54c092134",
               "0x04988af54ec88d2de77b191025eaef2fe488f93b", "0x19a9b39e6710aad109c829294b0841f0851c6bb4",
               "0x6760e3b032ee2d670cb684d9076b8f48cb066c48", "0xbed275459578c87a63f2f50a0b077c720e838816",
               "0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f"]

# (kind, block, tx hash or None, created contracts, grids?)
STAGES = [
    ("catchup", 51154977, None, [], False),
    ("deploy", 51154978, "0x302c5d3135c344f9a97cc49f0294c18cbfd962445b5f9926b510258d376a2852", ["0xbed275459578c87a63f2f50a0b077c720e838816"], False),
    ("deploy", 51154979, "0x25a1b6bc1877a075520d027c917d6e773a2200bb22bd5d6b0876ed0f21a335b2", ["0x1da990c8b0bf15d1f441782f01351acfee49bbaa"], False),  # unregistered code: ignored
    ("deploy", 51154983, "0x73fd500ebf61353682e9e68cafb35abec738613a0c8357a816fbc477f3b8b137", ["0xaad580beaa2cbd8ab5f3956a5c56eda1d5ee7184"], False),
    ("deploy", 51154985, "0xa57e5ec2d9fcddd7efc18c566f36864d3d55062620939341d2186972349e18e6", ["0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f", "0x19a9b39e6710aad109c829294b0841f0851c6bb4"], False),
    ("deploy", 51154986, "0x535571239108699b51103de915ecb7005dceb8f7923bbe920b3343682298be6d", ["0x65cbd227cbc61248ae77a5fc813a29c54c092134"], False),
    ("deploy", 51154987, "0xeb9575c7bd2bcc7de0bbc11a1bb936bd4dfb04d70e3e69729817cb17eff322a4", ["0xe0a98d8e60035832b8bad7f7af7b9b0b3a7308f3"], False),
    ("deploy", 51154988, "0xcb95906da659f310b4d56065c778d962d4857df9f019174aa36cafcc48374785", ["0x04988af54ec88d2de77b191025eaef2fe488f93b"], False),
    ("real", 51154990, "0x783b464e93692538bde6dc1b53b959087f1e0ba2af3b6bc55ff13a449101d1e0", ["0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572", "0x6760e3b032ee2d670cb684d9076b8f48cb066c48"], False),
    ("catchup", 51155010, None, [], "paused"),
    ("real", 51298416, "0x4af4828c" , [], True),  # activation; hash completed from logs below
    ("real", 51300667, "0x689f4aa63c0715acee52d34cddf3bf3a860c387177789515c14904b01ee37a73", [], False),
    ("catchup", 51302915, None, [], False),
    ("real", 51302916, "0x46c3cd72a5860b2fe546e5a2130e066314e3777027151661e1e4f19a935901fa", [], True),
    ("catchup", 51302920, None, [], True),
    ("real", 51343234, "0x93ba2b7e2cda6181d2eb7ebf1b4da64cec70cffa0644d8dd398809f21b83fafc", [], False),
    ("real", 51347390, "0xab5aae179bae85754fdc0610f5a135b46836faf022b4cf3ebc8db868eccf9d30", [], False),
    ("real", 51348093, "0xe12d2ce7f85fd7d4d128afe77a49cec0f6a8cc7aa4da9b8aa352203ce8c03f42", [], True),  # withdrawal
    ("real", 51384803, "0xc5c72f13659eecf78bc01d17103a37e4b5dd6f5db475723d4a114e42716af705", [], True),  # keeper recenter
    ("real", 51420672, "0x9e96d58933198c48de75b66d352aa3c4fbf998963c88c1142d29fb1c3c42c687", [], False),
    ("real", 51420867, "0x37bea68272353f41964ed7a4eafcbe767849934a74b513ff0373369f0027d179", [], False),
    ("real", 51426394, "0x301f15d11b67d3313e6816f8e70890bc102fd9e854cb8df36419e547c5c47858", [], False),
    ("real", 51429815, "feed", [], True),
    ("real", 51430828, "0xb6faa30c24ac97fce2f25dac5f8ba3e1e6745b42c56479dc3ee116ae82491df5", [], False),
    ("real", 51433135, "feed", [], True),
    ("real", 51433218, "feed", [], True),
    ("real", 51433699, "0xcf29fef37feb3c9878cb16cbb96fa7f5a278178d34a61625d322e66146b73640", [], True),  # LevPauseSet(false)
    ("real", 51649706, "0x441839955de1f9bafe77eaaa96a8ba36517ae1c357c218a0f7919e516d22bdd1", [], True),  # MaxSpreadAgeSet(0)
    ("catchup", 51670000, None, [], True),
]


def state_json(state):
    return [{"address": a, "slot": hexword(s), "value": hexword(v)} for (a, s), v in sorted(state.items())]


def diff(before, after):
    out = []
    for k, v in sorted(after.items()):
        old = before.get(k)
        if old is None:
            if v != 0:
                out.append({"address": k[0], "slot": hexword(k[1]), "old": None, "new": hexword(v)})
        elif old != v:
            out.append({"address": k[0], "slot": hexword(k[1]), "old": hexword(old), "new": hexword(v)})
    return out


def tx_of_interest(rpc, block, hint):
    if hint == "feed":
        logs = rpc.get_logs(AGG_ADDRS, block, block)
        assert logs, block
        h = logs[0]["transactionHash"]
    elif len(hint) < 66:
        logs = rpc.get_logs(POOL_FAMILY, block, block)
        hs = {l["transactionHash"] for l in logs if l["transactionHash"].startswith(hint)}
        assert len(hs) == 1, (block, hint, hs)
        h = hs.pop()
    else:
        h = hint
    tx = rpc.call_raw("eth_getTransactionByHash", [h])
    rc = rpc.call_raw("eth_getTransactionReceipt", [h])
    assert rc["status"] == "0x1", (block, h)
    logs = [{"address": l["address"], "topics": l["topics"], "data": l["data"], "logIndex": l["logIndex"]} for l in rc["logs"]]
    return {"hash": h, "index": int(tx["transactionIndex"], 16), "from": tx["from"], "to": tx.get("to"), "input": tx["input"],
            "contractAddress": rc.get("contractAddress"), "logs": logs}


def record_grids(rpc, block, ts, mode):
    p = G.Prober(rpc, block)
    if mode == "paused":
        for a in [1, 15000, 10 ** 6]:
            p.swap(True, a)
        for a in [1, 10 ** 6, 10 ** 9]:
            p.swap(False, a)
        for a in [1, 15000]:
            p.lever(True, a)
        p.lever(False, 10 ** 6)
        sell_edge = buy_edge = lever_edge = None
    else:
        sells = G.log_grid(9) + [15000, 15001, 14999]
        buys = G.log_grid(12)
        for a in sells:
            p.swap(True, a)
        for a in buys:
            p.swap(False, a)
        sell_edge = p.edge(True)
        buy_edge = p.edge(False)
        for a in (1, 1000, 15000, 10 ** 6, 10 ** 8):
            p.lever(True, a)
        # The lever-up edge, the size the venue's get_limits answers. Its ~130 probes are worth
        # making only where the venue fills at all: a lever-up refuses on the spread and the pause
        # before the size is looked at, so where none of the sizes above filled there is no edge
        # to locate and no row is emitted. Every probe above is already cached, so this gate is
        # free; the replay asserts the port finds no limit wherever no edge is recorded.
        lever_edge = p.lever_edge() if any(p.lever_full(True, a) for a in (1, 1000, 15000)) else None
        for a in (1, 10 ** 6, 10 ** 9):
            p.lever(False, a)
    spot = p.call(G.HOOK, G.SEL_SPOT + (G.R.w256(0).hex() * 9))
    out = {
        "note": "eth_call at block %d (timestamp %d): pool.previewSwap / previewLever on the sizes below, EverlongHook.spot; "
                "the edges are the largest fully consumed sizes located by doubling from 1 and bisection, every probe "
                "recorded as a row" % (block, ts),
        "swap_sell": [p.rows[k] for k in sorted(p.rows) if k[0] == "sell"],
        "swap_buy": [p.rows[k] for k in sorted(p.rows) if k[0] == "buy"],
        "sell_edge": None if sell_edge is None else str(sell_edge),
        "buy_edge": None if buy_edge is None else str(buy_edge),
        "lever_up": [p.rows[k] for k in sorted(p.rows) if k[0] == "lever_up"],
        "lever_down": [p.rows[k] for k in sorted(p.rows) if k[0] == "lever_down"],
        "spot": spot,
    }
    if lever_edge is not None:
        out["lever_edge"] = str(lever_edge)
    return out


def main():
    rpc = RPC(verbose=True)
    os.makedirs(os.path.join(HERE, "out", "stages"), exist_ok=True)
    seed_path = os.path.join(HERE, "out", "stages", "seed_51154965.json")
    if os.path.exists(seed_path):
        prev = {(w["address"], int(w["slot"], 16)): int(w["value"], 16) for w in json.load(open(seed_path))["state"]}
    else:
        prev = U.read_universe(rpc, 51154965)
        json.dump({"block": 51154965, "state": state_json(prev)}, open(seed_path, "w"), indent=1)
    prev_block = 51154965
    for kind, block, hint, created, grids in STAGES:
        path = os.path.join(HERE, "out", "stages", "%d.json" % block)
        if os.path.exists(path):
            st = json.load(open(path))
            prev = {(w["address"], int(w["slot"], 16)): int(w["value"], 16) for w in st["state_after"]}
            prev_block = block
            print("cached", block, flush=True)
            continue
        header = rpc.block(block)
        ts = int(header["timestamp"], 16)
        out = {"kind": kind, "block": block, "header": {"number": header["number"], "hash": header["hash"],
               "parentHash": header["parentHash"], "timestamp": header["timestamp"]}}
        if kind == "catchup":
            after = U.read_universe(rpc, block)
            U.check_fronts(after)
            out["catchup_from"] = prev_block
            out["writes"] = diff(prev, after)
        else:
            before = U.read_universe(rpc, block - 1)
            after = U.read_universe(rpc, block)
            U.check_fronts(after)
            hb = rpc.block(block - 1)
            out["header_before"] = {"number": hb["number"], "hash": hb["hash"], "parentHash": hb["parentHash"], "timestamp": hb["timestamp"]}
            out["catchup_from"] = prev_block
            out["catchup_writes"] = diff(prev, before)  # the net diff of the blocks between the previous stage and this one
            out["writes"] = diff(before, after)         # this block's own diff
            out["tx"] = tx_of_interest(rpc, block, hint)
            out["feed_logs"] = rpc.get_logs(AGG_ADDRS, block, block)
            codes = {}
            for a in created:
                c = rpc.code(a, block)
                codes[a] = {"code": c, "codehash": "0x" + keccak256(bytes.fromhex(c[2:])).hex()}
            out["codes"] = codes
        if grids:
            out["grids"] = record_grids(rpc, block, ts, grids)
        out["state_after"] = state_json(after)
        json.dump(out, open(path, "w"), indent=1)
        print("wrote", block, kind, "writes", len(out["writes"]), "catchup", len(out.get("catchup_writes", [])),
              "calls", rpc.calls, flush=True)
        prev = after
        prev_block = block


if __name__ == "__main__":
    main()
