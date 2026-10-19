use substreams_ethereum::pb::eth::v2::StorageChange;

use super::{BalanceDelta, EventTrait};
use crate::{
    abi::pool::events::Initialize,
    pb::tycho::evm::velodrome::Pool,
    storage::{constants::TRACKED_SLOTS, pool_storage::SlipstreamsPoolStorage},
};
use substreams_helper::storage_change::StorageChangesFilter;
use tycho_substreams::{models::Transaction, prelude::Attribute};

impl EventTrait for Initialize {
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
        let pool_storage = SlipstreamsPoolStorage::new(&filtered_storage_changes);

        let attributes =
            pool_storage.get_changed_attributes(TRACKED_SLOTS.to_vec().iter().collect());

        attributes
    }

    fn get_balance_delta(
        &self,
        _tx: &Transaction,
        _pool: &Pool,
        _ordinal: u64,
    ) -> Vec<BalanceDelta> {
        vec![]
    }
}
