#!/usr/bin/env python3
"""Records `replay.json`: real Montra swaps on Elysium with the pool state each one started from.

Usage: ELYSIUM_RPC_URL=<archive node with debug_traceTransaction> python3 record_replay.py replay.json [head]

`head` is the block taken as the chain's head, the chain's latest block when omitted; passing the
`head` of an earlier recording reproduces it.

The swaps come from three sources:
- eras: one window per `ERA_BLOCKS` blocks from the hook's deployment block 588510, plus one
  starting `HEAD_WINDOW_BLOCKS` before the head. Each window takes the first `ERA_TRANSACTIONS`
  transactions with a Montra swap from its start block on, scanning at most `ERA_SCAN_BLOCKS`
  blocks.
- `TARGETS`: the swaps of one pool over a block range, for what the windows cannot be relied on to
  hold: pools still at stage 1 (`Crossed`, which a later transaction confirms), the
  8-decimal EQSPCX quote in both orientations.
- `EXTRA_TXS`: single transactions the Montra tests name: the LP fee switch, anti-snipe buys and
  a sell on a 1% pool.

Every exact-input swap the Montra hook charged (one `FeeAccrued` log per swap) in those
transactions becomes one case, unless it is skipped. `skipped` in the output lists every skipped
swap with its transaction, pool and reason:
- `swapped twice`: the transaction swapped the pool more than once, and its logs cannot tell the
  swaps apart;
- `exact output`: Tycho quotes exact-input swaps only;
- `earlier swap in block`: another swap on the pool ran earlier in the same block, so the pool
  state read at the end of the previous block is not the state the swap started from;
- `launch buy`: the pool did not exist at the end of the previous block (a launch transaction's own
  first buy, which pays the total fee and no anti-snipe rate).

For each case the recorder reads, at the end of block N - 1 and from the node alone:
- the PoolManager's `slot0`, active liquidity and every initialized tick of the pool (the tick
  bitmap words over the whole usable range, then each set tick's `liquidityNet`), by
  `eth_getStorageAt` on the v4-core `StateLibrary` layout (pools mapping at slot 6, liquidity at
  +3, ticks at +4, tick bitmap at +5);
- the hook's `poolKey(bytes32)` and `poolConfig(bytes32)` getters, decoded by their published ABI;
- both currencies' `decimals()` (18 for the native asset);
- block N's timestamp, which is the clock the hook prices the swap at.

The expected output is what the swapper received, from the transaction's logs: for a buy (input =
the pool's quote asset) the positive amount of the PoolManager `Swap` log, since the hook charges a
buy on the input; for a sell that amount minus `FeeAccrued.fee`, which the hook takes from the
output. The input is `-FeeAccrued.amountSpecified`, the gross amount the PoolManager was asked to
swap.

`gas` is the swap's gas from the transaction's `debug_traceTransaction` call trace (`callTracer`):
the `gasUsed` of the PoolManager `swap` frame for the pool, and of the hook's `beforeSwap` and
`afterSwap` frames inside it. `first_in_tx` marks the transaction's first `swap` frame that calls
the hook, which finds the hook's storage cold. `hookless_swap_gas` holds the `swap` frame gas of
every swap on a pool without a hook in `HOOKLESS_WINDOW`.

`LIMITS` are the bounds bisected with the V4Quoter at a block: the
largest input that settles, read here as the state at the end of that block and its timestamp.
`OPENING_PROBES` add sells from a price in a pool's opening tick with deep liquidity past the
opening: the recorder overrides the pool's `slot0` and active liquidity in an `eth_call` to the
V4Quoter, finds the largest sell that settles there by bisection, and records it with its output
after checking that one unit more reverts `PastOpening()` (0xfafcddb0). For a zero-for-one sell
the active liquidity written includes the opening tick's `liquidityNet` (see `opening_probe`).

`SPOT_PROBES` quote a small buy and a small sell through Uniswap's V4Quoter (`eth_call` at the
end of the block, under the given block timestamp where one is set): the buy sells `amount` of
the quote asset, the sell sells the tokens that buy returned. Their prices pin the marginal price
from both sides.
"""

