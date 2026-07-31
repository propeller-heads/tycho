---
description: Commonly used entities and concepts within Tycho.
---

# Concepts

This outlines the core entities and components that form the foundation of the Tycho system. Understanding these concepts is essential for working with or on the application effectively.

## Entities

### ProtocolSystem

With ProtocolSystems we usually refer to a DeFi protocol. A group of smart contracts that work collectively provide financial services to users. Each protocol typically contains:

* A single Extractor (see below)
* One or more ProtocolComponents

We model major versions of protocols as distinct entities. For example, Uniswap V2 and Uniswap V3 are separate ProtocolSystems.

**Attributes**:

* **name**: The protocols' identifier
* **protocol\_type**: The category of protocol being indexed, currently pure organisational use.
  * **name**: The identifier of the protocol type
  * **financial\_type**: The specific financial service provided:
    * Swap
    * PSM
    * Debt
    * Leverage
  * **attribute\_schema**: Currently unused; initially intended to validate static and hybrid attributes.
  * **implementation\_type**: Either VM or Custom (native - see below)

### Token

Tokens represent fungible tradeable assets on a blockchain. Users interact with protocols primarily to buy, sell, or provide liquidity for tokens. While ERC20 is the most common standard, Tycho supports other token types as well.

Tycho automatically detects and ingests new tokens when a ProtocolComponent using that token is ingested in the DB. Upon detection, we run test transactions to determine the token's behavior.

**Attributes:**

* **Address**: The blockchain address that uniquely identifies the token
* **Decimals**: Number of decimal places used to represent token values
* **Symbol**: Short human-readable identifier (e.g., ETH, USDC)
* **Tax**: Token transfer tax in basis points, averaged across simulated transfers
* **Gas**: Cost to transfer the token in the blockchain's native compute units
* **Chain**: The blockchain where the token is deployed
* **Quality**: Score from 0-100 indicating token reliability:
  * 100: Standard token with normal behavior
  * 75: Rebase token (supply adjusts automatically)
  * 50: Fee token (charges fees on transfers)
  * 10: Failed initial token analysis
  * 9-5: Failed subsequent analysis after creation
  * 0: Could not extract decimals from on-chain data

### ProtocolComponent

ProtocolComponents represent specific operations that can be executed on token sets within a ProtocolSystem. Examples include liquidity pools in DEXes or lending markets in lending protocols.

A new ProtocolComponent is created whenever a new operation becomes available for a set of tokens such as when a new trading pair is deployed on a DEX.

**Attributes:**

* **id**: A unique identifier for the component
* **protocol\_system**: The parent protocol system
* **protocol\_type\_name**: Subtype classification for filtering components
* **chain**: Blockchain where the component operates
* **tokens**: Addresses of tokens this component works with
* **contract\_addresses**: Smart contracts involved in executing operations (may be empty for native implementations)
* **static\_attributes**: Constant properties known at creation time, including:
  * Attributes used to filter components (e.g. RPC and/or DB queries)
  * Parameters needed to execute operations (fees, factory addresses, pool keys)
* **creation\_tx**: Transaction hash that created this component
* **created\_at**: Timestamp of component creation

Each component also has **dynamic attributes** that change over time and contain state required to simulate operations.

## Indexer

The indexer subsystem processes blockchain data, maintains an up-to-date representation of entities and provides RPC and Websocket endpoints exposing those entities to clients.

### Extractor

An Extractor processes incoming blockchain data, either at the block level or at shorter intervals (e.g. mempool data or partial blocks from builders).

The Extractor:

1. Pushes finalized state changes to permanent storage
2. Stores unfinalized data in system buffers (see ReorgBuffers below)
3. Performs basic validation, such as checking for the existence of related entities and verifying the connectedness of incoming data
4. Aggregates processed changes and broadcasts them to connected clients
5. Handles chain reorganizations by reverting changes in buffers and sending correction messages to clients

### Versioning

Tycho's persistence layer tracks state changes at the transaction level. This granular versioning enables future use cases such as:

* Replay changes transaction by transaction for backtesting
* Historical analysis of protocol behavior

