use substreams::store::{StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth;

use crate::{
    common::{is_zero, slot_key},
    config::DeploymentConfig,
};

/// Tracks the current treasury (inventory custodian) from writes to TesseraSwap's treasury
/// slot.
///
/// Uses `set` (not `set_if_not_exists`) so a treasury rotation propagates immediately. The
/// constructor's initial write is a storage change of the creation call, so the first value is
/// picked up at deployment. The treasury has rotated once on Base (block 37,737,344).
#[substreams::handlers::store]
pub fn store_treasury(params: String, block: eth::v2::Block, store: StoreSetString) {
    let config: DeploymentConfig = serde_qs::from_str(&params).expect("invalid params");
    let treasury_slot = slot_key(config.treasury_slot);
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address == config.tesseraswap &&
                    change.key == treasury_slot &&
                    !is_zero(&change.new_value)
                {
                    if let Some(treasury) = change.new_value.get(12..32) {
                        store.set(change.ordinal, "treasury", &hex::encode(treasury));
                    }
                }
            }
        }
    }
}
