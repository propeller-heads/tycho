# Normalizing relative ERC20 Balances

Tracking balances is complex if only relative values are available. If the protocol provides absolute balances (e.g., through logs), you can skip this section and simply emit the absolute balances.

To derive absolute balances from relative values, you’ll need to aggregate by component and token, ensuring that balance changes are tracked at the transaction level within each block.&#x20;

### Implementation Steps:

#### 1. Index relative balance changes

To accurately process each block and report balance changes, implement a handler that returns the `BlockBalanceDeltas` struct. Each `BalanceDelta` for a component-token pair must be assigned a strictly increasing ordinal to preserve transaction-level integrity. Incorrect ordinal sequencing can lead to inaccurate balance aggregation.

Example interface for a handler that uses an integer, loaded from a store to indicate if a specific address is a component:

```rust
#[substreams::handlers::map]
pub fn map_relative_balances(
    block: eth::v2::Block,
    components_store: StoreGetInt64,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    todo!()
}
```

Use the `tycho_substream::balances::extract_balance_deltas_from_tx` function from our Substreams SDK to extract `BalanceDelta` data from ERC20 Transfer events for a given transaction, as in the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/substreams/ethereum-curve/src/modules.rs" target="_blank" rel="noopener noreferrer">Curve implementation</a>.

# Tracking Balance Deltas from Events

In some protocols, you need to aggregate balance changes across multiple event types (swap, mint, burn, etc.) within a transaction. Rather than implementing separate processing logic for each event type, Tycho uses a trait-based pattern that allows uniform handling of all balance-changing events.

## Overview

The pattern involves:
1. Defining a `BalanceEventTrait` that extracts balance deltas from any event
2. Creating an enum that wraps all relevant event types
3. Implementing the trait for each specific event type
4. Using helper functions to process events uniformly

## Implementation Steps

### 1. Define the BalanceEventTrait

Create a trait that standardizes how balance deltas are extracted from events:
```rust
/// A trait for extracting balance changes from protocol events.
pub trait BalanceEventTrait {
    /// Get all balance deltas from the event.
    ///
    /// # Arguments
    ///
    /// * `tx` - Reference to the transaction containing this event
    /// * `pool` - The pool state (protobuf-defined struct, e.g., `MaverickPool`)
    /// * `event` - The event log, used to access the ordinal and emitter address
    ///
    /// # Returns
    ///
    /// A vector of balance deltas representing all token balance changes
    fn get_balance_delta(
        &self,
        tx: &Transaction,
        pool: &ProtocolComponent,
        event: &Log
    ) -> Vec<BalanceDelta>;
}
```

### 2. Create an EventType Enum

Define an enum representing all balance-changing events for your protocol. Each variant wraps a struct containing the decoded event data:
```rust
/// Represents all balance-changing events for a Maverick pool
pub enum EventType {
    PoolSwap(PoolSwap),
    AddLiquidity(PoolAddLiquidity),
    RemoveLiquidity(PoolRemoveLiquidity),
}

impl EventType {
    fn as_balance_event(&self) -> &dyn BalanceEventTrait {
        match self {
            EventType::PoolSwap(event) => event,
            EventType::AddLiquidity(event) => event,
            EventType::RemoveLiquidity(event) => event,
        }
    }
}
```

Each variant wraps a type that implements `BalanceEventTrait`. The `as_balance_event` method uses Rust's trait object coercion to convert each concrete event type (e.g., `&PoolSwap`) into `&dyn BalanceEventTrait`.

### 3. Implement Event Decoding

Create a function that matches and decodes Ethereum logs into your supported event types:
```rust
/// Decodes an Ethereum log into a recognized pool event
///
/// # Arguments
///
/// * `event` - Reference to the Ethereum log
///
/// # Returns
///
/// An `Option` containing the decoded `EventType`, or `None` if unrecognized
pub fn decode_event(event: &Log) -> Option<EventType> {
    [
        PoolSwap::match_and_decode(event).map(EventType::PoolSwap),
        PoolAddLiquidity::match_and_decode(event).map(EventType::AddLiquidity),
        PoolRemoveLiquidity::match_and_decode(event).map(EventType::RemoveLiquidity),
    ]
    .into_iter()
    .find_map(std::convert::identity)
}
```

### 4. Implement the Trait for Each Event Type

Implement `BalanceEventTrait` for each concrete event struct. Here are examples for liquidity and swap events:

**Add Liquidity Event:**
```rust
use crate::{
    abi::pool::events::PoolAddLiquidity,
    events::BalanceEventTrait,
    pb::maverick::v2::Pool,
};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

impl BalanceEventTrait for PoolAddLiquidity {
    fn get_balance_delta(
        &self,
        tx: &Transaction,
        pool: &Pool,
        event: &Log
    ) -> Vec<BalanceDelta> {
        vec![
            BalanceDelta {
                ord: event.ordinal,
                tx: Some(tx.clone()),
                token: pool.token_a.clone(),
                delta: self.token_a_amount.clone().to_signed_bytes_be(),
                component_id: pool.address.clone().to_hex().as_bytes().to_vec(),
            },
            BalanceDelta {
                ord: event.ordinal,
                tx: Some(tx.clone()),
                token: pool.token_b.clone(),
                delta: self.token_b_amount.clone().to_signed_bytes_be(),
                component_id: pool.address.clone().to_hex().as_bytes().to_vec(),
            },
        ]
    }
}
```

