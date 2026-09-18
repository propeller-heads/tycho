# FLAMM parity fixtures

Solidity-generated fixtures the `flamm` module's parity tests (`../tests/`) replay row by row, to the
wei and by revert class, with no sampling and no tolerance. Sections 1-4 are a selection of the
fixtures of the Go simulator of the same pool (`kyberswap-dex-lib`
`pkg/liquidity-source/everlong/flamm/testdata`, whose `README.md` and `gen/` directory hold the full
provenance and every generator); the files there are byte-identical copies, so their stored digests
below are the ones that README pins. Section 5 holds the deployed pool's own component snapshots and
`eth_call` answers, read from Base for the `ProtocolSim` tests.

## Common provenance

- Solidity source of truth: the c104 tree at commit `80abd43` (`80abd43dc4ea53fe612e6267a471f2937fc29f9c`),
  as deployed on Base (chain 8453): pool `0xc0fdCB1799cCc2CEBaA1fe247157b0dF33D57572`, swap hook
  `EverlongHook 0x65CBD227cBC61248ae77a5fC813A29C54C092134`, leverage hook `EverlongLeverageHook
  0xE0A98d8e60035832B8BaD7f7af7B9B0b3A7308F3`, `CollRebalancerMath 0xC002d0731E6a2E6e80Be754779bCEf6B01Aff0bb`,
  `AlmCurve 0xf82DdF0A8a50bc2C3F163997766bA1839E527A17`, `MMRouter 0x19A9…6bB4`, `MorphoBlueAccount
  0x6760…6c48`, `AdaptiveCurveIrm 0x46415998764C29aB2a25CbeA6254146D50D22687`. Foundry 1.7.1.
- Generators are Foundry tests named below as `gen/<file>`; each is copied into `test/kyber/` of a c104
  checkout at that commit and run from the tree root with the command quoted in the Go port's README
  (`FOUNDRY_SPARSE_MODE=true forge test --match-path test/kyber/<Generator>.t.sol -vv --gas-limit
  9223372036854775807`, `--via-ir` where the writer needs it, `--isolate` for the sequences). Fork
  generators read Base over RPC at the block named per fixture. Re-runs are byte-identical.
- Every file is `gzip -9 -n` of the generator's output (single JSON files compacted with `jq -c .`
  first); `lev_curve_tape_v1.tar.gz` is a deterministic ustar+gzip archive (python `tarfile`, mtime 0).
  The "stored" digest is of the file as committed, which `tests/fixtures.rs` checks before any row is
  replayed; the "uncompressed" digest is of the generator's output.
- The fixture loader quotes every bare JSON number of sixteen or more digits before parsing, since
  the edge generators write 256-bit words as numbers.
- Not carried over, to keep the set under 6 MB: `edges/lev_hook_edges.json.gz` (1.8 MB, 33,808
  `frame` / `previewLever` / `_assertAnchorAndBand` rows on the deployed leverage hook with a mocked
  book; the hook is covered here by `lev_hook_{local,fork,band}_fixture` and by every leverage row of
  the pool-core grids and sequences), and the Kyber tracker / fork fixtures of the Go port's section 5,
  which are not on the port's quoting path.

## 1. Swap hook: `AlmCurve`, `EverlongStrategy.fillFee`, `EverlongHook` fill (`tests/swap_hook.rs`)

Generators: `gen/AlmCurveGrid.t.sol`, `gen/FeeFillGrid.t.sol`, `gen/HookFillGrid.t.sol`,
`gen/HookLiveSwapTrace.t.sol`; edges: `gen/SwapHookEdgesBase.sol`, `gen/AlmCurveEdges.t.sol`,
`gen/FeeEdges.t.sol`, `gen/HookFillEdges.t.sol`.

- `alm_curve_grid` forks Base at block 51310000 and calls the deployed `AlmCurve` library (`supportFor`,
  `reservesAt`, `swapExactInX96`; `yAtX` read through `reservesAt` on a full-domain support, `priceAtX`
  through the live hook's `spot()`).
- `fee_fill_grid` calls `EverlongStrategy`'s internal `fillFee` / `reductionG` / `volMultiplier` /
  `logRatioAbsWad` through a harness compiled from the c104 source.
- `hook_fill_grid` forks at 51310000 and drives the live `EverlongHook` with its storage overwritten per
  state, recording `spot`, `bookFor`, `previewFeeWad`, `previewExactIn` and a pool-pranked
  `executeExactIn` whose committed book is read back from storage.
- `hook_live_swap` is the state at block 51302915 and the context of the pool's first settled swap (tx
  `0x46c3cd72a5860b2fe546e5a2130e066314e3777027151661e1e4f19a935901fa`, 15000 sats -> 11301759 USDC),
  transcribed from the trace. Stored gzipped here (the Go port keeps it as plain JSON, uncompressed
  digest `ab07329ca46620ca9d859747472a98d7603080c70028d8228311d83a6c20b220`).
- `edges/alm_curve_edges`: the internal curve functions through a source-compiled harness and
  `reservesAt` / `swapExactInX96` against the deployed library at 51310000: domain and amplification
  edges with 1-wei neighbours, the `b = 0` branch point, clamp and seed-skip gates, band truncation,
  malformed supports, a keyed random grid.
- `edges/fee_edges`: Solady `lnWad` at every power-of-two boundary and top-byte pattern, and the fee law
  with its overflow / zero-denominator panics and its tie, ramp and band thresholds.
