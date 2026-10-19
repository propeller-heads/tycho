use crate::{
    abi::angstrom::{BatchUpdatePools, PoolConfigured, PoolRemoved},
    pb::uniswap::v4::angstrom::AngstromConfig,
    store_tokens_to_pool_id_angstrom::generate_store_key_from_assets,
};
use std::collections::HashMap;
use substreams::{prelude::StoreGetString, store::StoreGet};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

use ethabi::ethereum_types::Address;
use std::str::FromStr;
use substreams_helper::event_handler::EventHandler;

use anyhow::{anyhow, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Params {
    pub controller_address: String,
    pub angstrom_address: String,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }
}

#[substreams::handlers::map]
pub fn map_angstrom_enriched_block_changes(
    params: String,
    block: eth::Block,
    tokens_to_id_store: StoreGetString,
    block_changes: BlockChanges,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let enriched_changes = enrich_block_changes(
        params.controller_address,
        block,
        params.angstrom_address,
        tokens_to_id_store,
        block_changes,
    );

    Ok(enriched_changes)
}

pub fn enrich_block_changes(
    controller_address: String,
    block: eth::Block,
    angstrom_address: String,
    tokens_to_id_store: StoreGetString,
    mut protocol_changes: BlockChanges,
) -> BlockChanges {
    let angstrom_configs = track_angstrom_config(controller_address, block, tokens_to_id_store);
    // Process each transaction's changes
    for tx_changes in &mut protocol_changes.changes {
        // Enrich component creations with Angstrom hook identifier
        for protocol_component in &mut tx_changes.component_changes {
            if protocol_component.change == i32::from(ChangeType::Creation) {
                // Check if this component has a hooks attribute
                if let Some(hooks_attr) = protocol_component
                    .static_att
                    .iter()
                    .find(|attr| attr.name == "hooks")
                {
                    let hook_address = hooks_attr.value.to_hex();
                    // Check if this hook address is the only Angstrom hook
                    if hook_address.to_lowercase() == angstrom_address {
                        protocol_component
                            .static_att
                            .push(Attribute {
                                name: "hook_identifier".to_string(),
                                value: "angstrom_v1".as_bytes().to_vec(),
                                change: ChangeType::Creation.into(),
                            });
                    }
                }
            }
        }

        let tx_hash = &tx_changes
            .tx
            .as_ref()
            .expect("Transaction not set in TransactionChanges")
            .hash;

        // Get all angstrom configs for this transaction
        if let Some(tx_configs) = angstrom_configs.get(tx_hash) {
            // First pass: update existing entity changes
            for entity_change in &mut tx_changes.entity_changes {
                if let Some(config) = tx_configs.get(&entity_change.component_id) {
                    entity_change
                        .attributes
                        .extend(create_angstrom_attributes(config));
                }
            }

            // Second pass: collect new entity changes needed
            let existing_component_ids: std::collections::HashSet<_> = tx_changes
                .entity_changes
                .iter()
                .map(|ec| &ec.component_id)
                .collect();

            let new_entity_changes: Vec<_> = tx_configs
                .iter()
                .filter(|(component_id, _)| !existing_component_ids.contains(component_id))
                .map(|(component_id, config)| EntityChanges {
                    component_id: component_id.clone(),
                    attributes: create_angstrom_attributes(config),
                })
                .collect();

            // Add the new entity changes
            tx_changes
                .entity_changes
                .extend(new_entity_changes);
        }
    }

    protocol_changes
}

fn track_angstrom_config(
    controller_address: String,
    block: eth::Block,
    tokens_to_id_store: StoreGetString,
) -> HashMap<Vec<u8>, HashMap<String, AngstromConfig>> {
    let mut config = HashMap::new();

    // Process batchUpdatePools calls first
    for tx in block.transactions() {
        for call in &tx.calls {
            if call.state_reverted {
                continue;
            }

            let call_address = call.address.to_hex().to_lowercase();
            if call_address == controller_address {
                if let Ok(batch_update) = BatchUpdatePools::decode_call(&call.input) {
                    for pool_update in batch_update.updates {
                        let store_key = generate_store_key_from_assets(
                            &pool_update.asset_a,
                            &pool_update.asset_b,
                        );

                        if let Some(component_id) = tokens_to_id_store.get_last(&store_key) {
                            let angstrom_config = AngstromConfig {
                                bundle_fee: pool_update.bundle_fee,
                                unlocked_fee: pool_update.unlocked_fee,
                                protocol_unlocked_fee: pool_update.protocol_unlocked_fee,
                                pool_removed: false,
                            };
                            config
                                .entry(tx.hash.clone())
                                .or_insert_with(HashMap::new)
                                .insert(component_id, angstrom_config);
                        }
                    }
                }
            }
        }
    }

    // Use block scope to avoid borrow checker issues
    {
        // Create closure for PoolConfigured events
        let mut on_pool_configured =
            |event: PoolConfigured, tx: &eth::TransactionTrace, _log: &eth::Log| {
                let store_key = generate_store_key_from_assets(&event.asset0, &event.asset1);

                let component_id = tokens_to_id_store
                    .get_last(store_key.clone())
                    .expect("Component ID should exist for Angstrom pool assets store");

                let angstrom_config = AngstromConfig {
                    bundle_fee: event.bundle_fee.clone(),
                    unlocked_fee: event.unlocked_fee.clone(),
                    protocol_unlocked_fee: event.protocol_unlocked_fee.clone(),
                    pool_removed: false,
                };
                config
                    .entry(tx.hash.clone())
                    .or_insert_with(HashMap::new)
                    .insert(component_id, angstrom_config);
            };

        let mut eh = EventHandler::new(&block);
        eh.filter_by_address(vec![Address::from_str(&controller_address).unwrap()]);
        eh.on::<PoolConfigured, _>(&mut on_pool_configured);
        eh.handle_events();
    }

    // Handle PoolRemoved events in separate scope
    {
        let mut on_pool_removed =
            |event: PoolRemoved, tx: &eth::TransactionTrace, _log: &eth::Log| {
                let store_key = generate_store_key_from_assets(&event.asset0, &event.asset1);

                if let Some(component_id) = tokens_to_id_store.get_last(&store_key) {
                    let angstrom_config = AngstromConfig {
                        bundle_fee: vec![],            // Empty since pool is removed
                        unlocked_fee: vec![],          // Empty since pool is removed
                        protocol_unlocked_fee: vec![], // Empty since pool is removed
                        pool_removed: true,
                    };

                    config
                        .entry(tx.hash.clone())
                        .or_insert_with(HashMap::new)
                        .insert(component_id.clone(), angstrom_config);
                }
            };

        let mut eh = EventHandler::new(&block);
        eh.filter_by_address(vec![Address::from_str(&controller_address).unwrap()]);
        eh.on::<PoolRemoved, _>(&mut on_pool_removed);
        eh.handle_events();
    }

    config
}

fn create_angstrom_attributes(config: &AngstromConfig) -> Vec<Attribute> {
    vec![
        Attribute {
            name: "angstrom_unlocked_fee".to_string(),
            value: config.unlocked_fee.clone(),
            change: ChangeType::Update.into(),
        },
        Attribute {
            name: "angstrom_protocol_unlocked_fee".to_string(),
            value: config.protocol_unlocked_fee.clone(),
            change: ChangeType::Update.into(),
        },
        Attribute {
            name: "angstrom_removed_pool".to_string(),
            value: if config.pool_removed { vec![1] } else { vec![0] },
            change: ChangeType::Update.into(),
        },
    ]
}