**Swap Event:**
```rust
use crate::{
    abi::pool::events::PoolSwap,
    events::BalanceEventTrait,
    pb::maverick::v2::Pool
};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

impl BalanceEventTrait for PoolSwap {
    fn get_balance_delta(
        &self,
        tx: &Transaction,
        pool: &Pool,
        event: &Log
    ) -> Vec<BalanceDelta> {
        let (token_in, token_out, amount_in, amount_out) = if self.params.1 {
            (&pool.token_a, &pool.token_b, &self.amount_in, &self.amount_out)
        } else {
            (&pool.token_b, &pool.token_a, &self.amount_in, &self.amount_out)
        };

        vec![
            BalanceDelta {
                ord: event.ordinal,
                tx: Some(tx.clone()),
                token: token_in.clone(),
                delta: amount_in.clone().to_signed_bytes_be(),
                component_id: pool.address.clone().to_hex().as_bytes().to_vec(),
            },
            BalanceDelta {
                ord: event.ordinal,
                tx: Some(tx.clone()),
                token: token_out.clone(),
                delta: amount_out.neg().clone().to_signed_bytes_be(),
                component_id: pool.address.clone().to_hex().as_bytes().to_vec(),
            },
        ]
    }
}
```

### 5. Create a Helper Function for Processing Events

Implement a helper function that processes any event using the trait:
```rust
/// Extracts balance deltas from a log event
///
/// # Arguments
///
/// * `tx` - Reference to the transaction
/// * `event` - Reference to the event log
/// * `pool` - Reference to the pool state
///
/// # Returns
///
/// A vector of balance deltas, or empty if the event is not recognized
pub fn get_log_changed_balances(
    tx: &Transaction,
    event: &Log,
    pool: &Pool
) -> Vec<BalanceDelta> {
    decode_event(event)
        .map(|e| e.as_balance_event().get_balance_delta(tx, pool, event))
        .unwrap_or_default()
}
```

### 6. Aggregate Balance Deltas Across a Block

Finally, create a map handler that processes all transactions in a block and aggregates balance deltas:
```rust
use crate::{events::get_log_changed_balances, pb::maverick::v2::Pool};
use anyhow::{Ok, Result};
use substreams::{prelude::StoreGetProto, store::StoreGet};
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_relative_balances(
    block: Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let mut balance_deltas = Vec::new();
    
    for trx in block.transactions() {
        let mut tx_deltas = Vec::new();
        
        for log in trx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| &call.logs)
        {
            if let Some(pool) = pools_store.get_last(format!("Pool:{}", &log.address.to_hex())) {
                tx_deltas.extend(get_log_changed_balances(&tx.into(), log, &pool));
            }
        }
        
        if !tx_deltas.is_empty() {
            balance_deltas.extend(tx_deltas);
        }
    }
    
    Ok(BlockBalanceDeltas { balance_deltas })
}
```

## Reference Implementations

For complete examples of this pattern, see:
- [Maverick Implementation](https://github.com/propeller-heads/tycho-protocol-sdk/tree/main/substreams/ethereum-maverick-v2/src/events)
- [CowAMM Implementation](https://github.com/propeller-heads/tycho-protocol-sdk/tree/main/substreams/ethereum-cowamm/src/events)
- [Aerodrome Slipstreams Implementation](https://github.com/propeller-heads/tycho-protocol-sdk/tree/main/substreams/base-aerodrome-slipstreams/src/events)

#### 2. Aggregate balances with an additive store

To efficiently convert `BlockBalanceDeltas` messages into absolute values while preserving transaction granularity, use the `StoreAddBigInt` type with a store module. The `tycho_substream::balances::store_balance_changes` helper function simplifies this task.

Typical usage of this function:

```rust
#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}
```

#### 3. Combine absolute values with component and address

Finally, associate absolute balances with their corresponding transaction, component, and token. Use the `tycho_substream::balances::aggregate_balances_changes` helper function for the final aggregation step. This function outputs `BalanceChange` structs for each transaction, which can then be integrated into `map_protocol_changes` to retrieve absolute balance changes per transaction.

Example usage:

```rust
#[substreams::handlers::map]
pub fn map_protocol_changes(
    block: eth::v2::Block,
    grouped_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetInt64,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let mut transaction_contract_changes: HashMap<_, TransactionChanges> = HashMap::new();

    aggregate_balances_changes(balance_store, deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            transaction_contract_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChanges::new(&tx))
                .balance_changes
                .extend(balances.into_values());
        });
}
```

Each step ensures accurate tracking of balance changes, making it possible to reflect absolute values for components and tokens reliably.