import collections
import concurrent.futures
import functools
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

RPC = os.environ["ELYSIUM_RPC_URL"]
CAST = os.environ.get("CAST", "cast")
HOOK = "0x6b1216a40b65c83e9c9e18a562c446697b8cf8ec"
POOL_MANAGER = "0xe2e105d3f7209a9da11f83ccf3e7398a753823f1"
QUOTER = "0x379f556c5a748bd7cc9baf0f7a344094ac1814f5"
NATIVE = "0x0000000000000000000000000000000000000000"

HOOK_DEPLOYMENT_BLOCK = 588_510
# Sampling choices, not properties of the chain: about a dozen windows over the hook's history,
# each with enough swaps to cover both directions and several pools.
ERA_BLOCKS = 500_000
ERA_TRANSACTIONS = 30
# A window that finds fewer swaps stops here, so a quiet stretch does not scan to the head.
ERA_SCAN_BLOCKS = 60_000
# How far before the head the last window starts.
HEAD_WINDOW_BLOCKS = 60_000
# The block span of one eth_getLogs request and the requests in one JSON-RPC batch: chosen to keep
# each reply small, not limits the Elysium RPC is known to enforce.
LOG_CHUNK_BLOCKS = 2_000
BATCH_SIZE = 200
TARGETS = [
    # (what the target covers, pool, first block, last block, at most this many transactions)
    # Graduates at 1030049 and crosses its LP line in the blocks after (spec test range 1).
    ("stage 1, native quote", "0x35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2",
     1_030_000, 1_031_999, 40),
    # Launched 4970022 and graduates in its launch block (spec test range 2).
    ("stage 1, native quote", "0x4937bb23c0503b32cf26d119cdfa5772d40a3900f5a1783437546e2e52025764",
     4_970_022, 4_971_999, 40),
    ("EQSPCX as currency1", "0x61379950571780e6649da42c72d93788a084b276b89bcddb4ba3c3a6a62a750e",
     4_963_600, 4_987_299, 15),
    ("EQSPCX as currency0", "0x0ee0fec1e1a3db8ea99daedfece3b81f8e16ed44a4df6f804280deeb5fbcd6d6",
     4_963_600, 4_987_299, 15),
]
EXTRA_TXS = [
    # The swap that switched the LP fee on, block 713192.
    "0x02e351ec8ffd7f641533d9340fcb3945d4c7c150b45a80ac0748e620d830b37e",
    # Buys inside an anti-snipe window.
    "0x024adfd8334d7b2998145379b31b11c5a96ef4e481a3628ae0a269415b78c54a",
    "0x080df039117616734e63b13e538c2b9f8539d32446bcbbde128f9670a48969eb",
    "0xfadf86726fb88b10c53dd6c40651cc60d79338ec7f0296915b315c0f3d15b96e",
    "0xd7c242531d3adbf81838c995ec3949536196ab924e84ba2a43e0ea8049a2bde0",
    "0xc1988d7249e48f915131de22d78fe3d4bcc3fb5daa7880bbcd24b95b525f99b8",
    # A sell on a 1% pool, block 5985683.
    "0xe221ea2739b3a59d002ff6be55756ac81ef11083346fee4334cb60c8a09479d1",
    # Sells on the EQSPCX-as-currency1 pool 0x61379950..., each the pool's first swap in its block
    # (4966806, 4966810, 4966814): the pool's first transactions in `TARGETS` are buys.
    "0xb6d11009559fd0896ad86437940f8f1f7477991ff8e2df2ef7f0d26d118c16ed",
    "0x8a2cdeaa1532aef43c96ca4725edab0a1dc311aaf797c806a23a57db041fc4db",
    "0x2b4919a556c94b369d9a699b4fd1fea2865ca7fa74bc9d87e8999fc3f7b5aaee",
]
HOOKLESS_WINDOW = (5_990_000, 5_990_400)
LIMITS = [
    # (pool, block, zero_for_one, largest input that settles, output at it)
    (
        "0x850fd929fb9399c5d881c587a601e47b8eb20b361bb224651fcd2883852e22e9",
        6_000_000,
        False,
        11345597969981426818405322,
        475398000000000000,
    ),
    (
        "0x61a799e016a5433af3e67cee4ca32dae81338c1ae7287192dc9d69b83b282aac",
        1_700_000,
        None,
        966809226680778394548981093,
        None,
    ),
    (
        "0x9e1059afeb0e08a2918f64018b722a7304067ab5ccad74baa5a6f89463bff00d",
        6_024_400,
        False,
        17559683734854681509494560323629769346,
        None,
    ),
    (
        "0x850fd929fb9399c5d881c587a601e47b8eb20b361bb224651fcd2883852e22e9",
        6_024_400,
        None,
        73,
        0,
    ),
    (
        "0x9e1059afeb0e08a2918f64018b722a7304067ab5ccad74baa5a6f89463bff00d",
        6_024_400,
        None,
        40034076,
        0,
    ),
]
OPENING_PROBES = [
    # (pool, block, opening tick, sqrt price in the opening tick, liquidity past the opening).
    # 0x850fd929... quotes in currency0 (stage 0 at 6024400, so no LP fee), at getSqrtPriceAtTick
    # of its opening seedTickUpper 169800: floor((L - 1) / 2^96) units settle with no output.
    ("0x850fd929fb9399c5d881c587a601e47b8eb20b361bb224651fcd2883852e22e9", 6_024_400, 169_800,
     385350023123796245667272766987274, 1 << 100),
    ("0x850fd929fb9399c5d881c587a601e47b8eb20b361bb224651fcd2883852e22e9", 6_024_400, 169_800,
     385350023123796245667272766987274, 1 << 110),
    # 0xff8294a2... quotes in currency1 (EQSPCX; stage 0 at 6024400, so no LP fee) and sits in its
    # opening tick, seedTickLower -405400, at its slot0 price, 6 units above getSqrtPriceAtTick
    # (124787350459464601646). A sell is zero-for-one, takes token0 in, and pays out what the walk
    # down to the opening yields before the opening absorbs the rest.
    ("0xff8294a27d1ded2e04c43f2f5d9a3eff518455a656b3ae4a95f7c748e94a79bb", 6_024_400, -405_400,
     124787350459464601652, 1 << 100),
]
PAST_OPENING = "0xfafcddb0"
SPOT_PROBES = [
    # (pool, block, timestamp override or None, small quote-asset input)
    # Inside the anti-snipe window of pool 0x8dba98f9...: t0 = 1794584110, W = 10 s, so t0 + 3 pays 699000.
    ("0x8dba98f9f6559a0ae9f1fdd95bda06fbda798cefba79f4ad092760b79947e22a", 6_001_266, 1_794_584_113, 10**10),
    # LP fee on (stage 4, lpFeePips 3000) at block 6002000.
    ("0x07184505168003343330f25a74a2ecdc2448d021974babf56a6aaecc5db0a08f", 6_002_000, None, 10**10),
]

