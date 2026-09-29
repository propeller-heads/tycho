# Recorders of the pool snapshots and the end-to-end blocks

The scripts that read the deployed Base pool (`0xc0fdCB1799cCc2CEBaA1fe247157b0dF33D57572`, c104 @
`80abd43`) into the fixtures of `../snapshots/` (README section 5 and 6) and of
`protocols/substreams/base-flamm/testdata/` (`e2e_blocks.json.gz`, `snapshot_51302915.json`). Every
read is `eth_getStorageAt`, `eth_getBlockByNumber`, `eth_getLogs`, `eth_getCode`, `eth_call`,
`eth_getTransactionByHash` or `eth_getTransactionReceipt` against an archive endpoint
(`rpc.py`, `https://mainnet.base.org` first: paced and backing off under its rate limit, no
`debug_*`). Python 3 with `pycryptodome` (`Crypto.Hash.keccak`); nothing else.

| script | role |
| --- | --- |
| `rpc.py` | the JSON-RPC client (retry, back-off, endpoint fallback), keccak and ABI helpers, slot arithmetic (`map_slot`, `array_base`) |
| `schema.py` | the storage schema: every word the decoder reads, its address, slot, packing and attribute name (`FLAMMStore.S` at `FLAMM_NS`, `EverlongHook`, `LeverageSpreadHook`, `MMRouter`, `MorphoBlueAccount`, `PriceFeed`, `FLAMMFactory`, Morpho Blue, `AdaptiveCurveIrm`, the Chainlink proxies and aggregators), with the views that report each field |
| `reads.py` | the chain reads over the schema: the words, the views, the aggregators' storage and events, the `DualAggregator` round selection (`_getLatestRound` / `_getSyncPrimaryRound` over the secondary proxy) |
| `snapshot.py` | `python3 snapshot.py <block>...`: a `ComponentWithState`-shaped snapshot of both components (swap and lever-up) at a block, purely from storage, logs and code, into `out/schema/<block>.json`; the source of `snapshot_51302915.json` (its swap component, attributes, balances and block) |
| `grids.py` | `python3 grids.py <block>...`: extends `out/schema/<block>.json` with `feed:<f>:kind` and the chain's answers at the block (`previewSwap` over a log-spaced grid, every probe of the edge search and the edges, `previewLever`, `EverlongHook.spot`), into `out/grids/<block>.json` |
| `package.py` | `python3 package.py <block>...`: `out/grids/<block>.json` gzipped (mtime 0) into `../snapshots/<block>.json.gz`, printing the digest rows of `tests/fixtures.rs` and README section 5 |
| `keys.py` | the tracked key sets per role, mirrored from `protocols/substreams/base-flamm/src/flamm/keys.rs` |
| `universe.py` | the words the package tracks for the live pool plus the Chainlink words `feed_state` reads (the proxies' phase and access words, the aggregators' hot words, read-access pairs, cutoff and the transmissions of the `DualAggregator` window), and their read at a block |
| `unpack.py` | `python3 unpack.py`: the committed end-to-end fixtures back into `out/stages/`, so a stage can be appended without re-reading the 27 already recorded ones; `pack.py` straight after it reproduces both fixtures byte for byte |
| `fetch.py` | `python3 fetch.py`: the stages of the end-to-end replay (`STAGES`: kind, block, transaction, created contracts, grids), one resumable JSON per stage under `out/stages/`: the tracked words before and after the block, the net diff since the previous stage, the transaction of interest with its receipt logs, the aggregators' logs, the created contracts' runtime code and, at the pinned blocks, the grids of `grids.py` |
| `morpho_events.py` | `python3 morpho_events.py`: for every stage with a pool transaction, the Morpho Blue events of the venue's market that the block's other transactions emitted (`out/stages/morpho_<block>.json`), so the market totals the block left can be reconstructed from the pool's own settlement |
| `pack.py` | `python3 pack.py`: the stages into `protocols/substreams/base-flamm/testdata/e2e_blocks.json.gz` and `../snapshots/e2e_grids.json.gz` (gzip mtime 0 of `json.dumps(sort_keys=True, separators=(",", ":"))`), printing both digests |
| `regen.sh` | `pack.py`, then the stream fixture `../snapshots/e2e_stream.json.gz` through the package's own test (`e2e_stream_fixture_is_the_package_output` with `FLAMM_E2E_WRITE`), then the digests to pin |

