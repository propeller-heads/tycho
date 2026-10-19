use crate::{
    abi::pool::events::Flash,
    pb::tycho::evm::velodrome::Pool,
    storage::{constants::TRACKED_SLOTS, pool_storage::SlipstreamsPoolStorage},
};
use substreams_ethereum::pb::eth::v2::StorageChange;
use substreams_helper::{hex::Hexable, storage_change::StorageChangesFilter};
use tycho_substreams::{models::Transaction, prelude::Attribute};

use super::{BalanceDelta, EventTrait};

impl EventTrait for Flash {
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

        pool_storage.get_changed_attributes(TRACKED_SLOTS.to_vec().iter().collect())
    }

    fn get_balance_delta(&self, tx: &Transaction, pool: &Pool, ordinal: u64) -> Vec<BalanceDelta> {
        let changed_balance = vec![
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: pool.token0.clone(),
                delta: self.paid0.clone().to_signed_bytes_be(),
                component_id: pool
                    .address
                    .clone()
                    .to_hex()
                    .as_bytes()
                    .to_vec(),
            },
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: pool.token1.clone(),
                delta: self.paid1.clone().to_signed_bytes_be(),
                component_id: pool
                    .address
                    .clone()
                    .to_hex()
                    .as_bytes()
                    .to_vec(),
            },
        ];
        changed_balance
    }
}
