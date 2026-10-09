# Copyright (c) 2026 Everlong Labs Limited
"""Shared chain reads for snapshot.py: the storage words of schema.WORDS, the views that report them,
the Chainlink aggregator state (storage and events) and the DualAggregator round selection (DualAggregator.sol
_getLatestRound / _getSyncPrimaryRound, secondary-proxy path)."""
import time
from rpc import (RPC, enc_call, dec_words, to_addr, field, signed, hexword, w256)
import schema as S

MAX_SYNC_ITERATIONS = 20  # DualAggregator i_maxSyncIterations (immutable, bytecode PUSH32 0x14)
RING = MAX_SYNC_ITERATIONS + 1  # rounds latest-20..latest are what _getSyncPrimaryRound may touch


def decode_fields(w: int, entry):
    out = {}
    for f in entry["fields"]:
        v = field(w, f["byte_offset"], f["bytes"])
        t = f["type"]
        if t == "address":
            out[f["name"]] = to_addr(v)
        elif t == "bool":
            out[f["name"]] = bool(v)
        elif t == "bytes32":
            out[f["name"]] = hexword(v)
        elif t.startswith("int"):
            out[f["name"]] = signed(v, int(t[3:]) if t != "int" else 256)
        elif t == "uint16[16]":
            out[f["name"]] = [(v >> (16 * k)) & 0xffff for k in range(16)]
        else:
            out[f["name"]] = v
    return out


def read_words(rpc: RPC, block: int, entries=None):
    entries = entries if entries is not None else S.WORDS
    words = rpc.storage_many([(e["address"], e["slot"]) for e in entries], block)
    return dict(zip([(e["address"], e["slot"]) for e in entries], words))


# ---------------------------------------------------------------- views
def call(rpc, to, sig, *args, block):
    ok, res = rpc.try_call(to, enc_call(sig, *args), block)
    if not ok:
        return None
    return dec_words(res)


def enc_call_dyn(sig: str, addr: str, data: bytes) -> str:
    """ABI-encode (address, bytes): head = address word, offset 0x40; tail = length, padded bytes."""
    from rpc import selector, addr_word
    body = addr_word(addr) + w256(0x40) + w256(len(data)) + data + b"\0" * ((32 - len(data) % 32) % 32)
    return selector(sig) + body.hex()


def call_raw(rpc, to, sig, *args, block):
    ok, res = rpc.try_call(to, enc_call(sig, *args), block)
    return (ok, res)


def dyn_arrays(hexdata, n):
    """Decode n dynamic uint arrays returned together (e.g. MMRouter.priorities)."""
    b = bytes.fromhex(hexdata[2:])
    outs = []
    for i in range(n):
        off = int.from_bytes(b[32 * i:32 * i + 32], "big")
        ln = int.from_bytes(b[off:off + 32], "big")
        outs.append([int.from_bytes(b[off + 32 + 32 * k:off + 64 + 32 * k], "big") for k in range(ln)])
    return outs


