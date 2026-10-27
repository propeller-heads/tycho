// Copyright (c) 2026 Everlong Labs Limited
//! The pools the package tracks: `pools` lists their addresses, `pool:<address>` holds each pool's
//! serialized tracking config (see [`crate::flamm::PoolConfig`]). Append-only, written once per
//! pool from its swap component.
use substreams::store::{Appender, StoreAppend};
use tycho_substreams::prelude::BlockTransactionProtocolComponents;

use crate::flamm::{keys::hex_address, statics};

pub fn pools_key() -> String {
    "pools".to_string()
}

pub fn pool_key(pool: &[u8]) -> String {
    format!("pool:{}", hex_address(pool))
}

#[substreams::handlers::store]
pub fn store_pools(components: BlockTransactionProtocolComponents, store: StoreAppend<String>) {
    for tx_components in components.tx_components {
        for component in tx_components.components {
            // One config per pool: the swap component (kind 0) carries it; the lever-up component
            // is the same pool.
            let kind = component
                .get_attribute_value("component_kind")
                .unwrap_or_default();
            if kind.iter().any(|b| *b != 0) {
                continue;
            }
            let Ok(cfg) = statics::pool_config_from_component(&component) else { continue };
            store.append(0, pools_key(), hex_address(&cfg.pool));
            store.append(0, pool_key(&cfg.pool), cfg.serialize());
        }
    }
}
