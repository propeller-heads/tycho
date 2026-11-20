# ethereum-uniswap-v4-with-hooks

Uniswap V4 pools with hooks, under protocol system `uniswap_v4_hooks`. One binary serves four
manifests:

| Manifest | Pools created |
|---|---|
| `ethereum-uniswap-v4-with-hooks.yaml` | every pool whose hook has swap permissions (`map_pools_created`) |
| `unichain-uniswap-v4-with-hooks.yaml` | every pool whose hook has swap permissions (`map_pools_created`) |
| `robinhood-uniswap-v4-with-hooks.yaml` | every pool whose hook has swap permissions (`map_pools_created`) |
| `elysium-uniswap-v4-with-hooks.yaml` | pools of the hooks in `montra_hooks` and `ascend_hooks` only (`map_montra_pools_created`) |

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
observed. The plugin would hand every Montra and Ascend pool (their hooks have swap permissions)
to its default orchestrator, which traces through `debug_storageRangeAt` (unavailable on Elysium,
above), and after failures writes a pool `paused` with value 2 or 3. The simulation reads any
`paused` value other than empty or 0 as paused, so the stream would keep decoding, but every
Montra and Ascend pool would then stay paused and unquotable. No package code guards this, and the
integration test cannot catch it, since the harness always runs with `--dci-plugin rpc`.

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

## Ascend on Elysium

An Ascend pool is a Uniswap V4 pool of the same `PoolManager` whose `PoolKey.hooks` is the Ascend
launchpad hook (`0xe0def4221b9840a610d4e7d55812a79a6431a8ec`, `AscendLaunchpadV5`). Only the
launchpad creates its pools and its liquidity: one position per pool, never removed (checked on
every pool to block 8365800). The hook
takes a fee in `beforeSwap`/`afterSwap`, observes the price into an EMA and a discount, and fills
sells from a per-pool bid inventory of the quote token. Those values live in hook storage and no
V4 event carries them. 4962 pools exist up to block 8365800, all with the dynamic-fee flag and tick
spacing 200.

### What the Elysium manifest emits for Ascend

- One component per pool, created from `PoolManager.Initialize` when `hooks` is listed in the
  manifest's `ascend_hooks` parameter, by the same module as the Montra pools. The component id is
  the pool id.
- The hook's pricing state as `ascend_*` state attributes, decoded from every write to five slots
  of the pool, in any transaction: `PoolData` words 2 and 3 (base slot 4) and `Bid` words 0 to 2
  (base slot 2). `src/ascend/mod.rs` owns the layout and the attribute formats. The attributes
  are `ascend_fee_pips`, `ascend_token_is_currency0`, `ascend_start_tick`,
  `ascend_ema_sqrt_price_x96`, `ascend_ema_updated_at`, `ascend_observed_tick`,
  `ascend_discount`, `ascend_bid_inventory`, `ascend_bid_filled_in_window`,
  `ascend_bid_window_start` and `ascend_bid_max_clip`, all state attributes, created with the
  pool. The fee, orientation and start tick sit in mutable hook storage with no verified source,
  so they are state attributes, not static ones as Pons' fee terms are.
- No `paused`. `setPaused` (slot 8) stops launches only: quotes in both directions are the same
  with it set, and the swap path never reads it. This is a switch deliberately not indexed.
- No entrypoints, contracts or storage, as for Montra (see "Rollout: no hooks DCI on Elysium").

The writes cover every writer of the five slots: the hook's 30 `SSTORE` sites were mapped to
traced functions. A swap writes `PoolData` word 3, and on a fill `Bid` words 0 to 2; the launch
writes `PoolData` words 0 to 3 and `Bid` word 2; `fundBid` writes `Bid` word 0. `collect`, `claim`,
`setQuoteConfig`, `setPaused` and the ownership functions write slots that do not price a swap.
`PoolData` words 0 and 1 (token, builder fee, quote) are not watched: the swap does not charge the
builder fee and the tokens come from the `PoolKey`. One function selector (`0xea72eef4`) is not
decoded; it has no `SSTORE` site of its own, and any write it caused to a watched slot would be
decoded anyway.

Deployment assumption: the launch writes `PoolData` words 0 to 3 and `Bid` word 2 in the same
transaction as `Initialize`. Observed in the prestate trace (`diffMode`) of one launch transaction,
`0x088340f49470918c0f99684bcff5deb387e409534688526aa762aca4052ed82e` (block 8195096); the other
launches are only known to have a `Launched` event (4962 of them to block 8365800), which shows
that each pool launched, not what each launch wrote. A pool initialised without them would get
zero for every attribute, and the simulation refuses such a pool. `Bid` words 0 and 1 (the
inventory and the filled quote, at `keccak(poolId . 2)` and the next slot) are not written at
launch and start at zero.

