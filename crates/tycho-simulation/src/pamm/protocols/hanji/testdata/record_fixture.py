"""Records a Hanji fixture at one Base block: every aggregator market's config and wiring, the
book each of its two quoters reports, and the fills its proxy orderbook returns for a ladder of
sizes to each kind of caller.

A proxy prices a caller with its FastQuoter when the caller is the transaction's sender or is on
the proxy's whitelist, and with a second quoter, a few bps wider each side, when it is any other
contract, such as a router. Both books are recorded, read from each quoter with
`assembleOrderbookFromOrders(lob, isAsk, depth)` at the depth the feed reads, and both kinds of
fill:

- "direct": a call from a fresh address, so the caller is the transaction's sender;
- "contract": the same call sent through a forwarding contract planted by a state override, so the
  caller is a contract that is not the transaction's sender and not whitelisted.

Each fill's `amount_out` is what the caller's balance of the output token gained, read from a
`prestateTracer` diff of the call, so it measures the payout rather than restating the proxy's
answer. Balances and allowances come from state overrides. The node must be an archive node that
serves `debug_traceCall` with state overrides (call and prestate tracers). Foundry's `cast` must be
on PATH: the script hashes with `cast keccak`.

Usage: BASE_RPC_URL=<rpc url> python3 -I record_fixture.py [block] > live_<block>.json

The RPC URL is read from the environment, not the command line, since it usually carries a key.
The constants below mirror the feed's (`reader.rs`, `mod.rs`); `testdata_constants_match` in the
Rust tests keeps them in step.

The Rust tests read one recording by name and pin its counts and gas figures, so a new recording
replaces `live_<block>.json` in `test_support.rs` and the figures that cite its block.
"""

import json
import os
import subprocess
import sys
import urllib.request

API = "https://api-base.hanji.io/aggregators/markets"
WRAPPED_NATIVE = "0x4200000000000000000000000000000000000006"
# Tycho's router on Base, whose whitelist status is recorded.
TYCHO_ROUTER = "0xd374c00614f95492d816b5957e3797ce3f0c445c"
TAKER = "0x1111111111111111111111111111111111111234"
FORWARDER = "0x2222222222222222222222222222222222225678"
MAX128 = (1 << 128) - 1
EXPIRES = 99_999_999_999
# The feed's MAX_PRICE_LEVELS.
DEPTH = 32
# Unnamed getters of the proxy orderbooks, identified by what they return: the underlying CLOB,
# the FastQuoter the sender and whitelisted callers get, and the quoter other contracts get.
LOB_SELECTOR = "f7d74524"
QUOTER_SELECTOR = "938c5132"
CONTRACT_QUOTER_SELECTOR = "f8bed25f"
# Balance and allowance mapping slots of the ERC20s the markets trade, found by overriding
# candidate slots and reading balanceOf back.
SLOTS = {
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": (9, 10),  # USDC
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": (9, 10),  # cbBTC
    WRAPPED_NATIVE: (3, 4),  # WETH
}
# Fractions of a side's depth to fill; the last is past it, to record a partial fill.
FRACTIONS = (0.0001, 0.003, 0.05, 0.3, 0.9, 1.2)
# Added to every size, so that none is a whole number of shares and the rounding shows.
OFF_GRID = 12_345

_keccak_cache: dict[bytes, bytes] = {}


def keccak(data: bytes) -> bytes:
    if data not in _keccak_cache:
        out = subprocess.run(
            ["cast", "keccak", "0x" + data.hex()],
            capture_output=True,
            text=True,
            check=True,
        )
        _keccak_cache[data] = bytes.fromhex(out.stdout.strip()[2:])
    return _keccak_cache[data]


