#!/usr/bin/env python3
"""Dump a Camelot V3 swap into a replay fixture for `camelot_v3::state::tests`.

Given the hash of an Arbitrum One transaction that swapped on a Camelot V3 (Algebra V1.9)
pool, the script reads from an archive RPC everything `CamelotV3State` needs to reproduce the
swap: the pool's state at the parent block, every initialized tick in the tick-table rows
around the swap's path, the oracle timepoints the fee recomputation reads, and the outcome the
chain recorded (the `Swap` and `Fee` events and the post-swap storage). The fixture is written
as `<pool prefix>_<block>.json` next to this script, in the shape the test deserializes.

The transaction must be the only one touching the pool in its block, so that the parent-block
state is the swap's pre-state. An exact-input swap replays to the wei; an exact-output swap
replayed as exact input differs by the rounding of its input, so the replay test rejects it.

Only the standard library is needed:

    python3 dump_fixture.py --rpc-url https://arbitrum.gateway.tenderly.co 0x<tx hash>
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

# Function selectors and event topics: keccak256 of the signature, as `cast sig` and
# `cast keccak` print them.
TOKEN0 = "0x0dfe1681"  # token0()
TOKEN1 = "0xd21220a7"  # token1()
DECIMALS = "0x313ce567"  # decimals()
DATA_STORAGE_OPERATOR = "0x29047dfa"  # dataStorageOperator()
TICK_SPACING = "0xd0c93a7c"  # tickSpacing()
TICKS = "0xf30dba93"  # ticks(int24)
TICK_TABLE = "0xc677e3e0"  # tickTable(int16)
# Swap(address,address,int256,int256,uint160,uint128,int24)
SWAP_TOPIC = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
# Fee(uint16,uint16)
FEE_TOPIC = "0x8a89de70856bccec096661388f305b9a75f5f65cb0d8a0e1e803c39dabedb57f"

# Storage layout, as `protocols/substreams/arbitrum-camelot-v3/src/camelot.rs` documents it.
GLOBAL_STATE_SLOT = 2
LIQUIDITY_SLOT = 3
RING_SIZE = 65_536
FEE_CONFIG_ZTO_SLOT = 2 * RING_SIZE
FEE_CONFIG_OTZ_SLOT = 2 * RING_SIZE + 1

# `DataStorage.WINDOW`: the averaging window of the adaptive fee.
WINDOW = 86_400
MIN_TICK = -887_272
MAX_TICK = 887_272
# Tick-table rows (256 ticks each) fetched on either side of the swap's path, so that the
# tests can also quote a second swap of the same size on the post-swap state.
TICK_ROW_MARGIN = 8


class Rpc:
    """A minimal JSON-RPC client with retries for transient transport errors."""

    def __init__(self, url: str) -> None:
        self.url = url
        self.next_id = 0

    def call(self, method: str, params: list) -> object:
        self.next_id += 1
        body = json.dumps(
            {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        ).encode()
        request = urllib.request.Request(
            self.url, data=body, headers={"content-type": "application/json"}
        )
        for attempt in range(3):
            try:
                with urllib.request.urlopen(request, timeout=60) as response:
                    reply = json.load(response)
                break
            except (urllib.error.URLError, TimeoutError) as err:
                if attempt == 2:
                    sys.exit(f"{method}: {err}")
                time.sleep(1 + attempt)
        if "error" in reply:
            sys.exit(f"{method} {params}: {reply['error']}")
        return reply["result"]

    def storage(self, address: str, slot: int, block: int) -> bytes:
        word = self.call("eth_getStorageAt", [address, hex(slot), hex(block)])
        return bytes.fromhex(word[2:].zfill(64))

    def eth_call(self, address: str, selector: str, argument: int, block: int) -> bytes:
        data = selector + abi_word(argument).hex()
        return bytes.fromhex(self.call("eth_call", [{"to": address, "data": data}, hex(block)])[2:])


def abi_word(value: int) -> bytes:
    """A 32-byte two's complement word, as the ABI encodes integers."""
    return (value % 2**256).to_bytes(32, "big")


def uint(data: bytes) -> int:
    return int.from_bytes(data, "big")


