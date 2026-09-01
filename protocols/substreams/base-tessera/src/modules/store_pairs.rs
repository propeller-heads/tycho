use substreams::store::{Appender, StoreAppend};
use tycho_substreams::prelude::*;

use crate::common::token_store_key;

/// Append-only indexes over discovered pairs:
///
/// * `"pairs"` — every pair-contract address (hex), for enumeration;
/// * `token:{hex}` — the component ids a token backs, for balance fan-out. Append-valued because
///   one token can back several pairs: USDC quotes most of them, and a base token can trade against
///   several quotes (WETH/USDC and WETH/USDbC are distinct pairs).
///
/// `StoreAppend` joins entries with `;`. A pair is appended exactly once, in the transaction
/// that creates its component.
#[substreams::handlers::store]
pub fn store_pairs(map: BlockTransactionProtocolComponents, store: StoreAppend<String>) {
    for tx_pc in map.tx_components {
        for pc in tx_pc.components {
            store.append(
                0,
                "pairs",
                pc.id
                    .trim_start_matches("0x")
                    .to_string(),
            );
            for token in &pc.tokens {
                store.append(0, token_store_key(token), pc.id.clone());
            }
        }
    }
}