- `edges/hook_fill_edges`: the live hook at 51310000 under overwritten storage (lazy-rescale branches,
  retracted and invalid books, invalid `_p.aWad` / support, spot overflow, fee-row edges), caps at
  1-wei neighbours of the realised net; `edges/hook_fill_loan_scale_edges`: the same with the hook's
  `LOAN_SCALE` PUSH32 sites patched to 1 and 1e10; `edges/hook_live_tx_edges`: the real sell re-driven
  through a calldata tap for its exact `executeExactIn` context.

| file | sha256 (stored) | sha256 (uncompressed) |
| --- | --- | --- |
| `alm_curve_grid.json.gz` | `04b679fb9a16062be73e3af140d65628b88a7b01e13c963189ff79af12c16373` | `ab374ed44844116a557ca41a508ea977b50701a073309db6564739abe26f5327` |
| `fee_fill_grid.json.gz` | `2d704853e4f3c88784646aa8fac663dabb4b1f793ee32f3386b9a4b1046fc5e3` | `d44fc45c2da5a84a99618abda64fbf2cdc8b567e4722863da517e415e6ad2608` |
| `hook_fill_grid.json.gz` | `ecd74e6003afd5cabf5b2a1c73df9e014f7cc5b91dc1c0cfb984767cbbe2b05b` | `892cfeda310f5c16a7dc0b524e0aff52d09941d49ff4528a0df4ebea79c23453` |
| `hook_live_swap.json.gz` | `93124624cfc488cd3fb0a14508405a637258b103f7fd3318dcaa4fac9985dc2f` | `ab07329ca46620ca9d859747472a98d7603080c70028d8228311d83a6c20b220` |
| `edges/alm_curve_edges.json.gz` | `3636618f5913e6aaf430b392736ac18f0e09c056b81b2b95e780530ee098a12b` | `cebb7308e5136e98185b0e92934b72256bd1275a7d3538f5115ec970c4ede4de` |
| `edges/fee_edges.json.gz` | `7959030d1ad231297c9690481882c6129d988454340bbeca27a0454c81d84253` | `b4e060c4a69f598a66b9764feab94590140a605ccc98ba4dfe7d472a75d0f1a9` |
| `edges/hook_fill_edges.json.gz` | `408011fc26d01bd6665dcdbe557419034dbcdfdbac59ad6a890a6279e4220b3b` | `42ac0a7800960c47bc3588c43b11aa3029a06e50f669a8fc318e1038e109b882` |
| `edges/hook_fill_loan_scale_edges.json.gz` | `8850d02792690121ecebb898088727fdc6d514dc122d98028e182d151add38ac` | `770b6944aab551db47235f22b20eccfa3f3906e32bdffe66090fd5fe5f1ded3b` |
| `edges/hook_live_tx_edges.json.gz` | `70562e9bb36f73fd388129f8a938e2189bda2fd205003486c94a61653809a271` | `d28bdcc17d7ea751659602c4ad32289c66b8a7bee97b671a565c15dbe8d15983` |

## 2. Financing: Morpho Blue, `AdaptiveCurveIrm`, `MorphoBlueAccount`, `MMRouterLib`, `FLAMMGateLib` (`tests/financing/`)

Generators: `gen/GateMathFixture.t.sol`, `gen/MMFixtureBase.sol`, `gen/MMFinancingFixture.t.sol`
(module fixtures); `gen/FinancingEdgesBase.sol` and `gen/{Morpho,Account,Router,Gate,Settle}Edges.t.sol`
(edges); `gen/GateIntEdges.t.sol` (integer edges); `gen/RouterSettlementEdgesBase.sol`,
`gen/RouterSettlementEdges.t.sol`, `gen/MorphoMarketEdges.t.sol` (settlement edges);
`gen/FinancingSequenceBase.sol`, `gen/RouterSequences.t.sol`, `gen/SwapSettlementSequences.t.sol`,
`gen/MorphoAccrualGrid.t.sol` (sequences, `--isolate`). All fork Base at block 51317000 except
`gate_math` (a harness with a mock router and feed) and the integer edges (no fork); the `mm_*` and
`edges/` runs write storage into the DEPLOYED Morpho Blue, `AdaptiveCurveIrm`, `MorphoBlueAccount` and
`MMRouter`, and the settlement runs DELEGATECALL the libraries the deployed pool implementation links
(`FLAMMSwapLib 0x89aA5f76765D16c460A5B4a7AC6a385e35d2D405`, `FLAMMGateLib
0x50417cB978f856b3885AbfA97308e0377FC2844e`).

- `gate_math`: `FLAMMGateLib` over 400 seeded books; `gate_int_edges`: 20 books on the gate's `int256`
  edges (wrapping casts at 2^255, `type(int256).min`, a `mulDivUp` floor at `type(uint256).max`);
  `mm_muldiv_edges`: OpenZeppelin `Math.mulDiv` floor / up over every triple of 15 boundary values plus
  400 seeded triples.
- `mm_irm_grid`: `borrowRateView` / `borrowRate` over rateAtTarget x supply x utilisation x elapsed;
  `mm_live_views`: router / account / Morpho / Lens views at 51317000 and warped +1s..+365d;
  `mm_live_settle`: real swaps through the live pool with pre / post state; `mm_real_sell`: the state
  around tx `0x46c3cd72…` replayed in its own block; `mm_multi_venue`: the deployed Router bytecode
  etched with fresh storage over three Morpho venues.
