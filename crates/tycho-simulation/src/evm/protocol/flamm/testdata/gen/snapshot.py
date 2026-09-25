# Copyright (c) 2026 Everlong Labs Limited
"""Build a ComponentWithState-shaped snapshot of the FLAMM pool at a block, purely from eth_getStorageAt,
eth_getLogs, eth_getCode and eth_call (the views are used only for the static attributes that are immutables,
which the substreams takes from the creation transaction instead).

usage: python3 snapshot.py <block> [more blocks...]      -> out/schema/<block>.json

Shape (tycho-client `ComponentWithState`):
  {"component": ProtocolComponent, "state": ResponseProtocolState{component_id, attributes, balances}, "component_tvl": null}
one entry per component: the swap component (id = pool address) and the lever-up component
(id = pool(20) || 0x00000000 || uint64(1)). Attribute values are hex strings of the bytes the substreams emits:
32-byte storage words for every `<role>:<slot>` / `mm:` / `irm:` attribute, 20 bytes for addresses, 32-byte
big-endian (two's complement for int256) for decoded feed integers, and the packed 32-byte Transmission word for
`feed:mo0:tx:<round>`.
"""
import json
import os
import sys

from rpc import RPC, hexword, keccak256, enc_call, dec_words, to_addr, w256
import schema as S
import reads as R

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out", "schema")
SWAP_ID = S.POOL
LEVER_UP_ID = S.POOL + "00000000" + "0000000000000001"
CREATION_BLOCK = 51154990
INITIAL_BLOCK = 51154966


def b32(x: int) -> str:
    return "0x" + (x % (1 << 256)).to_bytes(32, "big").hex()


ROLE_PREFIX = {"pool": "pool", "hook": "hook", "spread": "spread", "router": "router", "account": "account",
               "feed": "pricefeed", "factory": "factory"}  # PriceFeed storage words are `pricefeed:`; `feed:` is the decoded Chainlink state

# The static attributes a component carries, exactly schema.STATIC (gen_slots.py asserts the two lists agree).
STATIC_KEYS = tuple(S.STATIC_NAMES)


def attr_name(entry):
    if entry["role"] in ROLE_PREFIX:
        return "%s:%s" % (ROLE_PREFIX[entry["role"]], hexword(entry["slot"]))
    return entry["role"]  # mm:*, irm:*


def static_attributes(rpc, block):
    """Immutables and codehashes. The substreams decodes them from the creation transactions / PoolCreated; here
    they are read back through their getters and eth_getCode at `block`."""
    def code_hash(addr):
        return "0x" + keccak256(bytes.fromhex(rpc.code(addr, block)[2:])).hex()

    def call1(to, sig, *args):
        ws = R.call(rpc, to, sig, *args, block=block)
        return None if ws is None else ws[0]

    st = {
        "implementation": to_addr(call1(S.FACTORY, "implementation()")),
        "hook": to_addr(call1(S.POOL, "hooks()")),  # hooks()[0] = invariantHook
        "leverage_hook": S.LEV_HOOK,
        "spread_hook": S.SPREAD_HOOK,
        "router": to_addr(call1(S.POOL, "router()")),
        "price_feed": to_addr(call1(S.POOL, "priceFeed()")),
        "factory": to_addr(call1(S.POOL, "factory()")),
        "pool_asset": to_addr(call1(S.POOL, "asset()")),
        "loan_asset_0": S.USDC,
        "venue_0_account": S.ACCOUNT,
        "venue_0_market_id": S.MARKET_ID,
        "venue_0_morpho": S.MORPHO,
        "venue_0_irm": S.IRM,
        "venue_0_oracle": S.MORPHO_ORACLE,
    }
    st["implementation_codehash"] = code_hash(st["implementation"])
    st["hook_codehash"] = code_hash(S.HOOK)
    st["leverage_hook_codehash"] = code_hash(S.LEV_HOOK)
    st["spread_hook_codehash"] = code_hash(S.SPREAD_HOOK)
    st["router_codehash"] = code_hash(S.ROUTER)
    st["venue_0_account_codehash"] = code_hash(S.ACCOUNT)
    st["irm_codehash"] = code_hash(S.IRM)
    st["hook_loan_scale"] = b32(call1(S.HOOK, "LOAN_SCALE()"))
    st["hook_genesis_strategy_hash"] = hexword(call1(S.HOOK, "genesisStrategyHash()"))
    st["hook_genesis_params_hash"] = hexword(call1(S.HOOK, "genesisParamsHash()"))
    st["leverage_hook_loan_scale"] = b32(call1(S.LEV_HOOK, "LOAN_SCALE()"))
    st["leverage_hook_swap_hook"] = to_addr(call1(S.LEV_HOOK, "HOOK()"))
    st["price_feed_sequencer"] = to_addr(call1(S.PRICE_FEED, "SEQUENCER_FEED()"))
    st["price_feed_sequencer_grace"] = b32(call1(S.PRICE_FEED, "SEQUENCER_GRACE()"))
    st["factory_upgrade_delay"] = b32(call1(S.FACTORY, "UPGRADE_DELAY()"))
    st["venue_0_oracle_scale_factor"] = b32(call1(S.MORPHO_ORACLE, "SCALE_FACTOR()"))
    st["venue_0_oracle_base_feed_1"] = to_addr(call1(S.MORPHO_ORACLE, "BASE_FEED_1()"))
    st["feed_mo0_secondary_proxy"] = S.PROXY_BTC_USD  # DualAggregator i_secondaryProxy (bytecode immutable; base-flamm README, Attributes)
    st["feed_mo0_max_sync_iterations"] = b32(R.MAX_SYNC_ITERATIONS)
    st["feed_asset_proxy"] = to_addr(R.call(rpc, S.PRICE_FEED, "config(address)", S.CBBTC, block=block)[0])
    st["feed_loan0_proxy"] = to_addr(R.call(rpc, S.PRICE_FEED, "config(address)", S.USDC, block=block)[0])
    st["feed_seq_proxy"] = st["price_feed_sequencer"]
    st["feed_mo0_proxy"] = st["venue_0_oracle_base_feed_1"]
    assert set(st) | {"component_kind"} == set(STATIC_KEYS), (set(st) ^ set(STATIC_KEYS))
    return st


