use crate::{modules::utils::tick_spacing_fee_key, pb::tycho::evm::aerodrome::TickSpacingFees};
use substreams::store::{StoreNew, StoreSet, StoreSetInt64};

#[substreams::handlers::store]
pub fn store_tick_spacing_fee(tick_spacing_fees: TickSpacingFees, store: StoreSetInt64) {
    for fee in tick_spacing_fees
        .tick_spacing_fees
        .iter()
    {
        store.set(0, tick_spacing_fee_key(&fee.factory, fee.tick_spacing), &(fee.fee as i64));
    }
}
