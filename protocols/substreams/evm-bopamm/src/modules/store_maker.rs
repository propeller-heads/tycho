use substreams::store::{StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth;

use crate::{common::is_zero, config::DeploymentConfig};

/// Tracks the current global maker wallet from writes to the module's maker slot.
///
/// Uses `set` (not `set_if_not_exists`) so a maker rotation propagates immediately.
#[substreams::handlers::store]
pub fn store_maker(params: String, block: eth::v2::Block, store: StoreSetString) {
    let config: DeploymentConfig = serde_qs::from_str(&params).expect("invalid params");
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address == config.module &&
                    change.key == config.maker_slot &&
                    !is_zero(&change.new_value)
                {
                    if let Some(maker) = change.new_value.get(12..32) {
                        store.set(change.ordinal, "maker", &hex::encode(maker));
                    }
                }
            }
        }
    }
}
