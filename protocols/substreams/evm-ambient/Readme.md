Substreams Ethereum Ambient Module
==================================

Modules Description
-------------------

### `map_pool_changes`

*   **Type**: Map
*   **Purpose**: This module detects new pools within the Ethereum blockchain and balance changes.
*   **Inputs**: Ethereum block data (`sf.ethereum.type.v2.Block`).
*   **Output**: Emits data of type `proto:tycho.ambient.v1.BlockPoolChanges`.

### `store_pools_balances`

*   **Type**: Store
*   **Purpose**: Accumulates and stores the balances of pools detected by `map_pool_changes`. It uses an additive update policy, implying that new values are added to existing balances.
*   **Inputs**: Data mapped by `map_pool_changes`.

### `store_pools`

*   **Type**: Store
*   **Purpose**: Maintains a store of pool information using the `ProtocolComponent` data structure. This store is updated whenever `map_pool_changes` emits new pool data.
*   **Inputs**: Data mapped by `map_pool_changes`.

### `map_changes`

*   **Type**: Map
*   **Purpose**: This module integrates all the processed information to generate comprehensive `BlockChanges`. It considers new pools, balance changes and contract changes.
*   **Inputs**:
    *   Ethereum block data (`sf.ethereum.type.v2.Block`).
    *   Data from `map_pool_changes`.
    *   Data from `store_pools_balances`.
    *   Data from `store_pools`.
*   **Output**: Emits `proto:tycho.evm.state.v1.BlockChanges`.
