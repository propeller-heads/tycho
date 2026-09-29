# testdata

Real Base data the unit tests replay, all read from public RPC (`https://mainnet.base.org`, fallbacks
`base-rpc.publicnode.com` / `base.llamarpc.com`) with `eth_getStorageAt`, `eth_getLogs`, `eth_getCode`,
`eth_call`, `eth_getBlockByNumber`, `eth_getTransactionByHash` and `eth_getTransactionReceipt`; no `debug_*`.

| file | what |
|---|---|
| `seeds_51154965.json` | the 48 seed words at `initialBlock - 1` with the views that verify them (`aggregator()`, `phaseId()`, `accessController()`, `checkEnabled()`, `hasAccess(proxy, "")`, `latestRoundData()`, `getRoundData(r)`, `Morpho.market(id)`, `AdaptiveCurveIrm.rateAtTarget(id)`) |
| `immutables_51154990.json` | the ten pinned immutables that have a getter, read through it at the creation block (`views`); the IRM runtime codehash (`eth_getCode`); and, under `dual_code`, the two the `DualAggregator` has none for. `i_secondaryProxy` and `i_maxSyncIterations` are `internal immutable`: solc emits no accessor and inlines them, so they are read by walking `eth_getCode(0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1, 51154990)` one opcode at a time (push operands skipped by their length) and taking the operand of the PUSH32 at each use. 20 is pushed at 0x3d5e and 0x3ff2, the only two PUSH32s of that value, each masked to `uint32` and compared with `latest - round` in the reveal loop's exit test (the two inlined copies of `_getSyncPrimaryRound`, which reads `latestAggregatorRoundId` out of slot 13 just above them); the secondary proxy occurs once in the code, pushed at 0x319d and compared with `CALLER` to gate the secondary reveal, so the walk establishes its role and not merely that the address is present. Nothing in the package re-derives these two at test time; a reviewer re-runs the walk. |
| `feed_logs.json` | one full round of the cbBTC/USD aggregator (14785, block 51302648) and of the USDC/USD aggregator (26, block 51300186) with the aggregator's `HotVars` / `s_transmissions[r]` after the block; the sequencer feed's status-change round 20 (47851108) and its first refresh (47894326) with `s_feedState` before/after; the DualAggregator's primary round 3583 (51302909) with its transmission word and a `SecondaryRoundIdUpdated` (51302850) with `HotVars` before/after |
| `snapshot_51302915.json` | the schema snapshot of the swap component at 51302915: static attributes, the 147 dynamic attributes and the balances, every value verified against the contracts' views (schema section 5) |
| `e2e_blocks.json.gz` | the end-to-end replay's real inputs (`src/e2e_tests.rs`), gzipped (mtime 0) JSON with sorted keys: `seed`, every tracked word at 51154965; `stages`, one per block of interest in order — the header, the block's own storage diff over the tracked words (`writes`, `eth_getStorageAt` at block-1 vs block), the net diff of the blocks since the previous stage (`catchup_writes`, valued at the parent block `header_before`; a `catchup` stage carries only that diff as `writes`), the transaction of interest (`tx`: hash, index, from, to, input, receipt logs), the runtime code of the contracts it created (`codes`, `eth_getCode` at the block) and, at the pinned blocks, every tracked word after the block (`state_after`). Tracked words are the sets of `src/flamm/keys.rs` for the live pool plus the Chainlink words `feed_state` reads (the proxies' slots 2 and 5, each fronting aggregator's hot words, read-access pair and cutoff, the OCR2 latest transmission and the `DualAggregator` window). Blocks: 51154977, the deployments 51154978/79/83/85/86/87/88, the creation 51154990, 51155010, the activation 51298416, the swaps 51302916, 51343234, 51347390, 51420672, 51420867, 51430828, the deposits 51300667 and 51426394 (two of six), the withdrawal 51348093 (one of four), the keeper recenter 51384803 (one of the 24 through 51433699), the stop block 51302920 (and 51302915), the round blocks 51429815 (USDC/USD), 51433135 (cbBTC/USD), 51433218 (`DualAggregator`), the leverage unpause 51433699 (`LevPauseSet(false)`), the clearing of the spread's staleness window 51649706 (`MaxSpreadAgeSet(0)`, which opened the lever-up venue) and 51670000, a later block at which both venues quote; the pool's other deposits, withdrawals and keeper transactions are inside the catch-up diffs. sha256 `02fe16e5760e42eacf71fe750344f1c806c61eaa112a957537f35f490624415e` (stored), `f4ff0652108b7143a32207ab83aba2afc3f14878ee56eebd54f1254ff5c05df3` (uncompressed) |

`src/testdata.rs`'s `stage(block)` cuts the three per-block fixtures the unit tests replay out of
`e2e_blocks.json.gz` rather than carrying them again as their own files: the `createPool` transaction
`0x783b464e…d1e0` (51154990), the curator's activation `0x4af4828c…eafd` (51298416) and the pool's first
settled swap `0x46c3cd72…01fa` (51302916, 15000 sats -> 11301759 USDC). Each gives the stage's header,
transaction and receipt logs, the block's own tracked-word diff (`storage_diffs`), the codehash and runtime
code of every contract deployed up to it, the words store the block opens on (`store_before`, the writes
since 51154965) and the chain's words at the parent block (`words_before`, the seed with those writes
applied).

The manifest params of `base-flamm.yaml` (codehashes, seeds, immutables) are derived from these files and
pinned by the test `manifest_params_are_the_fixture_values`, which fails with the expected value when a fixture
and the manifest disagree. There is no generator script: a fixture change is carried into the manifest by hand,
using the value the failing assertion prints. `e2e_blocks.json.gz` was read on 2026-09-17
(the pool's history up to block 51433699) and extended on 2026-09-23 with the two stages around the spread
hook's `MaxSpreadAgeSet(0)` (51649706 and 51670000, appended, which invalidates no catch-up diff) by
`fetch.py` (stages), `morpho_events.py` (the other transactions' Morpho events at the pool's transaction
blocks, kept in the simulation's `e2e_grids.json.gz`) and `pack.py` of
`crates/tycho-simulation/src/evm/protocol/flamm/testdata/gen` (its README lists every script, its role and its
digest; `snapshot_51302915.json` is `snapshot.py`'s output there); the fold it produces through
`src/e2e_tests.rs` is the simulation's `e2e_stream.json.gz`. Solidity source of truth for every decoded value: `EverlongLabs/blockend` @ `80abd43`.
