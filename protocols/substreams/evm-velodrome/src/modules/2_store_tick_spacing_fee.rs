use crate::pb::tycho::evm::velodrome::TickSpacingFees;
use substreams::store::{StoreNew, StoreSet, StoreSetInt64};

#[substreams::handlers::store]
pub fn store_tick_spacing_fee(tick_spacing_fees: TickSpacingFees, store: StoreSetInt64) {
    // Tick spacing fees come from the factory contract, deployed at block
    // 13843704 (and the dynamic_fee_module contract at 26301110). Indexing that much history for
    // integration tests would require scanning over 10M blocks, which isn’t practical.
    //
    // To keep the tests fast, we pre-store the factory’s tick spacing fees here.
    // This allows us to start the integration test from the pool’s creation block
    // while still retaining the update logic to handle future on-chain changes properly.
    // store.set(0, "tick_spacing_1", &100);
    // store.set(0, "tick_spacing_10", &500);
    // store.set(0, "tick_spacing_50", &500);
    // store.set(0, "tick_spacing_100", &500);
    // store.set(0, "tick_spacing_200", &3000);
    // store.set(0, "tick_spacing_2000", &10000);

    for fee in tick_spacing_fees
        .tick_spacing_fees
        .iter()
    {
        store.set(0, format!("tick_spacing_{}", fee.tick_spacing), &(fee.fee as i64));
    }
}