- `edges/mm_{morpho,irm,account,router,settle}_edges`, `edges/gate_edges`: threshold edges and seeded
  grids over the live record extended to four venues on two loan assets, with mocked oracles and IRM
  outages, each array opening with a `{"header":true}` element.
- `edges/router_settlement_edges_{one_loan,two_loans}`: the settlement legs, the gate composites and
  every Router entry, cascade and view from 22 hand-built books, seeded books and threshold sweeps;
  `edges/mm_market_edges`: Blue, the IRM and the account on two created markets with 1-wei neighbours
  of every branch threshold and a 420-row seeded grid.
- `edges/router_sequence_{a,b}` (520 steps each), `edges/router_sequence_liquidation`,
  `edges/swap_settlement_sequence_{a,b,c,d}` (600 steps each), `edges/swap_settlement_sequence_e` and
  `edges/mm_accrual_grid` (900 rows): JSON lines, one row per transaction, replayed with the Morpho
  markets, positions, `rateAtTarget`, managed fields and the pool ledger carried from the port's own
  transitions and compared with the chain before and after every step.

| file | sha256 (stored) | sha256 (uncompressed) |
| --- | --- | --- |
| `gate_math.json.gz` | `3d2c978672c08d67df2310e34fb9eb936f09562882cfdb366f1d70ca5f14fe3c` | `1451caf859e41753459fb85c35d5468c024546ffdb3df5eefdae5e8a1bae6f94` |
| `gate_int_edges.json.gz` | `2cf610563f5c38e742ca243b77f9d7c254e5949cad35ae068e33b4d7c0ec123b` | `089f7189127fe806b766f052bde717f4c43eaa07760bbd883e710c5ebc4f2616` |
| `mm_muldiv_edges.json.gz` | `29f59dd8c4989eaedbc63a4fc5817c9de8e79b548e270b1811f67f22ca050c58` | `639678627c3da3f87372826aa4678febf6e0dcbb3e88c84e6bc1967ef9fd471e` |
| `mm_irm_grid.json.gz` | `968fdfad25549694a4376a665633cec0f1e9b3cfd6f9301f21c66da98a1ec9c1` | `eeb65d925ac036186cb62f61b6b2ad9f7a94f38de935d2e5a4d912db9af3ef37` |
| `mm_live_views.json.gz` | `14478ba371bc55b0df5ac5d4427c8e35d66e487796825a7679811ad81c107488` | `167efc2a11d037805ed29d2eddba0548e61da1a17227afca3f36a5a431416998` |
| `mm_live_settle.json.gz` | `870b1a9fc3b09ab84025292874d9f304ded40ba3c87626d2ec5b703ada3f7f9c` | `6ff776c04df8fcb4d2cf2e12708424d6bfce34612966df4332aa932ed1e48f47` |
| `mm_real_sell.json.gz` | `d6d6e3ff5e9c84eaab525f570ae31a1c6aff46d9b331e96b93731bfd33330b64` | `04d693f8be8e338a50200c5236800411f5a312a270c950a094dac7a844dd255e` |
| `mm_multi_venue.json.gz` | `46b96f01244f9a8380918554adc9e662c00f8ca7d7520e8d75b216a32da4c42b` | `44332b44b82794ff51ac3ae98ada677fdf0ba4c50295f72e87e72401dd094289` |
| `edges/mm_morpho_edges.json.gz` | `e44750b12fb7f2e56374a8d3f5afb511df2aaa8c68b3ee2d2fd7468925222847` | `b0b8893602dc98f8229f4d5c5ebacd9a845580d8720c7e570322f27748a10af7` |
| `edges/mm_irm_edges.json.gz` | `f89edd49854ef05173d17b0e8e3db076cb2da2565fe058b5b73221b3fd7ad5bc` | `12040f44ac7f12a00808abd2843a9237f4e21238366ac2c3c5f78403c0c44973` |
| `edges/mm_account_edges.json.gz` | `36f62065833adc4ac9010cd47b67201ee08cb90bfc42fd9afde6ee2a01a5e945` | `62967fe3ecb0738f170aa43e8338b90d619d17428061de571e4aa794e2af36df` |
| `edges/mm_router_edges.json.gz` | `a75ca51f72abd6742e623e741a12379faa8dfec2894b297ae3859939a3efef74` | `1e8979d1fb6363d175454f0ec1cb1c88fd14b58a113824244d86d8f28716bd7e` |
| `edges/mm_settle_edges.json.gz` | `bf1aa976d3ab281f6cc64c1f264539bcde480a02a974c3eab70c2fc7fb07b2b1` | `daa4c2a975c4923225452768fe99a9890505b5abe9020a0a8d17c4f8cb45f553` |
| `edges/gate_edges.json.gz` | `08b7042989376e9a143f9561bd4bf718ae5306351b1d4e0eabc866ececa3407b` | `0508c918ddae9567915ec80e05c9a60be726f3b8e573552fa3ec03ccc1f76474` |
| `edges/router_settlement_edges_one_loan.json.gz` | `565816ed7ce9c910d51381025c15e583228e032f8325ceadd87794491b963069` | `fd801edbff89604ac8eb76ab4b12e175aabb38209e0e4eac36b0cfb88275a6de` |
| `edges/router_settlement_edges_two_loans.json.gz` | `aad4bfb10e2cd5b5cf6e754dd918328d795d62813ef38cb016d5b5ea1412f266` | `2d69e492797ca7d2504f7ce84599393e42c59a1ab5a3b5448f7f962dd69f2ff5` |
| `edges/mm_market_edges.json.gz` | `ae283f592a151ace394b26b10b969d732dfa6697be55953b21d8f4e6ab2fdb0d` | `14bad2954178b3131f8da248249be4028b84c749b52b3ae05deb8a98441ffaeb` |
| `edges/router_sequence_a.jsonl.gz` | `4bb4ffac4e4b3cbad94c5114c9f1c0c65b971892501155bcab38ad1cf277b55d` | `0c03c48720ccea2147006316afdfe70d966f3bd0dffa43eaf4a9982d34e90bca` |
| `edges/router_sequence_b.jsonl.gz` | `3adc3e50d26f41a498307bfec81b4735d70c2238ce7575d6e2f737acbce0b646` | `28bafeb5b063e23ea00113a2de7e16eecd14cc70475c52f5bcf07f289e9fe6fa` |
| `edges/router_sequence_liquidation.jsonl.gz` | `2399691adf0b8116dc8adbb4b9caf06490f37577ff96a8d26b085835ec625a54` | `b81c16705d250fa905a1660675beba71910e5245175e12c7abfbe85a6dbbe927` |
| `edges/swap_settlement_sequence_a.jsonl.gz` | `8b1d5d0c0a144097078502c5d74f4b7cd07e96b6e21ff1567c62b0b346826cd8` | `077df643e320650debd00a91e342d6533e5c3b8b3da6aefac1b502236ad39dce` |
| `edges/swap_settlement_sequence_b.jsonl.gz` | `1e1353a7a804097e97ec648a5eec328b12386418a96552f1d560bae7124491ed` | `6d982e6f3f4e4594156ccaab6af160e4d2316323bf9c287ac989fda9b62cec31` |
| `edges/swap_settlement_sequence_c.jsonl.gz` | `6f4fe3b95b44bd336080f380dc7b7eee45bcb7e87e08a3ea89dc64483daa1740` | `964342e49092760c3c21a685da19c4917ee6512a34312b14768f6cb84a4caaeb` |
| `edges/swap_settlement_sequence_d.jsonl.gz` | `04eff241349eb5f1da08d98af84e8172450db92969ae4f56a88458b646a8d0c7` | `41388684858fc7aee5249a2b6f1b8a180bade9af984d02fce259e8f2e2d8b013` |
| `edges/swap_settlement_sequence_e.jsonl.gz` | `efaeb37faac834f3a422b1d12252e9a3fd8c84983a6672214c9b3c21bef094e2` | `d8d590240b6970e75808f0b02ad2d29b9a8d5da13effcf9e7121b89152ea486e` |
| `edges/mm_accrual_grid.jsonl.gz` | `25b2bc16424dd45f1165bec5ba5b6cf668d55b9674ecdb6d11fa251ebc771590` | `d56a5c0f66d79be98a7b863be4c2b0570ae146d4ab6697bbb12c00610db5a113` |

