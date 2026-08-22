# Verification scripts

Standalone checks behind the claims in this package's PR description. Each is plain Python 3
with no dependencies, and each re-derives its answer from chain rather than from anything the
Substreams produced — so they are independent of the code they are checking.

All three read `RPC_URL` from the environment and fall back to `https://eth.drpc.org`, a public
archive endpoint. `eth.drpc.org` caps `eth_getLogs` at 10k blocks, so `verify_event_replay.py`
wants a real archive node for its default 100k-block window.

```bash
export RPC_URL=https://your-archive-node
```

## `verify_event_replay.py <market>`

Checks the premise of the reserve tracking: that `totalPt`, `totalSy` and `lastLnImpliedRate`
reconstruct exactly from `Swap` / `Mint` / `Burn` / `UpdateImpliedRate`, with no storage reads.

Reads `_storage()` at both ends of a block window, replays every event in between, and compares.
Window is `START, END` at the top of the file.

```
$ python3 verify_event_replay.py 0x34280882267FfA6383B363E278B027Be083bBe3b
on-chain @ 25696804: totalPt=102584091760330947041 totalSy=1414859995100742285218 lastLnImpliedRate=22620090295034132
on-chain @ 25796804:   totalPt=87682116111289735171 totalSy=1423106038017924555830 lastLnImpliedRate=20791947223446649
events replayed: {'swap': 18, 'mint': 5, 'burn': 2, 'uir': 25}
replayed:        totalPt=87682116111289735171 totalSy=1423106038017924555830 lastLnImpliedRate=20791947223446649
MATCH totalPt=True  totalSy=True  lastLnImpliedRate=True
```

A `False` in the last line means the event set in `src/market_state.rs` is incomplete — that is
the failure this script exists to catch.

## `markets.py`

Enumerates every `CreateNewMarket` ever emitted, across both factory ABIs, via Blockscout.
Writes `all_markets.json`. Run this first; `redeem_sweep.py` consumes its output.

Note the two different `CreateNewMarket` topics — the original factory's four-parameter event and
the five-parameter one the other four emit. See `../abi/README.md`.

## `redeem_sweep.py`

Measures how much of the SY redeem side is quotable in closed form. Filters `all_markets.json`
down to unexpired markets, resolves their SYs, and probes `previewRedeem` for one whole SY unit
against each `getTokensOut` entry, classifying each `(SY, token)` pair as `one_to_one`,
`index_rate` or `other` — the same three-way decision `src/sy.rs` makes at component creation.

Writes `redeem_classes.json` and prints the class histogram quoted in the PR.
