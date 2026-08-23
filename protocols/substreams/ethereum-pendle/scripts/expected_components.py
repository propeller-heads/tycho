"""Derives the `expected_components` block of `integration_test.tycho.yaml` from chain.

Mirrors what `map_market_components` does at a market's creation block: reads the market's
tokens and expiry, profiles the SY behind it, and classifies each of its entry and exit tokens
from a one-unit `previewDeposit` / `previewRedeem` probe. Everything is read at the creation
block, which is the block the Substreams `eth_call`s run at.

Independent of the Substreams output, so a reviewer can regenerate the fixture rather than
trust it.

    python3 expected_components.py <market> <creation_block>
"""
import json
import os
import sys
import time
import urllib.request

RPC = os.environ.get("RPC_URL", "https://eth.drpc.org")
NULL = "0x0000000000000000000000000000000000000000"

EXPIRY = "0xe184c9be"
READ_TOKENS = "0x2c8ce6bc"
DECIMALS = "0x313ce567"
ASSET_INFO = "0xa40bee50"
EXCHANGE_RATE = "0x3ba0b9a9"
GET_TOKENS_IN = "0x213cae63"
GET_TOKENS_OUT = "0x071bc3c9"
PREVIEW_DEPOSIT = "0xb8f82b26"
PREVIEW_REDEEM = "0xcbe52ae3"

# `CreateNewMarket`: the V3+ generations carry `lnFeeRateRoot`, the original factory does not.
CREATE_NEW_MARKET = [
    "0xae811fae25e2770b6bd1dcb1475657e8c3a976f91d1ebf081271db08eef920af",
    "0x166ae5f55615b65bbd9a2496e98d4e4d78ca15bd6127c0fe2dc27b76f6c03143",
]

# `sy.rs`: a prediction is accepted when it is within 1/1e6 of the probe.
TOLERANCE_RECIPROCAL = 10**6


def call(to, data, block):
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{"to": to, "data": data}, hex(block)],
        }
    ).encode()
    req = urllib.request.Request(
        RPC, data=body, headers={"content-type": "application/json", "user-agent": "curl/8.0"}
    )
    for _ in range(4):
        try:
            r = json.load(urllib.request.urlopen(req, timeout=60))
            return r.get("result")
        except Exception:
            time.sleep(0.5)
    return None


def logs(block, market):
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getLogs",
            "params": [
                {
                    "fromBlock": hex(block),
                    "toBlock": hex(block),
                    "topics": [CREATE_NEW_MARKET, "0x" + market[2:].rjust(64, "0")],
                }
            ],
        }
    ).encode()
    req = urllib.request.Request(
        RPC, data=body, headers={"content-type": "application/json", "user-agent": "curl/8.0"}
    )
    return json.load(urllib.request.urlopen(req, timeout=60))["result"]


def words(d):
    d = d[2:]
    return [d[i : i + 64] for i in range(0, len(d), 64)]


def addr(word):
    return "0x" + word[-40:]


def num(x):
    return int(x, 16)


def token_list(res):
    if not res:
        return []
    ws = words(res)
    n = num(ws[1])
    return [addr(x) for x in ws[2 : 2 + n]]


def arg(value):
    return hex(value)[2:].rjust(64, "0")


def close(observed, predicted):
    return bool(predicted) and abs(observed - predicted) * TOLERANCE_RECIPROCAL <= predicted


def classify(observed, one_to_one, index_rate):
    """`sy.rs::classify` — a tie between the two predictions goes to `index_rate`."""
    if close(observed, one_to_one) and not close(one_to_one, index_rate):
        return "one_to_one"
    if close(observed, index_rate):
        return "index_rate"
    return None


