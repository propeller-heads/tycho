# base-flamm

Native Tycho indexing for Everlong FLAMM on Base: one substreams package that turns the state a FLAMM quote reads
into component attributes, without a VM and without DCI. The `flamm` `ProtocolSim` in `tycho-simulation` decodes
these attributes and reproduces the pool's fills to the wei.

Solidity source of truth: `EverlongLabs/blockend` @ `80abd43`; deployment record
`script/flamm/c104/deployments/c104.8453.json`. Storage layout and slot keys are derived in `src/flamm/keys.rs`
from the contracts' storage layouts (each derivation cites its Solidity file and line; the Chainlink layouts are
in `src/flamm/feeds.rs`) and checked by unit tests against `eth_getStorageAt` fixtures in `testdata/`. Comments
that cite `schema x.y` refer to the sections of the FLAMM native-integration storage schema, a design document
kept outside this repository; every fact of it the package depends on is reproduced here in the fixtures.

## What a quote reads, and where the package gets it

| state | source on chain | how the package emits it |
|---|---|---|
| FLAMM-owned storage: pool namespace (`FLAMMStore.S`, ERC-7201), `EverlongHook`, `LeverageSpreadHook`, the pool's `MMRouter` record, `MorphoBlueAccount`, `PriceFeed` token config, `FLAMMFactory` beacon words | contracts created in blocks 51154978-51154990, all inside the indexed range | every tracked storage write, as raw 32-byte words `<role>:<slot>` (`pool:`, `hook:`, `spread:`, `router:`, `account:`, `pricefeed:`, `factory:`) |
| Morpho Blue `market[id]` (3 words), `position[id][account]` (2 words), `AdaptiveCurveIrm.rateAtTarget[id]` | pre-existing contracts; keys derived from the venue's market id and account | raw words `mm:<v>:market:{0,1,2}`, `mm:<v>:position:{0,1}`, `irm:<v>:rate_at_target`, seeded at `initialBlock - 1` |
| Chainlink cbBTC/USD, USDC/USD (behind `PriceFeed`), BTC/USD (behind the Morpho oracle), the L2 sequencer uptime feed | the proxies' rotation word (slot 2) and access controller (slot 5); the aggregators' read-access pair and the words `latestRoundData` reads (`HotVars`, `s_transmissions`, `s_feedState`, `s_cutoffTime`) | decoded `feed:<f>:*` attributes (section "Attributes"), seeded at `initialBlock - 1`, re-derived and diffed for every transaction that writes one of the words |
| immutables (`LOAN_SCALE`, genesis hashes, `SEQUENCER_FEED`, `SEQUENCER_GRACE`, `UPGRADE_DELAY`, the oracle's `SCALE_FACTOR` / `BASE_FEED_1`, the `DualAggregator`'s secondary proxy and sync depth, the IRM codehash) | bytecode | static attributes from the manifest `immutables`, asserted by the range test |

Both components of a pool carry the same attribute set. Component ids: swap `= pool address`, lever-up
`= pool (20 bytes) || 0x00000000 || uint64(1)`; `protocol_system = flamm`, `protocol_type_name = flamm_pool`,
tokens `[pool asset, loan asset 0]`, `contracts` empty. The pool's contracts (pool, hook, leverage hook, spread
hook, router, account, price feed, factory) are address-valued static attributes rather than the component's
`contracts` list: the indexer resolves that list against the accounts the stream created (`tycho-storage`
`add_protocol_components` joins `contract_code` with `account` and fails the block's write with
`NotFound("Account")` otherwise), and a native integration creates no accounts.

## Module graph

```
store_deployments  (params, Block)                        deploy:<addr> -> <role>:<codehash>
store_words        (params, Block, store_deployments)     word:<addr>:<slot> -> value
map_components     (params, Block, store_deployments, store_words)
store_pools        (map_components)                       pools -> [addr];  pool:<addr> -> serialized config
map_protocol_changes (params, Block, map_components, store_pools, store_words)
```

