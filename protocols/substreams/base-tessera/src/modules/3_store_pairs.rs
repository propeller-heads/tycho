use crate::common::id;
use substreams::store::{Appender, StoreAppend};
use tycho_substreams::prelude::*;

#[substreams::handlers::store]
pub fn store_pairs(map: BlockTransactionProtocolComponents, store: StoreAppend<String>) {
    for tx in map.tx_components {
        for c in tx.components {
            store.append(0, "pairs", format!("{};", c.id));
            for token in c.tokens {
                store.append(0, format!("token:{}", id(&token)), format!("{};", c.id));
            }
        }
    }
}