The default storage backend (PostgreSQL) maintains versioned data up to a configurable time horizon. Older changes are pruned to conserve storage space and maintain query performance.

{% hint style="info" %}
While the system supports versioning, alternative persistence implementations aren't required to implement this feature.
{% endhint %}

### Reorg Buffer

ReorgBuffers store unfinalized blockchain state changes that haven't yet reached sufficient confirmation depth.

This approach allows Tycho to:

1. Respond to queries with the latest state by merging buffer data with permanent storage
2. Handle chain reorganizations by rolling back unconfirmed changes
3. Send precise correction messages to clients when previously reported states are invalidated

When a reorganization occurs, the system uses these buffers to calculate exactly what data needs correction, minimizing disruption to connected applications.

### Dynamic Contract Indexing (DCI)

The DCI is an extractor extension designed to dynamically identify and index dependency contracts based on supplied tracing information. The DCI relies on an integrated protocol to provide the information with which it can analyse and detect contracts that require indexing.

On a successful trace, the DCI identifies all external contracts that were called, which storage slots were accessed for those contracts, and potential retriggers for the entry point. A retrigger is any contract storage slot that is flagged for its potential to influence a trace result. If a retrigger slot is updated, the trace is repeated. For all identified contracts, the code and relevant storage is fetched at the current block. Thereafter, updates for those contracts are extracted from the block messages themselves.

## Simulation

The simulation library allows clients to locally compute the outcome of potential operations without executing them on-chain, enabling efficient price discovery and impact analysis.

### Virtual Machine (VM) vs Native (Custom)

Tycho offers two approaches for simulating protocol operations:

**Virtual Machine (VM) Integration**

* Uses the blockchain's VM to execute operations
* Requires a contract that adapts the protocol's interface to Tycho's interface
* Creates a minimal local blockchain view with only the necessary contract state
* **Advantages**:
  * Faster integration of new protocols
  * No need to reimplement complex protocol math
* **Disadvantages:**
  * Significantly slower simulation compared to native implementations

**Native Implementation**

* Reimplements protocol operations directly in Rust code
* Compiles to optimized machine code for the target architecture
* May still access the VM if required, e.g. to simulate Uniswap V4 hooks
* **Advantages**:
  * Much faster simulation performance
  * More efficient for high-volume protocols
* **Disadvantages**:
  * Longer integration time
  * Requires comprehensive understanding of protocol mathematics
  * Must identify and index all relevant state variables

## Execution

### Solution

The Solution represents a complete pathway for moving tokens through one or more protocols to fulfil a trade. It bridges the gap between finding the best trade route and actually executing it on-chain.

Solutions can represent single-hop swaps, sequential multi-hop trades, or split routes where a token amount is distributed across multiple pools. See more about Solutions [here](for-solvers/execution/encoding/#solution-struct).

### Strategy

Strategies define how Solutions are translated into Transactions, offering different tradeoffs between complexity, gas efficiency, and security. They encapsulate the logic for how trades should be executed on-chain.

Tycho currently supports three distinct strategies for executing trades: **Single**, **Sequential**, and **Split**.

<figure><img src=".gitbook/assets/all (1).svg" alt=""><figcaption><p>Diagram representing examples of the multiple types of solutions</p></figcaption></figure>

Before diving into these, it is useful to clarify a few terms:

* **Solution / Trade**: A complete plan to exchange token A for token B. This may involve routing through intermediate tokens, but it is conceptually treated as a single trade.
* **Swap/Hop**: An individual exchange between two tokens. A trade may consist of one or more swaps.

#### Single

The encoder uses the **single** strategy when a Solution has exactly one swap on one pool.

#### Sequential <a href="#sequential" id="sequential"></a>

The encoder uses the **sequential** strategy when your Solution has multiple sequential swaps, and no splits (e.g. A → B → C). Outputs from one are equal to the input to the next swap.

#### Split

With the **Split** strategy, you can encode the most advanced solutions: Trades that involve multiple swaps, where you split amounts either in parallel paths or within stages of a multi-hop route.

For more about split swaps, see [here](for-solvers/execution/encoding/#split-swaps).
