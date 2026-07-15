//! Baseline Substreams handlers.
//!
//! Baseline uses a singleton relay/proxy. Components are bTokens, and native
//! simulation is hydrated from quote-state attributes reconstructed from relay
//! storage diffs.

mod manual_updates;
mod quote_state;
mod slot_layout;
mod slot_stores;

use crate::{abi::b_swap::events::Swap, pool_factories, pool_factories::RELAY_ADDRESS};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetString, StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString},
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::prelude::*;

/// The last transaction that touched a component's quote state in a block, and the highest
/// storage-delta ordinal to read the state stores at (`None` means end of block).
#[derive(Clone, Copy, Default)]
struct QuoteStateUpdate {
    tx_index: u64,
    read_ordinal: Option<u64>,
}

impl QuoteStateUpdate {
    fn record(&mut self, tx_index: u64, read_ordinal: Option<u64>) {
        self.tx_index = self.tx_index.max(tx_index);
        self.read_ordinal = match (self.read_ordinal, read_ordinal) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
    }
}

fn record_quote_state_update(
    quote_state_updates: &mut HashMap<String, QuoteStateUpdate>,
    component_id: String,
    tx_index: u64,
    read_ordinal: Option<u64>,
) {
    quote_state_updates
        .entry(component_id)
        .or_default()
        .record(tx_index, read_ordinal);
}

/// Find and create all relevant protocol components.
#[substreams::handlers::map]
fn map_protocol_components(block: eth::v2::Block) -> Result<BlockTransactionProtocolComponents> {
    let mut tx_components_by_index: HashMap<u64, TransactionProtocolComponents> = HashMap::new();

    block.logs().for_each(|log| {
        let Some(component) = pool_factories::maybe_create_component(log.log) else {
            return;
        };

        let tx: Transaction = log.receipt.transaction.into();
        tx_components_by_index
            .entry(tx.index)
            .or_insert_with(|| TransactionProtocolComponents {
                tx: Some(tx),
                components: Vec::new(),
            })
            .components
            .push(component);
    });

    Ok(BlockTransactionProtocolComponents {
        tx_components: tx_components_by_index
            .into_iter()
            .sorted_unstable_by_key(|(index, _)| *index)
            .map(|(_, tx_components)| tx_components)
            .collect(),
    })
}

/// Records every created component id so later modules can tell created pools apart from
/// bTokens that only reached the `createBToken` stage.
#[substreams::handlers::store]
fn store_components(
    components: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsString,
) {
    components
        .tx_components
        .iter()
        .flat_map(|tx_components| tx_components.components.iter())
        .for_each(|component| {
            store.set_if_not_exists(0, component_key(&component.id), &component.id)
        });
}

fn component_key(component_id: &str) -> String {
    format!("pool:{component_id}")
}