def sint(data: bytes) -> int:
    return int.from_bytes(data, "big", signed=True)


def low_end(word: bytes, offset: int, length: int) -> bytes:
    """Bytes `[offset, offset + length)` counted from the low end of a storage word."""
    return word[32 - offset - length : 32 - offset]


def address_of(word: bytes) -> str:
    return "0x" + word[12:].hex()


def lte(a: int, b: int, current_time: int) -> bool:
    """`DataStorage.lteConsideringOverflow` for 32-bit timestamps."""
    res = a > current_time
    if res == (b > current_time):
        res = a <= b
    return res


class Pool:
    """Reads of one pool and its operator at one block."""

    def __init__(self, rpc: Rpc, address: str, operator: str, block: int) -> None:
        self.rpc = rpc
        self.address = address
        self.operator = operator
        self.block = block
        self.headers: dict[int, tuple[bool, int]] = {}

    def global_state(self) -> dict:
        word = self.rpc.storage(self.address, GLOBAL_STATE_SLOT, self.block)
        return {
            "sqrt_price": uint(low_end(word, 0, 20)),
            "tick": sint(low_end(word, 20, 3)),
            "fee_zto": uint(low_end(word, 23, 2)),
            "fee_otz": uint(low_end(word, 25, 2)),
            "timepoint_index": uint(low_end(word, 27, 2)),
        }

    def liquidity(self) -> tuple[int, int]:
        """The pool's liquidity and its `volumePerLiquidityInBlock`."""
        word = self.rpc.storage(self.address, LIQUIDITY_SLOT, self.block)
        return uint(low_end(word, 0, 16)), uint(low_end(word, 16, 16))

    def fee_config(self, slot: int) -> str:
        return "0x" + self.rpc.storage(self.operator, slot, self.block).hex()

    def timepoint_header(self, index: int) -> tuple[bool, int]:
        """`initialized` and `blockTimestamp` of the ring slot `index`."""
        index %= RING_SIZE
        if index not in self.headers:
            word = self.rpc.storage(self.operator, 2 * index, self.block)
            self.headers[index] = (word[31] != 0, uint(word[27:31]))
        return self.headers[index]

    def timepoint(self, index: int) -> str:
        """Both storage words of the ring slot `index`, as the `timepoints/{index}` attribute."""
        index %= RING_SIZE
        first = self.rpc.storage(self.operator, 2 * index, self.block)
        second = self.rpc.storage(self.operator, 2 * index + 1, self.block)
        return "0x" + (first + second).hex()

    def tick_row(self, row: int) -> int:
        return uint(self.rpc.eth_call(self.address, TICK_TABLE, row, self.block))

    def tick(self, tick: int) -> tuple[int, int]:
        """`liquidityTotal` and `liquidityDelta` of an initialized tick."""
        words = self.rpc.eth_call(self.address, TICKS, tick, self.block)
        return uint(words[0:32]), sint(words[32:64])


def initialized_ticks(pool: Pool, rows: list[int]) -> list[tuple[int, str, str]]:
    ticks = []
    for row in rows:
        bitmap = pool.tick_row(row)
        for bit in range(256):
            if bitmap >> bit & 1:
                tick = row * 256 + bit
                total, delta = pool.tick(tick)
                ticks.append((tick, str(total), str(delta)))
    return sorted(ticks)


def timepoints_read_by_the_fee(pool: Pool, index: int, time: int) -> list[tuple[int, str]]:
    """The ring slots `getAverages` and `write` read for a write at `time` after `index`.

    That is the newest slot, the one before it, the oldest slot (which the write overwrites
    once the ring has wrapped) and, when the window start precedes the newest slot, the two
    slots around `time - WINDOW`, found as the contract's `binarySearch` finds them.
    """
    wanted = {index}
    if pool.timepoint_header(index - 1)[0]:
        wanted.add((index - 1) % RING_SIZE)
    oldest = (index + 1) % RING_SIZE if pool.timepoint_header(index + 1)[0] else 0
    wanted.add(oldest)

    target = (time - WINDOW) % 2**32
    if not lte(pool.timepoint_header(index)[1], target, time):
        left = oldest
        right = index if index >= oldest else index + RING_SIZE
        while True:
            current = (left + right) // 2
            initialized, timestamp = pool.timepoint_header(current)
            if not initialized:
                left = current + 1
            elif not lte(timestamp, target, time):
                right = current - 1
            else:
                initialized_after, timestamp_after = pool.timepoint_header(current + 1)
                if not initialized_after or lte(target, timestamp_after, time):
                    wanted.update({current % RING_SIZE, (current + 1) % RING_SIZE})
                    break
                left = current + 1
    return [(slot, pool.timepoint(slot)) for slot in sorted(wanted)]


