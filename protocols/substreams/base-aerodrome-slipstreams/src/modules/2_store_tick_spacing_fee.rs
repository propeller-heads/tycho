use crate::{modules::utils::tick_spacing_fee_key, pb::tycho::evm::aerodrome::TickSpacingFees};
use substreams::store::{StoreNew, StoreSet, StoreSetInt64};

#[substreams::handlers::store]
pub fn store_tick_spacing_fee(tick_spacing_fees: TickSpacingFees, store: StoreSetInt64) {
    // Tick spacing fees come from the factory contracts, the earliest deployed at block 13843704.
    // Indexing that much history for integration tests would require scanning over 10M blocks,
    // which isn't practical.
    //
    // To keep the tests fast, pre-store a factory's tick spacing fees here. This allows us to
    // start the integration test from the pool's creation block while still retaining the update
    // logic below to handle future on-chain changes properly. Keys are scoped by factory, so
    // pre-store every factory a test's pools may come from. These are the fees the three deployed
    // factories currently charge, except that 5e7BB104 never enabled tick spacing 500.
    //
    // use hex_literal::hex;
    // for factory in [
    //     hex!("5e7BB104d84c7CB9B682AaC2F3d509f5F406809A"),
    //     hex!("aDe65c38CD4849aDBA595a4323a8C7DdfE89716a"),
    //     hex!("f8f2eB4940CFE7d13603DDDD87f123820Fc061Ef"),
    // ] {
    //     for (tick_spacing, fee) in [
    //         (1, 100),
    //         (10, 500),
    //         (50, 500),
    //         (100, 500),
    //         (200, 3000),
    //         (500, 20000),
    //         (2000, 10000),
    //     ] {
    //         store.set(0, tick_spacing_fee_key(&factory, tick_spacing), &fee);
    //     }
    // }

    for fee in tick_spacing_fees
        .tick_spacing_fees
        .iter()
    {
        store.set(0, tick_spacing_fee_key(&fee.factory, fee.tick_spacing), &(fee.fee as i64));
    }
}