def feed_attributes(rpc, block):
    feeds = R.read_feed_storage(rpc, block)
    attrs = {}
    for f in S.FEEDS:
        role = f["role"]
        st = feeds[role]
        p = "feed:%s:" % role
        attrs[p + "aggregator"] = st["aggregator"]
        attrs[p + "phase"] = b32(st["phase_id"])
        # read access (base-flamm README, Attributes): the proxy's accessController, and for a guarded aggregator its checkEnabled
        # and s_accessList[proxy]; the decoder folds schema.feed_read_ok into roundReads.Ok / venueReads.OracleOk
        attrs[p + "access_controller"] = st["access"]["proxy_controller"]
        if st["access"]["guarded"]:
            attrs[p + "check_enabled"] = b32(1 if st["access"]["check_enabled"] else 0)
            attrs[p + "access_list"] = b32(1 if st["access"]["access_list"] else 0)
        if f["kind"] == "ocr2":
            r = st["hotvars"]["latestAggregatorRoundId"]
            t = st["transmissions"][r]
            attrs[p + "round"] = b32(r)
            attrs[p + "answer"] = b32(t["answer"])
            attrs[p + "started_at"] = b32(t["observationsTimestamp"])
            attrs[p + "updated_at"] = b32(t["recordedTimestamp"])
        elif f["kind"] == "dual":
            hv = st["hotvars"]
            attrs[p + "round"] = b32(hv["latestAggregatorRoundId"])
            attrs[p + "secondary_round"] = b32(hv["latestSecondaryRoundId"])
            attrs[p + "cutoff"] = b32(st["cutoffTime"])
            for r, t in sorted(st["transmissions"].items()):
                attrs[p + "tx:%d" % r] = hexword(t["word"])
        else:
            fs = st["feedstate"]
            attrs[p + "round"] = b32(fs["latestRoundId"])
            attrs[p + "answer"] = b32(1 if fs["latestStatus"] else 0)
            attrs[p + "started_at"] = b32(fs["startedAt"])
            attrs[p + "updated_at"] = b32(fs["updatedAt"])
    return attrs, feeds


