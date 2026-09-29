# Copyright (c) 2026 Everlong Labs Limited
"""Rebuilds out/stages/ from the two committed end-to-end fixtures, so a stage can be appended to fetch.py's
STAGES without re-reading the 27 already recorded ones (about six hours of paced mainnet.base.org reads).

out/ is the scripts' working tree and is not committed; e2e_blocks.json.gz carries every stage exactly as
pack.py wrote it, so unpacking it and packing it again reproduces both fixtures byte for byte -- run pack.py
straight after this and check the digests against the READMEs before adding anything. The two fields pack.py
drops are not restored: `feed_logs`, which nothing downstream reads, and `state_after` on the stages that are
not pinned, which comes back as an empty list. fetch.py takes the previous stage's `state_after` as the base
of the next stage's catch-up diff, so only the last stage's is needed -- appending works, re-fetching a stage
in the middle of the list does not. morpho_<block>.json is rebuilt from the `other_morpho_events` of
e2e_grids.json.gz, which is the only part of it pack.py reads; the blocks that settled no swap and no deposit
have no file and morpho_events.py fetches them again (one eth_getLogs each).

Usage: python3 unpack.py   (then: python3 fetch.py, python3 morpho_events.py, ./regen.sh)
"""
import gzip, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, *([".."] * 8)))
BLOCKS = os.path.join(REPO, "protocols", "substreams", "base-flamm", "testdata", "e2e_blocks.json.gz")
GRIDS = os.path.join(os.path.dirname(HERE), "snapshots", "e2e_grids.json.gz")


def main():
    blocks = json.load(gzip.open(BLOCKS))
    grids = json.load(gzip.open(GRIDS))
    out = os.path.join(HERE, "out", "stages")
    os.makedirs(out, exist_ok=True)
    seed = {"block": blocks["seed_block"], "state": blocks["seed"]}
    json.dump(seed, open(os.path.join(out, "seed_%d.json" % blocks["seed_block"]), "w"), indent=1)
    assert "state_after" in blocks["stages"][-1], "the last stage carries no state_after: nothing can be appended"
    for st in blocks["stages"]:
        st.setdefault("state_after", [])
        json.dump(st, open(os.path.join(out, "%d.json" % st["block"]), "w"), indent=1)
    morpho = {}
    for row in grids["swaps"] + grids["deposits"]:
        evs = morpho.setdefault((row["block"], row["tx"]), [])
        evs += [e for e in row["other_morpho_events"] if e not in evs]
    for (block, tx), evs in morpho.items():
        json.dump({"block": block, "tx": tx, "events": evs}, open(os.path.join(out, "morpho_%d.json" % block), "w"), indent=1)
    print("unpacked %d stages and %d morpho blocks into %s" % (len(blocks["stages"]), len(morpho), out))


if __name__ == "__main__":
    main()
