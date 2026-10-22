use substreams::store::{StoreNew, StoreSet, StoreSetBigInt};
use substreams_ethereum::pb::eth::v2 as eth;

use crate::entry_fee::{fee_change, FeeChange, FEE_BPS_KEY, FEE_FROM_KEY};

/// The entry fee the vault charges and the timestamp it applies from, from its events.
#[substreams::handlers::store]
pub fn store_entry_fee(block: eth::Block, store: StoreSetBigInt) {
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            match fee_change(log) {
                Some(FeeChange::Bps(bps)) => store.set(log.ordinal, FEE_BPS_KEY, &bps),
                Some(FeeChange::From(from)) => store.set(log.ordinal, FEE_FROM_KEY, &from),
                None => {}
            }
        }
    }
}