def read_views(rpc: RPC, block: int):
    """Every view the words are checked against. Returns {name: [words]} (None where the call reverted)."""
    id_ = S.MARKET_ID
    plan = [
        ("pool.asset", S.POOL, "asset()", ()), ("pool.router", S.POOL, "router()", ()),
        ("pool.priceFeed", S.POOL, "priceFeed()", ()), ("pool.core", S.POOL, "core()", ()),
        ("pool.factory", S.POOL, "factory()", ()), ("pool.paused", S.POOL, "paused()", ()),
        ("pool.switches", S.POOL, "switches()", ()), ("pool.hooks", S.POOL, "hooks()", ()),
        ("pool.pendingHookSet", S.POOL, "pendingHookSet()", ()),
        ("pool.poolAssetPosition", S.POOL, "poolAssetPosition()", ()), ("pool.totalSupply", S.POOL, "totalSupply()", ()),
        ("pool.dials", S.POOL, "dials()", ()), ("pool.limits", S.POOL, "limits()", ()),
        ("pool.loanCount", S.POOL, "loanCount()", ()), ("pool.loanConfig0", S.POOL, "loanConfig(uint8)", (0,)),
        ("pool.loanPosition", S.POOL, "loanPosition()", ()), ("pool.decimals", S.POOL, "decimals()", ()),
        ("hook.params", S.HOOK, "params()", ()), ("hook.support", S.HOOK, "support()", ()),
        ("hook.anchorSqrtX96", S.HOOK, "anchorSqrtX96()", ()), ("hook.reservationPriceWad", S.HOOK, "reservationPriceWad()", ()),
        ("hook.kappa", S.HOOK, "kappa()", ()), ("hook.xWad", S.HOOK, "xWad()", ()),
        ("hook.reserveStable", S.HOOK, "reserveStable()", ()), ("hook.idleStable", S.HOOK, "idleStable()", ()),
        ("hook.reserveVolatile", S.HOOK, "reserveVolatile()", ()), ("hook.idleVolatile", S.HOOK, "idleVolatile()", ()),
        ("hook.rvWad", S.HOOK, "rvWad()", ()), ("hook.LOAN_SCALE", S.HOOK, "LOAN_SCALE()", ()),
        ("hook.POOL", S.HOOK, "POOL()", ()), ("hook.genesisStrategyHash", S.HOOK, "genesisStrategyHash()", ()),
        ("hook.genesisParamsHash", S.HOOK, "genesisParamsHash()", ()),
        ("spread.spread", S.SPREAD_HOOK, "spread()", ()), ("spread.minSpread", S.SPREAD_HOOK, "minSpread()", ()),
        ("spread.maxSpread", S.SPREAD_HOOK, "maxSpread()", ()), ("spread.maxSpreadAge", S.SPREAD_HOOK, "maxSpreadAge()", ()),
        ("spread.lastSetTs", S.SPREAD_HOOK, "lastSetTs()", ()), ("spread.POOL", S.SPREAD_HOOK, "POOL()", ()),
        ("lev.POOL", S.LEV_HOOK, "POOL()", ()), ("lev.HOOK", S.LEV_HOOK, "HOOK()", ()), ("lev.LOAN_SCALE", S.LEV_HOOK, "LOAN_SCALE()", ()),
        ("feed.config.asset", S.PRICE_FEED, "config(address)", (S.CBBTC,)), ("feed.config.loan0", S.PRICE_FEED, "config(address)", (S.USDC,)),
        ("feed.SEQUENCER_FEED", S.PRICE_FEED, "SEQUENCER_FEED()", ()), ("feed.SEQUENCER_GRACE", S.PRICE_FEED, "SEQUENCER_GRACE()", ()),
        ("router.globalPaused", S.ROUTER, "globalPaused()", ()), ("router.pin", S.ROUTER, "pin(address)", (S.POOL,)),
        ("router.maxDrawnAssets", S.ROUTER, "maxDrawnAssets(address)", (S.POOL,)),
        ("router.loanCount", S.ROUTER, "loanCount(address)", (S.POOL,)), ("router.venueCount", S.ROUTER, "venueCount(address)", (S.POOL,)),
        ("router.loan0", S.ROUTER, "loan(address,uint8)", (S.POOL, 0)), ("router.venue0", S.ROUTER, "venue(address,uint16)", (S.POOL, 0)),
        ("router.venuePosition0", S.ROUTER, "venuePosition(address,uint16)", (S.POOL, 0)),
        ("account.ROUTER", S.ACCOUNT, "ROUTER()", ()), ("account.POOL", S.ACCOUNT, "POOL()", ()),
        ("account.POOL_ASSET", S.ACCOUNT, "POOL_ASSET()", ()), ("account.LOAN_ASSET", S.ACCOUNT, "LOAN_ASSET()", ()),
        ("account.MORPHO", S.ACCOUNT, "MORPHO()", ()), ("account.lltv", S.ACCOUNT, "lltv(bytes32)", (id_,)),
        ("account.oraclePrice", S.ACCOUNT, "oraclePrice(bytes32)", (id_,)),
        ("account.positionOf", S.ACCOUNT, "positionOf(bytes32)", (id_,)),
        ("factory.implementation", S.FACTORY, "implementation()", ()), ("factory.pendingImplementation", S.FACTORY, "pendingImplementation()", ()),
        ("factory.implementationExecutableAt", S.FACTORY, "implementationExecutableAt()", ()),
        ("factory.pendingImplementationCodehash", S.FACTORY, "pendingImplementationCodehash()", ()),
        ("factory.isPool", S.FACTORY, "isPool(address)", (S.POOL,)), ("factory.UPGRADE_DELAY", S.FACTORY, "UPGRADE_DELAY()", ()),
        ("factory.ROUTER", S.FACTORY, "ROUTER()", ()),
        ("morpho.market", S.MORPHO, "market(bytes32)", (id_,)), ("morpho.position", S.MORPHO, "position(bytes32,address)", (id_, S.ACCOUNT)),
        ("morpho.idToMarketParams", S.MORPHO, "idToMarketParams(bytes32)", (id_,)),
        ("irm.rateAtTarget", S.IRM, "rateAtTarget(bytes32)", (id_,)),
        ("oracle.price", S.MORPHO_ORACLE, "price()", ()), ("oracle.SCALE_FACTOR", S.MORPHO_ORACLE, "SCALE_FACTOR()", ()),
        ("oracle.BASE_FEED_1", S.MORPHO_ORACLE, "BASE_FEED_1()", ()), ("oracle.BASE_FEED_2", S.MORPHO_ORACLE, "BASE_FEED_2()", ()),
        ("oracle.QUOTE_FEED_1", S.MORPHO_ORACLE, "QUOTE_FEED_1()", ()), ("oracle.QUOTE_FEED_2", S.MORPHO_ORACLE, "QUOTE_FEED_2()", ()),
    ]
    for f in S.FEEDS:
        plan += [("proxy.%s.aggregator" % f["role"], f["proxy"], "aggregator()", ()),
                 ("proxy.%s.phaseId" % f["role"], f["proxy"], "phaseId()", ()),
                 ("proxy.%s.latestRoundData" % f["role"], f["proxy"], "latestRoundData()", ()),
                 ("proxy.%s.decimals" % f["role"], f["proxy"], "decimals()", ()),
                 ("proxy.%s.accessController" % f["role"], f["proxy"], "accessController()", ()),
                 ("agg.%s.checkEnabled" % f["role"], f["aggregator"], "checkEnabled()", ()),
                 ("agg.%s.hasAccess(proxy)" % f["role"], f["aggregator"], "hasAccess(address,bytes)", (f["proxy"], b""))]
        if f["kind"] == "ocr2":
            plan += [("agg.%s.latestRoundData" % f["role"], f["aggregator"], "latestRoundData()", ()),
                     ("agg.%s.latestRound" % f["role"], f["aggregator"], "latestRound()", ())]
    items = [(to, enc_call_dyn(sig, *args) if sig == "hasAccess(address,bytes)" else enc_call(sig, *args)) for _, to, sig, args in plan]
    res = rpc.calls_many(items, block)
    out = {}
    for (name, _, _, _), (ok, r) in zip(plan, res):
        out[name] = dec_words(r) if ok else None
        if ok and name == "router.priorities":
            out[name] = dyn_arrays(r, 4)
    ok, r = rpc.try_call(S.ROUTER, enc_call("priorities(address)", S.POOL), block)
    out["router.priorities"] = dyn_arrays(r, 4) if ok else None
    return out


