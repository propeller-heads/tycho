use substreams::store::{Appender, StoreAppend};

use tycho_substreams::prelude::*;

/// Maps each solver-facing underlying token to every Ring pool that uses it.
#[substreams::handlers::store]
pub fn store_token_pools(pools_created: BlockChanges, store: StoreAppend<String>) {
    for changes in pools_created.changes {
        for component in changes.component_changes {
            for token in component.tokens {
                store.append(0, hex::encode(token), component.id.clone());
            }
        }
    }
}