MIN_TICK = -887272
MAX_TICK = 887272
SLOT0_SLOT = 0
LIQUIDITY_OFFSET = 3
TICKS_OFFSET = 4
TICK_BITMAP_OFFSET = 5
POOLS_SLOT = 6
# swap((address,address,uint24,int24,address),(bool,int256,uint160),bytes)
SWAP_SELECTOR = "0xf3cd914c"
# beforeSwap(address,(address,address,uint24,int24,address),(bool,int256,uint160),bytes)
BEFORE_SWAP_SELECTOR = "0x575e24b4"
# afterSwap(address,(address,address,uint24,int24,address),(bool,int256,uint160),int256,bytes)
AFTER_SWAP_SELECTOR = "0xb47b2fb1"


def is_transient(error):
    """Whether a failed POST is worth retrying: a rate limit or server error, a dropped or timed-out
    connection, or a reply cut off before its JSON ends. Any other HTTP status is a request the
    node refuses, which a retry does not change."""
    if isinstance(error, urllib.error.HTTPError):
        return error.code == 429 or error.code >= 500
    return isinstance(error, (urllib.error.URLError, ConnectionError, TimeoutError, json.JSONDecodeError))


def post(body, attempts=10):
    """The decoded JSON reply to `body`, retrying transient failures (`is_transient`) with
    exponential backoff. A JSON-RPC error inside the reply is returned as is: an `eth_call` revert
    is the answer `rpc_reply` callers read."""
    for attempt in range(attempts):
        http_request = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
        try:
            return json.load(urllib.request.urlopen(http_request, timeout=300))
        except Exception as error:
            if not is_transient(error) or attempt == attempts - 1:
                raise
            time.sleep(min(2**attempt, 60))


