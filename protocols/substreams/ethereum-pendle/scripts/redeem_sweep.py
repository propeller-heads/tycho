import os, json, urllib.request, concurrent.futures as cf, time
RPC = os.environ.get("RPC_URL", "https://eth.drpc.org")
NULL="0x0000000000000000000000000000000000000000"
def call(to,data):
    body=json.dumps({"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{"to":to,"data":data},"latest"]}).encode()
    req=urllib.request.Request(RPC,data=body,headers={"content-type":"application/json","user-agent":"curl/8.0"})
    for _ in range(4):
        try:
            r=json.load(urllib.request.urlopen(req,timeout=60))
            if "result" in r: return r["result"]
            return None
        except Exception: time.sleep(0.5)
    return None
def w(d): d=d[2:]; return [d[i:i+64] for i in range(0,len(d),64)]
def addr(x): return "0x"+x[-40:]
def num(x): return int(x,16)
def arr(res):
    if not res: return []
    ws=w(res); n=num(ws[1]); return [addr(x) for x in ws[2:2+n]]
def dec(a):
    r=call(a,"0x313ce567"); return num(r) if r else None

now=int(time.time())
mk=json.load(open("all_markets.json"))
def live(m):
    e=call(m["market"],"0xe184c9be")
    return m["market"] if e and num(e)>now else None
with cf.ThreadPoolExecutor(12) as ex: lives=[x for x in ex.map(live,mk) if x]
print("live markets:",len(lives))
def sy_of(m):
    r=call(m,"0x2c8ce6bc"); return addr(w(r)[0]) if r else None
with cf.ThreadPoolExecutor(12) as ex: sys_=sorted({s for s in ex.map(sy_of,lives) if s})
print("unique live SYs:",len(sys_))

def profile(sy):
    p={"sy":sy,"dec":dec(sy)}
    ai=call(sy,"0xa40bee50")
    if ai:
        ws=w(ai); p["asset_type"]=num(ws[0]); p["asset"]=addr(ws[1]); p["asset_dec"]=num(ws[2])
    er=call(sy,"0x3ba0b9a9"); p["rate"]=num(er) if er else None
    p["tokens_out"]=arr(call(sy,"0x071bc3c9"))
    return p
with cf.ThreadPoolExecutor(12) as ex: profs=list(ex.map(profile,sys_))

def close(a,b): return bool(b) and abs(a-b)*10**6 <= b
def classify_out(p):
    res=[]
    if p.get("dec") is None or not p.get("rate") or "asset_dec" not in p: return res
    sy_amt=10**p["dec"]
    for t in p["tokens_out"]:
        if t.lower()==NULL: res.append((t,"native")); continue
        td=dec(t)
        if td is None: res.append((t,"no-decimals")); continue
        got=call(p["sy"],"0xcbe52ae3"+t[2:].rjust(64,"0")+hex(sy_amt)[2:].rjust(64,"0"))
        if got is None: res.append((t,"revert")); continue
        got=num(got)
        pred_1to1 = sy_amt * 10**td // 10**p["dec"]
        asset_amt = sy_amt * p["rate"] // 10**18
        pred_idx  = asset_amt * 10**td // 10**p["asset_dec"]
        if close(got,pred_1to1) and not close(pred_1to1,pred_idx): res.append((t,"one_to_one"))
        elif close(got,pred_idx): res.append((t,"index_rate"))
        elif close(got,pred_1to1): res.append((t,"ambiguous_1to1_eq_idx"))
        else: res.append((t,"other"))
    return res
with cf.ThreadPoolExecutor(12) as ex: cls=list(ex.map(classify_out,profs))
json.dump([{**p,"tokens_out_class":c} for p,c in zip(profs,cls)],open("redeem_classes.json","w"),indent=1)
from collections import Counter
c=Counter(k for l in cls for _,k in l)
print("tokensOut pairs:",sum(len(l) for l in cls)); print(c)
for p,l in zip(profs,cls):
    bad=[x for x in l if x[1] in ("other","revert","no-decimals")]
    if bad: print(" ",p["sy"],"rate",p.get("rate"),"dec",p.get("dec"),"assetdec",p.get("asset_dec"),bad)
