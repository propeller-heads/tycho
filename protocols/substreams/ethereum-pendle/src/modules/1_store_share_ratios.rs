use substreams::store::{StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth::v2 as eth;

use crate::shares::{best_ratios, ratio_pairs, share_ratio_key};

/// Keeps, per token that keeps balances in shares, its latest amount per share.
///
/// Every token emitting `TransferShares` after its `Transfer` is recorded, whether or not an SY
/// holds it yet: an SY listing it later needs its ratio from the first block. Each block's value is
/// its largest pair, set at that pair's ordinal.
#[substreams::handlers::store]
pub fn store_share_ratios(block: eth::Block, store: StoreSetString) {
    let mut pairs = Vec::new();
    for tx in block.transactions() {
        let logs: Vec<&eth::Log> = tx
            .logs_with_calls()
            .map(|(log, _)| log)
            .collect();
        pairs.extend(ratio_pairs(&logs));
    }
    for (token, ratio, ordinal) in best_ratios(pairs) {
        store.set(ordinal, share_ratio_key(token), &ratio.encode());
    }
}
