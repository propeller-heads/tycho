# Copyright (c) 2026 Everlong Labs Limited
"""For every stage block with a pool transaction: the Morpho Blue events of the venue's market emitted by OTHER
transactions of the same block (eth_getLogs, topic1 = market id), so the simulation can reconstruct the market
totals the block left from its own settlement plus the other users' supplies, withdrawals, borrows, repays and
liquidations. Output: out/stages/morpho_<block>.json"""
import json, os, sys
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rpc import RPC, keccak256
from fetch import STAGES

MORPHO = "0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb"
MARKET = "0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836"
SIGS = {
    "Supply": ("Supply(bytes32,address,address,uint256,uint256)", ["assets", "shares"]),
    "Withdraw": ("Withdraw(bytes32,address,address,address,uint256,uint256)", ["caller", "assets", "shares"]),
    "Borrow": ("Borrow(bytes32,address,address,address,uint256,uint256)", ["caller", "assets", "shares"]),
    "Repay": ("Repay(bytes32,address,address,uint256,uint256)", ["assets", "shares"]),
    "AccrueInterest": ("AccrueInterest(bytes32,uint256,uint256,uint256)", ["prev_borrow_rate", "interest", "fee_shares"]),
    "Liquidate": ("Liquidate(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)",
                  ["repaid_assets", "repaid_shares", "seized_assets", "bad_debt_assets", "bad_debt_shares"]),
    "SupplyCollateral": ("SupplyCollateral(bytes32,address,address,uint256)", ["assets"]),
    "WithdrawCollateral": ("WithdrawCollateral(bytes32,address,address,address,uint256)", ["caller", "assets"]),
}
TOPICS = {"0x" + keccak256(sig.encode()).hex(): (name, fields) for name, (sig, fields) in SIGS.items()}


def main():
    rpc = RPC(verbose=False)
    for kind, block, hint, created, grids in STAGES:
        if kind != "real":
            continue
        st = json.load(open(os.path.join(HERE, "out", "stages", "%d.json" % block)))
        if hint == "feed":
            continue
        path = os.path.join(HERE, "out", "stages", "morpho_%d.json" % block)
        if os.path.exists(path):
            continue
        logs = rpc.get_logs(MORPHO, block, block, [None, MARKET])
        events = []
        for l in logs:
            name, fields = TOPICS.get(l["topics"][0], (None, None))
            if name is None:
                continue
            data = bytes.fromhex(l["data"][2:])
            words = [int.from_bytes(data[i:i + 32], "big") for i in range(0, len(data), 32)]
            ev = {"event": name, "tx": l["transactionHash"], "tx_index": int(l["transactionIndex"], 16),
                  "log_index": int(l["logIndex"], 16), "own": l["transactionHash"].lower() == st["tx"]["hash"].lower()}
            for f, w in zip(fields, words):
                ev[f] = ("0x%040x" % w) if f == "caller" else str(w)
            events.append(ev)
        json.dump({"block": block, "tx": st["tx"]["hash"], "events": events}, open(path, "w"), indent=1)
        others = [e for e in events if not e["own"]]
        print(block, "market events", len(events), "by other txs", len(others), [(e["event"], e.get("assets")) for e in others], flush=True)


if __name__ == "__main__":
    main()
