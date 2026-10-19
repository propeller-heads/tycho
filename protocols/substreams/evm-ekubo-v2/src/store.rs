use substreams::store::StoreSetSum;

use crate::pb::ekubo::ChangeType;

pub fn store_method_from_change_type<T, S: StoreSetSum<T>>(
    change_type: ChangeType,
) -> fn(&S, u64, String, T) {
    match change_type {
        ChangeType::Delta => StoreSetSum::sum,
        ChangeType::Absolute => StoreSetSum::set,
    }
}
