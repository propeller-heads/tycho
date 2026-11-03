"""How often a pAMM's on-chain price is refreshed, and how updates relate to swaps.

usage: update_cadence.py [CONTRACT] --blocks N [--to-block B]
           [--update-to ADDR ...] [--update-selector 0x.. ... | --update-prefix 0x..]
           [--update-arg INDEX=VALUE ...]
           [--swap-to ADDR ...] [--swap-selector 0x.. ...] [--sample-fees K]

Uses trace_filter (Erigon/Reth-style trace API) to collect every successful call, top-level or
internal, into the update and swap contracts over the last N blocks. Updates and swaps may hit
different contracts: price pushes often go to a shared registry or oracle (--update-to) while
swaps go to the venue (--swap-to). CONTRACT is short for both when they are the same contract.

An update is a call into an --update-to address whose calldata starts with one of the
--update-selector values, or with --update-prefix (for selector-less calldata such as `0x00`
followed by packed words), and whose every --update-arg matches. --update-arg INDEX=VALUE
compares the ABI word INDEX (0 = the first word after the 4-byte selector) with VALUE as an
integer, so an address matches whatever its padding: for a registry shared by several venues,
`updateState(target, lane, ...)` is `--update-arg 0=TARGET`. A swap is a call into a --swap-to
address with one of the --swap-selector values. Selectors and arguments are repeatable.

Without an update filter or a swap selector it prints the call census (address, selector,
top-level, caller) so you can pick them: price updates are usually top-level calls from a few
EOAs; swaps are usually internal calls from the router. With both it reports, for each update
contract and for all of them together:
- share of blocks with an update, gaps between update blocks (median/p90/max);
- swap blocks with an update earlier in the same block, the tx distance from the last such
  update to the first swap, and how many update blocks hold no swap;
- the updaters;
- with --sample-fees K, the effective priority fee of K update transactions spread over the
  window: min(maxPriorityFeePerGas, maxFeePerGas - baseFee), or gasPrice - baseFee for legacy
  transactions. 0 on most updates suggests they ride in bundles.
"""

import argparse
import json
import os
import statistics
import sys
import time
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from itertools import pairwise

CHUNK = 250
BATCH = 20


def rpc_url():
    for name in ("ETH_RPC_URL", "MAINNET_RPC_URL"):
        if os.environ.get(name):
            return os.environ[name]
    sys.exit("set ETH_RPC_URL to the chain the contract lives on (foundry convention)")


def rpc_batch(calls):
    """Results of [(method, params)], in order; exits on a JSON-RPC error."""
    results = []
    for start in range(0, len(calls), BATCH):
        part = calls[start : start + BATCH]
        payload = [
            {"jsonrpc": "2.0", "id": i, "method": m, "params": p} for i, (m, p) in enumerate(part)
        ]
        req = urllib.request.Request(
            rpc_url(), json.dumps(payload).encode(), {"Content-Type": "application/json"}
        )
        for attempt in range(8):
            try:
                replies = json.load(urllib.request.urlopen(req, timeout=300))
                break
            except urllib.error.HTTPError as err:
                if err.code not in (429, 503) or attempt == 7:
                    raise
                time.sleep(0.5 * 2**attempt)
        for reply, (method, params) in zip(sorted(replies, key=lambda r: r["id"]), part):
            if "error" in reply:
                sys.exit(f"{method} {params}: {reply['error']}")
            results.append(reply["result"])
    return results


def rpc(method, params):
    return rpc_batch([(method, params)])[0]


def collect(addresses, first, last):
    """Successful calls into `addresses` in blocks first..last, in block and trace order."""
    queries = [
        ("trace_filter", [{"fromBlock": hex(s), "toBlock": hex(min(s + CHUNK - 1, last)),
                           "toAddress": sorted(addresses)}])
        for s in range(first, last + 1, CHUNK)
    ]  # fmt: skip
    calls = []
    for traces in rpc_batch(queries):
        for t in traces:
            if t.get("type") != "call" or t.get("error"):
                continue
            calls.append(
                {
                    "block": t["blockNumber"],
                    "pos": t["transactionPosition"],
                    "tx": t["transactionHash"],
                    "from": t["action"]["from"].lower(),
                    "to": t["action"]["to"].lower(),
                    "input": t["action"]["input"].lower(),
                    "top_level": not t["traceAddress"],
                }
            )
    return calls


