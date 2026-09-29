# Copyright (c) 2026 Everlong Labs Limited
"""The FLAMM storage schema: every state word the native Tycho simulator needs, where it lives on chain, how it
is packed, which Go simulator field it feeds and which attribute the substreams emits for it.

Layouts come from `forge inspect` on c104 @ 80abd43 (layouts/*.json, FLAMMStore.S via test/tycho/LayoutProbe.sol)
and, for the external contracts, from their verified sources (Morpho Blue v1.0.0, AdaptiveCurveIrm, Chainlink
EACAggregatorProxy / OCR2Aggregator / DualAggregator 1.0.0 / OptimismSequencerUptimeFeed), each confirmed against
eth_getStorageAt + the view that reports the field (`reads.read_views`).

Attribute naming (the base-flamm README, section Attributes):
  raw words     "<role>:<slot-key-hex>"     FLAMM-owned storage, value = the 32-byte word as stored
                (role = pool | hook | spread | router | account | pricefeed | factory)
  raw words     "mm:<v>:market:<k>", "mm:<v>:position:<k>", "irm:<v>:rate_at_target"   Morpho / IRM words
  decoded       "feed:<f>:..."              Chainlink / sequencer state rebuilt from events
"""
from rpc import keccak256, w256, addr_word, map_slot, array_base

# ---------------------------------------------------------------- addresses (Base 8453, c104.8453.json)
POOL = "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572"
IMPL = "0xaad580beaa2cbd8ab5f3956a5c56eda1d5ee7184"
FACTORY = "0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f"
HOOK = "0x65cbd227cbc61248ae77a5fc813a29c54c092134"
LEV_HOOK = "0xe0a98d8e60035832b8bad7f7af7b9b0b3a7308f3"
SPREAD_HOOK = "0x04988af54ec88d2de77b191025eaef2fe488f93b"
PRICE_FEED = "0xbed275459578c87a63f2f50a0b077c720e838816"
ROUTER = "0x19a9b39e6710aad109c829294b0841f0851c6bb4"
ACCOUNT = "0x6760e3b032ee2d670cb684d9076b8f48cb066c48"
CORE = "0xf39b775926b876768f215489de4f42703f153fd1"
MORPHO = "0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb"
MARKET_ID = "0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836"
IRM = "0x46415998764c29ab2a25cbea6254146d50d22687"
MORPHO_ORACLE = "0x663becd10dae6c4a3dcd89f1d76c1174199639b9"
CBBTC = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"

PROXY_CBBTC_USD = "0x07da0e54543a844a80abe69c8a12f22b3aa59f9d"
PROXY_USDC_USD = "0x7e860098f58bbfc8648a4311b374b1d669a2bc6b"
PROXY_BTC_USD = "0x64c911996d3c6ac71f9b455b1e8e7266bcbd848f"
PROXY_SEQ = "0xbcf85224fc0756b9fa45aa7892530b47e10b6433"
# the aggregators behind the proxies at the pool's activation and today (proxy slot 2, `reads.read_feed_storage`)
AGG_CBBTC_USD = "0x51ce3091cf646587e02cad83b580992f8723e718"  # AccessControlledOCR2Aggregator, phase 2
AGG_USDC_USD = "0x68be4c50235205ede361ac8244b1ee221cdda5e2"   # AccessControlledOCR2Aggregator, phase 3
AGG_BTC_USD = "0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1"    # DualAggregator 1.0.0 (SVR), phase 3
AGG_SEQ = "0x606c6ecbd272e2174f6710b5974f23fe9899602e"        # OptimismSequencerUptimeFeed, phase 1

MULTICALL3 = "0xca11bde05977b3631167028862be2a173976ca11"

# ---------------------------------------------------------------- namespace bases
FLAMM_NS = 0x5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4500  # FLAMMStore.sol:315
ERC20_NS = 0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00  # OZ ERC20Upgradeable v5


def check_ns():
    """Both ERC-7201 bases: keccak256(abi.encode(uint256(keccak256(id)) - 1)) & ~0xff."""
    def ns(s):
        h = int.from_bytes(keccak256(s.encode()), "big") - 1
        return int.from_bytes(keccak256(w256(h)), "big") & ~0xff
    assert ns("everlong.storage.FLAMM") == FLAMM_NS, hex(ns("everlong.storage.FLAMM"))
    assert ns("openzeppelin.storage.ERC20") == ERC20_NS


check_ns()

# MMRouter: `mapping(address pool => PoolRecord) _pools` at slot 1 (MMRouter.sol:25)
ROUTER_RECORD = map_slot(addr_word(POOL), 1)
ROUTER_LOANS_BASE = array_base(ROUTER_RECORD + 2)     # Loan[]  data, 7 words per element
ROUTER_VENUES_BASE = array_base(ROUTER_RECORD + 3)    # Venue[] data, 6 words per element
ROUTER_ORDER_BASE = [array_base(ROUTER_RECORD + 4 + j) for j in range(4)]  # uint16[] data, 16 per word
# FLAMMStore.S.loans (LoanCfg[]) at namespace +11, 6 words per element
POOL_LOANS_BASE = array_base(FLAMM_NS + 11)
# MorphoBlueAccount `mapping(bytes32 id => Market) _markets` at slot 5
ACCOUNT_MARKET = map_slot(bytes.fromhex(MARKET_ID[2:]), 5)
# PriceFeed `mapping(address => Token) _tokens` at slot 0
FEED_TOKEN = {CBBTC: map_slot(addr_word(CBBTC), 0), USDC: map_slot(addr_word(USDC), 0)}
# FLAMMFactory `mapping(address => bool) isPool` at slot 4
FACTORY_ISPOOL = map_slot(addr_word(POOL), 4)
# Morpho Blue: position at slot 2 (nested), market at slot 3, idToMarketParams at slot 8
MM_MARKET = map_slot(bytes.fromhex(MARKET_ID[2:]), 3)
MM_POSITION = map_slot(addr_word(ACCOUNT), map_slot(bytes.fromhex(MARKET_ID[2:]), 2))
MM_PARAMS = map_slot(bytes.fromhex(MARKET_ID[2:]), 8)
# AdaptiveCurveIrm: `mapping(Id => int256) rateAtTarget` at slot 0
IRM_RATE = map_slot(bytes.fromhex(MARKET_ID[2:]), 0)


