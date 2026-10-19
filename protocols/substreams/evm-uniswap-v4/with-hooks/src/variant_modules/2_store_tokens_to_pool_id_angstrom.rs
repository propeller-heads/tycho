use substreams::store::{StoreNew, StoreSet, StoreSetString};
use substreams_helper::hex::Hexable;
use tycho_substreams::{models::ChangeType, prelude::*};

/// Generate a simple store key from two asset addresses
pub fn generate_store_key_from_assets(asset0: &[u8], asset1: &[u8]) -> String {
    format!("{}_{}", asset0.to_hex(), asset1.to_hex())
}
#[substreams::handlers::store]
pub fn store_tokens_to_pool_id_angstrom(
    ansgtrom_address: String,
    protocol_changes: BlockChanges,
    store: StoreSetString,
) {
    for tx_changes in protocol_changes.changes {
        for protocol_component in tx_changes.component_changes {
            if protocol_component.change == i32::from(ChangeType::Creation) {
                if let Some(hooks_attr) = protocol_component
                    .static_att
                    .iter()
                    .find(|attr| attr.name == "hooks")
                {
                    let hook_address = hooks_attr.value.to_hex();
                    // Check if this hook address is the only Angstrom hook
                    if hook_address.to_lowercase() == ansgtrom_address {
                        let store_key = generate_store_key_from_assets(
                            &protocol_component.tokens[0],
                            &protocol_component.tokens[1],
                        );
                        store.set(0, &store_key, &protocol_component.id);
                    }
                }
            }
        }
    }
}
