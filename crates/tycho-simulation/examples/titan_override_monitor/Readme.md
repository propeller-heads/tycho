# Titan Override Monitor

Manual monitor proving that Titan pAMM live state overrides reprice pools **mid-block**.

It subscribes to a Tycho stream for pAMM protocols (FermiSwap and bopAMM by default), shares the
production
Titan override provider between the pools and the monitor (one WebSocket connection), and re-quotes
every pool each time a Titan frame arrives. Because `EVMPoolState` resolves the latest override
snapshot on every `get_amount_out`, quotes change *between* Tycho block updates — which is exactly
what the output makes visible.

## Run

```bash
TYCHO_API_KEY=<key> cargo run -p tycho-simulation --example titan_override_monitor -- \
    --tycho-url tycho-dev.propellerheads.xyz
```

Optional flags: `--protocols vm:fermiswap` (comma-separated), `--tvl-threshold 1`,
`--sell-units 0.01` (sell amount in whole units of each pool's first token), `--vm-traces`
(print full EVM call traces; extremely verbose). The Titan endpoint can be overridden with the
`TITAN_PAMM_STREAM_URL` env var (same as production).

## What to look for

```
═══ tycho block 25450948 (3 pool(s)) ═══
08:59:38.290    vm:fermiswap titan_block=25450949 | 0.01 WETH = 17.179149 USDC
08:59:40.164    vm:fermiswap titan_block=25450949 | 0.01 WETH = 17.179549 USDC (Δ +0.0023%)
08:59:41.463    vm:fermiswap titan_block=25450949 | 0.01 WETH = 17.180548 USDC (Δ +0.0058%)
    block 25450948 recap: WETH->USDC 31 quote(s), 3 distinct  <-- repriced mid-block
    block 25450948 recap: WBTC->USDC 31 quote(s), 4 distinct  <-- repriced mid-block
═══ tycho block 25450949 (3 pool(s)) ═══
```

- Multiple quote lines with different values between two `═══ tycho block ═══` headers — and a
  recap with more than one distinct value — prove pools are repriced mid-block from the live
  stream.
- `titan_block` is the *pending* block the streamed overrides target (one ahead of the Tycho
  block); `(expired)` means the last snapshot outlived its 12 s TTL and the pool fell back to
  indexed state.
- Occasional `revert: ... 0x666a2814` (`StaleUpdate`) lines in the first ~2 s of a block are
  expected: Titan re-stamps oracle lanes gradually at block transitions, and a pair whose lane
  still carries the previous block's timestamp reverts exactly as it would on-chain.

## Baseline (overrides disabled)

Point the provider at an unreachable endpoint to see the contrast — quotes then stay constant
within each block:

```bash
TITAN_PAMM_STREAM_URL=ws://127.0.0.1:1 TYCHO_API_KEY=<key> \
    cargo run -p tycho-simulation --example titan_override_monitor -- \
    --tycho-url tycho-dev.propellerheads.xyz
```