def forwarder_code(target: str) -> str:
    """Runtime code that forwards its calldata and value to `target` and returns or reverts
    with the answer; a call without calldata (a payment) stops, so it accepts native ETH."""
    return (
        "0x3615603d57"  # if calldatasize == 0 jump to stop
        "366000600037"  # calldatacopy(0, 0, calldatasize)
        "6000600036600034"  # retSize, retOffset, argsSize, argsOffset, value
        "73" + target[2:].lower() + "5af1"  # target, gas, call
        "3d600060003e"  # returndatacopy(0, 0, returndatasize)
        "15603857"  # if the call failed jump to revert
        "3d6000f3"  # return(0, returndatasize)
        "5b3d6000fd"  # revert(0, returndatasize)
        "5b00"  # stop
    )


class Node:
    def __init__(self, url: str):
        self.url = url

    def rpc(self, method: str, params: list):
        body = json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
        )
        request = urllib.request.Request(
            self.url,
            body.encode(),
            {"Content-Type": "application/json", "User-Agent": "curl/8"},
        )
        out = json.load(urllib.request.urlopen(request))
        if "error" in out:
            raise RuntimeError(f"{method}: {out['error']}")
        return out["result"]


def word(value: int) -> str:
    return f"{value:064x}"


def address_word(address: str) -> str:
    return address[2:].lower().rjust(64, "0")


def selector(signature: str) -> str:
    return keccak(signature.encode())[:4].hex()


def words(raw: bytes) -> list[int]:
    return [int.from_bytes(raw[i : i + 32], "big") for i in range(0, len(raw), 32)]


def address_of(raw: bytes) -> str:
    return "0x" + f"{words(raw)[0]:040x}"


def erc20_overrides(token: str, owner: str, amount: int, spender: str) -> dict:
    balance_slot, allowance_slot = SLOTS[token]
    balance_key = keccak(bytes.fromhex(address_word(owner) + word(balance_slot)))
    owner_key = keccak(bytes.fromhex(address_word(owner) + word(allowance_slot)))
    allowance_key = keccak(bytes.fromhex(address_word(spender)) + owner_key)
    return {
        token: {
            "stateDiff": {
                "0x" + balance_key.hex(): "0x" + word(amount),
                "0x" + allowance_key.hex(): "0x" + word(MAX128),
            }
        }
    }


def balance_gain(diff: dict, owner: str, token: str | None) -> int:
    """What `owner`'s balance of `token` (native ETH when None) gained in a `prestateTracer` diff.
    The diff leaves out what the call did not change, and a slot that was zero before it, so a
    missing value reads as unchanged or zero respectively."""

    def read(side: str) -> int:
        if token is None:
            return int(diff[side].get(owner, {}).get("balance", "0x0"), 16)
        balance_slot, _ = SLOTS[token]
        key = (
            "0x" + keccak(bytes.fromhex(address_word(owner) + word(balance_slot))).hex()
        )
        storage = diff[side].get(token, {}).get("storage", {})
        return int(storage.get(key, "0x0"), 16)

    if token is None and owner not in diff["post"]:
        return 0
    return read("post") - read("pre")