def ocr2_transmission_slot(round_id: int) -> int:
    """OCR2Aggregator `mapping(uint32 => Transmission) s_transmissions` at slot 12."""
    return map_slot(w256(round_id), 12)


def dual_transmission_slot(round_id: int) -> int:
    """DualAggregator `mapping(uint32 => Transmission) s_transmissions` at slot 17."""
    return map_slot(w256(round_id), 17)


# ---------------------------------------------------------------- word table
# Each entry: role, address, slot, derivation, fields=[(name, type, byte_offset, nbytes, go_field, cite)], kind
# kind: "dynamic" (changes on chain, streamed), "config" (written once/rarely, streamed anyway), "ceremony"
#       (pending governance words the sim refuses to quote across).
WORDS = []


def word(role, addr, slot, derivation, fields, kind="dynamic", note=""):
    WORDS.append({"role": role, "address": addr, "slot": slot, "derivation": derivation, "fields": fields,
                  "kind": kind, "note": note})


F = lambda name, typ, off, n, go, cite: {"name": name, "type": typ, "byte_offset": off, "bytes": n,  # noqa: E731
                                         "go_field": go, "solidity": cite}

# ---- pool (FLAMMStore.S at FLAMM_NS; layouts/FLAMMStoreLayoutProbe.json)
S = "FLAMMStore.sol"
word("pool", POOL, FLAMM_NS + 0, "FLAMM_NS+0", [F("poolAsset", "address", 0, 20, "poolReads.Asset", S + ":248")], "config")
word("pool", POOL, FLAMM_NS + 1, "FLAMM_NS+1", [F("router", "address", 0, 20, "poolReads.Router", S + ":249")], "config")
word("pool", POOL, FLAMM_NS + 2, "FLAMM_NS+2", [F("priceFeed", "address", 0, 20, "poolReads.PriceFeed", S + ":250")], "config")
word("pool", POOL, FLAMM_NS + 3, "FLAMM_NS+3", [F("core", "address", 0, 20, "(auth only: spread hook keeper)", S + ":251")], "config")
word("pool", POOL, FLAMM_NS + 5, "FLAMM_NS+5", [F("factory", "address", 0, 20, "snapshot.Factory (drift)", S + ":253")], "config")
word("pool", POOL, FLAMM_NS + 7, "FLAMM_NS+7", [F("poolDecimals", "uint8", 0, 1, "(unused by quote)", S + ":255"),
                                               F("invariantHook", "address", 1, 20, "poolReads.Hooks[0]", S + ":256")], "config")
word("pool", POOL, FLAMM_NS + 8, "FLAMM_NS+8", [F("feeHook", "address", 0, 20, "poolReads.Hooks[1]", S + ":257")], "config")
word("pool", POOL, FLAMM_NS + 9, "FLAMM_NS+9", [F("recenterHook", "address", 0, 20, "poolReads.Hooks[2]", S + ":258")], "config")
word("pool", POOL, FLAMM_NS + 10, "FLAMM_NS+10", [F("controllerHook", "address", 0, 20, "poolReads.Hooks[3]", S + ":259"),
                                                 F("initialized", "bool", 20, 1, "(drift)", S + ":260"),
                                                 F("bootstrapped", "bool", 21, 1, "(drift)", S + ":261"),
                                                 F("paused", "bool", 22, 1, "poolReads.Paused", S + ":262"),
                                                 F("levPaused", "bool", 23, 1, "poolReads.LevPaused", S + ":263")])
word("pool", POOL, FLAMM_NS + 11, "FLAMM_NS+11", [F("loans.length", "uint256", 0, 32, "len(poolReads.Loans) (loanCount)", S + ":265")], "config")
for i in range(1):  # loan asset 0; a pool with more loan assets adds 6 words per index
    b = POOL_LOANS_BASE + 6 * i
    d = "keccak256(FLAMM_NS+11) + 6*%d" % i
    word("pool", POOL, b + 0, d + "+0", [F("loans[%d].token" % i, "address", 0, 20, "loanConfigReads.Token", S + ":209"),
                                        F("loans[%d].decimals" % i, "uint8", 20, 1, "loanConfigReads.Decimals", S + ":210")], "config")
    word("pool", POOL, b + 1, d + "+1", [F("loans[%d].scale" % i, "uint256", 0, 32, "gateLoanCfg.Scale (=10^(18-dec))", S + ":211")], "config")
    word("pool", POOL, b + 2, d + "+2", [F("loans[%d].swapPriceBandWad" % i, "uint64", 0, 8, "loanConfigReads.SwapPriceBandWad", S + ":212"),
                                        F("loans[%d].feeFloorWad" % i, "uint64", 8, 8, "loanConfigReads.FeeFloorWad", S + ":213")])
    word("pool", POOL, b + 3, d + "+3", [F("loans[%d].maxSwapNotional" % i, "uint256", 0, 32, "loanConfigReads.MaxSwapNotional", S + ":214")])
    word("pool", POOL, b + 4, d + "+4", [F("loans[%d].reserveTarget" % i, "uint256", 0, 32, "loanConfigReads.ReserveTarget", S + ":215")])
    word("pool", POOL, b + 5, d + "+5", [F("loans[%d].liquid" % i, "uint256", 0, 32, "loanConfigReads.Liquid", S + ":216")])
