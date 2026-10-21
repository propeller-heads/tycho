use ethabi::ethereum_types::Address;
use substreams::store::{StoreGet, StoreGetProto};

use substreams_helper::{common::HasAddresser, hex::Hexable};

use tycho_substreams::prelude::*;

pub struct PoolAddresser<'a> {
    pub store: &'a StoreGetProto<ProtocolComponent>,
}

impl HasAddresser for PoolAddresser<'_> {
    fn has_address(&self, key: Address) -> bool {
        let pool_address = key.to_hex();
        let pool = self
            .store
            .get_last(pool_address.as_str());

        pool.is_some()
    }
}
