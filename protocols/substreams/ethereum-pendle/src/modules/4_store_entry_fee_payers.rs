use substreams::store::{Appender, StoreAppend};
use tycho_substreams::prelude::*;

use crate::{
    consts::{ENTRY_FEE_PAYER_YIELD_TOKENS, PENDLE_SY},
    entry_fee::PAYERS_KEY,
    sy::YIELD_TOKEN,
};

/// Lists the SYs whose deposits pay the vault's entry fee: those created with a paying yield token.
#[substreams::handlers::store]
pub fn store_entry_fee_payers(
    components: BlockTransactionProtocolComponents,
    store: StoreAppend<String>,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            let is_sy = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_SY);
            let pays = component
                .get_attribute_value(YIELD_TOKEN)
                .is_some_and(|yield_token| {
                    ENTRY_FEE_PAYER_YIELD_TOKENS
                        .iter()
                        .any(|payer| yield_token.as_slice() == payer)
                });
            if is_sy && pays {
                store.append(0, PAYERS_KEY, component.id.clone());
            }
        }
    }
}
