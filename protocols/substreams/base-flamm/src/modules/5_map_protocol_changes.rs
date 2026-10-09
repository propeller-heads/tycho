// Copyright (c) 2026 Everlong Labs Limited
//! Per transaction: the new components with their full creation snapshot, every tracked storage
//! word that changed (as `<role>:<slot>` / `mm:…` / `irm:…` attributes: a `Creation` for the
//! first write of a word the indexer holds no row for, an `Update` otherwise), the Chainlink
//! feeds (a function of the proxies' and aggregators' words, re-derived and diffed for every
//! transaction that wrote one of them) and the inventory balances. Both components of a pool
//! carry the same attributes.
use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use substreams::store::{StoreGet, StoreGetArray, StoreGetRaw};
use substreams_ethereum::pb::eth::v2::Block;
use tycho_substreams::prelude::{
    Attribute, BalanceChange, BlockChanges, BlockTransactionProtocolComponents, ChangeType,
    EntityChanges, Transaction, TransactionChangesBuilder,
};

use crate::{
    config::Config,
    flamm::{
        attribute, balances, deleted_attribute,
        feeds::{feed_state, phase_and_aggregator},
        keys::{self, hex_address, Address, Word},
        pad_word, statics, tracked_attribute, word_attribute,
        words::{block_writes, store_key, BlockWrite, WordView},
        FeedConfig, PoolConfig,
    },
    modules::store_pools::{pool_key, pools_key},
};

struct Tracked {
    cfg: PoolConfig,
    ids: [String; 2],
    created_in: Option<u64>,
    key_map: HashMap<(Address, Word), Vec<String>>,
    /// The words the creation snapshot carries as zero rows when the store has no value: the
    /// Morpho positions of the venue accounts (zero by construction for a new pool). The indexer
    /// holds their rows from the creation on, so their first write is an `Update`.
    zero_rows: HashSet<(Address, Word)>,
}

impl Tracked {
    fn new(cfg: PoolConfig, created_in: Option<u64>) -> Self {
        let mut key_map: HashMap<(Address, Word), Vec<String>> = HashMap::new();
        for (address, key, name) in cfg.tracked_words() {
            key_map
                .entry((address, key))
                .or_default()
                .push(name);
        }
        let zero_rows = cfg
            .venues
            .iter()
            .flat_map(|v| {
                keys::morpho_position_keys(&v.market_id, &v.account)
                    .into_iter()
                    .map(move |k| (v.morpho, k))
            })
            .collect();
        Self { ids: cfg.component_ids(), cfg, created_in, key_map, zero_rows }
    }

    /// Whether the indexer holds a row for the word before transaction `tx_index`: the word was
    /// written in an earlier block (the words store) or transaction, is a manifest seed, or is a
    /// zero row of the creation snapshot. The creation snapshot carries every tracked word the
    /// view values and the zero rows, and every later write reaches the rows, so a word without
    /// a prior value has no row. Every tracked contract is tracked from its deployment (a pool's
    /// own contracts) or seeded (Morpho, the IRM, the proxies), so a word the store lacks was
    /// never written.
    fn holds_row(&self, view: &WordView<'_>, address: &Address, key: &Word, tx_index: u64) -> bool {
        self.zero_rows
            .contains(&(*address, *key)) ||
            view.before_tx(address, key, tx_index)
                .is_some()
    }

    fn addresses(&self) -> Vec<Address> {
        let mut out: Vec<Address> = self
            .cfg
            .storage_contracts()
            .into_iter()
            .map(|(_, a)| a)
            .collect();
        for v in &self.cfg.venues {
            out.push(v.morpho);
            out.push(v.irm);
        }
        out.extend(self.cfg.feeds.iter().map(|f| f.proxy));
        out
    }

    /// The words the inventory depends on (see `balances`).
    fn balance_keys(&self) -> Vec<(Address, Word)> {
        let mut out = vec![(self.cfg.pool, keys::add(&keys::FLAMM_NS, 12))];
        for i in 0..self.cfg.loan_assets.len() as u64 {
            out.push((self.cfg.pool, keys::add(&keys::pool_loan(i), 5)));
        }
        for (v, venue) in self.cfg.venues.iter().enumerate() {
            let head = keys::router_venue(&self.cfg.pool, v as u64);
            out.push((self.cfg.router, keys::add(&head, 2)));
            out.push((self.cfg.router, keys::add(&head, 4)));
            out.push((self.cfg.router, keys::add(&head, 5)));
            for k in keys::morpho_position_keys(&venue.market_id, &venue.account) {
                out.push((venue.morpho, k));
            }
            out.push((venue.morpho, keys::morpho_market_keys(&venue.market_id)[0]));
        }
        out
    }
}

