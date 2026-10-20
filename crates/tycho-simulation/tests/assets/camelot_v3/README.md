# Camelot V3 replay fixtures

Each JSON file captures one real Arbitrum One swap on a Camelot V3 (Algebra V1.9) pool: the
pool's state at the parent block, the initialized ticks around the swap's path, the oracle
timepoints the fee recomputation reads, and what the chain recorded (the `Swap` and `Fee`
events and the post-swap storage). `camelot_v3::state::tests::replays_arbitrum_swaps_wei_exact`
rebuilds `CamelotV3State` from the pre-state and must reproduce the output amount, the price,
tick and liquidity after the swap, the recomputed fees, the written timepoint and
`volumePerLiquidityInBlock` to the wei. Every file in this directory is replayed.

## Regenerating or adding a fixture

```bash
python3 dump_fixture.py --rpc-url https://arbitrum.gateway.tenderly.co 0x<transaction hash>
```

The script needs an archive RPC and nothing beyond the Python standard library. It writes
`<first four hex digits of the pool>_<block>.json` next to itself. A usable transaction:

- swaps exactly once on the pool, and is the only transaction touching the pool and its
  `DataStorageOperator` in its block, so that the parent-block state is the swap's pre-state;
- is an exact-input swap. An exact-output swap replayed as exact input differs by the rounding
  of its input, so the replay test fails on it. Aggregator routers (1inch, Kyber, Odos,
  Paraswap) send exact-input swaps.

Candidates are easy to find with `eth_getLogs`: every timepoint write emits `Fee(uint16,uint16)`
from the pool, so the gap between a pool's writes shows how long it sat idle, and a `Swap` whose
`liquidity` differs from the pool's liquidity at the parent block crossed an initialized tick.

The script fetches the tick-table rows around the swap's path with a margin, plus the rows that
hold full-range positions. When the pool has no position at or beyond a fetched boundary, it
places a zero-net tick there (`stand_in_ticks`), so that a quote walking past the fetched rows
stops with an error instead of treating unknown rows as empty.

## What each fixture covers

| File | Pool | Swap |
|---|---|---|
| `0b3b_512302759.json` | EDU/USD₮0 | First swap at a new timestamp in a full-range pool; the fee is recomputed from the window (6133 to 6134). |
| `299c_512302777.json` | PEAR/USDC | First swap at a new timestamp in a pool with twelve initialized ticks, staying inside one range. |
| `d7e2_512304331.json` | USDC/AARK | First swap at a new timestamp on a ring that has wrapped: the write overwrites the oldest slot; fee 11092 to 9668. |
| `9b6f_514495582.json` | GNS/WETH | First swap at a new timestamp that crosses an initialized tick, so the liquidity changes mid-swap. |
| `b102_514498465.json` | WETH/USDC, tick spacing 10 | Swap at a timestamp the ring already holds: nothing is written, the stored fees apply, and one initialized tick is crossed. |
| `c23f_514489425.json` | USDe/USDC, tick spacing 1 | Swap at a timestamp the ring already holds, crossing two initialized ticks. |
| `cecf_513935367.json` | WETH/GUTARD | Idle pool whose last timepoint is eight months old: the window start interpolates up to the write and the fee decays from 3180 to 3004. |
| `c3e2_514319159.json` | PREMIA/WETH | Idle pool, six days without a write: a large swap moves the price 706 ticks and the fee drops from 15000 to 11945. |

Every fixture also serves the other tests of the module: a second swap at the same timestamp,
the spot price against a small swap, and the limits of the full-range pools.
