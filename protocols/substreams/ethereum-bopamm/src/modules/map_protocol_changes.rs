use std::collections::HashMap;

use anyhow::Result;
use itertools::Itertools;
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    balances::aggregate_balances_changes, contract::extract_contract_changes_builder, prelude::*,
};

use crate::{
    common::{
        committed_updates, component_id, enumerate_books, is_zero, PAUSED_TOPIC, UNPAUSED_TOPIC,
    },
    config::DeploymentConfig,
};

/// Aggregates components, contract storage, balances, pause state and per-book quote
/// freshness into the final `BlockChanges`.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    grouped_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetProto<ProtocolComponent>,
    maker_store: StoreGetString,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    let maker = maker_store
        .get_last("maker")
        .and_then(|m| hex::decode(m).ok());

    add_new_components(&grouped_components, maker.as_deref(), &mut transaction_changes);

    aggregate_balances_changes(balance_store, deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map.values().for_each(|bc| {
                        builder.add_balance_change(bc);
                    })
                });
        });

    // Full storage + code of the three venue contracts (no DCI — fixed known set).
    extract_contract_changes_builder(
        &block,
        |addr| {
            addr == config.settlement.as_slice() ||
                addr == config.module.as_slice() ||
                addr == config.registry.as_slice()
        },
        &mut transaction_changes,
    );

    extract_committed_quotes(&block, &config, &mut transaction_changes);
    extract_maker_changes(&block, &config, &components_store, &mut transaction_changes);
    extract_pause_state(&block, &config, &components_store, &mut transaction_changes);
    mark_books_updated_on_module_changes(&config, &components_store, &mut transaction_changes);

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: vec![],
    })
}

/// Adds newly created book components and their default dynamic attributes.
fn add_new_components(
    grouped_components: &BlockTransactionProtocolComponents,
    maker: Option<&[u8]>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for tx_component in &grouped_components.tx_components {
        let tx = tx_component.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &tx_component.components {
            builder.add_protocol_component(component);
            let mut attributes = vec![Attribute {
                name: "update_marker".to_string(),
                value: vec![1u8],
                change: ChangeType::Creation.into(),
            }];
            if let Some(maker) = maker {
                attributes.push(Attribute {
                    name: "balance_owner".to_string(),
                    value: maker.to_vec(),
                    change: ChangeType::Creation.into(),
                });
            }
            builder.add_entity_change(&EntityChanges {
                component_id: component.id.clone(),
                attributes,
            });
        }
    }
}

/// Records each book's committed quote freshness from registry update calls as the
/// `override_block_timestamp` attribute (the `ts` decoded from the update calldata, 8-byte
/// big-endian u64).
///
/// `tycho-simulation` pins `block.timestamp` to this value when simulating the book, which
/// is what passes the registry's exact-timestamp `StaleUpdate()` gate. Marking the book
/// updated is required because `manual_updates` components only re-simulate when marked.
fn extract_committed_quotes(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            if call.address != config.registry {
                continue;
            }
            let updates = committed_updates(&call.input);
            if updates.is_empty() {
                continue;
            }
            let transaction: Transaction = tx.into();
            let builder = transaction_changes
                .entry(transaction.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
            for (book_id, committed_ts) in updates {
                let id = component_id(&config.settlement, book_id);
                builder.add_entity_change(&EntityChanges {
                    component_id: id.clone(),
                    attributes: vec![Attribute {
                        name: "override_block_timestamp".to_string(),
                        value: u64::from(committed_ts)
                            .to_be_bytes()
                            .to_vec(),
                        change: ChangeType::Update.into(),
                    }],
                });
                builder.mark_component_as_updated(&id);
            }
        }
    }
}

/// Emits/refreshes the `balance_owner` (maker) attribute on every book whenever the global
/// maker slot is written.
///
/// Covers the maker being configured *after* the books are created (the live case) and maker
/// rotation. New books created while the maker is already known get it via
/// `add_new_components`.
fn extract_maker_changes(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let books = enumerate_books(components_store);
    if books.is_empty() {
        return;
    }
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.module ||
                    change.key != config.maker_slot ||
                    is_zero(&change.new_value)
                {
                    continue;
                }
                let Some(maker) = change.new_value.get(12..32) else { continue };
                let transaction: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(transaction.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
                for book in &books {
                    builder.add_entity_change(&EntityChanges {
                        component_id: book.clone(),
                        attributes: vec![Attribute {
                            name: "balance_owner".to_string(),
                            value: maker.to_vec(),
                            change: ChangeType::Update.into(),
                        }],
                    });
                    builder.mark_component_as_updated(book);
                }
            }
        }
    }
}

/// Reflects settlement `Paused`/`Unpaused` events onto every book (the venue is paused as a
/// whole). Paused books should not be routed through until unpaused.
fn extract_pause_state(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let books = enumerate_books(components_store);
    if books.is_empty() {
        return;
    }
    for log in block.logs() {
        if log.address() != config.settlement.as_slice() {
            continue;
        }
        let Some(topic0) = log.log.topics.first() else { continue };
        let paused = if topic0.as_slice() == PAUSED_TOPIC {
            true
        } else if topic0.as_slice() == UNPAUSED_TOPIC {
            false
        } else {
            continue;
        };
        let tx: Transaction = log.receipt.transaction.into();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for book in &books {
            builder.change_component_pause_state(book, paused);
        }
    }
}

/// A change to the shared module storage (maker/asset config) marks every book as needing
/// re-simulation. Per-book registry quote refreshes are marked in `extract_committed_quotes`.
///
/// This is deliberately over-inclusive: any module storage write (not just config slots)
/// re-marks all books, which is safe (never misses an update) at the cost of occasional
/// redundant re-simulation.
fn mark_books_updated_on_module_changes(
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let books = enumerate_books(components_store);
    if books.is_empty() {
        return;
    }
    for builder in transaction_changes.values_mut() {
        let touches_module = builder
            .changed_contracts()
            .any(|addr| addr == config.module.as_slice());
        if touches_module {
            for book in &books {
                builder.mark_component_as_updated(book);
            }
        }
    }
}
