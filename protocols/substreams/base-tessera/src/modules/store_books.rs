use substreams::store::{Appender, StoreAppend};
use tycho_substreams::prelude::*;

/// Append-only list of every book's price-store address (hex), for enumeration.
///
/// `StoreAppend` joins entries with `;`. Fan-out logic (USDC balances under every book,
/// treasury-rotation refresh, mark-all-updated) reads this list and resolves each entry
/// through `store_components`. A store address is appended exactly once — in the transaction
/// that creates its component.
#[substreams::handlers::store]
pub fn store_books(map: BlockTransactionProtocolComponents, store: StoreAppend<String>) {
    for tx_pc in map.tx_components {
        for pc in tx_pc.components {
            if let Some(store_addr) = pc.get_attribute_value("price_store") {
                store.append(0, "books", hex::encode(store_addr));
            }
        }
    }
}
