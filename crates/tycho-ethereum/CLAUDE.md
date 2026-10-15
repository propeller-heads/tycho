# tycho-ethereum

Ethereum-specific implementations of traits defined in `tycho-common`. Consumed by `tycho-indexer`, `tycho-simulation`, and `tycho-test`.

## Module Map

```
rpc/                    Ethereum RPC client (alloy-based) with batching + retry
  ├─ config.rs          RPCRetryConfig, RPCBatchingConfig
  ├─ errors.rs          Error hierarchy: RPCError → RequestError → ReqwestError
  └─ retry.rs           Error classification (retryable vs permanent RPC codes)

erc20.rs                ERC-20 ABI bindings via alloy sol! macro
gas.rs                  BlockGasPrice / GasPrice (Legacy + EIP-1559) data types

services/
  ├─ account_extractor.rs     Fetches code, balance, storage for accounts at a block height
  ├─ token_pre_processor.rs   Fetches symbol + decimals; gracefully handles non-standard tokens
  ├─ token_analyzer/
  │    ├─ mod.rs              Public API; re-exports EthCallDetector (primary) and TraceCallDetector (deprecated)
  │    ├─ ethcall_detector.rs EthCallDetector — simulates token transfers via eth_call with bytecode state overrides; works on all EVM chains
  │    ├─ trace_detector.rs   TraceCallDetector — legacy trace_callMany approach; deprecated, use EthCallDetector
  │    ├─ common.rs           Shared helpers (token quality classification, transfer cost parsing)
  │    └─ bytecode.rs         Compiled bytecode + ABI for Analyzer and Forwarder Solidity contracts
  └─ entrypoint_tracer/
       ├─ tracer.rs                   Traces contract execution; returns access lists + state diffs
       ├─ slot_detector.rs            Detects ERC-20 storage slot layout via trace simulation
       ├─ balance_slot_detector.rs    Locates balance mapping slot
       └─ allowance_slot_detector.rs  Locates allowance mapping slot

test_fixtures.rs        Shared RPC/account fixtures, also consumed by tycho-test
lib.rs                  BytesCodec; SERVICES_SOURCE_DIGEST (see below)
build.rs                Computes SERVICES_SOURCE_DIGEST
build/source_digest.rs  Computes the source digest for build.rs; tested in this crate
```

## `SERVICES_SOURCE_DIGEST`

`build.rs` digests (Keccak-256) every file under `src/services/`, `src/rpc/`, `src/erc20.rs` and
`src/lib.rs` (`BytesCodec`, used by every hashed service), and `lib.rs` exposes the hex digest as
`SERVICES_SOURCE_DIGEST`. The digest is for the indexer's extractor fingerprints: its
recorded-extractor tests mock these services, so its delta format files cannot show a change to
them.

`build/source_digest.rs` computes it. It hashes each file's crate-relative path and contents, in
sorted path order, skipping hidden files and editor backups (`.*`, `*~`):

- a `.rs` file is parsed with `syn`, and the digest hashes its token stream's string form without
  `#[cfg(test)]` items (in modules, impl and trait blocks and function bodies), functions with a
  `test` attribute (`#[test]`, `#[tokio::test]`, ...) and doc attributes (`///`, `//!`,
  `#[doc = ...]`, also inside macro invocations). Comments, doc comments, formatting and inline
  test code therefore leave it unchanged; a file that does not parse fails the build;
- any other file is hashed as it is, with CRLF normalized to LF when it is UTF-8.

Upgrading `syn`, `quote` or `proc-macro2` can change the token stream's string form, and with it
every digest, once. The script reads only this crate's files, so it also runs from the published
package. When the services come to depend on a file outside those paths, add it to
`HASHED_PATHS` in `build.rs`.

## Module Dependencies

All services depend on `rpc/` for RPC calls and on `erc20.rs` for ABI encoding. The slot detectors are consumed by the indexer's `dynamic_contract_indexer/hooks/`, not by `account_extractor`.

```
tycho-common traits
        ↑
  services/* ──── rpc/ ──── alloy
        |
      erc20.rs
```

## Trait Implementations

| Module | Implements (tycho-common) |
|---|---|
| `account_extractor` | `AccountExtractor` |
| `token_pre_processor` | `TokenPreProcessor` |
| `token_analyzer` | `TokenAnalyzer` |
| `entrypoint_tracer` | `EntryPointTracer` |
| `entrypoint_tracer/balance_slot_detector` | `BalanceSlotDetector` (`EVMBalanceSlotDetector`) |
| `entrypoint_tracer/allowance_slot_detector` | `AllowanceSlotDetector` (`EVMAllowanceSlotDetector`) |
| `rpc` | `FeePriceGetter` (returns `BlockGasPrice` from `gas.rs`) |
