use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use tycho_substreams::prelude::*;

use crate::common::{store_key, token_key};

/// Indexes discovered books by their price-store address and by every token, so the
/// tracked-contract predicate can resolve a store address and balance logic can resolve a
/// token back to its component.
///
/// Every token is indexed (not just the base side) so lookups are independent of token
/// ordering; the USDC key is harmless because USDC resolves through its own hub branch in
/// `books_for_token` and never reads it.
#[substreams::handlers::store]
pub fn store_components(
    map: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    for tx_pc in map.tx_components {
        for pc in tx_pc.components {
            if let Some(store_addr) = pc.get_attribute_value("price_store") {
                store.set_if_not_exists(0, store_key(&store_addr), &pc);
            }
            for token in &pc.tokens {
                store.set_if_not_exists(0, token_key(token), &pc);
            }
        }
    }
}