word("pool", POOL, FLAMM_NS + 12, "FLAMM_NS+12", [F("physicalPoolAsset", "uint256", 0, 32, "poolReads.Physical", S + ":267")])
word("pool", POOL, FLAMM_NS + 13, "FLAMM_NS+13", [F("phiWad", "uint64", 0, 8, "poolReads.PhiWad", S + ":269"),
                                                 F("phiMinWad", "uint64", 8, 8, "(dials envelope)", S + ":270"),
                                                 F("phiMaxWad", "uint64", 16, 8, "(dials envelope)", S + ":271"),
                                                 F("ltvWad", "uint64", 24, 8, "poolReads.LtvWad", S + ":272")])
word("pool", POOL, FLAMM_NS + 14, "FLAMM_NS+14", [F("ltvMinWad", "uint64", 0, 8, "(dials envelope)", S + ":273"),
                                                 F("ltvMaxWad", "uint64", 8, 8, "(dials envelope)", S + ":274"),
                                                 F("ltvMaxStepWad", "uint64", 16, 8, "(dials envelope)", S + ":275"),
                                                 F("ltvCooldownSec", "uint32", 24, 4, "(dials envelope)", S + ":276")], "config")
word("pool", POOL, FLAMM_NS + 15, "FLAMM_NS+15", [F("lastDialMoveTs", "uint48", 0, 6, "(dials cooldown)", S + ":277"),
                                                 F("minStructDistWad", "uint64", 6, 8, "(dials envelope)", S + ":278"),
                                                 F("roomEpsilonWad", "uint64", 14, 8, "poolReads.RoomEpsilonWad", S + ":279"),
                                                 F("feeFloorWad", "uint64", 22, 8, "poolReads.FeeFloorWad", S + ":280")])
word("pool", POOL, FLAMM_NS + 16, "FLAMM_NS+16", [F("feeCapWad", "uint64", 0, 8, "poolReads.FeeCapWad", S + ":281"),
                                                 F("maxPriceAgeSec", "uint32", 8, 4, "(unused by quote)", S + ":282")])
word("pool", POOL, FLAMM_NS + 19, "FLAMM_NS+19", [F("pendingInvariantHook", "address", 0, 20, "snapshot.PendingInvariantHook", S + ":292")], "ceremony")
word("pool", POOL, FLAMM_NS + 22, "FLAMM_NS+22", [F("pendingControllerHook", "address", 0, 20, "snapshot.PendingControllerHook", S + ":295"),
                                                 F("lastLeverSpreadPpm", "uint32", 20, 4, "poolReads.LastLeverSpreadPpm", S + ":296"),
                                                 F("hookSetExecutableAt", "uint48", 24, 6, "snapshot.HookSetExecutableAt", S + ":297")])
word("pool", POOL, FLAMM_NS + 23, "FLAMM_NS+23", [F("features", "uint256", 0, 32, "poolReads.Features", S + ":298")])
word("pool", POOL, FLAMM_NS + 24, "FLAMM_NS+24", [F("leverageHook", "address", 0, 20, "poolReads.Hooks[4]", S + ":300")], "config")
word("pool", POOL, FLAMM_NS + 25, "FLAMM_NS+25", [F("spreadHook", "address", 0, 20, "poolReads.Hooks[5]", S + ":301")], "config")
word("pool", POOL, FLAMM_NS + 28, "FLAMM_NS+28", [F("loanSwapHook", "address", 0, 20, "poolReads.Hooks[6]", S + ":304")], "config")
word("pool", POOL, FLAMM_NS + 30, "FLAMM_NS+30", [F("pendingLoanHash", "bytes32", 0, 32, "snapshot.PendingLoanHash", S + ":308")], "ceremony")
word("pool", POOL, FLAMM_NS + 31, "FLAMM_NS+31", [F("pendingLoanAt", "uint48", 0, 6, "snapshot.PendingLoanAt", S + ":309")], "ceremony")
word("pool", POOL, FLAMM_NS + 32, "FLAMM_NS+32", [F("pendingVenueHash", "bytes32", 0, 32, "snapshot.PendingVenueHash", S + ":310")], "ceremony")
word("pool", POOL, FLAMM_NS + 33, "FLAMM_NS+33", [F("pendingVenueAt", "uint48", 0, 6, "snapshot.PendingVenueAt", S + ":311")], "ceremony")
word("pool", POOL, ERC20_NS + 2, "ERC20_NS+2 (openzeppelin.storage.ERC20 _totalSupply)",
     [F("totalSupply", "uint256", 0, 32, "poolReads.TotalSupply", "ERC20Upgradeable.sol:37")])

# ---- swap hook (EverlongHook; layouts/EverlongHook.json)
H = "EverlongHook.sol"
word("hook", HOOK, 0, "slot 0", [F("_p.aWad", "uint128", 0, 16, "hookReads.AWad", H + ":Params.aWad"),
                                F("_p.spanUpWad", "uint128", 16, 16, "(unused by quote)", H + ":Params.spanUpWad")], "config")
word("hook", HOOK, 4, "slot 4", [F("_p.tuning.fee.midFeeWad", "uint64", 0, 8, "hookReads.Fee[0]", "EverlongStrategy.sol FeeParams"),
                                F("_p.tuning.fee.outFeeWad", "uint64", 8, 8, "hookReads.Fee[1]", ""),
                                F("_p.tuning.fee.gammaWad", "uint64", 16, 8, "hookReads.Fee[2]", ""),
                                F("_p.tuning.fee.sigmaRefWad", "uint64", 24, 8, "hookReads.Fee[3]", "")])
