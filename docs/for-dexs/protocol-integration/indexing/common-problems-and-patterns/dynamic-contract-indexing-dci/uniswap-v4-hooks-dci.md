---
description: Complete Indexing Solution for All Uniswap V4 Hooks
---

# Uniswap V4 Hooks DCI

***

## Introduction

#### What is the Hooks DCI? <a href="#what-is-the-hooks-dci" id="what-is-the-hooks-dci"></a>

The **Hooks DCI (Dynamic Contract Indexer)** is Tycho's specialized indexing plugin for **all Uniswap V4 hooks**. It extends the standard DCI with capabilities designed specifically for hooks, including automatic balance tracking, sophisticated entrypoint generation, and optional external metadata collection.

The Hooks DCI is required for indexing **all** Uniswap V4 hooks. It provides a complete solution with sensible defaults that work out-of-the-box for most hooks, and optional extension points for hooks with advanced requirements.

In this document, we break down UniswapV4 pools into different categories, describe the challenges to index each one, and provide a guide on how to index pools that need custom integration to be indexed by Tycho.

#### Hook Types: <a href="#hook-types-internal-vs-external-liquidity" id="hook-types-internal-vs-external-liquidity"></a>

Before diving into the solution, we need to understand the different categories that differentiate hooks Indexing:

**1. Composable vs Non-Composable**

* **Composable Hooks**: Work with empty `hookData` in swaps
* **Non-Composable Hooks**: Require custom `hookData` for before or after swap hooks

**2. Internal vs External Liquidity**

* **Internal Liquidity**: Tokens accounting in PoolManager as ERC6909 claims
* **External Liquidity**: Tokens in external contracts, outside UniswapV4's Pool Manager.