## 3. Leverage venue: `CollRebalancerMath`, `EverlongLeverageHook` (`tests/leverage.rs`)

- `lev_curve_tape_v1.tar.gz` is the unmodified c104 parity tape
  `test/flamm/lev/tape/levcurve_c1_tape_v1.*.json` (16 blocks + manifest). The manifest's sha256 is
  `9df07c65ea45563c0f343ed98290df1fb10fa1a3ea870dbc3c9bf51c26a058f6`; it pins each block's sha256, and
  the test re-verifies them. All 22,465 rows are replayed with the `_expectedOut` / `_assertRow`
  semantics of `test/flamm/lev/CollRebalancerMathLevCurveParity.t.sol` @ `80abd43`; it is the one
  fixture whose rows reach the curve's pro-rata branch (`PRORATA_CROSSING` 3102, `PRORATA_DUST_GUARD`
  194, `PRORATA_NO_CAPPED_PORTION` 185, `LIVENESS_EXCLUSION` 108, `EXACT` 18876).
- Generators `gen/LevRecorder.sol`, `gen/LevGoldenFixture.t.sol` (the local LevBase stack:
  `lev_hook_local_fixture`, the VenueGolden sequence plus displaced and synthetic grids;
  `lev_hook_band_fixture`, `_assertAnchorAndBand` through a harness) and `gen/LevForkFixture.t.sol` (a
  Base fork at block 51317000 against the deployed stack: `lev_hook_fork_fixture`, frame /
  `previewLever` / `pool.previewLever` with the `LeverContext` captured by a calldata-echo probe;
  `lev_curve_fork_fixture`, `frozenParams()` and a `leverageQuote` / `deleverageQuote` / `anchorAndBase`
  / `isStateSafe` grid plus a keccak-seeded sweep on the deployed `CollRebalancerMath`).
- `edges/lev_curve_edges` (`gen/LevEdgeRows.sol`, `gen/LevCurveEdgesHarness.sol`,
  `gen/LevCurveEdges.t.sol`, `gen/lev_curve_math_copy.py`; a Base fork at block 51318000): 77,002 rows
  `[op, inputs, [status, words...]]` on the deployed library over branch thresholds, `MAX_INPUT` and
  `uint256` edges, exact `Mul512` ties, off-chain rounding ties and a keccak-seeded grid; every public
  row re-run on a verbatim internal-visibility copy that also supplies the private-helper rows.