# ---------------------------------------------------------------- Chainlink storage
def ocr2_hotvars(w):
    """OCR2Aggregator HotVars: f u8 | latestEpochAndRound u40 | latestAggregatorRoundId u32 | billing u32 x4."""
    return {"f": field(w, 0, 1), "latestEpochAndRound": field(w, 1, 5), "latestAggregatorRoundId": field(w, 6, 4)}


def dual_hotvars(w):
    """DualAggregator HotVars (DualAggregator.sol:46-61): f u8 | latestEpochAndRound u40 | latestAggregatorRoundId u32 |
    latestSecondaryRoundId u32 | billing u32 x4 | isLatestSecondary bool."""
    return {"f": field(w, 0, 1), "latestEpochAndRound": field(w, 1, 5), "latestAggregatorRoundId": field(w, 6, 4),
            "latestSecondaryRoundId": field(w, 10, 4), "isLatestSecondary": bool(field(w, 30, 1))}


def transmission(w):
    """Transmission: int192 answer | uint32 observationsTimestamp | uint32 transmissionTimestamp (OCR2) /
    recordedTimestamp (Dual)."""
    return {"answer": signed(field(w, 0, 24), 192), "observationsTimestamp": field(w, 24, 4), "recordedTimestamp": field(w, 28, 4)}