word("hook", HOOK, 5, "slot 5", [F("_p.tuning.fee.volBetaWad", "uint64", 0, 8, "hookReads.Fee[4]", ""),
                                F("_p.tuning.fee.volMinWad", "uint64", 8, 8, "hookReads.Fee[5]", ""),
                                F("_p.tuning.fee.volMaxWad", "uint64", 16, 8, "hookReads.Fee[6]", ""),
                                F("_p.tuning.fee.dirSkewWad", "uint64", 24, 8, "hookReads.Fee[7]", "")])
word("hook", HOOK, 6, "slot 6", [F("_p.tuning.invSkewKappaWad", "uint64", 0, 8, "hookReads.InvSkewKappaWad", H + ":Tuning"),
                                F("_p.tuning.invSkewBandWad", "uint64", 8, 8, "hookReads.InvSkewBandWad", ""),
                                F("_p.tuning.emaHalfLife", "uint64", 16, 8, "(observe path only)", ""),
                                F("_p.tuning.rvHalfLife", "uint64", 24, 8, "(observe path only)", "")])
for k, nm in enumerate(["aWad", "xLo", "xHi", "yHi"]):
    word("hook", HOOK, 10 + k, "slot %d" % (10 + k), [F("_sup." + nm, "uint256", 0, 32, "hookReads.Support[%d]" % k, "AlmCurve.sol Support")], "config")
word("hook", HOOK, 14, "slot 14", [F("anchorSqrtX96", "uint160", 0, 20, "hookReads.AnchorSqrtX96", H + ":anchorSqrtX96")])
word("hook", HOOK, 15, "slot 15", [F("reservationPriceWad", "uint256", 0, 32, "hookReads.ReservationPriceWad", H + ":reservationPriceWad")])
word("hook", HOOK, 16, "slot 16", [F("kappa", "uint256", 0, 32, "hookReads.Kappa", H + ":kappa")])
word("hook", HOOK, 17, "slot 17", [F("xWad", "uint256", 0, 32, "hookReads.XWad", H + ":xWad")])
word("hook", HOOK, 18, "slot 18", [F("reserveStable", "uint256", 0, 32, "hookReads.ReserveStable", H + ":reserveStable")])
word("hook", HOOK, 19, "slot 19", [F("idleStable", "uint256", 0, 32, "hookReads.IdleStable", H + ":idleStable")])
word("hook", HOOK, 20, "slot 20", [F("reserveVolatile", "uint256", 0, 32, "hookReads.ReserveVolatile", H + ":reserveVolatile")])
word("hook", HOOK, 21, "slot 21", [F("idleVolatile", "uint256", 0, 32, "hookReads.IdleVolatile", H + ":idleVolatile")])
word("hook", HOOK, 23, "slot 23", [F("rvWad", "uint256", 0, 32, "hookReads.RvWad", H + ":rvWad")])

# ---- spread hook (LeverageSpreadHook; layouts/LeverageSpreadHook.json)
word("spread", SPREAD_HOOK, 0, "slot 0", [F("spread", "uint24", 0, 3, "spreadHookState.Spread", "LeverageSpreadHook.sol:30"),
                                          F("minSpread", "uint24", 3, 3, "(setSpread bounds)", ":31"),
                                          F("maxSpread", "uint24", 6, 3, "(setSpread bounds)", ":32"),
                                          F("maxSpreadAge", "uint32", 9, 4, "spreadHookState.MaxSpreadAge", ":34"),
                                          F("lastSetTs", "uint48", 13, 6, "spreadHookState.LastSetTs", ":35")])

# ---- MMRouter (layouts/MMRouter.json; MMRouterLib.sol:39-78)
R = "MMRouterLib.sol"
word("router", ROUTER, 0, "slot 0", [F("globalPaused", "bool", 0, 1, "routerReads.GlobalPaused", "MMRouter.sol:23")])
word("router", ROUTER, ROUTER_RECORD + 0, "keccak256(pool.1)+0", [F("poolAsset", "address", 0, 20, "(drift)", R + ":67"),
                                                                  F("pinLtvWad", "uint64", 20, 8, "routerReads.PinLtvWad", R + ":68"),
                                                                  F("maxDrawnAssets", "uint8", 28, 1, "routerReads.MaxDrawnAssets", R + ":69")])
word("router", ROUTER, ROUTER_RECORD + 1, "keccak256(pool.1)+1", [F("safetyGapWad", "uint64", 0, 8, "routerReads.SafetyGapWad", R + ":70"),
                                                                  F("oracleBandWad", "uint64", 8, 8, "routerReads.OracleBandWad", R + ":71")])
word("router", ROUTER, ROUTER_RECORD + 2, "keccak256(pool.1)+2", [F("loans.length", "uint256", 0, 32, "len(routerReads.Loans)", R + ":72")], "config")
word("router", ROUTER, ROUTER_RECORD + 3, "keccak256(pool.1)+3", [F("venues.length", "uint256", 0, 32, "len(routerReads.Venues)", R + ":73")], "config")
for j, nm in enumerate(["borrowOrder", "supplyOrder", "withdrawOrder", "repayOrder"]):
    word("router", ROUTER, ROUTER_RECORD + 4 + j, "keccak256(pool.1)+%d" % (4 + j),
         [F(nm + ".length", "uint256", 0, 32, "len(routerReads.%s)" % (nm[0].upper() + nm[1:]), R + ":%d" % (74 + j))])
    word("router", ROUTER, ROUTER_ORDER_BASE[j], "keccak256(keccak256(pool.1)+%d)+0" % (4 + j),
         [F(nm + "[0..15]", "uint16[16]", 0, 32, "routerReads.%s (16 uint16 per word, element i at byte 2*i)" % (nm[0].upper() + nm[1:]), R + ":%d" % (74 + j))],
         note="one data word per 16 entries; a pool with more than 16 venues in an order adds words")