def balances(words):
    """Component balances = the pool's tradable inventory from tracked words (design 5.1), without the borrow
    capacity term (a port computation the decoder adds).

    cbBTC = gross poolAsset as FLAMMGateLib.grossOf (FLAMMGateLib.sol:166-169) computes it: physicalPoolAsset +
    MMRouterLib.positions(...).totalColl (MMRouterLib.sol:488-507), i.e. the sum over the NON-RETIRED venues of the
    venue's recognized collateral = min(position.collateral, venues[i].managedCollateral) (MMRouterLib.read, :556-563),
    zero for an unreadable venue (tryPosition's IRM accrual reverting past IRM_STALE_GRACE; never on the
    AdaptiveCurveIrm, venueReads.IrmReadable). Not `physical + position.collateral`: donated collateral above
    managedCollateral, or a retired venue, is not gross.
    USDC = loans[0].liquid + the same venues' recognized supplied assets: the full valuation when every share is
    managed, else the managed shares valued (MMRouterLib.read :561-562), at the stored Morpho totals
    (SharesMathLib.toAssetsDown; the accrual to the execution timestamp is the decoder's, morpho.go)."""
    def w(addr, slot):
        return words[(addr, slot)]
    physical = w(S.POOL, S.FLAMM_NS + 12)
    liquid = w(S.POOL, S.POOL_LOANS_BASE + 5)
    m0 = w(S.MORPHO, S.MM_MARKET + 0)
    tsa, tss = m0 & ((1 << 128) - 1), m0 >> 128

    def to_assets_down(shares):
        return shares * (tsa + 1) // (tss + 10 ** 6)

    total_coll = 0
    supplied = 0
    n_venues = w(S.ROUTER, S.ROUTER_RECORD + 3)
    # schema.WORDS carries venue 0 and its Morpho position (mm:0:*); a venue the curator admits later adds its
    # six router words and its own mm:<i>:* words, and this loop extends with them
    venues = [(S.ROUTER_VENUES_BASE, S.MM_POSITION)]
    assert n_venues == len(venues), "venue words not in the schema: %d venues on chain" % n_venues
    for b, pos in venues:
        flags = w(S.ROUTER, b + 2)  # kind @0 | loanIndex @1 | lltvWad @2 | borrowEnabled @10 | supplyEnabled @11 | retired @12
        retired = bool((flags >> (8 * 12)) & 0xff)
        loan_index = (flags >> 8) & 0xff
        if retired:
            continue
        managed_coll = w(S.ROUTER, b + 4)
        managed_shares = w(S.ROUTER, b + 5)
        shares = w(S.MORPHO, pos + 0)
        collateral = w(S.MORPHO, pos + 1) >> 128
        total_coll += min(collateral, managed_coll)
        if loan_index == 0:
            managed = min(shares, managed_shares)
            supplied += to_assets_down(shares) if managed == shares else to_assets_down(managed)
    return {S.CBBTC: hex(physical + total_coll), S.USDC: hex(liquid + supplied)}


def creation_info(rpc):
    logs = rpc.get_logs(S.FACTORY, CREATION_BLOCK, CREATION_BLOCK)
    tx = logs[0]["transactionHash"] if logs else None
    return tx


def snapshot(rpc, block):
    blk = rpc.block(block)
    ts = int(blk["timestamp"], 16)
    words = R.read_words(rpc, block)
    attrs = {}
    for e in S.WORDS:
        if e.get("note", "").startswith("not emitted"):
            continue
        attrs[attr_name(e)] = hexword(words[(e["address"], e["slot"])])
    fattrs, feeds = feed_attributes(rpc, block)
    attrs.update(fattrs)
    static = static_attributes(rpc, block)
    bal = balances(words)
    tx = creation_info(rpc)
    comps = []
    for cid, kind in ((SWAP_ID, 0), (LEVER_UP_ID, 1)):
        st = dict(static)
        st["component_kind"] = b32(kind)
        comps.append({
            "component": {
                "id": cid, "protocol_system": "flamm", "protocol_type_name": "flamm_pool", "chain": "base",
                "tokens": [S.CBBTC, S.USDC],
                "contract_ids": [S.POOL, S.HOOK, S.LEV_HOOK, S.SPREAD_HOOK, S.ROUTER, S.ACCOUNT, S.PRICE_FEED, S.FACTORY],
                "static_attributes": st, "change": "Creation", "creation_tx": tx,
                "created_at": None,
            },
            "state": {"component_id": cid, "attributes": attrs, "balances": bal},
            "component_tvl": None,
            "entrypoints": [],
        })
    return {"schema_version": 1, "block": {"number": block, "hash": blk["hash"], "timestamp": ts},
            "attribute_count": len(attrs), "components": comps,
            "decoded_feeds": _jsonable(feeds)}


def _jsonable(x):
    if isinstance(x, dict):
        return {str(k): _jsonable(v) for k, v in x.items()}
    if isinstance(x, list):
        return [_jsonable(i) for i in x]
    if isinstance(x, int) and not isinstance(x, bool) and x >= 1 << 64:
        return hex(x)
    return x


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    rpc = RPC(verbose=True)
    for b in [int(a) for a in sys.argv[1:]]:
        snap = snapshot(rpc, b)
        path = os.path.join(OUT, "%d.json" % b)
        json.dump(snap, open(path, "w"), indent=1)
        print("wrote", path, "attributes", snap["attribute_count"], "rpc calls so far", rpc.calls, flush=True)
