use crate::storage::{
    constants::{
        DEX_V2_TICK_DATA_MAPPING_SLOT, DEX_V2_TOKEN_RESERVES_MAPPING_SLOT, DEX_V2_VARIABLES2_SLOT,
        DEX_V2_VARIABLES_SLOT,
    },
    storage_view::{StorageChangesView, StorageLocation},
    utils::{
        double_mapping_slot, int256_be32_from_bigint, triple_mapping_slot, u256_be32_from_u64,
    },
};
use substreams::scalar::BigInt;
use tycho_substreams::prelude::Attribute;

pub fn dex_variables_attributes(
    storage_view: &StorageChangesView,
    dex_id: &[u8; 32],
    dex_type: u64,
) -> Vec<Attribute> {
    let locations = dex_variables_locations(dex_id, dex_type);
    storage_view.get_changed_attributes(&locations)
}

fn dex_variables_locations(dex_id: &[u8; 32], dex_type: u64) -> Vec<StorageLocation> {
    let dex_type_be32 = u256_be32_from_u64(dex_type);
    let dex_variables_slot = double_mapping_slot(DEX_V2_VARIABLES_SLOT, &dex_type_be32, dex_id);
    let dex_variables2_slot = double_mapping_slot(DEX_V2_VARIABLES2_SLOT, &dex_type_be32, dex_id);

    vec![
        StorageLocation {
            name: "dex_variables".to_string(),
            slot: dex_variables_slot,
            offset: 0,
            number_of_bytes: 32,
            signed: false,
        },
        StorageLocation {
            name: "dex_variables2".to_string(),
            slot: dex_variables2_slot,
            offset: 0,
            number_of_bytes: 32,
            signed: false,
        },
    ]
}

pub fn dex_variables2_attributes(
    storage_view: &StorageChangesView,
    dex_id: &[u8; 32],
    dex_type: u64,
) -> Vec<Attribute> {
    let dex_type_be32 = u256_be32_from_u64(dex_type);
    let dex_variables2_slot = double_mapping_slot(DEX_V2_VARIABLES2_SLOT, &dex_type_be32, dex_id);
    let locations = [StorageLocation {
        name: "dex_variables2".to_string(),
        slot: dex_variables2_slot,
        offset: 0,
        number_of_bytes: 32,
        signed: false,
    }];
    storage_view.get_changed_attributes(&locations)
}

pub fn tick_data_attributes(
    storage_view: &StorageChangesView,
    dex_id: &[u8; 32],
    dex_type: u64,
    ticks_idx: Vec<&BigInt>,
) -> Vec<Attribute> {
    let locations = tick_data_locations(dex_id, dex_type, ticks_idx);
    storage_view.get_changed_attributes(&locations)
}

fn tick_data_locations(
    dex_id: &[u8; 32],
    dex_type: u64,
    ticks_idx: Vec<&BigInt>,
) -> Vec<StorageLocation> {
    let dex_type_be32 = u256_be32_from_u64(dex_type);
    let mut locations = Vec::new();

    let mut tick_names = Vec::new();
    for tick_idx in ticks_idx.iter() {
        tick_names.push(format!("ticks/{tick_idx}"));
    }

    for (tick_idx, tick_name) in ticks_idx.iter().zip(tick_names.iter()) {
        let tick_key = int256_be32_from_bigint(tick_idx);
        let base_slot =
            triple_mapping_slot(DEX_V2_TICK_DATA_MAPPING_SLOT, &dex_type_be32, dex_id, &tick_key);

        locations.push(StorageLocation {
            name: format!("{tick_name}/net-liquidity"),
            slot: base_slot,
            offset: 0,
            number_of_bytes: 32,
            signed: true,
        });
    }

    locations
}

pub fn token_reserves_attributes(
    storage_view: &StorageChangesView,
    dex_id: &[u8; 32],
    dex_type: u64,
) -> Vec<Attribute> {
    let locations = token_reserves_locations(dex_id, dex_type);
    storage_view.get_changed_attributes(&locations)
}

fn token_reserves_locations(dex_id: &[u8; 32], dex_type: u64) -> Vec<StorageLocation> {
    let dex_type_be32 = u256_be32_from_u64(dex_type);
    let slot = double_mapping_slot(DEX_V2_TOKEN_RESERVES_MAPPING_SLOT, &dex_type_be32, dex_id);

    vec![
        StorageLocation {
            name: "token0/token_reserves".to_string(),
            slot,
            offset: 0,
            number_of_bytes: 16,
            signed: false,
        },
        StorageLocation {
            name: "token1/token_reserves".to_string(),
            slot,
            offset: 16,
            number_of_bytes: 16,
            signed: false,
        },
    ]
}