def parse_arg_filters(values):
    """['0=0xabc', ...] -> {0: 0xabc}."""
    filters = {}
    for value in values:
        index, _, expected = value.partition("=")
        if not index.isdigit() or not expected:
            sys.exit(f"--update-arg {value}: expected INDEX=VALUE, e.g. 0=0x2086...")
        filters[int(index)] = int(expected, 0)
    return filters


def calldata_word(data, index):
    """ABI word `index` after the selector, or None when the calldata is shorter."""
    word = data[10 + 64 * index : 10 + 64 * (index + 1)]
    return int(word, 16) if len(word) == 64 else None


def is_update(call, a):
    if call["to"] not in a.update_to:
        return False
    if a.update_prefix:
        if not call["input"].startswith(a.update_prefix):
            return False
    elif a.update_selector and call["input"][:10] not in a.update_selector:
        return False
    return all(calldata_word(call["input"], i) == v for i, v in a.update_arg.items())


def is_swap(call, a):
    return call["to"] in a.swap_to and call["input"][:10] in a.swap_selector


def percentile(values, q):
    return values[min(len(values) - 1, int(len(values) * q))] if values else None


def report_census(calls):
    census = Counter((c["to"], c["input"][:10], c["top_level"], c["from"]) for c in calls)
    blocks = defaultdict(set)
    for c in calls:
        blocks[(c["to"], c["input"][:10])].add(c["block"])
    print("calls | to | selector | top-level | caller")
    for (to, selector, top, sender), n in census.most_common(40):
        print(f"{n:5d} | {to} | {selector} | {top!s:5} | {sender}")
    for (to, selector), b in sorted(blocks.items(), key=lambda kv: -len(kv[1]))[:40]:
        print(f"{to} {selector}: in {len(b)} distinct blocks")


def report_swap_relation(updates, swaps):
    distances, before_counts = Counter(), []
    for block, swap_calls in swaps.items():
        first_swap = min(c["pos"] for c in swap_calls)
        before = [c["pos"] for c in updates.get(block, []) if c["pos"] < first_swap]
        if before:
            gap = first_swap - max(before)
            before_counts.append(gap)
            distances[f"{gap} tx before" if gap < 4 else "4+ tx before"] += 1
        elif block - 1 in updates:
            distances["none in block, update in the previous block"] += 1
        else:
            distances["none in block or the previous one"] += 1
    median = statistics.median(before_counts) if before_counts else None
    print(
        f"  swap blocks with an update earlier in the same block: {len(before_counts)} of "
        f"{len(swaps)}; tx distance to the first swap: median {median}, "
        f"max {max(before_counts) if before_counts else None}"
    )
    print("  swap blocks by latest update:", dict(distances.most_common()))
    lonely = sum(1 for block in updates if block not in swaps)
    print(f"  update blocks without a swap: {lonely} of {len(updates)}")


def report_cadence(label, updates, swaps, n_blocks):
    """`updates`/`swaps`: {block: [call]}."""
    gaps = sorted(b - a for a, b in pairwise(sorted(updates)))
    count = sum(len(v) for v in updates.values())
    print(
        f"\n{label}: {count} updates in {len(updates)} blocks "
        f"({len(updates) / n_blocks:.1%} of {n_blocks}); gap between update blocks: median "
        f"{percentile(gaps, 0.5)}, p90 {percentile(gaps, 0.9)}, max {gaps[-1] if gaps else None}"
    )
    report_swap_relation(updates, swaps)
    updaters = Counter(c["from"] for cs in updates.values() for c in cs)
    print("  updaters:", dict(updaters.most_common(10)))