Section 5 (`../snapshots/{51302915,51313000,51409000}.json.gz`):

```sh
python3 snapshot.py 51302915 51313000 51409000
python3 grids.py 51302915 51313000 51409000
python3 package.py 51302915 51313000 51409000
```

Section 6 (`e2e_blocks.json.gz`, `../snapshots/e2e_grids.json.gz`, `../snapshots/e2e_stream.json.gz`):

```sh
python3 unpack.py           # only when out/stages/ is empty: the committed fixtures back into it
python3 fetch.py            # resumable; ~13 minutes per stage under mainnet.base.org's rate limit
python3 morpho_events.py
./regen.sh
```

`regen.sh` cannot finish in one pass when the stage list changed. Its write pass runs
`e2e_stream_fixture_is_the_package_output` under `FLAMM_E2E_WRITE`, and that test reads the committed
fixture through `include_bytes!`, so it compares the new output against the stale embedded copy and
fails; `set -e` then stops the script before its verification pass. Run `regen.sh` once to write the
fixture, then re-run the same test without `FLAMM_E2E_WRITE` to verify it and print the digests. Nothing
is wrong with the fixture this produces — the failure is the compiled-in copy being a revision behind.

Adding a stage to `fetch.py`'s `STAGES` invalidates the catch-up diff of the stage after it (the net diff
is taken from the previous stage): delete that stage's file under `out/stages/` before re-running
`fetch.py`. Appending one at the end invalidates nothing, which is how the two blocks around the
`LeverageSpreadHook`'s `MaxSpreadAgeSet(0)` (51649706 and 51670000) were added. `out/` is the scripts'
working tree and is not committed; the committed fixtures are byte-for-byte what `package.py` and `pack.py`
write from it, and the digests in the READMEs and in `tests/fixtures.rs` pin them. `unpack.py` rebuilds
`out/stages/` from the committed fixtures when it is empty, so that byte-for-byte property is what makes a
later stage cheap to add: check it by running `pack.py` before touching `STAGES`.

| script | sha256 |
| --- | --- |
| `rpc.py` | `74f4471983ca4f54d3c05cf6a0b29694d83f3c7eebb5c6e0b8e5827e554205f7` |
| `schema.py` | `c583cc9c1a6cb5f2d921eaf94b7c57fb208cabf2c781a8d79fbd29dc5e9c7030` |
| `reads.py` | `fefcf92ba67af3d89355ebb8952452eba679e886c13de11005ea5d27d2e105fa` |
| `snapshot.py` | `c0de776246d204edc8fb7c02adc2f0f19d3cd0f805e62d0274db25b22cccd81c` |
| `grids.py` | `b0a4eee09417537fa465c16cae0876f9344f6e25f92a82ba89ab62145c0ac311` |
| `package.py` | `d498debec3743928522f03873171571b05d977446b512bd6587da0da51ed62b5` |
| `keys.py` | `6de0fd7055da621ee6394674fa10b04f26d3da8f6131db14119aaf255017f17b` |
| `universe.py` | `2e956fe6bcd95ae53636dfd2a03e699ed6385d1fd0a9080c86a1ad0e0b02883d` |
| `fetch.py` | `d77fbeca0b5003641e955b0e4d9ba7e5ee7c554232a21af9154d94dcd9c2a864` |
| `morpho_events.py` | `de7c6422884a9e368ff161b990497a5252fa7b1477b9ea2d372dbc9f21280f22` |
| `pack.py` | `7738b4a56d6abf3a997424ea614000091bbe7ceeb22930f6b4973963f269cd6f` |
| `unpack.py` | `778bddf1a848523a9ea523736851bd02e20fbd0d6f79d8cd9d7541c6fc20cef2` |
| `regen.sh` | `fe102d38d0523be043b2ceffc9fc5e84864392458feb69e7d8f6564f46e39cc0` |
