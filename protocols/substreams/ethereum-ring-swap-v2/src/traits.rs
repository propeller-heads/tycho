use ethabi::ethereum_types::Address;
use substreams::store::{StoreGet, StoreGetProto};

use substreams_helper::{common::HasAddresser, hex::Hexable};

use tycho_substreams::prelude::*;

use crate::store_key::StoreKey;

pub struct PoolAddresser<'a> {
    pub store: &'a StoreGetProto<ProtocolComponent>,
}

impl HasAddresser for PoolAddresser<'_> {
    fn has_address(&self, key: Address) -> bool {
        let pool = self
            .store
            .get_last(StoreKey::Pool.get_unique_pool_key(&key.to_hex()));

        pool.is_some()
    }
}