def feedstate(w):
    """OptimismSequencerUptimeFeed FeedState: uint80 latestRoundId | bool latestStatus | uint64 startedAt | uint64 updatedAt."""
    return {"latestRoundId": field(w, 0, 10), "latestStatus": bool(field(w, 10, 1)), "startedAt": field(w, 11, 8), "updatedAt": field(w, 19, 8)}


def read_feed_storage(rpc: RPC, block: int):
    """Per feed: proxy slot 2 (phase), aggregator hot state, and the transmission words the decoder needs."""
    out = {}
    reads = []
    for f in S.FEEDS:
        a = f["access"]
        reads.append((f["proxy"], 2))
        reads.append((f["proxy"], a["proxy_slot"]))
        reads.append((f["aggregator"], a["check_enabled"][0]))
        reads.append((f["aggregator"], a["access_list"]))
        if f["kind"] in ("ocr2", "dual"):
            reads.append((f["aggregator"], f["hotvars"]))
        if f["kind"] == "dual":
            reads.append((f["aggregator"], f["cutoff"]))
        if f["kind"] == "uptime":
            reads.append((f["aggregator"], f["feedstate"]))
    words = dict(zip(reads, rpc.storage_many(reads, block)))
    for f in S.FEEDS:
        a = f["access"]
        d = {"proxy_slot2": words[(f["proxy"], 2)]}
        d["phase_id"] = field(d["proxy_slot2"], 0, 2)
        d["aggregator"] = to_addr(field(d["proxy_slot2"], 2, 20))
        # read access (schema.ACCESS): the words, decoded, and the rule the decoder applies
        ce_word = words[(f["aggregator"], a["check_enabled"][0])]
        al_word = words[(f["aggregator"], a["access_list"])]
        pc_word = words[(f["proxy"], a["proxy_slot"])]
        d["access"] = {"proxy_controller_word": pc_word, "check_enabled_word": ce_word, "access_list_word": al_word,
                       "proxy_controller": to_addr(field(pc_word, 0, 20)),
                       "check_enabled": bool(field(ce_word, a["check_enabled"][1], 1)),
                       "access_list": bool(field(al_word, 0, 1)), "guarded": a["guarded"]}
        d["access"]["read_ok"] = S.feed_read_ok(a, field(pc_word, 0, 20), d["access"]["check_enabled"], d["access"]["access_list"])
        if f["kind"] == "ocr2":
            hv = ocr2_hotvars(words[(f["aggregator"], f["hotvars"])])
            d["hotvars"] = hv
            r = hv["latestAggregatorRoundId"]
            slot = S.ocr2_transmission_slot(r)
            tw = rpc.storage_at(f["aggregator"], slot, block)
            d["transmissions"] = {r: {"slot": slot, "word": tw, **transmission(tw)}}
        elif f["kind"] == "dual":
            hv = dual_hotvars(words[(f["aggregator"], f["hotvars"])])
            d["hotvars"] = hv
            d["cutoffTime"] = field(words[(f["aggregator"], f["cutoff"])], 0, 4)
            latest = hv["latestAggregatorRoundId"]
            rounds = sorted(set([r for r in range(max(1, latest - MAX_SYNC_ITERATIONS), latest + 1)] + [hv["latestSecondaryRoundId"]]))
            slots = [(f["aggregator"], S.dual_transmission_slot(r)) for r in rounds]
            tws = rpc.storage_many(slots, block)
            d["transmissions"] = {r: {"slot": s[1], "word": tw, **transmission(tw)} for r, s, tw in zip(rounds, slots, tws)}
        else:
            d["feedstate"] = feedstate(words[(f["aggregator"], f["feedstate"])])
        out[f["role"]] = d
    return out