def rpc_request(method, params, request_id=0):
    return {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}


def rpc_batch(calls):
    out = []
    for start in range(0, len(calls), BATCH_SIZE):
        chunk = calls[start : start + BATCH_SIZE]
        body = json.dumps([rpc_request(m, p, i) for i, (m, p) in enumerate(chunk)]).encode()
        for reply in sorted(post(body), key=lambda r: r["id"]):
            if "error" in reply:
                raise RuntimeError(reply["error"])
            out.append(reply["result"])
    return out


def rpc(method, params):
    return rpc_batch([(method, params)])[0]


def rpc_reply(method, params):
    """The whole JSON-RPC reply, error included."""
    return post(json.dumps(rpc_request(method, params)).encode())


@functools.cache
def cast(*args):
    return subprocess.run([CAST, *args], check=True, capture_output=True, text=True).stdout.strip()


@functools.cache
def mapping_slot(key_type, key, base):
    return cast("index", key_type, "--", str(key), hex(base) if isinstance(base, int) else base)


def word(value, index):
    return int(value[2 + 64 * index : 2 + 64 * (index + 1)], 16)


def signed(value, bits):
    return value - (1 << bits) if value >> (bits - 1) else value


def log_chunks(address, topics, first, last):
    """The logs from `first` to `last`, one `LOG_CHUNK_BLOCKS` request at a time, so a caller can stop
    early."""
    for start in range(first, last + 1, LOG_CHUNK_BLOCKS):
        yield rpc(
            "eth_getLogs",
            [{"address": address, "topics": topics, "fromBlock": hex(start),
              "toBlock": hex(min(start + LOG_CHUNK_BLOCKS - 1, last))}],
        )


def get_logs(address, topics, first, last):
    return [log for chunk in log_chunks(address, topics, first, last) for log in chunk]


def unique(items):
    """`items` without repeats, in first-seen order."""
    return list(dict.fromkeys(items))


def transactions_of(logs):
    return unique(log["transactionHash"] for log in logs)


def pool_slots(pool_id, tick_spacing):
    base = int(mapping_slot("bytes32", pool_id, POOLS_SLOT), 16)
    compressed_min = MIN_TICK // tick_spacing
    compressed_max = MAX_TICK // tick_spacing
    words = range(compressed_min >> 8, (compressed_max >> 8) + 1)
    return base, {w: mapping_slot("int16", w, base + TICK_BITMAP_OFFSET) for w in words}


@functools.cache
def decimals(token):
    if token == NATIVE:
        return 18
    return word(rpc("eth_call", [{"to": token, "data": cast("sig", "decimals()")}, "latest"]), 0)