def profile_sy(sy, block):
    sy_decimals = num(call(sy, DECIMALS, block))
    ws = words(call(sy, ASSET_INFO, block))
    asset, asset_decimals = addr(ws[1]), num(ws[2])
    rate = num(call(sy, EXCHANGE_RATE, block))

    classes_in = {}
    for token in token_list(call(sy, GET_TOKENS_IN, block)):
        if token == NULL:
            continue  # native ETH has no `decimals()`, so it is never classified
        decimals = call(token, DECIMALS, block)
        if decimals is None:
            continue
        probe = call(sy, PREVIEW_DEPOSIT + token[2:].rjust(64, "0") + arg(10 ** num(decimals)), block)
        if probe is None:
            continue
        cls = classify(num(probe), 10**sy_decimals, 10**asset_decimals * 10**18 // rate)
        if cls:
            classes_in[token] = cls

    classes_out = {}
    for token in token_list(call(sy, GET_TOKENS_OUT, block)):
        if token == NULL:
            continue
        decimals = call(token, DECIMALS, block)
        if decimals is None:
            continue
        probe = call(sy, PREVIEW_REDEEM + token[2:].rjust(64, "0") + arg(10**sy_decimals), block)
        if probe is None:
            continue
        asset_amount = 10**sy_decimals * rate // 10**18
        cls = classify(
            num(probe),
            10 ** num(decimals),
            asset_amount * 10 ** num(decimals) // 10**asset_decimals,
        )
        if cls:
            classes_out[token] = cls

    return sy_decimals, asset, asset_decimals, classes_in, classes_out


def be(value):
    """Signed big-endian, the encoding `to_signed_bytes_be` produces."""
    if value == 0:
        return "0x00"
    length = (value.bit_length() + 8) // 8
    return "0x" + value.to_bytes(length, "big").hex()


def main():
    market, block = sys.argv[1].lower(), int(sys.argv[2])

    creation = logs(block, market)[0]
    factory = creation["address"].lower()
    creation_tx = creation["transactionHash"]
    ws = words(creation["data"])
    scalar_root, initial_anchor = num(ws[0]), num(ws[1])
    # The original factory's event stops after `initialAnchor`; its fee is read from
    # `getMarketConfig` instead, and the attribute is zero.
    ln_fee_rate_root = num(ws[2]) if len(ws) > 2 else 0

    ws = words(call(market, READ_TOKENS, block))
    sy, pt, yt = addr(ws[0]), addr(ws[1]), addr(ws[2])
    expiry = num(call(market, EXPIRY, block))
    sy_decimals, asset, asset_decimals, classes_in, classes_out = profile_sy(sy, block)

    print(f"      - id: \"{market}\"")
    print("        tokens:")
    for token in (sy, pt, yt):
        print(f"          - \"{token}\"")
    print("        static_attributes:")
    print(f"          scalar_root: \"{be(scalar_root)}\"")
    print(f"          initial_anchor: \"{be(initial_anchor)}\"")
    print(f"          expiry: \"{be(expiry)}\"")
    print(f"          factory: \"{factory}\"")
    print(f"          ln_fee_rate_root_at_creation: \"{be(ln_fee_rate_root)}\"")
    print(f"          sy_address: \"{sy}\"")
    print(f"          pt_address: \"{pt}\"")
    print(f"          yt_address: \"{yt}\"")
    print(f"          sy_decimals: \"{be(sy_decimals)}\"")
    print(f"          asset_decimals: \"{be(asset_decimals)}\"")
    print(f"        creation_tx: \"{creation_tx}\"")
    print()

    tokens = [sy] + [t for t in list(classes_in) + list(classes_out) if t != sy]
    print(f"      - id: \"{sy}\"")
    print("        tokens:")
    seen = []
    for token in tokens:
        if token not in seen:
            seen.append(token)
            print(f"          - \"{token}\"")
    print("        static_attributes:")
    print(f"          asset_address: \"{asset}\"")
    print(f"          asset_decimals: \"{be(asset_decimals)}\"")
    print(f"          sy_decimals: \"{be(sy_decimals)}\"")
    for token, cls in classes_in.items():
        print(f"          token_in_class_{token}: \"0x{cls.encode().hex()}\"")
    for token, cls in classes_out.items():
        print(f"          token_out_class_{token}: \"0x{cls.encode().hex()}\"")
    print(f"        creation_tx: \"{creation_tx}\"")


if __name__ == "__main__":
    main()
