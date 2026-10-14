#!/usr/bin/env python3
"""Builds a startup-loadgen plan per chain: client profiles derived from a census of real
requests on the prod hosts (protocol set, TVL threshold implied by the pages fetched, page and
chunk sizes), each kept inside the prod plan rules for the main instances: tvl_gt >= 0.1, version at
most 10 minutes old, no protocol allowlist.

usage: plan2.py <chain>   -> load/plan2_<chain>.json
"""
import concurrent.futures as cf, json, math, os, sys, urllib.request
from chain import L, SIDE, auth_headers, env, host, post

PLAN_FLOOR_TVL = 0.1          # prod `default` plan on the main instances: component_tvl >= 0.1
MAX_VERSION_AGE_S = 600       # prod `default` plan: max_version_age_minutes 10
BLOCK_TIME_S = {"ethereum": 12.0, "bsc": 0.45, "base": 2.0, "robinhood": 0.1}

# Fynd's own thresholds in native units (prod helm values) and its protocol exclusions.
FYND_TVL = {"ethereum": 5.0, "bsc": 2.0, "base": 0.5, "robinhood": 0.5}
FYND_EXCLUDE = {"ethereum": {"vm:balancer_v2", "vm:balancer_v3", "vm:liquidityparty"}, "base": {"lunarbase"}}

