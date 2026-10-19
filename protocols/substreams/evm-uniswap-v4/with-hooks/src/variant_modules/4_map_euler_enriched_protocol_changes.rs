use std::str::FromStr;

use ethabi::ethereum_types::Address;
use evm_uniswap_v4_shared::abi::euler_swap_factory::events::PoolUninstalled;
use substreams::store::{StoreGet, StoreGetInt64, StoreGetString};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};
use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_euler_enriched_protocol_changes(
    params: String,
    block: eth::Block,
    protocol_changes: BlockChanges,
    euler_hooks_store: StoreGetInt64,
    euler_pools_per_hook_store: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let euler_factory_address = params.as_str();

    let enriched_changes = _enrich_protocol_changes(
        &block,
        euler_factory_address,
        protocol_changes,
        &euler_hooks_store,
        &euler_pools_per_hook_store,
    );

    Ok(enriched_changes)
}

pub fn _enrich_protocol_changes(
    block: &eth::Block,
    euler_factory_address: &str,
    mut protocol_changes: BlockChanges,
    euler_hooks_store: &StoreGetInt64,
    euler_pools_per_hook_store: &StoreGetString,
) -> BlockChanges {
    // Process each transaction's changes
    for tx_changes in &mut protocol_changes.changes {
        // Enrich component creations with Euler hook identifier
        for component_change in &mut tx_changes.component_changes {
            if component_change.change == i32::from(ChangeType::Creation) {
                // Check if this component has a hooks attribute
                if let Some(hooks_attr) = component_change
                    .static_att
                    .iter()
                    .find(|attr| attr.name == "hooks")
                {
                    let hook_address = hooks_attr.value.to_hex();
                    substreams::log::debug!("Hook address: {}", hook_address);

                    // Check if this hook address is a known Euler hook
                    if euler_hooks_store
                        .get_last(&hook_address)
                        .is_some()
                    {
                        // Add the hook_identifier static attribute
                        component_change
                            .static_att
                            .push(Attribute {
                                name: "hook_identifier".to_string(),
                                value: "euler_v1".as_bytes().to_vec(),
                                change: ChangeType::Creation.into(),
                            });
                    }
                }
            }
        }
    }

    // Process PoolUninstalled events to add paused attributes
    handle_pool_uninstalled_events(
        block,
        euler_factory_address,
        &mut protocol_changes,
        euler_pools_per_hook_store,
    );

    protocol_changes
}

fn handle_pool_uninstalled_events(
    block: &eth::Block,
    euler_factory_address: &str,
    enriched_changes: &mut BlockChanges,
    euler_pools_per_hook_store: &StoreGetString,
) {
    // Collect uninstalled hooks with their transaction hashes
    let mut uninstalled_hooks: Vec<(String, Vec<u8>)> = Vec::new();

    {
        let mut on_pool_uninstalled =
            |event: PoolUninstalled, tx: &eth::TransactionTrace, _log: &eth::Log| {
                let uninstalled_hook = event.pool.to_hex();
                uninstalled_hooks.push((uninstalled_hook, tx.hash.clone()));
            };

        let mut eh = EventHandler::new(block);
        eh.filter_by_address(vec![Address::from_str(euler_factory_address).unwrap()]);
        eh.on::<PoolUninstalled, _>(&mut on_pool_uninstalled);
        eh.handle_events();
    }

    // Now process the uninstalled hooks
    for (uninstalled_hook, tx_hash) in uninstalled_hooks {
        let hook_key = format!("hook:{}", uninstalled_hook);
        if let Some(pool_id) = euler_pools_per_hook_store.get_last(&hook_key) {
            // Find the specific transaction that contains the PoolUninstalled event
            let mut found_tx = false;
            for tx_changes in &mut enriched_changes.changes {
                if let Some(ref tx) = tx_changes.tx {
                    if tx.hash == tx_hash {
                        // Found the transaction, add the paused entity change
                        tx_changes
                            .entity_changes
                            .push(EntityChanges {
                                component_id: pool_id.clone(),
                                attributes: vec![Attribute {
                                    name: "paused".to_string(),
                                    value: vec![1u8], // true as a single byte
                                    change: ChangeType::Update.into(),
                                }],
                            });
                        found_tx = true;
                        break;
                    }
                }
            }

            // If transaction not found in existing changes, create it
            if !found_tx {
                let block_tx = block
                    .transactions()
                    .find(|tx| tx.hash == tx_hash)
                    .expect("Transaction not found in block");
                let new_tx: Transaction = block_tx.into();

                let mut new_tx_changes =
                    TransactionChanges { tx: Some(new_tx), ..Default::default() };
                new_tx_changes
                    .entity_changes
                    .push(EntityChanges {
                        component_id: pool_id,
                        attributes: vec![Attribute {
                            name: "paused".to_string(),
                            value: vec![1u8], // true as a single byte
                            change: ChangeType::Update.into(),
                        }],
                    });
                enriched_changes
                    .changes
                    .push(new_tx_changes);
            }
        }
    }
}

// Extracted version for testing - takes a generic store getter
// Returns the pool ID that would be affected by hook uninstallation
pub fn _handle_pool_uninstalled_events<T: StoreGet<String>>(
    uninstalled_hooks: Vec<String>,
    pools_per_hook_store: &T,
) -> Vec<(String, String)> {
    let mut results = Vec::new();

    for uninstalled_hook in uninstalled_hooks {
        let hook_key = format!("hook:{}", uninstalled_hook);
        if let Some(pool_id) = pools_per_hook_store.get_last(&hook_key) {
            // Now we store only one pool per hook with set_if_not_exists
            results.push((uninstalled_hook, pool_id));
        }
    }

    results
}
