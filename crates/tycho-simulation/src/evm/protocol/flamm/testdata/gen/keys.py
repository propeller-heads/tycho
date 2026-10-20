# Copyright (c) 2026 Everlong Labs Limited
"""The tracked key sets per role, mirrored from the Rust package (src/flamm/keys.rs)."""
from rpc import keccak256, w256, addr_word, map_slot, array_base

FLAMM_NS = 0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4500
ERC20_NS = 0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00
POOL = "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572"
HOOK = "0x65cbd227cbc61248ae77a5fc813a29c54c092134"
SPREAD = "0x04988af54ec88d2de77b191025eaef2fe488f93b"
ROUTER = "0x19a9b39e6710aad109c829294b0841f0851c6bb4"
ACCOUNT = "0x6760e3b032ee2d670cb684d9076b8f48cb066c48"
PRICEFEED = "0xbed275459578c87a63f2f50a0b077c720e838816"
FACTORY = "0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f"
MORPHO = "0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb"
IRM = "0x46415998764c29ab2a25cbea6254146d50d22687"
CBBTC = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
MARKET = bytes.fromhex("9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836")

MAX_POOL_LOANS = 8      # loans[i] words: 6 per loan
MAX_ROUTER_LOANS = 8    # 7 per loan
MAX_ROUTER_VENUES = 16  # 6 per venue
MAX_ORDER_WORDS = 4     # 16 uint16 per word

def pool_keys():
    ks = [FLAMM_NS + i for i in range(34)]
    base = array_base(FLAMM_NS + 11)
    ks += [base + i for i in range(6 * MAX_POOL_LOANS)]
    ks.append(ERC20_NS + 2)
    return ks

def hook_keys():
    return list(range(32))

def spread_keys():
    return [0, 1]

def account_keys(market_ids):
    ks = list(range(7))
    for mid in market_ids:
        b = map_slot(mid, 5)
        ks += [b, b + 1]
    return ks

def pricefeed_keys(tokens):
    ks = []
    for t in tokens:
        b = map_slot(addr_word(t), 0)
        ks += [b, b + 1]
    return ks

def factory_keys(pool):
    return [0, 1, 2, map_slot(addr_word(pool), 4)]

def router_keys(pool):
    rec = map_slot(addr_word(pool), 1)
    ks = [0] + [rec + i for i in range(8)]
    lb = array_base(rec + 2)
    ks += [lb + i for i in range(7 * MAX_ROUTER_LOANS)]
    vb = array_base(rec + 3)
    ks += [vb + i for i in range(6 * MAX_ROUTER_VENUES)]
    for j in range(4):
        ob = array_base(rec + 4 + j)
        ks += [ob + k for k in range(MAX_ORDER_WORDS)]
    return ks

def morpho_keys(market, account):
    mk = map_slot(market, 3)
    pos = map_slot(addr_word(account), map_slot(market, 2))
    return [mk, mk + 1, mk + 2, pos, pos + 1]

def irm_keys(market):
    return [map_slot(market, 0)]

def all_tracked():
    """(address, slot) for everything the package tracks for the live pool (FLAMM-owned + Morpho + IRM)."""
    out = []
    out += [(POOL, k) for k in pool_keys()]
    out += [(HOOK, k) for k in hook_keys()]
    out += [(SPREAD, k) for k in spread_keys()]
    out += [(ROUTER, k) for k in router_keys(POOL)]
    out += [(ACCOUNT, k) for k in account_keys([MARKET])]
    out += [(PRICEFEED, k) for k in pricefeed_keys([CBBTC, USDC])]
    out += [(FACTORY, k) for k in factory_keys(POOL)]
    out += [(MORPHO, k) for k in morpho_keys(MARKET, ACCOUNT)]
    out += [(IRM, k) for k in irm_keys(MARKET)]
    return out
