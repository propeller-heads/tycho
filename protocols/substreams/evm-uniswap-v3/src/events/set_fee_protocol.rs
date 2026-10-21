use substreams_ethereum::pb::eth::v2::StorageChange;

use crate::{
    abi::pool::events::SetFeeProtocol,
    pb::uniswap::v3::Pool,
    storage::{constants::TRACKED_SLOTS, pool_storage::UniswapPoolStorage},
};
use substreams_helper::storage_change::StorageChangesFilter;
use tycho_substreams::prelude::Attribute;

use super::{BalanceDelta, EventTrait};

impl EventTrait for SetFeeProtocol {
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

        pool_storage.get_changed_attributes(TRACKED_SLOTS.to_vec().iter().collect())
    }

    fn get_balance_delta(&self, _pool: &Pool, _ordinal: u64) -> Vec<BalanceDelta> {
        vec![]
    }
}
