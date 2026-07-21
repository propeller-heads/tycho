# tycho-common

Shared domain types, storage/extraction traits, and simulation abstractions used across all crates.

## Module Organisation

### Primitives
- **`hex_bytes`** — `Bytes` newtype with hex serde and Diesel support; used everywhere as the byte representation
- **`serde_primitives`** — Hex serde helpers for maps and vecs; used by `dto` and `models`
- **`display`** — `DisplayOption` wrapper for tracing logs

### Domain Models (`models/`)
- **`mod`** — Type aliases (`Address`, `TxHash`, …) and the non-exhaustive `Chain` enum, including registry-backed `Chain::Custom`; imported by every other module
- **`chain_config`** — `ChainConfigRegistry` and custom-chain YAML loading. The process-wide registry lazily reads `TYCHO_CHAINS_CONFIG`, or callers can install one explicitly before first access
- **`blockchain`** — `Block`, `Transaction`, `BlockAggregatedChanges`; the output type of the indexing pipeline
- **`contract`** — `Account` / `AccountDelta`; versioned EVM contract state written by tycho-ethereum and persisted by tycho-storage
- **`protocol`** — `ProtocolComponent` / `ProtocolComponentStateDelta`; DEX/lending pool state alongside `ComponentBalance`
- **`token`** — `Token` metadata and quality scoring; populated by tycho-ethereum's `TokenAnalyzer`
- **`error`** — `WebsocketError` for streaming subscription failures

### API Layer
- **`dto`** — JSON-serialisable mirrors of `models/` types for HTTP/WebSocket responses; used by tycho-indexer (server) and tycho-client (consumer)

### Trait Abstractions
- **`storage`** — Async gateway traits (`ProtocolGateway`, `ContractStateGateway`, …) that tycho-storage implements over Diesel/Postgres
- **`traits`** — Async extraction traits (`AccountExtractor`, `TokenAnalyzer`, `EntryPointTracer`, …) that tycho-ethereum implements

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
