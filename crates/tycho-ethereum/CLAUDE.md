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
  │    ├─ mod.rs              Public API: SimulationDetector, and ensure_rpc_supports_simulation for startup checks
  │    ├─ onchain_owners.rs   OnChainTokenOwners — TokenOwnerFinding that reads all candidate holders' balanceOf in one batch at the analysis block and picks the largest
  │    ├─ simulation/         SimulationDetector — buy, forward and sell against the token's pool via eth_simulateV1; verdict, per-leg fees, flags, control slots
  │    │    ├─ scenario.rs    One scenario's legs in eth_simulateV1 (buy, then the sell a block later); direct legs with Runner at the trader
  │    │    ├─ swap.rs        V2 pair detection and the swap legs (Runner at a router) in a realistic or bare environment
  │    │    ├─ trace.rs       debug_traceCall of the legs: external calls (delegate calls too, except a proxy's forward), storage slots read with their values
  │    │    ├─ code.rs        Bytecode summary via evmole; EIP-1967/1167 proxy resolution; owner()
  │    │    ├─ owner_calls.rs     Owner calls that may lift a gate or change a fee, discovered from bytecode; the caller of each (owner, stored admin, or a role holder)
  │    │    ├─ pool_funding.rs    Writes a balance (and V2 reserves) for a pool holding too little of the token; calibrated for reflection tokens
  │    │    ├─ pool_seizure.rs    Privileged calls that may take tokens out of the pool (forced transfers, burns)
  │    │    ├─ fee_fields.rs      For a token charging no fee: new values in the privileged fields its transfer reads, written as state overrides, to find a fee that can start
  │    │    ├─ verdict.rs     Outcomes → TokenAnalysis (honeypot and external-call rules, flags, slot roles)
  │    │    └─ runner.rs      Compiled bytecode + ABI of contracts/Runner.sol
  │    └─ common.rs           Shared helpers (block tag mapping, call requests)
  └─ entrypoint_tracer/
       ├─ tracer.rs                   Traces contract execution; returns access lists + state diffs
       ├─ slot_detector.rs            Detects ERC-20 storage slot layout via trace simulation
       ├─ balance_slot_detector.rs    Locates balance mapping slot
       └─ allowance_slot_detector.rs  Locates allowance mapping slot

test_fixtures.rs        Shared RPC/account fixtures, also consumed by tycho-test
```

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
| `token_analyzer` | `TokenAnalyzer`, `TokenOwnerFinding` (`OnChainTokenOwners`) |
| `entrypoint_tracer` | `EntryPointTracer` |
| `entrypoint_tracer/balance_slot_detector` | `BalanceSlotDetector` (`EVMBalanceSlotDetector`) |
| `entrypoint_tracer/allowance_slot_detector` | `AllowanceSlotDetector` (`EVMAllowanceSlotDetector`) |
| `rpc` | `FeePriceGetter` (returns `BlockGasPrice` from `gas.rs`) |
