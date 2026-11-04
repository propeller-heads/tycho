"""Turns a config's profiles into a plan: the ids every profile would fetch from the target today."""
import concurrent.futures as cf, json, math, os
import tycho
from tycho import FETCH_PAGE

TOKENS_PAGE = 12900


def all_systems(cfg):
    ps = tycho.post(cfg, "/v1/protocol_systems", {"chain": cfg["target"]["chain"], "pagination": {"page": 0, "page_size": 100}})
    # Some hosts list no dci_protocols although uniswap_v4_hooks is traced there.
    dci = set(ps.get("dci_protocols", [])) | {s for s in ps["protocol_systems"] if s == "uniswap_v4_hooks"}
    return sorted(ps["protocol_systems"]), dci


def components(cfg, system, tvl_gt):
    """All component ids and their contract ids above tvl_gt, paged like a client would."""
    ids, contracts, page = [], set(), 0
    while True:
        r = tycho.post(cfg, "/v1/protocol_components", {"chain": cfg["target"]["chain"], "protocol_system": system, "tvl_gt": tvl_gt,
                                                        "pagination": {"page": page, "page_size": FETCH_PAGE}})
        for c in r["protocol_components"]:
            ids.append(c["id"])
            contracts.update(c.get("contract_ids", []))
        total = r["pagination"]["total"]
        page += 1
        if page * FETCH_PAGE >= total or not r["protocol_components"]:
            break
    return ids, contracts, total


def entry_point_contracts(cfg, system, ids, chunk=100):
    def one(c):
        r = tycho.post(cfg, "/v1/traced_entry_points", {"chain": cfg["target"]["chain"], "protocol_system": system, "component_ids": c,
                                                        "pagination": {"page": 0, "page_size": chunk}})
        s = set()
        for traces in r.get("traced_entry_points", {}).values():
            for pair in traces:
                res = pair[1] if isinstance(pair, list) else pair.get("tracing_result", {})
                s.update(res.get("accessed_slots", {}).keys())
        return s
    out = set()
    with cf.ThreadPoolExecutor(4) as ex:
        for s in ex.map(one, [ids[i:i + chunk] for i in range(0, len(ids), chunk)]):
            out |= s
    return out


def build(cfg, out_path, log=print):
    chain = cfg["target"]["chain"]
    systems, dci = all_systems(cfg)
    first = tycho.post(cfg, "/v1/tokens", {"chain": chain, "min_quality": 100, "traded_n_days_ago": 30, "pagination": {"page": 0, "page_size": 1}})
    token_pages = max(1, math.ceil(first["pagination"]["total"] / TOKENS_PAGE))
    cache, profiles = {}, []
    for p in cfg["profiles"]:
        tvl = max(p["tvl_gt"], cfg["rules"]["tvl_floor"])
        sel = systems if p["protocols"] == "all" else [s for s in p["protocols"] if s in systems]
        sel = [s for s in sel if s not in p["exclude"]]
        prots = []
        for s in sel:
            is_dci = s in dci and not p["no_dci"]
            if (s, tvl) not in cache:
                ids, contracts, total = components(cfg, s, tvl)
                if s in dci and ids:
                    contracts |= entry_point_contracts(cfg, s, ids)
                cache[(s, tvl)] = (ids, sorted(contracts), total)
                log(f"  {s} tvl>{tvl}: {len(ids)} components, {len(contracts)} contracts")
            ids, contracts, total = cache[(s, tvl)]
            prots.append({"system": s, "dci": is_dci, "component_pages": max(1, math.ceil(total / p["page_size"])),
                          "component_ids": ids, "contract_ids": contracts if is_dci else []})
        profiles.append({"name": p["name"], "weight": p["weight"], "tvl_gt": tvl, "comp_page_size": p["page_size"], "state_chunk": p["state_chunk"],
                         "contract_chunk": p["contract_chunk"], "ep_chunk": p["ep_chunk"], "sort_contracts": p["sort_contracts"], "protocols": prots})
        log(f"profile {p['name']}: " + ", ".join(f"{k} {v}" for k, v in describe_profile(profiles[-1], token_pages).items()))
    plan = {"chain": chain, "url": cfg["target"]["url"], "user_plan": cfg["target"]["plan_header"], "token_pages": token_pages,
            "plan_floor_tvl": cfg["rules"]["tvl_floor"], "max_version_age_s": cfg["rules"]["max_version_age_s"], "profiles": profiles}
    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    with open(out_path + ".tmp", "w") as f:
        json.dump(plan, f)
    os.replace(out_path + ".tmp", out_path)
    return plan


def describe_profile(p, token_pages):
    """What one startup of this profile requests."""
    comps = sum(len(x["component_ids"]) for x in p["protocols"])
    contracts = sum(len(x["contract_ids"]) for x in p["protocols"])
    pages = sum(x["component_pages"] for x in p["protocols"])
    reqs = token_pages + pages
    for x in p["protocols"]:
        n = len(x["component_ids"])
        reqs += math.ceil(n / p["state_chunk"]) if n else 0
        if x["dci"]:
            reqs += (math.ceil(n / p["ep_chunk"]) if n else 0) + (math.ceil(len(x["contract_ids"]) / p["contract_chunk"]) if x["contract_ids"] else 0)
    return {"protocols": len(p["protocols"]), "components": comps, "contracts": contracts, "component_pages": pages, "requests": reqs}