| file | sha256 (stored) | sha256 (uncompressed) |
| --- | --- | --- |
| `lev_curve_tape_v1.tar.gz` | `18fe3e2aa02cce91f8b312f95730b2ef556363e271e82dbbc12d1d107ce29437` | `n/a` |
| `lev_curve_fork_fixture.json.gz` | `798735119ee4e322ec929a75aa48d8855e630f622fc20aa4d3a27a54c30d4e9f` | `74fce4f93e92ee09200739c5edd44cb6ebbf0bc527863466cbf68357ba45ac46` |
| `lev_hook_fork_fixture.json.gz` | `f5d027dc34dbc37289edbf91312d5adf67217bef1a94e02883bee49319580641` | `fb379cb74733dba31578cb9ed487a03375439993d0d78811814801968f3fdd8d` |
| `lev_hook_local_fixture.json.gz` | `6e8b8d178b2f07c24aa0b4b94021b48a44f50455829786f11f07b92dbeabd59f` | `46d8ea7be0ca6207527cd9d2f263ae39d71ff7213c0928d492dacd10618e5a53` |
| `lev_hook_band_fixture.json.gz` | `52e04fdf28c6224faa48e1a4cf581be3d8d070b5a0a53b65c47cf4651d2cb90d` | `e3f59a449cf5ac2e311066e7cb70f29fb57e86ea96056ea4d41b613605378231` |
| `edges/lev_curve_edges.json.gz` | `12ac1f76ce0eb7b9d03d8440a738b23a18abb531b19866321db12eb01ca66cd4` | `383484cf5b395ad8eaf5fee1a356d9d75eac2e4581b4e813335194384dd55337` |

## 4. Pool core end to end: `FLAMMSwapLib`, `FLAMMLeverLib`, `PriceFeed` over the composed state (`tests/core/`)