def read_pool(pool_id, block, slots_cache):
    tag = hex(block)
    key = rpc("eth_call", [{"to": HOOK, "data": cast("calldata", "poolKey(bytes32)", pool_id)}, tag])
    currency0, currency1 = "0x" + key[26:66], "0x" + key[90:130]
    tick_spacing = signed(word(key, 3), 256)
    if tick_spacing == 0:
        # The hook has no key for the pool yet: it is launched in a later block.
        return None
    if pool_id not in slots_cache:
        slots_cache[pool_id] = pool_slots(pool_id, tick_spacing)
    base, bitmap_slots = slots_cache[pool_id]
    config = rpc("eth_call", [{"to": HOOK, "data": cast("calldata", "poolConfig(bytes32)", pool_id)}, tag])
    reads = [hex(base + SLOT0_SLOT), hex(base + LIQUIDITY_OFFSET)] + list(bitmap_slots.values())
    values = rpc_batch([("eth_getStorageAt", [POOL_MANAGER, slot, tag]) for slot in reads])
    slot0, liquidity = int(values[0], 16), int(values[1], 16)
    ticks = []
    for w, value in zip(bitmap_slots, values[2:]):
        bits = int(value, 16)
        ticks += [((w << 8) + bit) * tick_spacing for bit in range(256) if bits >> bit & 1]
    tick_slots = [mapping_slot("int24", t, base + TICKS_OFFSET) for t in ticks]
    tick_words = rpc_batch([("eth_getStorageAt", [POOL_MANAGER, s, tag]) for s in tick_slots])
    names = [
        "total_fee_pips", "lp_fee_pips", "protocol_fee_pips", "exists", "quote_is_currency0",
        "liquidity_sealed", "seeded", "fee_locked", "early_buy_fee_until", "graduation_tick",
        "lp_line_tick", "stage", "lp_crossed_at", "lp_held_seconds", "total_fee_min_pips",
        "total_fee_max_pips", "liquidity_policy", "seed_tick_lower", "seed_tick_upper",
        "early_buy_fee_start_pips", "early_buy_fee_window", "seed_liquidity", "recipient",
    ]
    decoded = {n: word(config, i) for i, n in enumerate(names)}
    for n in ("graduation_tick", "lp_line_tick", "seed_tick_lower", "seed_tick_upper"):
        # The ABI sign-extends an int24 return value to the whole 256-bit word.
        decoded[n] = signed(decoded[n], 256)
    keep = [
        "total_fee_pips", "lp_fee_pips", "quote_is_currency0", "early_buy_fee_until", "stage",
        "lp_crossed_at", "lp_held_seconds", "seed_tick_lower", "seed_tick_upper",
        "early_buy_fee_start_pips", "early_buy_fee_window",
    ]
    return {
        "pool_id": pool_id,
        "currency0": currency0,
        "currency1": currency1,
        "decimals0": decimals(currency0),
        "decimals1": decimals(currency1),
        "tick_spacing": tick_spacing,
        "sqrt_price_x96": str(slot0 & ((1 << 160) - 1)),
        "tick": signed((slot0 >> 160) & ((1 << 24) - 1), 24),
        "protocol_fee": (slot0 >> 184) & ((1 << 24) - 1),
        "slot0_lp_fee": (slot0 >> 208) & ((1 << 24) - 1),
        "liquidity": str(liquidity),
        "ticks": [[t, str(signed(int(v, 16) >> 128, 128))] for t, v in zip(ticks, tick_words)],
        "config": {n: decoded[n] for n in keep},
    }


@functools.cache
def timestamp(block):
    return int(rpc("eth_getBlockByNumber", [hex(block), False])["timestamp"], 16)


def swaps_of_receipt(receipt, fee_topic, swap_topic):
    """(pool, zero_for_one, amount_in, fee, pool output) of every exact-input Montra swap, and
    the (pool, reason) of each swap it skipped."""
    fees, outputs = {}, {}
    for log in receipt["logs"]:
        pool = log["topics"][1] if len(log["topics"]) > 1 else None
        if log["address"].lower() == HOOK and log["topics"][0] == fee_topic:
            data = log["data"]
            fees.setdefault(pool, []).append(
                (bool(word(data, 0)), signed(word(data, 1), 256), word(data, 2))
            )
        if log["address"].lower() == POOL_MANAGER and log["topics"][0] == swap_topic:
            data = log["data"]
            amounts = (signed(word(data, 0), 256), signed(word(data, 1), 256))
            outputs.setdefault(pool, []).append(max(amounts))
    out, skipped = [], []
    for pool, accruals in fees.items():
        if len(accruals) != 1 or len(outputs.get(pool, [])) != 1:
            skipped += [(pool, "swapped twice")] * max(len(accruals), len(outputs.get(pool, [])))
            continue
        zero_for_one, amount_specified, fee = accruals[0]
        if amount_specified >= 0:
            skipped.append((pool, "exact output"))
            continue
        out.append((pool, zero_for_one, -amount_specified, fee, outputs[pool][0]))
    return out, skipped


