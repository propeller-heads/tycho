// Copyright (c) 2026 Everlong Labs Limited
//! `FLAMMFactory.PoolCreated` → the pool's swap and lever-up components.
//!
//! Both components are emitted only when the manifest carries the pool's immutables and every
//! contract of the pool, the invariant hook included, was created with registered code inside the
//! indexed range (so its storage has been tracked since creation, and the hook's code is one the
//! manifest names).
//! Anything else is logged and skipped.
use std::collections::HashMap;

use anyhow::Result;
use substreams::store::{StoreGet, StoreGetRaw, StoreGetString};
use substreams_ethereum::pb::eth::v2::Block;
use tycho_substreams::prelude::{
    BlockTransactionProtocolComponents, TransactionProtocolComponents,
};

use crate::{
    config::Config,
    flamm::{
        calldata::{
            decode_create_pool, decode_pool_created, CREATE_POOL_SELECTOR, POOL_CREATED_TOPIC,
        },
        keys::{hex_address, parse_word, Address, Word},
        pad_word, statics,
        words::{block_writes, deploy_key, store_key, WordView},
        Role,
    },
};

/// The components created in the block, given the deployments and words lookups.
pub fn components_in_block(
    block: &Block,
    config: &Config,
    deployment: &impl Fn(&Address) -> Option<(Role, Word)>,
    first_word: impl Fn(&Address, &Word) -> Option<Word>,
) -> BlockTransactionProtocolComponents {
    let mut tx_components = Vec::new();
    let mut view: Option<WordView<'_>> = None;
    for tx in block.transactions() {
        let mut components = Vec::new();
        for (log, _) in tx.logs_with_calls() {
            if log.address != config.factory ||
                log.topics.first().map(Vec::as_slice) != Some(&POOL_CREATED_TOPIC)
            {
                continue;
            }
            let event = match decode_pool_created(&log.topics, &log.data) {
                Ok(e) => e,
                Err(err) => {
                    substreams::log::info!("PoolCreated at tx {}: {err}", hex_address(&tx.hash));
                    continue;
                }
            };
            // The createPool call whose prediction is this pool (the factory may be called through
            // another contract, so every non-reverted call to it is a candidate).
            let call = tx
                .calls
                .iter()
                .filter(|c| {
                    !c.state_reverted &&
                        c.address == config.factory &&
                        c.input
                            .starts_with(&CREATE_POOL_SELECTOR)
                })
                .filter_map(|c| decode_create_pool(&c.input).ok())
                .find(|c| {
                    c.hooks.invariant_hook == event.invariant_hook && c.pool_asset != [0u8; 20]
                });
            let Some(call) = call else {
                substreams::log::info!(
                    "PoolCreated for {} without a matching createPool call",
                    hex_address(&event.pool)
                );
                continue;
            };
            // Creation blocks are rare: the view is built from every write of the block on first
            // use.
            let view = view.get_or_insert_with(|| {
                let writes = block_writes(block, |_, _| true);
                WordView::new(&writes, &first_word, &config.words)
            });
            match statics::creation(&event, &call, config, deployment, view, tx.index as u64) {
                Ok(creation) => components.extend(statics::components(&creation)),
                Err(err) => {
                    substreams::log::info!("pool {} refused: {err}", hex_address(&event.pool));
                }
            }
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    BlockTransactionProtocolComponents { tx_components }
}

/// Parses a deployments store value `<role>:<codehash>`.
pub fn parse_deployment(value: &str) -> Option<(Role, Word)> {
    let (role, codehash) = value.split_once(':')?;
    Some((Role::parse(role).ok()?, parse_word(codehash).ok()?))
}

#[substreams::handlers::map]
pub fn map_components(
    params: String,
    block: Block,
    deployments: StoreGetString,
    words: StoreGetRaw,
) -> Result<BlockTransactionProtocolComponents> {
    let config = Config::parse(&params)?;
    let cache: std::cell::RefCell<HashMap<Address, Option<(Role, Word)>>> = Default::default();
    let deployment = |address: &Address| -> Option<(Role, Word)> {
        *cache
            .borrow_mut()
            .entry(*address)
            .or_insert_with(|| {
                deployments
                    .get_last(deploy_key(address))
                    .and_then(|v| parse_deployment(&v))
            })
    };
    let first_word = |address: &Address, key: &Word| -> Option<Word> {
        words
            .get_first(store_key(address, key))
            .map(|v| pad_word(&v))
    };
    Ok(components_in_block(&block, &config, &deployment, first_word))
}
