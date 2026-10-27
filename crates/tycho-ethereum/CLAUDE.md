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
  │    ├─ ethcall_detector.rs EthCallDetector — simulates token transfers via eth_call with bytecode state overrides; works on all EVM chains (see Token analysis)
  │    ├─ onchain_owners.rs   OnChainTokenOwners — TokenOwnerFinding that picks each pool's holder among its candidate addresses with balanceOf at the analysis block
  │    ├─ trace_detector.rs   TraceCallDetector — legacy trace_callMany approach; deprecated, use EthCallDetector
  │    ├─ common.rs           Shared helpers (token quality classification, transfer cost parsing)
  │    └─ bytecode.rs         Compiled bytecode + ABI for Analyzer and Forwarder Solidity contracts
  └─ entrypoint_tracer/
       ├─ tracer.rs                   Traces contract execution; returns access lists + state diffs
       ├─ slot_detector.rs            Detects ERC-20 storage slot layout via trace simulation
       ├─ balance_slot_detector.rs    Locates balance mapping slot
       └─ allowance_slot_detector.rs  Locates allowance mapping slot

test_fixtures.rs        Shared RPC/account fixtures, also consumed by tycho-test
```

## Token analysis

`EthCallDetector` runs one `eth_call` per token, two when the first holder cannot move the token
(see below). It injects the Analyzer (`contracts/Analyzer.sol`) at
the holder from `TokenOwnerFinding::find_owner` and the Forwarder at the settlement contract. The
call moves tokens holder → settlement → a fresh recipient. The settlement keeps half of what it
received and sells equal slices of it into up to 8 candidate pools: the holder, then
`TokenOwnerFinding::find_pools`, largest balance first. A pool that receives less than its slice
shows a tax that only applies to transfers into that pool (sell taxes, pair-only taxes). The tax is
the highest fee of all transfers; any fee makes the verdict Bad. A sell leg that reverts or uses up
its 2M gas gives no information and is skipped. A sell leg shortfall of up to 2 wei is rounding,
not a fee, and so is a settlement balance up to 2 wei off once there are sell legs (share tokens
round the split). The holder is often the token's own pair; a token that calls its pair during a
transfer then fails, because the pair runs the Analyzer code. On such a transfer failure the
detector runs once more with the next pool (at least `MIN_AMOUNT`) as holder. Callers fill the
candidate pools from the new pools of a block (indexer) or from every `get_token_owners` row
(analysis cron). The cron passes each pool's candidate addresses to `OnChainTokenOwners`, which takes
the candidate with the largest `balanceOf` as the pool's address and its on-chain balance.

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
