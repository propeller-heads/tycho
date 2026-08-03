#!/usr/bin/env python3
"""Record Balancer V3 pool state and on-chain quotes for the native-maths parity test.

For every pool the script reads the state that `balancer-maths-rust` needs (from the Vault and the
pool's own getters) and the `amountOut` that `BatchRouter.querySwapExactIn` returns for a spread of
swap sizes, all at one fixed block. The result is written as JSON that
`tests/balancer_v3_native_parity.rs` replays without any RPC access.

Integers are emitted as decimal strings so the dataset stays readable in review and does not depend
on how `alloy` serializes `U256`.

Usage:
    ./fetch_native_parity_dataset.py <rpc-url> <block> > native_parity_dataset.json

Requires `cast` (Foundry) on PATH and an archive RPC, since the getters are read at a past block.
`querySwapExactIn` reverts unless `tx.origin` is the zero address, hence `--from 0x0…0`.
"""

from __future__ import annotations

import concurrent.futures
import json
import subprocess
import sys

VAULT = "0xbA1333333333a1BA1108E8412f11850A5C319bA9"
BATCH_ROUTER = "0x136f1EFcC3f8f88516B9E94110D56FDBfB1778d1"
ZERO = "0x" + "0" * 40

QUERY_SWAP = (
    "querySwapExactIn((address,(address,address,bool)[],uint256,uint256)[],address,bytes)"
    "(uint256[],address[],uint256[])"
)
POOL_TOKEN_INFO = (
    "getPoolTokenInfo(address)(address[],(uint8,address,bool)[],uint256[],uint256[])"
)
# aggregate_swap_fee is field 2; field 0 is LiquidityManagement, whose first flag is
# disableUnbalancedLiquidity. Vault.getAggregateSwapFeePercentage reverts "Not implemented".
POOL_CONFIG = (
    "getPoolConfig(address)"
    "(((bool,bool,bool,bool),uint256,uint256,uint256,uint40,uint32,bool,bool,bool,bool))"
)
WEIGHTED_DYNAMIC = (
    "getWeightedPoolDynamicData()((uint256[],uint256[],uint256,uint256,bool,bool,bool))"
)
WEIGHTED_IMMUTABLE = "getWeightedPoolImmutableData()((address[],uint256[],uint256[]))"
STABLE_DYNAMIC = (
    "getStablePoolDynamicData()"
    "((uint256[],uint256[],uint256,uint256,uint256,uint256,uint256,uint256,uint32,uint32,"
    "bool,bool,bool,bool))"
)
STABLE_IMMUTABLE = "getStablePoolImmutableData()((address[],uint256[],uint256))"

# Swap sizes as basis points of the input token's raw balance: 0.01% up to half the pool.
SWAP_SIZES_BPS = (1, 10, 100, 1000, 5000)

WEIGHTED_POOLS = (
    "0x1846c6cbe0d433e152fa358e5ff27968e18bce7c",
    "0xf91c11ba4220b7a72e1dc5e92f2b48d3fdf62726",
    "0x8115054a485d7775e13a8a420dd986ff595824fa",
    "0x571bea0e99e139cd0b6b7d9352ca872dfe0d72dd",
    "0xbda917a67c7d9ae67da92c4ea87e10e5d6c11b54",
    "0x1535d7ca00323aa32bd62aeddf7ca651e4b95966",
)
STABLE_POOLS = (
    "0x57c23c58B1D8C3292c15BEcF07c62C5c52457A42",
    "0xc4Ce391d82D164c166dF9c8336DDF84206b2F812",
    "0x4AB7aB316D43345009B2140e0580B072eEc7DF16",
    "0x89BB794097234E5E930446C0CeC0ea66b35D7570",
    "0x5Dd88b3AA3143173eb26552923922bDf33f50949",
)


