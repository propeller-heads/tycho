# tycho-ethereum

Ethereum-specific implementations of traits defined in `tycho-common`. Consumed exclusively by `tycho-indexer`.

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
```

## Module Dependencies

All services depend on `rpc/` for RPC calls and on `erc20.rs` for ABI encoding. The entrypoint
tracer's slot detectors feed the hooks DCI with balance/allowance slot layouts for traced entry
points; `account_extractor` independently fetches requested account state.

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
| `rpc` | `FeePriceGetter` (returns `BlockGasPrice` from `gas.rs`) |
