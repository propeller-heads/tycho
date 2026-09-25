// Copyright (c) 2026 Everlong Labs Limited
//! `word:<address>:<slot>` → the 32-byte value, for every tracked storage write.
//!
//! Tracked: every write of a contract the deployments store knows (a pool's own words filtered to
//! its namespace), every write of a manifest `aggregators` entry (the Chainlink aggregators, whose
//! round words have per-round keys), and the seeded `words` keys (Morpho market and position, the
//! IRM rate, the proxies' phase and access words, the aggregators' access words).
//! `map_protocol_changes` reads this store as of the start of a block to build the creation
//! snapshot of a new pool and to value words a transaction did not touch.
use std::collections::{HashMap, HashSet};

use substreams::store::{StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetRaw};
use substreams_ethereum::pb::eth::v2::Block;

use crate::{
    config::Config,
    flamm::{
        keys::{self, Address, Word},
        words::{block_writes, deploy_key, store_key, BlockWrite},
        Role,
    },
};

/// The writes the store records, given the deployments lookup.
pub fn tracked_writes(
    block: &Block,
    config: &Config,
    deployment_role: impl Fn(&Address) -> Option<Role>,
) -> Vec<BlockWrite> {
    let mut roles: HashMap<Address, Option<Role>> = HashMap::new();
    let mut pool_keys: Option<HashSet<Word>> = None;
    let mut keep = |address: &Address, key: &Word| -> bool {
        if config.seeded(address, key) || config.tracks_address(address) {
            return true;
        }
        let role = *roles
            .entry(*address)
            .or_insert_with(|| deployment_role(address));
        match role {
            Some(Role::Pool) => pool_keys
                .get_or_insert_with(|| keys::pool_keys().into_iter().collect())
                .contains(key),
            Some(_) => true,
            None => false,
        }
    };
    block_writes(block, &mut keep)
}

#[substreams::handlers::store]
pub fn store_words(params: String, block: Block, deployments: StoreGetString, store: StoreSetRaw) {
    let config = Config::parse(&params).expect("invalid base-flamm params");
    let role = |address: &Address| -> Option<Role> {
        deployments
            .get_last(deploy_key(address))
            .and_then(|v| Role::parse(v.split(':').next().unwrap_or_default()).ok())
    };
    for w in tracked_writes(&block, &config, role) {
        store.set(w.ordinal, store_key(&w.address, &w.key), &w.value.to_vec());
    }
}