def call(rpc: str, block: str, *args: str, sender: str | None = None):
    """Runs `cast call --json`, returning the decoded JSON or None when the call reverts."""
    cmd = ["cast", "call", "--rpc-url", rpc, "--block", block]
    if sender:
        cmd += ["--from", sender]
    cmd += [*args, "--json"]
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        return None
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return None


def dec(value) -> str:
    """Normalizes cast's mix of ints and strings into a decimal string."""
    return str(int(value))


def fetch_pool(rpc: str, block: str, pool: str, kind: str):
    info = call(rpc, block, VAULT, POOL_TOKEN_INFO, pool)
    config = call(rpc, block, VAULT, POOL_CONFIG, pool)
    if kind == "WEIGHTED":
        dynamic = call(rpc, block, pool, WEIGHTED_DYNAMIC)
        immutable = call(rpc, block, pool, WEIGHTED_IMMUTABLE)
    else:
        dynamic = call(rpc, block, pool, STABLE_DYNAMIC)
        immutable = call(rpc, block, pool, STABLE_IMMUTABLE)
    if not (info and config and dynamic and immutable):
        print(f"skipping {pool}: a getter reverted", file=sys.stderr)
        return None

    tokens, _token_info, balances_raw, _live = info
    dynamic, immutable, config = dynamic[0], immutable[0], config[0]
    balances_raw = [int(balance) for balance in balances_raw]
    if any(balance == 0 for balance in balances_raw):
        print(f"skipping {pool}: uninitialized (a balance is zero)", file=sys.stderr)
        return None

    state = {
        "pool_address": pool.lower(),
        "pool_type": kind,
        "tokens": [token.lower() for token in tokens],
        "scaling_factors": [dec(factor) for factor in immutable[1]],
        "token_rates": [dec(rate) for rate in dynamic[1]],
        "balances_live_scaled_18": [dec(balance) for balance in dynamic[0]],
        "swap_fee": dec(dynamic[2]),
        "aggregate_swap_fee": dec(config[2]),
        "total_supply": dec(dynamic[3]),
        "supports_unbalanced_liquidity": not bool(config[0][0]),
        "hook_type": None,
    }
    if kind == "WEIGHTED":
        state["weights"] = [dec(weight) for weight in immutable[2]]
    else:
        state["amp"] = dec(dynamic[5])

    swaps = []
    for bps in SWAP_SIZES_BPS:
        for token_in_index, token_out_index in ((0, 1), (1, 0)):
            amount = balances_raw[token_in_index] * bps // 10_000
            if amount == 0:
                continue
            token_in = state["tokens"][token_in_index]
            token_out = state["tokens"][token_out_index]
            path = f"[({token_in},[({pool},{token_out},false)],{amount},0)]"
            quote = call(
                rpc, block, BATCH_ROUTER, QUERY_SWAP, path, ZERO, "0x", sender=ZERO
            )
            if not quote:
                print(
                    f"skipping {pool} {bps}bps {token_in}->{token_out}: query reverted",
                    file=sys.stderr,
                )
                continue
            swaps.append(
                {
                    "token_in": token_in,
                    "token_out": token_out,
                    "amount": str(amount),
                    "chain": dec(quote[0][0]),
                }
            )

    if not swaps:
        print(f"skipping {pool}: no quote succeeded", file=sys.stderr)
        return None
    return {"state": state, "swaps": swaps}


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    rpc, block = sys.argv[1], sys.argv[2]

    pools = [(pool, "WEIGHTED") for pool in WEIGHTED_POOLS]
    pools += [(pool, "STABLE") for pool in STABLE_POOLS]

    entries = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool_executor:
        futures = [
            pool_executor.submit(fetch_pool, rpc, block, pool, kind)
            for pool, kind in pools
        ]
        for future in futures:
            entry = future.result()
            if entry:
                entries.append(entry)

    if not entries:
        print("no pool yielded usable data", file=sys.stderr)
        return 1

    json.dump(entries, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    swap_count = sum(len(entry["swaps"]) for entry in entries)
    print(
        f"recorded {len(entries)} pools and {swap_count} swaps at block {block}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