def swap_frames(frame):
    """Every PoolManager `swap` call frame in a call trace that did not revert, in execution
    order. A reverted frame (a router quoting by a swap it reverts) settled nothing."""
    if "error" in frame:
        return
    if frame.get("to", "").lower() == POOL_MANAGER and frame.get("input", "")[:10] == SWAP_SELECTOR:
        yield frame
    for child in frame.get("calls", []):
        yield from swap_frames(child)


def pool_id_of_swap(frame):
    """The pool id of a `swap` frame: keccak256 of its ABI-encoded PoolKey, the first five words."""
    return cast("keccak", "0x" + frame["input"][10 : 10 + 5 * 64])


def hooks_of_swap(frame):
    return "0x" + frame["input"][10 + 4 * 64 + 24 : 10 + 5 * 64]


@functools.cache
def trace(tx):
    return rpc("debug_traceTransaction", [tx, {"tracer": "callTracer"}])


def gas_of_swap(tx, pool):
    """The swap's gas from the call trace, as described in the module doc."""
    frames = list(swap_frames(trace(tx)))
    montra = [f for f in frames if hooks_of_swap(f) == HOOK]
    matching = [f for f in montra if pool_id_of_swap(f) == pool]
    if len(matching) != 1:
        raise RuntimeError(f"{tx}: {len(matching)} swap frames for {pool}")
    frame = matching[0]
    hook_calls = {
        call["input"][:10]: int(call["gasUsed"], 16)
        for call in frame.get("calls", [])
        if call.get("to", "").lower() == HOOK
    }
    return {
        "swap": int(frame["gasUsed"], 16),
        "before_swap": hook_calls[BEFORE_SWAP_SELECTOR],
        "after_swap": hook_calls[AFTER_SWAP_SELECTOR],
        "first_in_tx": montra[0] is frame,
    }


def cases_of_tx(tx, fee_topic, swap_topic, slots_cache):
    """The cases one transaction contributes, and the swaps it skipped."""
    receipt = rpc("eth_getTransactionReceipt", [tx])
    block = int(receipt["blockNumber"], 16)
    swaps, skipped = swaps_of_receipt(receipt, fee_topic, swap_topic)
    cases = []
    for pool, zero_for_one, amount_in, fee, pool_output in swaps:
        earlier = rpc(
            "eth_getLogs",
            [{"address": POOL_MANAGER, "topics": [swap_topic, pool],
              "fromBlock": hex(block), "toBlock": hex(block)}],
        )
        if any(int(l["transactionIndex"], 16) < int(receipt["transactionIndex"], 16) for l in earlier):
            skipped.append((pool, "earlier swap in block"))
            continue
        state = read_pool(pool, block - 1, slots_cache)
        if state is None:
            skipped.append((pool, "launch buy"))
            continue
        is_buy = zero_for_one == bool(state["config"]["quote_is_currency0"])
        amount_out = pool_output if is_buy else pool_output - fee
        cases.append(
            dict(state, tx=tx, block=block, timestamp=timestamp(block),
                 zero_for_one=zero_for_one, amount_in=str(amount_in),
                 amount_out=str(amount_out), hook_fee=str(fee), gas=gas_of_swap(tx, pool))
        )
    return cases, [{"tx": tx, "pool": pool, "reason": reason} for pool, reason in skipped]


