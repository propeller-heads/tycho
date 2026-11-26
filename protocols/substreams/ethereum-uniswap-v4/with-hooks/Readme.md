# ethereum-uniswap-v4-with-hooks

Uniswap V4 pools with hooks, under protocol system `uniswap_v4_hooks`. One binary serves four
manifests:

| Manifest | Pools created |
|---|---|
| `ethereum-uniswap-v4-with-hooks.yaml` | every pool whose hook has swap permissions (`map_pools_created`) |
| `unichain-uniswap-v4-with-hooks.yaml` | every pool whose hook has swap permissions (`map_pools_created`) |
| `robinhood-uniswap-v4-with-hooks.yaml` | every pool whose hook has swap permissions (`map_pools_created`) |
| `elysium-uniswap-v4-with-hooks.yaml` | pools of the hooks in `montra_hooks` only (`map_montra_pools_created`) |

## Montra on Elysium

A Montra pool is a Uniswap V4 pool of the Elysium `PoolManager`
(`0xe2e105d3f7209a9da11f83ccf3e7398a753823f1`) whose `PoolKey.hooks` is the Montra `LaunchHook`
(`0x6b1216a40b65c83e9c9e18a562c446697b8cf8ec`). The hook takes its fee through
`beforeSwap`/`afterSwap` deltas and switches the pool's dynamic LP fee on once a pool has held past
its LP line, so the pool's price depends on hook state that no V4 event carries.

### What the Elysium manifest emits

- One component per pool, created from `PoolManager.Initialize` when `hooks` is listed in the
  manifest's `montra_hooks` parameter. Its id, tokens, static attributes and V4 state attributes
  are the other manifests' (the shared crate `ethereum-uniswap-v4/shared` indexes ticks,
  liquidity, price, protocol fees and balances).
- The hook's per-pool pricing state as `montra_*` state attributes, decoded from every write to
  the pool's `PoolConfig` words 0 to 2, in any transaction. `src/montra/pool_config.rs` owns the
  layout and the attribute formats.
- `paused`, which `src/variant_modules/6_map_montra_enriched_block_changes.rs` sets and lifts; its
  module doc states the conditions and the deployment assumption they guard.

The hook has no pause or kill switch: none of its 45 functions (enumerated from its bytecode)
stops swaps.

The Elysium manifest emits no entrypoints, contracts or storage. Elysium's RPC (Chainstack, Nitro
v3.12.4) failed every `debug_storageRangeAt` call with -32603 "method handler crashed" when probed
on 2026-11-13. Dynamic contract indexing is therefore unavailable there, and the simulation prices
these pools natively from the attributes.

### Rollout: no hooks DCI on Elysium

Run the Elysium `uniswap_v4_hooks` extractor with no `dci_plugin`, or with type `rpc`, never with
type `uniswap_v4_hooks`. tycho-indexer attaches that plugin by extractor config, not by manifest.
Nobody has run it on Elysium; what follows is read from tycho-indexer's hooks DCI code, not
observed. The plugin would hand every Montra pool (its hook has swap permissions) to its default
orchestrator, which traces through `debug_storageRangeAt` (unavailable on Elysium, above), and
after failures writes a pool `paused` with value 2 or 3. The simulation reads any `paused` value
other than empty or 0 as paused, so the stream would keep decoding, but every Montra pool would
then stay paused and unquotable. No package code guards this, and the integration test cannot
catch it, since the harness always runs with `--dci-plugin rpc`.

### Balances

Balances come from the shared V4 pipeline: each pool's token amounts follow its liquidity and swap
events, with `balance_owner` the `PoolManager`. The hook takes its fee as an ERC-6909 claim on the
`PoolManager`, held by the hook, outside the pool's amounts. The `PoolManager` keeps every pool's
tokens in one balance and stores no per-pool amount, so the integration test reads each pool's
amounts with the `UniswapV4Balances` lens, which derives them from the pool's ticks and liquidity
at its current price. The pipeline estimates each swap's LP fee and prices a liquidity change at
the price of its block, so its balance can differ from the lens's by the rounding of those
estimates.

### Upgrade boundary

The hook has one runtime code hash at blocks 588510, 5987400 and 6002000 and no proxy slot, so its
storage layout, which `src/montra/pool_config.rs` decodes, cannot change under a deployed pool.
The package follows or misses the contracts that can change as follows:

| Dependency | Upgradeable | How the package follows it, or what breaks |
|---|---|---|
| `LaunchFactory` proxy `0x38d3ef3a06d18fa3286fb5deafe611be725fcd15` (EIP-1967, implementation `0x54b5b72311fa360cd827ccaf2835bfba44f63840` at blocks 588508 to 6002000) | yes | An upgrade can launch pools with other terms or sequencing, but cannot write a live pool's `PoolConfig` or `PoolKey`: the factory address is an immutable in the hook, and `register` and `seed` refuse registered pools. The package decodes every `PoolConfig` write whatever its caller, so it follows new terms. The creation-time pause catches a launch split over several transactions. |
| Quote tokens KNTQ, wNVDAx, wSPYx, wQQQx, EQTSLA, EQSPCX, behind beacon `0x659f40efd07d70224b7ac61e4721f6f3e11b625e` (implementation `0xbD080eeF044a3E6226757487C5cef33568AfEfD6` at blocks 300000 to 6024400) | yes | Not followed. An upgrade that taxes or blocks transfers is invisible to the package, and the simulation would over-quote pools in that token. |
| A new Montra hook (the `LaunchpadRegistry` lists hooks) | n/a | `map_montra_pools_created` creates none of its pools until a maintainer adds its address to `montra_hooks`. Before adding it, the maintainer checks its storage layout against `src/montra/pool_config.rs`. `store_montra_slots` keys each slot by the pool's own hook, so one hook's writes are never read as another's pool, whatever their layouts. |

### Store cost

`store_montra_slots` keeps six keys per pool (its `PoolConfig` words 0 to 2 and its `PoolKey`
words), each value about 70 bytes. At 97,167 pools registered by block 6024400 that is about
583,000 keys, growing by six per launch. Per block, the terminal module reads it once for each
distinct hook slot a transaction writes, watched or not: a swap's `PoolConfig` word 0 and every
fee-accounting slot of the hook it updates.

### Terms the hook refuses

The package emits a pool's `PoolConfig` terms as written; it does not validate them, so a value
the hook would never accept cannot stop the extractor. The simulation refuses to quote a pool
whose terms the hook's `register` refuses. One of those rules was established on a fork rather
than read from source: with an anti-snipe window set, `register` reverts `InvalidFeeConfig()` for
a start rate at or below the total fee (Elysium fork at block 7964700, windows of 1 to 1800 s,
totals 5000 to 100000 pips), and without a window it accepts only a start rate of 0.

### Quotes in the next block

The decoder prices every state for the next block, at the latest block's timestamp plus one
second on Elysium. Elysium makes a block about every 100 ms and timestamps whole seconds, so the
swap often lands in a block with the same timestamp as the latest one. Inside an anti-snipe
window a buy is then quoted at up to one second's decay below the rate it pays (97,000 pips for
a 10-second window from 990,000 to 20,000), and a pool holding past its LP line can be quoted
with its LP fee on one second early. Outside those windows the estimate changes nothing.
