use substreams_ethereum::pb::eth::v2::TransactionTrace;

use crate::pb::pancakeswap::infinity::bin::Transaction;

#[path = "1_map_pool_created.rs"]
pub mod map_pool_created;

#[path = "2_store_pools.rs"]
pub mod store_pools;

#[path = "3_map_events.rs"]
pub mod map_events;

#[path = "4_store_active_id.rs"]
pub mod store_active_id;

#[path = "4_map_tree_changes.rs"]
pub mod map_tree_changes;

#[path = "5_store_bin_trees.rs"]
pub mod store_bin_trees;

#[path = "6_map_bin_changes.rs"]
pub mod map_bin_changes;

#[path = "6_map_store_balance_changes.rs"]
pub mod map_store_balance_changes;

#[path = "7_store_bin_reserves.rs"]
pub mod store_bin_reserves;

#[path = "8_map_protocol_changes.rs"]
pub mod map_protocol_changes;

impl From<TransactionTrace> for Transaction {
    fn from(value: TransactionTrace) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index.into() }
    }
}

impl From<&TransactionTrace> for Transaction {
    fn from(value: &TransactionTrace) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index.into(),
        }
    }
}

impl From<Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: Transaction) -> Self {
        Self { hash: value.hash, from: value.from, to: value.to, index: value.index }
    }
}

impl From<&Transaction> for tycho_substreams::prelude::Transaction {
    fn from(value: &Transaction) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_stores {
    use substreams::store::StoreGet;

    /// Raw store holding `(ordinal, key, value)` writes; `get_at` returns the latest at or below.
    pub struct FakeRawStore(pub Vec<(u64, String, Vec<u8>)>);

    impl StoreGet<Vec<u8>> for FakeRawStore {
        fn new(_idx: u32) -> Self {
            Self(Vec::new())
        }

        fn get_at<K: AsRef<str>>(&self, ord: u64, key: K) -> Option<Vec<u8>> {
            self.0
                .iter()
                .filter(|(written_at, written_key, _)| {
                    *written_at <= ord && written_key == key.as_ref()
                })
                .max_by_key(|(written_at, _, _)| *written_at)
                .map(|(_, _, value)| value.clone())
        }

        fn get_last<K: AsRef<str>>(&self, _key: K) -> Option<Vec<u8>> {
            unimplemented!("tests only read at an ordinal")
        }

        fn get_first<K: AsRef<str>>(&self, _key: K) -> Option<Vec<u8>> {
            unimplemented!("tests only read at an ordinal")
        }

        fn has_at<K: AsRef<str>>(&self, _ord: u64, _key: K) -> bool {
            unimplemented!("tests only read at an ordinal")
        }

        fn has_last<K: AsRef<str>>(&self, _key: K) -> bool {
            unimplemented!("tests only read at an ordinal")
        }

        fn has_first<K: AsRef<str>>(&self, _key: K) -> bool {
            unimplemented!("tests only read at an ordinal")
        }
    }
}
