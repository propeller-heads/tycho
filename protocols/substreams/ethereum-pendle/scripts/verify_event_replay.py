import os, json, urllib.request

RPC = os.environ.get("RPC_URL", "https://eth.drpc.org")
import sys
MARKET = sys.argv[1]
SWAP = "0x829000a5bc6a12d46e30cdcecd7c56b1efd88f6d7d059da6734a04f3764557c4"
MINT = "0xb4c03061fb5b7fed76389d5af8f2e0ddb09f8c70d1333abbb62582835e10accb"
BURN = "0x4cf25bc1d991c17529c25213d3cc0cda295eeaad5f13f361969b12ea48015f90"
UIR  = "0x5c0e21d57bb4cf91d8fe238d6f92e2685a695371b19209afcce6217b478f83e1"

def rpc(method, params):
    req = urllib.request.Request(RPC, data=json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode(),
                                 headers={"content-type":"application/json","user-agent":"curl/8.0"})
    r = json.load(urllib.request.urlopen(req, timeout=60))
    if "error" in r: raise RuntimeError(r["error"])
    return r["result"]

def words(data):
    d = data[2:]
    return [int(d[i:i+64], 16) for i in range(0, len(d), 64)]

def signed(x):
    return x - (1 << 256) if x >> 255 else x

def read_storage(block):
    # _storage() selector
    sel = "0xc3fb90d6"
    res = rpc("eth_call", [{"to": MARKET, "data": sel}, hex(block)])
    w = words(res)
    return signed(w[0]) if w[0] >> 127 == 0 else w[0] - (1 << 128), w[1], w[2]

START, END = 25_696_804, 25_796_804

def storage(block):
    res = rpc("eth_call", [{"to": MARKET, "data": "0xc3fb90d6"}, hex(block)])
    w = words(res)
    def i128(v): return v - (1 << 128) if v >> 127 else v
    return i128(w[0]), i128(w[1]), w[2]

pt0, sy0, rate0 = storage(START)
pt1, sy1, rate1 = storage(END)
print(f"on-chain @ {START}: totalPt={pt0} totalSy={sy0} lastLnImpliedRate={rate0}")
print(f"on-chain @ {END}:   totalPt={pt1} totalSy={sy1} lastLnImpliedRate={rate1}")

logs = []
step = 10_000
b = START + 1
while b <= END:
    to = min(b + step - 1, END)
    logs += rpc("eth_getLogs", [{"address": MARKET, "fromBlock": hex(b), "toBlock": hex(to),
                                 "topics": [[SWAP, MINT, BURN, UIR]]}])
    b = to + 1

pt, sy, rate = pt0, sy0, rate0
counts = {"swap":0, "mint":0, "burn":0, "uir":0}
for lg in sorted(logs, key=lambda l: (int(l["blockNumber"],16), int(l["logIndex"],16))):
    t0 = lg["topics"][0]
    w = words(lg["data"])
    if t0 == SWAP:
        net_pt_out, net_sy_out = signed(w[0]), signed(w[1])
        net_sy_to_reserve = w[3]
        pt -= net_pt_out
        sy -= net_sy_out + net_sy_to_reserve
        counts["swap"] += 1
    elif t0 == MINT:
        _lp, net_sy_used, net_pt_used = w
        sy += net_sy_used; pt += net_pt_used
        counts["mint"] += 1
    elif t0 == BURN:
        _lp, net_sy_out, net_pt_out = w
        sy -= net_sy_out; pt -= net_pt_out
        counts["burn"] += 1
    elif t0 == UIR:
        rate = w[0]
        counts["uir"] += 1

print(f"events replayed: {counts}")
print(f"replayed:        totalPt={pt} totalSy={sy} lastLnImpliedRate={rate}")
print(f"MATCH totalPt={pt==pt1}  totalSy={sy==sy1}  lastLnImpliedRate={rate==rate1}")
