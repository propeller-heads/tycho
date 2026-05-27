## [0.302.2](https://github.com/propeller-heads/tycho-indexer/compare/0.302.1...0.302.2) (2026-05-27)


### Bug Fixes

* **tycho-execution:** add quickswap_v2 to swap encoder registry ([a67d15a](https://github.com/propeller-heads/tycho-indexer/commit/a67d15a504e30844eefae4c1043c4ad1e5cf6dad))
* **tycho-execution:** add quickswap_v2 to swap encoder registry ([#1047](https://github.com/propeller-heads/tycho-indexer/issues/1047)) ([d69caf2](https://github.com/propeller-heads/tycho-indexer/commit/d69caf2eaa7a3ae0050a5c8892f4220bffb36f7d))

## [0.302.1](https://github.com/propeller-heads/tycho-indexer/compare/0.302.0...0.302.1) (2026-05-26)


### Bug Fixes

* **gas-estimations:** Add univ2 clones to PROTOCOLS_OPTIMIZABLE_TRANSFER_IN ([29852db](https://github.com/propeller-heads/tycho-indexer/commit/29852db2f79a34758468b579db65a6e228a7d145))
* **gas-estimations:** Tweak gas estimations for uniswap v2 ([40e1742](https://github.com/propeller-heads/tycho-indexer/commit/40e174214661a5471b69a021ed42c530d9941035))
* **gas-estimations:** Tweak gas estimations for uniswap v4 ([2a84ee1](https://github.com/propeller-heads/tycho-indexer/commit/2a84ee170ce3541e441b72da7de3b2294b7e36e3))
* **gas-estimations:** Tweak gas estimations for univ4 and univ2 ([#1050](https://github.com/propeller-heads/tycho-indexer/issues/1050)) ([1c33714](https://github.com/propeller-heads/tycho-indexer/commit/1c3371437920ed597699590810d8fc9450bf68b1))
* **tycho-test:** Use proper eth marker when encoding router call ([395e16e](https://github.com/propeller-heads/tycho-indexer/commit/395e16e824e7813dcb9d10d5f2b1361f07ec7d66))

## [0.302.0](https://github.com/propeller-heads/tycho-indexer/compare/0.301.1...0.302.0) (2026-05-26)


### Features

* **encoding:** add client fee signature offset to EncodedSolution ([24e7a38](https://github.com/propeller-heads/tycho-indexer/commit/24e7a387cee281486518d49b6faa1fe4cbde7291))
* **encoding:** add client fee signature offset to EncodedSolution ([#1046](https://github.com/propeller-heads/tycho-indexer/issues/1046)) ([89e2e47](https://github.com/propeller-heads/tycho-indexer/commit/89e2e470a01664a00fe41b0fba0f359b83c0e926))

## [0.301.1](https://github.com/propeller-heads/tycho-indexer/compare/0.301.0...0.301.1) (2026-05-26)


### Bug Fixes

* **gas-estimations:** Add SWAP_BASE_GAS constant in slipstreams ([9ac79cc](https://github.com/propeller-heads/tycho-indexer/commit/9ac79ccf5727069028770d84ef7471565634789b))
* **gas-estimations:** Tweak gas estimations for slipstreams ([12bd6b5](https://github.com/propeller-heads/tycho-indexer/commit/12bd6b534ae9e478319caa6fb7a88f6525b46883))
* **gas-estimations:** Tweak gas estimations for slipstreams and univ3 ([#1036](https://github.com/propeller-heads/tycho-indexer/issues/1036)) ([59d959a](https://github.com/propeller-heads/tycho-indexer/commit/59d959a1f4eeb8fbeb67a8a6c53716a155ec9f37))
* **gas-estimations:** Tweak gas estimations for uniswap v3 ([5c5c56e](https://github.com/propeller-heads/tycho-indexer/commit/5c5c56e757887de0a81e659c4bf47c7854bb1ca5))

## [0.301.0](https://github.com/propeller-heads/tycho-indexer/compare/0.300.5...0.301.0) (2026-05-25)


### Features

* **tycho-client:** move runtime snapshot fetches off the delta hot path ([92abe66](https://github.com/propeller-heads/tycho-indexer/commit/92abe66736513ed4c830d5e43a7c2e14af0ca6a9))
* **tycho-client:** move runtime snapshot fetches off the delta hot path ([#1031](https://github.com/propeller-heads/tycho-indexer/issues/1031)) ([47f9449](https://github.com/propeller-heads/tycho-indexer/commit/47f9449b89b5a14a088f4f91cc99e60292fceec0))

## [0.300.5](https://github.com/propeller-heads/tycho-indexer/compare/0.300.4...0.300.5) (2026-05-22)


### Bug Fixes

* remove unused tycho-test dev dependency from simulation ([496a3e5](https://github.com/propeller-heads/tycho-indexer/commit/496a3e5a93e81656a4dd2da1fffc7fe713cda92f))
* remove unused tycho-test dev dependency from simulation ([#1043](https://github.com/propeller-heads/tycho-indexer/issues/1043)) ([5c44fd4](https://github.com/propeller-heads/tycho-indexer/commit/5c44fd4db7b84f1e27743e6423cd103584af900e))

## [0.300.4](https://github.com/propeller-heads/tycho-indexer/compare/0.300.3...0.300.4) (2026-05-22)


### Bug Fixes

* **simulation:** add 0x276084 to Fluid V1 paused pools filter ([77cae93](https://github.com/propeller-heads/tycho-indexer/commit/77cae93d89a941ac7886173b126fe1ebc8f5bc55))
* **simulation:** add 0x276084 to Fluid V1 paused pools filter ([#1042](https://github.com/propeller-heads/tycho-indexer/issues/1042)) ([f7b1bf5](https://github.com/propeller-heads/tycho-indexer/commit/f7b1bf55c5143f4c4dafffad4ef632d79f67958d))

## [0.300.3](https://github.com/propeller-heads/tycho-indexer/compare/0.300.2...0.300.3) (2026-05-22)


### Bug Fixes

* **ci:** Publish tycho-execution before tycho-simulation ([da5691e](https://github.com/propeller-heads/tycho-indexer/commit/da5691ec35134f69014412f80c9e55ecf3871950))
* **ci:** Publish tycho-execution before tycho-simulation ([#1040](https://github.com/propeller-heads/tycho-indexer/issues/1040)) ([4dd32ca](https://github.com/propeller-heads/tycho-indexer/commit/4dd32ca749deffe4a8fc41f595465549f707448e))

## [0.300.2](https://github.com/propeller-heads/tycho-indexer/compare/0.300.1...0.300.2) (2026-05-21)


### Bug Fixes

* Add missing readmes to simulation and execution ([92fa022](https://github.com/propeller-heads/tycho-indexer/commit/92fa02250a92601d5f5e94d9ea1af25797c8cd63))
* Add missing readmes to simulation and execution ([#1039](https://github.com/propeller-heads/tycho-indexer/issues/1039)) ([6d7748c](https://github.com/propeller-heads/tycho-indexer/commit/6d7748c6e3be69427c00097ef8b3c9343e5f927a))

## [0.300.1](https://github.com/propeller-heads/tycho-indexer/compare/0.300.0...0.300.1) (2026-05-21)


### Bug Fixes

* Don't push to crates tycho-storage and tycho ([22da463](https://github.com/propeller-heads/tycho-indexer/commit/22da46339e8655ead4f847b05e36a656a8580961))
* Don't push to crates tycho-storage and tycho ([#1038](https://github.com/propeller-heads/tycho-indexer/issues/1038)) ([7fc4407](https://github.com/propeller-heads/tycho-indexer/commit/7fc440738575bcde06929dd44a9523cdbeea12b5))

## [0.300.0](https://github.com/propeller-heads/tycho-indexer/compare/0.299.0...0.300.0) (2026-05-21)


### Features

* Remove dry-run from release.yaml and push to crates! ([5014973](https://github.com/propeller-heads/tycho-indexer/commit/5014973d3d2e192ea0d5fa5902ebf2632d0892c1))
* Remove dry-run from release.yaml and push to crates! ([#1037](https://github.com/propeller-heads/tycho-indexer/issues/1037)) ([5636f18](https://github.com/propeller-heads/tycho-indexer/commit/5636f18cb2dbf35e70fc69da0c600a05dda457e7))

## [0.299.0](https://github.com/propeller-heads/tycho-indexer/compare/0.298.0...0.299.0) (2026-05-21)


### Features

* add arbitrum executors to deployment script ([b7f82f1](https://github.com/propeller-heads/tycho-indexer/commit/b7f82f1efc385116afc6804107f1b3cde98db15a))
* Base deployment ([246b7cb](https://github.com/propeller-heads/tycho-indexer/commit/246b7cb191dc08faca807212437689efb5b6d6e8))
* bsc executor deployment params ([88aee49](https://github.com/propeller-heads/tycho-indexer/commit/88aee494402c480e27ab31445301e418d065be0a))
* polygon deployment ([8f08f95](https://github.com/propeller-heads/tycho-indexer/commit/8f08f95a0a3f826388966aa21c8bfb7e07e966d1))
* Router V3 Deployment ([#1020](https://github.com/propeller-heads/tycho-indexer/issues/1020)) ([924c511](https://github.com/propeller-heads/tycho-indexer/commit/924c5111de0a6caf6939672f87f04e515286b347))
* **router-v3:** Deploy on Arbitrum ([149610d](https://github.com/propeller-heads/tycho-indexer/commit/149610d44914a3413471f0fc6d0564c670b52040))
* **router-v3:** Deploy on BSC ([15eb8f8](https://github.com/propeller-heads/tycho-indexer/commit/15eb8f8c5e0b69a97ae7d18a18177f4f5df98d34))
* **router-v3:** Deploy on ethereum ([f6d5879](https://github.com/propeller-heads/tycho-indexer/commit/f6d58797c2dd267d83d87212f549a39b4adc7f92))
* **router-v3:** Deploy on Unichain ([e2681a7](https://github.com/propeller-heads/tycho-indexer/commit/e2681a70efa5407e8f77cf07379412a00b1273de))


### Bug Fixes

* **integration-test:** add infinite retries for state synchronizer ([5461461](https://github.com/propeller-heads/tycho-indexer/commit/546146134dba0609f35432ee5b761e9bb7d833ee))
* **integration-test:** add infinite retries for state synchronizer ([#1027](https://github.com/propeller-heads/tycho-indexer/issues/1027)) ([a3ac0ae](https://github.com/propeller-heads/tycho-indexer/commit/a3ac0aecf915022270af7b64e9de2967d421e4ae))
* **integration-test:** increase startup timeout to 1000s for slow extractors ([cb3c1b0](https://github.com/propeller-heads/tycho-indexer/commit/cb3c1b04c46b1f1d733e462c0436308f83801ad0))
* **quickstart:** support router v3 ([81de012](https://github.com/propeller-heads/tycho-indexer/commit/81de01241d79cbfb772093263b34e5ce20ccbb21))

## [0.298.0](https://github.com/propeller-heads/tycho-indexer/compare/0.297.2...0.298.0) (2026-05-21)


### Features

* add PendingBlockProcessor and TxDeltaIndexer infrastructure ([7bd0904](https://github.com/propeller-heads/tycho-indexer/commit/7bd090485baded4f638a8a004ae6aeb516dd0def))
* add PendingBlockProcessor and TxDeltaIndexer infrastructure ([#1029](https://github.com/propeller-heads/tycho-indexer/issues/1029)) ([0734b72](https://github.com/propeller-heads/tycho-indexer/commit/0734b7238056cf715e9fadaf6eab835044b6613d))
* Add Polygon and Arbitrum defaults to integration test ([3d05b68](https://github.com/propeller-heads/tycho-indexer/commit/3d05b6885c4344448b0f8103495f5815214e5ce6))
* Add Polygon and Arbitrum defaults to integration test ([#1035](https://github.com/propeller-heads/tycho-indexer/issues/1035)) ([1ac5df7](https://github.com/propeller-heads/tycho-indexer/commit/1ac5df732995b282d854bd42e853bd491e0923b2))


### Bug Fixes

* address review comments on PendingBlockProcessor ([c09ba67](https://github.com/propeller-heads/tycho-indexer/commit/c09ba67e7fedd60e63865f2d32ddf17b15a8cdb2))

## [0.297.2](https://github.com/propeller-heads/tycho-indexer/compare/0.297.1...0.297.2) (2026-05-21)


### Bug Fixes

* handle codeless Creation deltas and missing contract_code in snapshots ([#1032](https://github.com/propeller-heads/tycho-indexer/issues/1032)) ([adce46b](https://github.com/propeller-heads/tycho-indexer/commit/adce46b88081ce42b0d57f69250a8a854aa662ca))
* **indexer:** set code=Some for Creation deltas with empty bytecode ([a4fafcd](https://github.com/propeller-heads/tycho-indexer/commit/a4fafcd72706d0a6757dd13de59019cc0d29a36d))
* **simulation:** keep change=Creation when patching codeless Creation deltas ([bdd2bef](https://github.com/propeller-heads/tycho-indexer/commit/bdd2bef58313634517aac7b63da4433b92cc9fff))
* **storage:** skip accounts missing contract_code in snapshot instead of erroring ([a3b65f7](https://github.com/propeller-heads/tycho-indexer/commit/a3b65f74fd34dea4fcb4bf28facabaf281b8a115))

## [0.297.1](https://github.com/propeller-heads/tycho-indexer/compare/0.297.0...0.297.1) (2026-05-21)


### Bug Fixes

* **gas-estimation:** Add logic for PROTOCOLS_OPTIMIZABLE_TRANSFER_IN ([d99d891](https://github.com/propeller-heads/tycho-indexer/commit/d99d891c9d8423c1e6664f6341eb2519713cb9fe))
* **gas-estimation:** Add logic for PROTOCOLS_OPTIMIZABLE_TRANSFER_IN ([#1033](https://github.com/propeller-heads/tycho-indexer/issues/1033)) ([0554c73](https://github.com/propeller-heads/tycho-indexer/commit/0554c73f384dc5151ba47539f26aba46b120ff75))
* **gas-estimations:** Fix gas calculation for split swaps ([700450c](https://github.com/propeller-heads/tycho-indexer/commit/700450c395eaad61539c9fdbca87f9c133d359a5))

## [0.297.0](https://github.com/propeller-heads/tycho-indexer/compare/0.296.4...0.297.0) (2026-05-19)


### Features

* **simulation:** add is_partial flag to Update and propagate through decoder ([a22e6f1](https://github.com/propeller-heads/tycho-indexer/commit/a22e6f12f797473a30f2838597ab319904ebe19f))
* **simulation:** add partial blocks flag to Update ([#1021](https://github.com/propeller-heads/tycho-indexer/issues/1021)) ([ad86e76](https://github.com/propeller-heads/tycho-indexer/commit/ad86e76699e0119f9141dbb513da26657aff76d9))


### Bug Fixes

* **gas-estimation:** Make estimate_gas_usage public ([aff6ac6](https://github.com/propeller-heads/tycho-indexer/commit/aff6ac66ba66b06b7ec058e6d4cef5009f5c96ce))
* **gas-estimation:** Make estimate_gas_usage public ([#1028](https://github.com/propeller-heads/tycho-indexer/issues/1028)) ([cb6fe43](https://github.com/propeller-heads/tycho-indexer/commit/cb6fe439fcbd88f03c5048bc6ec9ce8778b73cce))
* **integration-test:** add --disable-execution flag to skip Tenderly swap execution ([24cd3ea](https://github.com/propeller-heads/tycho-indexer/commit/24cd3ea89cd64c911fb3aa2d184699a0746ad505))
* **integration-test:** use chain-specific TVL defaults ([c2f8012](https://github.com/propeller-heads/tycho-indexer/commit/c2f8012d71b01e60015e3cacb04f35533c5720ce))

## [0.296.4](https://github.com/propeller-heads/tycho-indexer/compare/0.296.3...0.296.4) (2026-05-19)


### Bug Fixes

* **univ4:** Address review comments from [#1009](https://github.com/propeller-heads/tycho-indexer/issues/1009) ([d0b627a](https://github.com/propeller-heads/tycho-indexer/commit/d0b627a56132b7df7bf199a1f1841cdf5bd79bfb))
* **univ4:** Address review comments from [#1009](https://github.com/propeller-heads/tycho-indexer/issues/1009) ([#1022](https://github.com/propeller-heads/tycho-indexer/issues/1022)) ([fe6a4f5](https://github.com/propeller-heads/tycho-indexer/commit/fe6a4f5f957db3a61f2a871026bf54095aeda897))

## [0.296.3](https://github.com/propeller-heads/tycho-indexer/compare/0.296.2...0.296.3) (2026-05-18)


### Bug Fixes

* **client:** initialization failure didn't name the failing extractor ([c68f592](https://github.com/propeller-heads/tycho-indexer/commit/c68f592b92a99ddaa0630014e9d97444ae7fc95c))
* **integration-test:** ws retry cooldown exceeded state-sync cooldown ([fe01c0f](https://github.com/propeller-heads/tycho-indexer/commit/fe01c0f9e41d12084ad91da2e78b12054eb1fffc))
* preserve UnknownExtractor through paginated fetch + ws retry cooldown ([#1025](https://github.com/propeller-heads/tycho-indexer/issues/1025)) ([9406bb7](https://github.com/propeller-heads/tycho-indexer/commit/9406bb7db935763bde8ae62aa08d7a2ea5f0e1d9))
* **rpc:** unknown extractor swallowed by Fatal during paginated fetch ([9b5c600](https://github.com/propeller-heads/tycho-indexer/commit/9b5c6002a915dc18dfc8df17ce659fbf5cc790d2))

## [0.296.2](https://github.com/propeller-heads/tycho-indexer/compare/0.296.1...0.296.2) (2026-05-18)


### Bug Fixes

* **rpc:** retry null result DeserError and log RPC failures ([74823d6](https://github.com/propeller-heads/tycho-indexer/commit/74823d693de92b677889942ce662e3b356891767))
* **rpc:** retry null result DeserError and log RPC failures ([#1024](https://github.com/propeller-heads/tycho-indexer/issues/1024)) ([b5a2ce2](https://github.com/propeller-heads/tycho-indexer/commit/b5a2ce26e07443d4a25bfd24792cc36e27a90cf8))

## [0.296.1](https://github.com/propeller-heads/tycho-indexer/compare/0.296.0...0.296.1) (2026-05-18)


### Bug Fixes

* **integration-test:** Handle router fee when computing slippage ([e410366](https://github.com/propeller-heads/tycho-indexer/commit/e4103665ca49e0175c3fbae72cca34b8b04b73c2))
* **integration-test:** Handle router fee when computing slippage ([#1023](https://github.com/propeller-heads/tycho-indexer/issues/1023)) ([0fb1b2a](https://github.com/propeller-heads/tycho-indexer/commit/0fb1b2ab5bfb433aae13ec488d462723c9320996))

## [0.296.0](https://github.com/propeller-heads/tycho-indexer/compare/0.295.0...0.296.0) (2026-05-18)


### Features

* **tycho-client:** add Page<T> wrapper and params structs for paginated RPCClient methods ([8e28e9c](https://github.com/propeller-heads/tycho-indexer/commit/8e28e9c3cab25ab76dcb853eeca4388e6188a452))
* **tycho-client:** add serializable dto wrappers for feed pipeline types ([c3e9bca](https://github.com/propeller-heads/tycho-indexer/commit/c3e9bca1f3748a61dcbca87da1aad834166e2ce7))
* **tycho-client:** replace dto types with model types in public API ([#999](https://github.com/propeller-heads/tycho-indexer/issues/999)) ([2d37a17](https://github.com/propeller-heads/tycho-indexer/commit/2d37a17bb30948c0ea7fcab80c057589569d6862))


### Bug Fixes

* address post-review CI failures ([4b0b769](https://github.com/propeller-heads/tycho-indexer/commit/4b0b769723060ed5a55dac2fd38ea9cbd51da5a0))
* resolve ws.rs import collision and apply nightly fmt ([2fed1e3](https://github.com/propeller-heads/tycho-indexer/commit/2fed1e3d1da44ab0375189cd49b95e397ad7f867))
* **tycho-simulation:** move impl blocks before test module ([0cd39ed](https://github.com/propeller-heads/tycho-indexer/commit/0cd39ed85833a8f6fbd2e9e8207ee49659a7f7e0))
* **tycho-simulation:** restore uniswap v3/v4 tests using ComponentWithStateDto ([cd94e85](https://github.com/propeller-heads/tycho-indexer/commit/cd94e8545a72e3be2ddc7163c7b0b2d7bb0ab6fc))
* use infallible into() and fix ws test DummyDelta type ([8becda7](https://github.com/propeller-heads/tycho-indexer/commit/8becda7a55563408846fe67d282f2f04943ae3ea))

## [0.295.0](https://github.com/propeller-heads/tycho-indexer/compare/0.294.0...0.295.0) (2026-05-18)


### Features

* add SwapEncoderRegistry::new_with_defaults and update docs ([edfffa8](https://github.com/propeller-heads/tycho-indexer/commit/edfffa8f1730193fe4f6073e09e86c525f7700cc))
* **encoding:** add ClientFeeParams struct with into_abi_params conversion ([e02e368](https://github.com/propeller-heads/tycho-indexer/commit/e02e3685918f7d5e74bcd24bca174fab34388b4f))
* **tycho-executor:** extend interface for convenience ([#1001](https://github.com/propeller-heads/tycho-indexer/issues/1001)) ([5b562d1](https://github.com/propeller-heads/tycho-indexer/commit/5b562d1e0895fcfa02778ca70add8c83c8e65a75))

## [0.294.0](https://github.com/propeller-heads/tycho-indexer/compare/0.293.0...0.294.0) (2026-05-18)


### Features

* **gas-estimations:** Add router overhead gas ([e1e7320](https://github.com/propeller-heads/tycho-indexer/commit/e1e7320c40463bb7eca2d1e400e6b8e5d0fa79d8))
* **gas-estimations:** Add router overhead gas ([#1019](https://github.com/propeller-heads/tycho-indexer/issues/1019)) ([c8f7a7c](https://github.com/propeller-heads/tycho-indexer/commit/c8f7a7c1f900e6c956c04141ad8269795f4f8e1d))

## [0.293.0](https://github.com/propeller-heads/tycho-indexer/compare/0.292.0...0.293.0) (2026-05-15)


### Features

* (WIP) enable sequential unlocked USV4 swaps ([079ebf2](https://github.com/propeller-heads/tycho-indexer/commit/079ebf2a1674a52143bd52203b4f074290a2ba1b))
* **UniV4 execution:** Allow swap when already unlocked ([5add52a](https://github.com/propeller-heads/tycho-indexer/commit/5add52ae49ced60a3d19183f5cc74e386a7cdfbb))
* **UniV4 execution:** Allow swap when already unlocked ([#1009](https://github.com/propeller-heads/tycho-indexer/issues/1009)) ([339d1f4](https://github.com/propeller-heads/tycho-indexer/commit/339d1f42df1ae3857116e46b3784d0e277eebde8))


### Bug Fixes

* **UniV4:** Decode user_data as typed JSON struct ([be0ab7e](https://github.com/propeller-heads/tycho-indexer/commit/be0ab7e8bb528fea663e27e04d2120f6e112f7e7))
* **univ4:** Fixes after merge with main ([47bf018](https://github.com/propeller-heads/tycho-indexer/commit/47bf018054b7eb26fe423a2ccb106c6640cfd018))

## [0.292.0](https://github.com/propeller-heads/tycho-indexer/compare/0.291.4...0.292.0) (2026-05-15)


### Features

* add BSC default URL configuration in get_default_url function ([87b09cb](https://github.com/propeller-heads/tycho-indexer/commit/87b09cb34b984e86d72ad0e7d8742d39410a0e9e))
* add BSC stream config for examples and integration tests ([e9e7a3c](https://github.com/propeller-heads/tycho-indexer/commit/e9e7a3ce2264720758b6dc34b3a18f7fd9a68bff))
* add BSC stream config for examples and integration tests ([#1016](https://github.com/propeller-heads/tycho-indexer/issues/1016)) ([95b9ef2](https://github.com/propeller-heads/tycho-indexer/commit/95b9ef2bdae4a78085a950be16fdd36585421160))

## [0.291.4](https://github.com/propeller-heads/tycho-indexer/compare/0.291.3...0.291.4) (2026-05-15)


### Bug Fixes

* **integration-test:** improve integration test stability and protocol state metrics ([#1017](https://github.com/propeller-heads/tycho-indexer/issues/1017)) ([fbe58b1](https://github.com/propeller-heads/tycho-indexer/commit/fbe58b12036d4e1003adfb4956c86144fa40572a))
* **integration:** emit sync state 7 (Skipped) when RPC block is ahead of update block ([40fe540](https://github.com/propeller-heads/tycho-indexer/commit/40fe540ae81ef8f1101889dbf275bbd4805594ff))
* **integration:** set infinite WebSocket reconnect retries on protocol stream ([8d8d5b9](https://github.com/propeller-heads/tycho-indexer/commit/8d8d5b96413beaaa90020325291fbf1aed15b70a))

## [0.291.3](https://github.com/propeller-heads/tycho-indexer/compare/0.291.2...0.291.3) (2026-05-14)


### Bug Fixes

* **router v3:** Account for final transfer in slippage check ([0476e7f](https://github.com/propeller-heads/tycho-indexer/commit/0476e7f255a54c208d99f46e68df466de6337d00))
* **router v3:** Account for final transfer in slippage check ([#1015](https://github.com/propeller-heads/tycho-indexer/issues/1015)) ([2b4bc93](https://github.com/propeller-heads/tycho-indexer/commit/2b4bc938a76f92a5f2514aa8eaaba514dda1045b))

## [0.291.2](https://github.com/propeller-heads/tycho-indexer/compare/0.291.1...0.291.2) (2026-05-14)


### Bug Fixes

* **router v3:** Vault deposits for fee tokens ([d9efb9d](https://github.com/propeller-heads/tycho-indexer/commit/d9efb9da17aaa0fb22fd1deeb0caf01f140e7c5f))
* **router v3:** Vault deposits for fee tokens ([#1014](https://github.com/propeller-heads/tycho-indexer/issues/1014)) ([b5b7873](https://github.com/propeller-heads/tycho-indexer/commit/b5b7873d203a8a342ea9c62b3d8010a39ccc942b))

## [0.291.1](https://github.com/propeller-heads/tycho-indexer/compare/0.291.0...0.291.1) (2026-05-13)


### Bug Fixes

* **deps:** upgrade diesel to 2.3.9 to resolve RUSTSEC-2026-0136 and RUSTSEC-2026-0137 ([a280cb5](https://github.com/propeller-heads/tycho-indexer/commit/a280cb5dbbb45d21e8c6c12373f2b94b062efe62))
* **deps:** upgrade diesel to 2.3.9 to resolve RUSTSEC-2026-0136 and RUSTSEC-2026-0137 ([#1012](https://github.com/propeller-heads/tycho-indexer/issues/1012)) ([72e75fc](https://github.com/propeller-heads/tycho-indexer/commit/72e75fcf5ab3b1690b8c05b866228071d7c337f1))

## [0.291.0](https://github.com/propeller-heads/tycho-indexer/compare/0.290.0...0.291.0) (2026-05-13)


### Features

* tune protocol gas estimates based on tenderly traces ([8f649db](https://github.com/propeller-heads/tycho-indexer/commit/8f649db02a5cd047065ac9bf6b66af30b2722ff6))


### Bug Fixes

* calibrate protocol specific gas costs ([#977](https://github.com/propeller-heads/tycho-indexer/issues/977)) ([f9c7c4f](https://github.com/propeller-heads/tycho-indexer/commit/f9c7c4fdd90064d28846c9013f57acb40eecce74))

## [0.290.0](https://github.com/propeller-heads/tycho-indexer/compare/0.289.2...0.290.0) (2026-05-13)


### Features

* **common:** Add chain-aware TVL default thresholds ([3efe875](https://github.com/propeller-heads/tycho-indexer/commit/3efe87569cdab2639899e7e2a7f80de6ed938e2f))
* **common:** Add chain-aware TVL default thresholds ([#1008](https://github.com/propeller-heads/tycho-indexer/issues/1008)) ([f956e3f](https://github.com/propeller-heads/tycho-indexer/commit/f956e3fd0c55058ba1022a8c776fa64cf389999b))


### Performance Improvements

* Check for cli TVL only if needed ([7987192](https://github.com/propeller-heads/tycho-indexer/commit/79871925d38a22566ae7a04dca83b2133f1206e5))

## [0.289.2](https://github.com/propeller-heads/tycho-indexer/compare/0.289.1...0.289.2) (2026-05-13)

## [0.289.1](https://github.com/propeller-heads/tycho-indexer/compare/0.289.0...0.289.1) (2026-05-12)


### Bug Fixes

* **tycho-execution:** add missing alloy sol-types dependency feature ([f121a91](https://github.com/propeller-heads/tycho-indexer/commit/f121a91af9ee120a353b84a08880d9ce34f79833))

## [0.289.0](https://github.com/propeller-heads/tycho-indexer/compare/0.288.3...0.289.0) (2026-05-11)


### Features

* **router v3:** use native ETH marker instead of address(0) ([6395eda](https://github.com/propeller-heads/tycho-indexer/commit/6395eda1eeb14b9ad41c9afd49a234a67ae74012))
* **router v3:** use native ETH marker instead of address(0) ([#984](https://github.com/propeller-heads/tycho-indexer/issues/984)) ([db076f6](https://github.com/propeller-heads/tycho-indexer/commit/db076f6d0c5173f2045efea68f33f6ad0266441c))


### Bug Fixes

* Helper method to fix slither's cyclomatic-complexity warning ([365a09b](https://github.com/propeller-heads/tycho-indexer/commit/365a09b55698989a1939d7925b419274592f3755))
* **maximodel:** Native sell address for FluidV1 ([62b2a53](https://github.com/propeller-heads/tycho-indexer/commit/62b2a53c9c695f4b62a0d97a209d12986817c43c))

## [0.288.3](https://github.com/propeller-heads/tycho-indexer/compare/0.288.2...0.288.3) (2026-05-11)


### Bug Fixes

* **Dispatcher:** Handle delegatecall errors nicely ([46ab64f](https://github.com/propeller-heads/tycho-indexer/commit/46ab64ffe85d9d517016ac613926a2f5f6ee9b7a))
* **Dispatcher:** Use regular calls instead of static calls ([15c2a0c](https://github.com/propeller-heads/tycho-indexer/commit/15c2a0c571772f1cf12f780cb8be2a370c41791a))
* **Dispatcher:** Use regular calls instead of static calls + Handle delegatecall errors nicely  ([#1004](https://github.com/propeller-heads/tycho-indexer/issues/1004)) ([ce9d535](https://github.com/propeller-heads/tycho-indexer/commit/ce9d53508a89ff271eadec38be7fff390d9479cb))

## [0.288.2](https://github.com/propeller-heads/tycho-indexer/compare/0.288.1...0.288.2) (2026-05-11)


### Bug Fixes

* **integration-test:** add staleness watchdog for protocol sync metrics ([988ee88](https://github.com/propeller-heads/tycho-indexer/commit/988ee88aac8877b6f7f9447f8464d92b87fd5279))
* **integration-test:** add staleness watchdog for protocol sync metrics ([#978](https://github.com/propeller-heads/tycho-indexer/issues/978)) ([e74b576](https://github.com/propeller-heads/tycho-indexer/commit/e74b576972c18e401345c890408bb5da6594945f))

## [0.288.1](https://github.com/propeller-heads/tycho-indexer/compare/0.288.0...0.288.1) (2026-05-08)


### Bug Fixes

* update CI workflow and Dockerfile for improved profiling ([e02d13f](https://github.com/propeller-heads/tycho-indexer/commit/e02d13f7854e395d66ee6a0cc4af8957762c39b1))
* update CI workflow and Dockerfile for improved profiling ([#1002](https://github.com/propeller-heads/tycho-indexer/issues/1002)) ([ca6a961](https://github.com/propeller-heads/tycho-indexer/commit/ca6a9613f11a19f10520dcfe871a807835550a10))

## [0.288.0](https://github.com/propeller-heads/tycho-indexer/compare/0.287.0...0.288.0) (2026-05-08)


### Features

* **indexer:** add access control to heap profiling endpoint ([26a2f05](https://github.com/propeller-heads/tycho-indexer/commit/26a2f05dbf79d325cafb7088692b9f9ca5fa8c74))
* **indexer:** add jemalloc memory profiling support ([7f77178](https://github.com/propeller-heads/tycho-indexer/commit/7f77178f1c218bfc71629043b8a1730cb01d7088))
* **indexer:** make jemalloc default and add flexible CI build options ([93e691a](https://github.com/propeller-heads/tycho-indexer/commit/93e691aae3d593e5dd7077398939fb9bb8b12751))
* **indexer:** use jemalloc and add memory profiling tools ([#994](https://github.com/propeller-heads/tycho-indexer/issues/994)) ([13d3ac1](https://github.com/propeller-heads/tycho-indexer/commit/13d3ac10f944ee8e5e0691215afc406baa953002))


### Bug Fixes

* **indexer:** enable pprof pre-symbolization for cross-platform profiling ([a2a122b](https://github.com/propeller-heads/tycho-indexer/commit/a2a122be17c1d6df0271997cb451bef3e0928c87))

## [0.287.0](https://github.com/propeller-heads/tycho-indexer/compare/0.286.0...0.287.0) (2026-05-07)


### Features

* add signed error ratio ([1c9980b](https://github.com/propeller-heads/tycho-indexer/commit/1c9980b098da47f5ae5e831ce5749c82c7030407))
* add signed gas_error_ratio metric histogram for integration tests ([#995](https://github.com/propeller-heads/tycho-indexer/issues/995)) ([5a18b0b](https://github.com/propeller-heads/tycho-indexer/commit/5a18b0bc17a1197687d6db5aec0fec56850d715c))

## [0.286.0](https://github.com/propeller-heads/tycho-indexer/compare/0.285.4...0.286.0) (2026-05-07)


### Features

* **router v3:** Timelock FeeCalculator ([0091337](https://github.com/propeller-heads/tycho-indexer/commit/00913377dd3a53f4951307d625027ca32c4143ac))
* **router v3:** Timelock FeeCalculator ([#986](https://github.com/propeller-heads/tycho-indexer/issues/986)) ([62ccdcc](https://github.com/propeller-heads/tycho-indexer/commit/62ccdccc77b79b1f61516553835d6d7d0919507a))

## [0.285.4](https://github.com/propeller-heads/tycho-indexer/compare/0.285.3...0.285.4) (2026-05-07)


### Bug Fixes

* **encoding:** Estimate gas properly for grouped swaps ([1e31ca2](https://github.com/propeller-heads/tycho-indexer/commit/1e31ca2992bcecae07baf6357bd9a8c111e75fcb))
* **encoding:** Estimate gas properly for grouped swaps ([#993](https://github.com/propeller-heads/tycho-indexer/issues/993)) ([837c2b3](https://github.com/propeller-heads/tycho-indexer/commit/837c2b3adf6dc3371a076729c473c935a0690343))

## [0.285.3](https://github.com/propeller-heads/tycho-indexer/compare/0.285.2...0.285.3) (2026-05-06)


### Bug Fixes

* Move input token gas logic from integration test to execution ([fa48863](https://github.com/propeller-heads/tycho-indexer/commit/fa48863bb0022b9096d9481aa3629c1b0ec4e9ff))
* Move input token gas logic from integration test to execution ([#991](https://github.com/propeller-heads/tycho-indexer/issues/991)) ([e1bfc76](https://github.com/propeller-heads/tycho-indexer/commit/e1bfc7638a4999184a7a62874ceb5d3725bcf80f))

## [0.285.2](https://github.com/propeller-heads/tycho-indexer/compare/0.285.1...0.285.2) (2026-05-06)


### Bug Fixes

* ensure_chains seeds token prices for pre-existing chains ([6cbd656](https://github.com/propeller-heads/tycho-indexer/commit/6cbd6561ebfbadb92be1c53a49a3b3d0f2082a43))
* ensure_chains seeds token prices for pre-existing chains ([#992](https://github.com/propeller-heads/tycho-indexer/issues/992)) ([f31287b](https://github.com/propeller-heads/tycho-indexer/commit/f31287b1c266e935f837f9eecbb58c4ef74e4895))

## [0.285.1](https://github.com/propeller-heads/tycho-indexer/compare/0.285.0...0.285.1) (2026-05-06)

## [0.285.0](https://github.com/propeller-heads/tycho-indexer/compare/0.284.0...0.285.0) (2026-05-06)


### Features

* add seed_native_token_prices to MockGateway ([43e5d0c](https://github.com/propeller-heads/tycho-indexer/commit/43e5d0c00b5961262deea7ec6eddae101b206cbf))
* add seed_native_token_prices to ProtocolGateway trait ([a8635f2](https://github.com/propeller-heads/tycho-indexer/commit/a8635f25250e8c65b4153232ee6f23117e337616))
* call seed_native_token_prices at indexer startup ([77c5f28](https://github.com/propeller-heads/tycho-indexer/commit/77c5f28908edfb02b96d00367eb640f79f16ceb2))
* **encoding:** Add gas usage attributes ([2198bfc](https://github.com/propeller-heads/tycho-indexer/commit/2198bfc7f31dd1242f3f5fc649dbd3ee331d6ceb))
* **encoding:** Add gas usage attributes  ([#983](https://github.com/propeller-heads/tycho-indexer/issues/983)) ([74683e7](https://github.com/propeller-heads/tycho-indexer/commit/74683e739770b6f7980b654659cd76ff71d04187))
* implement seed_native_token_prices on CachedGateway and DirectGateway ([9be3ee1](https://github.com/propeller-heads/tycho-indexer/commit/9be3ee1b0174c5fd53805a88cb518da1acb43ec6))
* implement seed_native_token_prices on PostgresGateway ([176685f](https://github.com/propeller-heads/tycho-indexer/commit/176685f63c3c631e000e5179c87f40dbde930a2d))
* seed native token prices at indexer startup ([#990](https://github.com/propeller-heads/tycho-indexer/issues/990)) ([f527417](https://github.com/propeller-heads/tycho-indexer/commit/f527417b4d1927586c2036387f51ec3552af89a3))


### Bug Fixes

* address PR feedback and fix DB test setup ([ad6614a](https://github.com/propeller-heads/tycho-indexer/commit/ad6614a2ed3c39c182cd60c5d91b4b08a8e49c29))
* **encoding:** Rename (estimated_)gas_usage -> estimated_gas ([7660e1b](https://github.com/propeller-heads/tycho-indexer/commit/7660e1bbec2283446a1493d947d4867e5a00b3ee))
* **quickstart:** Use GetAmountOutResult in get_best_swap ([c2986ce](https://github.com/propeller-heads/tycho-indexer/commit/c2986ce7dc861a06797a671f41dbffbe64183c01))


### Reverts

* Revert "feat: Change Swap token_in/token_out from Token to Bytes (#89)" ([04d30c7](https://github.com/propeller-heads/tycho-indexer/commit/04d30c7648b173ae3b305643f42537e63035bd7d)), closes [#89](https://github.com/propeller-heads/tycho-indexer/issues/89) [#89](https://github.com/propeller-heads/tycho-indexer/issues/89) [post-#89](https://github.com/propeller-heads/post-/issues/89)

## [0.284.0](https://github.com/propeller-heads/tycho-indexer/compare/0.283.1...0.284.0) (2026-05-06)


### Features

* add liquidityparty to integration test ([dc99f9e](https://github.com/propeller-heads/tycho-indexer/commit/dc99f9ef0ce8cc75a8c4fa234019e482a8407dff))
* add liquidityparty to integration test ([#974](https://github.com/propeller-heads/tycho-indexer/issues/974)) ([bca8a62](https://github.com/propeller-heads/tycho-indexer/commit/bca8a62663e36d8503b1292971ec21ddc5ee375a))

## [0.283.1](https://github.com/propeller-heads/tycho-indexer/compare/0.283.0...0.283.1) (2026-05-06)


### Bug Fixes

* update v3-style swap tick after partial price move ([f365636](https://github.com/propeller-heads/tycho-indexer/commit/f365636ab8027b98ab998da9fd6003b81f0459de))
* update v3-style swap tick after partial price move ([#971](https://github.com/propeller-heads/tycho-indexer/issues/971)) ([8b902c1](https://github.com/propeller-heads/tycho-indexer/commit/8b902c124492d820e51cd833411bc1904970cc0f))

## [0.283.0](https://github.com/propeller-heads/tycho-indexer/compare/0.282.0...0.283.0) (2026-05-05)


### Features

* **router v3 model:** Consider wasted router funds ([725c59f](https://github.com/propeller-heads/tycho-indexer/commit/725c59f6037a9498a80679ac80fbc9dab9807e41))
* **router v3 model:** Consider wasted router funds ([#970](https://github.com/propeller-heads/tycho-indexer/issues/970)) ([643e7f8](https://github.com/propeller-heads/tycho-indexer/commit/643e7f8903beaf77c49d39e27a7ae89692a326ad))

## [0.282.0](https://github.com/propeller-heads/tycho-indexer/compare/0.281.1...0.282.0) (2026-05-05)


### Features

* **integration test:** Consider input token gas ([42c2ef9](https://github.com/propeller-heads/tycho-indexer/commit/42c2ef991b9e61f6c65fd12d7d7a060179343494))
* **integration test:** Consider input token gas ([#975](https://github.com/propeller-heads/tycho-indexer/issues/975)) ([849ec05](https://github.com/propeller-heads/tycho-indexer/commit/849ec0525ea847d494941c0af135f8b1bb68bac0))

## [0.281.1](https://github.com/propeller-heads/tycho-indexer/compare/0.281.0...0.281.1) (2026-05-05)


### Bug Fixes

* liquidity party protocol name ([cd0680b](https://github.com/propeller-heads/tycho-indexer/commit/cd0680b29935ae36df087384a501fb4551d0ea69))
* liquidity party protocol name ([#980](https://github.com/propeller-heads/tycho-indexer/issues/980)) ([163b772](https://github.com/propeller-heads/tycho-indexer/commit/163b772120b9e82ddb80d1880d122a19ccb2866e))

## [0.281.0](https://github.com/propeller-heads/tycho-indexer/compare/0.280.1...0.281.0) (2026-05-05)


### Features

* **config:** add liquidityparty address to executor_addresses.json ([230a6fa](https://github.com/propeller-heads/tycho-indexer/commit/230a6faa8e5e1c2c7f63f5d8c052bee4ad7b846f))
* **config:** add liquidityparty address to executor_addresses.json ([#979](https://github.com/propeller-heads/tycho-indexer/issues/979)) ([33dae2c](https://github.com/propeller-heads/tycho-indexer/commit/33dae2c2f4ad36d39b7c50449639381846e17a95))

## [0.280.1](https://github.com/propeller-heads/tycho-indexer/compare/0.280.0...0.280.1) (2026-05-05)


### Bug Fixes

* **client:** address review feedback on ws-connections fixes ([c7677b9](https://github.com/propeller-heads/tycho-indexer/commit/c7677b9943ec7f37413e3cf3cbaf2e0dae3319a7))
* **client:** handle stale snapshot blocks during initialization ([31d0d0e](https://github.com/propeller-heads/tycho-indexer/commit/31d0d0e52a870f1490262d4b02741a223cb571ec))
* **client:** improve rate limit and WS close logging ([f75b366](https://github.com/propeller-heads/tycho-indexer/commit/f75b3666e54e0dcc7f0073f42c2eee83325d92bc))
* **client:** keep main loop alive when all synchronizers are temporarily stale ([9dfd288](https://github.com/propeller-heads/tycho-indexer/commit/9dfd288f282913ed7fa9f662dd9cd35e19625653))
* **client:** skip unrecognised extractors instead of crashing on init ([a7beec6](https://github.com/propeller-heads/tycho-indexer/commit/a7beec676a33e1dcc2201a2230fe8bea2d7bf420))
* **deltas:** add timeouts to subscribe/unsubscribe confirmation waits ([0291430](https://github.com/propeller-heads/tycho-indexer/commit/0291430ef17b4d8e83b00f1a896134c0d7f52d08))
* **deltas:** close TOCTOU race in ensure_connection using Notify::enable ([8d58128](https://github.com/propeller-heads/tycho-indexer/commit/8d58128aedeeefe7772f86d4f032011026180174))
* **deltas:** detect stalled TCP connections with a 60s WS idle timeout ([f898479](https://github.com/propeller-heads/tycho-indexer/commit/f89847922a06bbb1553b9a3256d17db6549e2aca))
* reduce client timeout time ([034d103](https://github.com/propeller-heads/tycho-indexer/commit/034d10372617d08f226ec72065ed8732009ae2fe))
* **synchronizer:** reset retry count after a successful synchronization run ([b78f4b0](https://github.com/propeller-heads/tycho-indexer/commit/b78f4b0ff99feaaf9be6733311f1998eff680bf7))
* **tycho-client:** improve WS connection handling ([#967](https://github.com/propeller-heads/tycho-indexer/issues/967)) ([56757e8](https://github.com/propeller-heads/tycho-indexer/commit/56757e826dc55511aae6eb866e64396d9c705e28))

## [0.280.0](https://github.com/propeller-heads/tycho-indexer/compare/0.279.0...0.280.0) (2026-05-01)


### Features

* add build-time environment diagnostics ([8dd7604](https://github.com/propeller-heads/tycho-indexer/commit/8dd7604f93aa7752d2edd6535d144c9520d73dca))
* **router v3 model:** Add missing executors ([1c4dfa4](https://github.com/propeller-heads/tycho-indexer/commit/1c4dfa42176fe552850af5bcdae50284c62dd6a3))
* **router v3 model:** Add missing executors ([#969](https://github.com/propeller-heads/tycho-indexer/issues/969)) ([0894632](https://github.com/propeller-heads/tycho-indexer/commit/08946320a41f5aedb31ffe95b575792488edaacd))


### Reverts

* remove build-time feature detection script ([892425f](https://github.com/propeller-heads/tycho-indexer/commit/892425f3f6b3c1ead3f940206f2e68f21c159f8f))
* remove build-time protocol feature flags ([f897dd3](https://github.com/propeller-heads/tycho-indexer/commit/f897dd304cb1aa1ccab52047c101dea71e2d4ddc))

## [0.279.0](https://github.com/propeller-heads/tycho-indexer/compare/0.278.5...0.279.0) (2026-04-30)


### Features

* add gas_error_ratio histogram metric for integration tests ([a19e2ac](https://github.com/propeller-heads/tycho-indexer/commit/a19e2ace28b8f5b5db873a4497e106527e10fe18))
* add gas_error_ratio metric histogram for integration tests ([#968](https://github.com/propeller-heads/tycho-indexer/issues/968)) ([e89e9a8](https://github.com/propeller-heads/tycho-indexer/commit/e89e9a82279d493b67367627f0b8545bd7cef837))
* wire gas_error_ratio metric emission at execution result callsite ([fa06b51](https://github.com/propeller-heads/tycho-indexer/commit/fa06b51e76bc4b1bebdc6e9b0779a065e45e5890))


### Bug Fixes

* add missing field ([e384d5d](https://github.com/propeller-heads/tycho-indexer/commit/e384d5dfb9dd9a8d3227e9b2d7537ed91ea90c92))

## [0.278.5](https://github.com/propeller-heads/tycho-indexer/compare/0.278.4...0.278.5) (2026-04-30)

## [0.278.4](https://github.com/propeller-heads/tycho-indexer/compare/0.278.3...0.278.4) (2026-04-30)


### Bug Fixes

* **RouterV3 model:** Curve executor token decoding ([#964](https://github.com/propeller-heads/tycho-indexer/issues/964)) ([243fef1](https://github.com/propeller-heads/tycho-indexer/commit/243fef173596c6b2a11a8385f67e18a01a76e797))
* **RouterV3 model:** Fix curve executor decoding ([2a21bee](https://github.com/propeller-heads/tycho-indexer/commit/2a21bee2064a31f52965bf9fbe078968b8041f16))

## [0.278.3](https://github.com/propeller-heads/tycho-indexer/compare/0.278.2...0.278.3) (2026-04-30)


### Bug Fixes

* **indexer:** DCI cache unbounded growth from retry params ([#962](https://github.com/propeller-heads/tycho-indexer/issues/962)) ([71fe607](https://github.com/propeller-heads/tycho-indexer/commit/71fe6079bed7a83eb093ca0a7a339d3bd2937494))
* **indexer:** read retry count across all cache layers before incrementing ([28e01bd](https://github.com/propeller-heads/tycho-indexer/commit/28e01bd8702f2850442ce757d3df5b7899b536af))
* **indexer:** stop re-broadcasting retry params into DCI cache ([fccb715](https://github.com/propeller-heads/tycho-indexer/commit/fccb715ec02ee9fc1620a27b07334c7ee0d372b7))

## [0.278.2](https://github.com/propeller-heads/tycho-indexer/compare/0.278.1...0.278.2) (2026-04-30)


### Bug Fixes

* handle short byte slices in ResponseAccount B256 conversion ([f85e888](https://github.com/propeller-heads/tycho-indexer/commit/f85e88805857055c72cd9ae8ce1e61434f925c89))
* handle short byte slices in ResponseAccount B256 conversion ([#957](https://github.com/propeller-heads/tycho-indexer/issues/957)) ([42467af](https://github.com/propeller-heads/tycho-indexer/commit/42467afab8b1eb7195a8e6fd30ff19a65d110082))
* remove dead tx fields from simulation ResponseAccount, use TryFrom ([e4df7ad](https://github.com/propeller-heads/tycho-indexer/commit/e4df7adece2b0972b52db71d4bb64517ddeb307d))

## [0.278.1](https://github.com/propeller-heads/tycho-indexer/compare/0.278.0...0.278.1) (2026-04-30)

## [0.278.0](https://github.com/propeller-heads/tycho-indexer/compare/0.277.0...0.278.0) (2026-04-30)


### Features

* add RPC for forge test ([c65b5ab](https://github.com/propeller-heads/tycho-indexer/commit/c65b5ab7db4c9e8e012c4c6c5ecdd238707fb617))
* add RPC for forge test ([#960](https://github.com/propeller-heads/tycho-indexer/issues/960)) ([b22a00d](https://github.com/propeller-heads/tycho-indexer/commit/b22a00d3dd90003f132926d8f219a9b890dd0abe))

## [0.277.0](https://github.com/propeller-heads/tycho-indexer/compare/0.276.0...0.277.0) (2026-04-29)


### Features

* add Router V3 model ([#961](https://github.com/propeller-heads/tycho-indexer/issues/961)) ([b5f6d8c](https://github.com/propeller-heads/tycho-indexer/commit/b5f6d8cf40f1d739c69e0b4eaa766a63bf0e8223))

## [0.276.0](https://github.com/propeller-heads/tycho-indexer/compare/0.275.1...0.276.0) (2026-04-28)


### Features

* **indexer:** add granular DCI cache metrics ([e859679](https://github.com/propeller-heads/tycho-indexer/commit/e859679e38dde992a07b6543bfe98fec89ddc1c7))
* **indexer:** add granular DCI cache metrics ([#953](https://github.com/propeller-heads/tycho-indexer/issues/953)) ([c0886ad](https://github.com/propeller-heads/tycho-indexer/commit/c0886ad0ac4b50e7fdb7fa417b4788ff4310d1a0))

## [0.275.1](https://github.com/propeller-heads/tycho-indexer/compare/0.275.0...0.275.1) (2026-04-28)


### Bug Fixes

* **integration test:** update validation to handle partial blocks ([#955](https://github.com/propeller-heads/tycho-indexer/issues/955)) ([5ca66da](https://github.com/propeller-heads/tycho-indexer/commit/5ca66da175e69cfed2a4bdf108b15bf494e2f91c))
* **integration-test:** use pending block ID for validation in partial blocks mode ([e217f40](https://github.com/propeller-heads/tycho-indexer/commit/e217f400440de0fb5680565c5f4259b39171828c))

## [0.275.0](https://github.com/propeller-heads/tycho-indexer/compare/0.274.1...0.275.0) (2026-04-27)


### Features

* add Serialize/Deserialize to Update struct ([a7c3859](https://github.com/propeller-heads/tycho-indexer/commit/a7c3859040f47c5971a7e3fb6220467d09cf009c))
* **simulation:** add Serialize/Deserialize to Update struct ([#954](https://github.com/propeller-heads/tycho-indexer/issues/954)) ([a6ca0b7](https://github.com/propeller-heads/tycho-indexer/commit/a6ca0b7b87df86dc2a48f513b572bba9ab63bb29))


### Bug Fixes

* address PR review comments ([a61c7ed](https://github.com/propeller-heads/tycho-indexer/commit/a61c7ed6fa047e1038001d60b0150f3a6c5fa51f))
* address review feedback for Update serde support ([a00578a](https://github.com/propeller-heads/tycho-indexer/commit/a00578a40b567c81058b99bbc4e202b1977b5db3))

## [0.274.1](https://github.com/propeller-heads/tycho-indexer/compare/0.274.0...0.274.1) (2026-04-27)


### Bug Fixes

* **integration-test:** fix flashblock execution simulations ([#948](https://github.com/propeller-heads/tycho-indexer/issues/948)) ([18775c3](https://github.com/propeller-heads/tycho-indexer/commit/18775c30a4ff46834cee65ac62ace3d982090649))
* **integration-test:** fix flashblock simulation support ([a3dcc8b](https://github.com/propeller-heads/tycho-indexer/commit/a3dcc8bceedbc2e71600e8376214e0e10ef190bc))
* **integration-test:** record latency for stale blocks and use signed values ([b7a6aac](https://github.com/propeller-heads/tycho-indexer/commit/b7a6aacdbf461e3c8bd0ede440a4ed0bf621b296))

## [0.274.0](https://github.com/propeller-heads/tycho-indexer/compare/0.273.0...0.274.0) (2026-04-27)


### Features

* **tracing:** upgrade opentelemetry stack and add per-layer OTLP filtering ([ea2b191](https://github.com/propeller-heads/tycho-indexer/commit/ea2b191b06ab0b730638f343afd0be1db8a4c1d1))
* **tracing:** upgrade opentelemetry stack and add per-layer OTLP filtering ([#952](https://github.com/propeller-heads/tycho-indexer/issues/952)) ([7547074](https://github.com/propeller-heads/tycho-indexer/commit/754707469e19065a2692586354ce5ff941c4bffc))

## [0.273.0](https://github.com/propeller-heads/tycho-indexer/compare/0.272.0...0.273.0) (2026-04-27)


### Features

* add limit on number of tries while searching token slots ([77f7dea](https://github.com/propeller-heads/tycho-indexer/commit/77f7deadb1c139260aae4decade9f97180c0ce91))
* add limit on number of tries while searching token slots ([#946](https://github.com/propeller-heads/tycho-indexer/issues/946)) ([4e88cfa](https://github.com/propeller-heads/tycho-indexer/commit/4e88cfa7bf77479e936b790c0074000a738682be))
* add tests for sort_slots_by_priority ([50cabfc](https://github.com/propeller-heads/tycho-indexer/commit/50cabfc42aecf8256cf806b37070f10c7a2536e3))


### Bug Fixes

* reduce the amount of slots to test instead of retries ([2277893](https://github.com/propeller-heads/tycho-indexer/commit/2277893f6a275694aa1c43a51f1224492c11c1e4))

## [0.272.0](https://github.com/propeller-heads/tycho-indexer/compare/0.271.0...0.272.0) (2026-04-27)


### Features

* minor adjustments to `tycho-ethereum` ([#818](https://github.com/propeller-heads/tycho-indexer/issues/818)) ([6353f69](https://github.com/propeller-heads/tycho-indexer/commit/6353f6910d3637a5010f7e6bd53402cc1003ed71))
* **rpc:** add `simulate_txs_with_trace` method with state overrides and batch tracing ([0552d31](https://github.com/propeller-heads/tycho-indexer/commit/0552d312e20bedc364e7c29f19dc15634fe8374b))
* **rpc:** handle non-retryable "execution reverted" EVM error in retry logic ([5eefdda](https://github.com/propeller-heads/tycho-indexer/commit/5eefddad51f9685a1d22a4c8ab4ea11a36689597))

## [0.271.0](https://github.com/propeller-heads/tycho-indexer/compare/0.270.0...0.271.0) (2026-04-24)


### Features

* cowamm substreams ([4a3c362](https://github.com/propeller-heads/tycho-indexer/commit/4a3c3627b68e15768a8644649894a203bd754610))
* cowamm substreams ([#939](https://github.com/propeller-heads/tycho-indexer/issues/939)) ([98b6d61](https://github.com/propeller-heads/tycho-indexer/commit/98b6d61f51770061586085eb4e7ca85528511a7b))


### Bug Fixes

* remove non-ethereum cowamm manifests ([bc33221](https://github.com/propeller-heads/tycho-indexer/commit/bc33221abdefb2c412a3490b90178a2c046e3506))
* remove unused cowamm dependency ([5428fb7](https://github.com/propeller-heads/tycho-indexer/commit/5428fb78f527f49d5bae9bbd53e91de1a04ad293))

## [0.270.0](https://github.com/propeller-heads/tycho-indexer/compare/0.269.0...0.270.0) (2026-04-24)


### Features

* ekubo_v3 substreams ([#935](https://github.com/propeller-heads/tycho-indexer/issues/935)) ([c424f86](https://github.com/propeller-heads/tycho-indexer/commit/c424f86bb26f48c5c8eff61fcb72048e0940aab6))


### Bug Fixes

* address ekubo v3 substreams ci ([2a25b64](https://github.com/propeller-heads/tycho-indexer/commit/2a25b64ed84abd978045caa1c93d416b3ec7e9e1))

## [0.269.0](https://github.com/propeller-heads/tycho-indexer/compare/0.268.0...0.269.0) (2026-04-24)


### Features

* arbitrum substreams configs ([40f498b](https://github.com/propeller-heads/tycho-indexer/commit/40f498b39fa65c0b282ae1fde9c36a6f0774aedb))
* arbitrum substreams configs ([#941](https://github.com/propeller-heads/tycho-indexer/issues/941)) ([b720fef](https://github.com/propeller-heads/tycho-indexer/commit/b720fef2bf3c257e83405e2c594010665c4198ae))
* bsc substreams configs ([56c3699](https://github.com/propeller-heads/tycho-indexer/commit/56c369968a95ac812bf0b137a003f3d40658e0e4))
* bsc substreams configs ([#942](https://github.com/propeller-heads/tycho-indexer/issues/942)) ([a53ce59](https://github.com/propeller-heads/tycho-indexer/commit/a53ce5915ee2620fc60a5ffcea5473d5ef63a69a))
* curve substreams ([9161740](https://github.com/propeller-heads/tycho-indexer/commit/9161740386fc7f6e1a5380e62c7ce91f8c3dbc68))
* curve substreams ([#940](https://github.com/propeller-heads/tycho-indexer/issues/940)) ([a193b2c](https://github.com/propeller-heads/tycho-indexer/commit/a193b2c3b939945b155da47c0ef3bd26e32dafda))


### Bug Fixes

* bump curve substreams package versions ([9a9eeb6](https://github.com/propeller-heads/tycho-indexer/commit/9a9eeb63a21da51a655f3a15aa8c874d2bdaa7dd))

## [0.268.0](https://github.com/propeller-heads/tycho-indexer/compare/0.267.0...0.268.0) (2026-04-24)


### Features

* Add Aerodrome V1 substreams integration for Base chain ([a49fa30](https://github.com/propeller-heads/tycho-indexer/commit/a49fa307791400ac7436331730652f0519cadc67))
* add attributes for RocketPool Queue to compute the getEffectiveCapacity in simulation ([d1b60d4](https://github.com/propeller-heads/tycho-indexer/commit/d1b60d4257ec30e82968cd4b540a25d19a9d3a68))
* add deposit_assign_enabled attribute for simulation ([3b0f397](https://github.com/propeller-heads/tycho-indexer/commit/3b0f3972f4b1f8885a009374addf2e1c9f603540))
* add deposit_assign_maximum and deposit_assign_socialised_maximum constants ([98f9084](https://github.com/propeller-heads/tycho-indexer/commit/98f9084aed0e3f65653167503d45b42023ce2e38))
* add reth_collateral_target tracking, use v4 network balances ABI ([71ecbc8](https://github.com/propeller-heads/tycho-indexer/commit/71ecbc8e42cf833199561fe4a5a038a669080bd9))
* Add Rocketpool ABI path to rustfmt configuration ([70d1810](https://github.com/propeller-heads/tycho-indexer/commit/70d181083cdf0e47685d22ee0cedb5b94b8b785b))
* add RocketpoolState support in protocol mapping and update dependencies ([add13e6](https://github.com/propeller-heads/tycho-indexer/commit/add13e687165cf3caa75eeeb06fba62cb5dff13d))
* add Saturn v4 dual-version support for RocketPool substreams ([ef630c2](https://github.com/propeller-heads/tycho-indexer/commit/ef630c29988884a01458ebd34b0408c03c1138d1))
* add script to compute initial state values for substreams.yaml parameters ([a51d847](https://github.com/propeller-heads/tycho-indexer/commit/a51d8476960b16bf57f7a055701c16b694c386a0))
* add target_reth_collateral_rate storage slot tracking ([d5f6743](https://github.com/propeller-heads/tycho-indexer/commit/d5f674391f8ff360cd5e3f499c390768da0768fc))
* aerodrome v1 substreams ([#930](https://github.com/propeller-heads/tycho-indexer/issues/930)) ([eebd577](https://github.com/propeller-heads/tycho-indexer/commit/eebd577f86a7a40dc6157c2ce766981930b7471a))
* do vault liquidity tracking by using deposit pool events and storage slots instead of deltas ([dff4d9b](https://github.com/propeller-heads/tycho-indexer/commit/dff4d9b0170b552b950753806cf01db2ac726dd9))
* enhance minipool queue handling with queue_id based storage slot mapping ([433276f](https://github.com/propeller-heads/tycho-indexer/commit/433276f3a33e9b1aa5f8300d21d16567b823e891))
* finish updated execution integration, and explain why we will not be running execution integration tests due to reasons explain in the integration_test ([dda708c](https://github.com/propeller-heads/tycho-indexer/commit/dda708c612a6019f9f6da9afb48652aadcd78a93))
* Implement Ethereum Rocketpool integration with protocol components and balance management (tests do not pass) ([b8c514a](https://github.com/propeller-heads/tycho-indexer/commit/b8c514a175c565e1efa0c7a56684146f23a07cd9))
* init velodrome ([7989ce3](https://github.com/propeller-heads/tycho-indexer/commit/7989ce3527a9afb0f7f53d231d29baad313f006b))
* initialize protocol component with initial state values at creation ([91c3261](https://github.com/propeller-heads/tycho-indexer/commit/91c3261e00dc9ff7e03dfc502ef0c7446d135fcd))
* integrate RocketTokenRETH contract and update liquidity indexing ([11b22eb](https://github.com/propeller-heads/tycho-indexer/commit/11b22eb1f0b06ef7450117dd2066579e9564e0ab))
* liquidity party substreams ([#933](https://github.com/propeller-heads/tycho-indexer/issues/933)) ([9852f12](https://github.com/propeller-heads/tycho-indexer/commit/9852f12963bacfcd308bc9f5627f0fc06f7c8fad))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) ([5194d52](https://github.com/propeller-heads/tycho-indexer/commit/5194d523679910339d145e21dbe8cb1d32c9fa71))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) added generated abi files ([1ed8d8f](https://github.com/propeller-heads/tycho-indexer/commit/1ed8d8f265234b9f7d6c7586e8ddab7440ff7a5d))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) added generated abi files ([848190d](https://github.com/propeller-heads/tycho-indexer/commit/848190d6de9cdef6190fafc6c8245f8509380049))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) clippy lint ([987ca5a](https://github.com/propeller-heads/tycho-indexer/commit/987ca5ac3a04fff84d2a56d999bbc5227dc813f3))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) factory update ([ba65aa6](https://github.com/propeller-heads/tycho-indexer/commit/ba65aa6cb3b160e87582060a4f07970ec17ad17c))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) helper contract rewind ([bab40e5](https://github.com/propeller-heads/tycho-indexer/commit/bab40e51cc99ccf66341a3cbd7c41daea91ef84b))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) license update ([78a2f08](https://github.com/propeller-heads/tycho-indexer/commit/78a2f089e843ca8980ab64463d4d12c663b37bc0))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) missing helper contract fix ([a2ea2af](https://github.com/propeller-heads/tycho-indexer/commit/a2ea2afeb7baa04a3a9e4101f22ffaf835596f4d))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) substreams Cargo.toml fix ([a87f530](https://github.com/propeller-heads/tycho-indexer/commit/a87f53041a0c9d7db45d8ed6dc2513e15a6c30e3))
* LiquidityParty adapter ([#296](https://github.com/propeller-heads/tycho-indexer/issues/296)) substreams yaml fix ([751023d](https://github.com/propeller-heads/tycho-indexer/commit/751023d906232c34412b801f6c7ae2ab5d629856))
* Refactor protocol component updates to use try_for_each and simplify error handling ([5c3fc04](https://github.com/propeller-heads/tycho-indexer/commit/5c3fc04e145f412a70dbf63ec4f96afc0bf13dfe))
* rename deposit contract liquidity to deposit contract balance for clarity ([ce12bef](https://github.com/propeller-heads/tycho-indexer/commit/ce12bef989e3e16f4c5d11eefcfd4b1250405966))
* rename deposit_assign_enabled to deposit_assigning_enabled for clarity ([3095b95](https://github.com/propeller-heads/tycho-indexer/commit/3095b95f84b1dff9e8b7ad01312323164b9929d8))
* Rename function to map_relative_component_liquidity and remove Result ([7f79ee0](https://github.com/propeller-heads/tycho-indexer/commit/7f79ee0577425072ace26291716bbba330adb3b4))
* rename max_deposit_amount to maximum_deposit_pool_size for clarity ([7bf860e](https://github.com/propeller-heads/tycho-indexer/commit/7bf860e19e1a7d2f1af4e2c39db7fa74f72c8126))
* rename maximum_deposit_pool_size to max_deposit_pool_size for consistency ([a5e216f](https://github.com/propeller-heads/tycho-indexer/commit/a5e216f8bec402d158a5c381d97337bea12b0e82))
* Replace dynamic fee module with custom fee module for Velodrome ([0d6fa25](https://github.com/propeller-heads/tycho-indexer/commit/0d6fa257f31ba1e742b1be2afe22859cea1666e9))
* rocketpool substreams ([#934](https://github.com/propeller-heads/tycho-indexer/issues/934)) ([60e090a](https://github.com/propeller-heads/tycho-indexer/commit/60e090a9c4681acbc69f46c79bbff745ed227816))
* simplify queue handling by removing legacy queue storage slots and asserting variable queue IDs ([99b238c](https://github.com/propeller-heads/tycho-indexer/commit/99b238cbfadbb9571b664865355e81a6b79d5448))
* start indexing from the RocketDepositPool_V1_2 deployment instead of V1_0 ([c7dc708](https://github.com/propeller-heads/tycho-indexer/commit/c7dc7085556c53e0a93711793f5723758641c5d9))
* unichain velodrome substreams ([#932](https://github.com/propeller-heads/tycho-indexer/issues/932)) ([12766b7](https://github.com/propeller-heads/tycho-indexer/commit/12766b7f12bbe8bae079ad217284d68d77fc70c2))
* unify ABI generation by reading from directory and converting names to snake case ([b2ae85e](https://github.com/propeller-heads/tycho-indexer/commit/b2ae85eb468a13add92e659befbcab226671bf61))
* Update protocol type name to 'rocketpool_pool' ([21c6cd1](https://github.com/propeller-heads/tycho-indexer/commit/21c6cd190ef9952aa28dca7cbb9c1260a3f2ba8d))
* Update Rocket Deposit Pool addresses to collect historical event information ([86a61c9](https://github.com/propeller-heads/tycho-indexer/commit/86a61c9c0b4b8ea9b906398a83414d0383b43c2f))


### Bug Fixes

* adapt aerodrome v1 manifest for monorepo ([fe4564a](https://github.com/propeller-heads/tycho-indexer/commit/fe4564ac3128d94a8c2b8a78a528eb65c6cdf5b4))
* adapt rocketpool manifest for monorepo ([d225995](https://github.com/propeller-heads/tycho-indexer/commit/d225995d9bf79d36c0ca740bb4048423c29a2dba))
* adapt unichain-velodrome manifest for monorepo ([589766d](https://github.com/propeller-heads/tycho-indexer/commit/589766d1150f72cfb3e7156bcb3d8bb780bd970a))
* correct logic for filtering ProposalExecuted events in protocol mapping ([9f06133](https://github.com/propeller-heads/tycho-indexer/commit/9f0613389a1c3ebf272f77b75d804ef706cecc29))
* downgrade heck to 0.4 to deduplicate dependency ([6f439ad](https://github.com/propeller-heads/tycho-indexer/commit/6f439ad9f3302445438f872f6b38ddff9fcba6b0))
* filter out reverted calls in protocol mapping for accurate data retrieval ([bf083bc](https://github.com/propeller-heads/tycho-indexer/commit/bf083bc4f29d50eaaac59da8b328349bbe25702e))
* Filter protocol settings storage updates by RocketStorage address ([8e4b276](https://github.com/propeller-heads/tycho-indexer/commit/8e4b276c2dd769672303444301b85ae910fec680))
* have liquidity as a attribute, and total eth as a component balance ([5ec7738](https://github.com/propeller-heads/tycho-indexer/commit/5ec7738582afb40c1ec39c312710111174828ec7))
* Ignore rustfmt for unichain-velodrome abi ([fde042e](https://github.com/propeller-heads/tycho-indexer/commit/fde042e5ec8a3aa27f688b39f83d9af81ee02d26))
* make unichain velodrome tests list explicit ([3f4a2b4](https://github.com/propeller-heads/tycho-indexer/commit/3f4a2b4866677a506feb2ec46037ca2589f393cb))
* remove unuse param and update module name ([9873bea](https://github.com/propeller-heads/tycho-indexer/commit/9873bea8381f5f85c8bad89e93c978a2cc5d3b16))
* restore skip-on-creation semantics, fix script default block ([cc3be8f](https://github.com/propeller-heads/tycho-indexer/commit/cc3be8fef005083bc28224b09080dfd7b2081518))
* restore swap adapter error types for liquidityparty ([4e75616](https://github.com/propeller-heads/tycho-indexer/commit/4e756160ab4ace227b7f737880570cee5d85673a))
* support both rocket_network_balances v2 and v3, update references in protocol mapping ([1203b3e](https://github.com/propeller-heads/tycho-indexer/commit/1203b3e7ae102db93dd63146968c1a65a4391969))
* Update CustomSwapFeeModule ABI ([d659a42](https://github.com/propeller-heads/tycho-indexer/commit/d659a42fde6e44fd5a91f83c76fca0870218bb3d))
* update documentation to clarify minipool queue behavior post V1.2 upgrade ([b359516](https://github.com/propeller-heads/tycho-indexer/commit/b359516f1372b9ab5cffb41816fc0b9d3eb67a34))
* Update liquidity attribute to use u128 instead of BigInt ([bac8032](https://github.com/propeller-heads/tycho-indexer/commit/bac80324254e5b23a997381d1daddf76c768518b))
* update liquidityparty substreams lockfile ([2e03af9](https://github.com/propeller-heads/tycho-indexer/commit/2e03af9e22b6b4b8efc7cb5805227642723eec76))
* update mapping logic to find the transaction that activated the Rocket Pool Deposit Pool V1.2 ([6253943](https://github.com/propeller-heads/tycho-indexer/commit/6253943f240ed602e568a5a5e1320ba3282aaf1d))
* update Rocket Deposit Pool to start indexing from the V1.2 contract was activated and not from the point the V1.2 contract was deployed ([ee75fdb](https://github.com/propeller-heads/tycho-indexer/commit/ee75fdb685694817cced6e5a44d17dbd1b52ecd9))
* Update storage slot mappings for Unichain Velodrome pools ([eaba018](https://github.com/propeller-heads/tycho-indexer/commit/eaba0185b7b85c1f210e996d24361648db1e0123))
* update substreams lockfile for liquidityparty ([f5783c9](https://github.com/propeller-heads/tycho-indexer/commit/f5783c9eed975fa9020ef7b0f82e4ee0b41ce25d))
* update substreams lockfile for rocketpool ([f318019](https://github.com/propeller-heads/tycho-indexer/commit/f318019c210b492c064408e96cf2f9815620060c))
* update substreams.yaml to be correctly parsed ([80f0c08](https://github.com/propeller-heads/tycho-indexer/commit/80f0c0837fdeaf1da8d484483f3eef31b6c96c33))
* Update tick attribute to use i32 to_be_bytes ([c69e0f1](https://github.com/propeller-heads/tycho-indexer/commit/c69e0f19a03ae291e46fa143b19b8639f979562f))
* use first tx in starting block for component creation ([163e213](https://github.com/propeller-heads/tycho-indexer/commit/163e2131f443915f4aa884d471f97c7d0e62e81d))
* use raw pool fee ([8dcbb0e](https://github.com/propeller-heads/tycho-indexer/commit/8dcbb0e55267eb7c3afec307488525e14509768b))
* use verified initial state, run handlers on creation block ([9ef3b74](https://github.com/propeller-heads/tycho-indexer/commit/9ef3b743a69c04e7196c348707795c9ac437faed))


### Reverts

* remove reth_collateral_target — doesn't affect swap output ([0d5e986](https://github.com/propeller-heads/tycho-indexer/commit/0d5e9869521ce74cedc939bc9775eac1bb4e2577))

## [0.267.0](https://github.com/propeller-heads/tycho-indexer/compare/0.266.0...0.267.0) (2026-04-24)


### Features

* **tracing:** add more debug spans ([e6f52ad](https://github.com/propeller-heads/tycho-indexer/commit/e6f52adc0f52b6705982db225cd179d274c76daa))
* **tracing:** add more debug spans ([#949](https://github.com/propeller-heads/tycho-indexer/issues/949)) ([853ca1a](https://github.com/propeller-heads/tycho-indexer/commit/853ca1a0d45ac2e72f092d192d6bff5efeb19551))

## [0.266.0](https://github.com/propeller-heads/tycho-indexer/compare/0.265.0...0.266.0) (2026-04-24)


### Features

* add workflow to detect deps with vulnerabilities ([7bd4199](https://github.com/propeller-heads/tycho-indexer/commit/7bd419975d1a57877fd2a957faa4998cdd16488b))
* add workflow to detect deps with vulnerabilities ([#917](https://github.com/propeller-heads/tycho-indexer/issues/917)) ([f878c36](https://github.com/propeller-heads/tycho-indexer/commit/f878c365b56ce27c5497c33e9f6a17cc79fcf2cf))


### Bug Fixes

* add cargo audit exception for tracing-subscriber ([07e8886](https://github.com/propeller-heads/tycho-indexer/commit/07e8886c116b57c7d11328735abea6a298e608f4))
* bump dependency with vulnerability ([a4164b9](https://github.com/propeller-heads/tycho-indexer/commit/a4164b960ace5de8ba921c190cbc9b15a2e71ce1))
* ignore doctests ([6c9cbbe](https://github.com/propeller-heads/tycho-indexer/commit/6c9cbbe4aad441858b85586dbf8a76f4bd4498a4))
* remove unused dependencies ([ea46bad](https://github.com/propeller-heads/tycho-indexer/commit/ea46bad476c285cec4e06e8ee9d71a9433c96fec))
* update alloy version with known vulnerabilities ([1cd4231](https://github.com/propeller-heads/tycho-indexer/commit/1cd423107b631171952c79ee6fa91e5cb863ec38))

## [0.265.0](https://github.com/propeller-heads/tycho-indexer/compare/0.264.2...0.265.0) (2026-04-24)


### Features

* add pipeline to run cargo update periodically ([#937](https://github.com/propeller-heads/tycho-indexer/issues/937)) ([0a864a1](https://github.com/propeller-heads/tycho-indexer/commit/0a864a12987362ea811751833807156d122c22ca))
* add pipeline to update dependencies ([de20fa0](https://github.com/propeller-heads/tycho-indexer/commit/de20fa0d719b3536b5b1a8c8833852e28d1ba717))
* pin too-recent crates to latest safe version instead of old version ([40f776c](https://github.com/propeller-heads/tycho-indexer/commit/40f776c63a66a0f057e65fdc0ce835ff3b20ac2b))


### Bug Fixes

* exclude Cargo.lock from substreams CI trigger ([c47de9c](https://github.com/propeller-heads/tycho-indexer/commit/c47de9cc5a68d26dd51996ac11ed7ac70d44112e))
* scope cargo-update PR to only Cargo.lock files ([f0dd6f4](https://github.com/propeller-heads/tycho-indexer/commit/f0dd6f4ba31d5e7604cb8823f3f9e5e9a7a3e7b2))

## [0.264.2](https://github.com/propeller-heads/tycho-indexer/compare/0.264.1...0.264.2) (2026-04-23)


### Bug Fixes

* Don't allow Curve tokens to be zero address ([5648058](https://github.com/propeller-heads/tycho-indexer/commit/56480585f5aefa32def0d45234f143976f0d58ec))
* Don't allow Curve tokens to be zero address ([#943](https://github.com/propeller-heads/tycho-indexer/issues/943)) ([fb31205](https://github.com/propeller-heads/tycho-indexer/commit/fb31205095dedc8100ac362ff65c34429d8adf04))

## [0.264.1](https://github.com/propeller-heads/tycho-indexer/compare/0.264.0...0.264.1) (2026-04-22)


### Bug Fixes

* **docker:** bump Rust base image to 1.91 to support cargo-chef ([0a5bebd](https://github.com/propeller-heads/tycho-indexer/commit/0a5bebd73dcbe08f4ce161d1d9beb64ab0d63b3e))
* **docker:** copy extractors.yaml from build context to avoid kaniko path bug ([365b7dd](https://github.com/propeller-heads/tycho-indexer/commit/365b7ddd2a2e887fe60b74c25ae072433ec9effc))
* **docker:** fix build workflows ([#936](https://github.com/propeller-heads/tycho-indexer/issues/936)) ([fa94247](https://github.com/propeller-heads/tycho-indexer/commit/fa94247a6f194f52f76696adb4d7f273a4f4fa88))

## [0.264.0](https://github.com/propeller-heads/tycho-indexer/compare/0.263.3...0.264.0) (2026-04-22)


### Features

* add unichain-curve clone integration test config ([a9aab7a](https://github.com/propeller-heads/tycho-indexer/commit/a9aab7aa07793d6786edbe1877a13ec71a57ca43)), closes [#423](https://github.com/propeller-heads/tycho-indexer/issues/423)


### Bug Fixes

* build tycho-indexer from monorepo source instead of GitHub clone ([2bd72fe](https://github.com/propeller-heads/tycho-indexer/commit/2bd72fef2dc60a22a309b1ede986552d9a8b423b))
* clone and other chain protocol tests CI running ([f2593a2](https://github.com/propeller-heads/tycho-indexer/commit/f2593a23cc3a1dfb8d262c6a2a36ae11e93092cc))
* clone and other chain protocol tests CI running ([#918](https://github.com/propeller-heads/tycho-indexer/issues/918)) ([9f62b96](https://github.com/propeller-heads/tycho-indexer/commit/9f62b961f32adcb3ec3ca4d1dd6046c211bdc2d6))
* handle missing git in protocol-testing build script ([038e441](https://github.com/propeller-heads/tycho-indexer/commit/038e44167d9b8602e14cba9290d11416c1b534a6))
* install forge deps in Dockerfile and add build-specific dockerignore ([947db32](https://github.com/propeller-heads/tycho-indexer/commit/947db327d75907aab52b0fdb4837aba551223152))
* match Docker image directory layout to test runner expectations ([5f311d9](https://github.com/propeller-heads/tycho-indexer/commit/5f311d935bebbedffbf52fc74f8d76d92a0339cf))
* update docker-compose path in substreams-docker-single action ([3a72d91](https://github.com/propeller-heads/tycho-indexer/commit/3a72d912d6ea29f0e61b265ca922b45fe7559233))
* use --no-git flag for forge install ([3e96aac](https://github.com/propeller-heads/tycho-indexer/commit/3e96aacdca6cc0b1f6a42a07d4684f53a85fe8ee))

## [0.263.3](https://github.com/propeller-heads/tycho-indexer/compare/0.263.2...0.263.3) (2026-04-22)


### Bug Fixes

* **cd:** pin Rust image and fix if-condition syntax on reusable workflow jobs ([e50400a](https://github.com/propeller-heads/tycho-indexer/commit/e50400a07effca18dd3d3a8e4f983fd053b33926))
* **docker:** copy extractors.yaml from build context instead of builder stage ([705f6ab](https://github.com/propeller-heads/tycho-indexer/commit/705f6ab6bccc7a582ac24688cdd0b6f81acf32a8))
* **docker:** work around Kaniko path resolution bug for extractors.yaml ([12d646b](https://github.com/propeller-heads/tycho-indexer/commit/12d646b05faed7bb5e938a81dc234945165a2037))
* **integration-test:** Fix block processing latency metric ([#931](https://github.com/propeller-heads/tycho-indexer/issues/931)) ([9142284](https://github.com/propeller-heads/tycho-indexer/commit/91422840111dffb64e82b4fefffcc8214d6c189d))
* **integration-test:** make RPC poll attempts and interval configurable ([0a53997](https://github.com/propeller-heads/tycho-indexer/commit/0a53997b82b65b7bde80f8e317ae04ecdd93f705))
* **integration-test:** poll RPC until it reaches update block number ([4e271cf](https://github.com/propeller-heads/tycho-indexer/commit/4e271cf8b034a06c447c21b8610f4aecfdc3a940))
* **integration-test:** separate RFQ and protocol update channels ([e3183ca](https://github.com/propeller-heads/tycho-indexer/commit/e3183cae880bcd830bc871823c241b84d49c1a85))

## [0.263.2](https://github.com/propeller-heads/tycho-indexer/compare/0.263.1...0.263.2) (2026-04-22)

## [0.263.1](https://github.com/propeller-heads/tycho-indexer/compare/0.263.0...0.263.1) (2026-04-21)


### Bug Fixes

* **tycho-client-py:** pass --chain to CLI subprocess, add polygon ([1e5ff9f](https://github.com/propeller-heads/tycho-indexer/commit/1e5ff9f01e0686a69f8097aab5d6f54db2015368))
* update cli and python clients ([#908](https://github.com/propeller-heads/tycho-indexer/issues/908)) ([9ca8523](https://github.com/propeller-heads/tycho-indexer/commit/9ca85233bd5071aa62d565889a14f2e97733b7a8))

## [0.263.0](https://github.com/propeller-heads/tycho-indexer/compare/0.262.0...0.263.0) (2026-04-21)


### Features

* aerodrome v1 simulation ([#928](https://github.com/propeller-heads/tycho-indexer/issues/928)) ([a7bf21e](https://github.com/propeller-heads/tycho-indexer/commit/a7bf21e67e6ca6ade92992e316242ceb3920c5a4))
* init aerodrome_v1 simulation ([e05a893](https://github.com/propeller-heads/tycho-indexer/commit/e05a89326effc5cfe735ca54473f784064202952))
* support stable curve swaps and raw custom fee semantics ([09ac2c5](https://github.com/propeller-heads/tycho-indexer/commit/09ac2c56a06b65437a761221984a0a64e97a931c))


### Bug Fixes

* address aerodrome v1 review ([a24cf00](https://github.com/propeller-heads/tycho-indexer/commit/a24cf006fba8567dda9ba4c8f39a213fc5e53cc5))
* aerodrome v1 decoder test ([e51cca1](https://github.com/propeller-heads/tycho-indexer/commit/e51cca1537b1f02a0f092ac592eacf0d965f68da))
* align rounding with onchain pools and add real pool tests ([40d721c](https://github.com/propeller-heads/tycho-indexer/commit/40d721c00be041728d87e52d765ebebef05d66a7))

## [0.262.0](https://github.com/propeller-heads/tycho-indexer/compare/0.261.1...0.262.0) (2026-04-21)


### Features

* **integration-test:** add --partial-blocks flag to opt in to flashblock stream updates ([ca84328](https://github.com/propeller-heads/tycho-indexer/commit/ca84328e89503ad0e4a929d22b572e769ccbd548))
* **integration-test:** add --partial-blocks flag to opt in to flashblock stream updates ([#921](https://github.com/propeller-heads/tycho-indexer/issues/921)) ([2fa2182](https://github.com/propeller-heads/tycho-indexer/commit/2fa2182933d03ec67304e314682c302d5decb06c))
* liquorice protocol integration ([#922](https://github.com/propeller-heads/tycho-indexer/issues/922)) ([45725c0](https://github.com/propeller-heads/tycho-indexer/commit/45725c0f3fb314a161befd52e0bd8cb465350e22))


### Bug Fixes

* provide all price levels for the component ([03469af](https://github.com/propeller-heads/tycho-indexer/commit/03469affffe43f5ba912be3744163b3ef52a8f03))

## [0.261.1](https://github.com/propeller-heads/tycho-indexer/compare/0.261.0...0.261.1) (2026-04-20)


### Bug Fixes

* build tycho-indexer from monorepo source instead of GitHub clone ([17ebed1](https://github.com/propeller-heads/tycho-indexer/commit/17ebed1b53ee63f15a1741879492fe9b6ab99216))
* build tycho-indexer from monorepo source instead of GitHub clone  ([#925](https://github.com/propeller-heads/tycho-indexer/issues/925)) ([326ce90](https://github.com/propeller-heads/tycho-indexer/commit/326ce905aa37213cd94b134b3ab7df146a7fd9eb))
* checkout PR branch in pull_request_target workflow ([fbea963](https://github.com/propeller-heads/tycho-indexer/commit/fbea9638ff94cdfb5da3da9ef6da2b2195e3d87b))

## [0.261.0](https://github.com/propeller-heads/tycho-indexer/compare/0.260.0...0.261.0) (2026-04-20)


### Features

* Add Fluid V1 DEX indexing and simulation ([#919](https://github.com/propeller-heads/tycho-indexer/issues/919)) ([279e456](https://github.com/propeller-heads/tycho-indexer/commit/279e456b68aef5680a09379a07584e6d6c489313))
* **protocol-testing:** register fluid_v1 native decoder and add integration test ([cff34c1](https://github.com/propeller-heads/tycho-indexer/commit/cff34c17328eec6a8236d8998d832b527364ffaa))
* **substreams:** add ethereum-fluid indexer ([668b47e](https://github.com/propeller-heads/tycho-indexer/commit/668b47ef6979fe896bdd48047e3c0af598f87395))

## [0.260.0](https://github.com/propeller-heads/tycho-indexer/compare/0.259.3...0.260.0) (2026-04-20)


### Features

* **execution:** add missing files from migration ([64c5267](https://github.com/propeller-heads/tycho-indexer/commit/64c5267ac9474299c53d5c133fbd2e54df50349d))
* **execution:** add missing files from migration ([#915](https://github.com/propeller-heads/tycho-indexer/issues/915)) ([c8ea624](https://github.com/propeller-heads/tycho-indexer/commit/c8ea62463654a60f176751327a25814699a29fb0))

## [0.259.3](https://github.com/propeller-heads/tycho-indexer/compare/0.259.2...0.259.3) (2026-04-20)


### Bug Fixes

* proto import paths and root_path detection ([2556966](https://github.com/propeller-heads/tycho-indexer/commit/2556966cdc306d920e2a1953ee6ebbfb9dddd35d))
* proto import paths and root_path detection ([#914](https://github.com/propeller-heads/tycho-indexer/issues/914)) ([bf00f14](https://github.com/propeller-heads/tycho-indexer/commit/bf00f14d9812d60d0409aa674b83416d521681f4))
* update substreams-check action paths for monorepo layout ([8027757](https://github.com/propeller-heads/tycho-indexer/commit/80277577925fb64526be266ff59a8612232f561a))

## [0.259.2](https://github.com/propeller-heads/tycho-indexer/compare/0.259.1...0.259.2) (2026-04-17)


### Bug Fixes

* remove unused substreams CLI from Docker build stage ([20d167f](https://github.com/propeller-heads/tycho-indexer/commit/20d167f524fbee392371fd5355e71ced9fc99fe6))

## [0.259.1](https://github.com/propeller-heads/tycho-indexer/compare/0.259.0...0.259.1) (2026-04-17)


### Bug Fixes

* repair integration test configs from monorepo assembly ([c419458](https://github.com/propeller-heads/tycho-indexer/commit/c4194582a223de3a2914df8972b64c30feebcb43))
* resolve monorepo assembly issues ([238f95f](https://github.com/propeller-heads/tycho-indexer/commit/238f95f6294ffe4cf17d7d49fc272a3a98de4b6e))
* resolve pre-existing clippy and rustdoc issues ([46451fb](https://github.com/propeller-heads/tycho-indexer/commit/46451fb490589aeeb88b796933fc81ea1c6587ae))
* update quickstart example to current tycho-execution API ([085350d](https://github.com/propeller-heads/tycho-indexer/commit/085350d6eafef9dc70f2ff56308ad0cac9a2ce68))

## [0.158.0](https://github.com/propeller-heads/tycho-indexer/compare/0.157.4...0.158.0) (2026-04-15)


### Features

* change version of github actions/checkout to v5 ([#654](https://github.com/propeller-heads/tycho-indexer/issues/654)) ([12348bc](https://github.com/propeller-heads/tycho-indexer/commit/12348bc05a4f9e272f6c0baaa9a7abc822c234f1))
* **partial blocks:** defer fetching snapshots of newly created component ([3603594](https://github.com/propeller-heads/tycho-indexer/commit/36035945541a524694ed87323af77d2402ee2e21))
* **tycho-client:** handle partial reverts ([64a50b5](https://github.com/propeller-heads/tycho-indexer/commit/64a50b52eee57f7a6c48c205a1292c0f0ac68f5e))


### Bug Fixes

* **tycho-client:** fetch snapshots on full blocks only ([84beb19](https://github.com/propeller-heads/tycho-indexer/commit/84beb1981c56084d66646f87f57df5df97f138a1))
* **tycho-client:** fetch snapshots on full blocks only ([#855](https://github.com/propeller-heads/tycho-indexer/issues/855)) ([7ee7d97](https://github.com/propeller-heads/tycho-indexer/commit/7ee7d978c050334aba96de24e3060154a1065da7))

## [0.157.4](https://github.com/propeller-heads/tycho-indexer/compare/0.157.3...0.157.4) (2026-04-14)

## [0.157.3](https://github.com/propeller-heads/tycho-indexer/compare/0.157.2...0.157.3) (2026-04-09)


### Bug Fixes

* redact URL paths in RPC error messages to prevent API key leaks ([833aca8](https://github.com/propeller-heads/tycho-indexer/commit/833aca88ecb2011174064a871ab3f971a925d400))
* redact URL paths in RPC error messages to prevent API key leaks ([#906](https://github.com/propeller-heads/tycho-indexer/issues/906)) ([8c15eea](https://github.com/propeller-heads/tycho-indexer/commit/8c15eea88ff9c8ba6e4d7135c21b1c777e7a0763))

## [0.157.2](https://github.com/propeller-heads/tycho-indexer/compare/0.157.1...0.157.2) (2026-04-09)


### Bug Fixes

* update Solidity compilation commands to include EVM version ([61365be](https://github.com/propeller-heads/tycho-indexer/commit/61365bec2413bf91f188b6f838c5b16d846e8e45))
* update Solidity compilation commands to include EVM version ([#905](https://github.com/propeller-heads/tycho-indexer/issues/905)) ([cdaffbd](https://github.com/propeller-heads/tycho-indexer/commit/cdaffbd0c123abf51a3742cf6a0890b9c52feffd))

## [0.157.1](https://github.com/propeller-heads/tycho-indexer/compare/0.157.0...0.157.1) (2026-04-09)


### Bug Fixes

* reduce spammy logs ([5d67cbf](https://github.com/propeller-heads/tycho-indexer/commit/5d67cbfd494171500ca00e6f4b7c34e6324024c7))
* reduce spammy logs ([#904](https://github.com/propeller-heads/tycho-indexer/issues/904)) ([4e0d3f9](https://github.com/propeller-heads/tycho-indexer/commit/4e0d3f93cac388ad8d3555862825c93e2b2ec007))

## [0.157.0](https://github.com/propeller-heads/tycho-indexer/compare/0.156.0...0.157.0) (2026-04-09)


### Features

* **token-analyzer:** replace TraceCallDetector with EthCallDetector ([ab7f9b7](https://github.com/propeller-heads/tycho-indexer/commit/ab7f9b77d2468b072fb60275102a5ea81a175e0b))
* **token-analyzer:** replace TraceCallDetector with EthCallDetector ([#903](https://github.com/propeller-heads/tycho-indexer/issues/903)) ([df35692](https://github.com/propeller-heads/tycho-indexer/commit/df35692b80bb1025f2f6419987550836fc2941c4))

## [0.156.0](https://github.com/propeller-heads/tycho-indexer/compare/0.155.0...0.156.0) (2026-04-08)


### Features

* **token-analyzer:** expose settlement address as CLI arg ([7dd4ab7](https://github.com/propeller-heads/tycho-indexer/commit/7dd4ab79c767d329cc220d1dc999912e9c20a980))
* **token-analyzer:** implement EthCallDetector ([#898](https://github.com/propeller-heads/tycho-indexer/issues/898)) ([3c341ae](https://github.com/propeller-heads/tycho-indexer/commit/3c341ae89974c200d001fe7241f1d7a67f6545cb))
* **token-analyzer:** implement EthCallDetector using eth_call with state overrides ([bd64af6](https://github.com/propeller-heads/tycho-indexer/commit/bd64af66ed9be3a02b3c958519becc1b8fdd2755))
* **token-analyzer:** make settlement address configurable ([#900](https://github.com/propeller-heads/tycho-indexer/issues/900)) ([65209e8](https://github.com/propeller-heads/tycho-indexer/commit/65209e81d5a0805af52fd988562c0394724382cd))

## [0.155.0](https://github.com/propeller-heads/tycho-indexer/compare/0.154.0...0.155.0) (2026-04-08)


### Features

* **token-analyzer:** add Analyzer + Forwarder Solidity contracts ([00d4b53](https://github.com/propeller-heads/tycho-indexer/commit/00d4b538ca3df62ea9836d77610725de763cafc2))
* **token-analyzer:** add Analyzer + Forwarder Solidity contracts ([#897](https://github.com/propeller-heads/tycho-indexer/issues/897)) ([eb371fa](https://github.com/propeller-heads/tycho-indexer/commit/eb371fab3843d77e7f7d8669bbee9a1c1c79bcec))


### Bug Fixes

* **token-analyzer:** handle non-standard transfer() in Analyzer and Forwarder ([694fe64](https://github.com/propeller-heads/tycho-indexer/commit/694fe64bf6647716510e90f600d90f5a764ee8d4))

## [0.154.0](https://github.com/propeller-heads/tycho-indexer/compare/0.153.1...0.154.0) (2026-04-07)


### Features

* Add Chain::Polygon to tycho-common ([#901](https://github.com/propeller-heads/tycho-indexer/issues/901)) ([4e2193c](https://github.com/propeller-heads/tycho-indexer/commit/4e2193c6d8e6a187451e340074ddc83da96feb75))

## [0.153.1](https://github.com/propeller-heads/tycho-indexer/compare/0.153.0...0.153.1) (2026-04-01)


### Bug Fixes

* resume from block number instead of cursor on cold restart ([7a4ce03](https://github.com/propeller-heads/tycho-indexer/commit/7a4ce0376e9df91bd16674fc7216275a19a356a0))
* resume from block number instead of cursor on cold restart ([#893](https://github.com/propeller-heads/tycho-indexer/issues/893)) ([2aca9f2](https://github.com/propeller-heads/tycho-indexer/commit/2aca9f25a088a7af7bc34baebb5505229f16eb96))
* return error instead of panicking on block fetch failure ([4754b1a](https://github.com/propeller-heads/tycho-indexer/commit/4754b1a5ad4ef735de8072b948bcb3d8ff09868f))

## [0.153.0](https://github.com/propeller-heads/tycho-indexer/compare/0.152.4...0.153.0) (2026-04-01)


### Features

* add blocklist filter ([7e01876](https://github.com/propeller-heads/tycho-indexer/commit/7e01876af915946080a99e21d568ac943ebbcd65))
* add blocklist filter ([#886](https://github.com/propeller-heads/tycho-indexer/issues/886)) ([93913cb](https://github.com/propeller-heads/tycho-indexer/commit/93913cb2a0e064e22b509abb8415e5ae770b3d41))
* add cli blocklist option ([186daf1](https://github.com/propeller-heads/tycho-indexer/commit/186daf1e1adfce7adb77f339ac9dfacaab1c74c2))
* **tycho-client:** use TOML as input for the blocklist-config CLI flag ([706a0ed](https://github.com/propeller-heads/tycho-indexer/commit/706a0ed9ea89ef395b813bd1348c3c90abed14c1))


### Bug Fixes

* do not update ComponentFilter with set Ids ([93ba9c1](https://github.com/propeller-heads/tycho-indexer/commit/93ba9c18af645a4e28a4844d22cacd72ebe2904f))

## [0.152.4](https://github.com/propeller-heads/tycho-indexer/compare/0.152.3...0.152.4) (2026-03-25)

## [0.152.3](https://github.com/propeller-heads/tycho-indexer/compare/0.152.2...0.152.3) (2026-03-23)


### Bug Fixes

* handle null storageKeys in eth_createAccessList response ([586ec92](https://github.com/propeller-heads/tycho-indexer/commit/586ec92e6e5726db0faf1662e44503004ffd0014))
* handle null storageKeys in eth_createAccessList response ([#881](https://github.com/propeller-heads/tycho-indexer/issues/881)) ([689a285](https://github.com/propeller-heads/tycho-indexer/commit/689a2858ce0ce37c412bef12b5a22ee63cad8738))

## [0.152.2](https://github.com/propeller-heads/tycho-indexer/compare/0.152.1...0.152.2) (2026-03-23)


### Bug Fixes

* resolve cargo audit vulnerabilities ([04fa9eb](https://github.com/propeller-heads/tycho-indexer/commit/04fa9ebebc1c5cc4b5669afa035f2e09748708b2))
* resolve cargo audit vulnerabilities ([#883](https://github.com/propeller-heads/tycho-indexer/issues/883)) ([7a4fb8f](https://github.com/propeller-heads/tycho-indexer/commit/7a4fb8f6c500116087e664ae63ab3988594e00fe))

## [0.152.1](https://github.com/propeller-heads/tycho-indexer/compare/0.152.0...0.152.1) (2026-03-20)


### Bug Fixes

* add reorg logs ([fb02bdc](https://github.com/propeller-heads/tycho-indexer/commit/fb02bdc33f8480fd62816201b5cdacd78b60ce92))

## [0.152.0](https://github.com/propeller-heads/tycho-indexer/compare/0.151.0...0.152.0) (2026-03-19)


### Features

* add user_identity label to websocket subscriptions gauge ([4dd2537](https://github.com/propeller-heads/tycho-indexer/commit/4dd2537acb5377562b1a44044deca36b2473cbe9))


### Bug Fixes

* reduce metrics cardinality by removing UUID labels ([8c6c60c](https://github.com/propeller-heads/tycho-indexer/commit/8c6c60cf1f55e6194b0f449875e82a8a6b767384))

## [0.151.0](https://github.com/propeller-heads/tycho-indexer/compare/0.150.0...0.151.0) (2026-03-19)


### Features

* add run-ci skill and nextest wrapper script ([aae965f](https://github.com/propeller-heads/tycho-indexer/commit/aae965f024509206f83ad114d3b8ab324286c573))


### Bug Fixes

* update Python client tests and DTOs for chain-in-body API ([03f1c3e](https://github.com/propeller-heads/tycho-indexer/commit/03f1c3e8c3ddab8615e794d25dcf625b93c7af1a))

## [0.150.0](https://github.com/propeller-heads/tycho-indexer/compare/0.149.0...0.150.0) (2026-03-18)


### Features

* add max_retries option to CLI arguments for startup attempts ([2f8202c](https://github.com/propeller-heads/tycho-indexer/commit/2f8202cd465f92048036e5c86bdc228296737e94))
* add max_retries option to CLI arguments for startup attempts ([#858](https://github.com/propeller-heads/tycho-indexer/issues/858)) ([3f1e33b](https://github.com/propeller-heads/tycho-indexer/commit/3f1e33b7e3e6999f2d9399481da33b43e7b8ee4d))

## [0.149.0](https://github.com/propeller-heads/tycho-indexer/compare/0.148.0...0.149.0) (2026-03-16)


### Features

* add allow_historical plan restriction to reject historical RPC queries ([aa362cb](https://github.com/propeller-heads/tycho-indexer/commit/aa362cb329d38c9664f8e6b3978d561b369780de))
* add allow_historical plan restriction to reject historical RPC queries ([#872](https://github.com/propeller-heads/tycho-indexer/issues/872)) ([82611a1](https://github.com/propeller-heads/tycho-indexer/commit/82611a11d5a079032a5d6e1d1f02350f2ad3a659))

## [0.148.0](https://github.com/propeller-heads/tycho-indexer/compare/0.147.2...0.148.0) (2026-03-13)


### Features

* expose RPC batching config as CLI arguments ([00996e4](https://github.com/propeller-heads/tycho-indexer/commit/00996e43ddada753d49acd95f7ad3712fe93d837))
* expose RPC batching config as CLI arguments ([#876](https://github.com/propeller-heads/tycho-indexer/issues/876)) ([78e7acb](https://github.com/propeller-heads/tycho-indexer/commit/78e7acba4169912462688489468713ea0c2f00a7))


### Bug Fixes

* make batching args optional, keep default client behavior ([ff7956c](https://github.com/propeller-heads/tycho-indexer/commit/ff7956c6d109b18303def95782d60ce2379f1e9d))
* panic on startup if storage slot batch size is 0 ([363a406](https://github.com/propeller-heads/tycho-indexer/commit/363a406c2e190da2ccbdcaeaa921390757bc4943))

## [0.147.2](https://github.com/propeller-heads/tycho-indexer/compare/0.147.1...0.147.2) (2026-03-11)


### Bug Fixes

* fall back to default plan for unknown X-User-Plan values ([ea687df](https://github.com/propeller-heads/tycho-indexer/commit/ea687dfd06a46e9aefc680d955b1942b9558b8a0))
* fall back to default plan for unknown X-User-Plan values ([#875](https://github.com/propeller-heads/tycho-indexer/issues/875)) ([97827ee](https://github.com/propeller-heads/tycho-indexer/commit/97827eebe1d94de244a1e53a621000e404e62722))

## [0.147.1](https://github.com/propeller-heads/tycho-indexer/compare/0.147.0...0.147.1) (2026-03-09)


### Bug Fixes

* skip claude-review job instead of failing for non-review comments ([f918d52](https://github.com/propeller-heads/tycho-indexer/commit/f918d528e2909c016f3d67ed9d4af050525b2e6e))
* skip claude-review job instead of failing for non-review comments ([#870](https://github.com/propeller-heads/tycho-indexer/issues/870)) ([281adab](https://github.com/propeller-heads/tycho-indexer/commit/281adab37b51be3b1e05d829ca8cc4fa764a4a63))

## [0.147.0](https://github.com/propeller-heads/tycho-indexer/compare/0.146.0...0.147.0) (2026-03-09)


### Features

* extend plan restrictions to all RPC endpoints ([f5d3630](https://github.com/propeller-heads/tycho-indexer/commit/f5d36304bcfc47f67c0fd6fab6ee5fefd3b49372))
* Implement plan-based request restrictions ([#868](https://github.com/propeller-heads/tycho-indexer/issues/868)) ([9a186ad](https://github.com/propeller-heads/tycho-indexer/commit/9a186ad36db56ce812673b2ce3563cc4194b06ea))
* replace ServerRpcConfig with per-plan YAML-based restrictions ([7a8c074](https://github.com/propeller-heads/tycho-indexer/commit/7a8c0743deb6abeafa0548f93984a32cc37f9516))


### Bug Fixes

* filter protocol_systems by plan's allowed protocols ([16788d5](https://github.com/propeller-heads/tycho-indexer/commit/16788d5bdd3f976b9e0d2bc88850344dbd0ae839))

## [0.146.0](https://github.com/propeller-heads/tycho-indexer/compare/0.145.2...0.146.0) (2026-03-06)


### Features

* dynamic dci skip condition ([#863](https://github.com/propeller-heads/tycho-indexer/issues/863)) ([cfe164c](https://github.com/propeller-heads/tycho-indexer/commit/cfe164cc7b88cba943030b1356731c3aa6016467))
* Dynamic DCI skip condition for entrypoint requests ([3645481](https://github.com/propeller-heads/tycho-indexer/commit/3645481cdab811fbe0b5388baa18d2aca378750f))


### Bug Fixes

* add legacy DCI fallback for backward compatibility ([784df4a](https://github.com/propeller-heads/tycho-indexer/commit/784df4add21d63c3bb7d995c702e5b18c2398aa2))
* Filter dci_protocols against DB-returned protocol systems ([45027a9](https://github.com/propeller-heads/tycho-indexer/commit/45027a91db4472cde04b403a7e71967c3fc2b1d6))
* Resolve clippy::iter_kv_map lints for nightly 1.95 ([aa47e4e](https://github.com/propeller-heads/tycho-indexer/commit/aa47e4e082e8c3cac49c2b3cea52f7be89b9e560))
* serve protocol_systems from config instead of DB ([2f6be95](https://github.com/propeller-heads/tycho-indexer/commit/2f6be95d30d2601d5934a27caaab49c5ff664562))
* Use PaginationLimits trait for protocol_systems page size ([dd7c56a](https://github.com/propeller-heads/tycho-indexer/commit/dd7c56a91fa812df9668541d5e02d64ce1500056))
* Warn when protocol_systems response is truncated by pagination ([117a9c9](https://github.com/propeller-heads/tycho-indexer/commit/117a9c9a9e5cca133d0cf3ce4db067a97efcfca4))

## [0.145.2](https://github.com/propeller-heads/tycho-indexer/compare/0.145.1...0.145.2) (2026-03-04)


### Bug Fixes

* **swagger:** correct AccountUpdate.address schema annotation ([7bd1b0c](https://github.com/propeller-heads/tycho-indexer/commit/7bd1b0c30aaf5f5bd7da6fef474068afd2c17fd2))
* **swagger:** correct AccountUpdate.address schema type from Vec<String> to String ([#866](https://github.com/propeller-heads/tycho-indexer/issues/866)) ([3cb5b82](https://github.com/propeller-heads/tycho-indexer/commit/3cb5b82a3832f44949bca3bae26f6679ae68e2dd))

## [0.145.1](https://github.com/propeller-heads/tycho-indexer/compare/0.145.0...0.145.1) (2026-03-03)


### Bug Fixes

* Use PR author instead of merged_by for release author ([7496350](https://github.com/propeller-heads/tycho-indexer/commit/74963506b1f3493a8f7beda4c6af4401193ebca4))

## [0.145.0](https://github.com/propeller-heads/tycho-indexer/compare/0.144.3...0.145.0) (2026-03-02)


### Features

* add a chack that audit.json is valid ([56087a1](https://github.com/propeller-heads/tycho-indexer/commit/56087a1420b51f5053f0742e19b87f7956ab66d7))
* add cargo audit pipeline ([b9607bf](https://github.com/propeller-heads/tycho-indexer/commit/b9607bfe809e362d23318517aeb06afd439963c5))
* add cargo audit pipeline ([#861](https://github.com/propeller-heads/tycho-indexer/issues/861)) ([817db0b](https://github.com/propeller-heads/tycho-indexer/commit/817db0bd4986db0324e8a1e8340ec86f61b375a9))
* add check for unused dependencies ([5891864](https://github.com/propeller-heads/tycho-indexer/commit/5891864ee7207e6ab3a9b73421e651f072c59d19))
* update versions to remove vulnerabilities ([97cfe26](https://github.com/propeller-heads/tycho-indexer/commit/97cfe2662fe59cb52b6a6ef41a29ad0fc1982fc8))


### Bug Fixes

* add action to install cargo-audit ([e7e9ff1](https://github.com/propeller-heads/tycho-indexer/commit/e7e9ff112cfbb71ed98bc1a39f9ec8b6db554e68))
* replace with fatster job ([9f9ac53](https://github.com/propeller-heads/tycho-indexer/commit/9f9ac5327aeed38bfe67d75de27f12388953d5b6))
* update packages with vulnerabilities ([d968a9c](https://github.com/propeller-heads/tycho-indexer/commit/d968a9c679c7913da03460bf89e9e72f694c5d47))

## [0.144.3](https://github.com/propeller-heads/tycho-indexer/compare/0.144.2...0.144.3) (2026-03-02)

## [0.144.2](https://github.com/propeller-heads/tycho-indexer/compare/0.144.1...0.144.2) (2026-03-02)


### Bug Fixes

* **lint:** fix clippy lints ([88d504a](https://github.com/propeller-heads/tycho-indexer/commit/88d504af29ba421ab7ebdfd27887570460d9c6d2))
* **lint:** fix clippy lints ([#864](https://github.com/propeller-heads/tycho-indexer/issues/864)) ([c1efdd8](https://github.com/propeller-heads/tycho-indexer/commit/c1efdd80143c00b5d23b5cafce7afe9be17a6d90))

## [0.144.1](https://github.com/propeller-heads/tycho-indexer/compare/0.144.0...0.144.1) (2026-02-24)


### Bug Fixes

* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering ([3f5f961](https://github.com/propeller-heads/tycho-indexer/commit/3f5f96194c63d1d740aa9bd5605d2a4bf04d3d48))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering ([#859](https://github.com/propeller-heads/tycho-indexer/issues/859)) ([656bb5c](https://github.com/propeller-heads/tycho-indexer/commit/656bb5c60b3403d4b4d80f405acc521f2d2eec81))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering (fix test case) ([cd932b3](https://github.com/propeller-heads/tycho-indexer/commit/cd932b32d655a8bcf9c058f2763b1239f333f9f7))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering (lint fix) ([2524339](https://github.com/propeller-heads/tycho-indexer/commit/2524339fd4cabfb408e94f0926b93404874047c5))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering (linter fix) ([b7e37d2](https://github.com/propeller-heads/tycho-indexer/commit/b7e37d274f67b6d8e2238247d84d9970f0e9ef52))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering (no gateway sort) ([9e8de35](https://github.com/propeller-heads/tycho-indexer/commit/9e8de35f520fabb667b3cfc0ed248c78885d9504))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering (part 2) ([3bf64e0](https://github.com/propeller-heads/tycho-indexer/commit/3bf64e0046b7d37d7ea471aef0977a583b2d9c84))
* [#856](https://github.com/propeller-heads/tycho-indexer/issues/856) pc token ordering (trigger fix) ([32abd2d](https://github.com/propeller-heads/tycho-indexer/commit/32abd2d83b326b3dc48a08ecc6100698ac67eafd))

## [0.144.0](https://github.com/propeller-heads/tycho-indexer/compare/0.143.1...0.144.0) (2026-02-20)


### Features

* Blanket implementation for legacy trait ([f54ae4b](https://github.com/propeller-heads/tycho-indexer/commit/f54ae4b189e9701d2addacefdcb2beda6c755377))
* Blanket implementation query_pool_swap ([bd79bca](https://github.com/propeller-heads/tycho-indexer/commit/bd79bcaa742de5c2e3b32761e3f33fa7e559b79c))
* expose parameters ([3f9a838](https://github.com/propeller-heads/tycho-indexer/commit/3f9a838c56336a00d0729899d24d5d3578e1bf37))
* Generic protocol component to dto conversion. ([1d3c826](https://github.com/propeller-heads/tycho-indexer/commit/1d3c826aee1bb1ad23f4ce49fc2103ccb6cd4dce))
* get_token helper on ProtocolComponent ([0a7b006](https://github.com/propeller-heads/tycho-indexer/commit/0a7b00668b991d96bd32c2760c723396b4e7c415))
* implement default quotable pairs ([ade05b6](https://github.com/propeller-heads/tycho-indexer/commit/ade05b60e6bde4f4bfb6b09290085f6dd0ad8a47))
* include tokens in protocol component ([9256a41](https://github.com/propeller-heads/tycho-indexer/commit/9256a41913bc9a88b5ae138e1608fe40720f1b14))
* Misc fixes to SwapQuoter & docs ([bccd3a4](https://github.com/propeller-heads/tycho-indexer/commit/bccd3a4606938482f9d31a82dd895347bc35d14f))
* quotable pair returns full tokens ([cc7ade4](https://github.com/propeller-heads/tycho-indexer/commit/cc7ade47497b23a09c0d9b002be0973b0493c5b0))
* Simplify transition error struct ([713421d](https://github.com/propeller-heads/tycho-indexer/commit/713421dbc60a6ddfee8eb2a35ff3dcda0b78d6f1))
* Support for fixed out quotes ([79098ab](https://github.com/propeller-heads/tycho-indexer/commit/79098aba139551169910593247931dcfea88b69a))
* v1 quote interface proposal ([3952d29](https://github.com/propeller-heads/tycho-indexer/commit/3952d295938fd912b4a73f93a276843de3ab7f49))
* v1 quote interface proposal ([#837](https://github.com/propeller-heads/tycho-indexer/issues/837)) ([a448aae](https://github.com/propeller-heads/tycho-indexer/commit/a448aae4151b263b02b1a35a6988fa97b118c98a))


### Bug Fixes

* delta_transition error type ([21b3d53](https://github.com/propeller-heads/tycho-indexer/commit/21b3d5315a775eb2663a70e50e2d49a7a45a81f8))
* force include new state in get_amount_out ([4e76170](https://github.com/propeller-heads/tycho-indexer/commit/4e76170c5738c54072e16e9c67465cd14c40499e))
* post rebase fixes ([e0123f8](https://github.com/propeller-heads/tycho-indexer/commit/e0123f850dc6c903a9bfe55fc5ccafe82a981128))

## [0.143.1](https://github.com/propeller-heads/tycho-indexer/compare/0.143.0...0.143.1) (2026-02-20)


### Bug Fixes

* remove depreceated models for BlockContractChanges and BlockEntityChanges ([#857](https://github.com/propeller-heads/tycho-indexer/issues/857)) ([35852c0](https://github.com/propeller-heads/tycho-indexer/commit/35852c0a58fdd470295fe61ce35642c65b562afd))

## [0.143.0](https://github.com/propeller-heads/tycho-indexer/compare/0.142.0...0.143.0) (2026-02-19)


### Features

* Add a public PricePoint struct ([49c5dde](https://github.com/propeller-heads/tycho-indexer/commit/49c5dde44dd12aa2b3b6a575add4f9189aad7447))
* Add a public PricePoint struct ([#819](https://github.com/propeller-heads/tycho-indexer/issues/819)) ([970dbf8](https://github.com/propeller-heads/tycho-indexer/commit/970dbf86807e3d94abeb25c5a818dba9830a3d22))

## [0.142.0](https://github.com/propeller-heads/tycho-indexer/compare/0.141.1...0.142.0) (2026-02-16)


### Features

* add block_type label to block processing time metric ([12127c5](https://github.com/propeller-heads/tycho-indexer/commit/12127c5a9d0fae2d6711d58854e3856d3bdd8f29))
* add metrics partial block metrics ([#850](https://github.com/propeller-heads/tycho-indexer/issues/850)) ([11f56d7](https://github.com/propeller-heads/tycho-indexer/commit/11f56d75c4b4f7341e1a2ab9d45aca75ac89775a))
* **extractor:** add partial block metrics for Grafana observability ([0ee6e49](https://github.com/propeller-heads/tycho-indexer/commit/0ee6e494c0cc7dc44c7f51e238ba93d3865dc350))

## [0.141.1](https://github.com/propeller-heads/tycho-indexer/compare/0.141.0...0.141.1) (2026-02-13)


### Bug Fixes

* allow merging partials with blocks of different hashes ([8177364](https://github.com/propeller-heads/tycho-indexer/commit/8177364c57d9392d4eb4b66454e883f83e5de54f))
* normalise block hash during partial merge ([25861aa](https://github.com/propeller-heads/tycho-indexer/commit/25861aab5ea57b2035c40b448ca6f2b3e5b0e977))
* Partial blocks support ([#847](https://github.com/propeller-heads/tycho-indexer/issues/847)) ([66c2e41](https://github.com/propeller-heads/tycho-indexer/commit/66c2e41eff3cf6c601b8f50236a258a9c4228f66))
* remove partial value from collect_and_process_full_block response ([1cb4c92](https://github.com/propeller-heads/tycho-indexer/commit/1cb4c9296eca987cf7fed7c97e5c026c3b0cb164))
* skip applying partial reverts to pending deltas buffer ([0130219](https://github.com/propeller-heads/tycho-indexer/commit/013021981deadc9a2b9234b3e9d46a8cc29a2993))
* skip inserting partial blocks on pending deltas buffer ([cc873f7](https://github.com/propeller-heads/tycho-indexer/commit/cc873f716fc244a157d66dd462fccaf6de59e234))

## [0.141.0](https://github.com/propeller-heads/tycho-indexer/compare/0.140.1...0.141.0) (2026-02-13)


### Features

* add configuration options for partial blocks, TVL inclusion, and compression ([d58c0ea](https://github.com/propeller-heads/tycho-indexer/commit/d58c0ea30183f7be8e08478f23993413dc7deb02))
* add python package configuration options for partial blocks, TVL inclusion, and compression ([#849](https://github.com/propeller-heads/tycho-indexer/issues/849)) ([4a40650](https://github.com/propeller-heads/tycho-indexer/commit/4a4065021bd82e08dcc4ed63a6df7c40783c99a7))

## [0.140.1](https://github.com/propeller-heads/tycho-indexer/compare/0.140.0...0.140.1) (2026-02-12)


### Bug Fixes

* stop requesting all snapshots everytime there is an update ([9409de5](https://github.com/propeller-heads/tycho-indexer/commit/9409de5938838ebbb41696d9c1f1aa9869bcdee9))
* stop requesting all snapshots everytime there is an update ([#848](https://github.com/propeller-heads/tycho-indexer/issues/848)) ([10cd5f4](https://github.com/propeller-heads/tycho-indexer/commit/10cd5f418f4bdcbab4fecca3deb094b9a9b9d546))

## [0.140.0](https://github.com/propeller-heads/tycho-indexer/compare/0.139.0...0.140.0) (2026-02-11)


### Features

* handle partial block messages in ExtractorRunner ([a63151c](https://github.com/propeller-heads/tycho-indexer/commit/a63151c1178fddb7079942df047175718efeced7))
* handle partial block messages in ExtractorRunner ([#844](https://github.com/propeller-heads/tycho-indexer/issues/844)) ([77e7a23](https://github.com/propeller-heads/tycho-indexer/commit/77e7a23d7e09247f1277bb2c4072c6175394647e))
* implement collect_full_block to drain partial buffer ([e9982aa](https://github.com/propeller-heads/tycho-indexer/commit/e9982aa8eadafa023c27d124162c642b681ae5e3))

## [0.139.0](https://github.com/propeller-heads/tycho-indexer/compare/0.138.0...0.139.0) (2026-02-09)


### Features

* **extractor:** mark revert blocks as partial when no full blocks are rolled back. ([131ad3d](https://github.com/propeller-heads/tycho-indexer/commit/131ad3d045decdc6f313a2cc3b7fa0d0b6b49149))
* **extractor:** purge partial block buffer on revert ([7eb11fb](https://github.com/propeller-heads/tycho-indexer/commit/7eb11fba467329179ea50682a2c0a2bc7f22c86c))
* revert handling for partial blocks ([#845](https://github.com/propeller-heads/tycho-indexer/issues/845)) ([74914fd](https://github.com/propeller-heads/tycho-indexer/commit/74914fd42d80f679fe476191dc8f7041221afaff))

## [0.138.0](https://github.com/propeller-heads/tycho-indexer/compare/0.137.0...0.138.0) (2026-02-09)


### Features

* add partial block buffer in the extractor ([#841](https://github.com/propeller-heads/tycho-indexer/issues/841)) ([ee6b5be](https://github.com/propeller-heads/tycho-indexer/commit/ee6b5be6b0cee9da5026c60b14b9fd4f3146d5d3))
* **extractor:** add memory tracking for PartialBlockBuffer ([7166b6b](https://github.com/propeller-heads/tycho-indexer/commit/7166b6b2616b4ef8910fe3c7634221213cb27dd4))
* **extractor:** add partial_block_buffer to manage accumulating partial blocks ([09ebc87](https://github.com/propeller-heads/tycho-indexer/commit/09ebc87edc8e60788495e530eddb66f408dceef6))
* **extractor:** add PartialBlockBuffer for accumulating partial blocks ([6483359](https://github.com/propeller-heads/tycho-indexer/commit/6483359040af2a189ced8614c73fa00cf332e752))
* **extractor:** improve handling of partial and full blocks in ProtocolExtractor ([384421a](https://github.com/propeller-heads/tycho-indexer/commit/384421a56d5d4d02d170a1d6cfcf4add7b01e0e0))
* **extractor:** refactor merge_partial to return updated BlockChanges and improve validation ([78dd3d1](https://github.com/propeller-heads/tycho-indexer/commit/78dd3d140e1fb07bcf501ea242c343d596eac08a))
* **extractor:** simplify PartialBlockBuffer by replacing it with an Option<BlockChanges> and integrate it into protocol extractor ([ad21437](https://github.com/propeller-heads/tycho-indexer/commit/ad214375dfe0b4b7c77cc53ac5176c50095b5d65))


### Bug Fixes

* **extractor:** simplify token insertion in PartialBlockBuffer ([507df3f](https://github.com/propeller-heads/tycho-indexer/commit/507df3f9935745c593397a8e1fe6cfc152e02c81))
* improve error message, comments, and extract helper in protocol_extractor ([301c9f6](https://github.com/propeller-heads/tycho-indexer/commit/301c9f6193e418b0140a8b88efff537870fd5a3d))
* update test expectations and entity names for MergeError::IdMismatch ([e22aa85](https://github.com/propeller-heads/tycho-indexer/commit/e22aa85ac53a7c014ca5cc2a1a567af5b1b0325a))

## [0.137.0](https://github.com/propeller-heads/tycho-indexer/compare/0.136.1...0.137.0) (2026-02-06)


### Features

* Notify Tycho SDK of new releases ([d86899f](https://github.com/propeller-heads/tycho-indexer/commit/d86899f5e25f72ddc9297631c33838ec6267ac59))
* Notify Tycho SDK of new releases ([#842](https://github.com/propeller-heads/tycho-indexer/issues/842)) ([e020d97](https://github.com/propeller-heads/tycho-indexer/commit/e020d970abaecd32ee0c9977837030bc9e8b9d6f))

## [0.136.1](https://github.com/propeller-heads/tycho-indexer/compare/0.136.0...0.136.1) (2026-02-05)

## [0.136.0](https://github.com/propeller-heads/tycho-indexer/compare/0.135.0...0.136.0) (2026-02-03)


### Features

* **block_history:** add error handling for partial block reverts and ensure latest block is a partial block ([e8e61fd](https://github.com/propeller-heads/tycho-indexer/commit/e8e61fd77adedd5d69815275818ded93507cb83c))
* **block_history:** add support for partial block handling and position determination and improve test coverage ([d8eacab](https://github.com/propeller-heads/tycho-indexer/commit/d8eacab821a54a9bcee964fb8bbfb8846a4740c7))
* **cli:** add partial_blocks flag to enable receiving incremental block updates ([cb27e64](https://github.com/propeller-heads/tycho-indexer/commit/cb27e64d3fc29af2280e5024a6dbb65a55b0dfad))
* client supports partial blocks ([#838](https://github.com/propeller-heads/tycho-indexer/issues/838)) ([7ec8b14](https://github.com/propeller-heads/tycho-indexer/commit/7ec8b145aa4a8e82aed52c4983d6841108a78fa5))
* **merge:** enhance merge logic to handle partial block validation and error reporting ([d77e22a](https://github.com/propeller-heads/tycho-indexer/commit/d77e22a9e78f21652dedb5731863790cdf48205c))
* **stream:** add send_partials flag to `TychoStreamBuilder` to enable receiving incremental block updates ([6c6a69a](https://github.com/propeller-heads/tycho-indexer/commit/6c6a69a83b94982d9ed39cc16b73d91685060798))
* **synchronizer:** add send_partials flag to `ProtocolStateSynchronizer` to enable incremental block updates ([1ed9e27](https://github.com/propeller-heads/tycho-indexer/commit/1ed9e2730ae4f4facae3ab8f514044a286a9e330))
* **synchronizer:** implement partial block handling and synchronization logic ([e7318e2](https://github.com/propeller-heads/tycho-indexer/commit/e7318e256162aa0ac770be0dc6bbb10e9cd82706))
* **synchronizer:** improve partial block handling by accepting full blocks as first messages and detecting block number increases instead of partial index decrease ([6661e76](https://github.com/propeller-heads/tycho-indexer/commit/6661e769996846b9dc9ec19858326152a369a129))

## [0.135.0](https://github.com/propeller-heads/tycho-indexer/compare/0.134.0...0.135.0) (2026-02-02)


### Features

* add partial block support to the ws ([#835](https://github.com/propeller-heads/tycho-indexer/issues/835)) ([e936191](https://github.com/propeller-heads/tycho-indexer/commit/e936191e0a3a829d40cbaf55e467614a81289cda))
* **blockchain:** add `is_partial` field to `BlockAggregatedChanges` ([156a92c](https://github.com/propeller-heads/tycho-indexer/commit/156a92c226c7cb2ba5d345cfd6333d794d5eb1d1))
* **common:** add is_partial method to check for partial block presence ([cea6371](https://github.com/propeller-heads/tycho-indexer/commit/cea63710ce9d44bbf5853d2df9c47a0e6ba76323))
* **dto:** add `is_partial` field to BlockChanges for partial block handling ([74d9631](https://github.com/propeller-heads/tycho-indexer/commit/74d9631db9d1b65a63eebea9f50092e4303c1a56))
* **dto:** add `partial_blocks` field to Subscribe command for partial block updates subscriptions ([8c67ff0](https://github.com/propeller-heads/tycho-indexer/commit/8c67ff0572838d559e4d67acae1395f4618282bb))
* **ws:** implement partial block filtering in subscription logic ([0ea7532](https://github.com/propeller-heads/tycho-indexer/commit/0ea7532d70d9d67fded35b918b9d69855dfe9a63))
* **ws:** rename partial_blocks parameter to send_partials and update related logic for block filtering ([66de802](https://github.com/propeller-heads/tycho-indexer/commit/66de80205047628f672ffaba6c305898742da927))
* **ws:** update block filtering logic for partial and revert subscriptions ([d862f04](https://github.com/propeller-heads/tycho-indexer/commit/d862f04a4e65371bb4f1335086b9d8b2e7c9b03f))

## [0.134.0](https://github.com/propeller-heads/tycho-indexer/compare/0.133.0...0.134.0) (2026-02-02)


### Features

* add optionnal partial block index to our block message structs ([bacca66](https://github.com/propeller-heads/tycho-indexer/commit/bacca66390cb0e29e3af5a0df0983d4e366ca6f6))
* add optionnal partial block index to our block message structs ([#834](https://github.com/propeller-heads/tycho-indexer/issues/834)) ([25d3623](https://github.com/propeller-heads/tycho-indexer/commit/25d3623346477f3605849cf8c91c0e8948959965))

## [0.133.0](https://github.com/propeller-heads/tycho-indexer/compare/0.132.2...0.133.0) (2026-01-30)


### Features

* bring back typetag derive to `ProtocolSim` trait ([#836](https://github.com/propeller-heads/tycho-indexer/issues/836)) ([aaaee59](https://github.com/propeller-heads/tycho-indexer/commit/aaaee590968aba1103d3bf1d9984bc10a7d08614))

## [0.132.2](https://github.com/propeller-heads/tycho-indexer/compare/0.132.1...0.132.2) (2026-01-28)


### Bug Fixes

* handle missing protocol component gracefully in PostgresGateway ([1074b45](https://github.com/propeller-heads/tycho-indexer/commit/1074b453bd33b2d350957aa07f83dc7a24cc7249))
* handle missing protocol component gracefully in PostgresGateway ([#833](https://github.com/propeller-heads/tycho-indexer/issues/833)) ([fbabc39](https://github.com/propeller-heads/tycho-indexer/commit/fbabc39fdbe4879a9528c8be855b07d774355112))

## [0.132.1](https://github.com/propeller-heads/tycho-indexer/compare/0.132.0...0.132.1) (2026-01-28)


### Bug Fixes

* substreams_lag_millis conditional on partial blocks ([ae17442](https://github.com/propeller-heads/tycho-indexer/commit/ae174423759190389199c23152821cdd6d475fa6))
* substreams_lag_millis conditional on partial blocks ([#832](https://github.com/propeller-heads/tycho-indexer/issues/832)) ([b3d6bf0](https://github.com/propeller-heads/tycho-indexer/commit/b3d6bf003f6aef7c74d8906b52311556901ee226))

## [0.132.0](https://github.com/propeller-heads/tycho-indexer/compare/0.131.0...0.132.0) (2026-01-27)


### Features

* add enable_partial_blocks option to indexer CLI ([951cb15](https://github.com/propeller-heads/tycho-indexer/commit/951cb15f60e93a7891afef218c715327cb78b234))
* add enable_partial_blocks option to indexer CLI ([#831](https://github.com/propeller-heads/tycho-indexer/issues/831)) ([8fdb033](https://github.com/propeller-heads/tycho-indexer/commit/8fdb033fa708fad5771619057a394f7221773c62))

## [0.131.0](https://github.com/propeller-heads/tycho-indexer/compare/0.130.1...0.131.0) (2026-01-27)


### Features

* expose partial blocks flad on SubstreamsStream ([ddd0054](https://github.com/propeller-heads/tycho-indexer/commit/ddd0054cb5a427e39dda81c8fe7ae03631e07d22))
* skip lag metrics on partial blocks ([dd2a783](https://github.com/propeller-heads/tycho-indexer/commit/dd2a783405621783662ce86944238489a37a9792))
* update substream dependencies ([f0b04ea](https://github.com/propeller-heads/tycho-indexer/commit/f0b04ea30a2312ac285dea9de872461ef45b637d))
* update substream stream to use v3 Request ([51dabc4](https://github.com/propeller-heads/tycho-indexer/commit/51dabc43881186ea8047bd06f0f713d9977e27bb))
* Update substreams sink files to support partial blocks ([#830](https://github.com/propeller-heads/tycho-indexer/issues/830)) ([84fd327](https://github.com/propeller-heads/tycho-indexer/commit/84fd327f0e8fb1a5c5bfac851f8b364d7cbd983c))

## [0.130.1](https://github.com/propeller-heads/tycho-indexer/compare/0.130.0...0.130.1) (2026-01-22)

## [0.130.0](https://github.com/propeller-heads/tycho-indexer/compare/0.129.1...0.130.0) (2026-01-21)


### Features

* add PartialEq and Serde to tycho-ethereum GasPrice. ([e66af2d](https://github.com/propeller-heads/tycho-indexer/commit/e66af2dfdc30e4ef82283e77137b7119f4897529))
* add PartialEq and Serde to tycho-ethereum GasPrice. ([#828](https://github.com/propeller-heads/tycho-indexer/issues/828)) ([2f2a78d](https://github.com/propeller-heads/tycho-indexer/commit/2f2a78d8512861cb4c09f97965263c4a48844363))

## [0.129.1](https://github.com/propeller-heads/tycho-indexer/compare/0.129.0...0.129.1) (2026-01-20)

## [0.129.0](https://github.com/propeller-heads/tycho-indexer/compare/0.128.0...0.129.0) (2026-01-13)


### Features

* add GasPriceGetter trait and implement gas price retrieval in EthereumRpcClient ([a276cc1](https://github.com/propeller-heads/tycho-indexer/commit/a276cc1f2e6187c65851753ac9259557c9393814))
* add GasPriceGetter trait and implement gas price retrieval in EthereumRpcClient ([#823](https://github.com/propeller-heads/tycho-indexer/issues/823)) ([ca96929](https://github.com/propeller-heads/tycho-indexer/commit/ca96929a425084d1d3fb8b8f9c99d4a65f0a26f8))

## [0.128.0](https://github.com/propeller-heads/tycho-indexer/compare/0.127.0...0.128.0) (2026-01-09)


### Features

* revert add typetag derive to `ProtocolSim` trait ([#822](https://github.com/propeller-heads/tycho-indexer/issues/822)) ([1641595](https://github.com/propeller-heads/tycho-indexer/commit/164159586d34fe62ca94f39dd812965533f2744b))

## [0.127.0](https://github.com/propeller-heads/tycho-indexer/compare/0.126.0...0.127.0) (2026-01-08)


### Features

* add typetag derive to `ProtocolSim` trait ([425c194](https://github.com/propeller-heads/tycho-indexer/commit/425c194a731df998ff94fb6610616b27cc75d969))
* add typetag derive to `ProtocolSim` trait ([#811](https://github.com/propeller-heads/tycho-indexer/issues/811)) ([bdbdf71](https://github.com/propeller-heads/tycho-indexer/commit/bdbdf713787dd76a1a228203c105391ae359da8d))

## [0.126.0](https://github.com/propeller-heads/tycho-indexer/compare/0.125.2...0.126.0) (2026-01-08)


### Features

* **migrations:** add SQL migrations for `pg_stat_statements` extension ([#820](https://github.com/propeller-heads/tycho-indexer/issues/820)) ([06599da](https://github.com/propeller-heads/tycho-indexer/commit/06599da112f3d06aa40e779404411e7cf2a86faf))
* **migrations:** add SQL scripts to create and drop pg_stat_statements extension ([4850934](https://github.com/propeller-heads/tycho-indexer/commit/4850934f9daa147ed31abecbbc7c94d05d798272))

## [0.125.2](https://github.com/propeller-heads/tycho-indexer/compare/0.125.1...0.125.2) (2025-12-19)


### Bug Fixes

* Update error message in query_pool_swap function ([d4b5450](https://github.com/propeller-heads/tycho-indexer/commit/d4b54505202e380c9467ef0ca5c0adc33a69861f))
* Update error message in query_pool_swap function ([#812](https://github.com/propeller-heads/tycho-indexer/issues/812)) ([7fd85ff](https://github.com/propeller-heads/tycho-indexer/commit/7fd85ff84fda2b641310b2ff28c6e9d6d72aa228))

## [0.125.1](https://github.com/propeller-heads/tycho-indexer/compare/0.125.0...0.125.1) (2025-12-17)

## [0.125.0](https://github.com/propeller-heads/tycho-indexer/compare/0.124.0...0.125.0) (2025-12-17)


### Features

* restore higher pagination limits ([#813](https://github.com/propeller-heads/tycho-indexer/issues/813)) ([5f33369](https://github.com/propeller-heads/tycho-indexer/commit/5f33369021d8259ce252843247b2a63db98feed5))

## [0.124.0](https://github.com/propeller-heads/tycho-indexer/compare/0.123.0...0.124.0) (2025-12-16)


### Features

* add min_traded_n_days_ago filter to RPC server configuration and validation ([b99e717](https://github.com/propeller-heads/tycho-indexer/commit/b99e7171e5afd4fbde3e7efbad68107df5099d5b))
* add RPC server configuration and validation for filtering thresholds ([c86bb8f](https://github.com/propeller-heads/tycho-indexer/commit/c86bb8fe5cd4b628d89022aedd41b3ed08bdff75))
* add RPC server configuration and validation for filtering thresholds ([#803](https://github.com/propeller-heads/tycho-indexer/issues/803)) ([8ce170b](https://github.com/propeller-heads/tycho-indexer/commit/8ce170bff47f33b910ae35898c8861c58cf96612))

## [0.123.0](https://github.com/propeller-heads/tycho-indexer/compare/0.122.3...0.123.0) (2025-12-16)


### Features

* Address PR reviews. Add new return parameter. ([25da39c](https://github.com/propeller-heads/tycho-indexer/commit/25da39c66f2d6d30e75e4f60510e27d0fde4ca50))
* Unify two methods in protocol sim. Add enum for price constraint ([ecbf5b2](https://github.com/propeller-heads/tycho-indexer/commit/ecbf5b26ccd935c11849586930c9ce358981c4ae))


### Bug Fixes

* add input validation for Price struct parameters ([e2508a0](https://github.com/propeller-heads/tycho-indexer/commit/e2508a0a5e19f9bf013e9813dbc61fd4ed863e96))

## [0.122.3](https://github.com/propeller-heads/tycho-indexer/compare/0.122.2...0.122.3) (2025-12-15)


### Bug Fixes

* **dci:** add entrypoint_id to trace metrics ([ef3012d](https://github.com/propeller-heads/tycho-indexer/commit/ef3012dfbb306d2cb3c20bdb5194e0dbf730624b))

## [0.122.2](https://github.com/propeller-heads/tycho-indexer/compare/0.122.1...0.122.2) (2025-12-15)


### Bug Fixes

* early exit account extractions on empty storage requests ([1772677](https://github.com/propeller-heads/tycho-indexer/commit/1772677b17e6d8f65b597e55a28a94b5e89094aa))
* full indexing check on slot extraction trigger ([eb2187b](https://github.com/propeller-heads/tycho-indexer/commit/eb2187b8a690302ad2e60e4c570fbe194240885c))
* skip account extraction on new slots for full indexed contracts ([61e74ba](https://github.com/propeller-heads/tycho-indexer/commit/61e74ba95745abb848981eb994b4c1e5185f2611))
* skip account extraction on new slots for full indexed contracts ([#808](https://github.com/propeller-heads/tycho-indexer/issues/808)) ([c87e18e](https://github.com/propeller-heads/tycho-indexer/commit/c87e18ef994a81a38fd34daeb28b2b1be51ef9c0))

## [0.122.1](https://github.com/propeller-heads/tycho-indexer/compare/0.122.0...0.122.1) (2025-12-15)

## [0.122.0](https://github.com/propeller-heads/tycho-indexer/compare/0.121.2...0.122.0) (2025-12-13)


### Features

* fix balance overrides for Go node providers ([355da1b](https://github.com/propeller-heads/tycho-indexer/commit/355da1b73cfd904e2f9cb77731cd224110f23ce3))
* fix balance overrides for Go node providers ([#807](https://github.com/propeller-heads/tycho-indexer/issues/807)) ([a31d5f2](https://github.com/propeller-heads/tycho-indexer/commit/a31d5f2db8497f463ace52de3d10cc37a1013bd6))

## [0.121.2](https://github.com/propeller-heads/tycho-indexer/compare/0.121.1...0.121.2) (2025-12-12)


### Bug Fixes

* normalise balance overrides (remove leading zeros) ([4ff652b](https://github.com/propeller-heads/tycho-indexer/commit/4ff652b0a6aab9bb290a998140d61581174a85b6))
* normalise balance overrides (remove leading zeros) ([#806](https://github.com/propeller-heads/tycho-indexer/issues/806)) ([6b7a6f9](https://github.com/propeller-heads/tycho-indexer/commit/6b7a6f99b0752e4ca16a31d917a353d731592079))

## [0.121.1](https://github.com/propeller-heads/tycho-indexer/compare/0.121.0...0.121.1) (2025-12-12)


### Bug Fixes

* add dci trace metrics ([5dee152](https://github.com/propeller-heads/tycho-indexer/commit/5dee152a6ffb479ffaf5fb0c4b761b0a896dd16e))
* add dci trace metrics ([#805](https://github.com/propeller-heads/tycho-indexer/issues/805)) ([e217b90](https://github.com/propeller-heads/tycho-indexer/commit/e217b90226bfac8f6c108d78b4ad590461df222e))
* **dci:** skip cache update on blocks with no traces ([4cc50cc](https://github.com/propeller-heads/tycho-indexer/commit/4cc50ccca667c104b32846cd3a1dbb4d171524fc))

## [0.121.0](https://github.com/propeller-heads/tycho-indexer/compare/0.120.0...0.121.0) (2025-12-11)


### Features

* add debug logging for RPC retry attempts and backoff duration ([96349e8](https://github.com/propeller-heads/tycho-indexer/commit/96349e84ae34eb4aa963c0f3ce4db286b190131b))
* add RPC retry and batching configuration structs and otherwise polish the PR ([039872f](https://github.com/propeller-heads/tycho-indexer/commit/039872fda9cb51e6fe3cf29832b5a627e3285400))
* add tracing instrumentation to async RPC methods for improved debugging ([2b32b8f](https://github.com/propeller-heads/tycho-indexer/commit/2b32b8f1d3b41c884ec82866052ffdff663c4424))
* implement attempt-based RPC retry logic with custom error handling ([cfe018e](https://github.com/propeller-heads/tycho-indexer/commit/cfe018e7ac44b7976124de785a47dc3fe72db65f))
* implement batch RPC support for slot detection and tracing ([a925115](https://github.com/propeller-heads/tycho-indexer/commit/a925115dc587ec402e389f8c14c48947e1763696))
* integrate `RetryPolicy` across RPC methods for improved error handling ([1408734](https://github.com/propeller-heads/tycho-indexer/commit/140873415a0c557ca532925c64ff48d0f9b461db))
* refactor RPC batching configuration to use enum and improve default settings. Update the methods that were checking for batch config to explicitly fail specifying that the RPC does not support batching ([1aeb24c](https://github.com/propeller-heads/tycho-indexer/commit/1aeb24c44010543a75559edaf2784f3d13473469))
* universal retry logic for the RPC ([#778](https://github.com/propeller-heads/tycho-indexer/issues/778)) ([6514e4e](https://github.com/propeller-heads/tycho-indexer/commit/6514e4e246330726afd33544399970aa7c0cb89b))
* use rpc retry policy for batch trace and access list processing ([964eb05](https://github.com/propeller-heads/tycho-indexer/commit/964eb05fb822212311e6dfff9cdb1ae98e0e690b))


### Bug Fixes

* add chunking based on `max_batch_size` for the slot_detector batch requests ([2382c4f](https://github.com/propeller-heads/tycho-indexer/commit/2382c4f9866e42fdab1c9e150f678e5f474fa985))
* correct typo in `ValueExtractionError` error message ([7d8dc86](https://github.com/propeller-heads/tycho-indexer/commit/7d8dc8627567cd9869929cdd341411762e04ffad))
* format slot and test values as 32-byte hex strings in RPC tracer params ([1eb0302](https://github.com/propeller-heads/tycho-indexer/commit/1eb03026aec6d858f0fb4021a57dc5ca5cc84d96))
* integrate the changes in the tycho-ethereum into tycho-indexer and re-export useful structs from backoff crate ([e36c7a3](https://github.com/propeller-heads/tycho-indexer/commit/e36c7a3a3df741c1cddaea812df6bf2978815768))

## [0.120.0](https://github.com/propeller-heads/tycho-indexer/compare/0.119.2...0.120.0) (2025-12-11)


### Features

* improvements for DCI on fast chains ([#802](https://github.com/propeller-heads/tycho-indexer/issues/802)) ([9835efb](https://github.com/propeller-heads/tycho-indexer/commit/9835efbaf4429a0d4139188cb201ad878bf7c8dc))
* **tycho-client:** extend dci support to non-ethereum chains ([424977c](https://github.com/propeller-heads/tycho-indexer/commit/424977c71ccaa09a47b399e3d570f20bd70fc836))


### Bug Fixes

* **dci:** handle native token balances overwrites if native token is a pool token ([7d92dd6](https://github.com/propeller-heads/tycho-indexer/commit/7d92dd6c879689af3aa0b78dfae462e178bc8d41))
* **storage:** chunk trace result inserts to avoid exceeding PostgreSQL param limit ([bd13042](https://github.com/propeller-heads/tycho-indexer/commit/bd130424b140c0bbc5473468cd264dab413716e2))

## [0.119.2](https://github.com/propeller-heads/tycho-indexer/compare/0.119.1...0.119.2) (2025-12-08)


### Bug Fixes

* chuck inserts across to avoid exceeding PostgreSQL parameter limits. ([c873493](https://github.com/propeller-heads/tycho-indexer/commit/c87349301a9358d3ecd991096b6dbb3b2debad7d))
* chuck inserts across to avoid exceeding PostgreSQL parameter limits. ([#800](https://github.com/propeller-heads/tycho-indexer/issues/800)) ([711e137](https://github.com/propeller-heads/tycho-indexer/commit/711e137a0f7d59766edcf60b898bb4b71d5ad8f6))

## [0.119.1](https://github.com/propeller-heads/tycho-indexer/compare/0.119.0...0.119.1) (2025-12-03)

## [0.119.0](https://github.com/propeller-heads/tycho-indexer/compare/0.118.1...0.119.0) (2025-12-02)


### Features

* restore higher pagination limits ([#796](https://github.com/propeller-heads/tycho-indexer/issues/796)) ([5a075c3](https://github.com/propeller-heads/tycho-indexer/commit/5a075c3c7f68395770b7bbd8410fb8d4b2353955))


### Bug Fixes

* restore higher pagination limits for StateRequestBody, TokensRequestBody, ProtocolComponentsRequestBody, and ProtocolStateRequestBody ([fdfde70](https://github.com/propeller-heads/tycho-indexer/commit/fdfde7040a0e3e511fc8f8529af6b0695cef2fbe))

## [0.118.1](https://github.com/propeller-heads/tycho-indexer/compare/0.118.0...0.118.1) (2025-12-02)

## [0.118.0](https://github.com/propeller-heads/tycho-indexer/compare/0.117.4...0.118.0) (2025-12-02)


### Features

* add minimum TVL config for RPC responses ([#794](https://github.com/propeller-heads/tycho-indexer/issues/794)) ([9caaa9f](https://github.com/propeller-heads/tycho-indexer/commit/9caaa9f7c7d49e3daf09dd4b94a545b456733dc1))
* add minimum TVL threshold for RPC responses ([1f651a8](https://github.com/propeller-heads/tycho-indexer/commit/1f651a8d61d65900597c347b1e58768cff207d66))
* Add script to compare slipstreams observations with on-chain state ([fbb3fae](https://github.com/propeller-heads/tycho-indexer/commit/fbb3faede0b13d8c941957e53af412a8b3cfa516))
* Add script to compare slipstreams observations with on-chain state ([#776](https://github.com/propeller-heads/tycho-indexer/issues/776)) ([5fa1166](https://github.com/propeller-heads/tycho-indexer/commit/5fa11660f99f6511da0ddc0337c52ffd40e3b6c5))

## [0.117.4](https://github.com/propeller-heads/tycho-indexer/compare/0.117.3...0.117.4) (2025-12-01)

## [0.117.3](https://github.com/propeller-heads/tycho-indexer/compare/0.117.2...0.117.3) (2025-11-28)

## [0.117.2](https://github.com/propeller-heads/tycho-indexer/compare/0.117.1...0.117.2) (2025-11-28)


### Bug Fixes

* ping point the source of untracked memory and adjust for it ([00350c5](https://github.com/propeller-heads/tycho-indexer/commit/00350c52fae87462ef5ebe87a6d4ab488e424c27))
* ping point the source of untracked memory and resolve most of it ([#788](https://github.com/propeller-heads/tycho-indexer/issues/788)) ([659a9f3](https://github.com/propeller-heads/tycho-indexer/commit/659a9f3cb7ad53308bf566169ffeccd0c6b70bc3))

## [0.117.1](https://github.com/propeller-heads/tycho-indexer/compare/0.117.0...0.117.1) (2025-11-27)

## [0.117.0](https://github.com/propeller-heads/tycho-indexer/compare/0.116.0...0.117.0) (2025-11-26)


### Features

* add a configurable RPC retry mechanism to CLI ([9397b32](https://github.com/propeller-heads/tycho-indexer/commit/9397b3206623abe60eb58b2e6a6d616e570d6f8d))
* get `RPCRetryConfig` from the CLI and pass it to the DCI plugin ([8db0cb4](https://github.com/propeller-heads/tycho-indexer/commit/8db0cb4c1a425f50a9ff3986b827fca06bccee31))
* make EVM balance slot detector configuration dependent on RPC retry settings ([6d077d2](https://github.com/propeller-heads/tycho-indexer/commit/6d077d2c0d386dcafece28059a70020c5e936727))
* RPC retries configurable from the CLI  ([#787](https://github.com/propeller-heads/tycho-indexer/issues/787)) ([e7df6d9](https://github.com/propeller-heads/tycho-indexer/commit/e7df6d9fe601ef8797a44f2ac8a83f8e6e00bc4b))
* unify RPC configuration into single RPCArgs and RPCConfig structs ([d83f976](https://github.com/propeller-heads/tycho-indexer/commit/d83f97626648e1db4ab37aa0b838c0e5bb4a64da))


### Bug Fixes

* resolve rebase conflicts by reverting to RPCRetryConfig-only approach ([027079a](https://github.com/propeller-heads/tycho-indexer/commit/027079a3766782a6ba703c1333c0cde8e5a12731))

## [0.116.0](https://github.com/propeller-heads/tycho-indexer/compare/0.115.0...0.116.0) (2025-11-26)


### Features

* add Price struct and swap_to_price/query_demand methods to ProtocolSim trait ([308ce9f](https://github.com/propeller-heads/tycho-indexer/commit/308ce9f0738c83a4e64e1c68e3e35151e0cfef02))
* Add swap_to_price and query_demand methods to ProtocolSim trait ([#789](https://github.com/propeller-heads/tycho-indexer/issues/789)) ([44e6a69](https://github.com/propeller-heads/tycho-indexer/commit/44e6a69ebf745b2fe6258c7ee27f7a372154f49d))

## [0.115.0](https://github.com/propeller-heads/tycho-indexer/compare/0.114.3...0.115.0) (2025-11-24)


### Features

* error on missing component id:params link ([cb47866](https://github.com/propeller-heads/tycho-indexer/commit/cb4786695500a588e9270dcb39ca10a5ee67c435))
* make component to params link required ([9064cf4](https://github.com/propeller-heads/tycho-indexer/commit/9064cf4740ae1d0b275dc4737cdf8a6c7f3bf00b))
* Make component to tracing params link required ([#779](https://github.com/propeller-heads/tycho-indexer/issues/779)) ([3e26adf](https://github.com/propeller-heads/tycho-indexer/commit/3e26adf20b8f034a034d56451e42aadef45cd5bb))

## [0.114.3](https://github.com/propeller-heads/tycho-indexer/compare/0.114.2...0.114.3) (2025-11-20)


### Bug Fixes

* handle `"storage": null` in `debug_storageRangeAt` responses with a wrapper ([0152c78](https://github.com/propeller-heads/tycho-indexer/commit/0152c78c55f3508447a24ed318497d9997d7829f))
* tmp handle `"storage": null` in `debug_storageRangeAt` responses  ([#785](https://github.com/propeller-heads/tycho-indexer/issues/785)) ([5a5fea8](https://github.com/propeller-heads/tycho-indexer/commit/5a5fea8008758b1eb3397b85b9049200c8ecdf74))

## [0.114.2](https://github.com/propeller-heads/tycho-indexer/compare/0.114.1...0.114.2) (2025-11-20)

## [0.114.1](https://github.com/propeller-heads/tycho-indexer/compare/0.114.0...0.114.1) (2025-11-18)


### Bug Fixes

* Fix DCI cache initialisation. ([78d7206](https://github.com/propeller-heads/tycho-indexer/commit/78d720601ca0d46276fe7e79bd0623f4c63a54e0))
* Fix DCI cache initialisation. ([#784](https://github.com/propeller-heads/tycho-indexer/issues/784)) ([54b321c](https://github.com/propeller-heads/tycho-indexer/commit/54b321c648723baf0e52eff36c8bef2fbb929b3d))

## [0.114.0](https://github.com/propeller-heads/tycho-indexer/compare/0.113.2...0.114.0) (2025-11-18)


### Features

* add script to delete huge protocols ([bcdd54f](https://github.com/propeller-heads/tycho-indexer/commit/bcdd54f23cfa80302f7099c8af0dffb5f7ae8802))
* add script to delete huge protocols ([#747](https://github.com/propeller-heads/tycho-indexer/issues/747)) ([b917e4f](https://github.com/propeller-heads/tycho-indexer/commit/b917e4f8a521f6c118636dab79e0cb3656235eec))

## [0.113.2](https://github.com/propeller-heads/tycho-indexer/compare/0.113.1...0.113.2) (2025-11-18)

## [0.113.1](https://github.com/propeller-heads/tycho-indexer/compare/0.113.0...0.113.1) (2025-11-17)


### Bug Fixes

* have both compressed and uncompressed pagination limits equal ([4e50026](https://github.com/propeller-heads/tycho-indexer/commit/4e5002618ea7a7785aa797f474007032f2c9b500))
* have both compressed and uncompressed pagination limits equal ([#780](https://github.com/propeller-heads/tycho-indexer/issues/780)) ([81285fc](https://github.com/propeller-heads/tycho-indexer/commit/81285fcca7fbdf979421fc1d0d768b27a724dc29))

## [0.113.0](https://github.com/propeller-heads/tycho-indexer/compare/0.112.0...0.113.0) (2025-11-14)


### Features

* add compression-aware pagination validation in `tycho-indexer` ([#768](https://github.com/propeller-heads/tycho-indexer/issues/768)) ([39b29e3](https://github.com/propeller-heads/tycho-indexer/commit/39b29e30cbc9b55b6a70c30a9efe558fa062ba5e))
* add compression-aware pagination validation in `tycho-indexer` RPC services ([98b636c](https://github.com/propeller-heads/tycho-indexer/commit/98b636c80952e26200474130a2bffca87c75a141))
* add pagination validation to `component_tvl` RPC handler and define limits for `ComponentTvlRequestBody` ([e717457](https://github.com/propeller-heads/tycho-indexer/commit/e7174572d0a635bd7353afcddabcfafab9422b23))

## [0.112.0](https://github.com/propeller-heads/tycho-indexer/compare/0.111.1...0.112.0) (2025-11-14)


### Features

* improve token retrieval with concurrent paginated requests ([#771](https://github.com/propeller-heads/tycho-indexer/issues/771)) ([a5a7bc6](https://github.com/propeller-heads/tycho-indexer/commit/a5a7bc6fd30c2709762ba2d8dac147feec021515))
* make concurrency limit configurable in `get_all_tokens` ([1103dac](https://github.com/propeller-heads/tycho-indexer/commit/1103dacb639148c9d74af721ce5aaf5972fc1798))

## [0.111.1](https://github.com/propeller-heads/tycho-indexer/compare/0.111.0...0.111.1) (2025-11-14)


### Bug Fixes

* address errors in swagger API ([#733](https://github.com/propeller-heads/tycho-indexer/issues/733)) ([1d5bc57](https://github.com/propeller-heads/tycho-indexer/commit/1d5bc574355f62d48a817fd8ebd9444c62155bd1))
* adjust schema definitions and resolve the swagger UI errors ([5ee04ee](https://github.com/propeller-heads/tycho-indexer/commit/5ee04eedb35bd723c2aa8c35ddc6ad618f980205))

## [0.111.0](https://github.com/propeller-heads/tycho-indexer/compare/0.110.0...0.111.0) (2025-11-14)


### Features

* major refactor of `tycho-ethereum` ([#742](https://github.com/propeller-heads/tycho-indexer/issues/742)) ([ab16908](https://github.com/propeller-heads/tycho-indexer/commit/ab169081cd1f54fb1854e420dc2225a21dd47b09))
* make `max_storage_slot_batch_size` configurable via environment variable in `BatchingConfig` ([ed88c90](https://github.com/propeller-heads/tycho-indexer/commit/ed88c905bb42a2ecf4182cdbc47262446f959d8a))


### Bug Fixes

* adjust the tracer tests that were added on main to match the refactored version of tycho ethereum ([21fe442](https://github.com/propeller-heads/tycho-indexer/commit/21fe4425b36dd868a51f344859007337160e13f2))
* correct JSON-RPC response IDs in entrypoint tracer tests ([9f87dde](https://github.com/propeller-heads/tycho-indexer/commit/9f87ddee65038565a82de9a1dcf60a9fda5f37ac))
* create a dedicated trace rpc client consistent with the previous design. Additionally, streamline the `EVMEntrypointService` initialization of an unnecessary Result wrapping ([91c4f5d](https://github.com/propeller-heads/tycho-indexer/commit/91c4f5d2f5d0767991b17d1f95192039a7304875))
* fix compilation issues ([a83a59a](https://github.com/propeller-heads/tycho-indexer/commit/a83a59a5d69446346efd0100ab70ce2cbf7712f1))
* handle `MissingBatchResponse` errors in RPC operations and improve error messages to fix test failing ([505f792](https://github.com/propeller-heads/tycho-indexer/commit/505f79258e905e856e8b7efaed7d503eb4cfda68))
* replace placeholder logic with `format!` for constructing JSON-RPC response in tracer tests ([797f420](https://github.com/propeller-heads/tycho-indexer/commit/797f4209f94790f04822cf139a5773418a9133d3))
* use dynamic `self.chain` instead of hardcoded `Chain::Ethereum` in block processing logic ([0445a3c](https://github.com/propeller-heads/tycho-indexer/commit/0445a3cfd53f1cd9dfe1b23fbbc741f5d972fde5))

## [0.110.0](https://github.com/propeller-heads/tycho-indexer/compare/0.109.2...0.110.0) (2025-11-13)


### Features

* rename component to params link table ([bdac794](https://github.com/propeller-heads/tycho-indexer/commit/bdac794efc7f3a52e8d8f1d6dea53bbc642f0c8f))
* utilise component to params link table to fetch only relevant params per component ([#775](https://github.com/propeller-heads/tycho-indexer/issues/775)) ([90acaa5](https://github.com/propeller-heads/tycho-indexer/commit/90acaa5dab13863925e295d2d22f9caf3667d1e6))


### Bug Fixes

* fetch only directly linked params per component ([095061b](https://github.com/propeller-heads/tycho-indexer/commit/095061b0f24ed730d1c2347b035139f9377da85c))

## [0.109.2](https://github.com/propeller-heads/tycho-indexer/compare/0.109.1...0.109.2) (2025-11-13)


### Bug Fixes

* prevent reqwest enabling compression by default ([1f55643](https://github.com/propeller-heads/tycho-indexer/commit/1f55643fc90d2e2fd2afad88d728968c340cf5ad))
* prevent reqwest enabling compression by default ([#772](https://github.com/propeller-heads/tycho-indexer/issues/772)) ([052f5d0](https://github.com/propeller-heads/tycho-indexer/commit/052f5d0e8f1587dd9d7fa7c3bbde43142632bc8c))

## [0.109.1](https://github.com/propeller-heads/tycho-indexer/compare/0.109.0...0.109.1) (2025-11-12)

## [0.109.0](https://github.com/propeller-heads/tycho-indexer/compare/0.108.0...0.109.0) (2025-11-07)


### Features

* add a way to quickly configure storage batch size ([e6994b8](https://github.com/propeller-heads/tycho-indexer/commit/e6994b8b04a648e3bd150db487837cb273d49354))
* add a way to quickly configure storage batch size ([#770](https://github.com/propeller-heads/tycho-indexer/issues/770)) ([29a0b4f](https://github.com/propeller-heads/tycho-indexer/commit/29a0b4f811896ed58fc94800fd7df1a72641b932))

## [0.108.0](https://github.com/propeller-heads/tycho-indexer/compare/0.107.1...0.108.0) (2025-11-07)


### Features

* add client support for both `ws` and `rpc` compression ([#753](https://github.com/propeller-heads/tycho-indexer/issues/753)) ([fb0f702](https://github.com/propeller-heads/tycho-indexer/commit/fb0f702c91a41c20ae5c75b4e3f523023dbcb328))
* add configurable options and zstd compression support to `HttpRPCClient` and WebSocket subscriptions ([c9d1ae2](https://github.com/propeller-heads/tycho-indexer/commit/c9d1ae24e84ebb100c5374374ffa8d5147786b35))

## [0.107.1](https://github.com/propeller-heads/tycho-indexer/compare/0.107.0...0.107.1) (2025-11-07)


### Bug Fixes

* **account_extractor:** gracefully handle null storage responses ([6a63a6c](https://github.com/propeller-heads/tycho-indexer/commit/6a63a6cf1bf408fb42a7710e743a200061564577))
* **account_extractor:** gracefully handle null storage responses ([#769](https://github.com/propeller-heads/tycho-indexer/issues/769)) ([cbb4a4f](https://github.com/propeller-heads/tycho-indexer/commit/cbb4a4f278a8c44bc4d5e5d2f3c512808af1245f))
* handle null storage value in storage fetch response ([789fe2e](https://github.com/propeller-heads/tycho-indexer/commit/789fe2eff4ded788cd9da92e812662d68b4976f0))

## [0.107.0](https://github.com/propeller-heads/tycho-indexer/compare/0.106.0...0.107.0) (2025-11-07)


### Features

* Call all account_extractor futures together ([357bbe9](https://github.com/propeller-heads/tycho-indexer/commit/357bbe98babce6f1149712261099a31bc8a2090a))
* sequentially process storage futures instead of concurrently ([1bc92e4](https://github.com/propeller-heads/tycho-indexer/commit/1bc92e4fbb9200948771f5ce229771dd60549c88))


### Bug Fixes

* Fix batch account storage requests handling ([#766](https://github.com/propeller-heads/tycho-indexer/issues/766)) ([66f57d7](https://github.com/propeller-heads/tycho-indexer/commit/66f57d7159687f0d2e0b686d22e2963d8d98d33d))
* Fix batch vec initialization by recreating it for every batch ([2d7b2cb](https://github.com/propeller-heads/tycho-indexer/commit/2d7b2cb9563ec8d45cb7ae16a81e2fcd707d8418))

## [0.106.0](https://github.com/propeller-heads/tycho-indexer/compare/0.105.4...0.106.0) (2025-11-06)


### Features

* Add custom bytecode for Unichain V4 MiniRouter ([af86232](https://github.com/propeller-heads/tycho-indexer/commit/af862328a8111fd4b167e7234c3070afa1eb1332))
* Add custom bytecode for Unichain V4 MiniRouter ([#765](https://github.com/propeller-heads/tycho-indexer/issues/765)) ([6b585d8](https://github.com/propeller-heads/tycho-indexer/commit/6b585d892108f9a1beea04bad32966de54d79cf3))

## [0.105.4](https://github.com/propeller-heads/tycho-indexer/compare/0.105.3...0.105.4) (2025-11-06)

## [0.105.3](https://github.com/propeller-heads/tycho-indexer/compare/0.105.2...0.105.3) (2025-11-06)

## [0.105.2](https://github.com/propeller-heads/tycho-indexer/compare/0.105.1...0.105.2) (2025-11-06)


### Bug Fixes

* use a correct tracing router address ([a3b2f8a](https://github.com/propeller-heads/tycho-indexer/commit/a3b2f8a227be861e8712bef673bdb6cdbf75d7ad))
* use a correct tracing router address ([#760](https://github.com/propeller-heads/tycho-indexer/issues/760)) ([298d398](https://github.com/propeller-heads/tycho-indexer/commit/298d398dc6d403530ff733c7a7091f6780add549))

## [0.105.1](https://github.com/propeller-heads/tycho-indexer/compare/0.105.0...0.105.1) (2025-11-06)

## [0.105.0](https://github.com/propeller-heads/tycho-indexer/compare/0.104.1...0.105.0) (2025-11-06)


### Features

* add dedicated UniswapV4Hooks dci plugin config type ([9d1f3cc](https://github.com/propeller-heads/tycho-indexer/commit/9d1f3cc42c099bcc98e1167e62b0b14c6ba45182))
* make dci blacklist addresses configurable ([8e1db98](https://github.com/propeller-heads/tycho-indexer/commit/8e1db9826086bfcb82527c33eb58f786aca262df))
* make uniswap v4 hooks DCI plugin chain agnostic ([#756](https://github.com/propeller-heads/tycho-indexer/issues/756)) ([95257d3](https://github.com/propeller-heads/tycho-indexer/commit/95257d3f2ecfe6159c93fb422952f0c3fd7c3cf9))
* set trace router address to hardcoded random address ([27ad55c](https://github.com/propeller-heads/tycho-indexer/commit/27ad55ccd722606e76e70e70ba3493f25bf53f31))


### Bug Fixes

* argument order of usv4 hook dci creation ([208c6c9](https://github.com/propeller-heads/tycho-indexer/commit/208c6c9b7a369e48d597f4284857933b17689e01))

## [0.104.1](https://github.com/propeller-heads/tycho-indexer/compare/0.104.0...0.104.1) (2025-11-05)


### Bug Fixes

* Decrease max batch size on account extractor ([16c477b](https://github.com/propeller-heads/tycho-indexer/commit/16c477be24efc4e67b6d65a88c3f779725bc0b0a))
* Decrease max batch size on account extractor ([#758](https://github.com/propeller-heads/tycho-indexer/issues/758)) ([fd4524d](https://github.com/propeller-heads/tycho-indexer/commit/fd4524d60a196b845a369242afa48090e89e43aa))

## [0.104.0](https://github.com/propeller-heads/tycho-indexer/compare/0.103.1...0.104.0) (2025-11-05)


### Features

* Add ability to register default Hook Orchestrator ([1b1b5c9](https://github.com/propeller-heads/tycho-indexer/commit/1b1b5c9a00a15a6d9c2cd1781a3d774a67f1fb7b))
* Enriches component metadata with balance updates ([7c78ef7](https://github.com/propeller-heads/tycho-indexer/commit/7c78ef71962d87bb0b9719e2c4ee423d75e48c1f))
* Index all composable hooks ([#746](https://github.com/propeller-heads/tycho-indexer/issues/746)) ([a90ec11](https://github.com/propeller-heads/tycho-indexer/commit/a90ec11d0e510aa967ff3a3b06c6f0018d5c4e6d))
* Move the metadata enrichment logic to the hook orchestrator ([bc842c3](https://github.com/propeller-heads/tycho-indexer/commit/bc842c38020d0e00b5280da83ae9c06e85b0e22b))

## [0.103.1](https://github.com/propeller-heads/tycho-indexer/compare/0.103.0...0.103.1) (2025-11-05)

## [0.103.0](https://github.com/propeller-heads/tycho-indexer/compare/0.102.0...0.103.0) (2025-11-05)


### Features

* add zstd compression support for WebSocket subscriptions ([4cf8256](https://github.com/propeller-heads/tycho-indexer/commit/4cf8256e77e97625060d5ec439a9b8190e5d0891))
* changed `CompressionError` handling for WebSocket communication from retry to fatal ([4a469bf](https://github.com/propeller-heads/tycho-indexer/commit/4a469bfd2d7677e07fe7b5a01599ceb18585bce2))
* enable zstd compression support in `tycho-indexer` ws ([#749](https://github.com/propeller-heads/tycho-indexer/issues/749)) ([2e2e3d9](https://github.com/propeller-heads/tycho-indexer/commit/2e2e3d99fe4626366fea4c03aa855b3946183b9a))

## [0.102.0](https://github.com/propeller-heads/tycho-indexer/compare/0.101.4...0.102.0) (2025-11-05)


### Features

* enable zstd compression support in `tycho-indexer` HTTP responses and add tests for compression behavior ([50be5b5](https://github.com/propeller-heads/tycho-indexer/commit/50be5b5314df36bf9444ad8c94453467c899204c))
* enable zstd compression support in `tycho-indexer` rpc ([#748](https://github.com/propeller-heads/tycho-indexer/issues/748)) ([ae13a7d](https://github.com/propeller-heads/tycho-indexer/commit/ae13a7d4e20a160284c6181f8950246820e73375))

## [0.101.4](https://github.com/propeller-heads/tycho-indexer/compare/0.101.3...0.101.4) (2025-11-05)

## [0.101.3](https://github.com/propeller-heads/tycho-indexer/compare/0.101.2...0.101.3) (2025-11-04)


### Bug Fixes

* Fix tracer batch request response parsing order ([a5fb8d3](https://github.com/propeller-heads/tycho-indexer/commit/a5fb8d3846eeb35bee228c631d8577f9d7d339f4))
* Fix tracer batch request response parsing order ([#752](https://github.com/propeller-heads/tycho-indexer/issues/752)) ([f7500bf](https://github.com/propeller-heads/tycho-indexer/commit/f7500bff4e77da92e821254177ec4dbe4ed34f7a))

## [0.101.2](https://github.com/propeller-heads/tycho-indexer/compare/0.101.1...0.101.2) (2025-11-04)


### Bug Fixes

* BlockParam creation in synchronizer ([dc797c4](https://github.com/propeller-heads/tycho-indexer/commit/dc797c4b55ca8f316885ea635c0e37dfda4ca558))
* BlockParam creation in synchronizer ([#754](https://github.com/propeller-heads/tycho-indexer/issues/754)) ([1658a74](https://github.com/propeller-heads/tycho-indexer/commit/1658a7426c53d25cda6ec0421924e9dbe20464ff))

## [0.101.1](https://github.com/propeller-heads/tycho-indexer/compare/0.101.0...0.101.1) (2025-11-04)


### Bug Fixes

* Allowance slot calculation - try next slot on error ([cdafeb5](https://github.com/propeller-heads/tycho-indexer/commit/cdafeb5854c41bea0a733a52e3d52103db9e4b55))
* Allowance slot calculation - try next slot on error ([#750](https://github.com/propeller-heads/tycho-indexer/issues/750)) ([7619036](https://github.com/propeller-heads/tycho-indexer/commit/7619036ad7c26253b73173a421e322c67da8e16b))

## [0.101.0](https://github.com/propeller-heads/tycho-indexer/compare/0.100.1...0.101.0) (2025-11-03)


### Features

* Better RPC get_snapshots organization ([5451349](https://github.com/propeller-heads/tycho-indexer/commit/54513492302d7b85bc4eac056fdbd3c68502a4b7))
* Expose get_snapshots in RPC client ([1f9e702](https://github.com/propeller-heads/tycho-indexer/commit/1f9e70277f533941ba295bf5e9ac2bd82210acc9))
* Expose get_snapshots in RPC client ([#743](https://github.com/propeller-heads/tycho-indexer/issues/743)) ([cfa8959](https://github.com/propeller-heads/tycho-indexer/commit/cfa89593b3d4eb1adbe8cb2d936d5d42b9dbc17e))
* get_snapshots takes SnapshotRequestBody as input ([bd8b2ea](https://github.com/propeller-heads/tycho-indexer/commit/bd8b2eab775da15f59d37f76c10f77d46b39ed31))
* Move chunk_size and concurrency to get_snapshots input ([b153090](https://github.com/propeller-heads/tycho-indexer/commit/b1530907d61ab490bcec828ddfbfdbe42a1588d8))
* Use references in ShapshotParameters ([8e966be](https://github.com/propeller-heads/tycho-indexer/commit/8e966beead2983911997d42e45f70b6122ea9e4f))


### Bug Fixes

* Test fixes after get_snapshots moved to RPC ([18437d6](https://github.com/propeller-heads/tycho-indexer/commit/18437d6f6406b571a98bf5b7d4c25ad91c003c5f))

## [0.100.1](https://github.com/propeller-heads/tycho-indexer/compare/0.100.0...0.100.1) (2025-10-30)


### Bug Fixes

* Balance + Allowance slot calculation ([abc5a94](https://github.com/propeller-heads/tycho-indexer/commit/abc5a940f68cfb86d7d96ef99f3dd832b880c636))
* Balance + Allowance slot calculation ([#725](https://github.com/propeller-heads/tycho-indexer/issues/725)) ([3cd424f](https://github.com/propeller-heads/tycho-indexer/commit/3cd424fc58079cce93ede8492d7908fcd19a4e2d))
* instead of choosing last slot, use same balance back as before ([facc5e9](https://github.com/propeller-heads/tycho-indexer/commit/facc5e9868a06c31a4c6f62b6d251b3fcf501482))

## [0.100.0](https://github.com/propeller-heads/tycho-indexer/compare/0.99.2...0.100.0) (2025-10-30)


### Features

* Add a cap on how much TracingParams are retried ([62671fb](https://github.com/propeller-heads/tycho-indexer/commit/62671fb20aad5cce2664735e9fbacffc908b0236))
* move params_to_retry identification to separate fn ([655e0f9](https://github.com/propeller-heads/tycho-indexer/commit/655e0f9ce0d066656bd3e3226489c0acdae95fef))
* Retrigger on updates of untraced TracingParams ([#728](https://github.com/propeller-heads/tycho-indexer/issues/728)) ([6de6644](https://github.com/propeller-heads/tycho-indexer/commit/6de66445dca8a4a041b80edffd88abeb68464e9d))
* Rollback using EntryPointWithTracingParams as cache key ([2c04cb6](https://github.com/propeller-heads/tycho-indexer/commit/2c04cb638a0c4908a199ed5bdb6ffebff84d3807))
* Update DCI Cache to keep track of TraceParams without results ([706f34a](https://github.com/propeller-heads/tycho-indexer/commit/706f34a654e4e4fd749d76a90ace052b7f7388a8))
* Update DCI to retry on changed Components that had failed tracing ([625b475](https://github.com/propeller-heads/tycho-indexer/commit/625b475d56204d8f5890ba0f5f45599bde36da7a))
* Use EntryPointWithTracingParams as cache key ([17d1a47](https://github.com/propeller-heads/tycho-indexer/commit/17d1a4771c1c8dc32c4209ff5c452326d203b402))

## [0.99.2](https://github.com/propeller-heads/tycho-indexer/compare/0.99.1...0.99.2) (2025-10-28)


### Bug Fixes

* erc20 abi panicking on big decimals values ([#744](https://github.com/propeller-heads/tycho-indexer/issues/744)) ([be07e3b](https://github.com/propeller-heads/tycho-indexer/commit/be07e3b882ea05d5be188e81e9d3a48fc540826e))
* fix erc20 abi panicking on big decimals values ([0fe3bdb](https://github.com/propeller-heads/tycho-indexer/commit/0fe3bdb49436b99a46f5c1a13fbd10dfcc093d5c))

## [0.99.1](https://github.com/propeller-heads/tycho-indexer/compare/0.99.0...0.99.1) (2025-10-28)


### Bug Fixes

* reschedule DB cronjobs ([1c123f0](https://github.com/propeller-heads/tycho-indexer/commit/1c123f0266f389b721c69d8d89fa70c67eafa5c2))
* reschedule DB cronjobs ([#740](https://github.com/propeller-heads/tycho-indexer/issues/740)) ([cc4256a](https://github.com/propeller-heads/tycho-indexer/commit/cc4256abcc8176d1eac4a5d2b72ec3ec11c7fcbf))

## [0.99.0](https://github.com/propeller-heads/tycho-indexer/compare/0.98.1...0.99.0) (2025-10-22)


### Features

* optimize cloning instead of expensive `msg`, only clone `msg.header` ([6ff30ba](https://github.com/propeller-heads/tycho-indexer/commit/6ff30ba3dcdd3b8f89ef3555c4fbde7a6222207d))


### Bug Fixes

* catch missed error handling in message printer of `cli.rs` ([6f8b4fb](https://github.com/propeller-heads/tycho-indexer/commit/6f8b4fbb0e73ae2f408e727bc6dc9df0d09d2d2c))
* correct typo in `deltas.rs` doc comments ([4134115](https://github.com/propeller-heads/tycho-indexer/commit/413411584ed391c35fc8c593c10a822db51cd0f7))
* handle CLI errors cleanly in `main.rs` ([6e96ca5](https://github.com/propeller-heads/tycho-indexer/commit/6e96ca5bb053ff3a8f62d11435125fc42e73e747))
* improve error handling for task monitoring in `cli.rs` ([7d97eac](https://github.com/propeller-heads/tycho-indexer/commit/7d97eacdaf04521702d8e8356df298da9350b466))
* incorrect `block_history` usage by replacing it with `new_block_history` in `feed/mod.rs` ([eedd967](https://github.com/propeller-heads/tycho-indexer/commit/eedd9675aef73a3b111acb21801b8edc19708ddc))
* remove unreachable error branches in `stream.rs` ([6e9cc37](https://github.com/propeller-heads/tycho-indexer/commit/6e9cc3755f7a010846ee579b25551fc7d241a5fa))
* simplify error handling in `handle_error_for_backoff` ([1bb6ae5](https://github.com/propeller-heads/tycho-indexer/commit/1bb6ae5e11f41e2bdf95e8e1c08311a92001ec4d))

## [0.98.1](https://github.com/propeller-heads/tycho-indexer/compare/0.98.0...0.98.1) (2025-10-21)


### Bug Fixes

* bug reporting 0 blocks sync rate ([d6df0f1](https://github.com/propeller-heads/tycho-indexer/commit/d6df0f1f2c8430efe71d1879a664d500b28d4a92))
* bug reporting 0 blocks sync rate ([#737](https://github.com/propeller-heads/tycho-indexer/issues/737)) ([9a82f93](https://github.com/propeller-heads/tycho-indexer/commit/9a82f9358322d10e7e9f3a2b08761154e407dd06))

## [0.98.0](https://github.com/propeller-heads/tycho-indexer/compare/0.97.0...0.98.0) (2025-10-17)


### Features

* add cache entry count metric to RPC monitoring ([b6eb0b7](https://github.com/propeller-heads/tycho-indexer/commit/b6eb0b7b23866009f5d29eea82a7e89db80408e0))

## [0.97.0](https://github.com/propeller-heads/tycho-indexer/compare/0.96.2...0.97.0) (2025-10-16)


### Features

* logging middleware ([#732](https://github.com/propeller-heads/tycho-indexer/issues/732)) ([6624db9](https://github.com/propeller-heads/tycho-indexer/commit/6624db9b628e6c721ba381827f574358db1d3add))

## [0.96.2](https://github.com/propeller-heads/tycho-indexer/compare/0.96.1...0.96.2) (2025-10-16)


### Bug Fixes

* RPC cache weighting and tidy memory metrics ([#731](https://github.com/propeller-heads/tycho-indexer/issues/731)) ([40a21f0](https://github.com/propeller-heads/tycho-indexer/commit/40a21f0d2ed29f20154d69389db15c9488f34f3b))

## [0.96.1](https://github.com/propeller-heads/tycho-indexer/compare/0.96.0...0.96.1) (2025-10-13)


### Bug Fixes

* Revert "chore: monitoring and logging ([#723](https://github.com/propeller-heads/tycho-indexer/issues/723))" ([#730](https://github.com/propeller-heads/tycho-indexer/issues/730)) ([003656d](https://github.com/propeller-heads/tycho-indexer/commit/003656dc567e53cd6943d9b1089962425d24f08c))


### Reverts

* Revert "chore: monitoring and logging (#723)" ([d2b7406](https://github.com/propeller-heads/tycho-indexer/commit/d2b7406885a66159cecede50057778c8161b4f58)), closes [#723](https://github.com/propeller-heads/tycho-indexer/issues/723)

## [0.96.0](https://github.com/propeller-heads/tycho-indexer/compare/0.95.2...0.96.0) (2025-10-13)


### Features

* integrate `deepsize` for memory usage reporting ([#729](https://github.com/propeller-heads/tycho-indexer/issues/729)) ([b194f84](https://github.com/propeller-heads/tycho-indexer/commit/b194f8424290548da36b7f2f0bb303ef5e901337))

## [0.95.2](https://github.com/propeller-heads/tycho-indexer/compare/0.95.1...0.95.2) (2025-10-10)

## [0.95.1](https://github.com/propeller-heads/tycho-indexer/compare/0.95.0...0.95.1) (2025-10-10)

## [0.95.0](https://github.com/propeller-heads/tycho-indexer/compare/0.94.1...0.95.0) (2025-10-09)


### Features

* add bsc chain ([a8d3973](https://github.com/propeller-heads/tycho-indexer/commit/a8d39731e51115a081c23ae8224b18a8c5b2f780))
* add bsc chain ([#724](https://github.com/propeller-heads/tycho-indexer/issues/724)) ([0a8d4bf](https://github.com/propeller-heads/tycho-indexer/commit/0a8d4bfc31c5345867b6f9c06fb4cdd47f02abec))


### Bug Fixes

* function name typo ([e8da824](https://github.com/propeller-heads/tycho-indexer/commit/e8da824904e3325c685fc7b85242e4d3f02d62b6))

## [0.94.1](https://github.com/propeller-heads/tycho-indexer/compare/0.94.0...0.94.1) (2025-10-08)

## [0.94.0](https://github.com/propeller-heads/tycho-indexer/compare/0.93.1...0.94.0) (2025-10-07)


### Features

* make RPC retry config more permissive ([101325f](https://github.com/propeller-heads/tycho-indexer/commit/101325f5857b04ad954a198ddc26e5267e34b006))
* make RPC retry config more permissive ([#717](https://github.com/propeller-heads/tycho-indexer/issues/717)) ([e322b5c](https://github.com/propeller-heads/tycho-indexer/commit/e322b5c8292537587f8bcc5ae1ca5cdf68e826cc))

## [0.93.1](https://github.com/propeller-heads/tycho-indexer/compare/0.93.0...0.93.1) (2025-10-07)

## [0.93.0](https://github.com/propeller-heads/tycho-indexer/compare/0.92.1...0.93.0) (2025-10-07)


### Features

* add native balance tracking in DCI ([55edb21](https://github.com/propeller-heads/tycho-indexer/commit/55edb2188552f2f80cffa109d80cb6e2b6218c71))
* add native balance tracking in DCI ([#721](https://github.com/propeller-heads/tycho-indexer/issues/721)) ([247525f](https://github.com/propeller-heads/tycho-indexer/commit/247525f90cd241e0eb151a8175c9126636aefcde))

## [0.92.1](https://github.com/propeller-heads/tycho-indexer/compare/0.92.0...0.92.1) (2025-10-02)


### Bug Fixes

* make new component tokens available to blacklist ([23e21ed](https://github.com/propeller-heads/tycho-indexer/commit/23e21ed00b9e4ed175c4526367155cdfc79433c5))
* make new component tokens available to blacklist ([#720](https://github.com/propeller-heads/tycho-indexer/issues/720)) ([f686d77](https://github.com/propeller-heads/tycho-indexer/commit/f686d770fbe1da43196592ec7358cb3bdd060ea0))

## [0.92.0](https://github.com/propeller-heads/tycho-indexer/compare/0.91.2...0.92.0) (2025-10-02)


### Features

* add configurable batch commit threshold for extractors ([#712](https://github.com/propeller-heads/tycho-indexer/issues/712)) ([635a3a6](https://github.com/propeller-heads/tycho-indexer/commit/635a3a6d164126c7252f9b43ab82f96296c4af45))

## [0.91.2](https://github.com/propeller-heads/tycho-indexer/compare/0.91.1...0.91.2) (2025-10-01)


### Bug Fixes

* support configuring `substreams_api_token` via CLI and instead of only env variables ([f8b1ece](https://github.com/propeller-heads/tycho-indexer/commit/f8b1ece18db7ce710ebd3c27e9e88ae33576e480))
* support configuring `substreams_api_token` via CLI and instead of only env variables ([#716](https://github.com/propeller-heads/tycho-indexer/issues/716)) ([f7dfb24](https://github.com/propeller-heads/tycho-indexer/commit/f7dfb2408746d9ce3808e7926f96eaf91c033182))

## [0.91.1](https://github.com/propeller-heads/tycho-indexer/compare/0.91.0...0.91.1) (2025-09-30)


### Bug Fixes

* correctly parse token balances in token-analyzer ([7ebb91d](https://github.com/propeller-heads/tycho-indexer/commit/7ebb91dad827fe72780ad83d22284cbae061b560))
* correctly parse token balances in token-analyzer ([#715](https://github.com/propeller-heads/tycho-indexer/issues/715)) ([c31f74e](https://github.com/propeller-heads/tycho-indexer/commit/c31f74e28d61c4bb95ee271e8eb0db68b8835a25))

## [0.91.0](https://github.com/propeller-heads/tycho-indexer/compare/0.90.0...0.91.0) (2025-09-30)


### Features

* Remove dependency on ethers, contracts, ethrpc and ethcontract ([0e798bf](https://github.com/propeller-heads/tycho-indexer/commit/0e798bfbc4e87fd7b9268e10feb46dbbb0bb1c5b))
* Remove dependency on ethers, contracts, ethrpc and ethcontract ([#713](https://github.com/propeller-heads/tycho-indexer/issues/713)) ([043d774](https://github.com/propeller-heads/tycho-indexer/commit/043d774f206620462c1395c7c3945d2a6c941b71))

## [0.90.0](https://github.com/propeller-heads/tycho-indexer/compare/0.89.0...0.90.0) (2025-09-29)


### Features

* Client stability improvements ([#711](https://github.com/propeller-heads/tycho-indexer/issues/711)) ([4a87809](https://github.com/propeller-heads/tycho-indexer/commit/4a87809d5ab09c5eebf5f116c849989f534aaf90))
* correctly deal with advanced synchronizers ([cc9bdce](https://github.com/propeller-heads/tycho-indexer/commit/cc9bdce0dadbe093ef7bfb7d5560cf0f20753bb7))
* keep stale protocol streams ([d6837dd](https://github.com/propeller-heads/tycho-indexer/commit/d6837dddfc4083aa57b85cb13d4c46cc4777073e))
* skip snapshots on quick recoveries ([1648362](https://github.com/propeller-heads/tycho-indexer/commit/1648362bf944c94c6143e4a78c0289191ad1cb86))
* skip syncing extractors messages ([e272e70](https://github.com/propeller-heads/tycho-indexer/commit/e272e70a57e1dd3894c4cee71031a8ad672bcd74))


### Bug Fixes

* avoid unsubscribing multiple times ([7a23165](https://github.com/propeller-heads/tycho-indexer/commit/7a2316592babd431cd6cb56a9e972017b138b19e))

## [0.89.0](https://github.com/propeller-heads/tycho-indexer/compare/0.88.1...0.89.0) (2025-09-26)


### Features

* add `get_oldest_block` method in reorg buffer ([913ee16](https://github.com/propeller-heads/tycho-indexer/commit/913ee1605a261738c012f92bf9c3ce2a1efea941))
* decouple `ReorgBuffer` db commit tracking from finality ([#710](https://github.com/propeller-heads/tycho-indexer/issues/710)) ([a969a82](https://github.com/propeller-heads/tycho-indexer/commit/a969a82dd99d428083306497981bb6e19cea0a0a))
* have the extractor set the `db_committed_upto_block_height` ([711d80b](https://github.com/propeller-heads/tycho-indexer/commit/711d80bb3a3827846d2236379aa42bdd3543ea88))
* rework reorg buffer to track committed height independently of finality ([fd676f9](https://github.com/propeller-heads/tycho-indexer/commit/fd676f9a0d9dfb654328f025a245e6d9085a5c4f))


### Bug Fixes

* adjust revert-finalized height to equal to last not first purged block and improve error handling ([e5982d5](https://github.com/propeller-heads/tycho-indexer/commit/e5982d5d6749a3534d7acbf35f38eaf0230b3064))

## [0.88.1](https://github.com/propeller-heads/tycho-indexer/compare/0.88.0...0.88.1) (2025-09-25)

## [0.88.0](https://github.com/propeller-heads/tycho-indexer/compare/0.87.0...0.88.0) (2025-09-25)


### Features

* add `RPCMetadataProvider` for batch processing of RPC requests. ([286a0ec](https://github.com/propeller-heads/tycho-indexer/commit/286a0ec6d801a2d3e6bbaa8d139cbc66c8c65226))
* Add AllowanceSlotDetector ([8f9f3f9](https://github.com/propeller-heads/tycho-indexer/commit/8f9f3f95214255f0bc06c7656f136e2858e064d5))
* Add AllowanceSlotDetector ([#698](https://github.com/propeller-heads/tycho-indexer/issues/698)) ([ca7546b](https://github.com/propeller-heads/tycho-indexer/commit/ca7546b0ff6f1a4a9ef2fd56cf3dd7bff3c0016f))
* add back tracer.rs ([f8b4d54](https://github.com/propeller-heads/tycho-indexer/commit/f8b4d545583aa2f8f8bc4be91f85dd6923eb0efe))
* Add Balance overrides to Entrypoint Generator ([88df98b](https://github.com/propeller-heads/tycho-indexer/commit/88df98b0300e59ba104d877fe9ddf1c25cd9655d))
* add component pausing in `hook_dci` ([231a65e](https://github.com/propeller-heads/tycho-indexer/commit/231a65e6653dda788f34c8463148be291892a85d))
* add custom errors for improved debugging and handling ([289b34d](https://github.com/propeller-heads/tycho-indexer/commit/289b34dbbbf1cf2ccfd060fe51bc0fa624ece7bd))
* Add Display implementation for EntryPointWithTracingParams ([ca4c030](https://github.com/propeller-heads/tycho-indexer/commit/ca4c03010c61f878236e5a4dafca77e4ce408946))
* Add Display implementation for StorageSnapshotRequest ([cd4434a](https://github.com/propeller-heads/tycho-indexer/commit/cd4434aa9580af8bd3e41a392f09bc7bbfe3cdae))
* Add Grafana stack to docker-compose.yaml ([3f553b2](https://github.com/propeller-heads/tycho-indexer/commit/3f553b2c9404de9aeb0925643c0100945b2afd25))
* Add integration tests and debug logs ([7bcc400](https://github.com/propeller-heads/tycho-indexer/commit/7bcc4003930efaa68ea29a26726412a0cf92fee0))
* add logic to pause components on tracing failures ([7a00b5f](https://github.com/propeller-heads/tycho-indexer/commit/7a00b5ff5da454fca9365266a0b85fd9bb3450dc))
* add metadata response parser trait ([1ee35c1](https://github.com/propeller-heads/tycho-indexer/commit/1ee35c1fbf2c77459ba607619d2f189eb53e1f76))
* Add prune_addresses to RPCTracerParams ([014059a](https://github.com/propeller-heads/tycho-indexer/commit/014059a27900b71fda925281ec192588b7f5a8ad))
* Add retry on Metadata RPC calls ([52a6a9a](https://github.com/propeller-heads/tycho-indexer/commit/52a6a9a3160b2c4c66a8436e41c644ec1ed6541b))
* Add retry with exponential backoff for failed RPC calls ([e265a05](https://github.com/propeller-heads/tycho-indexer/commit/e265a05506975f29557d2f1be0a28f610efd95d2))
* Add slot offset to tracing result ([199ce78](https://github.com/propeller-heads/tycho-indexer/commit/199ce78400717c8ebd11aabfc862ebba0e81bbb2))
* add support for hook identifier in hook registries ([6a5f201](https://github.com/propeller-heads/tycho-indexer/commit/6a5f2018863170ed28ed789b663e9f2ce09285fc))
* add support for hooks ([#611](https://github.com/propeller-heads/tycho-indexer/issues/611)) ([a4e6725](https://github.com/propeller-heads/tycho-indexer/commit/a4e6725942def91f5e6a6c43de6d039aeeb7320e))
* Add test initialization for Univ4 DCI ([08ff9cc](https://github.com/propeller-heads/tycho-indexer/commit/08ff9cc474c682c9c993efdb842e7db1fdee4f18))
* add the ability to override state on DCI EntrypointParams ([e008974](https://github.com/propeller-heads/tycho-indexer/commit/e008974a632ae10e2ede7352087d9091565bd27b))
* add tracing to evaluate dci and hook_dci performance ([34f53ed](https://github.com/propeller-heads/tycho-indexer/commit/34f53ed3efaa7809ac62cfd9772fc2973934f80a))
* Add unit tests and improve packed slot detection in detect_retrigger ([3d0ecae](https://github.com/propeller-heads/tycho-indexer/commit/3d0ecae1160e872fe33b963ce4ba0a67de9d4ca8))
* Add V4MiniRouter runtime ([83d571b](https://github.com/propeller-heads/tycho-indexer/commit/83d571bc526d911170013955429fa6b55e2e1501))
* Enhance storage response handling in EVMBatchAccountExtractor ([732041a](https://github.com/propeller-heads/tycho-indexer/commit/732041a9101c913bc283c6e619aef8475ac50c65))
* Expose previous slot value field ([e06039c](https://github.com/propeller-heads/tycho-indexer/commit/e06039c6e7a7eda1dd62792c64b000d8a154674a))
* Expose separate startup timeout for protocols. ([749fb71](https://github.com/propeller-heads/tycho-indexer/commit/749fb711788f883317932563988685c1bd045759))
* extract reason for failed request ([6cc91ea](https://github.com/propeller-heads/tycho-indexer/commit/6cc91ea7b8317203f3e39c41dea16ef029a792be))
* Fix chrono deprecation errors post update ([40a7b39](https://github.com/propeller-heads/tycho-indexer/commit/40a7b39a76a463a613d02de155ebfd16b292d155))
* Fix EntrypointTracingParams index on Database ([ad2af1f](https://github.com/propeller-heads/tycho-indexer/commit/ad2af1f7758c1c999a411be74e81e3eddcfc81fa))
* Handle initialization and errors ([ebe0da2](https://github.com/propeller-heads/tycho-indexer/commit/ebe0da288f659c65f3dd33b3731b762e6e643efd))
* **hook-support:** implement metadata generator for Euler Swap ([a392dc0](https://github.com/propeller-heads/tycho-indexer/commit/a392dc0ff7f87aed8eca697e2649887ef42e6839))
* implement `DefaultUniswapV4HookOrchestrator` ([5375a7d](https://github.com/propeller-heads/tycho-indexer/commit/5375a7de2d74620d95d9bdabdac8a1a6e5ce21f1))
* implement a retry logic in the tracer ([d65b2c6](https://github.com/propeller-heads/tycho-indexer/commit/d65b2c67a1b33decabe800e37e53a6c9c3b18646))
* implement Balance slot detector ([69bab67](https://github.com/propeller-heads/tycho-indexer/commit/69bab67a9640a243c31b82b101d3de1227dadb01))
* Implement default SwapAmountEstimator ([aa8b41d](https://github.com/propeller-heads/tycho-indexer/commit/aa8b41d570db152c67e75fa140cbc9200eaf0864))
* implement Euler metadata parser ([44c52e3](https://github.com/propeller-heads/tycho-indexer/commit/44c52e3ff202e292c65fdfa87885fb391229b10a))
* Implement HookEntrypointGenerator ([f70f7fa](https://github.com/propeller-heads/tycho-indexer/commit/f70f7fa791af1b3c2fca0016df7f56d8392040b0))
* Implement Hooks permission detector ([e3929a4](https://github.com/propeller-heads/tycho-indexer/commit/e3929a486c1209e5ffbe4d69fa4f690872402c65))
* implement metadata orchestrator ([14fa1df](https://github.com/propeller-heads/tycho-indexer/commit/14fa1df171300f32bb2c8eb557b547da1d63c989))
* implement metadata registries ([cfb06f2](https://github.com/propeller-heads/tycho-indexer/commit/cfb06f2f33e2695c22d71bf773976e45001585a6))
* Implement UniwapV4 Hooks DCI with core features ([4c8c71a](https://github.com/propeller-heads/tycho-indexer/commit/4c8c71a9d78ff466cd498cdffea54af9213e1e01))
* improve account extractor logging ([7923ee4](https://github.com/propeller-heads/tycho-indexer/commit/7923ee479bcdb86ce4ca7c18a475eadfb7830d7b))
* Improve logs and instrumentation on dci ([579985f](https://github.com/propeller-heads/tycho-indexer/commit/579985f581830873b427fa19102646d437052363))
* Improve retrigger detection with offset-aware address comparison ([f0b1dbb](https://github.com/propeller-heads/tycho-indexer/commit/f0b1dbb7b270b65e9a77277385f2505e6705bcee))
* introduce a new error type for tracing failure ([3c3a65d](https://github.com/propeller-heads/tycho-indexer/commit/3c3a65d7acc2c97ccde3c77d595b376b274d102e))
* Load Euler hooks from JSON, rename uniswap-v4 protocol system ([e5477ab](https://github.com/propeller-heads/tycho-indexer/commit/e5477ab511dfcb814c1ec2027deadf2f7ec645f3))
* Make AccountDelta code and change private ([5ec44b2](https://github.com/propeller-heads/tycho-indexer/commit/5ec44b2b23396fe679767486b9857e83b909f0f2))
* Make BalanceSlotDetector obligatory and configurable on EntrypointGenerator ([02e8fdd](https://github.com/propeller-heads/tycho-indexer/commit/02e8fdd4c14791cbaddd9649cb9cb17ffd69aefd))
* merge tracing results on inserts ([7a6ff8f](https://github.com/propeller-heads/tycho-indexer/commit/7a6ff8f2a4319ffba9e843086ceff1dd96099207))
* reduce batch size to 50 ([6ea6042](https://github.com/propeller-heads/tycho-indexer/commit/6ea60429a3ce54d6ce892adf4db4c3c4eeb61064))
* reduce time version to fix conflict with tycho-simulation ([0838b85](https://github.com/propeller-heads/tycho-indexer/commit/0838b8506b57a0fcb092b638e1a9f2f8d3c4f787))
* Rename structs and fix StorageOverride usage ([7afba1e](https://github.com/propeller-heads/tycho-indexer/commit/7afba1e26fd23f346cc91dd89929ddaa057685c7))
* Retry on transient rpc error. ([a56d8d9](https://github.com/propeller-heads/tycho-indexer/commit/a56d8d93df5a6fa572094e206d26bf585487d117))
* return separated error on tracing ([40c6ff6](https://github.com/propeller-heads/tycho-indexer/commit/40c6ff6a4fb0310932e5c13d33371b05c5c190fb))
* Track all storage slots on DCI, excluding tokens or blacklist addr ([886b3da](https://github.com/propeller-heads/tycho-indexer/commit/886b3daa7a997d7b80a1413f2a8ea7259fed0722))
* Update balances and limits percentages to cover more cases ([87a09ee](https://github.com/propeller-heads/tycho-indexer/commit/87a09ee0d9f49c08919a3bfe1419b4ee20bc6704))
* Upgrade alloy, contracts, ethrpc, ethcontract ([847177d](https://github.com/propeller-heads/tycho-indexer/commit/847177d80e45c512820128d7590e58085b10b9bd))
* Use BigInt instead of U256 ([4515b0c](https://github.com/propeller-heads/tycho-indexer/commit/4515b0ca6a023f54e354f1c5909b19b2338b7711))
* Use custom contract to fetch Euler limits with the correct values ([#690](https://github.com/propeller-heads/tycho-indexer/issues/690)) ([2d24a09](https://github.com/propeller-heads/tycho-indexer/commit/2d24a093f46decfd50dc8bc709ba48eac5a06480))
* Use hook address for Entrypoint id ([ed251c6](https://github.com/propeller-heads/tycho-indexer/commit/ed251c668ac75f2000e884d88970edf49c54dd49))
* Use specific RPC for tracing ([6f856cb](https://github.com/propeller-heads/tycho-indexer/commit/6f856cb9bf1bf1ba9489b2f16a1bf29b5a0bfadf))
* Use tycho-substreams 0.5.0 ([5e2ab18](https://github.com/propeller-heads/tycho-indexer/commit/5e2ab180e48e6c7bf5c1618673dd388d85b46c71))
* Use unique entrypoint IDs per hook on Entrypoint Generator ([5965541](https://github.com/propeller-heads/tycho-indexer/commit/59655416476351a59ea5c565d9e7c474857dc9f6))
* validate that the found slots actually modify the balance ([410610b](https://github.com/propeller-heads/tycho-indexer/commit/410610bde1d21351bbe1811b0c1e9d9946ba8155))


### Bug Fixes

* adapt dci `extract_tracked_updates` ([ba5d56a](https://github.com/propeller-heads/tycho-indexer/commit/ba5d56a38e57cd1f9c6f1b43535a9403d21799c2))
* Add and use constructor for AccountDelta ([4116686](https://github.com/propeller-heads/tycho-indexer/commit/4116686ce077d8d4d314698e5fdff5c2e00f1adf))
* add prune_addresses to the generated RPCTracerParams ([104098d](https://github.com/propeller-heads/tycho-indexer/commit/104098d0b81447588d08dde8273922c0db98bac6))
* Blacklist permit2 ([13a1cab](https://github.com/propeller-heads/tycho-indexer/commit/13a1cabe360bbb9c91b07f6742507e840219e2d6))
* cargo fmt + test fixes ([ea61492](https://github.com/propeller-heads/tycho-indexer/commit/ea614927b6a339d58d0ec6f873d827b70aaf7773))
* correct tracked_contract cache None semantics ([6f2b241](https://github.com/propeller-heads/tycho-indexer/commit/6f2b241fdbd724153650354cacdfb5a6d98cc2d9))
* correctly merge attributes when inserting "pausing". ([771ad9a](https://github.com/propeller-heads/tycho-indexer/commit/771ad9a98895efc7ce4779a71a733e9149bebcb0))
* DCI logs and traces ([dbacdbc](https://github.com/propeller-heads/tycho-indexer/commit/dbacdbc2c11732d64b1ca2b5c00ff930bf3fb94b))
* detect account dependencies from non-call opcodes in EVM tracer ([ab4f93f](https://github.com/propeller-heads/tycho-indexer/commit/ab4f93fb58825e83d2b44abd9bb206f0594c2b63))
* Do not overwrite Component State when adding Entrypoints ([786f661](https://github.com/propeller-heads/tycho-indexer/commit/786f66101bb11d13a022fe014ed59fd81753df67))
* Enable TLS on Alloy ([153b501](https://github.com/propeller-heads/tycho-indexer/commit/153b501606a5464c859ad538b5a5106590ea8758))
* faulty client rpc test ([49f7953](https://github.com/propeller-heads/tycho-indexer/commit/49f7953ad0da2b4a8a3aef11777707fac32d21ff))
* Fetch new accounts even if they don't have storage slots ([77df048](https://github.com/propeller-heads/tycho-indexer/commit/77df048b394e5832c3c944ecf4f28867573aea71))
* Fetch new storage slots detected after a new tracing ([1a93cbf](https://github.com/propeller-heads/tycho-indexer/commit/1a93cbf5ae1516ffe0bbbd70cd662d506c8f2273))
* fix CI ([c6131a6](https://github.com/propeller-heads/tycho-indexer/commit/c6131a6c928f07d99c33af96127fab6105df1168))
* Fix EulerMetadataGenerator to use hooks address instead of ComponentId ([5947240](https://github.com/propeller-heads/tycho-indexer/commit/5947240d5a5e8d1b16b99b0a2efea9b50a5ec92c))
* Fix Limit entrypoint insertion ([42ec085](https://github.com/propeller-heads/tycho-indexer/commit/42ec085b8484a13015e19f42d27da6a890c1d02c))
* Fix TLS not working after bumping Alloy version ([#706](https://github.com/propeller-heads/tycho-indexer/issues/706)) ([1355379](https://github.com/propeller-heads/tycho-indexer/commit/1355379cfcef553fcb675ab0f7ff726e166eac80))
* Fix token ordering and calldata generation ([0315746](https://github.com/propeller-heads/tycho-indexer/commit/031574621dba13219e79bd61a0376ac75c279a7b))
* fmt & badly closed braces ([994d27d](https://github.com/propeller-heads/tycho-indexer/commit/994d27d9ab84d322c8eeed2afcb5fde9331c47ea))
* Handle unordered tokens on EntrypointGenerator ([66efdf7](https://github.com/propeller-heads/tycho-indexer/commit/66efdf7299b3ac8b15a76797e1dcc6972ba24087))
* Improve interfaces, fix EntrypointId and attributes ([4ffd876](https://github.com/propeller-heads/tycho-indexer/commit/4ffd8768e5d3f14afb9331e89a05f19c83510ef8))
* increase client timeout ([674caf9](https://github.com/propeller-heads/tycho-indexer/commit/674caf9b4e3d05fc855fee02f87f9a880252d901))
* increase default max missed blocks on mainnet ([3114dd6](https://github.com/propeller-heads/tycho-indexer/commit/3114dd6618e933761c3b987e1ef5f381bad03d7a))
* increase heartbeat timeout ([#703](https://github.com/propeller-heads/tycho-indexer/issues/703)) ([93560e9](https://github.com/propeller-heads/tycho-indexer/commit/93560e9af872bb90d9ccb7304b4eb56baff6342f))
* Inject on block Entrypoints instead of only EntrypointParams ([5b3e267](https://github.com/propeller-heads/tycho-indexer/commit/5b3e2679ed89a9c4d2b9c2a29dc394d871c341ae))
* make `TracingResult` deserialization backward compatible ([868202d](https://github.com/propeller-heads/tycho-indexer/commit/868202d5684f8bb38d40b9e0d20092fce724dd5b))
* make `TracingResult` deserialization backward compatible ([#707](https://github.com/propeller-heads/tycho-indexer/issues/707)) ([6695c1e](https://github.com/propeller-heads/tycho-indexer/commit/6695c1edab0aca1ef3d8a6c929b52040862266ee))
* make clippy happy ([8a3fb8d](https://github.com/propeller-heads/tycho-indexer/commit/8a3fb8d6e405b5942d304c12ef686fb31a69a99f))
* Override the correct contract for token balances ([4054c2a](https://github.com/propeller-heads/tycho-indexer/commit/4054c2a38ee482a3ef4c43cba6ff79c2d426724c))
* Post rebase fixes ([7bfada4](https://github.com/propeller-heads/tycho-indexer/commit/7bfada4842015fab783a9124a5f6c3d7a26bb44e))
* prevent entrypoint regeneration for components that were already traced. ([319eef8](https://github.com/propeller-heads/tycho-indexer/commit/319eef87b0e1e59ef393f3e04581e92fe770590d))
* properly handle finality for layers with set values ([bb5c02d](https://github.com/propeller-heads/tycho-indexer/commit/bb5c02df88fc27bf2983cee864b8ef12ccdaf553))
* properly order tokens when parsing euler response ([f058dc8](https://github.com/propeller-heads/tycho-indexer/commit/f058dc803a603d8b93e54d426e6a514b322ecd9e))
* Reduce swap amounts to up to 10% of the reserves ([aef8e5c](https://github.com/propeller-heads/tycho-indexer/commit/aef8e5c17e9ba0754188ec6c028c160ebcc47bd8))
* Reduce the percentages used to generate swap amounts ([1cca1f7](https://github.com/propeller-heads/tycho-indexer/commit/1cca1f72d8e1a533a2fd40f61f74eea6746f5cf3))
* Remove From<StorageKey> implementation ([373cc59](https://github.com/propeller-heads/tycho-indexer/commit/373cc5974adbabe4da253341e6483ed36dfc4b2e))
* remove unnecessary spans ([f83d424](https://github.com/propeller-heads/tycho-indexer/commit/f83d424138aebc06b94399d107f94d190cb3b7c7))
* retry on Balance detector if the call returns an RPC retriable error ([2878e51](https://github.com/propeller-heads/tycho-indexer/commit/2878e51f4d42fcf2ef7256d6a9f18b14c274bd40))
* retry on Balance detector if the call returns an RPC retriable error ([#708](https://github.com/propeller-heads/tycho-indexer/issues/708)) ([b25a266](https://github.com/propeller-heads/tycho-indexer/commit/b25a2661f3bfab33ed7103bbe6cbf8a89c65a48e))
* test_traced_entry_point_display test ([f997b66](https://github.com/propeller-heads/tycho-indexer/commit/f997b66913436ea0f0fa07c0d83ea09ac02542c7))
* Update Entrypoint DB Schema to include external_id on uniqueness validation ([c91ed9b](https://github.com/propeller-heads/tycho-indexer/commit/c91ed9b459c2ea40597f2dd394309ce208113b49))
* wrongly dereferenced offset var ([c0d645d](https://github.com/propeller-heads/tycho-indexer/commit/c0d645d7e00bb3931d6c641ebb565912f3bef638))

## [0.87.0](https://github.com/propeller-heads/tycho-indexer/compare/0.86.0...0.87.0) (2025-09-23)


### Features

* enhance RpcCache with memory size weighing ([d33e6aa](https://github.com/propeller-heads/tycho-indexer/commit/d33e6aa7d02b9ca290c879a87ae92cdc15f39c11))
* enhance RpcCache with memory size weighing ([#700](https://github.com/propeller-heads/tycho-indexer/issues/700)) ([58d352a](https://github.com/propeller-heads/tycho-indexer/commit/58d352a3f56414b9e697d74163c4ba7e318efb40))

## [0.86.0](https://github.com/propeller-heads/tycho-indexer/compare/0.85.0...0.86.0) (2025-09-19)


### Features

* handle end-of-stream as a valid state, instead of as an error ([3463428](https://github.com/propeller-heads/tycho-indexer/commit/34634287f6549168cf2c079b92f71ad099efb0dd))


### Bug Fixes

* create an async_main function so that run_indexer can create its own tokio runtime ([64c7720](https://github.com/propeller-heads/tycho-indexer/commit/64c77200f93d0607f5a62bd78fa7bbb4bfe2503d))

## [0.85.0](https://github.com/propeller-heads/tycho-indexer/compare/0.84.0...0.85.0) (2025-09-18)


### Features

* Expose retry configurations on TychoStreamBuilder ([e677e13](https://github.com/propeller-heads/tycho-indexer/commit/e677e13e9a15bce36925d05cf1c34fdf01122531))
* Expose retry configurations on TychoStreamBuilder ([#695](https://github.com/propeller-heads/tycho-indexer/issues/695)) ([ccbc67d](https://github.com/propeller-heads/tycho-indexer/commit/ccbc67d32eba32c9c9de3ee97d684e20959dbf13))


### Bug Fixes

* reconnection test timing issues. ([780d967](https://github.com/propeller-heads/tycho-indexer/commit/780d967779f1fdab42c7c5f173fca251f058fb6f))

## [0.84.0](https://github.com/propeller-heads/tycho-indexer/compare/0.83.4...0.84.0) (2025-09-18)


### Features

* communicate websocket server errors correctly ([e3b1d1e](https://github.com/propeller-heads/tycho-indexer/commit/e3b1d1e6e3042ce73fba9afbfc8957a5e9758746))
* communicate websocket server errors correctly ([#694](https://github.com/propeller-heads/tycho-indexer/issues/694)) ([75a9a37](https://github.com/propeller-heads/tycho-indexer/commit/75a9a37e066debcb24c74ec6c886e10ceb4c2f79))

## [0.83.4](https://github.com/propeller-heads/tycho-indexer/compare/0.83.3...0.83.4) (2025-09-11)


### Bug Fixes

* prevent race condition in extractors/server startup sequence ([5a1af8e](https://github.com/propeller-heads/tycho-indexer/commit/5a1af8ead6e9eca9906b914a8b81c4b5e13f0da7))
* prevent race condition in extractors/server startup sequence ([#689](https://github.com/propeller-heads/tycho-indexer/issues/689)) ([90023a1](https://github.com/propeller-heads/tycho-indexer/commit/90023a13554a49e09c3e4e84be3c5d2424580506))

## [0.83.3](https://github.com/propeller-heads/tycho-indexer/compare/0.83.2...0.83.3) (2025-09-10)


### Bug Fixes

* Add protocol_system to RunSpkgArgs ([7673d41](https://github.com/propeller-heads/tycho-indexer/commit/7673d41ea2982c8486712fb0ae0bf2a12d847235))
* Add protocol_system to RunSpkgArgs ([#688](https://github.com/propeller-heads/tycho-indexer/issues/688)) ([6afbf17](https://github.com/propeller-heads/tycho-indexer/commit/6afbf17578801d3b14d7a67805c682d3cdd7d3dc))

## [0.83.2](https://github.com/propeller-heads/tycho-indexer/compare/0.83.1...0.83.2) (2025-09-09)


### Bug Fixes

* buffer overflow handling with force unsubscribe ([216ed42](https://github.com/propeller-heads/tycho-indexer/commit/216ed42a67ead3f36ce671e96cd6d7f01c4f018a))
* handle buffer overflow in client ([#686](https://github.com/propeller-heads/tycho-indexer/issues/686)) ([a34668b](https://github.com/propeller-heads/tycho-indexer/commit/a34668b68c3dc7fac5e3892619ae4ab69f19762d))

## [0.83.1](https://github.com/propeller-heads/tycho-indexer/compare/0.83.0...0.83.1) (2025-09-05)


### Bug Fixes

* correctly get all values from cache when needed. ([ebe6ff5](https://github.com/propeller-heads/tycho-indexer/commit/ebe6ff57504722c71c95db77a7b8b5e584c221b9))
* correctly get all values from cache when needed. ([#673](https://github.com/propeller-heads/tycho-indexer/issues/673)) ([b8250fa](https://github.com/propeller-heads/tycho-indexer/commit/b8250fa9dcad90a4b2237c23753b3a158c0aba11))

## [0.83.0](https://github.com/propeller-heads/tycho-indexer/compare/0.82.0...0.83.0) (2025-08-30)


### Features

* add a retry logic for account extraction in DCI ([fc6d226](https://github.com/propeller-heads/tycho-indexer/commit/fc6d22696baf91d123cfc8c7ea08de4093995d95))
* add retry loop around account extraction in DCI ([#671](https://github.com/propeller-heads/tycho-indexer/issues/671)) ([f529864](https://github.com/propeller-heads/tycho-indexer/commit/f5298642ac0b86ba6c6dafa5770bbeb6c1192524))

## [0.82.0](https://github.com/propeller-heads/tycho-indexer/compare/0.81.6...0.82.0) (2025-08-26)


### Features

* add user identity in metrics and spans ([382f408](https://github.com/propeller-heads/tycho-indexer/commit/382f408dd8a93b4a41a9f5f2dfc22ef69ce699f2))
* add user identity in metrics and spans ([#669](https://github.com/propeller-heads/tycho-indexer/issues/669)) ([1aadf8d](https://github.com/propeller-heads/tycho-indexer/commit/1aadf8de18c055b0a4eea8c8f544130b8d7ccdaf))

## [0.81.6](https://github.com/propeller-heads/tycho-indexer/compare/0.81.5...0.81.6) (2025-08-13)


### Bug Fixes

* Simplify GetAmountOutParams to hold Bytes only ([cdc8509](https://github.com/propeller-heads/tycho-indexer/commit/cdc85098fddbb7aad8fe95bf98c02dd8ee2676f5))
* Simplify GetAmountOutParams to hold Bytes only ([#653](https://github.com/propeller-heads/tycho-indexer/issues/653)) ([c35d458](https://github.com/propeller-heads/tycho-indexer/commit/c35d458fae12d98ff1818918e51c3232a3134413))

## [0.81.5](https://github.com/propeller-heads/tycho-indexer/compare/0.81.4...0.81.5) (2025-08-12)


### Bug Fixes

* Derive Debug in SignedQuote ([31fd3c7](https://github.com/propeller-heads/tycho-indexer/commit/31fd3c7e715d61e586568f1b818ec5df2946fe28))
* Derive Debug in SignedQuote ([#650](https://github.com/propeller-heads/tycho-indexer/issues/650)) ([29fe8ad](https://github.com/propeller-heads/tycho-indexer/commit/29fe8ad75277e833bb80e0bbf8657ada5a2986a1))

## [0.81.4](https://github.com/propeller-heads/tycho-indexer/compare/0.81.3...0.81.4) (2025-08-11)


### Bug Fixes

* WsDeltas client forever blocking subscribe. ([1b6ff33](https://github.com/propeller-heads/tycho-indexer/commit/1b6ff333150613031dc2ffc1b0c1b36170c2fdea))
* WsDeltas client forever blocking subscribe. ([#648](https://github.com/propeller-heads/tycho-indexer/issues/648)) ([4c2e989](https://github.com/propeller-heads/tycho-indexer/commit/4c2e989e3f0997646fe82834d237cc1a0b6c9a67))

## [0.81.3](https://github.com/propeller-heads/tycho-indexer/compare/0.81.2...0.81.3) (2025-08-08)


### Bug Fixes

* buggy transitions to advanced ([904314e](https://github.com/propeller-heads/tycho-indexer/commit/904314e5d20485d768e0abb0be3080d30220c430))
* For now error if we are left with only advanced streams. ([40d3670](https://github.com/propeller-heads/tycho-indexer/commit/40d367092a125ba8a50838b3dc814b527e69fe57))
* improve stale synchronizer detection in BlockSynchronizer ([0991d3f](https://github.com/propeller-heads/tycho-indexer/commit/0991d3f84989a211c55bd092c8509f530cd0efb3))
* prevent premature main loop exit when all synchronizers are delayed ([d04ad69](https://github.com/propeller-heads/tycho-indexer/commit/d04ad6909f198536e3d99956d9de39063b785f57))
* stale synchronizer detection ([#647](https://github.com/propeller-heads/tycho-indexer/issues/647)) ([77f45d4](https://github.com/propeller-heads/tycho-indexer/commit/77f45d437d5df1e73d463403301671276df6e4f1))

## [0.81.2](https://github.com/propeller-heads/tycho-indexer/compare/0.81.1...0.81.2) (2025-08-06)


### Bug Fixes

* Resolve WebSocket deadlock with async subscription handling ([ba5715f](https://github.com/propeller-heads/tycho-indexer/commit/ba5715f92a81a3242696419fda15c7263e62a3f1))
* Websocket deadlocks ([#642](https://github.com/propeller-heads/tycho-indexer/issues/642)) ([cfde3ba](https://github.com/propeller-heads/tycho-indexer/commit/cfde3ba0cad753a1e05f98aab605f2358e9e80a4))


### Performance Improvements

* Eliminate mutex from WebSocket subscribers for lock-free access ([2a11192](https://github.com/propeller-heads/tycho-indexer/commit/2a11192f1a5dce89f3f2b40d3f158bbb2cd0a494))

## [0.81.1](https://github.com/propeller-heads/tycho-indexer/compare/0.81.0...0.81.1) (2025-08-06)


### Bug Fixes

* add proper deltas subscription cleanup and fix tests ([14a682f](https://github.com/propeller-heads/tycho-indexer/commit/14a682f013dba94236e2637209439d700f224cd1))
* ensure state_sync cleanup runs on all exit paths ([c482b23](https://github.com/propeller-heads/tycho-indexer/commit/c482b236ae30022744e5c852d0d7e8a9e2d6b33e))
* prevent unnecessary warnings in deltas client unsubscribe ([0fa5c1b](https://github.com/propeller-heads/tycho-indexer/commit/0fa5c1bdd671f26b555700b1a9e17dcd11fff297))
* properly close WebSocket client and log close errors ([db0d27e](https://github.com/propeller-heads/tycho-indexer/commit/db0d27e26641e02c6e641697c70449945f15d214))
* tycho-client shutdown cleanup and improve error handling ([#637](https://github.com/propeller-heads/tycho-indexer/issues/637)) ([2043997](https://github.com/propeller-heads/tycho-indexer/commit/2043997db77bdb68177a2a8507d1cfa80f4f912b))

## [0.81.0](https://github.com/propeller-heads/tycho-indexer/compare/0.80.0...0.81.0) (2025-07-29)


### Features

* Change return of as_indicatively_priced to Result ([c34c6bf](https://github.com/propeller-heads/tycho-indexer/commit/c34c6bf6424af37407926f135545bd76d06525ed))
* Change return of as_indicatively_priced to Result ([#645](https://github.com/propeller-heads/tycho-indexer/issues/645)) ([803575b](https://github.com/propeller-heads/tycho-indexer/commit/803575bcbd287840c00bcd90516272a1181e0b23))

## [0.80.0](https://github.com/propeller-heads/tycho-indexer/compare/0.79.0...0.80.0) (2025-07-29)


### Features

* Add as_indicatively_priced in ProtocolSim ([1d71bb2](https://github.com/propeller-heads/tycho-indexer/commit/1d71bb2e316d27ef089a81fe4200cbdb476e6c11))
* Add as_indicatively_priced in ProtocolSim  ([#644](https://github.com/propeller-heads/tycho-indexer/issues/644)) ([30e1d57](https://github.com/propeller-heads/tycho-indexer/commit/30e1d57de680f4cab529e3c844af330e2f181504))
* Implement IndicativelyPriced for ProtocolSim ([7a3571e](https://github.com/propeller-heads/tycho-indexer/commit/7a3571eadaf3ff95b5180284237a3e952052b06a))
* Make IndicativelyPriced a super trait of ProtocolSim ([7bce804](https://github.com/propeller-heads/tycho-indexer/commit/7bce8048c14b1fa3d6eb99715b19c62ccb4bee07))

## [0.79.0](https://github.com/propeller-heads/tycho-indexer/compare/0.78.6...0.79.0) (2025-07-25)


### Features

* Add default to FeedMessage ([cedc469](https://github.com/propeller-heads/tycho-indexer/commit/cedc469222ccf75912ef2a6a6258f9141b817d7f))
* Add default to FeedMessage ([#639](https://github.com/propeller-heads/tycho-indexer/issues/639)) ([b120186](https://github.com/propeller-heads/tycho-indexer/commit/b120186dcfaefe7f6469976dd506920724d1f9e4))


### Bug Fixes

* Add gas_usage to Token ([66a91b5](https://github.com/propeller-heads/tycho-indexer/commit/66a91b5b18446b69c5490a68f6e0025ab2cb4bdf))

## [0.78.6](https://github.com/propeller-heads/tycho-indexer/compare/0.78.5...0.78.6) (2025-07-25)

## [0.78.5](https://github.com/propeller-heads/tycho-indexer/compare/0.78.4...0.78.5) (2025-07-24)


### Bug Fixes

* Remove missing state query on versioning ([e0db7c0](https://github.com/propeller-heads/tycho-indexer/commit/e0db7c088f70d0d2af54ccad0a42b09b10ed520f))
* Remove missing state query on versioning ([#636](https://github.com/propeller-heads/tycho-indexer/issues/636)) ([df08a88](https://github.com/propeller-heads/tycho-indexer/commit/df08a88e8c622d7b9f82ef75e1424ca1d65c6fb7))

## [0.78.4](https://github.com/propeller-heads/tycho-indexer/compare/0.78.3...0.78.4) (2025-07-24)


### Bug Fixes

* Specify minimum rust version ([de37e86](https://github.com/propeller-heads/tycho-indexer/commit/de37e86220d32f678a77082cfe377d01402ca125))
* Specify minimum rust version ([#638](https://github.com/propeller-heads/tycho-indexer/issues/638)) ([a052d76](https://github.com/propeller-heads/tycho-indexer/commit/a052d76b883228532f1d7f00d67231e90c4da02e))

## [0.78.3](https://github.com/propeller-heads/tycho-indexer/compare/0.78.2...0.78.3) (2025-07-22)

## [0.78.2](https://github.com/propeller-heads/tycho-indexer/compare/0.78.1...0.78.2) (2025-07-21)

## [0.78.1](https://github.com/propeller-heads/tycho-indexer/compare/0.78.0...0.78.1) (2025-07-18)


### Bug Fixes

* disable DCI queries on Base and Unichain ([7cbc552](https://github.com/propeller-heads/tycho-indexer/commit/7cbc55250367c65872d4213e9538ccbb3875d56f))
* disable DCI queries on Base and Unichain ([#632](https://github.com/propeller-heads/tycho-indexer/issues/632)) ([7d94b96](https://github.com/propeller-heads/tycho-indexer/commit/7d94b96996de5c8dcec182d9cc92e5bb5cb8888f))

## [0.78.0](https://github.com/propeller-heads/tycho-indexer/compare/0.77.2...0.78.0) (2025-07-18)


### Features

* Add IndicativelyPriced trait ([14d4b47](https://github.com/propeller-heads/tycho-indexer/commit/14d4b4760c3886c30bdbe8d4e7d7e0c3ed7401ca))
* Add IndicativelyPriced trait ([#631](https://github.com/propeller-heads/tycho-indexer/issues/631)) ([60fa7b2](https://github.com/propeller-heads/tycho-indexer/commit/60fa7b2a46385200e49d58cc0958ccc9642cb1f8))

## [0.77.2](https://github.com/propeller-heads/tycho-indexer/compare/0.77.1...0.77.2) (2025-07-17)


### Bug Fixes

* Generalise FeedMessage to hold a HeaderLike and not a BlockHeader ([8394336](https://github.com/propeller-heads/tycho-indexer/commit/839433620169c764aa27cacdb79f791cd96daa71))
* Generalise FeedMessage to hold a HeaderLike and not a BlockHeader ([#624](https://github.com/propeller-heads/tycho-indexer/issues/624)) ([85ae669](https://github.com/propeller-heads/tycho-indexer/commit/85ae669a6441ed4ecedac9d79ade35fa63d0edb8))
* Rename ts to block_number_or_timestamp ([58a0128](https://github.com/propeller-heads/tycho-indexer/commit/58a012848295bf748c1bed277f9557f1b1ede46c))

## [0.77.1](https://github.com/propeller-heads/tycho-indexer/compare/0.77.0...0.77.1) (2025-07-16)


### Bug Fixes

* Revert: "fix: Correctly set Url scheme in TychoStreamBuilder" ([#627](https://github.com/propeller-heads/tycho-indexer/issues/627)) ([d8f2b0b](https://github.com/propeller-heads/tycho-indexer/commit/d8f2b0b83fde53b1a47e0d8b4f4f6b7caa96aa94))

## [0.77.0](https://github.com/propeller-heads/tycho-indexer/compare/0.76.1...0.77.0) (2025-07-14)


### Features

* Generalise Header and add ProtocolSim ([#619](https://github.com/propeller-heads/tycho-indexer/issues/619)) ([d6c7be4](https://github.com/propeller-heads/tycho-indexer/commit/d6c7be437c54c8369740d577442657d3e6c1350e))
* Generalise Header by creating a HeaderLike trait ([828f4d4](https://github.com/propeller-heads/tycho-indexer/commit/828f4d44bba6fec9be3e25546eff52391dd808fa))
* Move ProtocolSim from simulation to tycho-common ([97e8e08](https://github.com/propeller-heads/tycho-indexer/commit/97e8e08db5ee442af13c7dd25135bd6edb3fb269))

## [0.76.1](https://github.com/propeller-heads/tycho-indexer/compare/0.76.0...0.76.1) (2025-07-11)


### Bug Fixes

* Correctly set Url scheme in TychoStreamBuilder ([bac1975](https://github.com/propeller-heads/tycho-indexer/commit/bac19756741194b2ecc5902b4fc4e92f20585464))
* Correctly set Url scheme in TychoStreamBuilder ([#621](https://github.com/propeller-heads/tycho-indexer/issues/621)) ([839e919](https://github.com/propeller-heads/tycho-indexer/commit/839e91917da0a7c9a1ad340fd7fed205700a6f36))

## [0.76.0](https://github.com/propeller-heads/tycho-indexer/compare/0.75.1...0.76.0) (2025-07-11)


### Features

* Add id and wrapped_native_token to Chain ([9c79c8f](https://github.com/propeller-heads/tycho-indexer/commit/9c79c8f7a3501e1edd2b86cff43460729b62991f))
* Add timestamp to Header ([684834a](https://github.com/propeller-heads/tycho-indexer/commit/684834a8f053907ad9547b68d870d2e2c4117249))
* Remove duplicated models ([#618](https://github.com/propeller-heads/tycho-indexer/issues/618)) ([b528e9b](https://github.com/propeller-heads/tycho-indexer/commit/b528e9b38a9e321f295f09bcb90d9760a7e3c591))
* Rename CurrencyToken to Token ([a30c046](https://github.com/propeller-heads/tycho-indexer/commit/a30c046ac7b022f7a816545486af81e724863f07))

## [0.75.1](https://github.com/propeller-heads/tycho-indexer/compare/0.75.0...0.75.1) (2025-07-11)


### Bug Fixes

* correctly handle reverts in DCI cache ([aee8b7d](https://github.com/propeller-heads/tycho-indexer/commit/aee8b7d70ef65977b4cd9343cf9f5473643fb154))
* correctly handle reverts in DCI cache ([#620](https://github.com/propeller-heads/tycho-indexer/issues/620)) ([ab4c838](https://github.com/propeller-heads/tycho-indexer/commit/ab4c838fafeb73138f386249c9af75cd154215a3))

## [0.75.0](https://github.com/propeller-heads/tycho-indexer/compare/0.74.0...0.75.0) (2025-07-07)


### Features

* Support cloning FeedMessage ([408a349](https://github.com/propeller-heads/tycho-indexer/commit/408a349ff9b282276402c5090a550b7edc38535b))
* Support cloning FeedMessage ([#612](https://github.com/propeller-heads/tycho-indexer/issues/612)) ([ab32f4a](https://github.com/propeller-heads/tycho-indexer/commit/ab32f4a0d3458ceb186835d6318beadb230735e3))

## [0.74.0](https://github.com/propeller-heads/tycho-indexer/compare/0.73.0...0.74.0) (2025-06-30)


### Features

* **client-py:** add missing endpoints ([efd6d96](https://github.com/propeller-heads/tycho-indexer/commit/efd6d96a8d09d6313af77ca42bda4b9ac9d1e8d0))
* **client-py:** add pagination to all endpoints ([f7c98b2](https://github.com/propeller-heads/tycho-indexer/commit/f7c98b2658f5b4c047db0842d1ca1bab236c67f0))
* **client-py:** update python RPC client ([#606](https://github.com/propeller-heads/tycho-indexer/issues/606)) ([f6a4ef5](https://github.com/propeller-heads/tycho-indexer/commit/f6a4ef54095d5e5d2b550d81316ce282413268c0))

## [0.73.0](https://github.com/propeller-heads/tycho-indexer/compare/0.72.1...0.73.0) (2025-06-27)


### Features

* mark creation_tx as deprecated on ResponseAccount ([224c4b8](https://github.com/propeller-heads/tycho-indexer/commit/224c4b883a6733e310b9feb6c06ecc81c5d7338d))
* **storage:** remove join to transaction table in get_contracts ([0e6e9e5](https://github.com/propeller-heads/tycho-indexer/commit/0e6e9e5071ca722887266a8861862a66ddb6dea6))


### Bug Fixes

* correctly fill the reorg buffer ([10c951d](https://github.com/propeller-heads/tycho-indexer/commit/10c951def0487fa97baf4e93ee519d40e35435b0))
* correctly fill the reorg buffer ([#605](https://github.com/propeller-heads/tycho-indexer/issues/605)) ([2f924f5](https://github.com/propeller-heads/tycho-indexer/commit/2f924f5f38380e5f878d294693af946b3ab02379))
* do not filter out contracts with NULL created_ts ([46fbd82](https://github.com/propeller-heads/tycho-indexer/commit/46fbd822dfb91cd75a9ed83b4ee9ef96875648d4))
* filter for accounts that have associated code ([e9904bb](https://github.com/propeller-heads/tycho-indexer/commit/e9904bb7b1b6d518142e116b22887e4d7a29836d))
* improve handling of missing code errors ([7393c92](https://github.com/propeller-heads/tycho-indexer/commit/7393c92d006e211aca4eed378485334c8a159700))
* remove transaction table query from get_contract ([f60106a](https://github.com/propeller-heads/tycho-indexer/commit/f60106afa7932a063d9be0891bb53cc369f88cdf))
* **storage:** fix bug that filters out contracts missing a creation_tx ([#602](https://github.com/propeller-heads/tycho-indexer/issues/602)) ([75428da](https://github.com/propeller-heads/tycho-indexer/commit/75428da852b4b0aca169504f10cc9a59cec068a5))

## [0.72.1](https://github.com/propeller-heads/tycho-indexer/compare/0.72.0...0.72.1) (2025-06-25)


### Bug Fixes

* **client:** link dci detected contracts to components within the tracker ([#601](https://github.com/propeller-heads/tycho-indexer/issues/601)) ([98855bf](https://github.com/propeller-heads/tycho-indexer/commit/98855bf2ad1a0930a5f2fba08056c3b18231beb0))
* link dci detected contracts to components within the tracker ([3137ccf](https://github.com/propeller-heads/tycho-indexer/commit/3137ccf29beb50783c26baf8b9c2191c2fde823d))

## [0.72.0](https://github.com/propeller-heads/tycho-indexer/compare/0.71.4...0.72.0) (2025-06-24)


### Features

* **dci:** add revert logic ([#590](https://github.com/propeller-heads/tycho-indexer/issues/590)) ([2d12b87](https://github.com/propeller-heads/tycho-indexer/commit/2d12b87417ba3d59d72d7f257e8933031ad529c8))
* **dci:** add support for reorgs. ([f6c23bf](https://github.com/propeller-heads/tycho-indexer/commit/f6c23bf842065ec271dffe781e058403a41ebe13))

## [0.71.4](https://github.com/propeller-heads/tycho-indexer/compare/0.71.3...0.71.4) (2025-06-23)


### Bug Fixes

* correctly handle traced entrypoint upserts ([0e69e84](https://github.com/propeller-heads/tycho-indexer/commit/0e69e84cd004faef89c1a9f177440c55d7e8ef86))
* correctly handle traced entrypoint upserts ([#599](https://github.com/propeller-heads/tycho-indexer/issues/599)) ([e58d546](https://github.com/propeller-heads/tycho-indexer/commit/e58d5469f91b04c1776006a7dc12def6b7a62763))

## [0.71.3](https://github.com/propeller-heads/tycho-indexer/compare/0.71.2...0.71.3) (2025-06-19)

## [0.71.2](https://github.com/propeller-heads/tycho-indexer/compare/0.71.1...0.71.2) (2025-06-19)


### Bug Fixes

* fix build for python wheel and crates.io ([#597](https://github.com/propeller-heads/tycho-indexer/issues/597)) ([b0e99ae](https://github.com/propeller-heads/tycho-indexer/commit/b0e99aee906cd4d70912f42d72e79747ef7225bc))
* fix python wheel build ([20ae4bf](https://github.com/propeller-heads/tycho-indexer/commit/20ae4bfe1652359f17c2fa9a87dd9f4903080f2b))

## [0.71.1](https://github.com/propeller-heads/tycho-indexer/compare/0.71.0...0.71.1) (2025-06-19)


### Bug Fixes

* get DCI plugin RPC url from global config ([88f2b91](https://github.com/propeller-heads/tycho-indexer/commit/88f2b91ab1e85fddd4883324b707e7c610d29b34))
* get DCI plugin RPC url from global config ([#596](https://github.com/propeller-heads/tycho-indexer/issues/596)) ([cc922c4](https://github.com/propeller-heads/tycho-indexer/commit/cc922c4d0eef3cfbdab365f6efad8982b2be0bbc))

## [0.71.0](https://github.com/propeller-heads/tycho-indexer/compare/0.70.9...0.71.0) (2025-06-19)


### Features

* (WIP) Add get_traced_entrypoints endpoint. ([0528c3c](https://github.com/propeller-heads/tycho-indexer/commit/0528c3cbcff441e2d03542c3a513556ec49c9539))
* (WIP) get_traced_entrypoints pagination + caching ([2af5359](https://github.com/propeller-heads/tycho-indexer/commit/2af5359f92a9666659a4791089a8cb0bcbcf1f88))
* access control on add_entry_points ([9e9a7a4](https://github.com/propeller-heads/tycho-indexer/commit/9e9a7a4da46cc29c13e27c32baf1c6a0e76e9e6b))
* Add BatchAccountExtractor with balance and code get logic ([e3b6566](https://github.com/propeller-heads/tycho-indexer/commit/e3b6566fc6b46a9f2929816fe99c969578bee450))
* add block storage changes to BlockChanges extractor model ([fc96b8e](https://github.com/propeller-heads/tycho-indexer/commit/fc96b8eea49c667b5ffbfc880c5161fd11626671))
* add dci plugin to ProtocolExtractor ([5edf7e8](https://github.com/propeller-heads/tycho-indexer/commit/5edf7e8c6b9bf6538a8f205db24ad5d096861862))
* add entry point tables and types to PostgreSQL schema ([3e53c46](https://github.com/propeller-heads/tycho-indexer/commit/3e53c4682ae32f7a7cb8570b4c8c5aa86bc3c3f8))
* add entry point tracing functionality ([1d64d12](https://github.com/propeller-heads/tycho-indexer/commit/1d64d12d1a5c33c6dd5861e41f4b93ca76de5dca))
* add entrypoint and entrypoint params to TxWithChanges ([3237e0f](https://github.com/propeller-heads/tycho-indexer/commit/3237e0f06bf049156302b8f5b6908770a67679f0))
* add entrypoint tracking to component tracker ([87839a0](https://github.com/propeller-heads/tycho-indexer/commit/87839a081fe38c176ccbe7d15ce301b3095d1271))
* add get_traced_entry_points endpoint to RPCClient ([d4a3238](https://github.com/propeller-heads/tycho-indexer/commit/d4a3238765fbecba10c11ea16253293ce7de65d3))
* add get_valid_from to row versioning control ([003df54](https://github.com/propeller-heads/tycho-indexer/commit/003df54f6e1b093966f76bc67bc14857d40af363))
* add method to fetch account storage ([d1f2b4f](https://github.com/propeller-heads/tycho-indexer/commit/d1f2b4f6e887329150e3c69a97650e3a5c6507c8))
* add pagination to EntryPointGateway ([9df2e5a](https://github.com/propeller-heads/tycho-indexer/commit/9df2e5a5f7a99ee782bf9fa848e494e32785ffb2))
* add slots tracing to the RPC tracer ([21b98e5](https://github.com/propeller-heads/tycho-indexer/commit/21b98e5901085d1520477f29bad0e74cf5d08a1e))
* add trace results to BlockChanges and BlockAggregatedChanges ([5b4e56e](https://github.com/propeller-heads/tycho-indexer/commit/5b4e56e3c77a6af20d244f8f4b7db369948abf7d))
* add triggers for updating modification timestamps on entry point tables ([5d0d72b](https://github.com/propeller-heads/tycho-indexer/commit/5d0d72b0407eed06c1edf60607ce871a7eda73cb))
* Add Unknown RpcError type ([f0caeee](https://github.com/propeller-heads/tycho-indexer/commit/f0caeee31d53be0668834046e4efac49196306cb))
* add UpsertEntryPoints and UpsertTracedEntryPoints operations to WriteOp ([a71add2](https://github.com/propeller-heads/tycho-indexer/commit/a71add211a04c19c75dc4c73d9b79133cae8ede7))
* AddEntrypointRequestBody improvements ([405499c](https://github.com/propeller-heads/tycho-indexer/commit/405499c10c3b6f065b8f071979d1520f32e5d4db))
* address rebase changes ([3fd0d50](https://github.com/propeller-heads/tycho-indexer/commit/3fd0d503df43a1994c40a3d55b1b4507ca2f9bfb))
* Create DirectGateway and use in RPCHandler ([fc349ce](https://github.com/propeller-heads/tycho-indexer/commit/fc349ce4ef8ca61f7cc1e183660bd88f3ae865a8))
* create DynamicContractIndexerTrait for simpler generic handling ([df48540](https://github.com/propeller-heads/tycho-indexer/commit/df485403b7e374fc05360539642b45596530c8f3))
* DCI Release ([#595](https://github.com/propeller-heads/tycho-indexer/issues/595)) ([d33afb3](https://github.com/propeller-heads/tycho-indexer/commit/d33afb3eb28b20f0c7542883a7a6823da6913bc2))
* Do not error if tracing result not found + small fixes ([79d16e0](https://github.com/propeller-heads/tycho-indexer/commit/79d16e0b5d1def028f59e8ee465af520dbcd47f3))
* enhance EntryPointFilter to support component id filtering ([4694a18](https://github.com/propeller-heads/tycho-indexer/commit/4694a18fb7cdc9a60f1ff49c4fb455780a96a742))
* extend BlockAggregatedChanges and BlockChanhes dto to include DCI data ([cc6e8ed](https://github.com/propeller-heads/tycho-indexer/commit/cc6e8ed9f2140e4e2e724726481dc54fc3f717d2))
* extend TxWithChanges to include EntryPoints ([475e253](https://github.com/propeller-heads/tycho-indexer/commit/475e2538c3a2c0fa86761b05b2cdc45ed206a292))
* get rpc url from global args, get api key from env ([06debe1](https://github.com/propeller-heads/tycho-indexer/commit/06debe113e88a150bf46bfadbcfe377d852e9fdc))
* handle dci data on tycho-client synchronizer ([5196938](https://github.com/propeller-heads/tycho-indexer/commit/5196938f72d6ea1763d2d155894ea356d9804af2))
* implement entry-points functions on PostgresGateway ([1a374db](https://github.com/propeller-heads/tycho-indexer/commit/1a374db56bb42e5a786e9c5f61da743abf202d82))
* Manually paginate when getting traced entrypoint ([8dafb0d](https://github.com/propeller-heads/tycho-indexer/commit/8dafb0d0c4b038f396f7f99ab008fdbb803fff5b))
* merge traits and finish EVMBatchAccountExtractor ([69ef0f8](https://github.com/propeller-heads/tycho-indexer/commit/69ef0f84cca8b4f66c2ae2a6909e3e446a3acab5))
* move setting dci plugin to extractor config ([be06549](https://github.com/propeller-heads/tycho-indexer/commit/be06549d6fc106bc4a131656429b788fdba824e6))
* register add_entry_points endpoint ([6fdf540](https://github.com/propeller-heads/tycho-indexer/commit/6fdf540242f18a5c2dbcfe6a15859aabcac59e81))
* Return tracing results to user after adding entry points ([6a3153a](https://github.com/propeller-heads/tycho-indexer/commit/6a3153a8fae263b1e98c9113907f37e0023f9c9c))
* sketch DCI behavior ([efa87c7](https://github.com/propeller-heads/tycho-indexer/commit/efa87c74534d3a89f2e7c617a278ce88f0af7987))
* split inserting static and dynamic parts of a new contract ([76d8f71](https://github.com/propeller-heads/tycho-indexer/commit/76d8f71f29ee6af021295260283fe3bc6f6f901a))
* Take RPC_URL as input to ServicesBuilder ([b775185](https://github.com/propeller-heads/tycho-indexer/commit/b775185d88e6410d9ccca409d36a269103cad9df))
* Take tracer as input to RpcHandler ([28ed1b4](https://github.com/propeller-heads/tycho-indexer/commit/28ed1b45c279f735acd88efa8da7492b2d03be09))
* **tycho-common:** create a dedicated MergeError for merge issues ([169fe65](https://github.com/propeller-heads/tycho-indexer/commit/169fe65b7ece09fe1ab60cff7febe331afc4ef5b))
* Update DCI to use new parameters ([2ecd88c](https://github.com/propeller-heads/tycho-indexer/commit/2ecd88cb2c08d7b90bad73d808f6d36871673841))
* update tycho-substreams to 0.3.1 ([5336a53](https://github.com/propeller-heads/tycho-indexer/commit/5336a537a4b1e08417ce8514ed4d4fc1f85493ab))
* Use generic over EntryPointTracer for tracer ([3c62f1b](https://github.com/propeller-heads/tycho-indexer/commit/3c62f1b4e19c3eed1e08e5a61884aeee537dc7d6))
* **versioning:** drop versions older than what already exists in the db ([1c0c764](https://github.com/propeller-heads/tycho-indexer/commit/1c0c764a56149cfc656d314c0d2d3ba1a82be64a))


### Bug Fixes

* address a bug with `.returning` not returning on conflicts. ([db11bd2](https://github.com/propeller-heads/tycho-indexer/commit/db11bd24f2eb4add248260c9f136cf9fac0df5f2))
* correctly add default balances and code on account creations ([6e64424](https://github.com/propeller-heads/tycho-indexer/commit/6e6442462fb4e57528362d5595469025da28071b))
* correctly handle contract inserts ([9f35ee8](https://github.com/propeller-heads/tycho-indexer/commit/9f35ee8d94622f7b7455e8c5a376f9415e3c7f42))
* correctly handle many new entrypoints for a single transaction ([7ffdf5a](https://github.com/propeller-heads/tycho-indexer/commit/7ffdf5a8f10729f96e311eca669ec4f94fa5ef25))
* correctly pass entrypoint data when parsing Substreams message ([f36cfdf](https://github.com/propeller-heads/tycho-indexer/commit/f36cfdf7f337b1eb8bfc2f579cfe39ab16f66b03))
* correctly retrieve db ids when inserting tracing results. ([ae22943](https://github.com/propeller-heads/tycho-indexer/commit/ae2294312b0a7bbd824a1da1c4a8e2c82eabfecf))
* correctly retrives the rpc url from CLI args ([6731078](https://github.com/propeller-heads/tycho-indexer/commit/673107819203bf383f4eb2c89e0470ed216610b7))
* correctly serve entrypoint and tracing params in the PostgresGateway ([fbdab08](https://github.com/propeller-heads/tycho-indexer/commit/fbdab085de5868cfaf6376cf2d23175e4bea9f5b))
* correctly setup "rpc" DCI on startup ([7b31a19](https://github.com/propeller-heads/tycho-indexer/commit/7b31a19fe93c9ad997284b10cc846a1f6c2d2ada))
* correctly sort `txs_with_update` after DCI inserts ([98f6ace](https://github.com/propeller-heads/tycho-indexer/commit/98f6ace6abaade2585780f0bb98b6ad65596065e))
* do not update in insert contract gateway fn ([6cdcf8a](https://github.com/propeller-heads/tycho-indexer/commit/6cdcf8aca3dd2725ac8e1f8538cc209789251cb5))
* fix broken sql down migrations ([e5d41ab](https://github.com/propeller-heads/tycho-indexer/commit/e5d41ab1248b72ba6fc60159f1c0307971fcdb45))
* get_traced_entrypoints pagination + caching ([f6e1742](https://github.com/propeller-heads/tycho-indexer/commit/f6e17429620c30b65654d79313e7f54452ef636e))
* handle outdated deletes ([730e55b](https://github.com/propeller-heads/tycho-indexer/commit/730e55ba935a4cdf5b489d28686bf5cb7b142152))
* handle skipping multiple updates for same entity within a block ([e9ef60b](https://github.com/propeller-heads/tycho-indexer/commit/e9ef60b3ed22cf4d8417acb74958a7c7dea12760))
* Keep OpenAPI clean with proper dto macros ([a69ddcc](https://github.com/propeller-heads/tycho-indexer/commit/a69ddccb7c59c0f2c0d779d728ddcb40a77770ee))
* Make clippy happy ([67d725c](https://github.com/propeller-heads/tycho-indexer/commit/67d725c5ee3b99b4bbb695cce7efe3f1412e3536))
* Properly match params to result in get_traced_entry_points_inner ([ddd6cf5](https://github.com/propeller-heads/tycho-indexer/commit/ddd6cf51f537de567275984ad4abe6c50f741453))
* Remove add_entry_points from swagger UI ([29b9be1](https://github.com/propeller-heads/tycho-indexer/commit/29b9be121ded8b16f04f6cf4fd4c2753b0d0f9e9))
* Remove unnecessary clones ([8ad2c6c](https://github.com/propeller-heads/tycho-indexer/commit/8ad2c6cb7ae160c25fa7bebeb24c08401fa9624a))
* Remove unnecessary to_string (equivalent of cloning) ([20ac1f4](https://github.com/propeller-heads/tycho-indexer/commit/20ac1f4196d0c1a35c72984a606a523793e60693))
* Uncomment test ignore ([8cd9214](https://github.com/propeller-heads/tycho-indexer/commit/8cd92146a5781006354ae9cdb25a02dd3541604e))
* use latest db versions for outdated data check ([feecf18](https://github.com/propeller-heads/tycho-indexer/commit/feecf1809e10378d7f5b4aab758737316a64be6d))
* wrap the DCI plugin in an Arc Mutex ([09f0d51](https://github.com/propeller-heads/tycho-indexer/commit/09f0d51b34fb31770021f8c15cd384c8ca105018))

## [0.70.9](https://github.com/propeller-heads/tycho-indexer/compare/0.70.8...0.70.9) (2025-06-05)


### Bug Fixes

* convert ids to lowercase for the id base ComponentFilter ([0103c60](https://github.com/propeller-heads/tycho-indexer/commit/0103c60c95c8f61375d0d61c6f094e4a3058bf83))
* **tycho-client:** update cli docs, validation and address handling ([45870b1](https://github.com/propeller-heads/tycho-indexer/commit/45870b1bb7d4c9ec51336fe53571f9bd11d6bb3c))
* **tycho-client:** update cli docs, validation and address handling ([#591](https://github.com/propeller-heads/tycho-indexer/issues/591)) ([3272d34](https://github.com/propeller-heads/tycho-indexer/commit/3272d34cf420c2e0ce6d183ff0f1f07035b1eeea))

## [0.70.8](https://github.com/propeller-heads/tycho-indexer/compare/0.70.7...0.70.8) (2025-06-03)

## [0.70.7](https://github.com/propeller-heads/tycho-indexer/compare/0.70.6...0.70.7) (2025-05-21)


### Bug Fixes

* order protocol states query by protocol component ([a45d4a0](https://github.com/propeller-heads/tycho-indexer/commit/a45d4a01828c0346cf99ed1de136f4ec846064fd))
* order protocol states query by protocol component ([#582](https://github.com/propeller-heads/tycho-indexer/issues/582)) ([4597925](https://github.com/propeller-heads/tycho-indexer/commit/4597925eda1292bf1f05f06ad51c64a409e1c7d4))

## [0.70.6](https://github.com/propeller-heads/tycho-indexer/compare/0.70.5...0.70.6) (2025-05-19)

## [0.70.5](https://github.com/propeller-heads/tycho-indexer/compare/0.70.4...0.70.5) (2025-05-16)


### Bug Fixes

* **tycho-client:** fix block position bug for delayed extractors on start up ([#570](https://github.com/propeller-heads/tycho-indexer/issues/570)) ([46aa1af](https://github.com/propeller-heads/tycho-indexer/commit/46aa1afe47ff5cfffc984215a6ecc4f78ee92e7b))
* **tycho-client:** mark blocks older than the oldest in BlockHistory as delayed ([ebb55a9](https://github.com/propeller-heads/tycho-indexer/commit/ebb55a9844f8d7750f060f160bfd617224f33dc4))
* **tycho-client:** skip detached blocks on BlockHistory creation ([116e75a](https://github.com/propeller-heads/tycho-indexer/commit/116e75a77f461e72194f5a513f39f474898ea5c1))

## [0.70.4](https://github.com/propeller-heads/tycho-indexer/compare/0.70.3...0.70.4) (2025-05-15)


### Bug Fixes

* **postgres:** add on_conflict_do_nothing to contract balance and code upserts ([2ef58a8](https://github.com/propeller-heads/tycho-indexer/commit/2ef58a8c866a2ff41bbabf0538cb50ed18e71680))
* **postgres:** add on_conflict_do_nothing to contract balance and code upserts ([#574](https://github.com/propeller-heads/tycho-indexer/issues/574)) ([eb8956b](https://github.com/propeller-heads/tycho-indexer/commit/eb8956bcbbf3b9002fa86e3d74fe3bfae3e34718))

## [0.70.3](https://github.com/propeller-heads/tycho-indexer/compare/0.70.2...0.70.3) (2025-05-14)


### Bug Fixes

* update dockerfile rust version to 1.82 ([#573](https://github.com/propeller-heads/tycho-indexer/issues/573)) ([5b4a015](https://github.com/propeller-heads/tycho-indexer/commit/5b4a01506835545ab082a3694b53a404f16be6b8))

## [0.70.2](https://github.com/propeller-heads/tycho-indexer/compare/0.70.1...0.70.2) (2025-05-14)


### Bug Fixes

* log error code for substream error metrics ([b7d8f78](https://github.com/propeller-heads/tycho-indexer/commit/b7d8f78aaac3f597a4599fa845f31fa6bd07ef61))

## [0.70.1](https://github.com/propeller-heads/tycho-indexer/compare/0.70.0...0.70.1) (2025-05-14)


### Bug Fixes

* include message text in websocket error logging ([b2c78a9](https://github.com/propeller-heads/tycho-indexer/commit/b2c78a993390fa5bc5f2d6c511d48885e5f5e22b))
* include message text in websocket error logging ([0d2e6e7](https://github.com/propeller-heads/tycho-indexer/commit/0d2e6e730634e90016716f6cfceee013093c6683))
* **tycho-client:** include message text in websocket error logging ([#571](https://github.com/propeller-heads/tycho-indexer/issues/571)) ([cce6373](https://github.com/propeller-heads/tycho-indexer/commit/cce6373a3fe39376df0cfa4f166adc7d7f1c6bb7))

## [0.70.0](https://github.com/propeller-heads/tycho-indexer/compare/0.69.0...0.70.0) (2025-05-08)


### Features

* **tycho-client:** propogate block history errors on synchronizer ([de9c792](https://github.com/propeller-heads/tycho-indexer/commit/de9c79205b35312faf736165c9b8e4d56b3b0cd6))
* **tycho-client:** remove panics from block history handler ([81bf5af](https://github.com/propeller-heads/tycho-indexer/commit/81bf5af524eddbe16503b29026ee41aa55d42b4e))
* **tycho-client:** remove panics from BlockSynchronizer ([d6b4ae5](https://github.com/propeller-heads/tycho-indexer/commit/d6b4ae559d72cf56e3d3f9cb7034e144fdebbad9))
* **tycho-client:** remove panics from CLI ([d7a2aae](https://github.com/propeller-heads/tycho-indexer/commit/d7a2aae23f2b2943bb5d83e6d1f05d975c206a64))
* **tycho-client:** Remove panics from client ([#561](https://github.com/propeller-heads/tycho-indexer/issues/561)) ([d03c4a6](https://github.com/propeller-heads/tycho-indexer/commit/d03c4a6e2f2e9dbc8b6b69094e20e5c3ebe6888f))
* **tycho-client:** remove panics from detlas websocket client ([cc38d6c](https://github.com/propeller-heads/tycho-indexer/commit/cc38d6c6008f86edbf8938d920c9042a958b6d12))
* **tycho-client:** remove panics from detlas websocket client ([727429e](https://github.com/propeller-heads/tycho-indexer/commit/727429e36ce5fb373206c96b89a9b5b3f4cedef9))
* **tycho-client:** remove panics from RPCClient ([3df8ce7](https://github.com/propeller-heads/tycho-indexer/commit/3df8ce72723782a288efc841221021ff6981e00c))
* **tycho-client:** remove panics from state synchroniser ([5ae277b](https://github.com/propeller-heads/tycho-indexer/commit/5ae277b110ff795471dd49f49490b3eee71dd8e2))
* **tycho-client:** remove panics from TychoStreamBuilder ([ca5c82b](https://github.com/propeller-heads/tycho-indexer/commit/ca5c82bdced7bb87c293fa9d8e282f0f29d8b672))


### Bug Fixes

* box large errors ([771a36a](https://github.com/propeller-heads/tycho-indexer/commit/771a36a2111542dea7dbb72ee88dfde6ff5c3f2e))
* break synchronisation retry loop if ws client disconnected ([22b77b3](https://github.com/propeller-heads/tycho-indexer/commit/22b77b3eec28de7403fe98919addf3204c181d6c))
* combine SetUpError and InitializationError on the stream builder ([4edf71b](https://github.com/propeller-heads/tycho-indexer/commit/4edf71b66a6786ac1926dd93ba04a00fe9fc46c8))
* **tycho-client:** convert large SendError to an error string ([4c1f3e7](https://github.com/propeller-heads/tycho-indexer/commit/4c1f3e7125f953f4730c658c1434910b31a04065))

## [0.69.0](https://github.com/propeller-heads/tycho-indexer/compare/0.68.2...0.69.0) (2025-05-08)


### Features

* add component tvl ([#547](https://github.com/propeller-heads/tycho-indexer/issues/547)) ([86663e6](https://github.com/propeller-heads/tycho-indexer/commit/86663e6a9386cb38ecb56c419bde29171b9cb3f3))
* add system and pagination params ([356ab1a](https://github.com/propeller-heads/tycho-indexer/commit/356ab1ad9eac285a08d5652e5ecd8b9cbf05824d))
* component tvl rpc ([49b146c](https://github.com/propeller-heads/tycho-indexer/commit/49b146c492f9ea72243a33da62f9fa79c52adcf0))


### Bug Fixes

* lint ([2c74dfb](https://github.com/propeller-heads/tycho-indexer/commit/2c74dfb24d2bbeae4799e43bcd71219bcc7ee895))

## [0.68.2](https://github.com/propeller-heads/tycho-indexer/compare/0.68.1...0.68.2) (2025-05-02)


### Bug Fixes

* correctly exit extraction loop on stop signal ([1ad56d8](https://github.com/propeller-heads/tycho-indexer/commit/1ad56d8705a7da53e77c9b7678b1d617c0ea8df1))
* correctly exit extraction loop on stop signal ([#569](https://github.com/propeller-heads/tycho-indexer/issues/569)) ([d0fd91c](https://github.com/propeller-heads/tycho-indexer/commit/d0fd91c151df45cb395492a61034fa57b928fcc8))

## [0.68.1](https://github.com/propeller-heads/tycho-indexer/compare/0.68.0...0.68.1) (2025-05-02)


### Bug Fixes

* add backoff for retries on connections error with Substreams ([e9379b2](https://github.com/propeller-heads/tycho-indexer/commit/e9379b229c6866895709bd1b6ee32a50624e5f03))
* add backoff for retries on connections error with Substreams ([#562](https://github.com/propeller-heads/tycho-indexer/issues/562)) ([177857c](https://github.com/propeller-heads/tycho-indexer/commit/177857c017512356e032eb20b9fe28752cb5aaba))

## [0.68.0](https://github.com/propeller-heads/tycho-indexer/compare/0.67.3...0.68.0) (2025-05-02)


### Features

* enhance shutdown handling with SIGTERM support ([c7c5b0d](https://github.com/propeller-heads/tycho-indexer/commit/c7c5b0dd7e57bd6c12269cf24a7b8f2760ca54ff))
* enhance shutdown handling with SIGTERM support ([#568](https://github.com/propeller-heads/tycho-indexer/issues/568)) ([05cbea3](https://github.com/propeller-heads/tycho-indexer/commit/05cbea3fb0cbae507d0a36926794cd8cb7bcb4ab))

## [0.67.3](https://github.com/propeller-heads/tycho-indexer/compare/0.67.2...0.67.3) (2025-05-01)

## [0.67.2](https://github.com/propeller-heads/tycho-indexer/compare/0.67.1...0.67.2) (2025-05-01)

## [0.67.1](https://github.com/propeller-heads/tycho-indexer/compare/0.67.0...0.67.1) (2025-05-01)

## [0.67.0](https://github.com/propeller-heads/tycho-indexer/compare/0.66.5...0.67.0) (2025-04-30)


### Features

* update CORS to allow all subdomains of `propellerheads.xyz` ([d709443](https://github.com/propeller-heads/tycho-indexer/commit/d70944343d9b9ca3a6f7f9491493bf953807262e))
* update CORS to allow all subdomains of `propellerheads.xyz` ([#563](https://github.com/propeller-heads/tycho-indexer/issues/563)) ([a451301](https://github.com/propeller-heads/tycho-indexer/commit/a4513010bf3c95ef4f346f84805473f95cdc8bf0))

## [0.66.5](https://github.com/propeller-heads/tycho-indexer/compare/0.66.4...0.66.5) (2025-04-29)


### Bug Fixes

* remove error logging on RPCClient ([9934abd](https://github.com/propeller-heads/tycho-indexer/commit/9934abd76eecc93f0bdb84509121a5722e4c7ef5))
* remove error logging on RPCClient ([#560](https://github.com/propeller-heads/tycho-indexer/issues/560)) ([41f0704](https://github.com/propeller-heads/tycho-indexer/commit/41f0704af3002ed7df5ccee2fa818bf83ec1b7fc))

## [0.66.4](https://github.com/propeller-heads/tycho-indexer/compare/0.66.3...0.66.4) (2025-04-18)


### Bug Fixes

* **tycho-client:** relax default timeout values ([eba302f](https://github.com/propeller-heads/tycho-indexer/commit/eba302f11606798acc589f0ae3a57407a7b16da3))
* **tycho-client:** relax default timeout values ([#555](https://github.com/propeller-heads/tycho-indexer/issues/555)) ([3852e6a](https://github.com/propeller-heads/tycho-indexer/commit/3852e6a5db5932083feb37d9a0817e54ad8d3c49))

## [0.66.3](https://github.com/propeller-heads/tycho-indexer/compare/0.66.2...0.66.3) (2025-04-18)


### Bug Fixes

* cleanly close ws connections on timeouts ([319d988](https://github.com/propeller-heads/tycho-indexer/commit/319d988d3d095326f191907ab5e2a28e7fe7ca77))
* cleanly close ws connections on timeouts ([#556](https://github.com/propeller-heads/tycho-indexer/issues/556)) ([262d070](https://github.com/propeller-heads/tycho-indexer/commit/262d070b827730a594fef833484667d8825bdd78))

## [0.66.2](https://github.com/propeller-heads/tycho-indexer/compare/0.66.1...0.66.2) (2025-04-11)


### Bug Fixes

* Make Snapshot attributes pub ([#552](https://github.com/propeller-heads/tycho-indexer/issues/552)) ([55c668e](https://github.com/propeller-heads/tycho-indexer/commit/55c668e29a453809d820e362435093485d77d019))

## [0.66.1](https://github.com/propeller-heads/tycho-indexer/compare/0.66.0...0.66.1) (2025-04-10)

## [0.66.0](https://github.com/propeller-heads/tycho-indexer/compare/0.65.0...0.66.0) (2025-04-10)


### Features

* Implement retry logic for deadlock detection in database transactions ([5d63e49](https://github.com/propeller-heads/tycho-indexer/commit/5d63e497a0b8279a57f3809c84a1df91dd9cc261))
* Implement retry logic for deadlock detection in database transactions ([#549](https://github.com/propeller-heads/tycho-indexer/issues/549)) ([5a4b28e](https://github.com/propeller-heads/tycho-indexer/commit/5a4b28e49533ad3a99b2e7a08e905c18f9990a75))

## [0.65.0](https://github.com/propeller-heads/tycho-indexer/compare/0.64.2...0.65.0) (2025-04-02)


### Features

* Add link to Tycho.build telegram ([2ef1fe6](https://github.com/propeller-heads/tycho-indexer/commit/2ef1fe6687eb005b7f0b305abcb886136242a415))

## [0.64.2](https://github.com/propeller-heads/tycho-indexer/compare/0.64.1...0.64.2) (2025-04-01)


### Bug Fixes

* Set default value for account_balances in BlockChanges model ([4f69191](https://github.com/propeller-heads/tycho-indexer/commit/4f69191e2507a1e397ba930ce9719fec8bcd9366))
* Set default value for account_balances in BlockChanges model ([#544](https://github.com/propeller-heads/tycho-indexer/issues/544)) ([8ced9f9](https://github.com/propeller-heads/tycho-indexer/commit/8ced9f92a4c2d36feab51677f4fffea448dd5b60))

## [0.64.1](https://github.com/propeller-heads/tycho-indexer/compare/0.64.0...0.64.1) (2025-03-31)


### Bug Fixes

* include workspace cargo toml in release config ([2cecbd8](https://github.com/propeller-heads/tycho-indexer/commit/2cecbd8f050544b35e8bb82171dde7fa75781dd0))
* include workspace cargo toml in release config ([#543](https://github.com/propeller-heads/tycho-indexer/issues/543)) ([071af8a](https://github.com/propeller-heads/tycho-indexer/commit/071af8a0e40e51aa089d9b9d73da08d9ea736e6d))

## [0.64.0](https://github.com/propeller-heads/tycho-indexer/compare/0.63.1...0.64.0) (2025-03-31)


### Features

* add max_missed_blocks as staleness threshold ([d9c6846](https://github.com/propeller-heads/tycho-indexer/commit/d9c6846cb407a4a10928b3df6a1a7779530272ac))
* handle delayed exchanges on tycho-client ([d106b82](https://github.com/propeller-heads/tycho-indexer/commit/d106b82056a1db551031c3bf1911069fa19dd371))
* Handle delayed exchanges on tycho-client ([#539](https://github.com/propeller-heads/tycho-indexer/issues/539)) ([a5da6fd](https://github.com/propeller-heads/tycho-indexer/commit/a5da6fd1ebf7212430cff2b3d28d1efe48f211f6))
* only wait for one deltas message on client start up ([4fba42f](https://github.com/propeller-heads/tycho-indexer/commit/4fba42f168fac3a6d6c94a235a6aac9e39ffbdd5))
* use block_time + timeout as wait time for first deltas ([51e2313](https://github.com/propeller-heads/tycho-indexer/commit/51e23130d96f7b69fbe02eb439482fd7042bc693))


### Bug Fixes

* **tycho-client:** allow setting TYCHO_URL as an env var ([54fa5c4](https://github.com/propeller-heads/tycho-indexer/commit/54fa5c4a53f58e3562d1225f523c46831e8ae4ce))
* wait block time + timeout for first message ([751aae7](https://github.com/propeller-heads/tycho-indexer/commit/751aae7e62881412ca58840dd97085147b5a47df))

## [0.63.1](https://github.com/propeller-heads/tycho-indexer/compare/0.63.0...0.63.1) (2025-03-26)


### Bug Fixes

* add missing version to checkout action ([c025616](https://github.com/propeller-heads/tycho-indexer/commit/c025616b3c19106113c195bc95cb04c05848153f))
* add missing version to checkout action ([#537](https://github.com/propeller-heads/tycho-indexer/issues/537)) ([a803970](https://github.com/propeller-heads/tycho-indexer/commit/a803970ee129ef8d9ed8317ca0d2ecb5fe8c52c6))

## [0.63.0](https://github.com/propeller-heads/tycho-indexer/compare/0.62.0...0.63.0) (2025-03-26)


### Features

* Rename tycho-core to tycho-common. ([f5ed755](https://github.com/propeller-heads/tycho-indexer/commit/f5ed7559adf4fc64b4ac39ea0afdb9845e23bb1e))
* Rename tycho-core to tycho-common. ([#536](https://github.com/propeller-heads/tycho-indexer/issues/536)) ([476f3f3](https://github.com/propeller-heads/tycho-indexer/commit/476f3f34f58039877fa47863fd37e541ae4bb25f))

## [0.62.0](https://github.com/propeller-heads/tycho-indexer/compare/0.61.1...0.62.0) (2025-03-25)


### Features

* add Unichain support ([2422692](https://github.com/propeller-heads/tycho-indexer/commit/242269286d6020c1bd1489606783f2bf2e9ed6fd))
* add Unichain support ([#534](https://github.com/propeller-heads/tycho-indexer/issues/534)) ([e0c1b41](https://github.com/propeller-heads/tycho-indexer/commit/e0c1b41209b95b7408ef7a795ab99baea7b922d2))

## [0.61.1](https://github.com/propeller-heads/tycho-indexer/compare/0.61.0...0.61.1) (2025-03-11)


### Bug Fixes

* Add default to change attr to deprecate ([38efe73](https://github.com/propeller-heads/tycho-indexer/commit/38efe738f1620409f521c8ee5757c1b0f0598032))
* Revert skipping serialization on change field ([12e4482](https://github.com/propeller-heads/tycho-indexer/commit/12e4482f0b7e148223dbf550b3dd2223291d51fe))
* Revert skipping serialization on change field ([#529](https://github.com/propeller-heads/tycho-indexer/issues/529)) ([d53c7cf](https://github.com/propeller-heads/tycho-indexer/commit/d53c7cfe2607e30eaba517a49f0f3a444df7445c))

## [0.61.0](https://github.com/propeller-heads/tycho-indexer/compare/0.60.0...0.61.0) (2025-03-11)


### Features

* Add propellerheads docs domain to CORS allowance list ([7d87115](https://github.com/propeller-heads/tycho-indexer/commit/7d8711574bdf15c5e20fe9de1705a3dfd5bb85ab))
* add security requirement to paths ([55eec82](https://github.com/propeller-heads/tycho-indexer/commit/55eec823f39dc01d83407c63fad78a246e9da582))
* Allow CORS from Gitbook UI ([83c68b9](https://github.com/propeller-heads/tycho-indexer/commit/83c68b99a0fcd0ae26db230b2341e59d8fedbb65))
* Allow CORS from Gitbook UI ([#526](https://github.com/propeller-heads/tycho-indexer/issues/526)) ([98f8b41](https://github.com/propeller-heads/tycho-indexer/commit/98f8b4108e137b87bf5c98f00efc39d2c49021a8))
* Improve autogenerated openapi schema  ([#524](https://github.com/propeller-heads/tycho-indexer/issues/524)) ([53a4020](https://github.com/propeller-heads/tycho-indexer/commit/53a40202e9be65545e533a52592b86b57246de57))
* skip serializing change field on PC ([0317c8d](https://github.com/propeller-heads/tycho-indexer/commit/0317c8dc8a268ed78daa9d98fbfc74897d6cc5e9))

## [0.60.0](https://github.com/propeller-heads/tycho-indexer/compare/0.59.5...0.60.0) (2025-03-06)


### Features

* update codebase with latest substream message changes ([c5dca5e](https://github.com/propeller-heads/tycho-indexer/commit/c5dca5e5aac38b65f3fbd7e6472da28450263db9))
* update substreams proto files ([b72cb77](https://github.com/propeller-heads/tycho-indexer/commit/b72cb776e6ec88aaffd954c4374be39ee4c8b99c))
* update substreams proto messages ([#525](https://github.com/propeller-heads/tycho-indexer/issues/525)) ([3175a47](https://github.com/propeller-heads/tycho-indexer/commit/3175a470c552ca19495e1f7a532f07914b0a791b))

## [0.59.5](https://github.com/propeller-heads/tycho-indexer/compare/0.59.4...0.59.5) (2025-03-06)

## [0.59.4](https://github.com/propeller-heads/tycho-indexer/compare/0.59.3...0.59.4) (2025-03-06)

## [0.59.3](https://github.com/propeller-heads/tycho-indexer/compare/0.59.2...0.59.3) (2025-03-04)


### Bug Fixes

* update dockerfile ([114ad12](https://github.com/propeller-heads/tycho-indexer/commit/114ad127b494ec117f47dc30a21df141f9953739))
* update dockerfile ([dd4ffdc](https://github.com/propeller-heads/tycho-indexer/commit/dd4ffdc5e5a9bf177ce3504ab09f2285b9ad750f))
* update dockerfile ([be3c408](https://github.com/propeller-heads/tycho-indexer/commit/be3c40842e83a44e93244fd8c498e0da6e68b0df))
* update dockerfile ([#522](https://github.com/propeller-heads/tycho-indexer/issues/522)) ([488ac2e](https://github.com/propeller-heads/tycho-indexer/commit/488ac2edf3298085577b9b0eb506b4db37d27657))

## [0.59.2](https://github.com/propeller-heads/tycho-indexer/compare/0.59.1...0.59.2) (2025-03-04)

## [0.59.1](https://github.com/propeller-heads/tycho-indexer/compare/0.59.0...0.59.1) (2025-03-04)

## [0.59.0](https://github.com/propeller-heads/tycho-indexer/compare/0.58.3...0.59.0) (2025-02-28)


### Features

* fix conflicting timestamps, update latest block on reverts ([0bbd30e](https://github.com/propeller-heads/tycho-indexer/commit/0bbd30e64c7a5e74e173d5e3195449353c76efbd))
* get block by hash instead of number ([b38d97d](https://github.com/propeller-heads/tycho-indexer/commit/b38d97d1288bf528df6a9f21f68b8390479612e6))
* Handle blocks with same ts ([#494](https://github.com/propeller-heads/tycho-indexer/issues/494)) ([314de48](https://github.com/propeller-heads/tycho-indexer/commit/314de48e270ad8a749a72d3b254fc634a8994cc6))
* remove #[allow(dead_code)] ([f88c606](https://github.com/propeller-heads/tycho-indexer/commit/f88c606fe9040b316164727870645abf415c8632))
* return block_id together with cursor ([160757a](https://github.com/propeller-heads/tycho-indexer/commit/160757a1b662248dc0894074f342cb4e5250383b))
* simplify logic, remove previous block ts overwrite for Arbitrum ([e32cd2b](https://github.com/propeller-heads/tycho-indexer/commit/e32cd2bbb3778e10b5f25f7c8867b944ce63b8be))

## [0.58.3](https://github.com/propeller-heads/tycho-indexer/compare/0.58.2...0.58.3) (2025-02-28)


### Bug Fixes

* Fix token_balances typing on ResponseAccount ([e9d854b](https://github.com/propeller-heads/tycho-indexer/commit/e9d854bd9dedced448ffe6067a55738bf21cc689))
* Fix token_balances typing on ResponseAccount ([#514](https://github.com/propeller-heads/tycho-indexer/issues/514)) ([924e3d0](https://github.com/propeller-heads/tycho-indexer/commit/924e3d037d3def9dfb4804548de49e4095a547d7))

## [0.58.2](https://github.com/propeller-heads/tycho-indexer/compare/0.58.1...0.58.2) (2025-02-27)


### Bug Fixes

* propagate more meaningful RPC errors on the client ([#515](https://github.com/propeller-heads/tycho-indexer/issues/515)) ([6261432](https://github.com/propeller-heads/tycho-indexer/commit/6261432585ae1f2d4f8a99b6cfd629e4a9f1285f))
* propogate more meaningful RPC errors on the client ([bf92d73](https://github.com/propeller-heads/tycho-indexer/commit/bf92d73ae4a0535619b4d8d42f78c6c981403c34))
* return error with body on failed response parsing ([08b74cc](https://github.com/propeller-heads/tycho-indexer/commit/08b74cc0178dc0d69a3e461c271c401e3d23ee1e))

## [0.58.1](https://github.com/propeller-heads/tycho-indexer/compare/0.58.0...0.58.1) (2025-02-26)


### Bug Fixes

* index tokens table on quality for quicker quality based lookups ([7f38460](https://github.com/propeller-heads/tycho-indexer/commit/7f38460f1189deee46c934ec7ad535d93218fdec))

## [0.58.0](https://github.com/propeller-heads/tycho-indexer/compare/0.57.2...0.58.0) (2025-02-26)


### Features

* update PG gateway get_tokens quality filter to be ranged ([9648dd6](https://github.com/propeller-heads/tycho-indexer/commit/9648dd6b4b5d7021d8cc0057b644bbf3e5a656f2))
* update postgres gateway get_tokens quality filter to be ranged ([#516](https://github.com/propeller-heads/tycho-indexer/issues/516)) ([72fac95](https://github.com/propeller-heads/tycho-indexer/commit/72fac95de65110625d5fba154e675240866274f7))

## [0.57.2](https://github.com/propeller-heads/tycho-indexer/compare/0.57.1...0.57.2) (2025-02-14)

## [0.57.1](https://github.com/propeller-heads/tycho-indexer/compare/0.57.0...0.57.1) (2025-02-14)


### Bug Fixes

* add arbitrum to the native token migration ([657aa53](https://github.com/propeller-heads/tycho-indexer/commit/657aa53110416f5c54c8fc0ef6bba3293dcacf12))
* add arbitrum to the native token migration ([#512](https://github.com/propeller-heads/tycho-indexer/issues/512)) ([4a76b38](https://github.com/propeller-heads/tycho-indexer/commit/4a76b3802b1b1c9082f6bfc162d585c8173a2d6e))

## [0.57.0](https://github.com/propeller-heads/tycho-indexer/compare/0.56.5...0.57.0) (2025-02-14)


### Features

* add account balances to client-py ([e37f1c6](https://github.com/propeller-heads/tycho-indexer/commit/e37f1c6d96599aaa3e86e91643f4aebb226db575))
* add account balances to ResponseAccount ([145f270](https://github.com/propeller-heads/tycho-indexer/commit/145f2709aa4f6f27e30cc513553fa80f68c206f4))
* add account_balances to BlockChanges dto struct ([7cc3e3c](https://github.com/propeller-heads/tycho-indexer/commit/7cc3e3cd66d3d5a5a046e84d373a38e653185c26))
* add add_account_balances postgres gateway fn ([a38c0ae](https://github.com/propeller-heads/tycho-indexer/commit/a38c0aec5353992aca3b30c50066c0e9fb8351b9))
* add get_account_balances gateway fn ([83e87ef](https://github.com/propeller-heads/tycho-indexer/commit/83e87eff5bda2a373c64f123a25f5f92d0f917c9))
* add migration for token_id in account_balance table ([d3acab0](https://github.com/propeller-heads/tycho-indexer/commit/d3acab0c53ead8a902e43eb4db5d3055b602046c))
* add migration for token_id in account_balance table ([#495](https://github.com/propeller-heads/tycho-indexer/issues/495)) ([fa7e424](https://github.com/propeller-heads/tycho-indexer/commit/fa7e42442bed5cf7b840736fd042008f065317c1))
* also ensure native token when ensuring chain on start-up ([4cfd6c2](https://github.com/propeller-heads/tycho-indexer/commit/4cfd6c2387ad99ee5b40dd0a630e47b9c62ef72e))
* fetch account balances on get_contracts ([9ded485](https://github.com/propeller-heads/tycho-indexer/commit/9ded485ad33f5336f7f35f5a24a2ceceebc0d04f))
* handle account balance changes on reverts ([4cf80bb](https://github.com/propeller-heads/tycho-indexer/commit/4cf80bb4fe56cb8010935398067d2d3da0fb11f7))
* implement chain -> native token DB id cache ([b35fe77](https://github.com/propeller-heads/tycho-indexer/commit/b35fe77049697215ed3b530474c673aba9f53a52))
* implement hardcoded chain -> native token map ([531348c](https://github.com/propeller-heads/tycho-indexer/commit/531348c524ad5c265a5dd185c3082a027efa75e0))
* update existing postgres gateway methods with AccountBalance ([f67fb68](https://github.com/propeller-heads/tycho-indexer/commit/f67fb68fab649dea0ceb260e85c0b5b2568fb54a))
* Update Tycho Python client Account DTO ([#506](https://github.com/propeller-heads/tycho-indexer/issues/506)) ([a48bb82](https://github.com/propeller-heads/tycho-indexer/commit/a48bb822b5b2a54d71100ab309012a88633097e0))


### Bug Fixes

* do not error if ensure chains finds existing chain ([3874ac0](https://github.com/propeller-heads/tycho-indexer/commit/3874ac0528c65ec1437471bcd0b1785cb0fa97d4))
* filter balances by native token on account balance delta retrieval ([2d4655b](https://github.com/propeller-heads/tycho-indexer/commit/2d4655b3f4ff3b5b6561a639a9768f0cc730a832))
* only insert ETH native token if DB has 1 chain ([8a38bca](https://github.com/propeller-heads/tycho-indexer/commit/8a38bcabc85ed9b777017980b7adc919aa00f9f3))
* re-add mistakenly removed dead code clippy skip ([0c2de2b](https://github.com/propeller-heads/tycho-indexer/commit/0c2de2b92dc2cae3e35ae82641abf39eb54518e8))
* remove balance_float field ([2bb50f4](https://github.com/propeller-heads/tycho-indexer/commit/2bb50f4788b2227cad43c0e0df7f706e81fa40fc))
* set native token gas correctly ([58f1832](https://github.com/propeller-heads/tycho-indexer/commit/58f1832c584042f25944212b07ce8114ca8686e2))
* set native token gas value and improve cache name ([d3c3033](https://github.com/propeller-heads/tycho-indexer/commit/d3c303334f8496ac2239c75b44d2a9311af64302))
* set Starknet native token to ETH ([3f7ec77](https://github.com/propeller-heads/tycho-indexer/commit/3f7ec7733c9604924094d507558513ff5a79dac7))
* update accout balance table constraints ([7278897](https://github.com/propeller-heads/tycho-indexer/commit/727889779358bae778ee9f8863448439642f7c9f))
* update get_contract to filter balances by native token ([03dc2e0](https://github.com/propeller-heads/tycho-indexer/commit/03dc2e0fa616dc512dda14c3bb422a3f8f91d077))

## [0.56.5](https://github.com/propeller-heads/tycho-indexer/compare/0.56.4...0.56.5) (2025-02-12)


### Bug Fixes

* **rpc:** correctly apply TVL filtering on `/protocol_component` requests ([dd4dc20](https://github.com/propeller-heads/tycho-indexer/commit/dd4dc203ededcbe9c6de1eb73b1f99884966f3b2))
* **rpc:** correctly apply TVL filtering on `/protocol_component` requests ([#511](https://github.com/propeller-heads/tycho-indexer/issues/511)) ([588e368](https://github.com/propeller-heads/tycho-indexer/commit/588e36806046b1a779c791d6d9959b4fc76153a2))

## [0.56.4](https://github.com/propeller-heads/tycho-indexer/compare/0.56.3...0.56.4) (2025-02-11)


### Bug Fixes

* default RPC client and server to use HTTP/2 ([#508](https://github.com/propeller-heads/tycho-indexer/issues/508)) ([f611383](https://github.com/propeller-heads/tycho-indexer/commit/f6113838c889e5fd76feeaa85b3e41fc9a5bebe6))
* disable connection pooling on tycho-client rpc ([c6bb715](https://github.com/propeller-heads/tycho-indexer/commit/c6bb715d24f4122801f71d86791483ec41a07d8c))
* finetune connection timeouts on rpc ([b06ec9b](https://github.com/propeller-heads/tycho-indexer/commit/b06ec9b9a107892acb76bbe012abd510b6e495e4))
* set both client and server to use HTTP/2 ([1516abe](https://github.com/propeller-heads/tycho-indexer/commit/1516abea713ab747835941e7a95726e54d38dd29))

## [0.56.3](https://github.com/propeller-heads/tycho-indexer/compare/0.56.2...0.56.3) (2025-02-10)


### Bug Fixes

* increase Base block time default ([9bcb718](https://github.com/propeller-heads/tycho-indexer/commit/9bcb7188f281c39c36b20af860666605d7d3b82f))
* increase Base blocktime default ([#510](https://github.com/propeller-heads/tycho-indexer/issues/510)) ([82702cd](https://github.com/propeller-heads/tycho-indexer/commit/82702cdeee20c53e04e0c9e6e720909a4155cd00))

## [0.56.2](https://github.com/propeller-heads/tycho-indexer/compare/0.56.1...0.56.2) (2025-02-07)

## [0.56.1](https://github.com/propeller-heads/tycho-indexer/compare/0.56.0...0.56.1) (2025-02-06)


### Bug Fixes

* improve efficiency of activity filter on tokens query ([5649e46](https://github.com/propeller-heads/tycho-indexer/commit/5649e4659aed1a9f23e64a8d2812b72b412dc03e))
* improve efficiency of activity filter on tokens query ([#504](https://github.com/propeller-heads/tycho-indexer/issues/504)) ([de741e6](https://github.com/propeller-heads/tycho-indexer/commit/de741e64d9de08d50017872c27975113310dffd8))

## [0.56.0](https://github.com/propeller-heads/tycho-indexer/compare/0.55.2...0.56.0) (2025-02-05)


### Features

* **rpc:** sort tokens by address in `get_protocol_component` ([3014126](https://github.com/propeller-heads/tycho-indexer/commit/3014126a7deecdc56ed5cdf998968193ef9b3443))
* **rpc:** sort tokens by address in `get_protocol_component` ([#503](https://github.com/propeller-heads/tycho-indexer/issues/503)) ([f59b665](https://github.com/propeller-heads/tycho-indexer/commit/f59b6658d5bfe87e61776f199901dabf7b31c1f9))

## [0.55.2](https://github.com/propeller-heads/tycho-indexer/compare/0.55.1...0.55.2) (2025-02-05)


### Bug Fixes

* update ChangeType enum derived traits ([2e9032e](https://github.com/propeller-heads/tycho-indexer/commit/2e9032ea0afe219e897f87d4bcbf10c9360bb011))
* update ChangeType enum derived traits ([#502](https://github.com/propeller-heads/tycho-indexer/issues/502)) ([383e348](https://github.com/propeller-heads/tycho-indexer/commit/383e3482f45bb7960ad6e2593c3d54e964b6357d))

## [0.55.1](https://github.com/propeller-heads/tycho-indexer/compare/0.55.0...0.55.1) (2025-01-30)


### Bug Fixes

* clean up imports ([a32f8f9](https://github.com/propeller-heads/tycho-indexer/commit/a32f8f9e8ce1cc933c39ea1578aea77f4e181673))
* clean up imports ([#499](https://github.com/propeller-heads/tycho-indexer/issues/499)) ([0a8971b](https://github.com/propeller-heads/tycho-indexer/commit/0a8971ba9b7b566247431f8bd0c9c087ac2f4161))
* fix protocol system endpoint conversion to POST ([9f8d11b](https://github.com/propeller-heads/tycho-indexer/commit/9f8d11bdd2aa76ed4a7869d966142a110cb804f3))
* fix protocol system endpoint conversion to POST ([#498](https://github.com/propeller-heads/tycho-indexer/issues/498)) ([e559128](https://github.com/propeller-heads/tycho-indexer/commit/e5591282340b408c1d23a13b4d4cebf5b3b64115))

## [0.55.0](https://github.com/propeller-heads/tycho-indexer/compare/0.54.0...0.55.0) (2025-01-29)


### Features

* make protocol system a POST endpoint ([d85b2a4](https://github.com/propeller-heads/tycho-indexer/commit/d85b2a44166c376808e634c13839d2a95b4705fa))
* make protocol system a POST endpoint ([#496](https://github.com/propeller-heads/tycho-indexer/issues/496)) ([aad6b51](https://github.com/propeller-heads/tycho-indexer/commit/aad6b5153203a502920b05b1fcad67c80e307ae3))

## [0.54.0](https://github.com/propeller-heads/tycho-indexer/compare/0.53.0...0.54.0) (2025-01-27)


### Features

* add AccountBalance model to tx aggregated model ([ee96ba2](https://github.com/propeller-heads/tycho-indexer/commit/ee96ba27233b1e6a114f2e7326c79d7f938fb416))
* Add AccountBalance to core models ([#493](https://github.com/propeller-heads/tycho-indexer/issues/493)) ([cb86d4e](https://github.com/propeller-heads/tycho-indexer/commit/cb86d4e0ffb905c4d51a0a3514296305b8d19d3b))
* update parsing of new protobuf message changes ([1c424c0](https://github.com/propeller-heads/tycho-indexer/commit/1c424c05606bf9f056b7849a64bce559b17d008f))

## [0.53.0](https://github.com/propeller-heads/tycho-indexer/compare/0.52.0...0.53.0) (2025-01-23)


### Features

* add account balances to protobuf messages ([349aa2c](https://github.com/propeller-heads/tycho-indexer/commit/349aa2c61e0766db72b18c4920588c3969c12de8))
* add AccountBalances to the protobuf messages ([#492](https://github.com/propeller-heads/tycho-indexer/issues/492)) ([08cb0c6](https://github.com/propeller-heads/tycho-indexer/commit/08cb0c600447851c91493d1e58e81ce1362d47da))


### Bug Fixes

* remove unnecessary tx field in ProtocolComponent ([43e6573](https://github.com/propeller-heads/tycho-indexer/commit/43e65737f05fb48f06051670882ffc379616b1dd))

## [0.52.0](https://github.com/propeller-heads/tycho-indexer/compare/0.51.0...0.52.0) (2025-01-22)


### Features

* Add Base to supported chains ([c0afd1a](https://github.com/propeller-heads/tycho-indexer/commit/c0afd1ac5df5a3302e4a5153638d1c71889fa2be))
* Add Base to supported chains ([#491](https://github.com/propeller-heads/tycho-indexer/issues/491)) ([31447a8](https://github.com/propeller-heads/tycho-indexer/commit/31447a8ad3de358f330775e634bda326714cf2da))

## [0.51.0](https://github.com/propeller-heads/tycho-indexer/compare/0.50.0...0.51.0) (2025-01-20)


### Features

* update substreams client to accept compressed messages ([3a68674](https://github.com/propeller-heads/tycho-indexer/commit/3a6867418ba78a89568757a249dcd66fe2b758ff))
* update substreams client to accept compressed messages ([#489](https://github.com/propeller-heads/tycho-indexer/issues/489)) ([b441682](https://github.com/propeller-heads/tycho-indexer/commit/b4416824d66799d208812bd698ca12e16bba01cc))

## [0.50.0](https://github.com/propeller-heads/tycho-indexer/compare/0.49.2...0.50.0) (2025-01-17)


### Features

* add protocol system rpc ([#484](https://github.com/propeller-heads/tycho-indexer/issues/484)) ([9987d56](https://github.com/propeller-heads/tycho-indexer/commit/9987d5630c8be58173ca73a85549bf5f7c585644))
* add protocol_systems rpc ([9e44522](https://github.com/propeller-heads/tycho-indexer/commit/9e4452234a51a3ce83e76c82cfad0d430b0775ab))


### Bug Fixes

* add cache exist ([97f75a1](https://github.com/propeller-heads/tycho-indexer/commit/97f75a12cd1ff3b2a6cca8edf9b555d819192ac9))
* add sort before pagination ([c864f8f](https://github.com/propeller-heads/tycho-indexer/commit/c864f8f43669f778d01dca937d0e263918abf9ae))
* get method ([8540c47](https://github.com/propeller-heads/tycho-indexer/commit/8540c474e9b7a716cb4e2d61229d874d19fd51ad))

## [0.49.2](https://github.com/propeller-heads/tycho-indexer/compare/0.49.1...0.49.2) (2025-01-16)


### Bug Fixes

* add more db gateway 'get' tracing spans ([#487](https://github.com/propeller-heads/tycho-indexer/issues/487)) ([b11ee91](https://github.com/propeller-heads/tycho-indexer/commit/b11ee913628b0ae3053081b415e0c67472390df6))
* add more db gateway read spans ([958f357](https://github.com/propeller-heads/tycho-indexer/commit/958f357b7fd9ee8f4f59484133803c52ebbad3b5))

## [0.49.1](https://github.com/propeller-heads/tycho-indexer/compare/0.49.0...0.49.1) (2025-01-13)


### Bug Fixes

* map empty user identity to 'unknown' in metrics ([352016a](https://github.com/propeller-heads/tycho-indexer/commit/352016abbf6f9b8947af8a3a4baf1a0353a6ca33))
* map empty user identity to 'unknown' in metrics ([#485](https://github.com/propeller-heads/tycho-indexer/issues/485)) ([8d9fb62](https://github.com/propeller-heads/tycho-indexer/commit/8d9fb62c940f7b4d1c3c96e2d1dbb82242ef8f56))

## [0.49.0](https://github.com/propeller-heads/tycho-indexer/compare/0.48.0...0.49.0) (2025-01-10)


### Features

* add chain reorg metric ([b17b74f](https://github.com/propeller-heads/tycho-indexer/commit/b17b74f42b702d05d5283f010f7ebc0e5fe0766c))
* add substreams block message size metric ([5180275](https://github.com/propeller-heads/tycho-indexer/commit/5180275a876f29f06c9548d116cfc99980ac343a))
* extend substreams block message metrics ([#482](https://github.com/propeller-heads/tycho-indexer/issues/482)) ([b005e26](https://github.com/propeller-heads/tycho-indexer/commit/b005e26ec23b9ef318b324ac10301f04852d2be7))


### Bug Fixes

* add to and from block data to reorg metric ([44df826](https://github.com/propeller-heads/tycho-indexer/commit/44df826b5635864e1e6c067c7c8516e29fc94066))

## [0.48.0](https://github.com/propeller-heads/tycho-indexer/compare/0.47.0...0.48.0) (2025-01-10)


### Features

* bound end_idx to latest ([ea548ef](https://github.com/propeller-heads/tycho-indexer/commit/ea548ef21d78c3ace7c144223029f35b29f956ed))
* predefined behaviour for latest ts ([2ae6012](https://github.com/propeller-heads/tycho-indexer/commit/2ae60126cc71cd60a16d36e430e3b732a9b53542))
* revert changes and match current time ([81ad8e1](https://github.com/propeller-heads/tycho-indexer/commit/81ad8e19cfc654f8b21bd1817efaad34a1826fe7))
* revert previous changes && make version optional ([be15c0d](https://github.com/propeller-heads/tycho-indexer/commit/be15c0d8932219ca716644e1c4a685a02171401c))


### Bug Fixes

* delete all account balances for accounts to be deleted ([a3d9ffc](https://github.com/propeller-heads/tycho-indexer/commit/a3d9ffc0db659d8a074cb0e5c73f92fea9e1c72d))
* fix account balances bug ([c0b6678](https://github.com/propeller-heads/tycho-indexer/commit/c0b6678fb09786f0522a875cebae345e521ddf3c))
* fix protocol system deletion script account balances bug ([#477](https://github.com/propeller-heads/tycho-indexer/issues/477)) ([a749b53](https://github.com/propeller-heads/tycho-indexer/commit/a749b5305f23d4a66b177f6c89d6c269a62801e4))
* get_block_range ([6460d3d](https://github.com/propeller-heads/tycho-indexer/commit/6460d3d297a6667417f71f39fc066f897c0af762))
* test ([fe343d4](https://github.com/propeller-heads/tycho-indexer/commit/fe343d4e5e0396cb05c33a84de12fb3c1878a6ef))
* Update get_block_range to return full buffer when Ts is now or greater ([#470](https://github.com/propeller-heads/tycho-indexer/issues/470)) ([9507f32](https://github.com/propeller-heads/tycho-indexer/commit/9507f3280678be1b13f47feae7b90d523ef9d91e))

## [0.47.0](https://github.com/propeller-heads/tycho-indexer/compare/0.46.8...0.47.0) (2025-01-10)


### Features

* add user identity to ws connection metrics ([ac807b2](https://github.com/propeller-heads/tycho-indexer/commit/ac807b265f775aa27509cc3caa06274fddcda467))
* add user identity to ws connection metrics ([#480](https://github.com/propeller-heads/tycho-indexer/issues/480)) ([f2a3c8d](https://github.com/propeller-heads/tycho-indexer/commit/f2a3c8d1b0fbacdaea73277a60ca52a862b29d10))


### Bug Fixes

* default to 'unknown' if no user identity is present ([977f02e](https://github.com/propeller-heads/tycho-indexer/commit/977f02e6385ed283b0915ab020de6403a910f2df))

## [0.46.8](https://github.com/propeller-heads/tycho-indexer/compare/0.46.7...0.46.8) (2025-01-10)

## [0.46.7](https://github.com/propeller-heads/tycho-indexer/compare/0.46.6...0.46.7) (2025-01-09)


### Bug Fixes

* docker build ([68cb6a8](https://github.com/propeller-heads/tycho-indexer/commit/68cb6a8ef3abd146871ef5adaf8b91cfcf6e01bb)), closes [#474](https://github.com/propeller-heads/tycho-indexer/issues/474)
* docker build ([#475](https://github.com/propeller-heads/tycho-indexer/issues/475)) ([3569ee3](https://github.com/propeller-heads/tycho-indexer/commit/3569ee3ed22e25a8c78657ea09a87bf703451c58))

## [0.46.6](https://github.com/propeller-heads/tycho-indexer/compare/0.46.5...0.46.6) (2025-01-07)


### Bug Fixes

* changed validate pr condition ([c2913e6](https://github.com/propeller-heads/tycho-indexer/commit/c2913e618aba3000f4f5be0f1ae2b9a5f017b3a3))
* changed validate pr condition ([#476](https://github.com/propeller-heads/tycho-indexer/issues/476)) ([7bb6e84](https://github.com/propeller-heads/tycho-indexer/commit/7bb6e84bb31b63330d471265e6826eb9f8b5193f))
* try to run validate pr ([e646ed8](https://github.com/propeller-heads/tycho-indexer/commit/e646ed882662eb352c109f1391dda556591303ba))
* try to run validate pr ([4da002f](https://github.com/propeller-heads/tycho-indexer/commit/4da002f687cc1548f2cbf024e40c2270dc2bb4a5))
* try to run validate pr ([e85317d](https://github.com/propeller-heads/tycho-indexer/commit/e85317df16a7dcdf49847817ae7f89e9e250e2b5))

## [0.46.5](https://github.com/propeller-heads/tycho-indexer/compare/0.46.4...0.46.5) (2024-12-20)


### Bug Fixes

* switch block_processing_time metric to a gauge ([6ce8fd0](https://github.com/propeller-heads/tycho-indexer/commit/6ce8fd08f314862724a2a3aa805341cf02c5d88d))
* switch block_processing_time metric to a gauge ([#466](https://github.com/propeller-heads/tycho-indexer/issues/466)) ([8f2f584](https://github.com/propeller-heads/tycho-indexer/commit/8f2f5848d4dccf488557f9f55db0adb1baee9faa))

## [0.46.4](https://github.com/propeller-heads/tycho-indexer/compare/0.46.3...0.46.4) (2024-12-19)


### Bug Fixes

* calculate substream lag in millis ([5f3d503](https://github.com/propeller-heads/tycho-indexer/commit/5f3d503141f0b89c3c1ebae0f8c5f734b2b2770c))
* calculate substream lag in millis ([#465](https://github.com/propeller-heads/tycho-indexer/issues/465)) ([07916ff](https://github.com/propeller-heads/tycho-indexer/commit/07916fffbd61f6d68eaf3609762a41fa8b5c5057))

## [0.46.3](https://github.com/propeller-heads/tycho-indexer/compare/0.46.2...0.46.3) (2024-12-18)


### Bug Fixes

* rpc_requests metrics typo ([5cc490c](https://github.com/propeller-heads/tycho-indexer/commit/5cc490c6f6283c0c5b40bd54e17f8d5f290de3e2))
* split chain and extractor metric labels ([f15f2e2](https://github.com/propeller-heads/tycho-indexer/commit/f15f2e2ffa17cd4f34a4a8493bdb6e8892a7789e))
* split chain and extractor metric labels ([#463](https://github.com/propeller-heads/tycho-indexer/issues/463)) ([b98a5db](https://github.com/propeller-heads/tycho-indexer/commit/b98a5dbc74cbf76cfae8eae3d8b93a9c2c36d73a))

## [0.46.2](https://github.com/propeller-heads/tycho-indexer/compare/0.46.1...0.46.2) (2024-12-18)


### Bug Fixes

* decrement active subscription on ws connection close ([329fedd](https://github.com/propeller-heads/tycho-indexer/commit/329fedda025f42d6c65da931d4a9f0af1cc7a94c))
* decrement active subscription on ws connection close ([#462](https://github.com/propeller-heads/tycho-indexer/issues/462)) ([585ec90](https://github.com/propeller-heads/tycho-indexer/commit/585ec90fd6a62166a379e5a5a7a9e2917fd11a44))

## [0.46.1](https://github.com/propeller-heads/tycho-indexer/compare/0.46.0...0.46.1) (2024-12-17)


### Bug Fixes

* improve websocket metrics with extended metadata ([51c70eb](https://github.com/propeller-heads/tycho-indexer/commit/51c70eb54c8be6500f67335a393b89b60e9f9124))
* improve websocket metrics with extended metadata ([#460](https://github.com/propeller-heads/tycho-indexer/issues/460)) ([b8b3e1e](https://github.com/propeller-heads/tycho-indexer/commit/b8b3e1e8d5a9b966a830aed6dab18bfea72ba318))
* remove api key metric metadata ([4c12f56](https://github.com/propeller-heads/tycho-indexer/commit/4c12f5600f2d3cdbd185b8ced5274940c9434fc3))

## [0.46.0](https://github.com/propeller-heads/tycho-indexer/compare/0.45.2...0.46.0) (2024-12-16)


### Features

* **tycho-client:** increase pagination chunksize to 100 ([6c9b2da](https://github.com/propeller-heads/tycho-indexer/commit/6c9b2dac16b1f1ba999dc3028c480c908065037c))
* **tycho-client:** increase pagination chunksize to 100 ([#459](https://github.com/propeller-heads/tycho-indexer/issues/459)) ([c8b7ac6](https://github.com/propeller-heads/tycho-indexer/commit/c8b7ac6c6d30f484d03fffb6bcde29a7a4e41b6b))

## [0.45.2](https://github.com/propeller-heads/tycho-indexer/compare/0.45.1...0.45.2) (2024-12-13)


### Bug Fixes

* add extractor tag to block processing time metric ([d055d29](https://github.com/propeller-heads/tycho-indexer/commit/d055d295d0134e94d28bde725183f1f97686b360))
* add extractor tag to block processing time metric ([#458](https://github.com/propeller-heads/tycho-indexer/issues/458)) ([8305784](https://github.com/propeller-heads/tycho-indexer/commit/830578471caed1462a4e34148ade8680f00b569c))

## [0.45.1](https://github.com/propeller-heads/tycho-indexer/compare/0.45.0...0.45.1) (2024-12-12)


### Bug Fixes

* update SQL script to prune `transaction` table ([e43094a](https://github.com/propeller-heads/tycho-indexer/commit/e43094a529ea41c3a739ad458599bfc7eb627755))
* update SQL script to prune `transaction` table ([#455](https://github.com/propeller-heads/tycho-indexer/issues/455)) ([34ec325](https://github.com/propeller-heads/tycho-indexer/commit/34ec32557a5647bc828015e0813b6297ef9b4dc0))

## [0.45.0](https://github.com/propeller-heads/tycho-indexer/compare/0.44.0...0.45.0) (2024-12-12)


### Features

* add active websocket connections metric ([94f0a44](https://github.com/propeller-heads/tycho-indexer/commit/94f0a44c79a805c2075c30a219c3549811452ea4))
* add block processing time metric ([dc83c5e](https://github.com/propeller-heads/tycho-indexer/commit/dc83c5ea279c7d0140c65ba8404dcda60b9268a3))
* add dropped websocket connections metric ([5349418](https://github.com/propeller-heads/tycho-indexer/commit/53494184d16647c16fb6295e71fa6a45349ab57b))
* add metric for extractors current block ([ce89671](https://github.com/propeller-heads/tycho-indexer/commit/ce896718605054fc1073f1c4281c8b41d717b51b))
* add remaining sync time metric ([49de728](https://github.com/propeller-heads/tycho-indexer/commit/49de7281ae4f686b5332fdeaf32d73efc728c6e1))
* add RPC cache hits and misses count metrics ([10cef88](https://github.com/propeller-heads/tycho-indexer/commit/10cef881e903a1fb233df9dca0dca00fd370010a))
* add RPC failed requests count metric ([1dd9f95](https://github.com/propeller-heads/tycho-indexer/commit/1dd9f950ea7e8982f6b852f8f21597b106d04049))
* add RPC requests count metric ([571af0f](https://github.com/propeller-heads/tycho-indexer/commit/571af0fae7753abdf04eb1ca39f731224dff837d))
* add substream failure metrics ([258acb9](https://github.com/propeller-heads/tycho-indexer/commit/258acb90b1f5cfedc5f908ed7b526846544c72e1))
* add substreams lag metric ([485ea9c](https://github.com/propeller-heads/tycho-indexer/commit/485ea9cbf5d6b1d9508560ad33e072b2136d620f))
* add tycho-indexer metrics  ([#454](https://github.com/propeller-heads/tycho-indexer/issues/454)) ([13f780f](https://github.com/propeller-heads/tycho-indexer/commit/13f780fc4a5af6e5d0aa33987b94104c8f816044))


### Bug Fixes

* improve metric naming ([ddfedab](https://github.com/propeller-heads/tycho-indexer/commit/ddfedabdf85522b99cb8b9f42bc66001a5e1afef))
* improve substream metric labels ([3bfbf37](https://github.com/propeller-heads/tycho-indexer/commit/3bfbf378ce7f1feeed5dcee8601514b730d1a28e))

## [0.44.0](https://github.com/propeller-heads/tycho-indexer/compare/0.43.0...0.44.0) (2024-12-06)


### Features

* add metrics exporter and expose /metrics endpoint ([ff247c7](https://github.com/propeller-heads/tycho-indexer/commit/ff247c7a1b3e01c347bc66899be154b8143d4cfc))
* set up metrics exporter ([#453](https://github.com/propeller-heads/tycho-indexer/issues/453)) ([c426cd3](https://github.com/propeller-heads/tycho-indexer/commit/c426cd3e7588e62286d9cfc86c2d2e55204ff6fa))

## [0.43.0](https://github.com/propeller-heads/tycho-indexer/compare/0.42.3...0.43.0) (2024-11-29)


### Features

* Allow FeedMsg to be deserialized. ([f8d7655](https://github.com/propeller-heads/tycho-indexer/commit/f8d765554194ddd222e4c6f07811e8c99700615a))
* Allow FeedMsg to be deserialized. ([#451](https://github.com/propeller-heads/tycho-indexer/issues/451)) ([5d22803](https://github.com/propeller-heads/tycho-indexer/commit/5d228037843eb71555bb4478ca17a47e0ab996b7))

## [0.42.3](https://github.com/propeller-heads/tycho-indexer/compare/0.42.2...0.42.3) (2024-11-26)


### Bug Fixes

* **client:** remove hardcoded tycho host url ([2d9b1e1](https://github.com/propeller-heads/tycho-indexer/commit/2d9b1e1cda595c4a1329fcdd478bd2e57d77a260))
* **client:** remove hardcoded Tycho host url ([#449](https://github.com/propeller-heads/tycho-indexer/issues/449)) ([0181a1e](https://github.com/propeller-heads/tycho-indexer/commit/0181a1ef8c76a747e2192443525feb91973b155d))

## [0.42.2](https://github.com/propeller-heads/tycho-indexer/compare/0.42.1...0.42.2) (2024-11-25)


### Bug Fixes

* **rpc:** add buffer lookup for version given as block hash ([8fd6a86](https://github.com/propeller-heads/tycho-indexer/commit/8fd6a86eef8a328ef2ea625d71144e27af1529c9))
* **rpc:** add buffer lookup for version given as block hash ([#435](https://github.com/propeller-heads/tycho-indexer/issues/435)) ([a9672ad](https://github.com/propeller-heads/tycho-indexer/commit/a9672ad0e92dcc5af2f04c452e48e9007088a572))

## [0.42.1](https://github.com/propeller-heads/tycho-indexer/compare/0.42.0...0.42.1) (2024-11-20)


### Bug Fixes

* fix token analysis cronjob not setting quality for good tokens ([e0470dd](https://github.com/propeller-heads/tycho-indexer/commit/e0470dd0a97ea209d6789822ed80879e4311df6d))

## [0.42.0](https://github.com/propeller-heads/tycho-indexer/compare/0.41.1...0.42.0) (2024-11-19)


### Features

* **hex_bytes:** change hex bytes conversions to big endian ([3961824](https://github.com/propeller-heads/tycho-indexer/commit/39618244c4bbdc90d09af3af740edc34e6e68f76))
* **hex_bytes:** change hex bytes conversions to big endian ([#429](https://github.com/propeller-heads/tycho-indexer/issues/429)) ([e88a4c6](https://github.com/propeller-heads/tycho-indexer/commit/e88a4c67a9865f9505cf9827bb64e24e9cd73845))
* **tycho-ethereum:** update ether <-> bytes conversions to big endian ([3943560](https://github.com/propeller-heads/tycho-indexer/commit/39435606aaf0e92f9d5051a5df17617a0c7a075e))


### Bug Fixes

* make ethcontract optional ([2482ecc](https://github.com/propeller-heads/tycho-indexer/commit/2482ecc0592c1ef2592252543d00759fe22c11fc))

## [0.41.1](https://github.com/propeller-heads/tycho-indexer/compare/0.41.0...0.41.1) (2024-11-10)


### Bug Fixes

* fix formatting ([02f4d59](https://github.com/propeller-heads/tycho-indexer/commit/02f4d59ab7515a154110030eea97d956b8fcda47))
* fix token preprocessor symbol length to 255 chars ([0af6caa](https://github.com/propeller-heads/tycho-indexer/commit/0af6caa6c559222c12cc23f420c01dd5989a6a6c))
* fix token preprocessor symbol length to 255 chars ([#433](https://github.com/propeller-heads/tycho-indexer/issues/433)) ([466e620](https://github.com/propeller-heads/tycho-indexer/commit/466e6202e2409a47d0585dcbd535b63c23574e4b))
* **indexer:** correctly truncate token symbol ([9d7cd61](https://github.com/propeller-heads/tycho-indexer/commit/9d7cd6126c6e5dead0fa544a08c6d86807730ac8))

## [0.41.0](https://github.com/propeller-heads/tycho-indexer/compare/0.40.0...0.41.0) (2024-11-04)


### Features

* **tycho-client:** return the tokio handle from the stream builder ([06a669e](https://github.com/propeller-heads/tycho-indexer/commit/06a669e59ef2ec05c3aeb9a60b571a44ccf6e5ec))
* **tycho-client:** return the tokio handle from the stream builder ([#441](https://github.com/propeller-heads/tycho-indexer/issues/441)) ([173e774](https://github.com/propeller-heads/tycho-indexer/commit/173e774bd3726df5c93a938eafa8d1762a363250))

## [0.40.0](https://github.com/propeller-heads/tycho-indexer/compare/0.39.0...0.40.0) (2024-11-04)


### Features

* **tycho-client:** create rust client builder ([21d11a1](https://github.com/propeller-heads/tycho-indexer/commit/21d11a1a8dae0d79d675350902bbeaad60fa09a4))
* **tycho-client:** implement a rust client stream builder ([#439](https://github.com/propeller-heads/tycho-indexer/issues/439)) ([20be73c](https://github.com/propeller-heads/tycho-indexer/commit/20be73cdc4391805e2acf405ccf2fb5191dec3b7))
* **tycho-client:** improve error handling on TychoStreamBuilder ([b0a175c](https://github.com/propeller-heads/tycho-indexer/commit/b0a175ce0b4b484eecf85e12330f75925c3fa717))


### Bug Fixes

* **tycho-client:** do not error if no auth key is provided with tsl active ([19ac0d1](https://github.com/propeller-heads/tycho-indexer/commit/19ac0d12b1ed974b5ccb95cf2f62eca0f3b647ed))
* **tycho-client:** support fetching auth token from env var ([c1c03aa](https://github.com/propeller-heads/tycho-indexer/commit/c1c03aaa0a74b9170b57e70cf8e05fc9a8e79573))

## [0.39.0](https://github.com/propeller-heads/tycho-indexer/compare/0.38.0...0.39.0) (2024-11-02)


### Features

* **indexer:** expose s3 bucket as cli arg ([a55e126](https://github.com/propeller-heads/tycho-indexer/commit/a55e1265af2bf5b5bdc6c284c49128fc2590ae2f))
* **indexer:** parse s3 bucket from env variable ([c67fc38](https://github.com/propeller-heads/tycho-indexer/commit/c67fc3812007edc644b86efc45fa499aa098b2a9))
* **indexer:** parse s3 bucket from env variable ([#440](https://github.com/propeller-heads/tycho-indexer/issues/440)) ([104c4e9](https://github.com/propeller-heads/tycho-indexer/commit/104c4e90cf18a81dfe40ea5ea71b57f9be607691))

## [0.38.0](https://github.com/propeller-heads/tycho-indexer/compare/0.37.0...0.38.0) (2024-10-31)


### Features

* **rpc:** mark chain field in version param as deprecated ([8c00bde](https://github.com/propeller-heads/tycho-indexer/commit/8c00bdeb75edac5939f5eff415639cba7dd0d420))
* **rpc:** remove chain param from individual protocol ids ([4386dd0](https://github.com/propeller-heads/tycho-indexer/commit/4386dd0993d94484b39e861c989b559dd1f82ee0))
* **rpc:** remove chain param from individual protocol ids ([#437](https://github.com/propeller-heads/tycho-indexer/issues/437)) ([8c10d6a](https://github.com/propeller-heads/tycho-indexer/commit/8c10d6a9f8960f2d0e6d3bba50d1f82a41e935f2))
* **tycho-client:** update state endpoint body ([c535f79](https://github.com/propeller-heads/tycho-indexer/commit/c535f79a35bad00ca2443a1d643a9a344bb44118))

## [0.37.0](https://github.com/propeller-heads/tycho-indexer/compare/0.36.0...0.37.0) (2024-10-30)


### Features

* **storage:** update protocol state fetch query to apply all given filters ([1f64df2](https://github.com/propeller-heads/tycho-indexer/commit/1f64df222e7b258571e4b176dc779321f8ca504f))
* **storage:** update protocol state fetch query to apply all given filters ([#432](https://github.com/propeller-heads/tycho-indexer/issues/432)) ([5cb824e](https://github.com/propeller-heads/tycho-indexer/commit/5cb824e13c1cb4c86b290162cdeca6200307bb9f))

## [0.36.0](https://github.com/propeller-heads/tycho-indexer/compare/0.35.3...0.36.0) (2024-10-30)


### Features

* **scripts:** add balance check in uniswapv3 validation script ([25dc808](https://github.com/propeller-heads/tycho-indexer/commit/25dc8082d7223d89e4de12a539612d007f66fb5f))
* **scripts:** update uniswapv3 check script ([e1a1ce0](https://github.com/propeller-heads/tycho-indexer/commit/e1a1ce0f7351b29069d2f5b1bf9b9be2f3075012))

## [0.35.3](https://github.com/propeller-heads/tycho-indexer/compare/0.35.2...0.35.3) (2024-10-25)


### Bug Fixes

* **indexer:** correctly handle attributes deletions ([57fad94](https://github.com/propeller-heads/tycho-indexer/commit/57fad947c172ad0b5ecf9078a32580c204df679d))
* **indexer:** correctly handle attributes deletions ([#420](https://github.com/propeller-heads/tycho-indexer/issues/420)) ([faa08f6](https://github.com/propeller-heads/tycho-indexer/commit/faa08f645fc8a63ad2ed1fcd8689f586f96af1b5))

## [0.35.2](https://github.com/propeller-heads/tycho-indexer/compare/0.35.1...0.35.2) (2024-10-25)


### Bug Fixes

* pacakge release workflow ([5881d64](https://github.com/propeller-heads/tycho-indexer/commit/5881d641467325f26a164c73dd7cd64cbb344135))
* pacakge release workflow ([#431](https://github.com/propeller-heads/tycho-indexer/issues/431)) ([7b329a3](https://github.com/propeller-heads/tycho-indexer/commit/7b329a3f90fda2b1571617adcad83e8b7c36d39e))

## [0.35.1](https://github.com/propeller-heads/tycho-indexer/compare/0.35.0...0.35.1) (2024-10-24)


### Bug Fixes

* Fix ProtocolState RPC pagination by pre-paginating IDs ([fa485d2](https://github.com/propeller-heads/tycho-indexer/commit/fa485d25825df645380ea5d155bae010006f5ff4))
* Fix ProtocolState RPC pagination by pre-paginating IDs ([#425](https://github.com/propeller-heads/tycho-indexer/issues/425)) ([64ee5ce](https://github.com/propeller-heads/tycho-indexer/commit/64ee5ce22a56445b1bf7f970e96e78206b57f40d))
* remove unnecessary clone ([287d57f](https://github.com/propeller-heads/tycho-indexer/commit/287d57f9565bf2da4dcc80c406da6d9f845b0867))
* return total components when no id is specified for protocol_states ([8a78cb9](https://github.com/propeller-heads/tycho-indexer/commit/8a78cb9d6a7825afd66f4fa6b15483d8ce3ea771))

## [0.35.0](https://github.com/propeller-heads/tycho-indexer/compare/0.34.1...0.35.0) (2024-10-24)


### Features

* **ci:** Build wheels for python client. ([b882252](https://github.com/propeller-heads/tycho-indexer/commit/b8822526aa73f5643f5cf821ce1f64febc07605a))
* Ship tycho-client-py with binaries ([#427](https://github.com/propeller-heads/tycho-indexer/issues/427)) ([6e55465](https://github.com/propeller-heads/tycho-indexer/commit/6e55465fc8cc6a5f636016b7d45f19310b0c5ea8))
* **tycho-client:** Distribute binary with python lib. ([1540a4a](https://github.com/propeller-heads/tycho-indexer/commit/1540a4a9fb495746f479d4ce1ed5fd9477ae8556))

## [0.34.1](https://github.com/propeller-heads/tycho-indexer/compare/0.34.0...0.34.1) (2024-10-23)


### Bug Fixes

* **rpc:** correctly pass down delta buffer in RPC ([a1f35d8](https://github.com/propeller-heads/tycho-indexer/commit/a1f35d8bd4871ec01386155c894db8d74ff3f180))
* **rpc:** correctly pass down delta buffer in RPC ([#428](https://github.com/propeller-heads/tycho-indexer/issues/428)) ([de328a0](https://github.com/propeller-heads/tycho-indexer/commit/de328a04b529331586d40363e2aa1f4b68f79bbe))

## [0.34.0](https://github.com/propeller-heads/tycho-indexer/compare/0.33.1...0.34.0) (2024-10-22)


### Features

* **indexer:** introduce configurable post processors ([a9b9f2c](https://github.com/propeller-heads/tycho-indexer/commit/a9b9f2ced4f79cd95431a02a9c32dd6234a7dcc4))
* **indexer:** introduce configurable post processors ([#423](https://github.com/propeller-heads/tycho-indexer/issues/423)) ([4627cb4](https://github.com/propeller-heads/tycho-indexer/commit/4627cb430d3a80773e474e29ced4491d4c4e1eae))


### Bug Fixes

* correctly propagate missing post processor error ([5f655ae](https://github.com/propeller-heads/tycho-indexer/commit/5f655aee05c3f7c5cf2fe9c55d30e7c5c495da92))

## [0.33.1](https://github.com/propeller-heads/tycho-indexer/compare/0.33.0...0.33.1) (2024-10-22)


### Bug Fixes

* added secrets for build and push ([#421](https://github.com/propeller-heads/tycho-indexer/issues/421)) ([6567024](https://github.com/propeller-heads/tycho-indexer/commit/656702412ea34b5527c0505cb845dc85b2bd6ddf))
* **rpc:** allow to run `RpcHandler` without pending deltas. ([9ebb629](https://github.com/propeller-heads/tycho-indexer/commit/9ebb6296851d32c3d213bd7772bb60bf7bff0801))
* **rpc:** correctly handle requests with no ids specified ([#412](https://github.com/propeller-heads/tycho-indexer/issues/412)) ([8c04f17](https://github.com/propeller-heads/tycho-indexer/commit/8c04f171031b101b9b409372cc22ad7251676ad4))
* **rpc:** correctly handle when no ids are requested ([5470bf1](https://github.com/propeller-heads/tycho-indexer/commit/5470bf1600984826f0c1f7495ab20ec42121d7ae))
* **rpc:** fix running RPC without extractors ([#411](https://github.com/propeller-heads/tycho-indexer/issues/411)) ([c5b05cb](https://github.com/propeller-heads/tycho-indexer/commit/c5b05cbf6097c83c2ea90ce472ea880329f6d3ea))

## [0.33.0](https://github.com/propeller-heads/tycho-indexer/compare/0.32.0...0.33.0) (2024-10-11)


### Features

* **rpc:** return custom message for RPC error ([02daedb](https://github.com/propeller-heads/tycho-indexer/commit/02daedb1a8fc66d7d0847eaa81f589f020a77881))
* **rpc:** return custom message for RPC error ([#414](https://github.com/propeller-heads/tycho-indexer/issues/414)) ([6a51645](https://github.com/propeller-heads/tycho-indexer/commit/6a51645e986c14b1d9a9ec00dcbae3dd2cca746d))

## [0.32.0](https://github.com/propeller-heads/tycho-indexer/compare/0.31.3...0.32.0) (2024-10-09)


### Features

* **tycho-client:** publicly expose snapshot vm storage ([55e7875](https://github.com/propeller-heads/tycho-indexer/commit/55e78752627dc1ee36f45eeaf7f56797b704f2ee))
* **tycho-client:** publicly expose snapshot vm storage ([#413](https://github.com/propeller-heads/tycho-indexer/issues/413)) ([ca2a3e7](https://github.com/propeller-heads/tycho-indexer/commit/ca2a3e7aef90a4aeee5c4b5d96741fc0aeb6db50))

## [0.31.3](https://github.com/propeller-heads/tycho-indexer/compare/0.31.2...0.31.3) (2024-10-07)

## [0.31.2](https://github.com/propeller-heads/tycho-indexer/compare/0.31.1...0.31.2) (2024-10-07)


### Bug Fixes

* **substreams:** output type in Substreams modules ([7d62512](https://github.com/propeller-heads/tycho-indexer/commit/7d625128d5242f2e7b589bff19e863af06070580))
* **uniswap-v2-substreams:** use correct strucs in store pools module. ([16bbfc3](https://github.com/propeller-heads/tycho-indexer/commit/16bbfc3ce4805b8e5c6fce74c43d0e7f09eb266b))

## [0.31.1](https://github.com/propeller-heads/tycho-indexer/compare/0.31.0...0.31.1) (2024-10-07)


### Bug Fixes

* exit build_wheel script on failed internal command ([47c30c7](https://github.com/propeller-heads/tycho-indexer/commit/47c30c7c66d0d32b4a8a2922e8e9bf06a37acb11))

## [0.31.0](https://github.com/propeller-heads/tycho-indexer/compare/0.30.2...0.31.0) (2024-10-07)


### Features

* Add auth token support to tycho python client ([#406](https://github.com/propeller-heads/tycho-indexer/issues/406)) ([85376a6](https://github.com/propeller-heads/tycho-indexer/commit/85376a624da4d91eef52c584602727cf2a7bf44e))
* add auth token to tycho python client rpc ([1033802](https://github.com/propeller-heads/tycho-indexer/commit/1033802b1c852b87d0e9b0761ad08be79855a8ce))
* add auth token to tycho python client stream constructor ([d1f21bc](https://github.com/propeller-heads/tycho-indexer/commit/d1f21bc18af60e309f289b9f652a031a9d7c9f47))

## [0.30.2](https://github.com/propeller-heads/tycho-indexer/compare/0.30.1...0.30.2) (2024-10-07)


### Bug Fixes

* also cache component requests for specified components ([ce0f559](https://github.com/propeller-heads/tycho-indexer/commit/ce0f55937dc4eda18d5c74df0a2f6aa50d253ee3))
* Also cache component requests for specified components ([#404](https://github.com/propeller-heads/tycho-indexer/issues/404)) ([ff49333](https://github.com/propeller-heads/tycho-indexer/commit/ff49333416299b5b2c58976236baab08d29cee0e))

## [0.30.1](https://github.com/propeller-heads/tycho-indexer/compare/0.30.0...0.30.1) (2024-10-07)


### Bug Fixes

* **tycho-indexer:** correctly `buffered_range` to the span ([761eb72](https://github.com/propeller-heads/tycho-indexer/commit/761eb722a048dc93817e0db6f323d7e7cf5c1de7))
* **tycho-indexer:** correctly `buffered_range` to the span ([#405](https://github.com/propeller-heads/tycho-indexer/issues/405)) ([9adfcf7](https://github.com/propeller-heads/tycho-indexer/commit/9adfcf71f11dd4790c0b557772d60544a88b0fdf))

## [0.30.0](https://github.com/propeller-heads/tycho-indexer/compare/0.29.1...0.30.0) (2024-10-07)


### Features

* **tycho-indexer:** add span for `get_block_range` ([40d3c30](https://github.com/propeller-heads/tycho-indexer/commit/40d3c30b07f52719a64b7906a670e680af9cc8a8))
* **tycho-indexer:** add span for `get_block_range` ([#403](https://github.com/propeller-heads/tycho-indexer/issues/403)) ([a3df5b4](https://github.com/propeller-heads/tycho-indexer/commit/a3df5b416623dc1329375cbe1e4d03c6e7250375))

## [0.29.1](https://github.com/propeller-heads/tycho-indexer/compare/0.29.0...0.29.1) (2024-10-07)


### Bug Fixes

* increase component cache capacity ([4c2c611](https://github.com/propeller-heads/tycho-indexer/commit/4c2c611cbe4176b0d8797ad57a6b19587c5e16d4))
* increase component cache capacity ([#402](https://github.com/propeller-heads/tycho-indexer/issues/402)) ([501afbc](https://github.com/propeller-heads/tycho-indexer/commit/501afbc9303ff9e06af7beb222a561b8c5d8a16c))

## [0.29.0](https://github.com/propeller-heads/tycho-indexer/compare/0.28.0...0.29.0) (2024-10-04)


### Features

* **rpc:** add events in delta buffer ([9047b7c](https://github.com/propeller-heads/tycho-indexer/commit/9047b7ce473478ee6cad50a584bcc0c96b972729))
* **rpc:** add events in delta buffer ([#398](https://github.com/propeller-heads/tycho-indexer/issues/398)) ([8bbe273](https://github.com/propeller-heads/tycho-indexer/commit/8bbe273697b6f760db645a7e3695b0eb55ca512b))

## [0.28.0](https://github.com/propeller-heads/tycho-indexer/compare/0.27.0...0.28.0) (2024-10-04)


### Features

* improve rpc spans ([#397](https://github.com/propeller-heads/tycho-indexer/issues/397)) ([296b71c](https://github.com/propeller-heads/tycho-indexer/commit/296b71c09f37222aed7b54756e7098c0af212099))
* **rpc:** Add pagination and protocol attributes to rpc spans. ([5119d44](https://github.com/propeller-heads/tycho-indexer/commit/5119d446417612684547d9bbc7d90378effca0f3))
* **rpc:** Improve cache tracing spans. ([9241119](https://github.com/propeller-heads/tycho-indexer/commit/9241119353bb4c6c50aa65d8088002a32fc75afd))

## [0.27.0](https://github.com/propeller-heads/tycho-indexer/compare/0.26.0...0.27.0) (2024-10-03)


### Features

* **rpc:** Implement per-key sharded locking in RpcCache ([ef68ca2](https://github.com/propeller-heads/tycho-indexer/commit/ef68ca2491e5fb7c1be58148b8b7e19bb092100b))
* **rpc:** Implement per-key sharded locking in RpcCache ([#396](https://github.com/propeller-heads/tycho-indexer/issues/396)) ([f0337bf](https://github.com/propeller-heads/tycho-indexer/commit/f0337bf2fb01aab478eb7f41c47c9d3f8718b8f6))

## [0.26.0](https://github.com/propeller-heads/tycho-indexer/compare/0.25.1...0.26.0) (2024-10-03)


### Features

* **client:** Ensure StateSynchronizer waits for initialization ([a378483](https://github.com/propeller-heads/tycho-indexer/commit/a378483a73b70d184f25e1093fe256ef0f383437))
* **client:** Ensure StateSynchronizer waits for initialization ([#393](https://github.com/propeller-heads/tycho-indexer/issues/393)) ([7852e43](https://github.com/propeller-heads/tycho-indexer/commit/7852e432f77a41dac6dc6dacba64e4bab9e68153))

## [0.25.1](https://github.com/propeller-heads/tycho-indexer/compare/0.25.0...0.25.1) (2024-10-03)


### Bug Fixes

* avoid concurrent requests for empty pages ([a881bba](https://github.com/propeller-heads/tycho-indexer/commit/a881bba3f780d86928d5b119acf9f973f2fb7513))
* avoid concurrent requests for empty pages ([#392](https://github.com/propeller-heads/tycho-indexer/issues/392)) ([681ae7e](https://github.com/propeller-heads/tycho-indexer/commit/681ae7e145403fc7e876d2216ff215f466ecd075))
* cache condition was reversed ([6f45a26](https://github.com/propeller-heads/tycho-indexer/commit/6f45a2639ca23c8f0447c8d263c02dde58e32f17))
* **client:** only apply concurrency once total is known ([82a65f1](https://github.com/propeller-heads/tycho-indexer/commit/82a65f13c5551ac6061fc38008c1c898e036789c))
* skip caching last page of components response ([7eff7bf](https://github.com/propeller-heads/tycho-indexer/commit/7eff7bfa91fc0229daaf86e158c82f2a7e9caddf))

## [0.25.0](https://github.com/propeller-heads/tycho-indexer/compare/0.24.1...0.25.0) (2024-10-03)


### Features

* **rpc:** add spans and event around the delta buffer and components query ([fe1a1f4](https://github.com/propeller-heads/tycho-indexer/commit/fe1a1f48e1a9d6ece1a540dc440847e71be5af54))
* **rpc:** add spans for cache, tokens and components requests ([e7112b0](https://github.com/propeller-heads/tycho-indexer/commit/e7112b0ebea8533b0ed9c3cc78741734e57a61e9))
* **rpc:** add spans for cache, tokens and components requests ([#394](https://github.com/propeller-heads/tycho-indexer/issues/394)) ([0c67a3d](https://github.com/propeller-heads/tycho-indexer/commit/0c67a3d329a19cb62953a31e9032d3da66bc2836))

## [0.24.1](https://github.com/propeller-heads/tycho-indexer/compare/0.24.0...0.24.1) (2024-10-02)


### Bug Fixes

* add order by to paginated queries ([b198f07](https://github.com/propeller-heads/tycho-indexer/commit/b198f07a3a05af4376c056b1925bbf80c38b63e6))
* add order by to paginated queries ([#391](https://github.com/propeller-heads/tycho-indexer/issues/391)) ([1db42b1](https://github.com/propeller-heads/tycho-indexer/commit/1db42b187daaee72d9a4be278b05270c523a6414))

## [0.24.0](https://github.com/propeller-heads/tycho-indexer/compare/0.23.1...0.24.0) (2024-10-02)


### Features

* Add component cache to rpc ([#390](https://github.com/propeller-heads/tycho-indexer/issues/390)) ([8dfc004](https://github.com/propeller-heads/tycho-indexer/commit/8dfc00430f4a8c0bb0cd4f152c640f91ef58b6b8))
* **rpc:** add component cache ([c7a1894](https://github.com/propeller-heads/tycho-indexer/commit/c7a189430df09aa1c8d517e0fe0a2468f0127cb9))


### Bug Fixes

* order components before pagination ([9eee0af](https://github.com/propeller-heads/tycho-indexer/commit/9eee0afd36706cae0c6a70679491d935664f6327))

## [0.23.1](https://github.com/propeller-heads/tycho-indexer/compare/0.23.0...0.23.1) (2024-10-02)


### Bug Fixes

* increase protocol component pagination page size ([2565c05](https://github.com/propeller-heads/tycho-indexer/commit/2565c051eb971aa697128c41a64a9e45913c1b13))
* increase protocol component pagination page size ([#389](https://github.com/propeller-heads/tycho-indexer/issues/389)) ([a90cad6](https://github.com/propeller-heads/tycho-indexer/commit/a90cad6998a357a259e115e4265b4759bc82699f))

## [0.23.0](https://github.com/propeller-heads/tycho-indexer/compare/0.22.5...0.23.0) (2024-10-02)


### Features

* **tycho-indexer:** make number of worker parametrable ([fc6e334](https://github.com/propeller-heads/tycho-indexer/commit/fc6e334b7b81d9e37076127038ff45c5fcb7518c))
* **tycho-indexer:** make number of worker parametrable ([#388](https://github.com/propeller-heads/tycho-indexer/issues/388)) ([f59397a](https://github.com/propeller-heads/tycho-indexer/commit/f59397a6ad0833ce67d2cc40e80a8746bad21b95))

## [0.22.5](https://github.com/propeller-heads/tycho-indexer/compare/0.22.4...0.22.5) (2024-10-01)


### Bug Fixes

* **otel:** create tracing subscriber inside the runtime ([65c3a02](https://github.com/propeller-heads/tycho-indexer/commit/65c3a02601b8b2cba53ecb8b70c905da2841f87a))
* Tokio runtime issue ([#387](https://github.com/propeller-heads/tycho-indexer/issues/387)) ([40ec9b2](https://github.com/propeller-heads/tycho-indexer/commit/40ec9b2ad29f7fe3f4ee98533fc5dac261b9d669))

## [0.22.4](https://github.com/propeller-heads/tycho-indexer/compare/0.22.3...0.22.4) (2024-10-01)

## [0.22.3](https://github.com/propeller-heads/tycho-indexer/compare/0.22.2...0.22.3) (2024-10-01)


### Bug Fixes

* **client:** populating python client error log bug ([#386](https://github.com/propeller-heads/tycho-indexer/issues/386)) ([87bcae5](https://github.com/propeller-heads/tycho-indexer/commit/87bcae5c37ef5c1d2a422ebbee53e64ed0d3d2e4))
* populating python client error log bug ([e6d9682](https://github.com/propeller-heads/tycho-indexer/commit/e6d9682704bae29e04d4dc4bac356866a4e42d1d))

## [0.22.2](https://github.com/propeller-heads/tycho-indexer/compare/0.22.1...0.22.2) (2024-10-01)


### Bug Fixes

* **client:** improve python client stream error logging ([#385](https://github.com/propeller-heads/tycho-indexer/issues/385)) ([5a243de](https://github.com/propeller-heads/tycho-indexer/commit/5a243dec32f9fda31cdb3cc3cf6d97e12897d63e))
* **client:** make python client error logs more readable ([baa1fa2](https://github.com/propeller-heads/tycho-indexer/commit/baa1fa20175d6a8b53b64aeec75e71c107d7889a))

## [0.22.1](https://github.com/propeller-heads/tycho-indexer/compare/0.22.0...0.22.1) (2024-09-30)


### Bug Fixes

* **client:** use new tokens endpoint ([02fb389](https://github.com/propeller-heads/tycho-indexer/commit/02fb389a648e46eb26199d2108c844934a6c9271))
* **client:** use new tokens endpoint ([#384](https://github.com/propeller-heads/tycho-indexer/issues/384)) ([5d568cd](https://github.com/propeller-heads/tycho-indexer/commit/5d568cd8bd55cac7fa787c0f3c8f68db09400d07))

## [0.22.0](https://github.com/propeller-heads/tycho-indexer/compare/0.21.0...0.22.0) (2024-09-30)


### Features

* **cache:** add tracing spans for every read methods ([be40f4c](https://github.com/propeller-heads/tycho-indexer/commit/be40f4cc1d2c5614d5ecd0a59ee4617edbfec894))
* **cache:** add tracing spans for every write methods on the 'CachedGateway' ([88ac98e](https://github.com/propeller-heads/tycho-indexer/commit/88ac98e61e895a6ac0c3fd3e566cb672c0761e87))
* **extractor:** improve database commits ([4a431df](https://github.com/propeller-heads/tycho-indexer/commit/4a431dfe3e88a3e642cf76455bc3e974dafa5cdd))
* **indexing:** Improve database commits logic ([#380](https://github.com/propeller-heads/tycho-indexer/issues/380)) ([55d40b9](https://github.com/propeller-heads/tycho-indexer/commit/55d40b9df1e315e77b4ea44647ba550412fb4582))

## [0.21.0](https://github.com/propeller-heads/tycho-indexer/compare/0.20.0...0.21.0) (2024-09-30)


### Features

* add method to get protocol components paginated ([7ce3cbe](https://github.com/propeller-heads/tycho-indexer/commit/7ce3cbe56305de3b821cf9b615718182997b3fc3))
* limit the page size for paginated endpoints ([26e8767](https://github.com/propeller-heads/tycho-indexer/commit/26e876767292a8d2751dfd651ff6f018d4c3fec5))
* more fixes ([29f1117](https://github.com/propeller-heads/tycho-indexer/commit/29f1117cb392b6b3cb113d0dd240ac0708828913))
* Return total count to pagination responses, get_contract_state ([d780212](https://github.com/propeller-heads/tycho-indexer/commit/d780212739025d6cda52b55fda82d9e04332857e))
* Return total count to pagination responses, get_protocol_components ([8115c9b](https://github.com/propeller-heads/tycho-indexer/commit/8115c9b41e7ff0e38c568cecb6ddeb5adcc1da1b))
* Return total count to pagination responses, get_protocol_state ([cfafb70](https://github.com/propeller-heads/tycho-indexer/commit/cfafb70c2ce38da01203c5f4a90d62c4815e2413))
* Return total count to pagination responses, get_tokens ([69ab6f7](https://github.com/propeller-heads/tycho-indexer/commit/69ab6f758f5be67c4f0007230ce43bbb5c2cf242))
* **rpc:** add pagination to all rpc endpoints ([39107f3](https://github.com/propeller-heads/tycho-indexer/commit/39107f36ffbd0c47609cb72977a67c3e68acf813))
* **rpc:** add pagination to all rpc endpoints ([#345](https://github.com/propeller-heads/tycho-indexer/issues/345)) ([f945d75](https://github.com/propeller-heads/tycho-indexer/commit/f945d753ed6da04a1b643e517db4949b981a9550))
* use pagination on rpc sync ([fd10192](https://github.com/propeller-heads/tycho-indexer/commit/fd1019207ab391c50f678afc1aefd35c2f2d269a))


### Bug Fixes

* bug with page and page_size swapped ([0aa2afd](https://github.com/propeller-heads/tycho-indexer/commit/0aa2afdd6e8079ee87422529d7c563d7a3891f87))
* correctly handle buffered components in pagination ([9f7bdbc](https://github.com/propeller-heads/tycho-indexer/commit/9f7bdbc01350627aebf0e5d2fdf49d856929c51f))
* correctly handle buffered contract states in pagination ([57209aa](https://github.com/propeller-heads/tycho-indexer/commit/57209aa9250790e6e126ff4c3a17be946dbfdb91))
* correctly handle buffered protocol states in pagination ([6afcc63](https://github.com/propeller-heads/tycho-indexer/commit/6afcc63bfd5c7779de8aa83787b80c3fe748f8a1))
* correctly pass state request ids chunk ([d5282cb](https://github.com/propeller-heads/tycho-indexer/commit/d5282cb9575c202b7353a63f20436c26bb01012c))
* fix pagination for contract_state ([22c8497](https://github.com/propeller-heads/tycho-indexer/commit/22c8497a0e9d1266102997f3bb44f617ac389977))
* fix pagination for contract_state by chain ([3e543e4](https://github.com/propeller-heads/tycho-indexer/commit/3e543e4fc99d1bb3518eee63e6e44a8a9e5d7639))
* fix pagination for fetching ProtocolState, add tests ([0ba95d9](https://github.com/propeller-heads/tycho-indexer/commit/0ba95d9bbbada13db9ccde5617e47383ab8f90a4))
* paginate contract_state using chunked ids ([87e702e](https://github.com/propeller-heads/tycho-indexer/commit/87e702e3ebcaff14633772ec210c9c276ebe6989))
* post rebase fixes, use Bytes instead of contractId ([ae57952](https://github.com/propeller-heads/tycho-indexer/commit/ae57952f6409923001222469eecf536617df4aef))
* rebased contract struct name change ([d04a519](https://github.com/propeller-heads/tycho-indexer/commit/d04a5192354dd5e14754bbaa2ed96bcc16e655a3))
* remove unnecessary filters ([c94be3f](https://github.com/propeller-heads/tycho-indexer/commit/c94be3fc650ce7e104b4d7221eabcbd588fac940))
* remove unnecessary uniqueness constraints ([7187f70](https://github.com/propeller-heads/tycho-indexer/commit/7187f70c8f42a09f178ef6b6bc91c543d9252661))
* undo formatting errors and typos ([dc68a61](https://github.com/propeller-heads/tycho-indexer/commit/dc68a6103e31eb3451a375f526f76bda531679b3))
* use total from pagination response to end pagination looping ([c763f96](https://github.com/propeller-heads/tycho-indexer/commit/c763f96c9cb599844178e3900b2954b84c4c1307))

## [0.20.0](https://github.com/propeller-heads/tycho-indexer/compare/0.19.0...0.20.0) (2024-09-26)


### Features

* **rpc:** Add a cache for contract storage. ([8e9c6d3](https://github.com/propeller-heads/tycho-indexer/commit/8e9c6d3c2c6a2ed5f3d54e572b93979b63e14817))
* **rpc:** Add a cache for protocol state. ([e020b08](https://github.com/propeller-heads/tycho-indexer/commit/e020b081f6102f97c40e3efea938af7e2aca81c5))
* **rpc:** Generalize RPC caching strategy. ([e4e4226](https://github.com/propeller-heads/tycho-indexer/commit/e4e4226b44a5c9dda881a4627831e4c10391f18c))
* **rpc:** Protocol state and contract storage rpc caching ([#378](https://github.com/propeller-heads/tycho-indexer/issues/378)) ([9cccd5d](https://github.com/propeller-heads/tycho-indexer/commit/9cccd5d0abf19d53d74ea07ad0ba6051f53a1d3c))

## [0.19.0](https://github.com/propeller-heads/tycho-indexer/compare/0.18.4...0.19.0) (2024-09-26)


### Features

* **tycho-client-py:** add no-tls flag to `TychoStream` ([68a184e](https://github.com/propeller-heads/tycho-indexer/commit/68a184e4f03654692444a93fa72bc22fed4757d8))
* **tycho-client:** add `no-tls` flag to allow using unsecured transports ([2f56780](https://github.com/propeller-heads/tycho-indexer/commit/2f5678025418ee0066d079d531b790d0ea1075d0))
* **tycho-client:** add `user-agent` to websocket connection requests ([008ab20](https://github.com/propeller-heads/tycho-indexer/commit/008ab20676b5021c5257988d076db547ffb9da5f))
* **tycho-client:** add auth key and support for https ([#379](https://github.com/propeller-heads/tycho-indexer/issues/379)) ([c37c9ad](https://github.com/propeller-heads/tycho-indexer/commit/c37c9adc21c81ee241cd1affe0e7e3425272f485))
* **tycho-client:** add Auth to websocket client ([e2e6ade](https://github.com/propeller-heads/tycho-indexer/commit/e2e6adefcb3e926111f3e0ab3fc75f1f603a7a5f))
* **tycho-client:** enable HTTPS and add auth key ([bbd0eee](https://github.com/propeller-heads/tycho-indexer/commit/bbd0eee1af932ba84e913f5e173e08a3f43010c6))
* **tycho-client:** get `auth-key` from env or cli ([37d02a0](https://github.com/propeller-heads/tycho-indexer/commit/37d02a0ce085eea1b32d7abc84800bdf0143937d))

## [0.18.4](https://github.com/propeller-heads/tycho-indexer/compare/0.18.3...0.18.4) (2024-09-26)


### Bug Fixes

* fix delete protocol script bug ([450e6c0](https://github.com/propeller-heads/tycho-indexer/commit/450e6c048337e40c65f40ef2eee0d91426f3611d))
* remove deleted attributes from default table ([aab9a76](https://github.com/propeller-heads/tycho-indexer/commit/aab9a76ecdd3d87379e5caff80ae587aa0dd9d53))
* Remove deleted attributes from default table ([#374](https://github.com/propeller-heads/tycho-indexer/issues/374)) ([a6b15b6](https://github.com/propeller-heads/tycho-indexer/commit/a6b15b63ca7e851300dde9a382ce66278c271ebc))
* skip deleted attributes delete query if no attr are deleted ([0e7bb6c](https://github.com/propeller-heads/tycho-indexer/commit/0e7bb6cb5c6bbf4d1bd913e9848f81c8034c90ee))

## [0.18.3](https://github.com/propeller-heads/tycho-indexer/compare/0.18.2...0.18.3) (2024-09-23)


### Bug Fixes

* add chain awareness to extraction state block migration ([ef92989](https://github.com/propeller-heads/tycho-indexer/commit/ef92989ee682003a6add6900a397f11cb0e7a9da))
* add chain awareness to extraction state block migration ([#361](https://github.com/propeller-heads/tycho-indexer/issues/361)) ([7a509e1](https://github.com/propeller-heads/tycho-indexer/commit/7a509e10a5d708141d801c65b4c608b0b7a94f40))

## [0.18.2](https://github.com/propeller-heads/tycho-indexer/compare/0.18.1...0.18.2) (2024-09-23)

## [0.18.1](https://github.com/propeller-heads/tycho-indexer/compare/0.18.0...0.18.1) (2024-09-20)

## [0.18.0](https://github.com/propeller-heads/tycho-indexer/compare/0.17.5...0.18.0) (2024-09-19)


### Features

* automate removal of orphaned transactions ([d0939a5](https://github.com/propeller-heads/tycho-indexer/commit/d0939a59bf2b1e05fe48ddec0d0a2af73980f79d))
* Automate removal of orphaned transactions from the DB ([#349](https://github.com/propeller-heads/tycho-indexer/issues/349)) ([898460d](https://github.com/propeller-heads/tycho-indexer/commit/898460dfaf10d40f2f1c714d7deec6ca9fc73ae5))


### Bug Fixes

* delete transactions in batches ([fd128fd](https://github.com/propeller-heads/tycho-indexer/commit/fd128fd2a8bd600a7ae21ca9f0842d0ac38125f9))
* improve transaction clean up script to minimise db locks ([fffe9cc](https://github.com/propeller-heads/tycho-indexer/commit/fffe9cc7d085a1f9633806ebe1664eb3d122600a))
* skip batching on search phase ([ceb5376](https://github.com/propeller-heads/tycho-indexer/commit/ceb5376ed4da853ae80f00410e7c442f59a1cd4d))
* speed up deletions with indexes ([ebea183](https://github.com/propeller-heads/tycho-indexer/commit/ebea1837a051bf36ac2252d6ecaadbc28beb0c23))

## [0.17.5](https://github.com/propeller-heads/tycho-indexer/compare/0.17.4...0.17.5) (2024-09-19)


### Bug Fixes

* fetch contracts from deltas buffer if not in db yet ([1321cbd](https://github.com/propeller-heads/tycho-indexer/commit/1321cbd0da60b732043bb570050d59d164a135d0))
* fetch contracts from deltas buffer if not in db yet ([#370](https://github.com/propeller-heads/tycho-indexer/issues/370)) ([b12538d](https://github.com/propeller-heads/tycho-indexer/commit/b12538d62f912641b81ee006662cdbbde6a7a0c3))
* rebase and fix subsequent changes ([6a2b3e3](https://github.com/propeller-heads/tycho-indexer/commit/6a2b3e32718af7f960d3d4388a519cd00c1ddd9e))

## [0.17.4](https://github.com/propeller-heads/tycho-indexer/compare/0.17.3...0.17.4) (2024-09-19)


### Bug Fixes

* adapt tycho-client-py to work with `Bytes` ([562e45a](https://github.com/propeller-heads/tycho-indexer/commit/562e45ac9d9b13aad358a3fc603b23fc9b42dc41))
* release config wrong crate name ([a024672](https://github.com/propeller-heads/tycho-indexer/commit/a024672489f6944349deba0b3a574135ae3223fd))
* release config wrong crate name ([#371](https://github.com/propeller-heads/tycho-indexer/issues/371)) ([452f88a](https://github.com/propeller-heads/tycho-indexer/commit/452f88a4637a0af7a15199bfe6339efc66d29dae))
* rename tycho analyzer in release config ([0a2a168](https://github.com/propeller-heads/tycho-indexer/commit/0a2a1683d5fdb250b0483a4f0427898fa10ce33b))

## [0.17.3](https://github.com/propeller-heads/tycho-indexer/compare/0.17.2...0.17.3) (2024-09-17)


### Bug Fixes

* protocol system delete script ([#365](https://github.com/propeller-heads/tycho-indexer/issues/365)) ([e3c3313](https://github.com/propeller-heads/tycho-indexer/commit/e3c3313afd76387c274fe39209ea6b6c0c978c1c))
* skip deleting accounts also linked to tokens used by other systems ([baefd07](https://github.com/propeller-heads/tycho-indexer/commit/baefd0713aeee1d45fa8edd0ee6c4b0b51c81c18))

## [0.17.2](https://github.com/propeller-heads/tycho-indexer/compare/0.17.1...0.17.2) (2024-09-16)

## [0.17.1](https://github.com/propeller-heads/tycho-indexer/compare/0.17.0...0.17.1) (2024-09-13)

## [0.17.0](https://github.com/propeller-heads/tycho-indexer/compare/0.16.4...0.17.0) (2024-09-11)


### Features

* expose `items()` directly on `TokenBalances` ([e5eb17e](https://github.com/propeller-heads/tycho-indexer/commit/e5eb17ec6b3c704574c07b97562d953942ff286f))

## [0.16.4](https://github.com/propeller-heads/tycho-indexer/compare/0.16.3...0.16.4) (2024-09-06)

## [0.16.3](https://github.com/propeller-heads/tycho-indexer/compare/0.16.2...0.16.3) (2024-09-06)


### Bug Fixes

* **rpc:** fi handling of default version ts ([9d60af2](https://github.com/propeller-heads/tycho-indexer/commit/9d60af2e3902a9817b5ad9cac91567a788ec9e24))
* **rpc:** Fix handling of default version ts ([#352](https://github.com/propeller-heads/tycho-indexer/issues/352)) ([2820a42](https://github.com/propeller-heads/tycho-indexer/commit/2820a42c8dd33f6d4d62816ceafad605ff493f8f))

## [0.16.2](https://github.com/propeller-heads/tycho-indexer/compare/0.16.1...0.16.2) (2024-09-06)


### Bug Fixes

* Improve protocol system deletion script ([#358](https://github.com/propeller-heads/tycho-indexer/issues/358)) ([6f20892](https://github.com/propeller-heads/tycho-indexer/commit/6f2089251a054d563785817adb7c46dbb8e5e82a))
* remove unnecessary queries from deletion script ([9c6f7a4](https://github.com/propeller-heads/tycho-indexer/commit/9c6f7a453879d5add16b5e00a8796fab590f4d95))

## [0.16.1](https://github.com/propeller-heads/tycho-indexer/compare/0.16.0...0.16.1) (2024-09-05)


### Bug Fixes

* Delete protocol system script to delete tokens as necessary ([#356](https://github.com/propeller-heads/tycho-indexer/issues/356)) ([3e1138b](https://github.com/propeller-heads/tycho-indexer/commit/3e1138ba25d5d8b6cb10c7b43c2cf99d0a9ee1df))
* delete token's account entries too ([0862e39](https://github.com/propeller-heads/tycho-indexer/commit/0862e39ca4165c3053f18d0b21a79c85e1789a3e))
* delete tokens that belong solely to the protocol system ([2289173](https://github.com/propeller-heads/tycho-indexer/commit/2289173bbe93f7bc7116647b5089b1f4bf617d24))
* remove unnecessary count check ([9b40290](https://github.com/propeller-heads/tycho-indexer/commit/9b40290f28d6dd3db322d3ce8319c1ed58d3d846))

## [0.16.0](https://github.com/propeller-heads/tycho-indexer/compare/0.15.2...0.16.0) (2024-09-04)


### Features

* Create remove protocol script ([#311](https://github.com/propeller-heads/tycho-indexer/issues/311)) ([b6b818b](https://github.com/propeller-heads/tycho-indexer/commit/b6b818b3809b05f54a2017f241ae45872b688ce4))
* **db:** add cascade deletes to protocol_system related tables ([f8326e2](https://github.com/propeller-heads/tycho-indexer/commit/f8326e27aab5a1b88fd85f4cc3aece5b12ba4271))
* **db:** add script to delete protocol system from db ([07ffa77](https://github.com/propeller-heads/tycho-indexer/commit/07ffa779c4d4f10fdb1625ecaedeaf298d3c8afa))
* skip deleting shared accounts ([37b17d2](https://github.com/propeller-heads/tycho-indexer/commit/37b17d2c19337b4bae2e05cc1e7aefa0d17ed48c))


### Bug Fixes

* delete substreams cursor too ([be9ebfa](https://github.com/propeller-heads/tycho-indexer/commit/be9ebfa5f0fee568abe089af878d276f3a5de542))
* typo in name of sushiswap configs ([3489aab](https://github.com/propeller-heads/tycho-indexer/commit/3489aab1374a8ca925a737c62d2f45da55899005))
* update protocol delete script to be more configurable ([a76e5db](https://github.com/propeller-heads/tycho-indexer/commit/a76e5db51f7b102c7e29a25fbf36af4633e9d68b))

## [0.15.2](https://github.com/propeller-heads/tycho-indexer/compare/0.15.1...0.15.2) (2024-09-04)


### Bug Fixes

* **tycho-client-py:** backward compatibility of `ContractStateParams` ([c5373a1](https://github.com/propeller-heads/tycho-indexer/commit/c5373a1cfb56a7ef8a9421424410e84a74809d46))
* **tycho-client-py:** backward compatibility of `ContractStateParams` ([#354](https://github.com/propeller-heads/tycho-indexer/issues/354)) ([81f0afc](https://github.com/propeller-heads/tycho-indexer/commit/81f0afcc0e898c7d156c53ebc91cffb2fa745290))

## [0.15.1](https://github.com/propeller-heads/tycho-indexer/compare/0.15.0...0.15.1) (2024-09-03)

## [0.15.0](https://github.com/propeller-heads/tycho-indexer/compare/0.14.0...0.15.0) (2024-09-02)


### Features

* Add block_id column to extraction_state table ([78514f5](https://github.com/propeller-heads/tycho-indexer/commit/78514f58607f851f7e29b0f4085f054189f07072))
* Add block_id column to extraction_state table ([#287](https://github.com/propeller-heads/tycho-indexer/issues/287)) ([ea7434a](https://github.com/propeller-heads/tycho-indexer/commit/ea7434a8cdf727654c8c03658f5552a2ac71cd63))
* add block_id to extraction_state db table ([e0c4f35](https://github.com/propeller-heads/tycho-indexer/commit/e0c4f350f1e0334f37f928d5eb09494223283ad1))


### Bug Fixes

* remove Block from get_state return ([563de75](https://github.com/propeller-heads/tycho-indexer/commit/563de758ce44e1a3e5e60cd63586a65e0a73e699))

## [0.14.0](https://github.com/propeller-heads/tycho-indexer/compare/0.13.0...0.14.0) (2024-09-02)


### Features

* Remove chain from contract id param ([#346](https://github.com/propeller-heads/tycho-indexer/issues/346)) ([3bb61a4](https://github.com/propeller-heads/tycho-indexer/commit/3bb61a49f695dad6e010e831a8e38a2e4d8defe9))
* **rpc:** remove chain from contract id param ([8092a1e](https://github.com/propeller-heads/tycho-indexer/commit/8092a1ef6e53edb3262bdd6d307c0efe78844c14))

## [0.13.0](https://github.com/propeller-heads/tycho-indexer/compare/0.12.0...0.13.0) (2024-08-30)


### Features

* add autodeletion to partition tables ([6302ae8](https://github.com/propeller-heads/tycho-indexer/commit/6302ae8c68368ca1af99c1aab939adfd993b24a7))
* Add autodeletion to partition tables ([#347](https://github.com/propeller-heads/tycho-indexer/issues/347)) ([e482522](https://github.com/propeller-heads/tycho-indexer/commit/e482522cda1ca7df8733c6a7bc41f486ca0c403c))

## [0.12.0](https://github.com/propeller-heads/tycho-indexer/compare/0.11.1...0.12.0) (2024-08-29)


### Features

* Move rpc endpoint params to request body ([#344](https://github.com/propeller-heads/tycho-indexer/issues/344)) ([c6ff178](https://github.com/propeller-heads/tycho-indexer/commit/c6ff17817b330957eff8250bc22e9fb6faff9f92))
* move rpc endpoints url params to request body ([4ad2a90](https://github.com/propeller-heads/tycho-indexer/commit/4ad2a908202e1411958770b739c09510a854cffb))
* **tycho-client-py:** update rpc client to use new endpoints ([f934269](https://github.com/propeller-heads/tycho-indexer/commit/f934269727f8521b8113994ef171878ba64de3f4))
* **tycho-client:** update rpc to use new endpoints ([6e79ed1](https://github.com/propeller-heads/tycho-indexer/commit/6e79ed113fb86b8d2064092fdd75e9728dd84fe8))

## [0.11.1](https://github.com/propeller-heads/tycho-indexer/compare/0.11.0...0.11.1) (2024-08-27)


### Bug Fixes

* **dto:** Use capitalize enum values. ([0707bde](https://github.com/propeller-heads/tycho-indexer/commit/0707bde431ea4da6b1ca5e76677e8e958e13abea))
* **dto:** Use capitalize enum values. ([#339](https://github.com/propeller-heads/tycho-indexer/issues/339)) ([1989d0c](https://github.com/propeller-heads/tycho-indexer/commit/1989d0c960b01cd2391e1364dc571922b0728d27))

## [0.11.0](https://github.com/propeller-heads/tycho-indexer/compare/0.10.0...0.11.0) (2024-08-26)


### Features

* add non-SIP protected binary directory option ([b4d3d69](https://github.com/propeller-heads/tycho-indexer/commit/b4d3d694e0c9c6601ab04dcd74da0aa75383f818))


### Bug Fixes

* support range tvl threshold on client stream creation ([fbbd8cf](https://github.com/propeller-heads/tycho-indexer/commit/fbbd8cf9404e74002f87ea80af9bba83d26e1dd4))
* **tycho-client:** remove hardcoded versioning on cli ([5a721f4](https://github.com/propeller-heads/tycho-indexer/commit/5a721f44c423330c18ba17874f2434b956c77a7b))
* update contract request body to include protocol_system ([b2858e9](https://github.com/propeller-heads/tycho-indexer/commit/b2858e9c61f66017fd6feb3f396758216eea94f9))
* Update python client ([#338](https://github.com/propeller-heads/tycho-indexer/issues/338)) ([0b3e59d](https://github.com/propeller-heads/tycho-indexer/commit/0b3e59dfcb967eb65e92fbeeb7cba64b701e5c61))

## [0.10.0](https://github.com/propeller-heads/tycho-indexer/compare/0.9.1...0.10.0) (2024-08-19)


### Features

* **tycho-client:** Add tvl range as a component filter ([6a197b7](https://github.com/propeller-heads/tycho-indexer/commit/6a197b745aaf7219f4b25fb3409dbcca704e70f1))
* **tycho-client:** Add tvl range as a component filter ([#328](https://github.com/propeller-heads/tycho-indexer/issues/328)) ([a33fb5c](https://github.com/propeller-heads/tycho-indexer/commit/a33fb5c518977add7f3ade77125cf408fe930c0f))
* **tycho-client:** update cli to accept min tvl range input ([78873c9](https://github.com/propeller-heads/tycho-indexer/commit/78873c9dedd185a9feae8198b4e3312d74709e82))

## [0.9.1](https://github.com/propeller-heads/tycho-indexer/compare/0.9.0...0.9.1) (2024-08-16)


### Bug Fixes

* deserialise WebSocketMessage workaround ([8021493](https://github.com/propeller-heads/tycho-indexer/commit/80214933c76d228a67ab4420df0642bd2f7821a4))
* improve deserialisation error messages ([d9e56b1](https://github.com/propeller-heads/tycho-indexer/commit/d9e56b1cbef1bb874fa401f1df6d40a10028e690))
* WebSocketMessage deserialisation bug ([#327](https://github.com/propeller-heads/tycho-indexer/issues/327)) ([6dfebb0](https://github.com/propeller-heads/tycho-indexer/commit/6dfebb0e5718979023cb2bb8890566cc740647f1))

## [0.9.0](https://github.com/propeller-heads/tycho-indexer/compare/0.8.3...0.9.0) (2024-08-15)


### Features

* **rpc:** make serde error if unknown field in bodies ([2aaaf0e](https://github.com/propeller-heads/tycho-indexer/commit/2aaaf0edbc814d26a8a89c965c2d3800e82dc0c9))

## [0.8.3](https://github.com/propeller-heads/tycho-indexer/compare/0.8.2...0.8.3) (2024-08-15)


### Bug Fixes

* **client-py:** fix hexbytes decoding and remove camelCase aliases ([4a0432e](https://github.com/propeller-heads/tycho-indexer/commit/4a0432e4446c6b0595168d0c99663f894d490694))
* **client-py:** fix hexbytes encoding and remove camelCase aliases ([#322](https://github.com/propeller-heads/tycho-indexer/issues/322)) ([10272a4](https://github.com/propeller-heads/tycho-indexer/commit/10272a4a2d35ece95713bf983efd5978a7587ca4))

## [0.8.2](https://github.com/propeller-heads/tycho-indexer/compare/0.8.1...0.8.2) (2024-08-14)


### Bug Fixes

* skip buggy clippy warning ([feeb6a1](https://github.com/propeller-heads/tycho-indexer/commit/feeb6a11692d6fabd171cff8cc0bd9be46ad4461))
* specify extractor on rpc requests ([98d57d2](https://github.com/propeller-heads/tycho-indexer/commit/98d57d281c32edcf0790e1d33fadcca0ca13a613))
* Specify extractor on rpc requests ([#323](https://github.com/propeller-heads/tycho-indexer/issues/323)) ([a45df90](https://github.com/propeller-heads/tycho-indexer/commit/a45df90fe5010a965404e368febca4dc414fe0f0))

## [0.8.1](https://github.com/propeller-heads/tycho-indexer/compare/0.8.0...0.8.1) (2024-08-09)


### Bug Fixes

* Hanging client on max connection attempts reached ([#317](https://github.com/propeller-heads/tycho-indexer/issues/317)) ([f9ca57a](https://github.com/propeller-heads/tycho-indexer/commit/f9ca57a1ad9af8af3d5b8e136abc5ea85641ef16))
* hanging client when max connection attempts reached ([feddb47](https://github.com/propeller-heads/tycho-indexer/commit/feddb4725143bde9cb0c99a8b7ca9c4d60ec741f))
* propagate max connection attempts error correctly ([6f7f35f](https://github.com/propeller-heads/tycho-indexer/commit/6f7f35fa9d56a8efe2ed2538b7f02543a5300b4a))
* **tycho-client:** reconnection error handling ([4829f97](https://github.com/propeller-heads/tycho-indexer/commit/4829f976e092da5ef0fdb96e353fb6157557f825))

## [0.8.0](https://github.com/propeller-heads/tycho-indexer/compare/0.7.5...0.8.0) (2024-08-09)


### Features

* change workflow behaviour ([61f7517](https://github.com/propeller-heads/tycho-indexer/commit/61f7517b64cb62468160a88eb485c2a91bceef49))
* change workflow behaviour ([#316](https://github.com/propeller-heads/tycho-indexer/issues/316)) ([3ca195b](https://github.com/propeller-heads/tycho-indexer/commit/3ca195b9f1a7ce76f857e3b7ad76d39d2a374a60))

## [0.7.5](https://github.com/propeller-heads/tycho-indexer/compare/0.7.4...0.7.5) (2024-08-07)


### chore

* black format code ([7dcb55a](https://github.com/propeller-heads/tycho-indexer/commit/7dcb55af3eea7c807e3c9491bd9d0574533ff8df))
* Remove unneeded new method and outdated comment ([d402acb](https://github.com/propeller-heads/tycho-indexer/commit/d402acb6c2e52f537f27b82d8b6dfd8449627a4a))

### fix

* Add missing requests dependency ([d64764c](https://github.com/propeller-heads/tycho-indexer/commit/d64764ca07cadc8f312c6d1c26f00da367d06447))
* Add property aliases to ResponseAccount. ([298c688](https://github.com/propeller-heads/tycho-indexer/commit/298c688fd8acca21da8c3cf45be953fbf1153b8e))

## [0.7.4](https://github.com/propeller-heads/tycho-indexer/compare/0.7.3...0.7.4) (2024-08-07)


### fix

* fix usv2 substreams merge bug ([88ce6c6](https://github.com/propeller-heads/tycho-indexer/commit/88ce6c6f7a440681113e442342e877cb6091656d))

## [0.7.3](https://github.com/propeller-heads/tycho-indexer/compare/0.7.2...0.7.3) (2024-08-06)


### chore

* Add trace logging for tokens queries ([01a5bbc](https://github.com/propeller-heads/tycho-indexer/commit/01a5bbcca61d8dde3620790ab11529e635b07cce))

### fix

* add defaults for initialized_accounts configs ([2becb5e](https://github.com/propeller-heads/tycho-indexer/commit/2becb5ea60a24f51fee9a49ce5b2b1b2edd213f9))
* changed tag format ([764d9e6](https://github.com/propeller-heads/tycho-indexer/commit/764d9e6bb33e623780f58c4be4628ba6985e0d58))
* ci-cd-templates path ([1c21f79](https://github.com/propeller-heads/tycho-indexer/commit/1c21f793bfabfdd233efa1c58af6cf0c686d2a8e))
* clean up defaults and spkg name ([eac825c](https://github.com/propeller-heads/tycho-indexer/commit/eac825c2d5d29586d53ba773c8f3695504a4298b))
* dockerfile restore quotes ([1d73485](https://github.com/propeller-heads/tycho-indexer/commit/1d73485f97b4dbe7dba188b8bd1b772b3107a01d))
* revert sushiswap config change ([b10921e](https://github.com/propeller-heads/tycho-indexer/commit/b10921e7510b229990c767d10795041a138e7a9f))

### update

* Cargo.lock ([9b129ef](https://github.com/propeller-heads/tycho-indexer/commit/9b129efa09bcd1956b56fa4c2ad1724d3a1dda12))
