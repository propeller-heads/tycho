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
    common::{address_from_word, all_pairs, is_zero, pair_store_key, slot_key, EIP1967_IMPL_SLOT},
    config::DeploymentConfig,
};

/// TesseraSwap `slot0` — the engine address; a write is an engine hot-swap.
const ENGINE_SLOT: u64 = 0;

/// Aggregates components, tracked-contract storage, balances and re-simulation markers into
/// the final `BlockChanges`.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    grouped_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    components_store: StoreGetProto<ProtocolComponent>,
    pairs_store: StoreGetString,
    treasury_store: StoreGetString,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    // The params fallback covers runs whose initial block is patched past the constructor
    // write (the testing harness); a real sync always has the store populated.
    let treasury = treasury_store
        .get_last("treasury")
        .and_then(|t| hex::decode(t).ok())
        .unwrap_or_else(|| config.treasury.clone());

    add_new_components(&grouped_components, &treasury, &mut transaction_changes);

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

    // Full storage + code of the venue contracts. The set is dynamic: the stable addresses and
    // the code-only satellites come from params; each pair contract is discovered at component
    // creation and resolved through the components store (visible in-block, so a pair's
    // creation code and init storage are captured in its creation transaction).
    let tracked = config.tracked_addresses();
    extract_contract_changes_builder(
        &block,
        |addr| {
            addr == config.tesseraswap.as_slice() ||
                addr == config.engine.as_slice() ||
                tracked
                    .iter()
                    .any(|t| t.as_slice() == addr) ||
                components_store
                    .get_last(pair_store_key(addr))
                    .is_some()
        },
        &mut transaction_changes,
    );

    extract_treasury_changes(
        &block,
        &config,
        &components_store,
        &pairs_store,
        &mut transaction_changes,
    );
    extract_admin_mutations(
        &block,
        &config,
        &components_store,
        &pairs_store,
        &mut transaction_changes,
    );
    mark_pairs_updated(&config, &components_store, &pairs_store, &mut transaction_changes);

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

/// Adds newly created pair components and their default dynamic attributes.
fn add_new_components(
    grouped_components: &BlockTransactionProtocolComponents,
    treasury: &[u8],
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for tx_component in &grouped_components.tx_components {
        let tx = tx_component.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &tx_component.components {
            builder.add_protocol_component(component);
            builder.add_entity_change(&EntityChanges {
                component_id: component.id.clone(),
                attributes: vec![
                    Attribute {
                        name: "update_marker".to_string(),
                        value: vec![1u8],
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "balance_owner".to_string(),
                        value: treasury.to_vec(),
                        change: ChangeType::Creation.into(),
                    },
                ],
            });
        }
    }
}

/// Refreshes the `balance_owner` attribute on every pair whenever TesseraSwap's treasury slot
/// is written (rotation — observed once on Base, block 37,737,344).
fn extract_treasury_changes(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    pairs_store: &StoreGetString,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let treasury_slot = slot_key(config.treasury_slot);
    // Computed lazily on the first treasury write so blocks without one do nothing.
    let mut pairs: Option<Vec<String>> = None;
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.tesseraswap ||
                    change.key != treasury_slot ||
                    is_zero(&change.new_value)
                {
                    continue;
                }
                let treasury = address_from_word(&change.new_value);
                let pairs = pairs.get_or_insert_with(|| {
                    all_pairs(components_store, pairs_store)
                        .into_iter()
                        .map(|c| c.id)
                        .collect()
                });
                if pairs.is_empty() {
                    continue;
                }
                let transaction: Transaction = tx.into();
                let builder = transaction_changes
                    .entry(transaction.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
                for pair in pairs.iter() {
                    builder.add_entity_change(&EntityChanges {
                        component_id: pair.clone(),
                        attributes: vec![Attribute {
                            name: "balance_owner".to_string(),
                            value: treasury.clone(),
                            change: ChangeType::Update.into(),
                        }],
                    });
                    builder.mark_component_as_updated(pair);
                }
            }
        }
    }
}