Generators: `gen/CoreE2EBase.sol`, `gen/CoreE2EGrid.t.sol`, `gen/CoreE2ESeq.t.sol` (Base forks at
blocks 51302915, the parent of the pool's first settled swap, 51313000 and 51324800); `gen/CoreEdgesBase.sol`,
`gen/CoreEdgeGrid.t.sol`, `gen/CoreEdgeSeq.t.sol` (forks at 51302915, 51324800 and 51326000, written
without the first set: own interfaces, own state dump, own scenario logic). JSON lines. Every state row
is one complete state read the way the tracker reads it (view getters plus the three storage words no
view exposes: `FLAMMStore.lastLeverSpreadPpm` at ERC-7201 base + 22 bits 160..191, and the Router
venues' `managedCollateral` / `managedSupplyShares`), together with deployed views the port must
reproduce (`peekCross`, `pegOk`, `loanPosition`, `positions`, `gross`, `totalAssets`).

- `core_e2e_grid_<block>`: per scenario (as deployed, armed at 17500 / 90000, stale spread, notional
  cap, tight band, fee bounds, paused, feature bits, stale feed, sequencer grace / down, peg broken,
  invalid feed, cbBTC moved, IRM grace / quarantine, low pin, low ltv, 30 days accrued) one state and
  `previewSwap` / `previewLever` rows over a log grid, the `uint256` edges and every class boundary the
  grid brackets, bisected to one unit and scanned 600 units on both sides.
- `core_e2e_seq_<block>`: executed sequences (`real_sell` at 51302915, `basic`, `reclaim`, `notional`,
  `lever`, `warp`, `pin_low`, `irm`) with governance moves and warps in between, each step recording the
  op, its arguments, the timestamp, the return words or revert data, the `Swap` / `LeverUp` /
  `LeverDown` event words and the post-state.
- `edges/core_edge_grid_<block>`: per scenario (spread and band edges, spread age, degrade value, ltv
  walks, notional and fee bounds, venue and loan caps, rate ceilings at the live rate, feed and
  sequencer boundaries, Morpho fee, IRM grace edges, oracle revert / zero / band, a whale at ~100%
  utilization, a 25 cbBTC deposit) `previewSwap` / `previewLever` sweeps bisected to adjacent units and
  `router.fundingCeiling` probes.
- `edges/core_edge_seq_<block>`: executed sequences (`heartbeat`, `sequencer`, `spread_degrade`,
  `ltv_walk`, `irm_outage`, `oracle`, `whale`, `lending`, `morpho_fee`, `same_block`, `donation`,
  `price_move`, `pingpong_{a,b,c}`, `settle_hunt`, `loancaps`, `big`, `random_{a,b,c}`) with `pv`
  previews, `x` calls, `warp` moves and `re` re-reads.

The replays carry the port's own post-state forward and compare every return and event word and the
post-state field for field; the sensitivity tests perturb one input at a time and require the replay
to break.

| file | sha256 (stored) | sha256 (uncompressed) |
| --- | --- | --- |
| `core_e2e_grid_51302915.jsonl.gz` | `ae588fcaf6e08b24b5d7e7b3f12c491b89c73f9e155ceb8641090c5a6004ddd5` | `e089ec3efeeafa6668f00bd6a4e6ddc9e49495097c3c81d0fffbf98e382771f7` |
| `core_e2e_grid_51313000.jsonl.gz` | `4e21a865486e59b4fc0eae66290ed509fc1afe88254c558b67cad6b7e62258ee` | `1b31022d0a4b0904fb1bfb74f44c01bc825da0e6cf4377636a98a4b49293f0fd` |
| `core_e2e_grid_51324800.jsonl.gz` | `2d053f484f935e3e715c8f3e8655ef6a32f08c4255a6d8d8d2622f0d303130b1` | `63c8de9c5663499d6ce96929b8f254e66ff294f34b756ee85cf610d6490b2836` |
| `core_e2e_seq_51302915.jsonl.gz` | `9ae2f16137c0a744dd730758ad97537bb8533ce6a0868367d1113ff7c35c1116` | `fcd3eb6165c36c68e73b31fdd57a7aee997a6f8218d79f9a7331c9b30fb3c359` |
| `core_e2e_seq_51313000.jsonl.gz` | `e2d4569b540b7853b49e04507e7ba485439aae3ebb26621c28352b411a92536f` | `44933b2f7b4d1f1be8e8fd0cc214e169d79c58e4cf72c5e2ff671ee90933a07e` |
| `core_e2e_seq_51324800.jsonl.gz` | `6529b264e751e5026344539a039b45bb90542d56b5f501d5ed3f3f5c05f64d45` | `ae062769d95c666eec3ea03b1ed5f87347d1337e5bfffefce53c624278326b51` |
| `edges/core_edge_grid_51302915.jsonl.gz` | `946d384674b004378198eb5fd023d28fea944ebcf6d626c34994561149d6eba2` | `4ce285898ef6a21e4c07ac7f631d2874711f3ee217c7f998b1360eda9184733e` |
| `edges/core_edge_grid_51324800.jsonl.gz` | `8c085a8183d9de9a6edbe7fdab5d08e276141a598739bd4a524baf814fe8f570` | `01c82fd2bda2d25addd8d9faf8abdb819793edbac1703ad62bec510815dca21d` |
| `edges/core_edge_grid_51326000.jsonl.gz` | `463868de0f612bdb55be5cad602a5911b6a9d08b3f2904cb578c42f9edc7d3b4` | `ff05306702daf4f71bd67f73e7e58b28b6d0486bc17f3fe1f091f5e7e81478ac` |
| `edges/core_edge_seq_51302915.jsonl.gz` | `e6e14db48bcfe28a3f87ff072a5cc2ac70e92e4a27d669c39d3dec9338fca99c` | `3d625c2dd43564c71b20f983f3439c30d276464db28c4721afa8ae15ac4cb328` |
| `edges/core_edge_seq_51324800.jsonl.gz` | `53414398f523e1507ed8fdd2c2e4f91642f02dba22a195068af4a5ac948b553d` | `fa2f63a6953ce69f7c861d6fefc28941b845ae815b5f7a49e4bea29657892122` |
| `edges/core_edge_seq_51326000.jsonl.gz` | `1906c0dfc12c37b6f3d7dd5b52ffee245cf1107b6aa5da43ea29dc0773ab341e` | `153ca1d81bce1907c1de451173580eeda623582c115831aa03075260f9ca94c4` |

| generator | sha256 |
| --- | --- |
| `gen/CoreE2EBase.sol` | `e577daaa6e2417722ba30a10511c1493c8a3e954763bf9495a65e2afd24716e7` |
| `gen/CoreE2EGrid.t.sol` | `37415f13b103fbbee629fce208ca290cdfa838a2c9a2783d32a5ea53a57584ee` |
| `gen/CoreE2ESeq.t.sol` | `b9df18f342862aa0823c7686ef73ae95ebf08d9631c1ec28cfcba9c99f851ef3` |
| `gen/CoreEdgesBase.sol` | `5e159ca61ddd51e8c6af9d7e90da40cadd898c9dee83d16abb784c8605d24aa5` |
| `gen/CoreEdgeGrid.t.sol` | `f83a5c6499080e394ba5b06ae08e5f620273718f69470556e4084dde44bdb487` |
| `gen/CoreEdgeSeq.t.sol` | `66558762467d92a309f13acb6834b1c938ce2864cc4549edb9e07338aac02363` |

## 5. The deployed pool's component snapshots and previews (`tests/protocol_sim.rs`)

Read from Base over public JSON-RPC (`https://mainnet.base.org`; `eth_getStorageAt`, `eth_getLogs`,
`eth_getCode`, `eth_call`, `eth_getBlockByNumber`; no `debug_*`), not generated by Foundry. Each
`snapshots/<block>.json.gz` is one JSON document:

- `block`: number, hash, timestamp.
- `components`: the two `ComponentWithState`s of the pool at that block, swap venue first (id
  `0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572`) then lever-up (`…7572000000000000000000000001`),
  each with the static attributes the `base-flamm` substreams sets at creation and the 151 dynamic
  attributes it maintains: the 87 raw storage words of the FLAMM-owned contracts (`pool:` /
  `hook:` / `spread:` / `router:` / `account:` / `pricefeed:` / `factory:` `<slot key>`, the value
  `eth_getStorageAt` returns), the 6 Morpho Blue and `AdaptiveCurveIrm` words (`mm:0:*`,
  `irm:0:rate_at_target`), and the four Chainlink feeds decoded from the proxies' and aggregators'
  storage (`feed:<f>:*`, the `DualAggregator`'s 21-round ring as `feed:mo0:tx:<round>`), plus the
  `feed:<f>:kind` of each aggregator. The 147 schema attributes were produced by the schema
  snapshot tool of the native-integration design and every one verified against the contracts'
  own views at the same block (the substreams' `testdata/snapshot_51302915.json` is the same
  document); the `kind` attributes are the substreams' addition, added here by the recorder.
- `grids`: `eth_call` answers at the block (`block.timestamp` is the snapshot's): `swap_sell` /
  `swap_buy` are `FLAMM.previewSwap(poolAssetIn, amountIn)` rows over a log-spaced grid (1, 2, 3,
  5, 7 × 10^k sats up to 7 cbBTC; the same mantissas up to 7,000,000 USDC) plus every probe of the
  edge search, each row the three return words (`amountInUsed`, `amountOut`, `feeWad`) or the
  revert data; `sell_edge` / `buy_edge` are the largest sizes that fill in full (`amountInUsed ==
  amountIn`), located by doubling from one unit to the first size that fills, doubling on to the
  first that does not, then bisection — the procedure `FlammPoolState::get_limits` runs on the
  port; `lever_up` / `lever_down` are `FLAMM.previewLever(up, amountIn)` rows (`LevPaused()` at
  the three pinned snapshot blocks: leverage was paused until 51433699); `spot` is
  `EverlongHook.spot(PoolContext)` (the argument is ignored on chain).
- `decoded_feeds`: the feeds' storage decoded for the reader (not read by the tests).

Blocks: 51302915 (the parent of the pool's first settled swap, timestamp 1789395177), 51313000
(1789415347) and 51409000 (1789607347). `snapshots/swap_51302916_delta.json.gz` is block 51302916,
the swap itself (tx `0x46c3cd72a5860b2fe546e5a2130e066314e3777027151661e1e4f19a935901fa`, 15000
sats → 11301759 USDC): the header and the 12 tracked words the block changed (`eth_getStorageAt`
at 51302915 vs 51302916), named as the substreams names the attributes, copied from
`protocols/substreams/base-flamm/testdata/swap_51302916.json`. The block also carried another
Morpho user's withdrawal from the market (its supply totals end 90,140,614 assets below what the
pool's own transaction leaves), which the test accounts for.

Recorder: `gen/snapshot.py` (the snapshot from storage, logs and code), `gen/grids.py` (the calls
above) and `gen/package.py` (`gzip`, mtime 0, of the recorder's `json.dump(indent=1,
sort_keys=True)`), `python3 snapshot.py 51302915 51313000 51409000 && python3 grids.py 51302915
51313000 51409000 && python3 package.py 51302915 51313000 51409000` in `gen/` (its README lists
every script and its digest). The tests decode each of the three snapshots through
`TryFromWithBlock`, replay every grid row (preview words and revert classes on the port; on the
`ProtocolSim` a quote for a full fill, a typed refusal for a clipped size or a revert above the
limit, and the empty trade for a revert at or below it, a buy's dust), locate the limits and check
the contract below them (the 0.1% / 1% / 10% sizes the protocol test harness quotes fill in full;
every sell fills from one sat; the buys the chain refused below the limit are dust of at most 5000
USDC units, not an interval, below a hundredth of a percent of the limit, quoted as nothing),
check `spot_price` in the trait's definition against the hook's recorded `spot` and the margin of
a small quote in the buying direction, run `query_pool_swap` in both directions (a limit between
the max-size and the zero-size execution price is met inside the limit; one above the zero-size
execution price is the zero swap, the search crossing the buy's dust without a revert), require
the words the pinned code writes at construction (each deletion is a typed refusal, at snapshot
and through a delta), reproduce the real swap from the parent snapshot
at its execution timestamp and compare the quote's post-state with the delta-applied one field
for field, and prove `apply_block`'s rule: quiet on a repeated block and, for the unpositioned
pool of 51302915, between deadlines; a change across the feed heartbeat, the oracle reveal and a
binding rate ceiling, and on every advance of the positioned pool of 51313000.

| file | sha256 (stored) | sha256 (uncompressed) |
| --- | --- | --- |
| `snapshots/51302915.json.gz` | `04b6aaf95785fcbe31b8dd2d90826c45253142a72a0bc26b65becb9e5f295cd5` | `a0ee1e54df13ad9a6cb52a7d3d856e68eaff97f42b1f1e71eca83362b6a1d71d` |
| `snapshots/51313000.json.gz` | `e001dea4effe6372872d7896e2b0cf522af8cf67f990d29c43e23514f757a970` | `52922dd720d550e00c0886bfa27a152d1938651801b1b86b147c58e9c7bc51bf` |
| `snapshots/51409000.json.gz` | `2e3d7e26a6a0385cc17ff2bf2d56ecb62777f2ff634fa72825a1c3fbb2a7cbd5` | `77a3c25fe8a30fdde55e49772fce35c84590fd00e100462e3849182d2d94efbe` |
| `snapshots/swap_51302916_delta.json.gz` | `f667cbfa9e24a8e48015b30e3969c767dabde9968972d54e6c45789d3ba91c98` | `4199d0dd102e4aaa19a889c3e5c43c5eb617217ec8acad598e2ef205549441e0` |

## 6. The substreams' stream, end to end (`tests/e2e.rs`)

Two fixtures replay the `base-flamm` substreams' own output through the `ProtocolSim`, the local
equivalent of the `protocols/testing` range harness (which needs a StreamingFast endpoint and a
Postgres; see the package README, "End to end, without the hosted harness").

- `snapshots/e2e_stream.json.gz` is written by the package's own test
  (`protocols/substreams/base-flamm/src/e2e_tests.rs`, `e2e_stream_fixture_is_the_package_output`,
  `FLAMM_E2E_WRITE=<path>`) from `protocols/substreams/base-flamm/testdata/e2e_blocks.json.gz`,
  real Base blocks assembled from `eth_getStorageAt` / `eth_getTransactionReceipt` / `eth_getCode`
  and fed to every module core in the manifest's order; that test also asserts the committed file
  equals what the package emits. One JSON document: `blocks`, in order, each with `number`,
  `timestamp`, `hash`, `parent_hash`, `new_components` (the components created in the block with
  their snapshot as the RPC would serve it: the component with its static attributes, tokens and
  creation transaction; the attribute rows after the block; the balances) and `deltas` (per
  component that existed before the block: `updated_attributes`, `deleted_attributes`,
  `balances`, merged over the block's transactions as `ProtocolComponentStateDelta::merge`
  does). Blocks: the creator's deployments 51154978-51154988 (no component yet, empty), the
  creation 51154990, the first stop block of the range test 51155010, the activation 51298416,
  every swap the pool has settled (51302916, 51343234, 51347390, 51420672, 51420867, 51430828),
  two of its six deposits (51300667, 51426394), one of its four withdrawals (51348093), one of
  the keeper's recenters (51384803), 51302915, the second stop block 51302920, three recent
  blocks each carrying a Chainlink round (51429815 USDC/USD, 51433135 cbBTC/USD, 51433218 the
  Morpho oracle's `DualAggregator`) and the block that unpaused leverage (51433699,
  `LevPauseSet(false)`); between two blocks of interest the net change of every tracked word is
  one synthetic block at the parent of the next one, which is how the pool's other deposits
  (51343943, 51347236, 51347413, 51353593), withdrawals (51344221, 51347256, 51353599) and keeper
  recenters and pokes reach the fold (their net effect, not their own per-block deltas).
- `snapshots/e2e_grids.json.gz` is the chain's answers: `grids` per pinned block (51155010,
  51298416, 51302916, 51302920, 51348093, 51384803, 51429815, 51433135, 51433218, 51433699), the
  same recorder and layout as section 5 (`previewSwap` over the log-spaced grid and every probe
  of the edge search, `sell_edge` / `buy_edge`, `previewLever`, `spot`; at 51155010 only a few
  sizes, the pool being paused), and `swaps` / `deposits`, the pool's settled fills decoded from
  their receipts (`Swap(sender, to, poolAssetIn, amountInUsed, amountOut, feeOut, feeWad,
  spotAfterWad)`; `amount_in` from the calldata when the swap went through
  `FLAMMSwapAdapter.swap`).

The test replays the stream as `TychoStreamDecoder::decode` would (a snapshot decoded at the
creation block's header, a `delta_transition` per later block with the decoder's `block_number` /
`block_timestamp`, every state advanced to the next block's clock after each confirmed block),
checks both components decode at every block and stay the same pool, compares every grid row at
every pinned block at that block's own clock (preview words and revert classes on the port; on the
`ProtocolSim` a quote, a typed refusal above the limit or the empty trade for a buy's dust below
it, the limits at the recorded edges, the hook's spot, the lever
venue's refusals: `LevPaused` while leverage is paused, from the creation through 51433698, and
after the unpause at 51433699 `SpreadUnavailable` for every lever-up, the keeper never having
re-posted a spread to the `LeverageSpreadHook`, so the constructor's spread aged past
`maxSpreadAge` an hour after the creation, and `NothingToFill` / `PriceBand` for the lever-downs;
a live spread would make the lever-up venue quotable, a state no fixture covers), quotes every
settled swap from the parent block's state at the swap block's clock (the receipt's `amountOut`
to the wei, the receipt's `feeWad`, the post-state's pool,
hook, Router legs, position, borrow totals and IRM rate equal to the block's own diff) and runs the
harness's step at both stop blocks (limits, spot, 0.1% / 1% / 10% quotes at the block after the
stop block: nothing tradable at 51155010, both directions at 51302920).

Generator: `gen/fetch.py` (the stages), `gen/morpho_events.py` (the other transactions' Morpho
events) and `gen/pack.py` (the two fixtures, `gzip` mtime 0 of `json.dumps(sort_keys=True,
separators=(",", ":"))`), over `https://mainnet.base.org` (the withdrawal, recenter and unpause
stages were added on 2026-09-17, the stages after an insertion re-fetched so their catch-up diffs
start at the new stage); the stream fixture by the package test above (`gen/regen.sh` runs both).
`gen/README.md` lists every script and its digest.

| file | sha256 (stored) | sha256 (uncompressed) |
| --- | --- | --- |
| `snapshots/e2e_stream.json.gz` | `06b482655e2b24d600494d617999cf84c50ec2682e4661d7ee97ea8bfe2f43e0` | `31e9b1e97e845cf67e6667285cc101b09842f26caba7e6269f52b377852a1374` |
| `snapshots/e2e_grids.json.gz` | `e6636e338627c8569d606b29da6a3b901c1b104057a5b790e87b66aa6c61d0c3` | `66e953f3496a92bba5cd2d2ed39dc6287fbb7c6104e7e57481419789d52739d1` |

Coverage: 42 stream blocks (the 27 stage blocks, 23 with a transaction of interest and 4 catch-up
stops, and the 15 synthetic catch-up blocks before stages), 143 attribute rows in the creation
snapshot, 2142 `previewSwap` rows at the eleven pinned blocks (1194 full fills, 0 clipped, 948
reverts, the pool refusing above its largest full fill rather than clipping at these states), 20
edges (both directions at the ten unpaused pinned blocks), 83 `previewLever` rows (`LevPaused`
at the first ten, `SpreadUnavailable` / `NothingToFill` / `PriceBand` at 51433699), 6 settled
swaps and 2 deposits. The other transactions' Morpho events at the swap blocks: a withdrawal at 51302916
(90,140,614), a supply at 51343234, a borrow and a supply at 51347390, two supplies at 51420672,
none at 51420867 and 51430828.