def era_transactions(start, last_block, fee_topic):
    """The first `ERA_TRANSACTIONS` transactions with a Montra swap from `start` on."""
    txs = []
    end = min(start + ERA_SCAN_BLOCKS, last_block)
    for logs in log_chunks(HOOK, [fee_topic], start, end):
        txs = unique(txs + transactions_of(logs))
        if len(txs) >= ERA_TRANSACTIONS:
            break
    return txs[:ERA_TRANSACTIONS]


def hookless_swap_gas(swap_topic):
    gas = []
    for tx in transactions_of(get_logs(POOL_MANAGER, [swap_topic], *HOOKLESS_WINDOW)):
        gas += [int(f["gasUsed"], 16) for f in swap_frames(trace(tx)) if int(hooks_of_swap(f), 16) == 0]
    return gas


def quote_exact_input(state, zero_for_one, amount_in, block, time_override=None, overrides=None):
    """The V4Quoter's output, or the revert data of a quote that reverts."""
    key = f"({state['currency0']},{state['currency1']},8388608,{state['tick_spacing']},{HOOK})"
    data = cast(
        "calldata",
        "quoteExactInputSingle(((address,address,uint24,int24,address),bool,uint128,bytes))",
        f"({key},{str(zero_for_one).lower()},{amount_in},0x)",
    )
    params = [{"to": QUOTER, "data": data}, hex(block), overrides or {}]
    if time_override is not None:
        params.append({"time": hex(time_override)})
    reply = rpc_reply("eth_call", params)
    if "error" in reply:
        return None, reply["error"].get("data", "")
    return word(reply["result"], 0), None


def spot_probe(pool, block, time_override, amount, slots_cache):
    state = read_pool(pool, block, slots_cache)
    buy_zero_for_one = bool(state["config"]["quote_is_currency0"])
    bought, error = quote_exact_input(state, buy_zero_for_one, amount, block, time_override)
    if error is not None:
        raise RuntimeError(f"{pool}: a buy of {amount} at {block} reverted with {error}")
    sold_for, error = quote_exact_input(state, not buy_zero_for_one, bought, block, time_override)
    if error is not None:
        raise RuntimeError(f"{pool}: a sell of {bought} at {block} reverted with {error}")
    return dict(
        state,
        block=block,
        timestamp=time_override if time_override is not None else timestamp(block),
        buy_amount_in=str(amount),
        buy_amount_out=str(bought),
        sell_amount_in=str(bought),
        sell_amount_out=str(sold_for),
    )


def largest_settling_input(state, zero_for_one, block, overrides):
    """The largest input the V4Quoter settles: doubling up to the first revert, then bisection.
    0 when one unit reverts."""
    def settles(amount):
        return quote_exact_input(state, zero_for_one, amount, block, overrides=overrides)[1] is None

    settling, reverting = 0, 1
    while settles(reverting):
        settling, reverting = reverting, reverting * 2
    while reverting - settling > 1:
        middle = (settling + reverting) // 2
        if settles(middle):
            settling = middle
        else:
            reverting = middle
    return settling