for i in range(1):
    b = ROUTER_LOANS_BASE + 7 * i
    d = "keccak256(keccak256(pool.1)+2) + 7*%d" % i
    word("router", ROUTER, b + 0, d + "+0", [F("loans[%d].token" % i, "address", 0, 20, "snapshot.RouterLoanToken", R + ":40"),
                                            F("loans[%d].decimals" % i, "uint8", 20, 1, "routerLoanReads.Decimals", R + ":41"),
                                            F("loans[%d].borrowEnabled" % i, "bool", 21, 1, "routerLoanReads.BorrowEnabled", R + ":42"),
                                            F("loans[%d].retired" % i, "bool", 22, 1, "routerLoanReads.Retired", R + ":43")])
    word("router", ROUTER, b + 1, d + "+1", [F("loans[%d].loanScale" % i, "uint256", 0, 32, "routerLoanReads.LoanScale", R + ":44")], "config")
    word("router", ROUTER, b + 2, d + "+2", [F("loans[%d].debtCap" % i, "uint128", 0, 16, "routerLoanReads.DebtCap", R + ":45"),
                                            F("loans[%d].supplyCap" % i, "uint128", 16, 16, "routerLoanReads.SupplyCap", R + ":46")])
    # loans[i].accounts[4] at +3..+6: never read on a quote path (the venue record carries the account)
for i in range(1):
    b = ROUTER_VENUES_BASE + 6 * i
    d = "keccak256(keccak256(pool.1)+3) + 6*%d" % i
    word("router", ROUTER, b + 0, d + "+0", [F("venues[%d].account" % i, "address", 0, 20, "StaticVenue.Account", R + ":51")], "config")
    word("router", ROUTER, b + 1, d + "+1", [F("venues[%d].id" % i, "bytes32", 0, 32, "StaticVenue.MarketID", R + ":52")], "config")
    word("router", ROUTER, b + 2, d + "+2", [F("venues[%d].kind" % i, "uint8", 0, 1, "venueReads.Kind", R + ":53"),
                                            F("venues[%d].loanIndex" % i, "uint8", 1, 1, "venueReads.LoanIndex", R + ":54"),
                                            F("venues[%d].lltvWad" % i, "uint64", 2, 8, "venueReads.LltvWad", R + ":55"),
                                            F("venues[%d].borrowEnabled" % i, "bool", 10, 1, "venueReads.BorrowEnabled", R + ":56"),
                                            F("venues[%d].supplyEnabled" % i, "bool", 11, 1, "venueReads.SupplyEnabled", R + ":57"),
                                            F("venues[%d].retired" % i, "bool", 12, 1, "venueReads.Retired", R + ":58"),
                                            F("venues[%d].debtCap" % i, "uint128", 13, 16, "venueReads.DebtCap", R + ":59")])
    word("router", ROUTER, b + 3, d + "+3", [F("venues[%d].supplyCap" % i, "uint128", 0, 16, "venueReads.SupplyCap", R + ":60"),
                                            F("venues[%d].maxBorrowRateWad" % i, "uint64", 16, 8, "venueReads.MaxBorrowRateWad", R + ":61")])
    word("router", ROUTER, b + 4, d + "+4", [F("venues[%d].managedCollateral" % i, "uint256", 0, 32, "venueReads.ManagedCollateral (no view)", R + ":62")])
    word("router", ROUTER, b + 5, d + "+5", [F("venues[%d].managedSupplyShares" % i, "uint256", 0, 32, "venueReads.ManagedSupplyShares (no view)", R + ":63")])

# ---- MorphoBlueAccount (layouts/MorphoBlueAccount.json)
A = "MorphoBlueAccount.sol"
word("account", ACCOUNT, 0, "slot 0", [F("ROUTER", "address", 0, 20, "(auth)", A + ":58")], "config")
word("account", ACCOUNT, 1, "slot 1", [F("POOL", "address", 0, 20, "(auth)", A + ":59")], "config")
word("account", ACCOUNT, 2, "slot 2", [F("POOL_ASSET", "address", 0, 20, "(market params)", A + ":60")], "config")
word("account", ACCOUNT, 3, "slot 3", [F("LOAN_ASSET", "address", 0, 20, "(market params)", A + ":61")], "config")
word("account", ACCOUNT, 4, "slot 4", [F("MORPHO", "address", 0, 20, "StaticExtra.Morpho", A + ":62")], "config")
word("account", ACCOUNT, ACCOUNT_MARKET + 0, "keccak256(id.5)+0", [F("_markets[id].oracle", "address", 0, 20, "StaticVenue.Oracle", A + ":63"),
                                                                    F("_markets[id].lltv", "uint64", 20, 8, "StaticVenue.Lltv / venueReads.MarketLltv", A + ":63")], "config")
word("account", ACCOUNT, ACCOUNT_MARKET + 1, "keccak256(id.5)+1", [F("_markets[id].irm", "address", 0, 20, "StaticVenue.Irm / venueReads.Irm, HasIrm", A + ":63")], "config")

# ---- PriceFeed (layouts/PriceFeed.json; PriceFeed.sol:13-19)
P = "PriceFeed.sol"
for tok, nm in ((CBBTC, "asset"), (USDC, "loan0")):
    b = FEED_TOKEN[tok]
    word("feed", PRICE_FEED, b + 0, "keccak256(%s.0)+0" % nm, [F("_tokens[%s].aggregator" % nm, "address", 0, 20, "StaticExtra.Aggregators (proxy)", P + ":14"),
                                                              F("_tokens[%s].heartbeat" % nm, "uint32", 20, 4, "feedTokenReads.Heartbeat", P + ":15"),
                                                              F("_tokens[%s].scale" % nm, "uint64", 24, 8, "feedTokenReads.Scale", P + ":16")], "config")
    word("feed", PRICE_FEED, b + 1, "keccak256(%s.0)+1" % nm, [F("_tokens[%s].unit" % nm, "uint64", 0, 8, "feedTokenReads.Unit", P + ":17"),
                                                              F("_tokens[%s].pegBandWad" % nm, "uint64", 8, 8, "feedTokenReads.PegBandWad", P + ":18")], "config")

