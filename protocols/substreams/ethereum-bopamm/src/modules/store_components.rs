use substreams::store::{StoreNew, StoreSet, StoreSetProto};
use tycho_substreams::prelude::*;

use crate::common::{token_index_keys, u64_from_word_padded};

/// Indexes discovered books both by `book:{assetId}` and by their asset token, so balance
/// and update logic can resolve a token or a book id back to its component.
///
/// The token index is newest-wins (`set`, not `set_if_not_exists`): when an asset is re-listed
/// under a new book, `token:{asset}` must resolve to the new (live) book so inventory routes there
/// rather than to the superseded one. The `book:{assetId}` keys are keyed by a unique asset id, so
/// their writes are effectively write-once regardless.
#[substreams::handlers::store]
pub fn store_components(
    map: BlockTransactionProtocolComponents,
    store: StoreSetProto<ProtocolComponent>,
) {
    for tx_pc in map.tx_components {
        for pc in tx_pc.components {
            if let Some(asset_id) = pc
                .get_attribute_value("asset_id")
                .and_then(|b| u64_from_word_padded(&b))
            {
                store.set(0, format!("book:{asset_id}"), &pc);
            }
            for key in token_index_keys(&pc.tokens) {
                store.set(0, key, &pc);
            }
        }
    }
}