def opening_probe(pool, block, opening_tick, opening_sqrt_price, liquidity_past, slots_cache):
    """A limit case at the opening with `liquidity_past` past it, bisected with the V4Quoter."""
    state = read_pool(pool, block, slots_cache)
    base, _ = slots_cache[pool]
    sell = not bool(state["config"]["quote_is_currency0"])
    # v4-core's Pool.swap finds each step's next tick with
    # TickBitmap.nextInitializedTickWithinOneWord(tick, tickSpacing, lte = zeroForOne)
    # (v4-core src/libraries/Pool.sol, src/libraries/TickBitmap.sol). With lte, a zero-for-one
    # sell's first step targets the initialized tick it is in and crosses it, removing that
    # tick's liquidityNet, so the active liquidity written is the liquidity past the opening plus
    # that net. A one-for-zero sell searches above the current tick and crosses nothing first.
    net_at_opening = dict(state["ticks"]).get(opening_tick, "0") if sell else "0"
    active_liquidity = liquidity_past + int(net_at_opening)
    slot0 = (
        opening_sqrt_price
        | ((opening_tick & 0xFFFFFF) << 160)
        | (state["protocol_fee"] << 184)
        | (state["slot0_lp_fee"] << 208)
    )
    overrides = {POOL_MANAGER: {"stateDiff": {
        "0x%064x" % (base + SLOT0_SLOT): "0x%064x" % slot0,
        "0x%064x" % (base + LIQUIDITY_OFFSET): "0x%064x" % active_liquidity,
    }}}
    largest = largest_settling_input(state, sell, block, overrides)
    if largest == 0:
        _, error = quote_exact_input(state, sell, 1, block, overrides=overrides)
        raise RuntimeError(f"{pool}: a sell of one unit at the opening reverted with {error}")
    out_at_limit, error = quote_exact_input(state, sell, largest, block, overrides=overrides)
    if error is not None:
        raise RuntimeError(
            f"{pool}: the bisected largest sell {largest} at the opening reverted with {error}"
        )
    out, error = quote_exact_input(state, sell, largest + 1, block, overrides=overrides)
    if error is None or PAST_OPENING[2:] not in error:
        raise RuntimeError(f"{pool}: a sell of {largest + 1} at the opening gave {out}, {error}")
    return dict(
        state, sqrt_price_x96=str(opening_sqrt_price), tick=opening_tick,
        liquidity=str(active_liquidity), block=block, timestamp=timestamp(block),
        zero_for_one=sell, limit_in=str(largest), out_at_limit=str(out_at_limit),
    )


def main():
    fee_topic = cast("sig-event", "FeeAccrued(bytes32,bool,int256,uint256,uint256)")
    swap_topic = cast("sig-event", "Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)")
    head = int(sys.argv[2]) if len(sys.argv) > 2 else int(rpc("eth_blockNumber", []), 16)
    era_starts = list(range(HOOK_DEPLOYMENT_BLOCK, head, ERA_BLOCKS)) + [head - HEAD_WINDOW_BLOCKS]
    txs = []
    for start in era_starts:
        era = era_transactions(start, head, fee_topic)
        print(f"era from {start}: {len(era)} transactions", file=sys.stderr)
        txs += era
    for label, pool, first, last, most in TARGETS:
        target = transactions_of(get_logs(POOL_MANAGER, [swap_topic, pool], first, last))[:most]
        print(f"{label} ({pool[:10]}): {len(target)} transactions", file=sys.stderr)
        txs += target
    txs = unique(txs + EXTRA_TXS)

    slots_cache = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        per_tx = list(pool.map(lambda tx: cases_of_tx(tx, fee_topic, swap_topic, slots_cache), txs))
    cases = [case for tx_cases, _ in per_tx for case in tx_cases]
    skipped = [skip for _, tx_skipped in per_tx for skip in tx_skipped]
    reasons = collections.Counter(skip["reason"] for skip in skipped)
    print(f"{len(txs)} transactions, {len(cases)} cases, skipped: {dict(reasons)}", file=sys.stderr)

    limits = []
    for pool, block, zero_for_one, limit_in, out_at_limit in LIMITS:
        state = read_pool(pool, block, slots_cache)
        sell_zero_for_one = not bool(state["config"]["quote_is_currency0"])
        direction = sell_zero_for_one if zero_for_one is None else zero_for_one
        limits.append(
            dict(state, block=block, timestamp=timestamp(block), zero_for_one=direction,
                 limit_in=str(limit_in), out_at_limit=None if out_at_limit is None else str(out_at_limit))
        )
    limits += [opening_probe(*probe, slots_cache) for probe in OPENING_PROBES]
    spots = [spot_probe(*probe, slots_cache) for probe in SPOT_PROBES]

    with open(sys.argv[1], "w") as out:
        json.dump(
            {
                "description": __doc__.strip().splitlines()[0],
                "head": head,
                "swaps": cases,
                "skipped": skipped,
                "hookless_swap_gas": hookless_swap_gas(swap_topic),
                "limits": limits,
                "spot_probes": spots,
            },
            out,
            indent=None,
            separators=(",", ":"),
        )


if __name__ == "__main__":
    main()