# ---- FLAMMFactory (layouts/FLAMMFactory.json)
word("factory", FACTORY, 0, "slot 0", [F("_implementation", "address", 0, 20, "snapshot.Implementation (beacon)", "FLAMMFactory.sol:52")], "config")
word("factory", FACTORY, 1, "slot 1", [F("pendingImplementation", "address", 0, 20, "snapshot.PendingImplementation", "FLAMMFactory.sol:53"),
                                       F("implementationExecutableAt", "uint48", 20, 6, "snapshot.ImplementationExecutableAt", "FLAMMFactory.sol:54")], "ceremony")
word("factory", FACTORY, 2, "slot 2", [F("pendingImplementationCodehash", "bytes32", 0, 32, "(upgrade ceremony)", "FLAMMFactory.sol:57")], "ceremony")
word("factory", FACTORY, FACTORY_ISPOOL, "keccak256(pool.4)", [F("isPool[pool]", "bool", 0, 1, "(adapter only)", "FLAMMFactory.sol:59")], "config")

# ---- Morpho Blue (v1.0.0 Morpho.sol: position slot 2, market slot 3, idToMarketParams slot 8)
M = "morpho-blue Morpho.sol"
word("mm:0:market:0", MORPHO, MM_MARKET + 0, "keccak256(id.3)+0", [F("totalSupplyAssets", "uint128", 0, 16, "mmMarket.TotalSupplyAssets", M + " Market"),
                                                                    F("totalSupplyShares", "uint128", 16, 16, "mmMarket.TotalSupplyShares", "")])
word("mm:0:market:1", MORPHO, MM_MARKET + 1, "keccak256(id.3)+1", [F("totalBorrowAssets", "uint128", 0, 16, "mmMarket.TotalBorrowAssets", ""),
                                                                    F("totalBorrowShares", "uint128", 16, 16, "mmMarket.TotalBorrowShares", "")])
word("mm:0:market:2", MORPHO, MM_MARKET + 2, "keccak256(id.3)+2", [F("lastUpdate", "uint128", 0, 16, "mmMarket.LastUpdate", ""),
                                                                    F("fee", "uint128", 16, 16, "mmMarket.Fee", "")])
word("mm:0:position:0", MORPHO, MM_POSITION + 0, "keccak256(account . keccak256(id.2))+0", [F("supplyShares", "uint256", 0, 32, "mmPosition.SupplyShares", M + " Position")])
word("mm:0:position:1", MORPHO, MM_POSITION + 1, "keccak256(account . keccak256(id.2))+1", [F("borrowShares", "uint128", 0, 16, "mmPosition.BorrowShares", ""),
                                                                                             F("collateral", "uint128", 16, 16, "mmPosition.Collateral", "")])
# idToMarketParams is never read on a FLAMM path; the account's _markets[id] carries the same values (verified at registration,
# MorphoBlueAccount.sol:109-115). Listed for the verification cross-check only.
for k, nm in enumerate(["loanToken", "collateralToken", "oracle", "irm", "lltv"]):
    word("mm:0:params:%d" % k, MORPHO, MM_PARAMS + k, "keccak256(id.8)+%d" % k, [F(nm, "address" if k < 4 else "uint256", 0, 20 if k < 4 else 32, "StaticVenue.%s (cross-check)" % nm, M + " idToMarketParams")], "config",
         note="not emitted: cross-check only")
# ---- AdaptiveCurveIrm
word("irm:0:rate_at_target", IRM, IRM_RATE, "keccak256(id.0)", [F("rateAtTarget", "int256", 0, 32, "venueReads.RateAtTarget", "AdaptiveCurveIrm.sol rateAtTarget")])

# ---------------------------------------------------------------- Chainlink / sequencer (event-sourced; storage listed for verification)
# Read access. `EACAggregatorProxy.latestRoundData` is `checkAccess()`-guarded: slot 5 `accessController` must be zero
# (else `accessController.hasAccess(msg.sender, msg.data)` on a contract this schema does not track: fail closed).
# `AccessControlledOCR2Aggregator.latestRoundData` and `OptimismSequencerUptimeFeed.latestRoundData` are guarded by
# `SimpleReadAccessController.hasAccess(proxy, data)` = `s_accessList[proxy] || !checkEnabled || proxy == tx.origin`
# (dualagg_src/SimpleWriteAccessController.sol; the proxy is a contract, never tx.origin). `DualAggregator.latestRoundData`
# (dualagg_src/DualAggregator.sol:1072) carries no `checkAccess` modifier, so its inherited pair is never read (census:
# no access word on 0xe5ec…) and is listed for the cross-check only. Slots: OCR2 `checkEnabled` slot 21 @0,
# `s_accessList` base 22; uptime feed and DualAggregator `checkEnabled` slot 1 @20 (packed after `s_pendingOwner`),
# `s_accessList` base 2 (each confirmed against `checkEnabled()` / `hasAccess(proxy, "")` when recorded).
ACCESS = {
    "ocr2": {"check_enabled": (21, 0), "access_list_base": 22, "guarded": True},
    "uptime": {"check_enabled": (1, 20), "access_list_base": 2, "guarded": True},
    "dual": {"check_enabled": (1, 20), "access_list_base": 2, "guarded": False},
}
PROXY_ACCESS_CONTROLLER_SLOT = 5  # EACAggregatorProxy: owner 0 | pendingOwner 1 | currentPhase 2 | proposedAggregator 3 | phaseAggregators 4