# Profiles per chain. weight = roughly the share of such clients in the census (24 h).
#  comp_page_size / state_chunk / contract_chunk / ep_chunk are the request sizes seen in the logs.
#  "all" = every protocol system the chain serves; a list = that subset.
PROFILES = {
    "ethereum": [
        # Fynd: tycho-client with contract chunks of 100, Fynd's TVL filter, every protocol it routes.
        dict(name="fynd", weight=3, tvl=FYND_TVL["ethereum"] / 1.1, systems="fynd", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # Most solvers: old tycho-client (contract chunks of 1200), high TVL, one components page per protocol, hooks + curve included.
        dict(name="solver_old_client", weight=5, tvl=10.0, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=1200, ep_chunk=100),
        # A solver on 7 protocols with components pages of 500, old client.
        dict(name="solver_pages_500", weight=1, tvl=10.0, systems=["uniswap_v4_hooks", "vm:curve", "ekubo_v2", "ekubo_v3", "vm:maverick_v2", "uniswap_v4", "uniswap_v3"], comp_page_size=500, state_chunk=360, contract_chunk=1200, ep_chunk=100),
        # Frontends: everything, at the plan floor, pages of 500, small chunks, own id order.
        dict(name="low_tvl_broad", weight=2, tvl=PLAN_FLOOR_TVL, systems="all", comp_page_size=500, state_chunk=100, contract_chunk=10, ep_chunk=100, sort_contracts=False),
        # Native-only clients: no entry points or contract state.
        dict(name="state_only", weight=1, tvl=1.0, systems=["uniswap_v2", "uniswap_v3", "uniswap_v4", "pancakeswap_v2", "sushiswap_v2", "pancakeswap_v3"], comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100, no_dci=True),
    ],
    "bsc": [
        dict(name="fynd", weight=2, tvl=FYND_TVL["bsc"] / 1.1, systems="fynd", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # An arbitrage bot: pages of 500, state chunks of 100, 121 pages of pancakeswap_v2.
        dict(name="low_tvl_small_pages", weight=2, tvl=0.5, systems="all", comp_page_size=500, state_chunk=100, contract_chunk=100, ep_chunk=100),
        # Solvers reading 19-24 pages of pancakeswap_v2 at 2550.
        dict(name="low_tvl", weight=3, tvl=1.0, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # A solver reading 16 pages, state chunks of 360.
        dict(name="mid_tvl", weight=2, tvl=2.0, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # High-TVL solvers: one or two pages per protocol.
        dict(name="high_tvl", weight=3, tvl=10.0, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # A client paging with 100.
        dict(name="tiny_pages", weight=1, tvl=5.0, systems="all", comp_page_size=100, state_chunk=360, contract_chunk=100, ep_chunk=100),
    ],
    "robinhood": [
        # Fynd: everything incl. hooks (its key lands on the default plan), Fynd's TVL filter.
        dict(name="fynd", weight=2, tvl=FYND_TVL["robinhood"] / 1.1, systems="fynd", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # Most solvers: all protocols incl. hooks, one page each, then poll state in 360s and entry points in 100s.
        dict(name="solver_hooks", weight=4, tvl=5.0, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # Same shape but 5 pages of hooks at 2550 = near the plan floor; the heaviest entry-point reader (80k/day).
        dict(name="solver_floor_hooks", weight=1, tvl=PLAN_FLOOR_TVL, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # A frontend: 7 pages of uniswap_v4, 11 of uniswap_v3, 13 x 500 of hooks: the broadest client at the floor.
        dict(name="low_tvl_broad", weight=1, tvl=PLAN_FLOOR_TVL, systems="all", comp_page_size=500, state_chunk=360, contract_chunk=100, ep_chunk=100, sort_contracts=False),
        # Native-only clients: no entry points.
        dict(name="state_only", weight=2, tvl=1.0, systems=["uniswap_v2", "uniswap_v3", "uniswap_v4", "ekubo_v3", "ramses_v3", "up_v3"], comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100, no_dci=True),
        # A solver on v4/v3/v2 only, pages of 500 with ids, state in chunks of 100 (and 70k single-id polls a day).
        dict(name="solver_pages_500", weight=1, tvl=10.0, systems=["uniswap_v4", "uniswap_v3", "uniswap_v2"], comp_page_size=500, state_chunk=100, contract_chunk=100, ep_chunk=100, no_dci=True),
    ],
    "base": [
        dict(name="fynd", weight=2, tvl=FYND_TVL["base"] / 1.1, systems="fynd", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # A client reading 6 pages of uniswap_v3, everything, at the floor.
        dict(name="low_tvl_broad", weight=2, tvl=PLAN_FLOOR_TVL, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # Most solvers: one page per protocol.
        dict(name="high_tvl", weight=5, tvl=5.0, systems="all", comp_page_size=2550, state_chunk=360, contract_chunk=100, ep_chunk=100),
        # A solver on slipstreams only, pages of 500.
        dict(name="solver_pages_500", weight=1, tvl=10.0, systems=["aerodrome_slipstreams"], comp_page_size=500, state_chunk=360, contract_chunk=100, ep_chunk=100),
    ],
}


try:
    import zstandard
except ImportError:          # no zstd: plain responses, so pages are capped at 500 (slower plan build, same plan)
    zstandard = None
FETCH_PAGE = 2550 if zstandard else 500


def api(chain, path, body):
    """Like chain.post but compressed, which is what lifts the page-size cap from 500 to 2550."""
    headers = {**auth_headers(), "Content-Type": "application/json", "User-Agent": "startup-load-bench/1"}
    if zstandard:
        headers["Accept-Encoding"] = "zstd"
    req = urllib.request.Request(host(chain, SIDE) + path, data=json.dumps(body).encode(), method="POST", headers=headers)
    with urllib.request.urlopen(req, timeout=300) as r:
        raw = r.read()
        if r.headers.get("Content-Encoding") == "zstd":
            raw = zstandard.ZstdDecompressor().decompressobj().decompress(raw)
    return json.loads(raw)


def all_systems(chain):
    ps = api(chain, "/v1/protocol_systems", {"chain": chain, "pagination": {"page": 0, "page_size": 100}})
    # Robinhood lists no dci_protocols, yet uniswap_v4_hooks is traced there (entry-point requests all day).
    dci = set(ps.get("dci_protocols", [])) | {s for s in ps["protocol_systems"] if s == "uniswap_v4_hooks"}
    return sorted(ps["protocol_systems"]), dci


def components(chain, system, tvl_gt, page_size):
    """All component ids and their contract ids above tvl_gt, paged like a client would."""
    ids, contracts = [], set()
    page = 0
    while True:
        r = api(chain, "/v1/protocol_components", {"chain": chain, "protocol_system": system, "tvl_gt": tvl_gt,
                                                   "pagination": {"page": page, "page_size": page_size}})
        for c in r["protocol_components"]:
            ids.append(c["id"]); contracts.update(c.get("contract_ids", []))
        total = r["pagination"]["total"]
        page += 1
        if page * page_size >= total or not r["protocol_components"]:
            break
    return ids, contracts, total


def entry_point_contracts(chain, system, ids, chunk=100):
    out = set()
    def one(c):
        r = api(chain, "/v1/traced_entry_points", {"chain": chain, "protocol_system": system, "component_ids": c,
                                                   "pagination": {"page": 0, "page_size": chunk}})
        s = set()
        for traces in r.get("traced_entry_points", {}).values():
            for pair in traces:
                res = pair[1] if isinstance(pair, list) else pair.get("tracing_result", {})
                s.update(res.get("accessed_slots", {}).keys())
        return s
    with cf.ThreadPoolExecutor(4) as ex:
        for s in ex.map(one, [ids[i:i + chunk] for i in range(0, len(ids), chunk)]):
            out |= s
    return out


def build(chain):
    systems, dci = all_systems(chain)
    fynd_systems = [s for s in systems if s not in FYND_EXCLUDE.get(chain, set())]
    first = api(chain, "/v1/tokens", {"chain": chain, "min_quality": 100, "traded_n_days_ago": 30, "pagination": {"page": 0, "page_size": 1}})
    token_pages = max(1, math.ceil(first["pagination"]["total"] / 12900))
    cache = {}
    profiles = []
    for spec in PROFILES[chain]:
        tvl = max(spec["tvl"], PLAN_FLOOR_TVL)
        sel = systems if spec["systems"] == "all" else fynd_systems if spec["systems"] == "fynd" else [s for s in spec["systems"] if s in systems]
        prots = []
        for s in sel:
            key = (s, tvl)
            if key not in cache:
                ids, contracts, total = components(chain, s, tvl, FETCH_PAGE)
                if s in dci and not spec.get("no_dci") and ids:
                    contracts |= entry_point_contracts(chain, s, ids)
                cache[key] = (ids, sorted(contracts), total)
                print(f"  {chain} {s} tvl>{tvl}: {len(ids)} components, {len(contracts)} contracts", flush=True)
            ids, contracts, total = cache[key]
            is_dci = s in dci and not spec.get("no_dci")
            prots.append({"system": s, "dci": is_dci, "component_pages": max(1, math.ceil(total / spec["comp_page_size"])),
                          "component_ids": ids, "contract_ids": contracts if is_dci else []})
        profiles.append({"name": spec["name"], "weight": spec["weight"], "tvl_gt": tvl, "comp_page_size": spec["comp_page_size"],
                         "state_chunk": spec["state_chunk"], "contract_chunk": spec["contract_chunk"], "ep_chunk": spec["ep_chunk"],
                         "sort_contracts": spec.get("sort_contracts", True), "protocols": prots})
        print(f"profile {spec['name']}: {len(prots)} protocols, {sum(len(p['component_ids']) for p in prots)} components, {sum(len(p['contract_ids']) for p in prots)} contracts, {sum(p['component_pages'] for p in prots)} pages", flush=True)
    plan = {"chain": chain, "url": host(chain, SIDE), "token_pages": token_pages, "plan_floor_tvl": PLAN_FLOOR_TVL,
            "max_version_age_s": MAX_VERSION_AGE_S, "block_time_s": BLOCK_TIME_S[chain], "profiles": profiles}
    os.makedirs(L, exist_ok=True)
    json.dump(plan, open(f"{L}/plan2_{chain}.json", "w"))
    return plan


if __name__ == "__main__":
    build(sys.argv[1])