def dual_latest_round_secondary(hv, cutoff, transmissions, now):
    """DualAggregator._getLatestRound for msg.sender == i_secondaryProxy (DualAggregator.sol:552-568), then
    _getSyncPrimaryRound (:529-548). `transmissions` maps roundId -> {recordedTimestamp}."""
    sec = hv["latestSecondaryRoundId"]
    if transmissions[sec]["recordedTimestamp"] + cutoff < now:
        latest = hv["latestAggregatorRoundId"]
        r = latest
        while r > 0:
            if latest - r == MAX_SYNC_ITERATIONS:
                break
            if transmissions[r]["recordedTimestamp"] + cutoff < now:
                return r
            r -= 1
        return sec
    return sec


def proxy_round_id(phase_id, agg_round):
    """EACAggregatorProxy.addPhase: (phaseId << 64) | aggregatorRoundId."""
    return (phase_id << 64) | agg_round


def feed_latest_round_data(role_state, kind, now):
    """What proxy.latestRoundData() answers at `now`, rebuilt from the tracked feed state."""
    ph = role_state["phase_id"]
    if kind == "ocr2":
        r = role_state["hotvars"]["latestAggregatorRoundId"]
        t = role_state["transmissions"][r]
        return [proxy_round_id(ph, r), t["answer"], t["observationsTimestamp"], t["recordedTimestamp"], proxy_round_id(ph, r)]
    if kind == "dual":
        r = dual_latest_round_secondary(role_state["hotvars"], role_state["cutoffTime"], role_state["transmissions"], now)
        t = role_state["transmissions"][r]
        return [proxy_round_id(ph, r), t["answer"], t["observationsTimestamp"], t["recordedTimestamp"], proxy_round_id(ph, r)]
    fs = role_state["feedstate"]
    rid = proxy_round_id(ph, fs["latestRoundId"])
    return [rid, 1 if fs["latestStatus"] else 0, fs["startedAt"], fs["updatedAt"], rid]


# ---------------------------------------------------------------- events
def logs_chunked(rpc, addr, a, b, topics=None, step=2000):
    """mainnet.base.org limits eth_getLogs to a 2,000-block range."""
    out = []
    s = a
    while s <= b:
        e = min(b, s + step - 1)
        out += rpc.get_logs(addr, s, e, topics)
        s = e + 1
        time.sleep(0.2)
    return out


def decode_new_transmission(log):
    """NewTransmission(uint32 indexed aggregatorRoundId, int192 answer, address transmitter, uint32 observationsTimestamp,
    int192[] observations, bytes observers, [int192 juelsPerFeeCoin,] bytes32 configDigest, uint40 epochAndRound)."""
    ws = dec_words(log["data"])
    return {"roundId": int(log["topics"][1], 16), "answer": signed(ws[0], 256), "transmitter": to_addr(ws[1]),
            "observationsTimestamp": ws[2], "block": int(log["blockNumber"], 16), "tx": log["transactionHash"]}


def decode_answer_updated(log):
    """AnswerUpdated(int256 indexed current, uint256 indexed roundId, uint256 updatedAt)."""
    return {"answer": signed(int(log["topics"][1], 16), 256), "roundId": int(log["topics"][2], 16),
            "updatedAt": dec_words(log["data"])[0], "block": int(log["blockNumber"], 16), "tx": log["transactionHash"]}


def decode_new_round(log):
    """NewRound(uint256 indexed roundId, address indexed startedBy, uint256 startedAt)."""
    return {"roundId": int(log["topics"][1], 16), "startedBy": to_addr(int(log["topics"][2], 16)),
            "startedAt": dec_words(log["data"])[0], "block": int(log["blockNumber"], 16)}