FEEDS = [
    # role, proxy, aggregator (today), kind, hotvars slot, transmissions slot, what it prices
    {"role": "asset", "proxy": PROXY_CBBTC_USD, "aggregator": AGG_CBBTC_USD, "kind": "ocr2", "hotvars": 11, "transmissions": 12,
     "use": "PriceFeed usd(cbBTC): cross numerator", "heartbeat": 3600},
    {"role": "loan0", "proxy": PROXY_USDC_USD, "aggregator": AGG_USDC_USD, "kind": "ocr2", "hotvars": 11, "transmissions": 12,
     "use": "PriceFeed usd(USDC): cross denominator, pegOk", "heartbeat": 90000},
    {"role": "seq", "proxy": PROXY_SEQ, "aggregator": AGG_SEQ, "kind": "uptime", "feedstate": 4,
     "use": "PriceFeed _requireSequencer: answer, startedAt"},
    {"role": "mo0", "proxy": PROXY_BTC_USD, "aggregator": AGG_BTC_USD, "kind": "dual", "hotvars": 13, "transmissions": 17, "cutoff": 18,
     "use": "MorphoChainlinkOracleV2.price() = SCALE_FACTOR * answer: MMRouterLib.bandOk and Morpho _isHealthy"},
]
for _f in FEEDS:
    _f["access"] = dict(ACCESS[_f["kind"]], proxy_slot=PROXY_ACCESS_CONTROLLER_SLOT,
                        access_list=map_slot(addr_word(_f["proxy"]), ACCESS[_f["kind"]]["access_list_base"]))


def feed_read_ok(access, proxy_controller: int, check_enabled: bool, access_list: bool) -> bool:
    """Whether `proxy.latestRoundData()` returns instead of reverting on access, from the tracked words:
    EACAggregatorProxy.checkAccess (accessController == 0) and, for a guarded aggregator,
    SimpleWriteAccessController.hasAccess(proxy) = s_accessList[proxy] || !checkEnabled. The decoder ANDs this into
    roundReads.Ok (asset, loan0, seq) and venueReads.OracleOk (mo0)."""
    if proxy_controller != 0:
        return False
    if access["guarded"] and not (access_list or not check_enabled):
        return False
    return True

TOPICS = {
    "AnswerUpdated(int256,uint256,uint256)": "0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f",
    "NewRound(uint256,address,uint256)": "0x0109fc6f55cf40689f02fbaad7af7fe7bbac8a3d2186600afc7d3e10cac60271",
    "NewTransmission(uint32,int192,address,uint32,int192[],bytes,int192,bytes32,uint40)":
        "0xc797025feeeaf2cd924c99e9205acb8ec04d5cad21c41ce637a38fb6dee6016a",
    "SecondaryRoundIdUpdated(uint32)": "0x8d530b9ddc4b318d28fdd4c3a21fcfecece54c1a72a824f262985b99afef009b",
    "PrimaryFeedUnlocked(uint32)": None,
    "CutoffTimeSet(uint32)": None,
    "RoundUpdated(int256,uint64)": "0x297642343ed2faefb1a411b39fc449eae700e54223d5d0499a9421eb6f68f66a",
    # NOT emitted by the v0.6 EACAggregatorProxy deployed on Base (bytecode carries only the Owned events; the
    # three past rotations produced no proxy log, rotations.py): a rotation is a write to proxy slot 2 and is
    # tracked as a storage change, see the base-flamm README, section Attributes.
    "AggregatorConfirmed(address,address) [not emitted]": "0x33745f67a407dcb785417f9c123dd3641479a102674b6e35c1f10975625b90e9",
    "AggregatorProposed(address,address) [not emitted]": "0xc0f151710f03d713b71d9970cee0d5b11ddc9a7552abaa3f6ee818010f21600d",
    "OwnershipTransferRequested(address,address)": None,
    "OwnershipTransferred(address,address)": None,
}


def fill_topics():
    from rpc import topic
    for k in list(TOPICS):
        t = topic(k.split(" [")[0])
        if TOPICS[k] is None:
            TOPICS[k] = t
        else:
            assert TOPICS[k] == t, (k, t)


fill_topics()

