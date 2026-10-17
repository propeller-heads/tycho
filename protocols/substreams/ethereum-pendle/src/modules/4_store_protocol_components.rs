use substreams::store::{StoreNew, StoreSet, StoreSetRaw};
use tycho_substreams::prelude::*;

use crate::{consts::PENDLE_MARKET, keys::market_tokens_key};

/// Indexes each component's token list by component id, for the balance modules to look up.
///
/// Markets additionally get a dedicated key with their token roles. The plain component-id key
/// cannot identify a market on its own: an SY component is keyed by address too, and its token
/// list can also be three long.
#[substreams::handlers::store]
pub fn store_protocol_components(
    components: BlockTransactionProtocolComponents,
    store: StoreSetRaw,
) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            let tokens = serde_sibor::to_bytes(&component.tokens)
                .expect("serializing component tokens for the component store");
            store.set(0, component.id.clone(), &tokens);

            let is_market = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_MARKET);
            if !is_market {
                continue;
            }

            let roles = ["sy_address", "pt_address", "yt_address"].map(|name| {
                component
                    .get_attribute_value(name)
                    .unwrap_or_else(|| panic!("market {} has no {}", component.id, name))
            });
            let roles = serde_sibor::to_bytes(roles.to_vec())
                .expect("serializing market token roles for the component store");
            store.set(0, market_tokens_key(&component.id), &roles);
        }
    }
}