class Recorder:
    def __init__(self, node: Node, block: str):
        self.node = node
        self.block = block

    def call(self, to: str, data: str) -> bytes:
        tx = {"from": TAKER, "to": to, "data": "0x" + data}
        return bytes.fromhex(self.node.rpc("eth_call", [tx, self.block])[2:])

    def fill(
        self,
        proxy: str,
        data: str,
        caller: str,
        token_in: str | None,
        token_out: str | None,
        amount: int,
    ) -> tuple[list[int], int, int]:
        """Sends a swap to `proxy` as `caller` ("direct" or "contract"), paying `amount` of
        `token_in` for `token_out` (native ETH when None). Returns the proxy's answer, its call's
        own gas, and what the caller's balance of `token_out` gained."""
        payer = TAKER if caller == "direct" else FORWARDER
        overrides = {TAKER: {"balance": hex(amount + 10**20)}}
        if caller == "contract":
            overrides[FORWARDER] = {"code": forwarder_code(proxy)}
        if token_in is not None:
            overrides.update(erc20_overrides(token_in, payer, amount, proxy))
        tx = {
            "from": TAKER,
            "to": proxy if caller == "direct" else FORWARDER,
            "data": "0x" + data,
            "gas": hex(30_000_000),
            # No gas price, so a native balance moves only by what the swap pays and receives.
            "gasPrice": "0x0",
        }
        if token_in is None:
            tx["value"] = hex(amount)
        config = {"tracer": "callTracer", "stateOverrides": overrides}
        result = self.node.rpc("debug_traceCall", [tx, self.block, config])
        if result.get("error"):
            raise RuntimeError(
                f"{proxy} reverted: {result.get('revertReason', result['error'])}"
            )
        frame = result if caller == "direct" else result["calls"][0]
        gas = int(frame["gasUsed"], 16)
        if caller == "direct":
            raw = bytes.fromhex(data)
            gas -= 21_000 + sum(16 if byte else 4 for byte in raw)
        diff_config = {
            "tracer": "prestateTracer",
            "tracerConfig": {"diffMode": True},
            "stateOverrides": overrides,
        }
        diff = self.node.rpc("debug_traceCall", [tx, self.block, diff_config])
        received = balance_gain(diff, payer, token_out)
        return words(bytes.fromhex(frame["output"][2:])), gas, received

    def config(self, proxy: str) -> dict:
        answer = words(self.call(proxy, selector("getConfig()")))
        return {
            "scaling_x": str(answer[0]),
            "scaling_y": str(answer[1]),
            "token_x": "0x" + f"{answer[2]:040x}",
            "token_y": "0x" + f"{answer[3]:040x}",
            "supports_native_eth": bool(answer[4]),
            "is_token_x_weth": bool(answer[5]),
            "aggressive_commission_rate": str(answer[9]),
            "passive_order_payout_rate": str(answer[11]),
        }

    def wiring(self, proxy: str) -> dict:
        whitelisted = self.call(
            proxy, selector("whitelisted(address)") + address_word(TYCHO_ROUTER)
        )
        return {
            "lob": address_of(self.call(proxy, LOB_SELECTOR)),
            "quoter": address_of(self.call(proxy, QUOTER_SELECTOR)),
            "contract_quoter": address_of(self.call(proxy, CONTRACT_QUOTER_SELECTOR)),
            "router_whitelisted": bool(words(whitelisted)[0]),
        }

    def book(self, quoter: str, lob: str) -> dict:
        sides = {}
        for name, is_ask in (("asks", True), ("bids", False)):
            data = selector("assembleOrderbookFromOrders(address,bool,uint24)")
            data += address_word(lob) + word(int(is_ask)) + word(DEPTH)
            answer = words(self.call(quoter, data))
            count = answer[2]
            prices = answer[3 : 3 + count]
            shares = answer[4 + count : 4 + 2 * count]
            sides[name] = [[str(p), str(s)] for p, s in zip(prices, shares)]
        return sides


def sell_case(
    recorder: Recorder, proxy: str, config: dict, caller: str, amount_in: int
) -> dict:
    scaling_x = int(config["scaling_x"])
    quantity = amount_in // scaling_x
    data = selector("placeOrder(bool,uint128,uint72,uint128,bool,bool,bool,uint256)")
    data += (
        word(1) + word(quantity) + word(1) + word(MAX128) + word(1) + word(0) + word(1)
    )
    data += word(EXPIRES)
    token_in, token_out = sides(config)
    paid = quantity * scaling_x if token_in is None else amount_in
    (_, shares, _, _), gas, received = recorder.fill(
        proxy, data, caller, token_in, token_out, paid
    )
    return {
        "caller": caller,
        "sell_base": True,
        "amount_in": str(amount_in),
        "shares": str(shares),
        "amount_out": str(received),
        "gas": gas,
    }