# ---------------------------------------------------------------- static attributes (immutables / bytecode constants / manifest)
STATIC = [
    # name, source contract, how obtained, value-or-None (filled by snapshot from eth_call at the block), type
    {"name": "implementation", "from": "factory slot 0 at creation / PoolCreated", "type": "address"},
    {"name": "implementation_codehash", "from": "keccak256(extcodecopy(implementation)) at creation; pins the FLAMMStore layout", "type": "bytes32"},
    {"name": "hook", "from": "pool FLAMM_NS+7 (invariantHook) at creation", "type": "address"},
    {"name": "hook_codehash", "from": "keccak256(code(hook)); registry match required to emit the component", "type": "bytes32"},
    {"name": "hook_loan_scale", "from": "EverlongHook.LOAN_SCALE() immutable (= 10^(18-loanDecimals) = 1e12)", "type": "uint256"},
    {"name": "hook_genesis_strategy_hash", "from": "EverlongHook.genesisStrategyHash() immutable", "type": "bytes32"},
    {"name": "hook_genesis_params_hash", "from": "EverlongHook.genesisParamsHash() immutable", "type": "bytes32"},
    {"name": "leverage_hook", "from": "pool FLAMM_NS+24 at creation", "type": "address"},
    {"name": "leverage_hook_codehash", "from": "keccak256(code(leverage_hook))", "type": "bytes32"},
    {"name": "leverage_hook_loan_scale", "from": "EverlongLeverageHook.LOAN_SCALE() immutable", "type": "uint256"},
    {"name": "leverage_hook_swap_hook", "from": "EverlongLeverageHook.HOOK() immutable (must equal hook)", "type": "address"},
    {"name": "spread_hook", "from": "pool FLAMM_NS+25 at creation", "type": "address"},
    {"name": "spread_hook_codehash", "from": "keccak256(code(spread_hook))", "type": "bytes32"},
    {"name": "router", "from": "pool FLAMM_NS+1", "type": "address"},
    {"name": "router_codehash", "from": "keccak256(code(router)); MMRouter is not upgradeable, pins the record layout", "type": "bytes32"},
    {"name": "price_feed", "from": "pool FLAMM_NS+2", "type": "address"},
    {"name": "price_feed_sequencer", "from": "PriceFeed.SEQUENCER_FEED() immutable (proxy)", "type": "address"},
    {"name": "price_feed_sequencer_grace", "from": "PriceFeed.SEQUENCER_GRACE() immutable (seconds)", "type": "uint256"},
    {"name": "factory", "from": "pool FLAMM_NS+5 / PoolCreated emitter", "type": "address"},
    {"name": "factory_upgrade_delay", "from": "FLAMMFactory.UPGRADE_DELAY() immutable", "type": "uint32"},
    {"name": "pool_asset", "from": "pool FLAMM_NS+0", "type": "address"},
    {"name": "loan_asset_0", "from": "pool loans[0].token", "type": "address"},
    {"name": "venue_0_account", "from": "router venues[0].account", "type": "address"},
    {"name": "venue_0_account_codehash", "from": "keccak256(code(account))", "type": "bytes32"},
    {"name": "venue_0_market_id", "from": "router venues[0].id", "type": "bytes32"},
    {"name": "venue_0_morpho", "from": "account slot 4 (MORPHO)", "type": "address"},
    {"name": "venue_0_irm", "from": "account _markets[id].irm; must be AdaptiveCurveIrm for the port", "type": "address"},
    {"name": "venue_0_oracle", "from": "account _markets[id].oracle", "type": "address"},
    {"name": "venue_0_oracle_scale_factor", "from": "MorphoChainlinkOracleV2.SCALE_FACTOR() immutable (1e26)", "type": "uint256"},
    {"name": "venue_0_oracle_base_feed_1", "from": "MorphoChainlinkOracleV2.BASE_FEED_1() immutable (BTC/USD proxy); BASE_FEED_2, QUOTE_FEED_1/2 and both vaults are zero", "type": "address"},
    {"name": "feed_mo0_secondary_proxy", "from": "DualAggregator i_secondaryProxy immutable (bytecode PUSH32); must equal venue_0_oracle_base_feed_1 for the secondary-path decoder", "type": "address"},
    {"name": "feed_mo0_max_sync_iterations", "from": "DualAggregator i_maxSyncIterations immutable (bytecode; 20)", "type": "uint32"},
    {"name": "irm_codehash", "from": "keccak256(code(venue_0_irm)); pins the AdaptiveCurveIrm code constants the port carries (CURVE_STEEPNESS 4e18, ADJUSTMENT_SPEED 50e18/365d, TARGET_UTILIZATION 0.9e18, INITIAL_RATE_AT_TARGET 0.04e18/365d, MIN 0.001e18/365d, MAX 2e18/365d)", "type": "bytes32"},
    {"name": "feed_asset_proxy", "from": "PriceFeed _tokens[pool_asset].aggregator at creation (the EACAggregatorProxy the feed:asset:* attributes describe)", "type": "address"},
    {"name": "feed_loan0_proxy", "from": "PriceFeed _tokens[loan_asset_0].aggregator at creation (feed:loan0:*)", "type": "address"},
    {"name": "feed_seq_proxy", "from": "= price_feed_sequencer (PriceFeed.SEQUENCER_FEED immutable); the proxy feed:seq:* describes", "type": "address"},
    {"name": "feed_mo0_proxy", "from": "= venue_0_oracle_base_feed_1 (MorphoChainlinkOracleV2.BASE_FEED_1 immutable); the proxy feed:mo0:* describes", "type": "address"},
    {"name": "component_kind", "from": "manifest: 0 = swap, 1 = lever-up", "type": "uint8"},
]
STATIC_NAMES = [x["name"] for x in STATIC]

# ---------------------------------------------------------------- seeds (design 4.4): manifest params, checked by the range test
SEEDS = [
    {"param": "seed_feed_aggregators",
     "attrs": ["feed:%s:%s" % (f["role"], a) for f in FEEDS for a in ("aggregator", "phase", "access_controller")]
              + ["feed:%s:%s" % (f["role"], a) for f in FEEDS if f["access"]["guarded"] for a in ("check_enabled", "access_list")],
     "source": "proxy.aggregator() / proxy.phaseId() / proxy.accessController() at initialBlock-1 (= slots 2 and 5 of each proxy); "
               "aggregator.checkEnabled() and aggregator.hasAccess(proxy, \"\") for the guarded aggregators (= the checkEnabled word "
               "and s_accessList[proxy]; hasAccess == access_list || !check_enabled)"},
    {"param": "seed_sequencer", "attrs": ["feed:seq:round", "feed:seq:answer", "feed:seq:started_at", "feed:seq:updated_at"],
     "source": "sequencer proxy latestRoundData() at initialBlock-1 (= OptimismSequencerUptimeFeed slot 4)"},
    {"param": "seed_rounds", "attrs": ["feed:asset:round", "feed:asset:answer", "feed:asset:started_at", "feed:asset:updated_at",
                                       "feed:loan0:round", "feed:loan0:answer", "feed:loan0:started_at", "feed:loan0:updated_at",
                                       "feed:mo0:round", "feed:mo0:secondary_round", "feed:mo0:cutoff", "feed:mo0:tx:<r> for the ring"],
     "source": "aggregator latestRoundData() / getRoundData(r) at initialBlock-1 (= hotVars + s_transmissions[r] words)"},
    {"param": "seed_morpho", "attrs": ["mm:0:market:0", "mm:0:market:1", "mm:0:market:2", "irm:0:rate_at_target"],
     "source": "Morpho.market(id), AdaptiveCurveIrm.rateAtTarget(id) at initialBlock-1; position words are zero before the account exists"},
]
