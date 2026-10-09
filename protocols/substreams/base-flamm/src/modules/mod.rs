// Copyright (c) 2026 Everlong Labs Limited
//! One substreams handler per file, in module-graph order.
#[path = "3_map_components.rs"]
mod map_components;
#[path = "5_map_protocol_changes.rs"]
mod map_protocol_changes;
#[path = "1_store_deployments.rs"]
mod store_deployments;
#[path = "4_store_pools.rs"]
mod store_pools;
#[path = "2_store_words.rs"]
mod store_words;

pub use map_components::{components_in_block, map_components};
pub use map_protocol_changes::{map_protocol_changes, protocol_changes};
pub use store_deployments::store_deployments;
pub use store_pools::{pool_key, pools_key, store_pools};
pub use store_words::{store_words, tracked_writes};
