#!/usr/bin/env python3
"""Which integration-test hardening gaps (the design's G1-G13) the harness at a tree supports.

A gate whose support has not landed is off: the run records it, and the PR description lists it
with the change that turns it on. Nothing passes because a check did not exist.

    harness_caps.py --tree <worktree>          JSON: {gap: {"on": bool, "what": ..., "reason": ...}}

Each gap is detected by a marker in the harness source or a file that only exists once the gap
has landed. Markers of gaps not built yet (G5, G6, G8-G13) are the names the
test-hardening plan proposes; when a hardening PR lands with different names, update its marker
here. This table is the one place that knows them.
"""

import argparse
import json
import re
import sys
from pathlib import Path

HARNESS_DIRS = ("protocols/testing/src", "crates/tycho-test/src")

# gap -> (what it checks, how to detect it, what turns it on)
# A marker is ("regex", <pattern>) searched in HARNESS_DIRS, or ("file", <glob>) from the tree root.
GAPS = {
    "strict": ("`harness: strict` and the per-check report (`range --report`)", [("regex", r"pub harness:\s*Harness")], "#1719 (merged)"),
    "G3": ("every skip is written as its reason", [("regex", r"pub skip_execution:\s*Skip\b")], "#1720 (stack #1804)"),
    "G1": ("each direction's declared limit kind tested: at the limit, past it, max output", [("regex", r"\bLimitsDeclaration\b")], "#1734 (stack #1804)"),
    "G7": ("execution exact by default, declared `execution_tolerance` otherwise", [("regex", r"pub execution_tolerance:\s*Option<DeclaredTolerance>")], "#1735 (stack #1804)"),
    "G2": ("balances per component against the package's `balance_lens`, `tvl_only` verified", [("regex", r"pub balance_lens:"), ("regex", r"pub tvl_only:\s*Option<DeclaredTolerance>")], "#1737, #1740 (stack #1804)"),
    "full-strict": ("strict `full` mode: ladder, limits and tolerance on every update, every pool", [("regex", r"\bTestTypeFull\b[\s\S]*\breport\b"), ("regex", r"limits\.at_limit")], "#1772 (stack #1804)"),
    "G12": ("range mode streamed: every block's deltas, stream vs snapshot, coverage floor", [("regex", r"\bno_stream\b")], "nothing scheduled (#1745 was closed)"),
    "G5": ("expectations at blocks of the streamed run (one test per mutation surface)", [("regex", r"\bcheckpoints\b")], "test-hardening PR 9"),
    "G6": ("the venue's own quotes through eth_call, and replayed swaps", [("regex", r"\breplay_swaps\b")], "test-hardening PR 10"),
    "G8": ("spot price compared with a small forward quote", [("regex", r"fn\s+check_spot_price\b")], "test-hardening PR 7"),
    "G13": ("one-directional components declared and probed on chain", [("regex", r"\bDirectionsDeclaration\b")], "test-hardening PR 7"),
    "G9": ("property tests over ProtocolSim on every decoded state", [("regex", r"fn\s+check_protocol_sim_properties\b")], "test-hardening PR 11"),
    "G10": ("mutation score: every catalogue mutant caught", [("file", "scripts/checks/mutants*")], "test-hardening PR 12"),
    "G11": ("wiring scripts as gates", [("file", "scripts/checks/protocol-system-registry*")], "test-hardening PR 6"),
}


def harness_sources(tree: Path) -> str:
    chunks = []
    for d in HARNESS_DIRS:
        root = tree / d
        if root.exists():
            chunks.extend(p.read_text(errors="replace") for p in root.rglob("*.rs"))
    return "\n".join(chunks)


def detect(tree: Path) -> dict:
    sources = harness_sources(tree)
    out = {}
    for gap, (what, markers, turns_on) in GAPS.items():
        missing = []
        for kind, pattern in markers:
            found = re.search(pattern, sources) if kind == "regex" else any(tree.glob(pattern))
            if not found:
                missing.append(pattern)
        on = not missing
        out[gap] = {
            "on": on,
            "what": what,
            "reason": "landed" if on else f"not landed (no {', '.join(missing)}); turned on by {turns_on}",
        }
    return out


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--tree", required=True)
    args = p.parse_args()
    tree = Path(args.tree)
    if not (tree / "protocols/testing").exists():
        sys.exit(f"{tree} is not a tycho tree")
    print(json.dumps(detect(tree), indent=2))


if __name__ == "__main__":
    main()
