# tycho-common

Shared domain types, storage/extraction traits, and simulation abstractions used across all crates.

## Module Organisation

### Primitives
- **`hex_bytes`** — `Bytes` newtype with hex serde and Diesel support; used everywhere as the byte representation
- **`serde_primitives`** — Hex serde helpers for maps and vecs; used by `dto` and `models`
- **`display`** — `DisplayOption` wrapper for tracing logs

### Domain Models (`models/`)
- **`mod`** — Type aliases (`Address`, `TxHash`, …) and the `Chain` enum; imported by every other module. `Chain` is `#[non_exhaustive]`: ten built-in variants plus `Custom(CustomChainId)`. `FromStr` is registry-backed and **fallible** — it resolves custom names against `chain_config`; use `Chain::builtin_from_str` to parse only built-ins
- **`chain_config`** — Custom-chain registry: `ChainConfigRegistry` (`load_default` / `from_yaml_file`) holds `CustomChainConfig` entries (quote tokens, `TvlThresholds` / `TvlThresholdTier`) keyed by `CustomChainId`. `chain_registry()` reads the process-global instance, `init_chain_registry()` installs it. **It must be installed before any chain name is parsed.** This is the mechanism for adding a chain without a code change; see `chains.yaml` / `TYCHO_CHAINS_CONFIG`
- **`blockchain`** — `Block`, `Transaction`, `BlockChanges`, `TxWithChanges`, `BlockAggregatedChanges` (the output type of the indexing pipeline), plus the pending-block input types `PendingBlock` / `TxInput` / `LogInput`
- **`contract`** — `Account` / `AccountDelta`; versioned EVM contract state written by tycho-ethereum and persisted by tycho-storage
- **`protocol`** — `ProtocolComponent` / `ProtocolComponentStateDelta`; DEX/lending pool state alongside `ComponentBalance`
- **`token`** — `Token` metadata and quality scoring; populated by tycho-ethereum's `TokenAnalyzer`
- **`error`** — `WebsocketError` for streaming subscription failures

### API Layer
- **`dto`** — JSON-serialisable mirrors of `models/` types for HTTP/WebSocket responses; used by tycho-indexer (server) and tycho-client (consumer)

### Trait Abstractions
- **`storage`** — Async gateway traits that tycho-storage implements over Diesel/Postgres: `ChainGateway`, `ExtractionStateGateway`, `ProtocolGateway`, `EntryPointGateway`, `ContractStateGateway`, and the `Gateway` umbrella trait combining them
- **`traits`** — Extraction traits: `AccountExtractor`, `TokenAnalyzer`, `TokenOwnerFinding`, `TokenPreProcessor`, `EntryPointTracer`, `BalanceSlotDetector`, `AllowanceSlotDetector`, `FeePriceGetter` (all async, implemented by tycho-ethereum), plus `TxDeltaIndexer` (sync `apply_block` / `generate_deltas`, implemented in tycho-simulation's `evm/pending.rs` for pending-block simulation)

### Simulation (`simulation/`)
- **`protocol_sim`** — `ProtocolSim` core trait (quote, price, state transition); implemented by protocol-specific simulators; To be replaced by SwapQuoter trait in the future.
- **`swap`** — `SwapQuoter` trait and `params_with_context!` macro for quoting with block context
- **`indicatively_priced`** — `IndicativelyPriced` extension trait for RFQ/off-chain-signed quotes
- **`errors`** — `SimulationError` (Fatal / InvalidInput / Recoverable) and `TransitionError`

## Client Sync

Changes to `dto.rs` or the `Chain` enum require updates to both `tycho-client/` (Rust) and
`tycho-client-py/` (Python). See `.claude/knowledge/python.md` for the Python sync checklist.

## Data Flow

```
tycho-ethereum (implements traits)
    → fills models/
    → tycho-storage (implements storage traits) persists models/
    → dto serialises for HTTP/WebSocket
    → tycho-client deserialises dto
    → simulation/ consumed by solvers via tycho-client
```
