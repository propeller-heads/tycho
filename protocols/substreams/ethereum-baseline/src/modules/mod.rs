//! Baseline Substreams handlers.
//!
//! Baseline uses a singleton relay/proxy. Components are bTokens, and native
//! simulation is hydrated from quote-state attributes reconstructed from relay
//! storage diffs.

mod manual_updates;
mod quote_state;
mod slot_layout;
mod slot_stores;

use crate::abi::b_swap::events::Swap;
use crate::{pool_factories, pool_factories::RELAY_ADDRESS};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetString},
};
use substreams_ethereum::pb::eth;
use substreams_ethereum::Event;
use tycho_substreams::prelude::*;

#[derive(Clone, Copy)]
enum QuoteStateAttributeChange {
    Creation,
    Update,
}

impl QuoteStateAttributeChange {
    fn change_type(self) -> ChangeType {
        match self {
            Self::Creation => ChangeType::Creation,
            Self::Update => ChangeType::Update,
        }
    }
}

#[derive(Clone, Copy)]
struct QuoteStateTx {
    tx_index: u64,
    change: QuoteStateAttributeChange,
    read_ordinal: Option<u64>,
}

impl QuoteStateTx {
    fn creation(tx_index: u64) -> Self {
        Self { tx_index, change: QuoteStateAttributeChange::Creation, read_ordinal: None }
    }

    fn update(tx_index: u64, read_ordinal: Option<u64>) -> Self {
        Self { tx_index, change: QuoteStateAttributeChange::Update, read_ordinal }
    }

    fn record_update(&mut self, tx_index: u64, read_ordinal: Option<u64>) {
        // Preserve Creation when same-block storage deltas arrive for a new component.
        self.tx_index = self.tx_index.max(tx_index);
        self.read_ordinal = max_optional(self.read_ordinal, read_ordinal);
    }
}

fn max_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn record_quote_state_update(
    latest_quote_state_tx: &mut HashMap<String, QuoteStateTx>,
    component_id: String,
    tx_index: u64,
    read_ordinal: Option<u64>,
) {
    latest_quote_state_tx
        .entry(component_id)
        .and_modify(|state_tx| state_tx.record_update(tx_index, read_ordinal))
        .or_insert_with(|| QuoteStateTx::update(tx_index, read_ordinal));
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

/// Aggregates protocol components and quote-state changes by transaction.
#[substreams::handlers::map]
fn map_protocol_changes(
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    state_deltas: StoreDeltas,
    state: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    let mut latest_quote_state_tx: HashMap<String, QuoteStateTx> = HashMap::new();

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
                    latest_quote_state_tx
                        .insert(component.id.clone(), QuoteStateTx::creation(tx.index));
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
                record_quote_state_update(&mut latest_quote_state_tx, component_id, tx.index, None);
            });
    });

    state_deltas
        .deltas
        .iter()
        .filter_map(|delta| state_component_id(&delta.key).map(|id| (id, delta.ordinal)))
        .for_each(|(component_id, ordinal)| {
            let Some(tx) = transaction_for_ordinal(&block, ordinal) else {
                return;
            };
            let tx: Transaction = (&tx).into();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.mark_component_as_updated(&component_id);
            record_quote_state_update(
                &mut latest_quote_state_tx,
                component_id,
                tx.index,
                Some(ordinal),
            );
        });

    latest_quote_state_tx
        .into_iter()
        .for_each(|(component_id, state_tx)| {
            let Some(builder) = transaction_changes.get_mut(&state_tx.tx_index) else {
                return;
            };
            let quote_state_attributes = quote_state::attributes_from_store(
                &state,
                &component_id,
                state_tx.read_ordinal,
                block.number,
                state_tx.change.change_type(),
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
            (tx.begin_ordinal <= ordinal && tx.end_ordinal >= ordinal)
                || tx
                    .calls
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
    fn preserves_creation_change_when_storage_delta_arrives_for_new_component() {
        let component_id = "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63".to_string();
        let mut latest_quote_state_tx =
            HashMap::from([(component_id.clone(), QuoteStateTx::creation(3))]);

        record_quote_state_update(&mut latest_quote_state_tx, component_id.clone(), 3, Some(42));

        let state_tx = latest_quote_state_tx
            .get(&component_id)
            .unwrap();
        assert!(matches!(state_tx.change, QuoteStateAttributeChange::Creation));
        assert_eq!(state_tx.tx_index, 3);
        assert_eq!(state_tx.read_ordinal, Some(42));
    }

    #[test]
    fn tracks_latest_storage_read_ordinal_for_component() {
        let component_id = "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63".to_string();
        let mut latest_quote_state_tx = HashMap::new();

        record_quote_state_update(&mut latest_quote_state_tx, component_id.clone(), 1, Some(10));
        record_quote_state_update(&mut latest_quote_state_tx, component_id.clone(), 1, Some(14));
        record_quote_state_update(&mut latest_quote_state_tx, component_id.clone(), 2, None);

        let state_tx = latest_quote_state_tx
            .get(&component_id)
            .unwrap();
        assert!(matches!(state_tx.change, QuoteStateAttributeChange::Update));
        assert_eq!(state_tx.tx_index, 2);
        assert_eq!(state_tx.read_ordinal, Some(14));
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