We define deeper these categories further on the [#hook-classification](uniswap-v4-hooks-dci.md#hook-classification "mention") section

{% hint style="warning" %}
Currently, Tycho only supports Composable Hooks. Non-composable support is coming soon.
{% endhint %}

**Why Hooks DCI Exists**

The standard DCI works well for self-contained protocols, but Uniswap V4 hooks require some extra steps for correct indexing. For Tycho to index all the state necessary for simulating each hook, it needs to have well-defined Entrypoints that cover all the possible Hook execution paths. This was achieved by adding:

* **V4-specific entrypoint generation** with custom swap encoding and state overrides, aiming to cover all the paths that a hook might take
* **Flexible metadata collection** supporting both internal (automatic) and external (custom) liquidity sources
* **Registry-based extension system** for hooks with specialized requirements
* **State-aware processing** to optimize performance and handle failures gracefully

On [#background--concepts](uniswap-v4-hooks-dci.md#background--concepts "mention") section below, we provide detailed explanations of hook types and architecture.

### Background & Concepts <a href="#background--concepts" id="background--concepts"></a>

#### Uniswap V4 Hooks Primer <a href="#uniswap-v4-hooks-primer" id="uniswap-v4-hooks-primer"></a>

Uniswap V4 introduces **hooks** - smart contracts that can execute custom logic at specific points in the pool lifecycle. Hooks enable powerful features like:

* Dynamic fees based on market conditions
* Custom oracle integrations
* Liquidity management strategies
* Integration with external DeFi protocols

Each hook address encodes **permissions** in its bytes, indicating which lifecycle events it handles:

```
Bit 7: beforeSwap
Bit 6: afterSwap
Bit 5: beforeAddLiquidity
Bit 4: afterAddLiquidity
...
```

The Hooks DCI only processes hooks with **swap permissions** (beforeSwap and/or afterSwap), as these are the ones that manage liquidity and affect swap behavior.

#### Hook Classification <a href="#hook-classification" id="hook-classification"></a>

Understanding hook types helps determine what (if anything) you need to implement for your hook.

**1. Composable vs Non-Composable Hooks**

**1.1 - Composable Hooks (Currently Supported)**

Composable hooks do NOT require custom calldata (`hookData`) to be passed during swaps. They work with empty or default `hookData`.

```solidity
// Composable hook - works with empty hookData
function beforeSwap(
    address sender,
    PoolKey calldata key,
    IPoolManager.SwapParams calldata params,
    bytes calldata hookData  // Can be empty: 0x
) external returns (bytes4, BeforeSwapDelta, uint24);
```

**Examples**:

* Dynamic fee hooks (calculate fees from pool state)
* Oracle integration hooks (read from external oracles, no user input needed)
* Internal liquidity management hooks
* <a href="https://docs.euler.finance/" target="_blank" rel="noopener noreferrer">Eulerswap's</a> external liquidity hooks

**1.2 - Non-Composable Hooks (Future Support)**

{% hint style="warning" %}
⚠️ **Not Currently Supported**: Non-composable hooks REQUIRE specific calldata to be passed in `hookData` for each swap. Support for these hooks is planned for a future release.
{% endhint %}

```solidity
// Non-composable hook - requires meaningful hookData
function beforeSwap(
    address sender,
    PoolKey calldata key,
    IPoolManager.SwapParams calldata params,
    bytes calldata hookData  // MUST contain routing info, signatures, etc.
) external returns (bytes4, BeforeSwapDelta, uint24) {
    // Decode hookData for routing decisions, user signatures, etc.
    (address router, bytes memory signature) = abi.decode(hookData, (address, bytes));
    // ...
}
```

**Examples** (not yet supported):

* Hooks requiring user signatures per swap
* Intent-based routing hooks
* Hooks with swap-specific configuration

**2. Internal vs External Liquidity (Primary Classification)**

This is the **key distinction** that determines what you, as a hook integrator, need to implement.

**2.1 - Internal Liquidity Hooks**

✅ **No Custom Implementation Required -** Composable Internal liquidity hooks are automatically indexed by Tycho.

```
┌─────────────────────────┐
│  Uniswap V4 Hook        │
│  (Logic & Coordination) │
└──────────┬──────────────┘
           │ Uses internal accounting
           ↓
┌─────────────────────────┐
│  PoolManager            │
│  - ERC6909 claims       │
│  - token0 balance: 1000 │
│  - token1 balance: 2000 │
└─────────────────────────┘
```

**Characteristics**:

* All liquidity tracked in PoolManager as <a href="https://docs.uniswap.org/contracts/v4/concepts/erc6909" target="_blank" rel="noopener noreferrer">ERC6909 claims</a>
* Balances automatically extracted from blockchain state
* No external calls needed for Metadata (Pool balances and Limits)
* Works with default orchestrator out-of-the-box

**How It Works**:

1. Hooks DCI extracts pool balances from `BlockChanges.balance_changes`
2. Default orchestrator's `enrich_metadata_from_block_balances()` builds metadata from the pool internal balance
3. Entrypoint generator creates state overrides for PoolManager ERC6909 only
4. Everything works automatically - **no custom code needed**

**Examples**:

* Dynamic fee hooks using PoolManager liquidity
* Hooks with custom AMM curves but standard storage
* Time-weighted average price (TWAP) hooks
* Most hooks that don't integrate with external DeFi

**2.2 - External Liquidity Hooks**

⚙️ **Requires Custom Metadata Implementation**

```
┌──────────────────┐
│  Uniswap V4 Hook │
│  (Coordination)  │
└────────┬─────────┘
         │ Deposits/withdraws
         ↓
┌──────────────────┐
│  External Vault  │
│  - token0: 1000  │
│  - token1: 2000  │
│  - Earning yield │
└──────────────────┘
```

**Characteristics**:

* Liquidity stored in external contracts (lending vaults, yield protocols, etc.)
* Requires custom RPC or API calls to fetch current balances and withdrawal limits
* **Needs custom `MetadataRequestGenerator` and `MetadataResponseParser`**
* May need balance slot detection for accurate simulations

**What You Need to Implement**:

1. `MetadataRequestGenerator` - Creates RPC requests for balances/limits
2. `MetadataResponseParser` - Parses RPC responses into structured data
3. (Optional) Custom `HookOrchestrator` - Only if entrypoint encoding is non-standard

On [#metadata-collection-system](uniswap-v4-hooks-dci.md#metadata-collection-system "mention")we go deeper on the Metadata collection and how you can implement to track any hook with External Liquidity. We also provide an [hook-integration-guide.md](hook-integration-guide.md "mention")to guide you through the implementation steps.

**Examples**:

* **Euler Hooks**: Tokens in Euler lending vaults
* **Yearn Integration**: Tokens in Yearn vaults earning yield
* **Staking Hooks**: Tokens locked in staking contracts

#### **What You Need to Implement (Decision Tree)**

```
START: I have a Uniswap V4 hook to index

├─ Q1: Does my hook require custom calldata (hookData) in swaps?
│   ├─ YES → ⚠️ NOT CURRENTLY SUPPORTED
│   │         Non-composable hooks will be supported in future release
│   └─ NO  → Continue to Q2 (Composable hook ✓)
│
├─ Q2: Does my hook store liquidity in external contracts?
│   ├─ YES → Implement MetadataRequestGenerator + Parser
│   │         (See Integration Guide)
│   └─ NO  → Skip to Q3 (Internal liquidity - auto-handled ✓)
│
├─ Q3: Does my hook need non-standard entrypoint encoding?
│   ├─ YES → Implement custom HookOrchestrator
│   │         (Rare - See Integration Guide)
│   └─ NO  → Use default orchestrator ✓
│
└─ RESULT: Register and initialize Hooks DCI
```

### Architecture Overview <a href="#architecture-overview" id="architecture-overview"></a>

**High-Level System Diagram**

```
┌───────────────────────────────────────────────────────────────┐
│                    UniswapV4HookDCI                           │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Inner DCI (Standard Dynamic Contract Indexer)           │ │
│  │  - Component tracing                                     │ │
│  │  - Storage operations                                    │ │
│  │  - Pruning logic                                         │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Metadata Orchestrator                                   │ │
│  │  - Request generation (Generator Registry)               │ │
│  │  - Request execution (Provider Registry)                 │ │
│  │  - Response parsing (Parser Registry)                    │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │  Hook Orchestrator Registry                              │ │
│  │  - Entrypoint generation                                 │ │
│  │  - Balance/limit injection                               │ │
│  │  - Component updates                                     │ │
│  └──────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────┘
```

#### **Core Components**

**1. UniswapV4HookDCI**

The main orchestrator that coordinates all hook indexing operations. It:

* Filters components with swap hook permissions
* Categorizes components by processing state
* Coordinates metadata collection
* Manages component lifecycle (success, failure, retry, pause)
* Delegates to inner DCI for tracing operations

**2. Metadata Orchestrator System**

**Purpose**: Collects external metadata for hooks with external liquidity. \
**Optional** - only used when a metadata generator is registered for a hook.

For increased performance, the external data collection is split into a three-layer architecture:

**Layer 1: Request Generation** (Protocol-Specific - Optional)

* Creates `MetadataRequest` objects specifying what data to fetch
* Supports different request types: Balances, Limits, TVL
* **Not needed for internal liquidity hooks** - system uses block balances instead

**Layer 2: Request Execution** (Transport-Specific)

* Implemented by providers (e.g., `RPCMetadataProvider`)
* Handles batching, deduplication, retries
* Routes requests to appropriate backends (RPC, HTTP APIs)

**Layer 3: Response Parsing** (Protocol-Specific - Optional)

* Converts raw responses into structured metadata
* Handles errors and validation

**Fallback for Internal Liquidity**: When no metadata generator is registered, the default orchestrator automatically enriches metadata from `BlockChanges.balance_changes` - no RPC calls needed.

**3. Hook Orchestrator Registry**

Maps hook addresses/identifiers to orchestrators that handle component processing. The **default orchestrator** (`DefaultUniswapV4HookOrchestrator`) handles both internal and external liquidity hooks automatically.

**Lookup Priority**:

1. **By Hook Address**: Direct mapping for specific hook deployments
2. **By Identifier**: String-based lookup (e.g., "euler\_v1")
3. **Default Orchestrator**: Fallback for all hooks

**Orchestrator Responsibilities**:

* Generating entrypoints with appropriate tracing parameters
* Injecting balances and limits into components
* Updating component state attributes

**Key Feature**: The default orchestrator's `enrich_metadata_from_block_balances()` method automatically extracts balances from blockchain state for hooks without custom metadata generators. This means **internal liquidity hooks work with zero custom code**.

**Internal vs External Liquidity Paths**

The system automatically chooses the appropriate path based on whether a metadata generator is registered:

**Path A: Internal Liquidity (Automatic)**

```
Block Arrives
     ↓
Extract Swap Hook Components
     ↓
Categorize: Full processing or balance-only
     ↓
┌─────────────────────────────────────────────────────┐
│ Metadata Collection (Step 3)                        │
│                                                      │
│ generator_registry.get_generator() → None           │
│                                                      │
│ ✓ Skip external RPC calls                           │
│ ✓ Balances will be enriched from block changes      │
└─────────────────────────────────────────────────────┘
     ↓
Process Each Component via Orchestrator
     ↓
┌─────────────────────────────────────────────────────┐
│ Default Orchestrator Processing                     │
│                                                      │
│ 1. enrich_metadata_from_block_balances()            │
│    - Extracts balances from BlockChanges            │
│    - No RPC calls needed                            │
│                                                      │
│ 2. generate_entrypoints()                           │
│    - Uses enriched balances                         │
│    - ERC6909 state overrides for PoolManager        │
│                                                      │
│ 3. Inject balances/limits into block_changes        │
└─────────────────────────────────────────────────────┘
     ↓
Delegate to Inner DCI → Store Results
```

**Path B: External Liquidity (Custom Metadata)**

```
Block Arrives
     ↓
Extract Swap Hook Components
     ↓
Categorize: Full processing or balance-only
     ↓
┌─────────────────────────────────────────────────────┐
│ Metadata Collection (Step 3)                        │
│                                                     │
│ generator_registry.get_generator() → Some(generator)│
│                                                     │
│ 1. Custom Generator creates metadata requests       │
│    - getReserves(), getLimits(), etc.               │
│                                                     │
│ 2. Provider executes batched requests               │
│    - Handles retries, rate limits                   │
│                                                     │
│ 3. Custom Parser converts responses                 │
│    - Extracts balances, limits, TVL                 │
└─────────────────────────────────────────────────────┘
     ↓
Process Each Component via Orchestrator
     ↓
┌─────────────────────────────────────────────────────┐
│ Default Orchestrator Processing                     │
│                                                     │
│ 1. Uses external metadata (already collected)       │
│    - Balances from RPC responses                    │
│    - Limits from external protocol                  │
│                                                     │
│ 2. generate_entrypoints()                           │
│    - Uses external balances + limits                │
│    - ERC6909 overrides for PoolManager              │
│    - Optional: ERC20 overrides for external tokens  │
│                                                     │
│ 3. Inject balances/limits into block_changes        │
└─────────────────────────────────────────────────────┘
     ↓
Delegate to Inner DCI → Store Results
```

**Key Takeaway**: The only difference is Step 3 (Metadata Collection). The rest of the flow is identical. This is why internal liquidity hooks require no custom implementation - they automatically use Path A.

#### **Metadata Collection System**

{% hint style="info" %}
💡 **For Internal Liquidity Hooks**: You can **skip this entire section**! The default orchestrator automatically extracts balances from blockchain state using `enrich_metadata_from_block_balances()`. This section is only relevant for hooks with external liquidity.
{% endhint %}

The metadata collection system uses a **three-layer architecture** that separates protocol-specific logic from transport concerns. This system is **optional** and only activated when you register a custom metadata generator for your hook.

**Two Paths for Metadata Collection**

**Path A: Internal Liquidity (Automatic - No Implementation Needed)**

* System checks: `generator_registry.get_generator(component)` → `None`
* Default orchestrator calls `enrich_metadata_from_block_balances()`
* Balances extracted from `BlockChanges.balance_changes`
* Zero RPC calls, zero custom code required

**Path B: External Liquidity (Requires Implementation)**

* System checks: `generator_registry.get_generator(component)` → `Some(generator)`
* Generator creates RPC requests for external data
* Provider executes requests
* Parser converts responses to structured metadata
* Requires implementing Generator + Parser traits

**Layer 1: Request Generation (Protocol-Specific - External Liquidity Only)**

**Purpose**: Create metadata requests specific to your hook's data needs.

**Interface**:

```rust
pub trait MetadataRequestGenerator: Send + Sync {
    fn generate_requests(
        &self,
        component: &ProtocolComponent,
        block: &Block,
    ) -> Result<Vec<MetadataRequest>, MetadataError>;

    fn generate_balance_only_requests(
        &self,
        component: &ProtocolComponent,
        block: &Block,
    ) -> Result<Vec<MetadataRequest>, MetadataError>;

    fn supported_metadata_types(&self) -> Vec<MetadataRequestType>;
}
```

**Metadata Request Types**:

* `ComponentBalance`: Fetch token balances for the component
* `Limits`: Fetch maximum swap amounts (withdrawal limits, liquidity caps)
* `Tvl`: Total value locked calculation
* `Custom`: Extensible for hook-specific needs

**Euler Example - Balance Request**:

```rust
// Generate request to call getReserves() on the hook contract
let balance_request = MetadataRequest {
    request_type: MetadataRequestType::ComponentBalance {
        token_addresses: component.tokens.clone(),
    },
    routing_key: "rpc_default".to_string(),
    generator_name: "euler".to_string(),
    transport: RpcTransport::new(
        rpc_url.clone(),
        "eth_call".to_string(),
        vec![
            json!({
                "to": hook_address,
                "data": "0x0902f1ac" // getReserves() selector
            }),
            json!(format!("0x{:x}", block.number)),
        ],
    ),
};
```

**Euler Example - Limits Request with State Overrides**:

```rust
// Deploy lens contract at deterministic address to query limits
let lens_address = "0x0000000000000000000000000000000000001337";
let limits_request = MetadataRequest {
    request_type: MetadataRequestType::Limits {
        token_pair: vec![token0, token1],
    },
    routing_key: "rpc_default".to_string(),
    generator_name: "euler".to_string(),
    transport: RpcTransport::new(
        rpc_url.clone(),
        "eth_call".to_string(),
        vec![
            json!({
                "to": lens_address,
                "data": format!("0xaaed87a3{token0}{token1}") // getLimits(token0, token1)
            }),
            json!(format!("0x{:x}", block.number)),
            json!({  // State overrides
                lens_address: {
                    "code": "0x608060...",  // Lens contract bytecode
                    "state": {
                        "0x00...00": format!("0x{hook_address}") // Hook addr in slot 0
                    }
                }
            }),
        ],
    ),
};
```

The lens contract pattern allows querying multiple values in a single RPC call using a custom contract deployed via state overrides.

**Layer 2: Request Execution (Transport-Specific)**

**Purpose**: Execute metadata requests efficiently, handling batching and retries.

**Interface**:

```rust
#[async_trait]
pub trait RequestProvider: Send + Sync {
    async fn execute(
        &self,
        requests: Vec<MetadataRequest>,
    ) -> Vec<MetadataResponse>;
}
```

**RPCMetadataProvider Features**:

* **Batching**: Groups multiple `eth_call` requests into JSON-RPC batches
* **Deduplication**: Avoids duplicate requests in the same batch
* **Retry Logic**: Exponential backoff for transient RPC failures
* **Concurrency Limiting**: Prevents overwhelming RPC endpoints

**Configuration**:

```rust
let retry_config = RPCRetryConfig {
    max_retries: 5,
    initial_backoff_ms: 150,
    max_backoff_ms: 5000,
};

let provider = RPCMetadataProvider::new_with_retry_config(
    50,  // Max batch size
    retry_config,
);
```

**Request Flow**:

```
Multiple MetadataRequests
        ↓
┌─────────────────────────┐
│ Group by routing_key    │
└───────────┬─────────────┘
            ↓
┌─────────────────────────┐
│ Batch RPC calls         │
│ (up to batch_size)      │
└───────────┬─────────────┘
            ↓
┌─────────────────────────┐
│ Execute with retries    │
│ (exponential backoff)   │
└───────────┬─────────────┘
            ↓
Multiple MetadataResponses
```

**Layer 3: Response Parsing (Protocol-Specific)**

**Purpose**: Convert raw RPC responses into structured metadata.

**Interface**:

```rust
pub trait MetadataResponseParser: Send + Sync {
    fn parse_response(
        &self,
        component: &ProtocolComponent,
        request: &MetadataRequest,
        response: &Value,
    ) -> Result<MetadataValue, MetadataError>;
}
```

**Metadata Value Types**:

```rust
pub enum MetadataValue {
    Balances(HashMap<Address, Bytes>),
    Limits(Vec<((Address, Address), (Bytes, Bytes, Option<EntryPointWithTracingParams>))>),
    Tvl(f64),
    Custom(serde_json::Value),
}
```

**Euler Example - Balance Parsing**:

```rust
// Parse getReserves() response: two 32-byte balance values
fn parse_balance_response(
    &self,
    component: &ProtocolComponent,
    response: &Value,
) -> Result<MetadataValue, MetadataError> {
    let hex_str = response.as_str()
        .ok_or(MetadataError::InvalidResponse)?
        .trim_start_matches("0x");

    // Ensure we have tokens sorted
    let mut tokens = component.tokens.clone();
    tokens.sort();

    // Extract balances (64 hex chars = 32 bytes each)
    let balance_0 = Bytes::from(&hex_str[0..64]);
    let balance_1 = Bytes::from(&hex_str[64..128]);

    let mut balances = HashMap::new();
    balances.insert(tokens[0].clone(), balance_0);
    balances.insert(tokens[1].clone(), balance_1);

    Ok(MetadataValue::Balances(balances))
}
```

**Euler Example - Limits Parsing**:

```rust
// Parse getLimits() response: two 32-byte limit values
fn parse_limits_response(
    &self,
    component: &ProtocolComponent,
    request: &MetadataRequest,
    response: &Value,
) -> Result<MetadataValue, MetadataError> {
    let hex_str = response.as_str()
        .ok_or(MetadataError::InvalidResponse)?
        .trim_start_matches("0x");

    let limit_0 = Bytes::from(&hex_str[0..64]);
    let limit_1 = Bytes::from(&hex_str[64..128]);

    // Extract token pair from request
    let token_pair = match &request.request_type {
        MetadataRequestType::Limits { token_pair } => token_pair,
        _ => return Err(MetadataError::InvalidRequest),
    };

    // Create entrypoint for the limits call (for reference/tracing)
    let limits_entrypoint = create_limits_entrypoint(component, token_pair)?;

    Ok(MetadataValue::Limits(vec![
        (token_pair[0].clone(), (limit_0, limit_1, Some(limits_entrypoint)))
    ]))
}
```

**Assembled Metadata**

All parsed metadata for a component is assembled into:

```rust
pub struct ComponentTracingMetadata {
    pub tx_hash: TxHash,
    pub balances: Option<Result<Balances, MetadataError>>,
    pub limits: Option<Result<Limits, MetadataError>>,
    pub tvl: Option<Result<Tvl, MetadataError>>,
}
```

Note that each field is `Option<Result<...>>`:

* `None`: Metadata type not requested
* `Some(Ok(...))`: Successfully collected
* `Some(Err(...))`: Collection failed (triggers component failure)

**4.3 Hook Orchestrators**

Hook orchestrators coordinate the processing of components, including entrypoint generation and metadata injection.

**Orchestrator Responsibilities**

1. **Entrypoint Generation**: Create `EntryPointWithTracingParams` for tracing
2. **Balance Injection**: Add balances to `ProtocolComponent` for storage
3. **Limits Injection**: Provide limits for RPC query optimization
4. **State Updates**: Modify component state attributes as needed

**Interface**

```rust
#[async_trait]
pub trait HookOrchestrator: Send + Sync {
    async fn update_components(
        &self,
        block_changes: &mut BlockChanges,
        components: &[ProtocolComponent],
        metadata: &HashMap<String, ComponentTracingMetadata>,
        generate_entrypoints: bool,
    ) -> Result<(), HookOrchestratorError>;
}
```

**Parameters**:

* `block_changes`: Mutable reference to modify transactions and components
* `components`: Components to process in this call
* `metadata`: Collected external metadata (balances, limits, TVL)
* `generate_entrypoints`: `true` for full processing, `false` for balance-only

**Registry Lookup Mechanisms**

The `HookOrchestratorRegistry` provides multiple lookup strategies:

**1. By Hook Address** (Highest Priority)

```rust
registry.register_hook_orchestrator(
    Address::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8"),
    Box::new(MyOrchestrator::new()),
);
```

**2. By Hook Identifier** (Medium Priority)

```rust
registry.register_hook_identifier(
    "euler_v1".to_string(),
    Box::new(EulerOrchestrator::new()),
);
```

**3. Default Orchestrator** (Lowest Priority)

```rust
registry.set_default_orchestrator(
    Box::new(DefaultUniswapV4HookOrchestrator::new(entrypoint_generator)),
);
```

**Lookup Order**:

1. Try hook address lookup
2. Try hook identifier lookup (from component static attributes)
3. Fall back to default orchestrator
4. Return error if no orchestrator found

**Default Orchestrator**

The `DefaultUniswapV4HookOrchestrator` handles most hook types:

**Features**:

* Extracts balances from block changes for components without external metadata
* Delegates entrypoint generation to `UniswapV4DefaultHookEntrypointGenerator`
* Injects balances and limits into `BlockChanges`
* Handles both full processing and balance-only updates

**When to Use Custom Orchestrator**:

* Hook requires special entrypoint encoding
* Balance/limit data needs transformation before injection
* Component state updates follow custom logic
* Hook uses non-standard token accounting

**Euler Example - When Default is Sufficient**:

For Euler hooks, the default orchestrator works well because:

* Balances come directly from metadata (no transformation needed)
* Limits are standard max withdrawal amounts
* Entrypoints follow standard Uniswap V4 swap encoding
* No special state updates required

Therefore, Euler only requires custom metadata generator/parser, not a custom orchestrator.

**4.4 Entrypoint Generation**

Entrypoints define the calls that will be traced to understand how a component behaves under different conditions.

Entrypoints allow Tycho to:

* **Simulate swaps** at various amounts to understand pricing curves
* **Test edge cases** (e.g., swaps at 1%, 50%, 95% of liquidity)
* **Understand touched contracts and state** that are necessary for reproducing a hook's behavior

For hooks with external liquidity, accurate entrypoints require:

* Correct balance overwrites (both in PoolManager and external contracts)
* Appropriate swap amounts based on limits
* State overrides to simulate external contract states

**Swap Amount Estimation**

The system supports two estimation strategies:

**1. Limits-Based Estimation** (Preferred)

```rust
EstimationMethod::Limits
```

When limits are available, generate samples at:

* 1% of limit (test small swaps)
* 10% of limit (test medium swaps)
* 50% of limit (test large swaps)
* 95% of limit (test near-maximum swaps)

**2. Balance-Based Estimation** (Fallback)

```rust
EstimationMethod::Balances
```

When limits are unavailable, generate samples at:

* 1% of balance
* 2% of balance
* 5% of balance
* 10% of balance

**Euler Example - Limits-Based Amounts**:

```rust
// Euler provides withdrawal limits from getLimits()
// For token0 → token1 swap with limit = 1000000000000000000 (1e18):
let amounts = [
    10000000000000000,    // 1% = 0.01e18
    100000000000000000,   // 10% = 0.1e18
    500000000000000000,   // 50% = 0.5e18
    950000000000000000,   // 95% = 0.95e18
];
```

**V4MiniRouter Pattern**

For Uniswap V4, entrypoints use a custom router deployed via state overrides:

**Purpose**: Execute swap operations against the PoolManager with proper token settlements

**Pattern**:

```rust
// 1. Define router address (deterministic)
let router_address = Address::from("0x2626664c2603336E57B271c5C0b26F421741e481");

// 2. Build swap parameters
let pool_key = build_pool_key(component); // Extract from component attributes
let params = ExactInputSingleParams {
    pool_key,
    zero_for_one: true,  // token0 → token1
    amount_in,
    amount_out_minimum: Bytes::from([0u8]),
    hook_data: Bytes::from([0u8]),
};

// 3. Encode V4Router actions
let actions = vec![
    V4RouterAction::SWAP_EXACT_IN_SINGLE,  // Execute swap
    V4RouterAction::SETTLE_ALL,             // Settle input token
    V4RouterAction::TAKE_ALL,               // Take output token
];

let calldata = encode_execute_call(actions, params);

// 4. Set state overrides
let state_overrides = {
    // Deploy router bytecode
    router_address => AccountOverrides {
        code: Some(V4_MINI_ROUTER_BYTECODE),
        ...
    },
    // Set ERC6909 balances in PoolManager
    pool_manager => AccountOverrides {
        slots: erc6909_overwrites(token_in, sender, amount_in),
        ...
    },
    // (Optional) Set ERC20 balances for external tokens
    token_in => AccountOverrides {
        slots: erc20_balance_overwrite(sender, amount_in),
        ...
    },
};

// 5. Create entrypoint
let entrypoint = EntryPointWithTracingParams {
    entry_point: EntryPoint {
        external_id: format!("swap_{token0}_{token1}_{amount_in}"),
        target: router_address,
        signature: "execute(bytes,bytes[])".to_string(),
    },
    params: TracingParams::RPCTracer(RPCTracerParams {
        caller: Some(sender),
        calldata,
        state_overrides: Some(state_overrides),
        prune_addresses: None,
    }),
};
```

**ERC6909 Overwrites**

Uniswap V4 uses ERC6909 for internal PoolManager accounting. To simulate swaps, we must set balances:

```rust
// Slot calculation: keccak256(abi.encode(owner, id)) + 1
// Where id = uint256(uint160(currency))
fn calculate_erc6909_balance_slot(owner: &Address, currency: &Address) -> Bytes {
    let id = U256::from_be_bytes(currency.as_bytes());
    let key = encode_packed(&[
        Token::Address(owner.clone()),
        Token::Uint(id),
    ]);
    let base_slot = keccak256(&key);
    base_slot + U256::from(1)
}

// Overwrite with amount * 2 (to account for settlements)
state_overrides.insert(
    pool_manager,
    AccountOverrides {
        slots: Some(StorageOverride::Diff(
            vec![(balance_slot, amount_in * 2)].into_iter().collect()
        )),
        ...
    },
);
```

**Balance Slot Detection**

For hooks with external liquidity, tokens may need balances set in external contracts:

**Optional Feature**: `EVMBalanceSlotDetector`

```rust
// Detect ERC20 balance slots for tokens
let detected_slots = balance_slot_detector
    .detect_balance_slots(&[token_in], pool_manager, block_hash)
    .await?;

// Overwrite detected slots
if let Some(slot) = detected_slots.get(&token_in) {
    state_overrides.insert(
        token_in.clone(),
        AccountOverrides {
            slots: Some(StorageOverride::Diff(
                vec![(slot.clone(), amount_in * 2)].into_iter().collect()
            )),
            ...
        },
    );
}
```

**Euler Example - Balance Overwrites**:

For Euler hooks, tokens are held in external vaults. The entrypoint generator:

1. Detects balance slots for vault tokens (wstETH, WETH, etc.)
2. Overwrites those slots with swap amounts
3. Ensures PoolManager has ERC6909 balances
4. Simulates full swap flow including vault withdrawals

This allows accurate tracing even though liquidity is external to PoolManager.

\\
