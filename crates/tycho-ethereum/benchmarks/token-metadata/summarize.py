"""Summarize raw live-benchmark observations without third-party dependencies."""

import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path


def timing(values):
    """Median and nearest-rank p95; with small samples p95 can equal the maximum."""
    ordered = sorted(values)
    return (
        statistics.median(ordered),
        ordered[math.ceil(0.95 * len(ordered)) - 1],
        ordered[-1],
    )


def valid_run(rows, run, groups):
    holder_count = sum(row["kind"] == "holder" for row in rows)
    expected = {(count, variant) for count in range(1, holder_count + 1)
                for variant in run.get("variants", ("sequential", "concurrent"))}
    return (
        holder_count > 0
        and set(groups) == expected
        and rows[-1]["kind"] == "complete"
        and rows[-1]["block_hash_unchanged"]
        and rows[-1]["invalid_samples"] == 0
        and all(
            len(samples) == run["rounds"]
            and {sample["round"] for sample in samples} == set(range(run["rounds"]))
            and all(sample["valid"] for sample in samples)
            for samples in groups.values()
        )
    )


def summarize(path):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
    run = next(row for row in rows if row["kind"] == "run")
    groups = defaultdict(list)
    calls = defaultdict(list)
    for row in rows:
        if row["kind"] != "sample":
            continue
        groups[(row["count"], row["variant"])].append(row)
        for call in row.get("calls", []):
            for method in ("symbol", "decimals", "analysis"):
                calls[method].append(call[f"{method}_ms"])

    valid = valid_run(rows, run, groups)
    print(f"Block: {run['block']}; started: {run['started_at']}")
    print(f"Complete and valid: {valid}")
    print("\nTokens | Variant | n | p50 ms | p95 ms | max ms | >3s | Invalid | Retry backoffs")
    print("---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---:")
    for (count, variant), samples in sorted(groups.items()):
        # Include failed samples in the table rather than silently dropping slow failures.
        durations = [sample["elapsed_ms"] for sample in samples]
        p50, p95, maximum = timing(durations)
        invalid = sum(not sample["valid"] for sample in samples)
        retries = sum(sample.get("retry_backoffs", 0) for sample in samples)
        print(f"{count} | {variant} | {len(samples)} | {p50:.1f} | {p95:.1f} | "
              f"{maximum:.1f} | {sum(value > 3000 for value in durations)} | {invalid} | {retries}")

    print("\nSequential-call client elapsed time (includes retries, not pure network RTT):")
    for method, durations in sorted(calls.items()):
        p50, p95, maximum = timing(durations)
        print(f"{method}: n={len(durations)}, p50={p50:.1f} ms, "
              f"p95={p95:.1f} ms, max={maximum:.1f} ms")
    print("\nSmall public-endpoint sample; not production latency or token-count distributions.")
    return valid


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("Usage: python3 summarize.py REPORT.jsonl")
    raise SystemExit(0 if summarize(sys.argv[1]) else 1)