/// Surfaces the admin mutations that can silently break simulation, as attributes for
/// monitoring to alert on (the runbook then adds the new address to `params` and re-releases
/// the spkg — see HANDOVER §9.3):
///
/// * an engine hot-swap (TesseraSwap `slot0` write) → `engine` attribute on every pair;
/// * a pair implementation upgrade (EIP-1967 slot write on a tracked pair) → `pair_impl` attribute
///   on that pair;
/// * a pricing-lib (re)assignment (pair lib-slot write) → `pair_lib` attribute on that pair — a lib
///   generation not in the `tracked` params would otherwise break that pair with no signal.
fn extract_admin_mutations(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    pairs_store: &StoreGetString,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let engine_slot = slot_key(ENGINE_SLOT);
    let lib_slot = slot_key(config.pair_lib_slot);
    let mut pairs: Option<Vec<String>> = None;
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address == config.tesseraswap &&
                    change.key == engine_slot &&
                    !is_zero(&change.old_value)
                {
                    // old_value == 0 is the constructor write, before any pair exists.
                    let engine = address_from_word(&change.new_value);
                    let pairs = pairs.get_or_insert_with(|| {
                        all_pairs(components_store, pairs_store)
                            .into_iter()
                            .map(|c| c.id)
                            .collect()
                    });
                    let transaction: Transaction = tx.into();
                    let builder = transaction_changes
                        .entry(transaction.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
                    for pair in pairs.iter() {
                        builder.add_entity_change(&EntityChanges {
                            component_id: pair.clone(),
                            attributes: vec![Attribute {
                                name: "engine".to_string(),
                                value: engine.clone(),
                                change: ChangeType::Update.into(),
                            }],
                        });
                        builder.mark_component_as_updated(pair);
                    }
                } else if change.key == EIP1967_IMPL_SLOT.as_slice() && !is_zero(&change.old_value)
                {
                    // old_value == 0 is pair init, already covered by component creation.
                    emit_pair_attribute(
                        "pair_impl",
                        change,
                        tx,
                        components_store,
                        transaction_changes,
                    );
                } else if change.key == lib_slot && !is_zero(&change.new_value) {
                    // Includes the post-creation first assignment: the lib is not part of the
                    // component's static attributes, so every write is surfaced.
                    emit_pair_attribute(
                        "pair_lib",
                        change,
                        tx,
                        components_store,
                        transaction_changes,
                    );
                }
            }
        }
    }
}

/// Emits a monitoring attribute (an address-valued update) on the pair the storage change
/// belongs to, and marks it for re-simulation. No-op for addresses that are not a known pair.
fn emit_pair_attribute(
    name: &str,
    change: &eth::v2::StorageChange,
    tx: &eth::v2::TransactionTrace,
    components_store: &StoreGetProto<ProtocolComponent>,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let Some(component) = components_store.get_last(pair_store_key(&change.address)) else {
        return;
    };
    let transaction: Transaction = tx.into();
    let builder = transaction_changes
        .entry(transaction.index)
        .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
    builder.add_entity_change(&EntityChanges {
        component_id: component.id.clone(),
        attributes: vec![Attribute {
            name: name.to_string(),
            value: address_from_word(&change.new_value),
            change: ChangeType::Update.into(),
        }],
    });
    builder.mark_component_as_updated(&component.id);
}

/// Marks pairs for re-simulation from tracked-contract changes: a change to a shared contract
/// (TesseraSwap, engine, any code-only satellite) marks every pair; a change to a pair
/// contract marks that pair. Prices post into each pair every block, so in steady state every
/// pair re-simulates every block — that is the venue repricing, not noise.
fn mark_pairs_updated(
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    pairs_store: &StoreGetString,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let tracked = config.tracked_addresses();
    let mut pairs: Option<Vec<String>> = None;
    for builder in transaction_changes.values_mut() {
        let mut mark_all = false;
        let mut mark_ids: Vec<String> = Vec::new();
        for addr in builder.changed_contracts() {
            if addr == config.tesseraswap.as_slice() ||
                addr == config.engine.as_slice() ||
                tracked
                    .iter()
                    .any(|t| t.as_slice() == addr)
            {
                mark_all = true;
                break;
            }
            if let Some(component) = components_store.get_last(pair_store_key(addr)) {
                mark_ids.push(component.id);
            }
        }
        if mark_all {
            let pairs = pairs.get_or_insert_with(|| {
                all_pairs(components_store, pairs_store)
                    .into_iter()
                    .map(|c| c.id)
                    .collect()
            });
            for pair in pairs.iter() {
                builder.mark_component_as_updated(pair);
            }
        } else {
            for id in mark_ids {
                builder.mark_component_as_updated(&id);
            }
        }
    }
}
