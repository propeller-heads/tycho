#[path = "1_map_pool_created.rs"]
pub mod map_pool_created;

#[path = "2_store_euler_hooks.rs"]
pub mod store_euler_hooks;

#[path = "2_store_tokens_to_pool_id_angstrom.rs"]
pub mod store_tokens_to_pool_id_angstrom;

#[path = "3_store_pool_per_euler_hook.rs"]
pub mod store_pool_per_euler_hook;

#[path = "4_map_euler_enriched_protocol_changes.rs"]
pub mod map_euler_enriched_protocol_changes;

#[path = "4_map_angstrom_enriched_block_changes.rs"]
pub mod map_angstrom_enriched_block_changes;

#[path = "4_map_flaunch_enriched_protocol_changes.rs"]
pub mod map_flaunch_enriched_protocol_changes;

#[path = "5_map_protocol_changes.rs"]
pub mod map_protocol_changes;

#[cfg(test)]
mod tests;
