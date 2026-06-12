//! Baseline/Mercury Substreams handlers.
//!
//! Baseline uses a singleton relay/proxy. Components are bTokens, and native
//! simulation is hydrated from quote-state attributes read from the relay.

mod quote_state;

use crate::abi::b_swap::events::Swap;
use crate::{pool_factories, pool_factories::DeploymentConfig};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
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

/// Find and create all relevant protocol components.
#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config = serde_qs::from_str(params.as_str())?;
    let mut tx_components_by_index: HashMap<u64, TransactionProtocolComponents> = HashMap::new();

    block.logs().for_each(|log| {
        let Some(component) = pool_factories::maybe_create_component(log.log, &config) else {
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
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    let mut latest_quote_state_tx: HashMap<String, (u64, QuoteStateAttributeChange)> =
        HashMap::new();

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
                    latest_quote_state_tx.insert(
                        component.id.clone(),
                        (tx.index, QuoteStateAttributeChange::Creation),
                    );
                });
        });

    block.transactions().for_each(|tx| {
        tx.logs_with_calls()
            .filter_map(|(log, _call)| {
                if log.address != config.relay_address {
                    return None;
                }
                Swap::match_and_decode(log)
                    .map(|event| format!("0x{}", hex::encode(event.b_token)))
                    .or_else(|| quote_state::maybe_update_component_id(log))
            })
            .for_each(|component_id| {
                let tx: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(tx.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&tx));
                builder.mark_component_as_updated(&component_id);
                latest_quote_state_tx
                    .entry(component_id)
                    .and_modify(|(index, _change)| *index = (*index).max(tx.index))
                    .or_insert((tx.index, QuoteStateAttributeChange::Update));
            });
    });

    latest_quote_state_tx
        .into_iter()
        .for_each(|(component_id, (tx_index, change))| {
            let Some(builder) = transaction_changes.get_mut(&tx_index) else {
                return;
            };
            let quote_state_attributes = quote_state::attributes_for_component(
                &config.relay_address,
                &component_id,
                change.change_type(),
            );
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
