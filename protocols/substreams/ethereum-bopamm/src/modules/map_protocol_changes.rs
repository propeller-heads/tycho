use std::collections::{HashMap, HashSet};

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
        asset_token, committed_updates, component_id, enumerate_books, is_zero,
        superseded_book_ids, superseded_books_for_token, u64_from_word_padded, PAUSED_TOPIC,
        UNPAUSED_TOPIC,
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
    pause_superseded_books(
        &grouped_components,
        &config,
        &components_store,
        &mut transaction_changes,
    );

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

    extract_committed_quotes(&block, &config, &components_store, &mut transaction_changes);
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

/// Pauses books that a newly created book has superseded.
///
/// BopAMM re-lists an asset under a fresh asset id rather than delisting the old book, so a
/// token's previous book keeps its last committed (now stale) quote. Left active it would surface
/// a dead market — and revert `StaleUpdate()` once its registry lane is cleared. When a
/// replacement book is created, mark the older book(s) for the same token paused so consumers
/// stop routing through them. Runs only on the block a replacement is created (rare).
fn pause_superseded_books(
    grouped_components: &BlockTransactionProtocolComponents,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for tx_component in &grouped_components.tx_components {
        let Some(tx) = tx_component.tx.as_ref() else { continue };
        for component in &tx_component.components {
            let Some(asset_id) = component
                .get_attribute_value("asset_id")
                .and_then(|b| u64_from_word_padded(&b))
            else {
                continue;
            };
            let Some(asset) = asset_token(component, &config.usdc) else { continue };
            let superseded = superseded_books_for_token(&asset, asset_id, components_store);
            if superseded.is_empty() {
                continue;
            }
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));
            for old_id in superseded {
                builder.change_component_pause_state(&old_id, true);
            }
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
    components_store: &StoreGetProto<ProtocolComponent>,
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
            for (caller, book_id, committed_ts) in updates {
                // The PrioUpdateRegistry is shared across venues. Book ids are only unique per
                // caller (the lane slot is keccak(caller, bookId)), so an update only refers to
                // a BopAMM book when its caller is the BopAMM pricing module. Without this
                // filter, another venue's book that happens to share a book id is attributed to
                // BopAMM — emitting a stale override_block_timestamp or, for unknown ids, an
                // update against a non-existent component that fails indexing.
                if caller != config.module {
                    continue;
                }
                // Defensive: only emit for a book we actually track (e.g. a book configured
                // before the indexed range would not yet exist as a component).
                if components_store
                    .get_last(format!("book:{book_id}"))
                    .is_none()
                {
                    continue;
                }
                let id = component_id(&config.settlement, book_id);
                let builder = transaction_changes
                    .entry(transaction.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
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
    // Computed lazily on the first maker write so blocks without one do nothing.
    let mut books: Option<Vec<String>> = None;
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
                let books = books.get_or_insert_with(|| enumerate_books(components_store));
                if books.is_empty() {
                    continue;
                }
                let transaction: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(transaction.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
                for book in books.iter() {
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
///
/// A global `Unpaused` never resurrects a superseded book: those are dead markets that revert
/// `StaleUpdate` and must stay paused regardless of the venue-wide pause state.
fn extract_pause_state(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    // Computed lazily on the first pause/unpause log so blocks without one do nothing.
    let mut books: Option<Vec<String>> = None;
    let mut superseded: Option<HashSet<String>> = None;
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
        let books = books.get_or_insert_with(|| enumerate_books(components_store));
        if books.is_empty() {
            continue;
        }
        let superseded =
            superseded.get_or_insert_with(|| superseded_book_ids(components_store, &config.usdc));
        let tx: Transaction = log.receipt.transaction.into();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for book in books.iter() {
            // Keep superseded (dead) books paused across a global unpause.
            if !paused && superseded.contains(book) {
                continue;
            }
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
    // Computed lazily on the first builder that touches the module, then reused.
    let mut books: Option<Vec<String>> = None;
    for builder in transaction_changes.values_mut() {
        let touches_module = builder
            .changed_contracts()
            .any(|addr| addr == config.module.as_slice());
        if !touches_module {
            continue;
        }
        let books = books.get_or_insert_with(|| enumerate_books(components_store));
        for book in books.iter() {
            builder.mark_component_as_updated(book);
        }
    }
}
