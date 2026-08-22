import json, urllib.request, time
def bs(url):
    req=urllib.request.Request(url,headers={"user-agent":"curl/8.0"})
    for _ in range(4):
        try: return json.load(urllib.request.urlopen(req,timeout=60))
        except Exception: time.sleep(1)
    return {}
FACT={"0x27b1dAcd74688aF24a64BD3C9C1B143118740784":"0x166ae5f55615b65bbd9a2496e98d4e4d78ca15bd6127c0fe2dc27b76f6c03143",
 "0x1A6fCc85557BC4fB7B534ed835a03EF056552D52":"0xae811fae25e2770b6bd1dcb1475657e8c3a976f91d1ebf081271db08eef920af",
 "0x3d75Bd20C983edb5fD218A1b7e0024F1056c7A2F":"0xae811fae25e2770b6bd1dcb1475657e8c3a976f91d1ebf081271db08eef920af",
 "0x6fcf753f2C67b83f7B09746Bbc4FA0047b35D050":"0xae811fae25e2770b6bd1dcb1475657e8c3a976f91d1ebf081271db08eef920af",
 "0x6d247b1c044fA1E22e6B04fA9F71Baf99EB29A9f":"0xae811fae25e2770b6bd1dcb1475657e8c3a976f91d1ebf081271db08eef920af"}
out=[]
for f,topic in FACT.items():
    url=f"https://eth.blockscout.com/api/v2/addresses/{f}/logs"; page=0
    while url and page<60:
        d=bs(url)
        for l in d.get("items",[]):
            if (l.get("topics") or [None])[0]==topic:
                out.append({"market":"0x"+l["topics"][1][-40:],"factory":f,"block":l["block_number"]})
        np=d.get("next_page_params")
        if not np: break
        url=f"https://eth.blockscout.com/api/v2/addresses/{f}/logs?"+"&".join(f"{k}={v}" for k,v in np.items())
        page+=1; time.sleep(0.15)
    print(f, sum(1 for m in out if m["factory"]==f))
json.dump(out, open("all_markets.json","w"))
print("total", len(out))