/// Aggregates protocol components and quote-state changes by transaction.
#[substreams::handlers::map]
fn map_protocol_changes(
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    components_store: StoreGetString,
    state_deltas: StoreDeltas,
    state: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    let mut quote_state_updates: HashMap<String, QuoteStateUpdate> = HashMap::new();

    // Register new components and initialize their entity state with zero-valued defaults in
    // one pass. The computed quote state from this block's storage deltas overwrites the
    // defaults below, so the snapshot always carries the full attribute set.
    new_components
        .tx_components
        .iter()
        .for_each(|tx_component| {
            let tx = tx_component.tx.as_ref().unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            tx_component
                .components
                .iter()
                .for_each(|component| {
                    builder.add_protocol_component(component);
                    builder.add_entity_change(&EntityChanges {
                        component_id: component.id.clone(),
                        attributes: quote_state::default_attributes(),
                    });
                    record_quote_state_update(
                        &mut quote_state_updates,
                        component.id.clone(),
                        tx.index,
                        None,
                    );
                });
        });

    block.transactions().for_each(|tx| {
        tx.logs_with_calls()
            .filter_map(|(log, _call)| {
                if log.address.as_slice() != RELAY_ADDRESS {
                    return None;
                }
                Swap::match_and_decode(log)
                    .map(|event| format!("0x{}", hex::encode(event.b_token)))
                    .or_else(|| manual_updates::maybe_component_id(log))
            })
            .for_each(|component_id| {
                let tx: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(tx.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&tx));
                builder.mark_component_as_updated(&component_id);
                record_quote_state_update(&mut quote_state_updates, component_id, tx.index, None);
            });
    });

    state_deltas
        .deltas
        .iter()
        .filter_map(|delta| state_component_id(&delta.key).map(|id| (id, delta.ordinal)))
        .for_each(|(component_id, ordinal)| {
            // pool.totalSupply is written by createBToken before the pool exists; ignore state
            // deltas until PoolCreated has made the component known.
            if !quote_state_updates.contains_key(&component_id) &&
                components_store
                    .get_last(component_key(&component_id))
                    .is_none()
            {
                return;
            }
            let Some(tx) = transaction_for_ordinal(&block, ordinal) else {
                return;
            };
            let tx: Transaction = (&tx).into();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.mark_component_as_updated(&component_id);
            record_quote_state_update(
                &mut quote_state_updates,
                component_id,
                tx.index,
                Some(ordinal),
            );
        });

    // Emit the computed quote state once per touched component, attached to the last
    // transaction that changed it.
    quote_state_updates
        .into_iter()
        .for_each(|(component_id, update)| {
            let Some(builder) = transaction_changes.get_mut(&update.tx_index) else {
                return;
            };
            let quote_state_attributes = quote_state::attributes_from_store(
                &state,
                &component_id,
                update.read_ordinal,
                block.number,
            )
            .unwrap_or_default();
            if !quote_state_attributes.is_empty() {
                builder.add_entity_change(&EntityChanges {
                    component_id,
                    attributes: quote_state_attributes,
                });
            }
        });

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: Vec::new(),
    })
}

fn state_component_id(key: &str) -> Option<String> {
    let mut segments = key.split(':');
    (segments.next()? == "state").then_some(())?;
    let component_id = segments.next()?.to_string();
    segments.next()?;
    segments.next()?;
    segments
        .next()
        .is_none()
        .then_some(component_id)
}

fn transaction_for_ordinal(
    block: &eth::v2::Block,
    ordinal: u64,
) -> Option<eth::v2::TransactionTrace> {
    block
        .transaction_traces
        .iter()
        .find(|tx| {
            (tx.begin_ordinal <= ordinal && tx.end_ordinal >= ordinal) ||
                tx.calls
                    .iter()
                    .any(|call| call.begin_ordinal <= ordinal && call.end_ordinal >= ordinal)
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state_store_keys() {
        assert_eq!(
            state_component_id("state:0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63:pool:6"),
            Some("0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63".to_string())
        );
        assert_eq!(
            state_component_id("state:0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63:pool"),
            None
        );
        assert_eq!(
            state_component_id("other:0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63:pool:6"),
            None
        );
    }

    #[test]
    fn tracks_latest_storage_read_ordinal_for_component() {
        let component_id = "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63".to_string();
        let mut quote_state_updates = HashMap::new();

        record_quote_state_update(&mut quote_state_updates, component_id.clone(), 1, Some(10));
        record_quote_state_update(&mut quote_state_updates, component_id.clone(), 1, Some(14));
        record_quote_state_update(&mut quote_state_updates, component_id.clone(), 2, None);

        let update = quote_state_updates
            .get(&component_id)
            .unwrap();
        assert_eq!(update.tx_index, 2);
        assert_eq!(update.read_ordinal, Some(14));
    }

    #[test]
    fn matches_storage_delta_ordinals_to_transactions() {
        let block = eth::v2::Block {
            transaction_traces: vec![
                eth::v2::TransactionTrace {
                    index: 0,
                    begin_ordinal: 10,
                    end_ordinal: 19,
                    calls: vec![eth::v2::Call {
                        begin_ordinal: 11,
                        end_ordinal: 18,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                eth::v2::TransactionTrace {
                    index: 1,
                    begin_ordinal: 20,
                    end_ordinal: 29,
                    calls: vec![eth::v2::Call {
                        begin_ordinal: 21,
                        end_ordinal: 28,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            transaction_for_ordinal(&block, 12)
                .unwrap()
                .index,
            0
        );
        assert_eq!(
            transaction_for_ordinal(&block, 27)
                .unwrap()
                .index,
            1
        );
        assert!(transaction_for_ordinal(&block, 30).is_none());
    }
}
