use substreams::store::{Appender, StoreAppend, StoreGet, StoreGetString};
use tycho_substreams::prelude::*;

use crate::{
    consts::PENDLE_SY,
    shares::{share_holders_key, share_ratio_key},
};

/// Lists, per token kept in shares, the SYs that hold it in shares.
///
/// Decided once, at the SY's creation, for each token it lists: one with a known ratio then is a
/// shares token, and the SY's balance of it is followed in shares from then on. Its token list is
/// fixed at creation, so no later entry is needed.
#[substreams::handlers::store]
pub fn store_share_holders(
    components: BlockTransactionProtocolComponents,
    ratios: StoreGetString,
    store: StoreAppend<String>,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            let is_sy = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_SY);
            if !is_sy {
                continue;
            }
            for token in &component.tokens {
                if ratios.has_last(share_ratio_key(token)) {
                    store.append(0, share_holders_key(token), component.id.clone());
                }
            }
        }
    }
}
