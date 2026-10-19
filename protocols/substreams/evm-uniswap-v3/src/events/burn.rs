use substreams_ethereum::pb::eth::v2::StorageChange;
use substreams_helper::storage_change::StorageChangesFilter;

use crate::{
    abi::pool::events::Burn,
    pb::uniswap::v3::Pool,
    storage::{constants::TRACKED_SLOTS, pool_storage::UniswapPoolStorage},
};

use super::{BalanceDelta, EventTrait};
use tycho_substreams::prelude::Attribute;

impl EventTrait for Burn {
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        pool_address: &[u8; 20],
    ) -> Vec<Attribute> {
        let storage_vec = storage_changes.to_vec();

        let filtered_storage_changes = storage_vec
            .filter_by_address(pool_address)
            .into_iter()
            .cloned()
            .collect();

        let pool_storage = UniswapPoolStorage::new(&filtered_storage_changes);

        let mut changed_attributes =
            pool_storage.get_changed_attributes(TRACKED_SLOTS.to_vec().iter().collect());

        let changed_ticks =
            pool_storage.get_ticks_changes(vec![&self.tick_upper, &self.tick_lower]);

        changed_attributes.extend(changed_ticks);

        changed_attributes
    }

    fn get_balance_delta(&self, _pool: &Pool, _ordinal: u64) -> Vec<BalanceDelta> {
        // Burn event balances deltas are accounted for by the Collect event.
        // In the case of a burn, the Collect event amounts will include both the burned amount and
        // the fees earned.
        vec![]
    }
}