def swap_logs(receipt: dict, pool: str) -> tuple[dict, dict | None]:
    """The pool's `Swap` log and its `Fee` log, if any, rejecting any other pool log."""
    swap = None
    fee = None
    for log in receipt["logs"]:
        if log["address"].lower() != pool:
            continue
        topic = log["topics"][0]
        if topic == SWAP_TOPIC and swap is None:
            swap = log
        elif topic == FEE_TOPIC and fee is None:
            fee = log
        else:
            sys.exit(f"the transaction touched {pool} beyond one swap: it emitted log {topic}")
    if swap is None:
        sys.exit(f"the transaction has no Swap log of {pool}")
    return swap, fee


def check_pool_untouched_by_others(rpc: Rpc, block: int, tx: str, *addresses: str) -> None:
    for address in addresses:
        logs = rpc.call(
            "eth_getLogs", [{"fromBlock": hex(block), "toBlock": hex(block), "address": address}]
        )
        others = {log["transactionHash"] for log in logs} - {tx}
        if others:
            sys.exit(
                f"{address} was also touched by {', '.join(sorted(others))} in block {block}, so "
                "the parent-block state is not the swap's pre-state"
            )


def dump(rpc: Rpc, tx: str, pool_filter: str | None) -> dict:
    receipt = rpc.call("eth_getTransactionReceipt", [tx])
    if receipt is None or receipt["status"] != "0x1":
        sys.exit(f"{tx} is unknown or reverted")
    block = int(receipt["blockNumber"], 16)
    pools = sorted(
        {log["address"].lower() for log in receipt["logs"] if log["topics"][0] == SWAP_TOPIC}
    )
    if pool_filter is not None:
        pools = [pool for pool in pools if pool == pool_filter.lower()]
    if len(pools) != 1:
        sys.exit(f"{tx} swapped on {len(pools)} pools: {pools}; pass --pool to pick one")
    pool_address = pools[0]
    swap, fee = swap_logs(receipt, pool_address)

    timestamp = int(rpc.call("eth_getBlockByNumber", [hex(block), False])["timestamp"], 16)
    operator = address_of(rpc.eth_call(pool_address, DATA_STORAGE_OPERATOR, 0, block - 1))
    check_pool_untouched_by_others(rpc, block, tx, pool_address, operator)

    pre = Pool(rpc, pool_address, operator, block - 1)
    post = Pool(rpc, pool_address, operator, block)
    tokens = []
    for selector in (TOKEN0, TOKEN1):
        token = address_of(rpc.eth_call(pool_address, selector, 0, block - 1))
        decimals = uint(rpc.eth_call(token, DECIMALS, 0, block - 1))
        tokens.append({"address": token, "decimals": decimals})
    tick_spacing = sint(rpc.eth_call(pool_address, TICK_SPACING, 0, block - 1))

    state_before = pre.global_state()
    liquidity_before, volume_before = pre.liquidity()
    data = bytes.fromhex(swap["data"][2:])
    amount0, amount1 = sint(data[0:32]), sint(data[32:64])
    sqrt_price_after, liquidity_after = uint(data[64:96]), uint(data[96:128])
    tick_after = sint(data[128:160])

    # The rows the swap walked, with a margin, and the rows that hold full-range positions.
    first_row = min(state_before["tick"], tick_after) >> 8
    last_row = max(state_before["tick"], tick_after) >> 8
    rows = set(range(first_row - TICK_ROW_MARGIN, last_row + TICK_ROW_MARGIN + 1))
    rows = {row for row in rows if MIN_TICK >> 8 <= row <= MAX_TICK >> 8}
    rows.update({MIN_TICK >> 8, MAX_TICK >> 8})
    ticks = initialized_ticks(pre, sorted(rows))
    # Without a position at or beyond the fetched rows, a zero-net tick marks the boundary,
    # so that a quote stops with an error there instead of walking into unknown ticks.
    lowest = max(first_row - TICK_ROW_MARGIN, MIN_TICK >> 8) * 256
    highest = (min(last_row + TICK_ROW_MARGIN, MAX_TICK >> 8) + 1) * 256 - 1
    stand_in_ticks = []
    if not any(tick <= lowest for tick, _, _ in ticks):
        stand_in_ticks.append((lowest, "0", "0"))
    if not any(tick >= highest for tick, _, _ in ticks):
        stand_in_ticks.append((highest, "0", "0"))

    state_after = post.global_state()
    liquidity_post, volume_after = post.liquidity()
    if (state_after["sqrt_price"], state_after["tick"], liquidity_post) != (
        sqrt_price_after,
        tick_after,
        liquidity_after,
    ):
        sys.exit("the post-swap storage does not match the Swap event")
    wrote_timepoint = state_after["timepoint_index"] != state_before["timepoint_index"]
    if wrote_timepoint != (fee is not None):
        sys.exit("a Fee event must accompany a timepoint write and nothing else")
    fee_event = None
    if fee is not None:
        fee_data = bytes.fromhex(fee["data"][2:])
        fee_event = (uint(fee_data[0:32]), uint(fee_data[32:64]))

    return {
        "pool": pool_address,
        "operator": operator,
        "tx": tx,
        "block": block,
        "timestamp": timestamp,
        "tick_spacing": tick_spacing,
        "token0": tokens[0],
        "token1": tokens[1],
        "pre": {
            "block": block - 1,
            "liquidity": str(liquidity_before),
            "sqrt_price": str(state_before["sqrt_price"]),
            "tick": state_before["tick"],
            "fee_zto": state_before["fee_zto"],
            "fee_otz": state_before["fee_otz"],
            "timepoint_index": state_before["timepoint_index"],
            "volume_per_liquidity_in_block": str(volume_before),
            "fee_config_zto": pre.fee_config(FEE_CONFIG_ZTO_SLOT),
            "fee_config_otz": pre.fee_config(FEE_CONFIG_OTZ_SLOT),
            "ticks": ticks,
            "stand_in_ticks": stand_in_ticks,
            "timepoints": timepoints_read_by_the_fee(
                pre, state_before["timepoint_index"], timestamp
            ),
        },
        "swap": {
            "amount0": str(amount0),
            "amount1": str(amount1),
            "sqrt_price_after": str(sqrt_price_after),
            "liquidity_after": str(liquidity_after),
            "tick_after": tick_after,
            "fee_event": fee_event,
        },
        "post": {
            "liquidity": str(liquidity_post),
            "volume_per_liquidity_in_block": str(volume_after),
            "timepoint_index": state_after["timepoint_index"],
            "fee_zto": state_after["fee_zto"],
            "fee_otz": state_after["fee_otz"],
            "sqrt_price": str(state_after["sqrt_price"]),
            "tick": state_after["tick"],
            "new_timepoint": post.timepoint(state_after["timepoint_index"])
            if wrote_timepoint
            else None,
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("tx", help="hash of the swap transaction")
    parser.add_argument("--rpc-url", required=True, help="Arbitrum One archive JSON-RPC endpoint")
    parser.add_argument(
        "--pool", help="the pool to dump when the transaction swapped on several Camelot pools"
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=Path(__file__).parent,
        help="where to write the fixture (default: next to this script)",
    )
    args = parser.parse_args()

    fixture = dump(Rpc(args.rpc_url), args.tx.lower(), args.pool)
    path = args.out_dir / f"{fixture['pool'][2:6]}_{fixture['block']}.json"
    with open(path, "w") as file:
        json.dump(fixture, file, indent=1)
        file.write("\n")
    print(path)


if __name__ == "__main__":
    main()