A second deployment assumption concerns `fundBid`, which is permissionless: a call on a pool id
before its launch would write `Bid` word 0 before the package watches the slot (the store lists
a pool's slots when it is created, and a write before the creation transaction is skipped), so the
pool would be created with `ascend_bid_inventory` 0 while the hook holds more. The simulation
would then under-quote fills until the next write to that word, and never revert on it. No such
call is known: the 6080 `BidFunded` events to block 8364931 (on 45 pools) were not checked against
their pools' launch blocks, and whether the hook rejects a `fundBid` on an unlaunched pool id is
not established, since its source is unverified.

### Balances of Ascend pools

As for Montra: the shared V4 pipeline's amounts, with the `UniswapV4Balances` lens in the test
and a `tvl_only` tolerance (bps 0.01, wei 1000) on the eight Ascend components, declared on the
measured cause below. The wei bound applies to every token of every Ascend component; KNTQ,
which the lens reads as 0, is the measured case it was sized for.

Measurements: a `range --report` run of the two Ascend tests with `tvl_only` removed (in
`rounds/indexing-2` of the integration run) gave these worst values:

- 574 wei of 2,805,774,267 USDC on pool `0xea03c9c4…` (0.002 bps);
- 310,400,288 wei on PNTH (5e-19 of its amount);
- 4 and 12 wei on the KNTQ entries the lens reads as 0.

The `PoolManager`'s own balance of each single-pool launch token equals the indexed amount at 7
of 8 pools (PNTH holds 4 wei more); the lens's rounded-down tick math is the lower figure.

The Ascend pool's own swaps charge no LP fee (every `Swap` event carries fee 0, and
`feeGrowthGlobal0/1X128` read 0 at the end of the tests' ranges), the hook's fee is its
ERC-6909 claim, and the pipeline's amounts are the summed `Swap` amounts. They sit above the
lens by a small amount, never below it. It was reproduced by replaying every `Swap` event of
two pools against the amounts their price steps imply: the surplus is the sum of the events'
amounts over those implied amounts (4610 and 254 wei of 4612 and 255 measured, in the 940 swaps
of `0x21e12450…`; 310,400,285 and 573 wei of 310,400,288 and 574 measured, in the 2384 swaps of
`0xea03c9c4…`). One unit of the Q96 price is worth `L * 2^96 / P^2` of currency0, which is 58
wei in the first pool and 2.2 million in the second, so the surplus grows with a pool's swap
count and with how low its price is.

The bid inventory is quote the hook holds at its own address, paid out only through fills; it is
not in the pool's amounts and not counted in the balance, so a pool's payable size on a fill is
larger than its indexed balance. The hook's fee claims are ERC-6909 balances at the `PoolManager`,
outside the pool's amounts too.

### Upgrade boundary for Ascend

The hook and the library it calls are not upgradeable: one code hash at blocks 423647 and
8365800, no proxy slot, no `DELEGATECALL`, `CREATE` or `SELFDESTRUCT`, so the layout in
`src/ascend/mod.rs` cannot move under a deployed pool.

| Dependency | Upgradeable | How the package follows it, or what breaks |
|---|---|---|
| Quote tokens: WHYPE (EIP-1967 proxy), USDC, KNTQ and wNVDAx (beacon proxies of `0x659f40efd07d70224b7ac61e4721f6f3e11b625e`) | yes | Not followed. An upgrade that rejects `transfer(hook, 0)` silently blocks fills the simulation still quotes. The simulation does not model the hook's balance guards, which decline a fill only on a strict shortfall and hold today because the hook's quote balance equals the summed inventories; an upgrade that leaves the hook holding less (a rebase, a transfer fee, a seizure) would make the hook decline fills the simulation quotes. |
| A new Ascend hook | n/a | `map_montra_pools_created` creates none of its pools until a maintainer adds its address to `ascend_hooks`, after checking its layout against `src/ascend/mod.rs`. `store_montra_slots` keys each slot by the pool's own hook. |
| A grouped V4 route in which an earlier hop delivers the launched token as a credit | n/a | The hook's token guard sees a balance short of what the fill needs; the simulation does not model it. |

### Store cost of Ascend

`store_montra_slots` keeps five keys per Ascend pool, about 24,800 keys at 4962 pools (block
8365800), each value about 70 bytes. As for Montra, the terminal module reads the store once for
each distinct hook slot a transaction writes, watched or not: an Ascend swap writes many slots of
the hook that no attribute reads.

### Quotes in the next block

A sell on a pool with bid inventory depends on the block timestamp through the observation and the
600 s window. The decoder prices the next block at the latest timestamp plus one second. Elysium
stamps whole seconds about every 100 ms, so a swap often lands in a block that carries the latest
timestamp, where a pool whose price moved since its last observation rejects the fill, while the
quote at plus one second re-observes and may include it. The difference reaches only sells on the
45 pools funded to block 8364931, and the route's minimum output bounds it.
