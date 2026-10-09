# Copyright (c) 2026 Everlong Labs Limited
"""The storage words the base-flamm package tracks for the live pool, mirrored from src/flamm/keys.rs and
src/flamm/feeds.rs: the FLAMM-owned key sets, the Morpho/IRM words and the Chainlink words `feed_state` reads
(the proxies' phase and access-controller words, the aggregators' hot words, read-access pairs and the
transmissions of the rounds in the DualAggregator window)."""
import keys as K
from rpc import map_slot, w256, addr_word, field

PROXIES = {"asset": "0x07da0e54543a844a80abe69c8a12f22b3aa59f9d", "loan0": "0x7e860098f58bbfc8648a4311b374b1d669a2bc6b",
           "seq": "0xbcf85224fc0756b9fa45aa7892530b47e10b6433", "mo0": "0x64c911996d3c6ac71f9b455b1e8e7266bcbd848f"}
# the aggregator each proxy fronts for the whole range (asserted per stage: no rotation happened)
FRONTS = {"asset": "0x51ce3091cf646587e02cad83b580992f8723e718", "loan0": "0x68be4c50235205ede361ac8244b1ee221cdda5e2",
          "seq": "0x606c6ecbd272e2174f6710b5974f23fe9899602e", "mo0": "0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1"}
# manifest `aggregators`: address -> kind
AGGREGATORS = {"0x51ce3091cf646587e02cad83b580992f8723e718": "ocr2", "0x68be4c50235205ede361ac8244b1ee221cdda5e2": "ocr2",
               "0x606c6ecbd272e2174f6710b5974f23fe9899602e": "uptime", "0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1": "dual"}
DUAL_RING = 20


def transmission_key(round_, base):
    return map_slot(w256(round_), base)


def access_list_key(proxy, base):
    return map_slot(addr_word(proxy), base)


def static_keys():
    """(address, slot) of every word read at every block: the FLAMM-owned sets, Morpho/IRM, the proxies' words and
    the aggregators' fixed words (hot words, read-access pairs, cutoff)."""
    out = list(K.all_tracked())
    for role, proxy in PROXIES.items():
        out += [(proxy, 2), (proxy, 5)]
    for role, agg in FRONTS.items():
        kind = AGGREGATORS[agg]
        proxy = PROXIES[role]
        if kind == "ocr2":
            out += [(agg, 11), (agg, 21), (agg, access_list_key(proxy, 22))]
        elif kind == "uptime":
            out += [(agg, 4), (agg, 1), (agg, access_list_key(proxy, 2))]
        elif kind == "dual":
            out += [(agg, 13), (agg, 18)]
    return out


def check_fronts(state):
    """Every proxy still fronts the aggregator the manifest seeds (no rotation inside the range)."""
    for role, proxy in PROXIES.items():
        agg = "0x%040x" % ((state[(proxy, 2)] >> 16) & ((1 << 160) - 1))
        assert agg == FRONTS[role], (role, agg)


def round_keys(state):
    """The round-keyed words the fixed words point at: `s_transmissions[latest]` of each OCR2 aggregator and the
    DualAggregator window `latest-20..=latest` plus the secondary round (feeds.rs `dual_ring`)."""
    out = []
    for agg, kind in AGGREGATORS.items():
        if kind == "ocr2":
            hot = state.get((agg, 11), 0)
            latest = field(hot, 6, 4)
            if latest:
                out.append((agg, transmission_key(latest, 12)))
        elif kind == "dual":
            hot = state.get((agg, 13), 0)
            latest, secondary = field(hot, 6, 4), field(hot, 10, 4)
            rounds = list(range(max(latest - DUAL_RING, 1), latest + 1)) if latest else []
            if secondary and secondary not in rounds:
                rounds.append(secondary)
            for r in rounds:
                out.append((agg, transmission_key(r, 17)))
    return out


def read_universe(rpc, block):
    """{(address, slot): value} at `block`: the static words, then the round-keyed words they point at."""
    ks = static_keys()
    vals = rpc.storage_many(ks, block)
    state = dict(zip(ks, vals))
    rk = round_keys(state)
    if rk:
        vals = rpc.storage_many(rk, block)
        state.update(dict(zip(rk, vals)))
    return state