* `store_deployments` records every contract created with a runtime code the `deployments` registry names
  (`keccak256(new_code)` of the block's code changes, recomputed rather than trusted). This is how a pool's
  contracts are tracked from *before* the pool exists: hooks, price feed, router and factory are deployed ahead
  of `createPool` against the CREATE2-predicted pool address (`FLAMMFactory.predictPool`).
* `store_words` records every storage write of a registered contract (a pool's own words filtered to its
  namespace, its loans array and `_totalSupply`), of the manifest `addresses` (the aggregators, whose round words
  have per-round keys) and of the seeded `words` keys. The maps read it as of the start of a block.
* `map_components` turns `FLAMMFactory.PoolCreated` plus the `createPool` calldata into the two components. A
  pool is refused, with the reason logged, unless its invariant hook's codehash is allowlisted, the manifest has
  its immutables, every one of its contracts was created with registered code in the range (a contract that
  was not has no tracked history, so the component would fail closed anyway), and the manifest tracks its
  external words (see "Failure modes").
* `map_protocol_changes` emits, per transaction: the new components with their full creation snapshot (every
  tracked word known after the creation transaction, the Morpho positions of the venue accounts as zero rows
  when never written, the feeds, the balances); the tracked words the transaction wrote (a `Creation` for the
  first write of a word the indexer holds no row for, an `Update` of a held row otherwise); the feed
  attributes that changed; and the balances of the tokens whose inventory the transaction moved. Words are
  valued as "last write up to this transaction, else the store at the start of the block, else
  the seed", so a snapshot is exact per transaction and the module stays a pure function of block + params (safe
  under Base partial blocks). A feed's attributes are one function of its words (`feeds::feed_state`): for every
  transaction that writes the proxy's rotation word or any word of the aggregator behind it, the state is
  derived before and after the transaction and the difference is emitted (`Creation` for a new attribute,
  `Update` for a changed value, `Deletion` for one that is gone), so a round and a rotation in one transaction
  resolve to the transaction's end state, as the chain's does. Nothing is emitted for a pool in the transactions
  of its creation block that precede its creation transaction.

## Attributes

Raw words (32 bytes): `pool:<slot>`, `hook:<slot>`, `spread:<slot>`, `router:<slot>`, `account:<slot>`,
`pricefeed:<slot>`, `factory:<slot>` with `<slot>` the `0x`-prefixed 64-hex-digit key, exactly the value
`eth_getStorageAt` returns; `mm:<v>:market:{0,1,2}`, `mm:<v>:position:{0,1}`, `irm:<v>:rate_at_target`. The
forwarded key sets are supersets of the schema's (the whole pool namespace `FLAMM_NS+0..33`, up to 8 pool loans,
hook slots 0..31, up to 8 router loans / 16 venues / 4 order words per order): a word a later curator action
writes still reaches the decoder. A FLAMM-owned word that is absent was never written (the contract is tracked
from its creation, and a write of zero to a zero slot is no storage change), so it reads as zero, except the
words the pinned code writes non-zero when it constructs the contract (the hook's tuning row, slots 4 to 6:
fee parameters, inventory surcharge and half-lives, and its reservation price; the pool's dials, fee bounds,
loan band and share supply), which the decoder requires; an absent
Morpho/IRM/feed word is unknown and the decoder refuses to quote.

