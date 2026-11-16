use substreams::prelude::BigInt;
use substreams_ethereum::pb::eth::v2::StorageChange;

use super::{BalanceDelta, EventTrait};
use crate::{
    abi::pool::events::IncreaseObservationCardinalityNext,
    pb::tycho::evm::aerodrome::Pool,
    storage::{constants::TRACKED_SLOTS, pool_storage::SlipstreamsPoolStorage},
};
use substreams_helper::storage_change::StorageChangesFilter;
use tycho_substreams::{models::Transaction, prelude::Attribute};

impl EventTrait for IncreaseObservationCardinalityNext {
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
        let mut attributes =
            pool_storage.get_changed_attributes(TRACKED_SLOTS.to_vec().iter().collect());

        let big_ints: Vec<BigInt> = (self
            .observation_cardinality_next_old
            .to_u64()..=
            self.observation_cardinality_next_new
                .to_u64())
            .map(BigInt::from)
            .collect();

        let observations_updated_range: Vec<&BigInt> = big_ints.iter().collect();
        attributes.extend(pool_storage.get_observations_changes(observations_updated_range));
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