def decode_round_updated(log):
    """RoundUpdated(int256 status, uint64 updatedAt)."""
    ws = dec_words(log["data"])
    return {"status": signed(ws[0], 256), "updatedAt": ws[1], "block": int(log["blockNumber"], 16)}


def rebuild_sequencer_state(rpc, block, span=6000, started_at_hint=None):
    """OptimismSequencerUptimeFeed.s_feedState at `block`, from events only, by the contract's rule:
    `_recordRound` (status change) writes {roundId, status, startedAt = the L1 timestamp the message carries,
    updatedAt = block.timestamp} and emits NewRound + AnswerUpdated(status, roundId, startedAt); `_updateRound`
    (same-status refresh) writes updatedAt = block.timestamp and emits RoundUpdated(status, updatedAt).
    Searches backwards from `block` for the last AnswerUpdated and the last RoundUpdated (RoundUpdated comes
    about daily, AnswerUpdated on an outage). Returns (state, evidence)."""
    agg = S.AGG_SEQ
    t_au = S.TOPICS["AnswerUpdated(int256,uint256,uint256)"]
    t_ru = S.TOPICS["RoundUpdated(int256,uint64)"]

    def last_log(topic, lo_limit):
        hi = block
        while hi >= lo_limit:
            lo = max(lo_limit, hi - span + 1)
            logs = logs_chunked(rpc, agg, lo, hi, [topic])
            if logs:
                return logs[-1]
            hi = lo - 1
        return None

    au_log = None
    if started_at_hint is not None:
        # the L2 block that relayed the status change has timestamp >= the L1 startedAt it carries and lands a few
        # blocks after the block whose timestamp equals it (2 s blocks): search +-span around that estimate first
        blk_ts = int(rpc.block(block)["timestamp"], 16)
        est = block - (blk_ts - started_at_hint) // 2
        logs = logs_chunked(rpc, agg, max(0, est - span), min(block, est + span), [t_au])
        au_log = logs[-1] if logs else None
    if au_log is None:
        au_log = last_log(t_au, block - 4_000_000)  # an outage within the last ~3 months; enough for every block here
    if au_log is None:
        return None, {"error": "no AnswerUpdated found"}
    au = decode_answer_updated(au_log)
    au_ts = int(rpc.block(au["block"])["timestamp"], 16)
    ru_log = last_log(t_ru, au["block"])  # a RoundUpdated at or after the AnswerUpdated's block
    ru = decode_round_updated(ru_log) if ru_log else None
    after = ru is not None and (ru["block"], int(ru_log["logIndex"], 16)) > (au["block"], int(au_log["logIndex"], 16))
    state = {"latestRoundId": au["roundId"], "latestStatus": au["answer"] == 1, "startedAt": au["updatedAt"],
             "updatedAt": ru["updatedAt"] if after else au_ts}
    return state, {"AnswerUpdated": au, "AnswerUpdated_block_timestamp": au_ts, "RoundUpdated_after": ru if after else None,
                   "rule": "startedAt = AnswerUpdated.updatedAt; updatedAt = RoundUpdated.updatedAt if one followed the AnswerUpdated, else the AnswerUpdated block's timestamp"}


def decode_secondary_round(log):
    """SecondaryRoundIdUpdated(uint32 indexed secondaryRoundId)."""
    return {"secondaryRoundId": int(log["topics"][1], 16), "block": int(log["blockNumber"], 16), "tx": log["transactionHash"]}


def decode_aggregator_confirmed(log):
    """AggregatorConfirmed(address indexed previous, address indexed latest) -- defined by newer AggregatorProxy
    versions only; the Base proxies never emit it (rotations.py)."""
    return {"previous": to_addr(int(log["topics"][1], 16)), "latest": to_addr(int(log["topics"][2], 16)),
            "block": int(log["blockNumber"], 16), "tx": log["transactionHash"]}
