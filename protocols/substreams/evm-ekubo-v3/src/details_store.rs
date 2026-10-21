use substreams::{
    log::debug,
    store::{StoreGet as _, StoreGetProto, StoreSetSum},
};

use crate::pb::ekubo::{ChangeType, PoolDetails};

pub fn store_method_from_change_type<T, S: StoreSetSum<T>>(
    change_type: ChangeType,
) -> fn(&S, u64, String, T) {
    match change_type {
        ChangeType::Delta => StoreSetSum::sum,
        ChangeType::Absolute => StoreSetSum::set,
    }
}

pub fn get_pool_details(
    store: &StoreGetProto<PoolDetails>,
    component_id: &str,
) -> Option<PoolDetails> {
    let opt = store.get_last(component_id);

    if opt.is_none() {
        debug!("Ignoring missing pool details for pool {}", component_id);
    }

    opt
}