def effective_priority_fees(tx_hashes):
    """min(maxPriorityFee, maxFee - baseFee), or gasPrice - baseFee for legacy transactions."""
    txs = rpc_batch([("eth_getTransactionByHash", [h]) for h in tx_hashes])
    blocks = rpc_batch([("eth_getBlockByNumber", [tx["blockNumber"], False]) for tx in txs])
    fees = []
    for tx, block in zip(txs, blocks):
        base = int(block["baseFeePerGas"], 16)
        if tx.get("maxPriorityFeePerGas"):
            tip = int(tx["maxPriorityFeePerGas"], 16)
            fees.append(min(tip, int(tx["maxFeePerGas"], 16) - base))
        else:
            fees.append(int(tx["gasPrice"], 16) - base)
    return sorted(fees)


def report_fees(updates, sample):
    hashes = list(dict.fromkeys(c["tx"] for b in sorted(updates) for c in updates[b]))
    step = max(1, len(hashes) // sample)
    fees = effective_priority_fees(hashes[::step][:sample])
    if fees:
        print(
            f"\neffective priority fee of {len(fees)} sampled update txs (wei/gas): "
            f"min {fees[0]}, median {statistics.median(fees)}, max {fees[-1]}; "
            f"zero: {sum(f == 0 for f in fees)}"
        )


def parse_args():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("contract", nargs="?", help="update and swap contract, when they are one")
    p.add_argument("--blocks", type=int, required=True)
    p.add_argument("--to-block", type=int)
    p.add_argument("--update-to", action="append", default=[])
    p.add_argument("--update-selector", action="append", default=[])
    p.add_argument("--update-prefix", help="calldata prefix for selector-less updates")
    p.add_argument("--update-arg", action="append", default=[], help="INDEX=VALUE, repeatable")
    p.add_argument("--swap-to", action="append", default=[])
    p.add_argument("--swap-selector", action="append", default=[])
    p.add_argument("--sample-fees", type=int, default=0)
    a = p.parse_args()
    if a.update_selector and a.update_prefix:
        sys.exit("pass --update-selector or --update-prefix, not both")
    a.update_to = {x.lower() for x in a.update_to + ([a.contract] if a.contract else [])}
    a.swap_to = {x.lower() for x in a.swap_to + ([a.contract] if a.contract else [])}
    if not a.update_to and not a.swap_to:
        sys.exit("pass CONTRACT, or --update-to and --swap-to")
    a.update_selector = {x.lower() for x in a.update_selector}
    a.swap_selector = {x.lower() for x in a.swap_selector}
    a.update_prefix = a.update_prefix.lower() if a.update_prefix else None
    a.update_arg = parse_arg_filters(a.update_arg)
    return a


def main():
    a = parse_args()
    print(f"chain id: {int(rpc('eth_chainId', []), 16)}")
    last = a.to_block or int(rpc("eth_blockNumber", []), 16)
    first = last - a.blocks + 1
    calls = collect(a.update_to | a.swap_to, first, last)
    print(f"{len(calls)} successful calls into {sorted(a.update_to | a.swap_to)}")
    print(f"blocks {first}..{last}")
    has_update_filter = a.update_selector or a.update_prefix or a.update_arg
    if not (has_update_filter and a.swap_selector and a.update_to and a.swap_to):
        report_census(calls)
        return
    updates, swaps = defaultdict(lambda: defaultdict(list)), defaultdict(list)
    for c in calls:
        if is_update(c, a):
            updates[c["to"]][c["block"]].append(c)
        elif is_swap(c, a):
            swaps[c["block"]].append(c)
    print(f"swaps: {sum(len(v) for v in swaps.values())} in {len(swaps)} blocks")
    merged = defaultdict(list)
    for address in sorted(a.update_to):
        for block, cs in updates[address].items():
            merged[block].extend(cs)
        if len(a.update_to) > 1:
            report_cadence(f"updates to {address}", updates[address], swaps, a.blocks)
    report_cadence("all updates", merged, swaps, a.blocks)
    if a.sample_fees:
        report_fees(merged, a.sample_fees)


if __name__ == "__main__":
    main()
