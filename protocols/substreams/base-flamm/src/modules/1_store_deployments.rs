// Copyright (c) 2026 Everlong Labs Limited
//! `deploy:<address>` → `<role>:<codehash>` for every contract created with a registered runtime
//! code.
//!
//! This is how the package tracks a pool's contracts from before the pool exists: the hooks, price
//! feed, router and factory are deployed ahead of `createPool` (`FLAMMFactory.predictPool`), so
//! their storage is recorded from the block that creates them, and the creation snapshot of a later
//! pool reads it back. The registry is the manifest `deployments` param; a contract created with
//! unregistered code is invisible.
use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString};
use substreams_ethereum::pb::eth::v2::Block;

use crate::{
    config::Config,
    flamm::{
        keys::hex_word,
        words::{deploy_key, deployments_in_block},
    },
};

#[substreams::handlers::store]
pub fn store_deployments(params: String, block: Block, store: StoreSetIfNotExistsString) {
    let config = Config::parse(&params).expect("invalid base-flamm params");
    for d in deployments_in_block(&block, &config.deployments) {
        store.set_if_not_exists(
            d.ordinal,
            deploy_key(&d.address),
            &format!("{}:{}", d.role, hex_word(&d.codehash)),
        );
    }
}