Feeds, per role `<f>` in `asset` (cbBTC/USD), `loan0` (USDC/USD), `seq` (sequencer uptime), `mo0` (BTC/USD
behind venue 0's Morpho oracle):

| attribute | bytes | source |
|---|---|---|
| `feed:<f>:aggregator`, `feed:<f>:phase` | 20, 32 | proxy slot 2 (`uint16 phaseId | address aggregator`), a storage change (`confirmAggregator` emits no log on these v0.6 proxies) |
| `feed:<f>:access_controller` | 20 | proxy slot 5; `latestRoundData` reverts unless zero (`checkAccess`) |
| `feed:<f>:check_enabled`, `feed:<f>:access_list` (asset, loan0, seq) | 32 (0/1) | the aggregator's `SimpleWriteAccessController` pair: `checkEnabled` and `s_accessList[proxy]`; `hasAccess = access_list || !check_enabled` |
| `feed:<f>:kind` | ascii `ocr2` / `uptime` / `dual` | the manifest `aggregators` kind of the current aggregator (an addition to the schema so the decoder knows which access rule applies); absent when the manifest does not list the aggregator, and the decoder then refuses to quote (it cannot tell which access rule applies) |
| `feed:<f>:round`, `feed:<f>:answer`, `feed:<f>:started_at`, `feed:<f>:updated_at` (asset, loan0, seq) | 32 | OCR2: `s_hotVars.latestAggregatorRoundId` (slot 11) and `s_transmissions[round]` (answer, observationsTimestamp, transmissionTimestamp), what `OCR2Aggregator.latestRoundData` reads; uptime feed: `s_feedState` (slot 4: round, status, startedAt, updatedAt); `round` is the aggregator round id, the proxy's is `phase << 64 | round` |
| `feed:mo0:round`, `feed:mo0:secondary_round`, `feed:mo0:cutoff`, `feed:mo0:tx:<r>` | 32 | `s_hotVars` (slot 13: `latestAggregatorRoundId`, `latestSecondaryRoundId`), `s_cutoffTime` (slot 18), and the packed `Transmission` word (answer, observationsTimestamp, recordedTimestamp) of every round the secondary-path reveal can answer with, `s_transmissions[r]` as stored; entries leaving the window are deleted. The `DualAggregator` has no `answer` / `started_at` / `updated_at` attributes: the ring word of `round` carries them (schema 3.2). The window is `latest-20..=latest` plus the secondary round, one round more than `_getSyncPrimaryRound` visits (`latest-19..=latest`, `DualAggregator.sol:530-548`, `i_maxSyncIterations = 20`): a deliberate superset, the seeds and the schema snapshot carry 21 |

Answers are two's-complement 32-byte words. Every feed attribute is decoded from the aggregators' storage words,
which they write before emitting their events (`OCR2Aggregator._report`, `DualAggregator.sol:931-964`,
`OptimismSequencerUptimeFeed._recordRound` / `_updateRound`); the events themselves are not read. The unit tests
decode the events of real rounds (`NewTransmission`, `AnswerUpdated`, `RoundUpdated`, `SecondaryRoundIdUpdated`,
`src/flamm/feed_events.rs`) as an independent check that the layouts are right.

Static attributes (schema 3.3): `implementation`, `implementation_codehash`, `hook`, `hook_codehash`,
`hook_loan_scale`, `hook_genesis_strategy_hash`, `hook_genesis_params_hash`, `leverage_hook`,
`leverage_hook_codehash`, `leverage_hook_loan_scale`, `leverage_hook_swap_hook`, `spread_hook`,
`spread_hook_codehash`, `router`, `router_codehash`, `price_feed`, `price_feed_sequencer`,
`price_feed_sequencer_grace`, `factory`, `factory_upgrade_delay`, `pool_asset`, `loan_asset_0`,
`venue_0_account`, `venue_0_account_codehash`, `venue_0_market_id`, `venue_0_morpho`, `venue_0_irm`,
`venue_0_oracle`, `venue_0_oracle_scale_factor`, `venue_0_oracle_base_feed_1`, `feed_mo0_secondary_proxy`,
`feed_mo0_max_sync_iterations`, `irm_codehash`, `feed_{asset,loan0,seq,mo0}_proxy`, `component_kind` (0 swap,
1 lever-up). Addresses are 20 bytes, everything else a 32-byte word. Sources: the `PoolCreated` log and
`createPool` calldata (addresses, the invariant hook's codehash, the market id `= keccak256(venueParams)`), the
storage the creation transaction wrote (router, price feed, the account's Morpho/oracle/IRM, the feed proxies from
`PriceFeed._tokens`), the deployments registry (the other codehashes), and the manifest `immutables`. A pool
created without the leverage pair (`HookSet.leverageHook == spreadHook == 0`, valid per
`FLAMMOpsLib._checkHookSet`; its lever paths revert `LeverageDisabled`) carries zero `leverage_hook`,
`spread_hook` and codehashes and tracks no `spread:` words; its lever-up component exists and the decoder refuses
it on the zero hook.

## Balances

The pool's tradable inventory as the contracts count it (design 5.1): pool asset `= physicalPoolAsset + Σ
min(position.collateral, venue.managedCollateral)` over non-retired venues (`FLAMMGateLib.grossOf` →
`MMRouterLib.positions` → `read`); loan asset `= loans[0].liquid + Σ recognizedSupplied`, the managed supply
shares valued at the market's totals (`MorphoBlueAccount.tryPosition` / `supplySharesToAssets`, `mulDiv(shares,
totalSupplyAssets + 1, totalSupplyShares + 1e6)` rounding down). Two simplifications, neither on a quote path:
the Morpho totals are the stored ones (the contract accrues them to `block.timestamp`; the decoder, which carries
the IRM, does), and every venue is taken as readable. The pool's ERC-20 balances are not the inventory (a sell
borrows on Morpho), hence `skip_balance_check` in the range test.

## Params

One string for every module, `key=value` pairs joined by `&` (`src/config.rs`). Every value is a chain fact and
the unit tests assert the manifest carries exactly the fixture values in `testdata/`:

| key | value | verified by |
|---|---|---|
| `factory` | `0x1BfcE014774D0DD7e04bC595D46Fa09F7dCCF45f` | the `PoolCreated` log of the creation tx |
| `hook_codehashes` | the `EverlongHook` runtime codehash | `eth_getCode(hook, 51154990)`, `PoolCreated.invariantCodehash` |
| `deployments` | `role:codehash` for implementation, pool proxy, hook, leverage hook, spread hook, router, price feed, factory, account | `eth_getCode` at 51154990 == the deployment record |
| `aggregators` | `0x51ce…:ocr2`, `0x68be…:ocr2`, `0x606c…:uptime`, `0xe5ec…:dual` | the aggregators' verified sources (schema 2.6) |
| `addresses` | the four aggregators | every storage write tracked (round words have per-round keys); every `aggregators` entry must be listed, the params are refused otherwise |
| `words` | 48 `address:slot:value` seeds at `initialBlock - 1 = 51154965`: Morpho market and position words, the IRM rate, each proxy's slots 2 and 5, the guarded aggregators' `checkEnabled` / `s_accessList[proxy]`, the OCR2 `HotVars` and latest transmission, the uptime feed's `s_feedState`, the `DualAggregator`'s `HotVars`, cutoff and 21-round ring | `eth_getStorageAt` at 51154965, each cross-checked with its view (`aggregator()`, `phaseId()`, `accessController()`, `checkEnabled()`, `hasAccess(proxy, "")`, `latestRoundData()`, `getRoundData(r)`, `Morpho.market(id)`, `rateAtTarget(id)`) in `testdata/seeds_51154965.json` |
| `immutables` | per pool, `name=value;…` for the 13 immutables listed above | the getters' answers in `testdata/immutables_51154990.json`; the DualAggregator's `i_secondaryProxy` / `i_maxSyncIterations` from its verified bytecode |

Seeds are the one place the state does not come from the stream; they are what the design (section 4.4) asks
PropellerHeads to review.

A further pool needs a package update carrying its `immutables` entry and, unless it shares them with the live
pool, its external words: the Morpho market and position words and the IRM rate of each venue and the slots 2
and 5 of each feed proxy in `words` (valued at 51154965, zero when the market or account did not exist yet), and
each proxy's aggregator in `aggregators` and `addresses`. `store_words` keeps nothing else outside the
registered contracts, so `map_components` refuses a pool whose external words the manifest does not track
(`statics::untracked_external_words`, the missing names logged).

A package update that changes `aggregators`, `addresses` or `words` (listing the aggregator a proxy rotated to,
seeding a further pool's words) changes what the attributes are a function of: the protocol is then re-indexed
from `initialBlock` with cleared state, not resumed from the cursor. Rows written by the previous package are
not the new package's `feed_state` of the words, so a resumed extractor would keep a feed's `kind` and rounds
absent until its next rotation, and that rotation could delete a row the indexer never held (which fails the
block's write, see "Failure modes").

## Failure modes, deliberately closed

* A pool whose hook code, immutables, contract provenance or external words are unknown to the manifest is not
  emitted (a pool with untracked external words would have them valued as unknown in every block after its
  creation: its balances would miss the venue and a feed behind an unlisted aggregator would never quote).
* A proxy rotation replaces the previous aggregator's attributes with the new aggregator's, both derived from
  their words (`feeds::feed_state`): what the new aggregator's words do not give (its `kind` when the manifest
  does not list it, a word never written or seeded) is deleted, and the feed fails closed until a package update
  lists and seeds it. An aggregator the manifest does not list has no tracked writes, so its rounds are not
  decoded either: after such a rotation the feed carries `aggregator`, `phase` and `access_controller` only.
* A `Deletion` only ever names an attribute the indexer holds. `tycho-storage` fails a block's write on a
  deletion of a row it does not have (`versioning.rs`, `set_partitioned_versioning_attributes`: `Missing
  deleted row`), which would halt the extractor at that block. The package guarantees it by construction: the
  feed attributes are one function of the words (`feed_state`), emitted whole at creation and as a diff of the
  function's value before and after every transaction that touches the feed's words, so the rows the indexer
  holds for a feed are exactly `feed_state` of the words store, and a deletion names a row of that state. The
  tests replay multi-block histories (rounds, configuration writes, rotations to listed, unlisted and unseeded
  aggregators and back) against the storage rule and this invariant (`src/verify_tests.rs`, `Rows` / `Chain`).
* A `Creation` only ever names a row the indexer does not hold and an `Update` one it holds. `tycho-indexer`
  reverts an `Update` by restoring the row's prior value from its buffer or the database and counts one without
  any as an attribute miss (`extractor/protocol_extractor.rs`, `extractor_revert_attr_miss`, reverted as a
  deletion), and reverts a `Creation` by deleting the row (`AttrRevert::CreatedInRange`), which would lose the
  prior value of a held one. A tracked word has a row once written (the words store, which tracks every FLAMM
  contract from its deployment), seeded, or carried as a zero row by the creation snapshot (the venue
  accounts' Morpho positions), so the change type of a write is decided from the word's value before the
  transaction. The replays assert the rule on every attribute of every block.
* A Morpho position funded on behalf of a venue account before the account exists is not observable; the
  snapshot carries zero and the first Morpho write corrects it (the router's `managedCollateral` bounds what the
  pool recognizes anyway).

## Tests

`cargo test -p base-flamm` replays real Base data through the pure cores of every module (the store-backed
handlers themselves need the substreams host): the `createPool` transaction (components, static attributes equal
to the schema snapshot, the creation snapshot, balances, and the same creation without the leverage pair), the
pool's first swap (the 12 words it moved, the inventory after it), the seeds against their views, one round of
each aggregator from the aggregator's words after it cross-checked against its events, a synthetic proxy
rotation, and the words-store filter. `src/verify_tests.rs` adds the curator's activation transaction (block
51298416: exactly the two pool words it moved, from real `eth_getStorageAt` diffs), synthetic blocks for the
ordering paths (a word written twice in one transaction, a tracked write before the creation transaction,
rotations of the Morpho oracle's proxy with and without a round of the same block, a round and a rotation in
one transaction, a secondary-only `DualAggregator` transmission, a Morpho accrual with and without managed
supply), and multi-block histories replayed against the indexer's storage rule. See `testdata/README.md` for
provenance.

### End to end, without the hosted harness

`src/e2e_tests.rs` is the local equivalent of the range test. `testdata/e2e_blocks.json.gz` carries real Base
blocks assembled from `eth_getStorageAt` / `eth_getTransactionReceipt` / `eth_getCode` (no `debug_*`, no
Firehose; the recorders are `fetch.py`, `morpho_events.py` and `pack.py` under
`crates/tycho-simulation/src/evm/protocol/flamm/testdata/gen`, see its README): every tracked word at `initialBlock - 1` (equal to the manifest seeds), the creator's deployment
blocks 51154978-51154988 with the runtime code they created, the pool's creation block, the first stop block
of the range test (51155010), the curator's activation (51298416), every swap the pool has settled (51302916,
51343234, 51347390, 51420672, 51420867, 51430828), two of its six deposits (51300667, 51426394), one of its
four withdrawals (51348093, a `redeemToAsset` through the periphery), one of the keeper's recenters (51384803),
the second stop block (51302920), three recent blocks each carrying a Chainlink round (USDC/USD 51429815,
cbBTC/USD 51433135, the Morpho oracle's `DualAggregator` 51433218) and the block that unpaused leverage
(51433699, `LevPauseSet(false)`, the curator Safe calling `FLAMM.setLevPaused(false)`). A block of interest is fed as its own transaction
carrying the block's storage diff, its receipt logs and its code changes; the net change of every tracked word
between two blocks of interest is fed as one synthetic transaction at the block before the next one (the words
store and every attribute are functions of the words' values, so the fold after it is the fold the real stream
reaches after the same blocks). The pool's other deposits (51343943, 51347236, 51347413, 51353593), withdrawals
(51344221, 51347256, 51353599) and keeper recenters and pokes are carried that way: their net effect on every
tracked word reaches the fold, their own blocks are not replayed. The module cores run in the manifest's order
with the stores kept as the engine keeps them (`store_deployments` read at its end-of-block state,
`store_words` and `store_pools` as of the start of the block), and the output is folded exactly as the indexer
holds it (`tycho-storage`: a creation or update inserts or replaces a row, a deletion must find one) and as the
client delivers it (`ProtocolComponentStateDelta::merge` over the block's transactions).

Verified there: the deployments store records exactly the nine registered contracts and ignores the creator's
others; the creation block emits the two components with the ids, tokens, static attributes and creation
transaction `integration_test.tycho.yaml` expects (the yaml is read by the test), born paused and lever-paused;
after every block both components carry the same rows, every `Creation` names a new row and every `Update` a
held one (the indexer's revert path depends on it, see "Failure modes"), the feed rows equal `feed_state` of
the words store, and at the pinned blocks every tracked word of the store equals `eth_getStorageAt` at that
block; the fold at 51302915 is the schema snapshot (`testdata/snapshot_51302915.json`, every value checked
against the contracts' views) attribute for attribute and balance for balance; the activation and the first
swap forward exactly the two and twelve words those transactions moved, the withdrawal its nine pool, Router,
Morpho and IRM words with the pool asset inventory, the recenter the twelve words of the hook's book alone,
the unpause the pool's flags word alone; the pause bits follow the chain (paused until 51298416, leverage
paused until 51433699); the yaml's skip flags are what the chain justifies at each stop block (`previewSwap`
reverts `Paused` at 51155010 and answers at 51302920; `previewLever` reverts `LevPaused` at both). The fold is
written out as `crates/tycho-simulation/src/evm/protocol/flamm/testdata/snapshots/e2e_stream.json.gz` (the
test asserts the committed file equals the package's output; regenerate with
`FLAMM_E2E_WRITE=<path> cargo test -p base-flamm e2e_stream_fixture`), and the `flamm` `ProtocolSim` replays that
stream as the stream decoder would (a snapshot at creation, a `delta_transition` per block, the clock advanced
to the next block) and compares its quotes with the pool's own `previewSwap` / `previewLever` answers recorded
by `eth_call` at the pinned blocks and with every settled swap's receipt (`tests/e2e.rs` of the flamm module).

### The range test

`integration_test.tycho.yaml` needs a StreamingFast endpoint and a Postgres, which the environment this package
was written in did not have, so it was not run; the local replay above is what stands in for it. To run it:

```bash
# prerequisites: docker (the Postgres), the `substreams` CLI, `tycho-indexer` on PATH (`cargo install --path
# crates/tycho-indexer` from the monorepo, or the docker route below), a StreamingFast token
# (SUBSTREAMS_API_TOKEN; the endpoint https://base-mainnet.streamingfast.io:443 is picked by --chain base), a
# Base RPC (archive not needed: the harness reads token metadata and forks at the stop block to execute)
cd protocols/testing
docker compose up db -d                       # or a local Postgres; each run drops and recreates the database
export RPC_URL=https://mainnet.base.org       # any Base RPC
export SUBSTREAMS_API_TOKEN=...               # StreamingFast
export DATABASE_URL=postgres://postgres:mypassword@localhost:5431/tycho_indexer_0   # the compose default
cargo run -- range --package base-flamm --chain base
cargo run -- range --package base-flamm --chain base --match-test test_pool_creation   # one test at a time

# or everything in containers (the image builds tycho-indexer and the wasm; `base-` infers --chain base and
# BASE_RPC_URL overrides RPC_URL for the package)
export SUBSTREAMS_API_TOKEN=... RPC_URL=https://mainnet.base.org PROTOCOLS="base-flamm"
docker compose up --build --abort-on-container-exit
```

The harness compiles the package for `wasm32-unknown-unknown` (pass `--prebuilt-wasm` to reuse a binary), packs
it, runs `tycho-indexer` over each test's block range, fetches the expected components from the indexer's RPC
at the stop block, compares ids, tokens, static attributes and creation transaction with the yaml, skips the
balance check (`skip_balance_check: true`, see "Balances"), decodes the snapshot through the `flamm` entry of
`protocols/testing/src/state_registry.rs` and, for every component whose `skip_simulation` is false, asks the
limits of both directions and quotes 0.1%, 1% and 10% of each non-zero limit at the block after the stop block.
Expected output:

* `test_pool_creation` (51154966-51155010): both components found and matching; simulation and execution
  skipped for both (the pool is paused, every direction's limit is zero and the harness would report "No
  tradable direction" otherwise).
* `test_activation_and_first_swap` (51154966-51302920): both components found and matching; the swap component
  quotes both directions (the largest full fills at 51302920 are 158168 sats and 180858003 USDC base units,
  `previewSwap`'s own edges; the three sizes of each direction fill in full); the lever-up component is
  skipped (`levPaused` at the stop block: leverage stayed paused until 51433699, and since the unpause every
  lever-up still reverts `SpreadUnavailable`, the keeper never having re-posted a spread after the
  `LeverageSpreadHook` constructor's 17500 ppm post aged past `maxSpreadAge = 3600 s`, so no block yet shows
  the venue quoting; a live spread would make it quotable, which no fixture covers); the swap component's
  quoted sizes are executed through the `FLAMMExecutor` the harness holds under `flamm`
  (`protocols/testing/fixtures/FLAMM.runtime.json`, the `flamm` row of `EXECUTOR_MAPPING` in
  `protocols/testing/src/execution.rs`) on a fork of the stop block; the lever-up component's execution is
  skipped with its simulation.

The second test streams ~148k Base blocks (the pool sat paused for ~143k of them); on the hosted stack that is
minutes of substreams time. A run that fails at the component comparison prints the differing field; one that
fails at simulation prints the component, direction and size (the local replay's `check_grids` reproduces the
same quotes from the same rows, so a failure there is a difference between the hosted stream and the fold of
the recorded storage diffs, which `e2e_stream_fixture_is_the_package_output` pins).

`abi/FLAMMFactory.json` is the factory's `PoolCreated` event and `createPool` function, extracted from the
`blockend` build artifacts; the tests derive the topic and selector from it and check them against the constants
in `src/flamm/calldata.rs` (no abigen: the calldata is decoded with explicit `ethabi` types).

Build: `cargo build --target wasm32-unknown-unknown --release -p base-flamm`, then `substreams pack
base-flamm.yaml`.