/// Whether a transaction's tracked writes can move a feed's state: a write of the proxy's phase
/// word (a rotation), or any write of the aggregator behind the proxy. Without a rotation the
/// aggregator is the same before and after the transaction, and an aggregator the manifest does
/// not list has no tracked writes.
fn feed_touched(
    feed: &FeedConfig,
    writes: &[&BlockWrite],
    view: &WordView<'_>,
    tx_index: u64,
) -> bool {
    let phase = keys::slot(keys::PROXY_PHASE_SLOT);
    if writes
        .iter()
        .any(|w| w.address == feed.proxy && w.key == phase)
    {
        return true;
    }
    let Some(phase_word) = view.at(&feed.proxy, &phase, tx_index) else { return false };
    let aggregator = phase_and_aggregator(&phase_word).1;
    writes
        .iter()
        .any(|w| w.address == aggregator)
}

/// The attribute changes from `before` to `after`: a `Creation` for every attribute `after` adds,
/// an `Update` for every value that changed, a `Deletion` for every attribute `before` had and
/// `after` has not. Nothing is emitted for an unchanged value.
fn diff_attributes(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Vec<Attribute> {
    let mut out = Vec::new();
    for (name, value) in after {
        match before.get(name) {
            Some(old) if old == value => {}
            Some(_) => out.push(attribute(name, value.clone(), ChangeType::Update)),
            None => out.push(attribute(name, value.clone(), ChangeType::Creation)),
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            out.push(deleted_attribute(name));
        }
    }
    out
}

fn push(builder: &mut TransactionChangesBuilder, ids: &[String; 2], attributes: Vec<Attribute>) {
    if attributes.is_empty() {
        return;
    }
    for id in ids {
        builder.add_entity_change(&EntityChanges {
            component_id: id.clone(),
            attributes: attributes.clone(),
        });
    }
}

/// The inventory after `tx_index`, for every token when `changed_only` is false (the creation
/// snapshot), else for the tokens whose inventory this transaction moved: a Morpho accrual moves
/// the market's totals word in most blocks without moving what the pool recognizes.
fn push_balances(
    builder: &mut TransactionChangesBuilder,
    tracked: &Tracked,
    view: &WordView<'_>,
    tx_index: u64,
    changed_only: bool,
) {
    let Some(after) = balances::balances(&tracked.cfg, view, tx_index) else { return };
    let before = if changed_only {
        balances::balances_before(&tracked.cfg, view, tx_index).unwrap_or_default()
    } else {
        Vec::new()
    };
    for id in &tracked.ids {
        for (token, balance) in &after {
            if before.contains(&(*token, *balance)) {
                continue;
            }
            builder.add_balance_change(&BalanceChange {
                token: token.to_vec(),
                balance: balances::balance_bytes(balance),
                component_id: id.as_bytes().to_vec(),
            });
        }
    }
}

/// The block's changes, given the pools known before it and the words store as of its start.
pub fn protocol_changes(
    block: &Block,
    config: &Config,
    known_pools: Vec<PoolConfig>,
    new_components: &BlockTransactionProtocolComponents,
    first_word: impl Fn(&Address, &Word) -> Option<Word>,
) -> BlockChanges {
    let mut pools: Vec<Tracked> = known_pools
        .into_iter()
        .map(|cfg| Tracked::new(cfg, None))
        .collect();
    let mut created: HashMap<u64, Vec<tycho_substreams::prelude::ProtocolComponent>> =
        HashMap::new();
    for tx_components in &new_components.tx_components {
        let Some(tx) = tx_components.tx.as_ref() else { continue };
        for component in &tx_components.components {
            created
                .entry(tx.index)
                .or_default()
                .push(component.clone());
            if component
                .get_attribute_value("component_kind")
                .is_some_and(|k| keys::is_zero(&k))
            {
                match statics::pool_config_from_component(component) {
                    Ok(cfg)
                        if !pools
                            .iter()
                            .any(|p| p.cfg.pool == cfg.pool) =>
                    {
                        pools.push(Tracked::new(cfg, Some(tx.index)))
                    }
                    Ok(_) => {}
                    Err(err) => substreams::log::info!("component {}: {err}", component.id),
                }
            }
        }
    }
    if pools.is_empty() {
        return BlockChanges {
            block: Some(block.into()),
            changes: Vec::new(),
            ..Default::default()
        };
    }

    let mut interesting: HashSet<Address> = config
        .aggregators
        .keys()
        .copied()
        .collect();
    for p in &pools {
        interesting.extend(p.addresses());
    }
    let writes: Vec<BlockWrite> = block_writes(block, |a, _| interesting.contains(a));
    let view = WordView::new(&writes, first_word, &config.words);
    let mut by_tx: HashMap<u64, Vec<&BlockWrite>> = HashMap::new();
    for w in &writes {
        by_tx
            .entry(w.tx_index)
            .or_default()
            .push(w);
    }

    let mut transaction_changes: HashMap<u64, tycho_substreams::prelude::TransactionChanges> =
        HashMap::new();
    for tx in block.transactions() {
        let tx_index = tx.index as u64;
        let transaction: Transaction = tx.into();
        let mut builder = TransactionChangesBuilder::new(&transaction);

        // New components: the component, then its full snapshot as of this transaction.
        if let Some(components) = created.remove(&tx_index) {
            for c in &components {
                builder.add_protocol_component(c);
            }
            for p in pools
                .iter()
                .filter(|p| p.created_in == Some(tx_index))
            {
                let mut attributes = Vec::new();
                let mut names: Vec<(&(Address, Word), &Vec<String>)> = p.key_map.iter().collect();
                names.sort_by(|a, b| a.1.cmp(b.1));
                for ((address, key), attrs) in names {
                    if let Some(value) = view.at(address, key, tx_index) {
                        for name in attrs {
                            attributes.push(tracked_attribute(name, &value, ChangeType::Creation));
                        }
                    }
                }
                // Morpho positions of the venue accounts created here are zero by construction
                // (`zero_rows`: the store has no value for a word never written).
                for (v, venue) in p.cfg.venues.iter().enumerate() {
                    for (i, key) in keys::morpho_position_keys(&venue.market_id, &venue.account)
                        .into_iter()
                        .enumerate()
                    {
                        debug_assert!(p
                            .zero_rows
                            .contains(&(venue.morpho, key)));
                        if view
                            .at(&venue.morpho, &key, tx_index)
                            .is_none()
                        {
                            attributes.push(word_attribute(
                                &format!("mm:{v}:position:{i}"),
                                &[0u8; 32],
                                ChangeType::Creation,
                            ));
                        }
                    }
                }
                for feed in &p.cfg.feeds {
                    let state = feed_state(&feed.role, &feed.proxy, &config.aggregators, |a, k| {
                        view.at(a, k, tx_index)
                    });
                    for (name, value) in state {
                        attributes.push(attribute(&name, value, ChangeType::Creation));
                    }
                }
                push(&mut builder, &p.ids, attributes);
                push_balances(&mut builder, p, &view, tx_index, false);
            }
        }

        // The tracked words this transaction wrote (in ordinal order, so a word written twice
        // carries its last value), and the feeds whose words it touched, valued after it.
        let tx_writes = by_tx
            .remove(&tx_index)
            .unwrap_or_default();
        for p in pools.iter() {
            if tx_writes.is_empty() {
                break;
            }
            // A pool created in this transaction has its snapshot above; one created later in the
            // block does not exist yet.
            if p.created_in
                .is_some_and(|created| created >= tx_index)
            {
                continue;
            }
            let mut attributes = Vec::new();
            for w in &tx_writes {
                let Some(names) = p.key_map.get(&(w.address, w.key)) else { continue };
                // The first write of a word the indexer holds no row for creates the row; a
                // write of a held row updates it. `tycho-indexer` restores a reverted `Update`
                // from the row's prior value and counts one without any as an attribute miss
                // (`extractor/protocol_extractor.rs`, `extractor_revert_attr_miss`), while a
                // reverted `Creation` is deleted outright, so the two must not be confused. A
                // word written twice in the transaction gets the same change type for both
                // writes and the builder keeps the last value.
                let change = if p.holds_row(&view, &w.address, &w.key, tx_index) {
                    ChangeType::Update
                } else {
                    ChangeType::Creation
                };
                for name in names {
                    attributes.push(tracked_attribute(name, &w.value, change));
                }
            }
            for feed in &p.cfg.feeds {
                if !feed_touched(feed, &tx_writes, &view, tx_index) {
                    continue;
                }
                let before = feed_state(&feed.role, &feed.proxy, &config.aggregators, |a, k| {
                    view.before_tx(a, k, tx_index)
                });
                let after = feed_state(&feed.role, &feed.proxy, &config.aggregators, |a, k| {
                    view.at(a, k, tx_index)
                });
                attributes.extend(diff_attributes(&before, &after));
            }
            push(&mut builder, &p.ids, attributes);
            if p.balance_keys()
                .iter()
                .any(|(a, k)| view.written_in(a, k, tx_index))
            {
                push_balances(&mut builder, p, &view, tx_index, true);
            }
        }
        if let Some(changes) = builder.build() {
            transaction_changes.insert(tx_index, changes);
        }
    }

    let mut changes: Vec<_> = transaction_changes
        .into_values()
        .collect();
    changes.sort_by_key(|c| {
        c.tx.as_ref()
            .map(|t| t.index)
            .unwrap_or_default()
    });
    BlockChanges { block: Some(block.into()), changes, ..Default::default() }
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: Block,
    new_components: BlockTransactionProtocolComponents,
    pools: StoreGetArray<String>,
    words: StoreGetRaw,
) -> Result<BlockChanges> {
    let config = Config::parse(&params)?;
    let known: Vec<PoolConfig> = pools
        .get_first(pools_key())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.is_empty())
        .filter_map(|pool| {
            let address = keys::parse_address(&pool).ok()?;
            let serialized = pools
                .get_first(pool_key(&address))?
                .into_iter()
                .next()?;
            match PoolConfig::parse(&serialized) {
                Ok(cfg) => Some(cfg),
                Err(err) => {
                    substreams::log::info!("pool {}: {err}", hex_address(&address));
                    None
                }
            }
        })
        .collect();
    let first_word = |address: &Address, key: &Word| -> Option<Word> {
        words
            .get_first(store_key(address, key))
            .map(|v| pad_word(&v))
    };
    Ok(protocol_changes(&block, &config, known, &new_components, first_word))
}
