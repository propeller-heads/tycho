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

#[path = "7_map_store_balance_changes.rs"]
pub mod map_store_balance_changes;

#[path = "7_store_bin_reserves.rs"]
pub mod store_bin_reserves;

#[path = "8_map_protocol_changes.rs"]
pub mod map_protocol_changes;

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

    /// Store holding `(ordinal, key, value)` writes; `get_at` returns the latest at or below.
    pub struct FakeStore<V>(pub Vec<(u64, String, V)>);

    /// The raw-bytes store the tree and reserve modules read.
    pub type FakeRawStore = FakeStore<Vec<u8>>;

    impl<V: Clone> StoreGet<V> for FakeStore<V> {
        fn new(_idx: u32) -> Self {
            Self(Vec::new())
        }

        fn get_at<K: AsRef<str>>(&self, ord: u64, key: K) -> Option<V> {
            self.0
                .iter()
                .filter(|(written_at, written_key, _)| {
                    *written_at <= ord && written_key == key.as_ref()
                })
                .max_by_key(|(written_at, _, _)| *written_at)
                .map(|(_, _, value)| value.clone())
        }

        fn get_last<K: AsRef<str>>(&self, _key: K) -> Option<V> {
            unimplemented!("tests only read at an ordinal")
        }

        fn get_first<K: AsRef<str>>(&self, _key: K) -> Option<V> {
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

/// The one pool, pool manager and event shapes every module's tests use.
#[cfg(test)]
pub(crate) mod test_fixtures {
    use crate::pb::pancakeswap::infinity::bin::{
        events::{
            pool_event::{self, Type},
            PoolEvent,
        },
        Transaction,
    };

    pub const POOL_ID: [u8; 32] = [0xab; 32];
    pub const POOL_MANAGER: [u8; 20] = [0xc6; 20];

    /// An event of `POOL_ID` in transaction 0.
    pub fn event(kind: Type, ordinal: u64) -> PoolEvent {
        event_in_tx(kind, ordinal, 0)
    }

    pub fn event_in_tx(kind: Type, ordinal: u64, tx_index: u64) -> PoolEvent {
        PoolEvent {
            log_ordinal: ordinal,
            pool_id: POOL_ID.to_vec(),
            currency0: Vec::new(),
            currency1: Vec::new(),
            transaction: Some(Transaction {
                hash: vec![1],
                from: vec![],
                to: vec![],
                index: tx_index,
            }),
            r#type: Some(kind),
        }
    }

    pub fn mint(ids: Vec<u32>) -> Type {
        Type::Mint(pool_event::Mint { ids })
    }

    pub fn burn(ids: Vec<u32>) -> Type {
        Type::Burn(pool_event::Burn { ids })
    }

    pub fn swap(active_id: u32) -> Type {
        Type::Swap(pool_event::Swap { active_id })
    }

    pub fn donate(amount0: &str, amount1: &str, bin_id: u32) -> Type {
        Type::Donate(pool_event::Donate {
            amount0: amount0.into(),
            amount1: amount1.into(),
            bin_id,
        })
    }

    /// A `reserveOfBin` word: y in the first 16 bytes, x in the last.
    pub fn packed(x: u128, y: u128) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[..16].copy_from_slice(&y.to_be_bytes());
        out[16..].copy_from_slice(&x.to_be_bytes());
        out
    }

    /// A 32-byte word of one repeated byte.
    pub fn word(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }
}