def buy_case(
    recorder: Recorder,
    proxy: str,
    config: dict,
    caller: str,
    amount_in: int,
    limit: int,
) -> dict:
    scaling_y = int(config["scaling_y"])
    target = amount_in // scaling_y
    data = selector(
        "placeMarketOrderWithTargetValue(bool,uint128,uint72,uint128,bool,uint256)"
    )
    data += (
        word(0) + word(target) + word(limit) + word(MAX128) + word(1) + word(EXPIRES)
    )
    token_out, token_in = sides(config)
    paid = target * scaling_y if token_in is None else amount_in
    (shares, _, _), gas, received = recorder.fill(
        proxy, data, caller, token_in, token_out, paid
    )
    return {
        "caller": caller,
        "sell_base": False,
        "amount_in": str(amount_in),
        "shares": str(shares),
        "amount_out": str(received),
        "gas": gas,
    }


def sides(config: dict) -> tuple[str | None, str | None]:
    """Token X and token Y as the swap pays and receives them: None for the native side."""
    native = config["supports_native_eth"]
    token_x = None if native and config["is_token_x_weth"] else config["token_x"]
    token_y = None if native and not config["is_token_x_weth"] else config["token_y"]
    return token_x, token_y


def price_limit(levels: list[list[str]]) -> int:
    """Twice the worst ask, cut to the six significant digits a Hanji price may carry."""
    price = 2 * max(int(price) for price, _ in levels)
    digits = len(str(price))
    if digits <= 6:
        return price
    scale = 10 ** (digits - 6)
    return price // scale * scale


def fundable(config: dict) -> bool:
    """Whether the taker can be funded in both tokens: native ETH, or an ERC20 whose slots are
    known. The tokenized equities (0xb2… addresses) have no contract code to override storage
    in, so their markets are recorded without fills."""
    native = WRAPPED_NATIVE if config["supports_native_eth"] else None
    return all(
        token in SLOTS or token == native
        for token in (config["token_x"], config["token_y"])
    )


def record_market(recorder: Recorder, market: dict) -> dict:
    proxy = market["orderbookAddress"].lower()
    config = recorder.config(proxy)
    wiring = recorder.wiring(proxy)
    # A market without a contract quoter (the tokenized equities) is recorded without that book.
    no_quoter = int(wiring["contract_quoter"], 16) == 0
    books = {
        "direct": recorder.book(wiring["quoter"], wiring["lob"]),
        "contract": None
        if no_quoter
        else recorder.book(wiring["contract_quoter"], wiring["lob"]),
    }
    record = {
        "api": market,
        "config": config,
        "wiring": wiring,
        "books": books,
        "cases": [],
    }
    if not fundable(config):
        return record
    for caller, book in books.items():
        if book is None:
            continue
        bid_depth = sum(int(shares) for _, shares in book["bids"]) * int(
            config["scaling_x"]
        )
        for fraction in FRACTIONS if book["bids"] else ():
            amount_in = max(int(bid_depth * fraction), 1) + OFF_GRID
            record["cases"].append(
                sell_case(recorder, proxy, config, caller, amount_in)
            )
        asks = book["asks"]
        ask_value = sum(int(p) * int(s) for p, s in asks) * int(config["scaling_y"])
        for fraction in FRACTIONS if asks else ():
            amount_in = max(int(ask_value * fraction), 1) + OFF_GRID
            case = buy_case(
                recorder, proxy, config, caller, amount_in, price_limit(asks)
            )
            record["cases"].append(case)
    return record


def main() -> None:
    node = Node(os.environ["BASE_RPC_URL"])
    head = int(node.rpc("eth_blockNumber", []), 16)
    block = hex(int(sys.argv[1])) if len(sys.argv) > 1 else hex(head - 1)
    header = node.rpc("eth_getBlockByNumber", [block, False])
    request = urllib.request.Request(API, headers={"User-Agent": "curl/8"})
    markets = json.load(urllib.request.urlopen(request))
    recorder = Recorder(node, block)
    fixture = {
        "block": int(block, 16),
        "timestamp": int(header["timestamp"], 16),
        "markets": [record_market(recorder, market) for market in markets],
    }
    json.dump(fixture, sys.stdout, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
