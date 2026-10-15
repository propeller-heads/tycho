//! Reference model and random extractor histories for delta catch-up tests.
//!
//! [`DeltaStateModel`] is the oracle: the state a client holds after applying a sequence of
//! WebSocket messages. A client that caught up after a gap must end in a state that matches the
//! canonical chain ([`DeltaStateModel::check_against`]), or take a snapshot.
//!
//! [`generate_history`] produces extractor histories with the shapes that make catch-up hard:
//! nested reverts, reverts below a client's block, components created on orphaned branches,
//! window evictions, extractor restarts and fingerprint changes. [`catch_up_spec`] says, for a
//! client that missed part of a history, whether catch-up must succeed and with which messages,
//! or why it must fall back to a snapshot.

use std::{
    collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
    fmt::Debug,
    hash::Hash,
    sync::Arc,
};

use chrono::NaiveDateTime;
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::{
    dto,
    models::{
        blockchain::{
            Block, BlockAggregatedChanges, DCIUpdate, EntryPoint, RPCTracerParams, TracingParams,
            TracingResult,
        },
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        token::Token,
        Address, AttrStoreKey, Balance, BlockHash, Chain, ChangeType, Code, ComponentId,
        EntryPointId, StoreKey, StoreVal,
    },
    Bytes,
};

/// Extractor of every message [`generate_history`] produces.
pub const EXTRACTOR: &str = "delta_history";

/// Chain of every message [`generate_history`] produces.
pub const CHAIN: Chain = Chain::Ethereum;

/// State a client holds after applying a sequence of full-block and revert messages.
///
/// Every value is the latest one written. Empty bytes count as absent: a revert sends them for a
/// slot or balance that had no value at its target. The fields fall in three groups, which
/// [`DeltaStateModel::check_against`] compares differently:
/// - Compared exactly: `extractor`, `head`, and what reverts restore: components, attributes,
///   component balances, storage slots.
/// - Not undone by reverts: tokens, TVL, DCI entry points and traced contracts. Revert messages
///   carry none of them and the client never removes them, so entries written on an orphaned branch
///   stay. A client that caught up past an orphaned branch it never saw lacks them.
/// - Not reliably restored by the indexer today: native balance and code (never restored) and
///   account token balances (restored only for accounts whose storage changed in the reverted
///   range).
///
/// Two equal models are clients a consumer could not tell apart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeltaStateModel {
    /// Extractor of the applied messages. `None` before the first message.
    pub extractor: Option<String>,
    /// Block of the last applied message: the tip the client believes in. `None` before the
    /// first message.
    pub head: Option<Block>,
    /// Components that exist: created and not deleted since. Stored with `change` set to
    /// `Creation`, since a revert rewrites it.
    pub components: HashMap<ComponentId, ProtocolComponent>,
    /// Attribute values by component id, then attribute name. Dropped with the component.
    pub component_attributes: HashMap<ComponentId, HashMap<AttrStoreKey, Bytes>>,
    /// Token balances by component id, then token address. Dropped with the component.
    pub component_balances: HashMap<ComponentId, HashMap<Address, Balance>>,
    /// Storage slot values by account address, then slot. An account `Creation` replaces the
    /// account's slots, as the consumer does.
    pub account_storage: HashMap<Address, HashMap<StoreKey, StoreVal>>,
    /// Native balance by account address. Not reliably restored by reverts.
    pub account_native_balances: HashMap<Address, Balance>,
    /// Contract code by account address, set by account `Creation` only, as the consumer does.
    /// Not reliably restored by reverts.
    pub account_code: HashMap<Address, Code>,
    /// Token balances held by accounts, by account address, then token address. Not reliably
    /// restored by reverts.
    pub account_token_balances: HashMap<Address, HashMap<Address, Balance>>,
    /// Tokens by address. Not undone by reverts.
    pub tokens: HashMap<Address, Token>,
    /// Latest TVL by component id. Not undone by reverts.
    pub component_tvl: HashMap<ComponentId, f64>,
    /// Entry points linked to each component, by component id. Not undone by reverts.
    pub component_entrypoints: HashMap<ComponentId, HashSet<EntryPointId>>,
    /// Contracts the traces of each entry point accessed, by entry point id. Decides which
    /// contracts' deltas the client keeps. Not undone by reverts.
    pub entrypoint_contracts: HashMap<EntryPointId, HashSet<Address>>,
}

impl DeltaStateModel {
    /// Applies one full-block or revert message the way a client does: every value in the message
    /// overwrites the held one, deletions and empty values remove it, DCI data is added to what
    /// is held, and the head moves to the message's block.
    ///
    /// Components apply after the message's attributes and balances, so a deleted component's
    /// state goes with it. `deleted_protocol_components` applies before `new_protocol_components`,
    /// each component by its `change`.
    ///
    /// # Panics
    ///
    /// On a partial-block message, or one from another extractor or chain than the messages
    /// applied before.
    pub fn apply(&mut self, message: &BlockAggregatedChanges) {
        let BlockAggregatedChanges {
            extractor,
            chain,
            block,
            // The watermarks and the revert flag drive the server's retention; what a client
            // holds does not depend on them.
            finalized_block_height: _,
            db_committed_block_height: _,
            revert: _,
            state_deltas,
            account_deltas,
            new_tokens,
            new_protocol_components,
            deleted_protocol_components,
            component_balances,
            account_balances,
            component_tvl,
            dci_update,
            partial_block_index,
        } = message;
        assert!(
            partial_block_index.is_none(),
            "the model applies full blocks only; block {} is partial {partial_block_index:?}",
            block.number
        );
        if let Some(held) = &self.extractor {
            assert_eq!(held, extractor, "block {} is from another extractor", block.number);
        }
        if let Some(head) = &self.head {
            assert_eq!(head.chain, *chain, "block {} is from another chain", block.number);
        }

        for (component_id, delta) in state_deltas {
            self.apply_state_delta(component_id, delta);
        }
        for (component_id, balances) in component_balances {
            let held = self
                .component_balances
                .entry(component_id.clone())
                .or_default();
            for (token, balance) in balances {
                let ComponentBalance {
                    token: _,
                    balance,
                    balance_float: _,
                    modify_tx: _,
                    component_id: _,
                } = balance;
                write(held, token, balance);
            }
            drop_if_empty(&mut self.component_balances, component_id);
        }
        for (address, delta) in account_deltas {
            self.apply_account_delta(address, delta);
        }
        for (account, balances) in account_balances {
            let held = self
                .account_token_balances
                .entry(account.clone())
                .or_default();
            for (token, balance) in balances {
                let AccountBalance { account: _, token: _, balance, modify_tx: _ } = balance;
                write(held, token, balance);
            }
            drop_if_empty(&mut self.account_token_balances, account);
        }
        for (component_id, component) in deleted_protocol_components
            .iter()
            .chain(new_protocol_components)
        {
            self.apply_component(component_id, component);
        }
        for (address, token) in new_tokens {
            self.tokens
                .insert(address.clone(), token.clone());
        }
        for (component_id, tvl) in component_tvl {
            self.component_tvl
                .insert(component_id.clone(), *tvl);
        }
        self.apply_dci(dci_update);
        self.extractor = Some(extractor.clone());
        self.head = Some(block.clone());
    }

    /// The state after applying `messages` in order to an empty state.
    ///
    /// # Panics
    ///
    /// Like [`DeltaStateModel::apply`].
    pub fn fold<'a>(messages: impl IntoIterator<Item = &'a BlockAggregatedChanges>) -> Self {
        let mut state = Self::default();
        for message in messages {
            state.apply(message);
        }
        state
    }

    /// Checks a client's state against the canonical chain and against a client that applied
    /// every message the server sent.
    ///
    /// `extractor`, `head` and the fields reverts restore must equal `canonical`. Every other
    /// field must hold every entry `canonical` holds, since a missing one is data the chain has
    /// and the client lost, and no entry `connected` lacks, since no message wrote it. TVL and the
    /// fields the indexer does not reliably restore are compared by key only: a value written on
    /// an orphaned branch stays until the key's next update, so it can match neither.
    ///
    /// Account token balances are only required to hold every canonical entry. A revert empties
    /// an orphaned branch's balance only for accounts whose storage changed in its range, so which
    /// of those balances a client keeps depends on the reverts it saw: a connected client can lack
    /// one that a client which caught up past the revert keeps.
    ///
    /// Returns the first rule the state breaks, in field order; within a field, the entry with
    /// the smallest debug form.
    pub fn check_against(&self, canonical: &Self, connected: &Self) -> Result<(), StateMismatch> {
        let Self {
            extractor,
            head,
            components,
            component_attributes,
            component_balances,
            account_storage,
            account_native_balances,
            account_code,
            account_token_balances,
            tokens,
            component_tvl,
            component_entrypoints,
            entrypoint_contracts,
        } = self;
        let Self {
            extractor: canonical_extractor,
            head: canonical_head,
            components: canonical_components,
            component_attributes: canonical_attributes,
            component_balances: canonical_component_balances,
            account_storage: canonical_storage,
            account_native_balances: canonical_native_balances,
            account_code: canonical_code,
            account_token_balances: canonical_token_balances,
            tokens: canonical_tokens,
            component_tvl: canonical_tvl,
            component_entrypoints: canonical_entrypoints,
            entrypoint_contracts: canonical_contracts,
        } = canonical;
        // A connected client's restored fields may hold an abandoned branch after an extractor
        // restart, so only the fields compared by entry are bounded by it.
        let Self {
            extractor: _,
            head: _,
            components: _,
            component_attributes: _,
            component_balances: _,
            account_storage: _,
            account_native_balances: connected_native_balances,
            account_code: connected_code,
            account_token_balances: _,
            tokens: connected_tokens,
            component_tvl: connected_tvl,
            component_entrypoints: connected_entrypoints,
            entrypoint_contracts: connected_contracts,
        } = connected;

        same("extractor", extractor, canonical_extractor)?;
        same("head", head, canonical_head)?;
        same("components", components, canonical_components)?;
        same("component_attributes", component_attributes, canonical_attributes)?;
        same("component_balances", component_balances, canonical_component_balances)?;
        same("account_storage", account_storage, canonical_storage)?;
        bounded(
            "account_native_balances",
            keys(account_native_balances),
            keys(canonical_native_balances),
            keys(connected_native_balances),
        )?;
        bounded("account_code", keys(account_code), keys(canonical_code), keys(connected_code))?;
        covers(
            "account_token_balances",
            &map_pairs(account_token_balances),
            &map_pairs(canonical_token_balances),
        )?;
        bounded("tokens", keys(tokens), keys(canonical_tokens), keys(connected_tokens))?;
        bounded("component_tvl", keys(component_tvl), keys(canonical_tvl), keys(connected_tvl))?;
        bounded(
            "component_entrypoints",
            set_pairs(component_entrypoints),
            set_pairs(canonical_entrypoints),
            set_pairs(connected_entrypoints),
        )?;
        bounded(
            "entrypoint_contracts",
            set_pairs(entrypoint_contracts),
            set_pairs(canonical_contracts),
            set_pairs(connected_contracts),
        )
    }

    fn apply_state_delta(
        &mut self,
        component_id: &ComponentId,
        delta: &ProtocolComponentStateDelta,
    ) {
        let ProtocolComponentStateDelta {
            component_id: _,
            updated_attributes,
            deleted_attributes,
            // An indexer-internal hint that the wire does not carry.
            created_attributes: _,
        } = delta;
        let held = self
            .component_attributes
            .entry(component_id.clone())
            .or_default();
        for (name, value) in updated_attributes {
            write(held, name, value);
        }
        for name in deleted_attributes {
            held.remove(name);
        }
        drop_if_empty(&mut self.component_attributes, component_id);
    }

    fn apply_account_delta(&mut self, address: &Address, delta: &AccountDelta) {
        let AccountDelta { chain: _, address: _, slots, balance, code, change } = delta;
        match change {
            ChangeType::Creation => {
                let mut storage = HashMap::new();
                for (slot, value) in slots {
                    if let Some(value) = value {
                        write(&mut storage, slot, value);
                    }
                }
                replace(&mut self.account_storage, address, storage);
                replace_bytes(&mut self.account_code, address, code.as_ref());
                replace_bytes(&mut self.account_native_balances, address, balance.as_ref());
            }
            ChangeType::Update => {
                let storage = self
                    .account_storage
                    .entry(address.clone())
                    .or_default();
                for (slot, value) in slots {
                    match value {
                        Some(value) => write(storage, slot, value),
                        None => {
                            storage.remove(slot);
                        }
                    }
                }
                drop_if_empty(&mut self.account_storage, address);
                if let Some(balance) = balance {
                    write(&mut self.account_native_balances, address, balance);
                }
                // The consumer takes code from an account creation only.
            }
            ChangeType::Deletion => {
                self.account_storage.remove(address);
                self.account_native_balances
                    .remove(address);
                self.account_code.remove(address);
            }
        }
    }

    fn apply_component(&mut self, component_id: &ComponentId, component: &ProtocolComponent) {
        match component.change {
            ChangeType::Creation | ChangeType::Update => {
                let stored =
                    ProtocolComponent { change: ChangeType::Creation, ..component.clone() };
                self.components
                    .insert(component_id.clone(), stored);
            }
            ChangeType::Deletion => {
                self.components.remove(component_id);
                self.component_attributes
                    .remove(component_id);
                self.component_balances
                    .remove(component_id);
            }
        }
    }

    /// Adds DCI data like `ComponentTracker::process_entrypoints` does.
    fn apply_dci(&mut self, update: &DCIUpdate) {
        let DCIUpdate {
            new_entrypoints,
            // Tracing parameters decide what the server traces, not what a client keeps.
            new_entrypoint_params: _,
            trace_results,
        } = update;
        for (component_id, entrypoints) in new_entrypoints {
            for EntryPoint { external_id, target: _, signature: _ } in entrypoints {
                self.component_entrypoints
                    .entry(component_id.clone())
                    .or_default()
                    .insert(external_id.clone());
            }
        }
        for (entrypoint_id, TracingResult { retriggers: _, accessed_slots }) in trace_results {
            for contract in accessed_slots.keys() {
                self.entrypoint_contracts
                    .entry(entrypoint_id.clone())
                    .or_default()
                    .insert(contract.clone());
            }
        }
    }
}

/// Writes `value` under `key`, or removes the key when `value` is empty: empty bytes count as
/// absent.
fn write<K: Eq + Hash + Clone>(map: &mut HashMap<K, Bytes>, key: &K, value: &Bytes) {
    if value.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.clone(), value.clone());
    }
}

/// Sets `key` to `value`, or removes it when `value` is absent or empty.
fn replace_bytes(map: &mut HashMap<Address, Bytes>, key: &Address, value: Option<&Bytes>) {
    match value {
        Some(value) => write(map, key, value),
        None => {
            map.remove(key);
        }
    }
}

/// Sets `key` to `inner`, or removes it when `inner` is empty.
fn replace<K: Eq + Hash + Clone, IK, IV>(
    map: &mut HashMap<K, HashMap<IK, IV>>,
    key: &K,
    inner: HashMap<IK, IV>,
) {
    if inner.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.clone(), inner);
    }
}

/// Removes `key` when its inner map is empty, so an empty inner map never differs from a missing
/// one.
fn drop_if_empty<K: Eq + Hash, IK, IV>(map: &mut HashMap<K, HashMap<IK, IV>>, key: &K) {
    if map
        .get(key)
        .is_some_and(HashMap::is_empty)
    {
        map.remove(key);
    }
}

fn same<T: PartialEq>(field: &'static str, own: &T, canonical: &T) -> Result<(), StateMismatch> {
    if own == canonical {
        Ok(())
    } else {
        Err(StateMismatch::Differs { field })
    }
}

/// Checks `canonical ⊆ own ⊆ connected`.
fn bounded<T: Eq + Hash + Debug>(
    field: &'static str,
    own: HashSet<T>,
    canonical: HashSet<T>,
    connected: HashSet<T>,
) -> Result<(), StateMismatch> {
    covers(field, &own, &canonical)?;
    match smallest(own.difference(&connected)) {
        Some(entry) => Err(StateMismatch::Unexpected { field, entry }),
        None => Ok(()),
    }
}

/// Checks `canonical ⊆ own`.
fn covers<T: Eq + Hash + Debug>(
    field: &'static str,
    own: &HashSet<T>,
    canonical: &HashSet<T>,
) -> Result<(), StateMismatch> {
    match smallest(canonical.difference(own)) {
        Some(entry) => Err(StateMismatch::Missing { field, entry }),
        None => Ok(()),
    }
}

/// The entry with the smallest debug form, so a mismatch names the same entry on every run.
fn smallest<'a, T: Debug + 'a>(entries: impl Iterator<Item = &'a T>) -> Option<String> {
    entries
        .map(|entry| format!("{entry:?}"))
        .min()
}

fn keys<K: Eq + Hash, V>(map: &HashMap<K, V>) -> HashSet<&K> {
    map.keys().collect()
}

fn map_pairs<K: Eq + Hash, IK: Eq + Hash, IV>(
    map: &HashMap<K, HashMap<IK, IV>>,
) -> HashSet<(&K, &IK)> {
    let mut pairs = HashSet::new();
    for (key, inner) in map {
        for inner_key in inner.keys() {
            pairs.insert((key, inner_key));
        }
    }
    pairs
}

fn set_pairs<K: Eq + Hash, IK: Eq + Hash>(map: &HashMap<K, HashSet<IK>>) -> HashSet<(&K, &IK)> {
    let mut pairs = HashSet::new();
    for (key, inner) in map {
        for inner_key in inner {
            pairs.insert((key, inner_key));
        }
    }
    pairs
}

/// The rule a client's state breaks in [`DeltaStateModel::check_against`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateMismatch {
    /// `extractor`, `head` or a field reverts restore differs from the canonical state.
    Differs {
        /// Name of the model field.
        field: &'static str,
    },
    /// A field compared by entry lacks an entry the canonical state holds.
    Missing {
        /// Name of the model field.
        field: &'static str,
        /// The missing entry, for the assertion message.
        entry: String,
    },
    /// A field compared by entry holds an entry no message the server sent wrote.
    Unexpected {
        /// Name of the model field.
        field: &'static str,
        /// The unexpected entry, for the assertion message.
        entry: String,
    },
}

/// Knobs of [`generate_history`].
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryConfig {
    /// Number of events to generate: messages and restarts.
    pub length: usize,
    /// Retention depth `W` of the server's delta window (`--delta-window-depth`), in blocks.
    /// Decides which blocks a catch-up can still reach. At least 1.
    pub window_depth: u64,
    /// Blocks between the tip and the finalized block. Reverts never reach below finality.
    pub finality_depth: u64,
    /// Most blocks the committed height trails the finalized one by. The committed height is
    /// `None` until the first commit after every extractor start, and on every revert message.
    pub max_commit_lag: u64,
    /// Chance that the next event is a revert instead of a new block.
    pub revert_probability: f64,
    /// Deepest revert, in blocks below the tip. Reverts never reach below the finalized block or
    /// the first block of the extractor's current run either. Below `window_depth`, so the
    /// server's window always holds the revert target.
    pub max_revert_depth: u64,
    /// Whether a revert may orphan the target of an earlier revert, so that a catch-up path can
    /// go through several reverts.
    pub nested_reverts: bool,
    /// Chance that the next event is an extractor restart. A restart ends every subscription,
    /// clears the window and replays every block above the last committed one.
    pub restart_probability: f64,
    /// Chance that a restart runs an upgraded extractor, which changes the fingerprint.
    pub fingerprint_change_probability: f64,
    /// Chance that the chain reorganised while the extractor was down, so the replay abandons
    /// the unfinalized blocks sent before the restart without a revert message.
    pub downtime_reorg_probability: f64,
    /// Number of component ids blocks draw changes from. Ids repeat, so later blocks overwrite
    /// earlier values and reverts must restore them.
    pub n_components: usize,
    /// Number of account addresses blocks draw changes from.
    pub n_accounts: usize,
    /// Number of entry point ids blocks draw DCI updates from. Ids repeat across branches, so
    /// some entry points are traced on both sides of a revert and some only on the orphaned one.
    pub n_entrypoints: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            length: 200,
            window_depth: 16,
            finality_depth: 8,
            max_commit_lag: 4,
            revert_probability: 0.1,
            max_revert_depth: 6,
            nested_reverts: true,
            restart_probability: 0.015,
            fingerprint_change_probability: 0.3,
            downtime_reorg_probability: 0.4,
            n_components: 8,
            n_accounts: 4,
            n_entrypoints: 6,
        }
    }
}

/// How a restarted extractor relates to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RestartKind {
    /// Same package, module and configuration: the fingerprint is kept and replayed blocks
    /// carry the original deltas, unless the chain reorganised during the downtime.
    Unchanged,
    /// Upgraded extractor: the fingerprint changes, and replayed blocks carry different deltas
    /// than the originals, so a client that resumed across the upgrade ends in a wrong state.
    /// Clients must resync from a snapshot, even when their next block follows their last one.
    Upgraded,
}

/// One thing the server does, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryEvent {
    /// A full-block message as the WebSocket sends it: a new block, or a revert whose block is
    /// the revert target and whose deltas restore the target's state.
    Message(Arc<BlockAggregatedChanges>),
    /// The extractor restarted. Every subscription ends, the server's window is cleared, and the
    /// following messages replay every block above `last_committed`, possibly on a branch that
    /// replaced the one sent before the restart.
    ExtractorRestarted {
        /// Last block the extractor committed; replay starts at its successor.
        last_committed: Block,
        /// Whether the restarted extractor kept its fingerprint.
        kind: RestartKind,
    },
}

/// A generated extractor history.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    /// Everything the server emitted, in order.
    pub events: Vec<HistoryEvent>,
    /// `canonical_states[i]` is the fold of the canonical chain up to the extractor's head right
    /// after `events[i]`. A restart leaves it unchanged; the replay after it starts again from
    /// the last committed block.
    pub canonical_states: Vec<DeltaStateModel>,
    /// `connected_states[i]` is the fold of every message up to `events[i]`: what a client that
    /// applied every message the server sent holds. It differs from the canonical state in the
    /// fields reverts do not undo, and after an extractor restart, which no revert message
    /// covers.
    pub connected_states: Vec<DeltaStateModel>,
    /// The canonical chain at the end of the history, oldest first, from block 1. Full blocks
    /// only, each as last sent.
    pub canonical: Vec<Arc<BlockAggregatedChanges>>,
    /// The configuration the history was generated with.
    pub config: HistoryConfig,
}

impl History {
    /// Every WebSocket message of the history, in order, without the restarts.
    pub fn messages(&self) -> Vec<&BlockAggregatedChanges> {
        let mut messages = Vec::with_capacity(self.events.len());
        for event in &self.events {
            match event {
                HistoryEvent::Message(message) => messages.push(message.as_ref()),
                HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => {}
            }
        }
        messages
    }
}

/// Generates a random history. The same `config` and `seed` always give the same history.
///
/// Every message carries the finalized and committed heights the server's window retention
/// depends on. The committed height trails finality and is `None` on reverts. Revert messages are
/// built the way the indexer builds them today, gaps included: they restore attributes, component
/// balances and slots (empty bytes where the target had no value), never native balance or code,
/// account token balances only for accounts with slot changes, and carry no tokens, TVL, DCI data
/// or committed height.
///
/// The blocks keep to what the indexer's reverts can undo:
/// - Component ids and account addresses are disjoint: the indexer skips restoring slots of an
///   account named like a component created in the reverted range.
/// - A component deletion also deletes the component's attributes and empties its balances in the
///   same block. The indexer restores only keys the reverted range touched, so a revert that
///   re-creates a deleted component would otherwise bring it back without its state.
/// - Absent attributes are always created, never updated, and an account is created only when it
///   has no slots, since the indexer looks prior values up without regard to deletions.
/// - An attribute is deleted, alone or with its component, only once the block that created it is
///   finalized. A revert omits an attribute its range both created and deleted, so a client that
///   applied the creation but not the deletion would keep the attribute after the revert.
///
/// An upgraded extractor writes a marker slot for every block it emits, so its replay differs
/// from the original blocks.
///
/// # Panics
///
/// When `window_depth` is 0, `max_revert_depth` is not below `window_depth`, or a probability
/// lies outside `[0, 1]`.
pub fn generate_history(config: &HistoryConfig, seed: u64) -> History {
    assert!(config.window_depth >= 1, "window_depth must be at least 1");
    assert!(
        config.max_revert_depth < config.window_depth,
        "max_revert_depth ({}) must be below window_depth ({}), or the window may not hold a \
         revert target",
        config.max_revert_depth,
        config.window_depth
    );
    let mut generator = Generator::new(config, seed);
    while generator.events.len() < config.length {
        generator.step();
    }
    generator.finish()
}

/// `message` as a client receives it: converted to the WebSocket DTO and back, so it carries
/// only what the wire carries (no committed height, no `created_attributes`, slots as values).
pub fn through_the_wire(message: &BlockAggregatedChanges) -> BlockAggregatedChanges {
    BlockAggregatedChanges::from(dto::BlockAggregatedChanges::from(message.clone()))
}

/// Where a client's view of a history breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// Index of the last message the client applied before its subscription ended. The client's
    /// head after it is the catch-up's `from`.
    pub last_applied: usize,
    /// Index of the first message of the client's new subscription. The catch-up's `to` is the
    /// parent of that message, or its target when it is a revert.
    pub first_live: usize,
}

/// What a catch-up must do for a [`Gap`].
#[derive(Debug, Clone, PartialEq)]
pub enum CatchUpSpec {
    /// The server must answer with exactly these messages, oldest first. Empty when `from` is
    /// `to`.
    Possible(Vec<Arc<BlockAggregatedChanges>>),
    /// No path can be proven; the client must resync from a snapshot.
    FallBack(FallBackReason),
}

/// Why a catch-up must fall back to a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FallBackReason {
    /// An upgraded extractor restarted inside the gap.
    FingerprintChanged,
    /// The server's window holds no block when the client resubscribes.
    EmptyWindow,
    /// `from` is older than the parent of the window's oldest block.
    BelowFloor,
    /// `from` is not held by the window, was orphaned by no revert the window still remembers,
    /// and is not the parent of the window's oldest block.
    UnknownHash,
    /// `to` is not a canonical block in the window.
    ToNotInWindow,
    /// `to` lies behind the canonical block the walk from `from` reaches: the client received a
    /// revert the window has not applied yet.
    ToNotAhead,
}

/// Says whether a client that missed `gap` must be able to catch up, and with which messages.
///
/// The server's window is rebuilt from the history's messages before `gap.first_live`, as
/// `DeltaWindow` keeps them with `config.window_depth` and a fold batch of 1:
/// - A block stays until it is at or below `min(finalized, db_committed, tip - window_depth)`, from
///   the highest watermarks the blocks carried. Nothing is evicted before the first commit.
/// - A revert purges the blocks above its target and leaves the watermarks alone.
/// - A restart clears the window.
/// - Every revert is journaled with the blocks it orphaned, until its target falls below the
///   window's oldest block or the window is cleared.
///
/// `from` is the block of `gap.last_applied`; `to` is the parent of `gap.first_live`, or its
/// target when it is a revert. The rules, in order:
/// 1. An upgraded restart inside the gap: [`FallBackReason::FingerprintChanged`].
/// 2. An empty window: [`FallBackReason::EmptyWindow`].
/// 3. `to` not held by the window: [`FallBackReason::ToNotInWindow`].
/// 4. Walk from `from` to a canonical block: through the journaled revert that orphaned it, then
///    from that revert's target, until reaching a block the window holds or the parent of its
///    oldest block. A walk that gets stuck is [`FallBackReason::BelowFloor`] when `from` is older
///    than that parent, else [`FallBackReason::UnknownHash`].
/// 5. A walk that ends past `to` is [`FallBackReason::ToNotAhead`]. Otherwise the path is the
///    reverts walked through, then the window's blocks after the walk's end up to `to`.
///
/// # Panics
///
/// When `gap` does not lie inside the history, `gap.last_applied` or `gap.first_live` is not a
/// message, or the history's messages do not form a chain the window accepts.
pub fn catch_up_spec(history: &History, gap: Gap) -> CatchUpSpec {
    let Gap { last_applied, first_live } = gap;
    assert!(
        last_applied < first_live && first_live < history.events.len(),
        "gap {gap:?} does not lie inside a history of {} events",
        history.events.len()
    );
    let HistoryEvent::Message(last) = &history.events[last_applied] else {
        panic!("event {last_applied} is not a message the client could apply");
    };
    let HistoryEvent::Message(live) = &history.events[first_live] else {
        panic!("event {first_live} is not a message a subscription could start with");
    };

    for event in &history.events[last_applied + 1..first_live] {
        match event {
            HistoryEvent::ExtractorRestarted { last_committed: _, kind: RestartKind::Upgraded } => {
                return CatchUpSpec::FallBack(FallBackReason::FingerprintChanged);
            }
            HistoryEvent::ExtractorRestarted {
                last_committed: _,
                kind: RestartKind::Unchanged,
            } |
            HistoryEvent::Message(_) => {}
        }
    }

    let mut window = Window::new(history.config.window_depth);
    for event in &history.events[..first_live] {
        window.apply(event);
    }
    let to = if live.revert { &live.block.hash } else { &live.block.parent_hash };
    window.path(&last.block, to)
}

/// One revert the server's window applied.
#[derive(Debug)]
struct JournalEntry {
    revert: Arc<BlockAggregatedChanges>,
    /// Hashes of the blocks the revert removed from the window.
    orphaned: Vec<BlockHash>,
}

/// The server's delta window for one extractor, with its revert journal.
#[derive(Debug)]
struct Window {
    depth: u64,
    /// Contiguous canonical blocks, oldest first.
    blocks: Vec<Arc<BlockAggregatedChanges>>,
    finalized: Option<u64>,
    committed: Option<u64>,
    /// Reverts applied, oldest first.
    journal: Vec<JournalEntry>,
}

impl Window {
    fn new(depth: u64) -> Self {
        Self { depth, blocks: Vec::new(), finalized: None, committed: None, journal: Vec::new() }
    }

    fn apply(&mut self, event: &HistoryEvent) {
        match event {
            HistoryEvent::Message(message) if message.revert => self.revert(message),
            HistoryEvent::Message(message) => self.insert(message),
            HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => self.clear(),
        }
    }

    fn insert(&mut self, message: &Arc<BlockAggregatedChanges>) {
        if let Some(tip) = self.blocks.last() {
            assert_eq!(
                tip.block.hash, message.block.parent_hash,
                "block {} does not extend the window's tip {}",
                message.block.number, tip.block.number
            );
        }
        self.blocks.push(Arc::clone(message));
        let finalized = message.finalized_block_height;
        self.finalized = Some(
            self.finalized
                .map_or(finalized, |held| held.max(finalized)),
        );
        if let Some(committed) = message.db_committed_block_height {
            self.committed = Some(
                self.committed
                    .map_or(committed, |held| held.max(committed)),
            );
        }
        self.evict();
    }

    fn revert(&mut self, message: &Arc<BlockAggregatedChanges>) {
        let target = &message.block;
        let irreversible = match (self.finalized, self.committed) {
            (Some(finalized), Some(committed)) => Some(finalized.min(committed)),
            (Some(finalized), None) => Some(finalized),
            (None, _) => None,
        };
        assert!(
            irreversible.is_none_or(|height| target.number >= height),
            "revert to block {} below the irreversible height {irreversible:?}",
            target.number
        );
        let position = self
            .position(&target.hash)
            .unwrap_or_else(|| {
                panic!("revert to block {} the window does not hold", target.number)
            });
        let orphaned = self
            .blocks
            .split_off(position + 1)
            .into_iter()
            .map(|orphan| orphan.block.hash.clone())
            .collect();
        self.journal
            .push(JournalEntry { revert: Arc::clone(message), orphaned });
        self.evict();
    }

    fn clear(&mut self) {
        self.blocks.clear();
        self.journal.clear();
        self.finalized = None;
        self.committed = None;
    }

    /// Evicts every block at or below `min(finalized, committed, tip - depth)`, then forgets the
    /// reverts whose target left the window.
    fn evict(&mut self) {
        let (Some(tip), Some(finalized), Some(committed)) =
            (self.blocks.last(), self.finalized, self.committed)
        else {
            return;
        };
        let bound = finalized.min(committed).min(
            tip.block
                .number
                .saturating_sub(self.depth),
        );
        let evicted = self
            .blocks
            .iter()
            .take_while(|block| block.block.number <= bound)
            .count();
        self.blocks.drain(..evicted);
        if let Some(oldest) = self.blocks.first() {
            let oldest = oldest.block.number;
            self.journal
                .retain(|entry| entry.revert.block.number >= oldest);
        }
    }

    fn position(&self, hash: &BlockHash) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| &block.block.hash == hash)
    }

    /// The newest journaled revert that orphaned `hash`.
    fn orphaned_by(&self, hash: &BlockHash) -> Option<&JournalEntry> {
        self.journal
            .iter()
            .rev()
            .find(|entry| entry.orphaned.contains(hash))
    }

    /// The messages that take a client from `from` to the block `to`; see [`catch_up_spec`].
    fn path(&self, from: &Block, to: &BlockHash) -> CatchUpSpec {
        let Some(oldest) = self.blocks.first() else {
            return CatchUpSpec::FallBack(FallBackReason::EmptyWindow);
        };
        let Some(to_position) = self.position(to) else {
            return CatchUpSpec::FallBack(FallBackReason::ToNotInWindow);
        };
        let mut path = Vec::new();
        let mut current = from.hash.clone();
        // Index of the first window block the path continues with.
        let continue_at = loop {
            if let Some(position) = self.position(&current) {
                break position + 1;
            }
            if current == oldest.block.parent_hash {
                break 0;
            }
            let Some(entry) = self.orphaned_by(&current) else {
                let reason = if from.number + 1 < oldest.block.number {
                    FallBackReason::BelowFloor
                } else {
                    FallBackReason::UnknownHash
                };
                return CatchUpSpec::FallBack(reason);
            };
            path.push(Arc::clone(&entry.revert));
            current = entry.revert.block.hash.clone();
        };
        if continue_at > to_position + 1 {
            return CatchUpSpec::FallBack(FallBackReason::ToNotAhead);
        }
        path.extend(
            self.blocks[continue_at..=to_position]
                .iter()
                .cloned(),
        );
        CatchUpSpec::Possible(path)
    }
}

/// Attribute names components draw from. Few, so blocks overwrite and delete earlier values.
const ATTRIBUTES: [&str; 3] = ["a", "b", "c"];
/// Number of storage slots accounts draw from.
const SLOTS: u8 = 4;
/// Number of token addresses components and accounts draw from.
const TOKENS: usize = 4;
/// Number of contracts traces reach that are not indexed accounts.
const UNINDEXED_CONTRACTS: usize = 2;

/// Chance that a block deletes one existing component.
const DELETE_COMPONENT: f64 = 0.06;
/// Chance that a block creates one component that does not exist.
const CREATE_COMPONENT: f64 = 0.3;
/// Chance that a block changes an existing component's attributes; its balances; its TVL.
const CHANGE_COMPONENT: f64 = 0.35;
/// Chance that a component change writes a given attribute.
const WRITE_ATTRIBUTE: f64 = 0.5;
/// Chance that a write to an existing attribute deletes it.
const DELETE_ATTRIBUTE: f64 = 0.2;
/// Chance that a block changes an account's storage; an account's token balance.
const CHANGE_ACCOUNT: f64 = 0.3;
/// Chance that a block carries a DCI update.
const UPDATE_DCI: f64 = 0.35;
/// Chance that a DCI update links its entry point to a component, beside tracing it.
const LINK_ENTRYPOINT: f64 = 0.7;
/// Chance that the first commit of an extractor run waits for another block.
const DELAY_FIRST_COMMIT: f64 = 0.5;

/// Address kinds, the first bytes of an address, which keep the address families apart.
const ACCOUNT: u8 = 0xac;
const TOKEN: u8 = 0x70;
const CONTRACT: u8 = 0xc0;
const MARKER: u8 = 0xff;

fn address(kind: u8, index: usize) -> Address {
    let mut bytes = vec![kind; 12];
    bytes.extend_from_slice(&(index as u64).to_be_bytes());
    Bytes::from(bytes)
}

fn component_id(index: usize) -> ComponentId {
    format!("component-{index}")
}

fn entrypoint_id(index: usize) -> EntryPointId {
    format!("entrypoint-{index}")
}

/// A non-empty random value.
fn value(rng: &mut StdRng) -> Bytes {
    Bytes::from(rng.gen::<u32>().to_be_bytes().to_vec())
}

/// Picks one of `items` uniformly.
///
/// # Panics
///
/// When `items` is empty.
fn pick<'a, T>(rng: &mut StdRng, items: &'a [T]) -> &'a T {
    &items[rng.gen_range(0..items.len())]
}

/// A block on the extractor's current chain.
struct ChainBlock {
    /// The message last sent for the block.
    message: Arc<BlockAggregatedChanges>,
    /// The block's deltas as an unchanged extractor replays them: no watermarks, no version
    /// marker.
    content: Arc<BlockAggregatedChanges>,
    /// The fold of the chain up to and including this block.
    state: DeltaStateModel,
}

/// The extractor and its chain while [`generate_history`] runs.
struct Generator<'a> {
    config: &'a HistoryConfig,
    rng: StdRng,
    /// The extractor's chain, block `n` at index `n - 1`.
    chain: Vec<ChainBlock>,
    /// Hashes handed out so far; block hashes are unique across the history.
    hashes: u64,
    /// The chain's finalized height. Never decreases, also across restarts.
    finalized: u64,
    /// The committed height of the current extractor run, `None` until its first commit.
    committed: Option<u64>,
    /// The database's committed height, across runs.
    db_cursor: Option<u64>,
    /// The first block the current extractor run sends.
    run_start: u64,
    /// The extractor version; an upgrade increments it.
    version: u32,
    /// Height the chain drops back to before the next message: the last committed block of a
    /// restart. Applied late so a restart, or several in a row, leave the canonical state
    /// unchanged.
    rewind_to: Option<u64>,
    /// Blocks the restarted extractor still replays.
    replay: VecDeque<Arc<BlockAggregatedChanges>>,
    last_revert_target: Option<u64>,
    events: Vec<HistoryEvent>,
    canonical_states: Vec<DeltaStateModel>,
    connected_states: Vec<DeltaStateModel>,
    connected: DeltaStateModel,
}

impl<'a> Generator<'a> {
    fn new(config: &'a HistoryConfig, seed: u64) -> Self {
        Self {
            config,
            rng: StdRng::seed_from_u64(seed),
            chain: Vec::new(),
            hashes: 0,
            finalized: 0,
            committed: None,
            db_cursor: None,
            run_start: 1,
            version: 0,
            rewind_to: None,
            replay: VecDeque::new(),
            last_revert_target: None,
            events: Vec::new(),
            canonical_states: Vec::new(),
            connected_states: Vec::new(),
            connected: DeltaStateModel::default(),
        }
    }

    fn step(&mut self) {
        let event = match self.replay.pop_front() {
            Some(content) => {
                self.rewind();
                HistoryEvent::Message(self.emit(content))
            }
            None => self.draw_event(),
        };
        if let HistoryEvent::Message(message) = &event {
            self.connected.apply(message);
        }
        let canonical = self
            .chain
            .last()
            .map(|block| block.state.clone())
            .unwrap_or_default();
        self.canonical_states.push(canonical);
        self.connected_states
            .push(self.connected.clone());
        self.events.push(event);
    }

    fn draw_event(&mut self) -> HistoryEvent {
        let roll: f64 = self.rng.gen();
        let restart = self.config.restart_probability;
        if roll < restart {
            if let Some(event) = self.restart() {
                return event;
            }
        }
        self.rewind();
        if (restart..restart + self.config.revert_probability).contains(&roll) {
            if let Some(revert) = self.revert() {
                return HistoryEvent::Message(revert);
            }
        }
        let content = self.new_block();
        HistoryEvent::Message(self.emit(content))
    }

    /// Drops the blocks above the last committed one after a restart, which the replay sends
    /// again or replaces.
    fn rewind(&mut self) {
        if let Some(last_committed) = self.rewind_to.take() {
            self.chain
                .truncate(last_committed as usize);
        }
    }

    /// Restarts the extractor if it ever committed a block.
    fn restart(&mut self) -> Option<HistoryEvent> {
        let committed = self.db_cursor?;
        let tip = self.chain.last()?.message.block.number;
        let last_committed = self.chain[committed as usize - 1]
            .message
            .block
            .clone();
        let kind = if self.rng.gen_bool(
            self.config
                .fingerprint_change_probability,
        ) {
            RestartKind::Upgraded
        } else {
            RestartKind::Unchanged
        };
        let lowest_fork = committed.max(self.finalized);
        let fork = if lowest_fork < tip &&
            self.rng
                .gen_bool(self.config.downtime_reorg_probability)
        {
            self.rng.gen_range(lowest_fork..tip)
        } else {
            tip
        };
        self.replay = self.chain[committed as usize..fork as usize]
            .iter()
            .map(|block| Arc::clone(&block.content))
            .collect();
        self.rewind_to = Some(committed);
        self.run_start = committed + 1;
        self.committed = None;
        if kind == RestartKind::Upgraded {
            self.version += 1;
        }
        Some(HistoryEvent::ExtractorRestarted { last_committed, kind })
    }

    /// Reverts to a random unfinalized block of the current run, if there is one.
    fn revert(&mut self) -> Option<Arc<BlockAggregatedChanges>> {
        let tip = self.chain.last()?.message.block.number;
        let mut lowest = tip
            .saturating_sub(self.config.max_revert_depth)
            .max(self.finalized)
            .max(self.run_start);
        if !self.config.nested_reverts {
            lowest = lowest.max(self.last_revert_target.unwrap_or(0));
        }
        if lowest >= tip {
            return None;
        }
        let target = self.rng.gen_range(lowest..tip);
        let target_index = target as usize - 1;
        let message = revert_message(&self.chain[target_index], &self.chain[target_index + 1..]);
        self.chain.truncate(target_index + 1);
        self.last_revert_target = Some(target);
        Some(Arc::new(message))
    }

    /// The content of a new block on top of the chain.
    fn new_block(&mut self) -> Arc<BlockAggregatedChanges> {
        self.hashes += 1;
        let hash = Bytes::from(self.hashes).lpad(32, 0);
        let empty = DeltaStateModel::default();
        let (number, parent_hash, state) = match self.chain.last() {
            Some(parent) => {
                (parent.message.block.number + 1, parent.message.block.hash.clone(), &parent.state)
            }
            None => (1, Bytes::from(0u64).lpad(32, 0), &empty),
        };
        let block = Block::new(number, CHAIN, hash, parent_hash, NaiveDateTime::default());
        let young = young_attributes(&self.chain, self.finalized);
        Arc::new(draw_block(&mut self.rng, self.config, block, state, &young))
    }

    /// Sends `content` as the next block, with the current watermarks and version marker.
    fn emit(&mut self, content: Arc<BlockAggregatedChanges>) -> Arc<BlockAggregatedChanges> {
        let number = content.block.number;
        debug_assert_eq!(number, self.chain.len() as u64 + 1, "blocks must extend the chain");
        self.finalized = self
            .finalized
            .max(number.saturating_sub(self.config.finality_depth));
        let committed = self.draw_committed(number);
        let mut message = BlockAggregatedChanges {
            finalized_block_height: self.finalized,
            db_committed_block_height: committed,
            ..(*content).clone()
        };
        if self.version > 0 {
            mark(&mut message, self.version);
        }
        let message = Arc::new(message);
        let mut state = self
            .chain
            .last()
            .map(|block| block.state.clone())
            .unwrap_or_default();
        state.apply(&message);
        self.chain
            .push(ChainBlock { message: Arc::clone(&message), content, state });
        message
    }

    /// The committed height a message for block `number` carries: within `max_commit_lag` of
    /// finality, never above the block, and `None` until the run's first commit.
    fn draw_committed(&mut self, number: u64) -> Option<u64> {
        let highest = self.finalized.min(number);
        if highest < self.run_start {
            return self.committed;
        }
        let lag_floor = self
            .finalized
            .saturating_sub(self.config.max_commit_lag);
        if self.committed.is_none() &&
            lag_floor < self.run_start &&
            self.rng.gen_bool(DELAY_FIRST_COMMIT)
        {
            return None;
        }
        let lowest = self
            .committed
            .unwrap_or(self.run_start)
            .max(lag_floor)
            .min(highest);
        let committed = self.rng.gen_range(lowest..=highest);
        self.committed = Some(committed);
        self.db_cursor = Some(committed);
        Some(committed)
    }

    fn finish(self) -> History {
        History {
            events: self.events,
            canonical_states: self.canonical_states,
            connected_states: self.connected_states,
            canonical: self
                .chain
                .iter()
                .map(|block| Arc::clone(&block.message))
                .collect(),
            config: self.config.clone(),
        }
    }
}

/// Adds the marker slot an extractor of `version` writes for every block it sends.
fn mark(message: &mut BlockAggregatedChanges, version: u32) {
    let marker = address(MARKER, 0);
    let slot = Bytes::from(message.block.number);
    let delta = AccountDelta::new(
        CHAIN,
        marker.clone(),
        HashMap::from([(slot, Some(Bytes::from(version)))]),
        None,
        None,
        ChangeType::Update,
    );
    message
        .account_deltas
        .insert(marker, delta);
}

/// The attributes whose current value was first created above `finalized`, so a revert can still
/// undo their creation.
fn young_attributes(chain: &[ChainBlock], finalized: u64) -> HashSet<(ComponentId, AttrStoreKey)> {
    let mut young = HashSet::new();
    for block in chain.iter().skip(finalized as usize) {
        for (id, delta) in &block.message.state_deltas {
            for name in &delta.deleted_attributes {
                young.remove(&(id.clone(), name.clone()));
            }
            for name in &delta.created_attributes {
                young.insert((id.clone(), name.clone()));
            }
        }
    }
    young
}

/// Random deltas for `block` on top of `state`, without watermarks. `young` attributes are not
/// deleted.
fn draw_block(
    rng: &mut StdRng,
    config: &HistoryConfig,
    block: Block,
    state: &DeltaStateModel,
    young: &HashSet<(ComponentId, AttrStoreKey)>,
) -> BlockAggregatedChanges {
    let tx = block.hash.clone();
    let mut message = BlockAggregatedChanges {
        extractor: EXTRACTOR.to_string(),
        chain: CHAIN,
        block,
        ..Default::default()
    };

    let mut live = Vec::new();
    let mut absent = Vec::new();
    for index in 0..config.n_components {
        let id = component_id(index);
        if state.components.contains_key(&id) {
            live.push(id);
        } else {
            absent.push(id);
        }
    }
    let deletable: Vec<ComponentId> = live
        .iter()
        .filter(|id| {
            !young
                .iter()
                .any(|(component, _)| component == *id)
        })
        .cloned()
        .collect();
    if !deletable.is_empty() && rng.gen_bool(DELETE_COMPONENT) {
        let deleted = pick(rng, &deletable).clone();
        delete_component(&mut message, state, &deleted);
        live.retain(|id| id != &deleted);
    }
    let created = if !absent.is_empty() && rng.gen_bool(CREATE_COMPONENT) {
        let id = pick(rng, &absent).clone();
        create_component(rng, &mut message, state, &id);
        change_component(rng, &mut message, state, &id, Lifetime::Created);
        Some(id)
    } else {
        None
    };
    for id in &live {
        if rng.gen_bool(CHANGE_COMPONENT) {
            change_component(rng, &mut message, state, id, Lifetime::Existing(young));
        }
    }

    for index in 0..config.n_accounts {
        if rng.gen_bool(CHANGE_ACCOUNT) {
            change_account(rng, &mut message, state, index);
        }
        if rng.gen_bool(CHANGE_ACCOUNT) {
            let account = address(ACCOUNT, index);
            let token = address(TOKEN, rng.gen_range(0..TOKENS));
            let balance =
                AccountBalance::new(account.clone(), token.clone(), value(rng), tx.clone());
            message
                .account_balances
                .entry(account)
                .or_default()
                .insert(token, balance);
        }
    }

    if config.n_entrypoints > 0 && rng.gen_bool(UPDATE_DCI) {
        let linkable: Vec<&ComponentId> = live.iter().chain(&created).collect();
        update_dci(rng, config, &mut message, &linkable);
    }
    message
}

/// Deletes `id` like a normal block does, in `new_protocol_components`, and deletes or empties
/// every attribute and balance it holds in the same block.
fn delete_component(
    message: &mut BlockAggregatedChanges,
    state: &DeltaStateModel,
    id: &ComponentId,
) {
    let component = state.components[id].clone();
    message
        .new_protocol_components
        .insert(id.clone(), ProtocolComponent { change: ChangeType::Deletion, ..component });
    if let Some(attributes) = state.component_attributes.get(id) {
        let deleted = attributes.keys().cloned().collect();
        message
            .state_deltas
            .insert(id.clone(), ProtocolComponentStateDelta::new(id, HashMap::new(), deleted));
    }
    if let Some(balances) = state.component_balances.get(id) {
        let mut emptied = HashMap::new();
        for token in balances.keys() {
            let balance = ComponentBalance::new(
                token.clone(),
                Bytes::new(),
                0.0,
                message.block.hash.clone(),
                id,
            );
            emptied.insert(token.clone(), balance);
        }
        message
            .component_balances
            .insert(id.clone(), emptied);
    }
}

fn create_component(
    rng: &mut StdRng,
    message: &mut BlockAggregatedChanges,
    state: &DeltaStateModel,
    id: &ComponentId,
) {
    let first = rng.gen_range(0..TOKENS);
    let second = (first + rng.gen_range(1..TOKENS)) % TOKENS;
    let tokens = vec![address(TOKEN, first), address(TOKEN, second)];
    for token in &tokens {
        if !state.tokens.contains_key(token) {
            let metadata = Token::new(token, "TKN", 18, 0, &[], CHAIN, 100);
            message
                .new_tokens
                .insert(token.clone(), metadata);
        }
    }
    let component = ProtocolComponent {
        id: id.clone(),
        protocol_system: EXTRACTOR.to_string(),
        protocol_type_name: "pool".to_string(),
        chain: CHAIN,
        tokens,
        contract_addresses: Vec::new(),
        static_attributes: HashMap::new(),
        change: ChangeType::Creation,
        creation_tx: message.block.hash.clone(),
        created_at: message.block.ts,
    };
    message
        .new_protocol_components
        .insert(id.clone(), component);
}

/// Whether a component a block changes already existed.
enum Lifetime<'a> {
    /// The block creates the component.
    Created,
    /// The component existed; the attributes it holds that a revert could still undo the
    /// creation of are not deleted.
    Existing(&'a HashSet<(ComponentId, AttrStoreKey)>),
}

/// Writes or deletes some attributes, balances and TVL of `id`.
fn change_component(
    rng: &mut StdRng,
    message: &mut BlockAggregatedChanges,
    state: &DeltaStateModel,
    id: &ComponentId,
    lifetime: Lifetime,
) {
    let held = state.component_attributes.get(id);
    let mut delta = ProtocolComponentStateDelta::new(id, HashMap::new(), HashSet::new());
    for name in ATTRIBUTES {
        if !rng.gen_bool(WRITE_ATTRIBUTE) {
            continue;
        }
        let name = name.to_string();
        let (exists, deletable) = match lifetime {
            Lifetime::Created => (false, false),
            Lifetime::Existing(young) => {
                let exists = held.is_some_and(|attributes| attributes.contains_key(&name));
                (exists, exists && !young.contains(&(id.clone(), name.clone())))
            }
        };
        if deletable && rng.gen_bool(DELETE_ATTRIBUTE) {
            delta.deleted_attributes.insert(name);
            continue;
        }
        if !exists {
            delta
                .created_attributes
                .insert(name.clone());
        }
        delta
            .updated_attributes
            .insert(name, value(rng));
    }
    if !delta.updated_attributes.is_empty() || !delta.deleted_attributes.is_empty() {
        message
            .state_deltas
            .insert(id.clone(), delta);
    }

    let tokens = match lifetime {
        Lifetime::Created => message.new_protocol_components[id]
            .tokens
            .clone(),
        Lifetime::Existing(_) => state.components[id].tokens.clone(),
    };
    if rng.gen_bool(CHANGE_COMPONENT) {
        let mut balances = HashMap::new();
        for token in tokens {
            let balance = ComponentBalance::new(
                token.clone(),
                value(rng),
                rng.gen_range(0.0..1e6),
                message.block.hash.clone(),
                id,
            );
            balances.insert(token, balance);
        }
        message
            .component_balances
            .insert(id.clone(), balances);
    }
    if rng.gen_bool(CHANGE_COMPONENT) {
        message
            .component_tvl
            .insert(id.clone(), rng.gen_range(0.0..1e6));
    }
}

/// Writes some slots of account `index`, creating the account when it has none.
fn change_account(
    rng: &mut StdRng,
    message: &mut BlockAggregatedChanges,
    state: &DeltaStateModel,
    index: usize,
) {
    let account = address(ACCOUNT, index);
    let mut slots = HashMap::new();
    for _ in 0..rng.gen_range(1..=2) {
        let slot = Bytes::from(vec![rng.gen_range(0..SLOTS)]);
        slots.insert(slot, Some(value(rng)));
    }
    let delta = if state
        .account_storage
        .contains_key(&account)
    {
        let balance = rng
            .gen_bool(CHANGE_ACCOUNT)
            .then(|| value(rng));
        AccountDelta::new(CHAIN, account.clone(), slots, balance, None, ChangeType::Update)
    } else {
        let code = value(rng);
        let balance = value(rng);
        AccountDelta::new(
            CHAIN,
            account.clone(),
            slots,
            Some(balance),
            Some(code),
            ChangeType::Creation,
        )
    };
    message
        .account_deltas
        .insert(account, delta);
}

/// Traces a random entry point and links it to one of `linkable` with [`LINK_ENTRYPOINT`].
fn update_dci(
    rng: &mut StdRng,
    config: &HistoryConfig,
    message: &mut BlockAggregatedChanges,
    linkable: &[&ComponentId],
) {
    let entrypoint = entrypoint_id(rng.gen_range(0..config.n_entrypoints));
    let contracts = config.n_accounts + UNINDEXED_CONTRACTS;
    let mut accessed = HashMap::new();
    for _ in 0..rng.gen_range(1..=2) {
        let index = rng.gen_range(0..contracts);
        let contract = if index < config.n_accounts {
            address(ACCOUNT, index)
        } else {
            address(CONTRACT, index)
        };
        accessed.insert(contract, HashSet::from([Bytes::from(vec![rng.gen_range(0..SLOTS)])]));
    }
    let target = accessed
        .keys()
        .min()
        .cloned()
        .unwrap_or_default();
    if !linkable.is_empty() && rng.gen_bool(LINK_ENTRYPOINT) {
        let component = (*pick(rng, linkable)).clone();
        let linked = EntryPoint::new(entrypoint.clone(), target, "swap()".to_string());
        let params = TracingParams::RPCTracer(RPCTracerParams {
            caller: None,
            calldata: value(rng),
            state_overrides: None,
            prune_addresses: None,
        });
        message
            .dci_update
            .new_entrypoints
            .insert(component.clone(), HashSet::from([linked]));
        message
            .dci_update
            .new_entrypoint_params
            .insert(entrypoint.clone(), HashSet::from([(params, component)]));
    }
    message
        .dci_update
        .trace_results
        .insert(entrypoint, TracingResult::new(HashSet::new(), accessed));
}

/// How a revert treats one attribute, like the indexer's `AttrRevert`.
enum AttrRevert {
    /// The attribute had a value before the range: restore it.
    ExistedBefore,
    /// The range created the attribute: delete it if it is still alive at the end of the range.
    CreatedInRange { alive: bool },
}

/// One event on an attribute, in chronological order, like the indexer's `AttrEvent`.
enum AttrEvent {
    Created,
    Updated,
    Deleted,
}

/// Records `event` on an attribute: its first event decides its class, later events only whether
/// an attribute created in the range is still alive.
fn classify(
    classes: &mut HashMap<(ComponentId, AttrStoreKey), AttrRevert>,
    (component_id, name): (&ComponentId, &AttrStoreKey),
    event: AttrEvent,
) {
    match classes.entry((component_id.clone(), name.clone())) {
        Entry::Occupied(mut held) => {
            if let AttrRevert::CreatedInRange { alive } = held.get_mut() {
                *alive = !matches!(event, AttrEvent::Deleted);
            }
        }
        Entry::Vacant(slot) => {
            slot.insert(match event {
                AttrEvent::Created => AttrRevert::CreatedInRange { alive: true },
                AttrEvent::Updated | AttrEvent::Deleted => AttrRevert::ExistedBefore,
            });
        }
    }
}

/// The revert message the indexer sends when the chain drops every block of `reverted`, back to
/// `target`. Values come from the target's state, as the indexer reads them from its buffer and
/// database.
fn revert_message(target: &ChainBlock, reverted: &[ChainBlock]) -> BlockAggregatedChanges {
    let prior = &target.state;

    // Only the oldest creation or deletion of a component in the range is undone.
    let mut recreated = HashMap::new();
    let mut removed = HashMap::new();
    for block in reverted {
        for (id, component) in &block.message.new_protocol_components {
            if recreated.contains_key(id) || removed.contains_key(id) {
                continue;
            }
            match component.change {
                ChangeType::Update => {}
                ChangeType::Deletion => {
                    let undone =
                        ProtocolComponent { change: ChangeType::Creation, ..component.clone() };
                    recreated.insert(id.clone(), undone);
                }
                ChangeType::Creation => {
                    let undone =
                        ProtocolComponent { change: ChangeType::Deletion, ..component.clone() };
                    removed.insert(id.clone(), undone);
                }
            }
        }
    }

    // Like the indexer, skips accounts named like a component the range created (deferred
    // bug 5), and restores neither native balance nor code (deferred bug 1).
    let mut account_deltas: HashMap<Address, AccountDelta> = HashMap::new();
    for block in reverted {
        for (address, delta) in &block.message.account_deltas {
            if removed.contains_key(&address.to_string()) {
                continue;
            }
            for slot in delta.slots.keys() {
                let restored = prior
                    .account_storage
                    .get(address)
                    .and_then(|slots| slots.get(slot))
                    .cloned()
                    .unwrap_or_default();
                account_deltas
                    .entry(address.clone())
                    .or_insert_with(|| {
                        AccountDelta::new(
                            CHAIN,
                            address.clone(),
                            HashMap::new(),
                            None,
                            None,
                            ChangeType::Update,
                        )
                    })
                    .slots
                    .insert(slot.clone(), Some(restored));
            }
        }
    }

    let mut classes = HashMap::new();
    for block in reverted {
        for (id, delta) in &block.message.state_deltas {
            if removed.contains_key(id) {
                continue;
            }
            for name in &delta.created_attributes {
                let event = if delta.deleted_attributes.contains(name) {
                    AttrEvent::Updated
                } else {
                    AttrEvent::Created
                };
                classify(&mut classes, (id, name), event);
            }
            for name in delta.updated_attributes.keys() {
                if !delta.created_attributes.contains(name) {
                    classify(&mut classes, (id, name), AttrEvent::Updated);
                }
            }
            for name in &delta.deleted_attributes {
                if !delta.created_attributes.contains(name) {
                    classify(&mut classes, (id, name), AttrEvent::Deleted);
                }
            }
        }
    }
    let mut state_deltas: HashMap<ComponentId, ProtocolComponentStateDelta> = HashMap::new();
    for ((id, name), class) in classes {
        let restored = match class {
            AttrRevert::ExistedBefore => prior
                .component_attributes
                .get(&id)
                .and_then(|attributes| attributes.get(&name))
                .cloned(),
            AttrRevert::CreatedInRange { alive: true } => None,
            AttrRevert::CreatedInRange { alive: false } => continue,
        };
        let delta = state_deltas
            .entry(id.clone())
            .or_insert_with(|| {
                ProtocolComponentStateDelta::new(&id, HashMap::new(), HashSet::new())
            });
        match restored {
            Some(value) => {
                delta
                    .updated_attributes
                    .insert(name, value);
            }
            None => {
                delta.deleted_attributes.insert(name);
            }
        }
    }

    let mut component_balances: HashMap<ComponentId, HashMap<Address, ComponentBalance>> =
        HashMap::new();
    for block in reverted {
        for (id, balances) in &block.message.component_balances {
            if removed.contains_key(id) {
                continue;
            }
            for token in balances.keys() {
                let restored = prior
                    .component_balances
                    .get(id)
                    .and_then(|balances| balances.get(token))
                    .cloned()
                    .unwrap_or_default();
                let balance = ComponentBalance::new(token.clone(), restored, 0.0, Bytes::new(), id);
                component_balances
                    .entry(id.clone())
                    .or_default()
                    .insert(token.clone(), balance);
            }
        }
    }

    let mut account_balances: HashMap<Address, HashMap<Address, AccountBalance>> = HashMap::new();
    for block in reverted {
        for (account, balances) in &block.message.account_balances {
            if !account_deltas.contains_key(account) {
                continue;
            }
            for token in balances.keys() {
                let restored = prior
                    .account_token_balances
                    .get(account)
                    .and_then(|balances| balances.get(token))
                    .cloned()
                    .unwrap_or_default();
                let balance =
                    AccountBalance::new(account.clone(), token.clone(), restored, Bytes::new());
                account_balances
                    .entry(account.clone())
                    .or_default()
                    .insert(token.clone(), balance);
            }
        }
    }

    let newest = reverted
        .last()
        .expect("a revert drops at least one block");
    BlockAggregatedChanges {
        extractor: EXTRACTOR.to_string(),
        chain: CHAIN,
        block: target.message.block.clone(),
        finalized_block_height: newest.message.finalized_block_height,
        db_committed_block_height: None,
        revert: true,
        state_deltas,
        account_deltas,
        new_tokens: HashMap::new(),
        new_protocol_components: recreated,
        deleted_protocol_components: removed,
        component_balances,
        account_balances,
        component_tvl: HashMap::new(),
        dci_update: DCIUpdate::default(),
        partial_block_index: None,
    }
}

#[cfg(test)]
mod test {
    use std::fmt::Display;

    use rstest::rstest;

    use super::*;
    use crate::models::BlockHash;

    /// Histories each property runs over.
    const SEEDS: u64 = 128;
    /// Random gaps the catch-up property draws per history, besides the gaps around restarts.
    const RANDOM_GAPS: usize = 40;

    fn address(n: u8) -> Address {
        Bytes::from(vec![n; 20])
    }

    fn hash(number: u64, branch: u8) -> BlockHash {
        let mut bytes = vec![branch];
        bytes.extend_from_slice(&number.to_be_bytes());
        Bytes::from(bytes)
    }

    fn block(number: u64, branch: u8, parent_branch: u8) -> Block {
        Block::new(
            number,
            CHAIN,
            hash(number, branch),
            hash(number.saturating_sub(1), parent_branch),
            Default::default(),
        )
    }

    fn full_block(number: u64, branch: u8, parent_branch: u8) -> Arc<BlockAggregatedChanges> {
        Arc::new(BlockAggregatedChanges {
            extractor: EXTRACTOR.to_string(),
            chain: CHAIN,
            block: block(number, branch, parent_branch),
            ..Default::default()
        })
    }

    /// A full block on branch 0 that carries the given watermarks.
    fn block_with_watermarks(
        number: u64,
        finalized: u64,
        committed: Option<u64>,
    ) -> Arc<BlockAggregatedChanges> {
        Arc::new(BlockAggregatedChanges {
            finalized_block_height: finalized,
            db_committed_block_height: committed,
            ..(*full_block(number, 0, 0)).clone()
        })
    }

    /// A full block on branch 0 whose watermarks make every block at or below `number`
    /// finalized and committed.
    fn committed_block(number: u64) -> Arc<BlockAggregatedChanges> {
        block_with_watermarks(number, number, Some(number))
    }

    fn revert_to(number: u64, branch: u8, parent_branch: u8) -> Arc<BlockAggregatedChanges> {
        Arc::new(BlockAggregatedChanges {
            revert: true,
            ..(*full_block(number, branch, parent_branch)).clone()
        })
    }

    /// A full block that links `entrypoint` to `component` and traces it to `contract`.
    fn block_with_dci(
        number: u64,
        branch: u8,
        parent_branch: u8,
        component: &str,
        entrypoint: &str,
        contract: Address,
    ) -> Arc<BlockAggregatedChanges> {
        let linked = EntryPoint::new(entrypoint.to_string(), contract.clone(), "f()".to_string());
        let traced =
            TracingResult::new(HashSet::new(), HashMap::from([(contract, HashSet::new())]));
        Arc::new(BlockAggregatedChanges {
            dci_update: DCIUpdate {
                new_entrypoints: HashMap::from([(component.to_string(), HashSet::from([linked]))]),
                new_entrypoint_params: HashMap::new(),
                trace_results: HashMap::from([(entrypoint.to_string(), traced)]),
            },
            ..(*full_block(number, branch, parent_branch)).clone()
        })
    }

    fn messages(blocks: &[Arc<BlockAggregatedChanges>]) -> Vec<HistoryEvent> {
        blocks
            .iter()
            .cloned()
            .map(HistoryEvent::Message)
            .collect()
    }

    fn restarted(last_committed: &Arc<BlockAggregatedChanges>, kind: RestartKind) -> HistoryEvent {
        HistoryEvent::ExtractorRestarted { last_committed: last_committed.block.clone(), kind }
    }

    /// A hand-built history for [`catch_up_spec`], which reads only the events and the config.
    fn history(
        events: Vec<HistoryEvent>,
        canonical: Vec<Arc<BlockAggregatedChanges>>,
        window_depth: u64,
    ) -> History {
        History {
            canonical_states: vec![DeltaStateModel::default(); events.len()],
            connected_states: vec![DeltaStateModel::default(); events.len()],
            events,
            canonical,
            config: HistoryConfig { window_depth, ..Default::default() },
        }
    }

    /// The state a snapshot at `at` gives: the canonical chain folded up to `at`, which is
    /// committed and so the same at any later point of the history.
    fn snapshot(history: &History, at: &Block) -> DeltaStateModel {
        let upto: Vec<&BlockAggregatedChanges> = history
            .canonical
            .iter()
            .map(Arc::as_ref)
            .take_while(|message| message.block.number <= at.number)
            .collect();
        assert_eq!(
            upto.last()
                .map(|message| &message.block),
            Some(at),
            "{at:?} is not canonical"
        );
        DeltaStateModel::fold(upto)
    }

    /// `states[i]` is what a client that subscribed at the start holds right after `events[i]`.
    /// The client applies every message, and after every restart resyncs from a snapshot at the
    /// last committed block before applying the replay.
    fn follower_states(history: &History) -> Vec<DeltaStateModel> {
        let mut client = DeltaStateModel::default();
        let mut resync_at = None;
        let mut states = Vec::with_capacity(history.events.len());
        for event in &history.events {
            match event {
                HistoryEvent::Message(message) => {
                    if let Some(at) = resync_at.take() {
                        client = snapshot(history, &at);
                    }
                    client.apply(message);
                }
                HistoryEvent::ExtractorRestarted { last_committed, kind: _ } => {
                    resync_at = Some(last_committed.clone());
                }
            }
            states.push(client.clone());
        }
        states
    }

    fn assert_canonical(
        history: &History,
        client: &DeltaStateModel,
        index: usize,
        context: impl Display,
    ) {
        assert_eq!(
            client
                .check_against(&history.canonical_states[index], &history.connected_states[index]),
            Ok(()),
            "{context}, after event {index}"
        );
    }

    /// How often the shapes that make catch-up hard occur in a set of histories.
    #[derive(Debug, Default)]
    struct Shapes {
        reverts: usize,
        /// Reverts that orphan the target of an earlier revert the window still remembers.
        nested_reverts: usize,
        /// Reverts that delete a component the reverted range created.
        orphaned_creations: usize,
        unchanged_restarts: usize,
        upgraded_restarts: usize,
        /// Restarts whose replay left the blocks sent before the restart.
        downtime_reorgs: usize,
        evicted_blocks: usize,
    }

    impl Shapes {
        fn add(&mut self, history: &History) {
            let mut window = Window::new(history.config.window_depth);
            // The window's blocks when the extractor last restarted, until the replay leaves
            // them or a revert makes the comparison moot.
            let mut abandoned: Vec<Arc<BlockAggregatedChanges>> = Vec::new();
            for event in &history.events {
                let held = window.blocks.len();
                match event {
                    HistoryEvent::Message(message) if message.revert => {
                        self.reverts += 1;
                        if !message
                            .deleted_protocol_components
                            .is_empty()
                        {
                            self.orphaned_creations += 1;
                        }
                        let target = message.block.number;
                        let nested = window.journal.iter().any(|entry| {
                            entry.revert.block.number > target &&
                                window
                                    .position(&entry.revert.block.hash)
                                    .is_some()
                        });
                        self.nested_reverts += usize::from(nested);
                        abandoned.clear();
                    }
                    HistoryEvent::Message(message) => {
                        let replaced = abandoned.iter().any(|old| {
                            old.block.number == message.block.number &&
                                old.block.hash != message.block.hash
                        });
                        if replaced {
                            self.downtime_reorgs += 1;
                            abandoned.clear();
                        }
                    }
                    HistoryEvent::ExtractorRestarted { last_committed: _, kind } => {
                        match kind {
                            RestartKind::Unchanged => self.unchanged_restarts += 1,
                            RestartKind::Upgraded => self.upgraded_restarts += 1,
                        }
                        abandoned = window.blocks.clone();
                    }
                }
                window.apply(event);
                if let HistoryEvent::Message(message) = event {
                    if !message.revert {
                        self.evicted_blocks += held + 1 - window.blocks.len();
                    }
                }
            }
        }
    }

    #[test]
    fn folding_the_ws_sequence_matches_the_canonical_chain() {
        let config = HistoryConfig { restart_probability: 0.0, ..Default::default() };
        for seed in 0..32 {
            let history = generate_history(&config, seed);

            let ws = DeltaStateModel::fold(history.messages());
            let canonical = DeltaStateModel::fold(
                history
                    .canonical
                    .iter()
                    .map(Arc::as_ref),
            );

            assert_eq!(ws.check_against(&canonical, &ws), Ok(()), "seed {seed}");
            assert_eq!(history.canonical_states.last(), Some(&canonical), "seed {seed}");
            assert_eq!(history.connected_states.last(), Some(&ws), "seed {seed}");
        }
    }

    #[test]
    fn a_client_following_the_ws_matches_the_canonical_chain_after_every_event() {
        let config = HistoryConfig::default();
        let mut shapes = Shapes::default();
        for seed in 0..SEEDS {
            let history = generate_history(&config, seed);
            shapes.add(&history);

            for (index, client) in follower_states(&history)
                .iter()
                .enumerate()
            {
                assert_canonical(&history, client, index, format!("seed {seed}"));
            }
            let canonical = DeltaStateModel::fold(
                history
                    .canonical
                    .iter()
                    .map(Arc::as_ref),
            );
            assert_eq!(history.canonical_states.last(), Some(&canonical), "seed {seed}");
            assert_eq!(
                history.connected_states.last(),
                Some(&DeltaStateModel::fold(history.messages())),
                "seed {seed}"
            );
        }

        let Shapes {
            reverts,
            nested_reverts,
            orphaned_creations,
            unchanged_restarts,
            upgraded_restarts,
            downtime_reorgs,
            evicted_blocks,
        } = shapes;
        for (shape, count) in [
            ("reverts", reverts),
            ("nested reverts", nested_reverts),
            ("components created on orphaned branches", orphaned_creations),
            ("unchanged restarts", unchanged_restarts),
            ("upgraded restarts", upgraded_restarts),
            ("reorgs during downtime", downtime_reorgs),
            ("evictions", evicted_blocks),
        ] {
            assert!(count > 0, "no {shape} in {SEEDS} histories");
        }
    }

    /// Gaps of every shape the spec tells apart: random ones up to three window depths long, and
    /// ones around every restart, from the client's last block and from the last committed one.
    fn gaps(history: &History, rng: &mut StdRng) -> Vec<Gap> {
        let mut messages = Vec::new();
        for (index, event) in history.events.iter().enumerate() {
            if let HistoryEvent::Message(_) = event {
                messages.push(index);
            }
        }
        let reach = 3 * history.config.window_depth as usize;
        let mut gaps = Vec::new();
        for _ in 0..RANDOM_GAPS {
            let start = rng.gen_range(0..messages.len() - 1);
            let end = (start + rng.gen_range(1..=reach)).min(messages.len() - 1);
            gaps.push(Gap { last_applied: messages[start], first_live: messages[end] });
        }
        for (restart, event) in history.events.iter().enumerate() {
            let HistoryEvent::ExtractorRestarted { last_committed, kind: _ } = event else {
                continue;
            };
            let before: Vec<usize> = messages
                .iter()
                .copied()
                .filter(|&index| index < restart)
                .collect();
            let after: Vec<usize> = messages
                .iter()
                .copied()
                .filter(|&index| index > restart)
                .take(reach)
                .collect();
            let at_last_committed = before
                .iter()
                .rev()
                .copied()
                .find(|&index| {
                    let HistoryEvent::Message(message) = &history.events[index] else {
                        return false;
                    };
                    &message.block == last_committed
                });
            let first_lives = after
                .iter()
                .take(3)
                .chain(after.last())
                .copied();
            for first_live in first_lives {
                for last_applied in before
                    .last()
                    .copied()
                    .into_iter()
                    .chain(at_last_committed)
                {
                    gaps.push(Gap { last_applied, first_live });
                }
            }
        }
        gaps
    }

    /// How often each outcome of [`catch_up_spec`] occurs over a set of gaps.
    #[derive(Debug, Default)]
    struct Outcomes {
        possible: usize,
        through_a_revert: usize,
        through_nested_reverts: usize,
        across_a_restart: usize,
        /// Catch-ups after a restart from the last committed block, the parent of the window's
        /// oldest block.
        from_the_last_committed_block: usize,
        fallbacks: HashMap<FallBackReason, usize>,
    }

    impl Outcomes {
        fn add_possible(
            &mut self,
            history: &History,
            gap: Gap,
            path: &[Arc<BlockAggregatedChanges>],
        ) {
            let reverts = path
                .iter()
                .filter(|message| message.revert)
                .count();
            self.possible += 1;
            self.through_a_revert += usize::from(reverts >= 1);
            self.through_nested_reverts += usize::from(reverts >= 2);
            let HistoryEvent::Message(last) = &history.events[gap.last_applied] else {
                panic!("the spec accepted a gap that does not start at a message");
            };
            for event in &history.events[gap.last_applied + 1..gap.first_live] {
                if let HistoryEvent::ExtractorRestarted { last_committed, kind: _ } = event {
                    self.across_a_restart += 1;
                    if last_committed == &last.block {
                        self.from_the_last_committed_block += 1;
                    }
                }
            }
        }
    }

    #[test]
    fn a_possible_catch_up_ends_in_the_canonical_state() {
        let config = HistoryConfig::default();
        let mut rng = StdRng::seed_from_u64(0);
        let mut outcomes = Outcomes::default();
        for seed in 0..SEEDS {
            let history = generate_history(&config, seed);
            let follower = follower_states(&history);
            for gap in gaps(&history, &mut rng) {
                let path = match catch_up_spec(&history, gap) {
                    CatchUpSpec::Possible(path) => path,
                    CatchUpSpec::FallBack(reason) => {
                        *outcomes
                            .fallbacks
                            .entry(reason)
                            .or_default() += 1;
                        continue;
                    }
                };
                outcomes.add_possible(&history, gap, &path);
                let context = format!("seed {seed}, {gap:?}");

                // The client's state before the gap, the path, then the live messages until the
                // next restart ends the new subscription.
                let mut client = follower[gap.last_applied].clone();
                for message in &path {
                    client.apply(message);
                }
                let mut last_live = gap.first_live;
                for (index, event) in history
                    .events
                    .iter()
                    .enumerate()
                    .skip(gap.first_live)
                {
                    let HistoryEvent::Message(message) = event else {
                        break;
                    };
                    client.apply(message);
                    if index == gap.first_live {
                        assert_canonical(&history, &client, index, &context);
                    }
                    last_live = index;
                }
                assert_canonical(&history, &client, last_live, &context);
            }
        }

        assert!(outcomes.possible > 0, "{outcomes:?}");
        assert!(outcomes.through_a_revert > 0, "{outcomes:?}");
        assert!(outcomes.through_nested_reverts > 0, "{outcomes:?}");
        assert!(outcomes.across_a_restart > 0, "{outcomes:?}");
        assert!(outcomes.from_the_last_committed_block > 0, "{outcomes:?}");
        // `ToNotInWindow` needs a window behind the WebSocket, which a history cannot express.
        for reason in [
            FallBackReason::FingerprintChanged,
            FallBackReason::EmptyWindow,
            FallBackReason::BelowFloor,
            FallBackReason::UnknownHash,
            FallBackReason::ToNotAhead,
        ] {
            assert!(outcomes.fallbacks.contains_key(&reason), "no {reason:?} in {outcomes:?}");
        }
    }

    #[test]
    fn resuming_after_a_restart_is_right_only_when_unchanged() {
        let config = HistoryConfig::default();
        let mut resumed = HashMap::new();
        for seed in 0..SEEDS {
            let history = generate_history(&config, seed);
            let follower = follower_states(&history);
            for (restart, event) in history.events.iter().enumerate() {
                let HistoryEvent::ExtractorRestarted { last_committed, kind } = event else {
                    continue;
                };
                let Some(last_applied) = (0..restart)
                    .rev()
                    .find(|&index| matches!(history.events[index], HistoryEvent::Message(_)))
                else {
                    continue;
                };
                let client = &follower[last_applied];
                let head = client
                    .head
                    .clone()
                    .expect("the client applied a message");
                if head.number == last_committed.number {
                    continue;
                }
                // Today's client skips replayed messages up to its block and resumes with the
                // next one when it follows its block. Only replays of new blocks are compared.
                let mut next = None;
                for (index, event) in history
                    .events
                    .iter()
                    .enumerate()
                    .skip(restart + 1)
                {
                    let HistoryEvent::Message(message) = event else {
                        break;
                    };
                    if message.revert {
                        break;
                    }
                    if message.block.number == head.number + 1 {
                        next = Some((index, message));
                        break;
                    }
                }
                let Some((index, next)) = next else {
                    continue;
                };
                let mut client = client.clone();
                client.apply(next);
                let result = client.check_against(
                    &history.canonical_states[index],
                    &history.connected_states[index],
                );

                match kind {
                    RestartKind::Unchanged if next.block.parent_hash == head.hash => {
                        assert_eq!(result, Ok(()), "seed {seed}, restart at {restart}");
                    }
                    // The chain reorganised during the downtime; the client would not resume.
                    RestartKind::Unchanged => continue,
                    RestartKind::Upgraded => {
                        assert!(result.is_err(), "seed {seed}, restart at {restart}");
                    }
                }
                *resumed.entry(*kind).or_insert(0) += 1;
            }
        }

        assert!(resumed.contains_key(&RestartKind::Unchanged), "{resumed:?}");
        assert!(resumed.contains_key(&RestartKind::Upgraded), "{resumed:?}");
    }

    #[test]
    fn a_message_through_the_wire_loses_only_what_the_wire_lacks() {
        for seed in 0..8 {
            let history = generate_history(&HistoryConfig::default(), seed);
            let sent = history.messages();

            let received: Vec<BlockAggregatedChanges> = sent
                .iter()
                .map(|message| through_the_wire(message))
                .collect();

            for (sent, received) in sent.iter().zip(&received) {
                let mut expected = (*sent).clone();
                expected.db_committed_block_height = None;
                for delta in expected.state_deltas.values_mut() {
                    delta.created_attributes.clear();
                }
                assert_eq!(received, &expected, "seed {seed}, block {}", sent.block.number);
            }
            assert_eq!(
                DeltaStateModel::fold(&received),
                DeltaStateModel::fold(sent),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn generation_is_deterministic_for_a_seed() {
        let config = HistoryConfig::default();

        assert_eq!(generate_history(&config, 7), generate_history(&config, 7));
    }

    #[test]
    fn without_nested_reverts_no_revert_orphans_an_earlier_target() {
        let config = HistoryConfig { nested_reverts: false, ..Default::default() };
        let mut shapes = Shapes::default();
        for seed in 0..32 {
            shapes.add(&generate_history(&config, seed));
        }

        assert!(shapes.reverts > 0, "{shapes:?}");
        assert_eq!(shapes.nested_reverts, 0, "{shapes:?}");
    }

    /// The chain of `messages` as the generator keeps it, with the state after every block.
    fn chain_of(messages: &[Arc<BlockAggregatedChanges>]) -> Vec<ChainBlock> {
        let mut state = DeltaStateModel::default();
        let mut chain = Vec::new();
        for message in messages {
            state.apply(message);
            chain.push(ChainBlock {
                message: Arc::clone(message),
                content: Arc::clone(message),
                state: state.clone(),
            });
        }
        chain
    }

    fn component(id: &str, change: ChangeType) -> ProtocolComponent {
        ProtocolComponent { id: id.to_string(), change, ..Default::default() }
    }

    fn component_balance(id: &str, token: &Address, balance: u8) -> ComponentBalance {
        ComponentBalance::new(token.clone(), Bytes::from(vec![balance]), 0.0, Bytes::new(), id)
    }

    fn account_balance(account: &Address, token: &Address, balance: u8) -> AccountBalance {
        AccountBalance::new(
            account.clone(),
            token.clone(),
            Bytes::from(vec![balance]),
            Bytes::new(),
        )
    }

    fn one(byte: u8) -> Bytes {
        Bytes::from(vec![byte])
    }

    #[test]
    fn a_revert_restores_what_the_indexer_restores() {
        let (vm, holder, token) = (address(1), address(2), address(9));
        let first = Arc::new(BlockAggregatedChanges {
            new_protocol_components: HashMap::from([
                ("kept".to_string(), component("kept", ChangeType::Creation)),
                ("deleted".to_string(), component("deleted", ChangeType::Creation)),
            ]),
            state_deltas: HashMap::from([(
                "kept".to_string(),
                ProtocolComponentStateDelta::new(
                    "kept",
                    HashMap::from([("a".to_string(), one(1))]),
                    HashSet::new(),
                ),
            )]),
            component_balances: HashMap::from([(
                "kept".to_string(),
                HashMap::from([(token.clone(), component_balance("kept", &token, 5))]),
            )]),
            account_deltas: HashMap::from([(
                vm.clone(),
                AccountDelta::new(
                    CHAIN,
                    vm.clone(),
                    HashMap::from([(one(1), Some(one(1)))]),
                    Some(one(1)),
                    Some(one(0xc0)),
                    ChangeType::Creation,
                ),
            )]),
            account_balances: HashMap::from([
                (vm.clone(), HashMap::from([(token.clone(), account_balance(&vm, &token, 1))])),
                (
                    holder.clone(),
                    HashMap::from([(token.clone(), account_balance(&holder, &token, 1))]),
                ),
            ]),
            ..(*full_block(1, 0, 0)).clone()
        });
        let mut created_b = ProtocolComponentStateDelta::new(
            "kept",
            HashMap::from([("a".to_string(), one(2)), ("b".to_string(), one(3))]),
            HashSet::new(),
        );
        created_b
            .created_attributes
            .insert("b".to_string());
        let second = Arc::new(BlockAggregatedChanges {
            new_protocol_components: HashMap::from([
                ("new".to_string(), component("new", ChangeType::Creation)),
                ("deleted".to_string(), component("deleted", ChangeType::Deletion)),
            ]),
            state_deltas: HashMap::from([("kept".to_string(), created_b)]),
            component_balances: HashMap::from([(
                "kept".to_string(),
                HashMap::from([(token.clone(), component_balance("kept", &token, 6))]),
            )]),
            account_deltas: HashMap::from([(
                vm.clone(),
                AccountDelta::new(
                    CHAIN,
                    vm.clone(),
                    HashMap::from([(one(1), Some(one(2))), (one(2), Some(one(7)))]),
                    Some(one(2)),
                    None,
                    ChangeType::Update,
                ),
            )]),
            account_balances: HashMap::from([
                (vm.clone(), HashMap::from([(token.clone(), account_balance(&vm, &token, 2))])),
                (
                    holder.clone(),
                    HashMap::from([(token.clone(), account_balance(&holder, &token, 2))]),
                ),
            ]),
            component_tvl: HashMap::from([("kept".to_string(), 1.0)]),
            ..(*full_block(2, 0, 0)).clone()
        });
        let chain = chain_of(&[first, second]);

        let revert = revert_message(&chain[0], &chain[1..]);

        assert!(revert.revert);
        assert_eq!(revert.block, chain[0].message.block);
        assert_eq!(revert.db_committed_block_height, None);
        assert_eq!(
            revert.new_protocol_components,
            HashMap::from([("deleted".to_string(), component("deleted", ChangeType::Creation))])
        );
        assert_eq!(
            revert.deleted_protocol_components,
            HashMap::from([("new".to_string(), component("new", ChangeType::Deletion))])
        );
        let attributes = &revert.state_deltas["kept"];
        assert_eq!(attributes.updated_attributes, HashMap::from([("a".to_string(), one(1))]));
        assert_eq!(attributes.deleted_attributes, HashSet::from(["b".to_string()]));
        assert_eq!(revert.component_balances["kept"][&token].balance, one(5));
        // The slot the range created comes back empty; native balance and code are not restored.
        assert_eq!(
            revert.account_deltas[&vm],
            AccountDelta::new(
                CHAIN,
                vm.clone(),
                HashMap::from([(one(1), Some(one(1))), (one(2), Some(Bytes::new()))]),
                None,
                None,
                ChangeType::Update,
            )
        );
        // `holder` changed no slot in the range, so its token balance stays.
        assert_eq!(
            revert.account_balances,
            HashMap::from([(
                vm.clone(),
                HashMap::from([(token.clone(), account_balance(&vm, &token, 1))])
            )])
        );
        assert!(revert.new_tokens.is_empty());
        assert!(revert.component_tvl.is_empty());
        assert_eq!(revert.dci_update, DCIUpdate::default());
    }

    #[test]
    fn dci_from_an_orphaned_branch_survives_the_revert() {
        // Block 3 links "ep" to "c" and is orphaned by a revert to 2; 3' carries no DCI.
        let canonical = [full_block(1, 0, 0), full_block(2, 0, 0), full_block(3, 1, 0)];
        let orphaned = block_with_dci(3, 0, 0, "c", "ep", address(1));
        let ws = [
            canonical[0].clone(),
            canonical[1].clone(),
            orphaned,
            revert_to(2, 0, 0),
            canonical[2].clone(),
        ];

        let connected = DeltaStateModel::fold(ws.iter().map(Arc::as_ref));
        let canonical = DeltaStateModel::fold(canonical.iter().map(Arc::as_ref));

        assert_eq!(
            connected.component_entrypoints,
            HashMap::from([("c".to_string(), HashSet::from(["ep".to_string()]))])
        );
        assert!(canonical
            .component_entrypoints
            .is_empty());
        assert_eq!(connected.check_against(&canonical, &connected), Ok(()));
        // A client that caught up from 2 never saw block 3 and lacks its DCI data.
        assert_eq!(canonical.check_against(&canonical, &connected), Ok(()));
    }

    #[test]
    fn an_empty_value_counts_as_absent() {
        // Orphaned block 2 writes a slot that did not exist at 1; the revert sends it back empty.
        let slot_delta = |value: Bytes| {
            HashMap::from([(
                address(1),
                AccountDelta::new(
                    CHAIN,
                    address(1),
                    HashMap::from([(Bytes::from(vec![1]), Some(value))]),
                    None,
                    None,
                    ChangeType::Update,
                ),
            )])
        };
        let written = Arc::new(BlockAggregatedChanges {
            account_deltas: slot_delta(Bytes::from(vec![7])),
            ..(*full_block(2, 0, 0)).clone()
        });
        let restored = Arc::new(BlockAggregatedChanges {
            account_deltas: slot_delta(Bytes::new()),
            ..(*revert_to(1, 0, 0)).clone()
        });
        let ws = [full_block(1, 0, 0), written, restored];

        let connected = DeltaStateModel::fold(ws.iter().map(Arc::as_ref));
        let canonical = DeltaStateModel::fold([ws[0].as_ref()]);

        assert_eq!(connected.check_against(&canonical, &connected), Ok(()));
    }

    #[test]
    fn deleting_a_component_drops_its_state() {
        let component = ProtocolComponent {
            id: "c".to_string(),
            change: ChangeType::Creation,
            ..Default::default()
        };
        let created = Arc::new(BlockAggregatedChanges {
            new_protocol_components: HashMap::from([("c".to_string(), component.clone())]),
            state_deltas: HashMap::from([(
                "c".to_string(),
                ProtocolComponentStateDelta::new(
                    "c",
                    HashMap::from([("fee".to_string(), Bytes::from(vec![1]))]),
                    HashSet::new(),
                ),
            )]),
            component_balances: HashMap::from([(
                "c".to_string(),
                HashMap::from([(address(9), component_balance("c", &address(9), 1))]),
            )]),
            ..(*full_block(1, 0, 0)).clone()
        });
        // Normal blocks report a deletion in `new_protocol_components`.
        let deleted = Arc::new(BlockAggregatedChanges {
            new_protocol_components: HashMap::from([(
                "c".to_string(),
                ProtocolComponent { change: ChangeType::Deletion, ..component },
            )]),
            ..(*full_block(2, 0, 0)).clone()
        });

        let state = DeltaStateModel::fold([created.as_ref(), deleted.as_ref()]);

        assert!(state.components.is_empty());
        assert!(state.component_attributes.is_empty());
        assert!(state.component_balances.is_empty());
    }

    #[test]
    fn an_account_creation_replaces_the_account() {
        let account = |slots: HashMap<Bytes, Option<Bytes>>, change: ChangeType| {
            HashMap::from([(
                address(1),
                AccountDelta::new(CHAIN, address(1), slots, Some(one(1)), Some(one(2)), change),
            )])
        };
        let updated = Arc::new(BlockAggregatedChanges {
            account_deltas: account(HashMap::from([(one(1), Some(one(1)))]), ChangeType::Update),
            ..(*full_block(1, 0, 0)).clone()
        });
        let created = Arc::new(BlockAggregatedChanges {
            account_deltas: account(HashMap::from([(one(2), Some(one(2)))]), ChangeType::Creation),
            ..(*full_block(2, 0, 0)).clone()
        });

        let state = DeltaStateModel::fold([updated.as_ref(), created.as_ref()]);

        assert_eq!(
            state.account_storage,
            HashMap::from([(address(1), HashMap::from([(one(2), one(2))]))])
        );
        assert_eq!(state.account_code, HashMap::from([(address(1), one(2))]));
    }

    #[test]
    #[should_panic(expected = "full blocks only")]
    fn applying_a_partial_block_panics() {
        let partial = BlockAggregatedChanges {
            partial_block_index: Some(0),
            ..(*full_block(1, 0, 0)).clone()
        };

        DeltaStateModel::default().apply(&partial);
    }

    #[test]
    #[should_panic(expected = "another extractor")]
    fn applying_a_block_of_another_extractor_panics() {
        let other = BlockAggregatedChanges {
            extractor: "other".to_string(),
            ..(*full_block(2, 0, 0)).clone()
        };

        DeltaStateModel::fold([full_block(1, 0, 0).as_ref(), &other]);
    }

    #[test]
    fn a_client_missing_canonical_dci_is_rejected() {
        let blocks = [full_block(1, 0, 0), block_with_dci(2, 0, 0, "c", "ep", address(1))];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let mut client = canonical.clone();
        client.component_entrypoints.clear();

        let result = client.check_against(&canonical, &canonical);

        assert!(
            matches!(result, Err(StateMismatch::Missing { field: "component_entrypoints", .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_client_with_dci_no_message_wrote_is_rejected() {
        let blocks = [full_block(1, 0, 0), block_with_dci(2, 0, 0, "c", "ep", address(1))];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let mut client = canonical.clone();
        client
            .entrypoint_contracts
            .entry("ep".to_string())
            .or_default()
            .insert(address(2));

        let result = client.check_against(&canonical, &canonical);

        assert!(
            matches!(result, Err(StateMismatch::Unexpected { field: "entrypoint_contracts", .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_client_with_a_different_restored_value_is_rejected() {
        let blocks = [full_block(1, 0, 0), full_block(2, 0, 0)];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let mut client = canonical.clone();
        client
            .account_storage
            .insert(address(1), HashMap::from([(Bytes::from(vec![1]), Bytes::from(vec![1]))]));

        let result = client.check_against(&canonical, &canonical);

        assert_eq!(result, Err(StateMismatch::Differs { field: "account_storage" }));
    }

    #[test]
    fn a_client_at_another_block_is_rejected() {
        let blocks = [full_block(1, 0, 0), full_block(2, 0, 0)];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let client = DeltaStateModel::fold([blocks[0].as_ref()]);

        assert_eq!(
            client.check_against(&canonical, &canonical),
            Err(StateMismatch::Differs { field: "head" })
        );
    }

    #[test]
    fn a_native_balance_is_compared_by_account_only() {
        // Reverts don't restore native balances, so a value written on an orphaned branch stays.
        let blocks = [full_block(1, 0, 0), full_block(2, 0, 0)];
        let mut canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        canonical
            .account_native_balances
            .insert(address(1), Bytes::from(vec![1]));
        let mut client = canonical.clone();
        client
            .account_native_balances
            .insert(address(1), Bytes::from(vec![2]));

        assert_eq!(client.check_against(&canonical, &client), Ok(()));
    }

    #[test]
    fn spec_from_a_canonical_block_is_the_canonical_blocks_after_it() {
        let blocks: Vec<_> = (1..=5)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let history = history(messages(&blocks), blocks.clone(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::Possible(blocks[2..4].to_vec()));
    }

    #[test]
    fn spec_from_an_orphaned_block_goes_through_the_revert() {
        // The client has 4, the chain reverts to 2 and grows 3', 4', 5', 6' on branch 1.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let revert = revert_to(2, 0, 0);
        let new_branch = vec![
            full_block(3, 1, 0),
            full_block(4, 1, 1),
            full_block(5, 1, 1),
            full_block(6, 1, 1),
        ];
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(revert.clone()));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 8 });

        let expected =
            vec![revert, new_branch[0].clone(), new_branch[1].clone(), new_branch[2].clone()];
        assert_eq!(spec, CatchUpSpec::Possible(expected));
    }

    #[test]
    fn spec_follows_nested_reverts_to_the_canonical_chain() {
        // The client has 4. The chain reverts to 2 and grows 3', 4' on branch 1, then reverts
        // to 1, orphaning 2 as well, and grows 2'', 3'', 4'' on branch 2.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let (first_revert, second_revert) = (revert_to(2, 0, 0), revert_to(1, 0, 0));
        let new_branch = vec![full_block(2, 2, 0), full_block(3, 2, 2), full_block(4, 2, 2)];
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(first_revert.clone()));
        events.extend(messages(&[full_block(3, 1, 0), full_block(4, 1, 1)]));
        events.push(HistoryEvent::Message(second_revert.clone()));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..1], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 10 });

        let expected =
            vec![first_revert, second_revert, new_branch[0].clone(), new_branch[1].clone()];
        assert_eq!(spec, CatchUpSpec::Possible(expected));
    }

    #[test]
    fn spec_to_the_client_block_is_an_empty_path() {
        // The client has 2 and misses 3 and 4; its new subscription starts with a revert to 2.
        let blocks: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks);
        events.push(HistoryEvent::Message(revert_to(2, 0, 0)));
        let history = history(events, blocks[..2].to_vec(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::Possible(Vec::new()));
    }

    #[test]
    fn spec_to_behind_the_client_block_falls_back() {
        // The client has 4; its new subscription starts with a revert to 2 the window has not
        // applied yet.
        let blocks: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks);
        events.push(HistoryEvent::Message(revert_to(2, 0, 0)));
        let history = history(events, blocks[..2].to_vec(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::ToNotAhead));
    }

    #[test]
    fn spec_to_a_block_the_window_lacks_falls_back() {
        // The client's first live block is 7, whose parent the window has not received yet.
        let mut blocks: Vec<_> = (1..=5)
            .map(|n| full_block(n, 0, 0))
            .collect();
        blocks.push(full_block(7, 0, 0));
        let history = history(messages(&blocks), blocks.clone(), 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 5 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::ToNotInWindow));
    }

    #[test]
    fn spec_right_after_a_restart_falls_back_on_the_empty_window() {
        let blocks: Vec<_> = (1..=3)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks);
        events.push(restarted(&blocks[1], RestartKind::Unchanged));
        events.push(HistoryEvent::Message(blocks[2].clone()));
        let history = history(events, blocks, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 4 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::EmptyWindow));
    }

    #[test]
    fn spec_an_upgraded_restart_in_the_gap_falls_back() {
        let blocks: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks[..3]);
        events.push(restarted(&blocks[1], RestartKind::Upgraded));
        events.extend(messages(&blocks[2..]));
        let history = history(events, blocks, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 2, first_live: 5 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::FingerprintChanged));
    }

    #[test]
    fn spec_a_client_at_the_last_committed_block_resumes_after_a_restart() {
        let blocks: Vec<_> = (1..=5)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let mut events = messages(&blocks[..3]);
        events.push(restarted(&blocks[1], RestartKind::Unchanged));
        events.extend(messages(&blocks[2..]));
        let history = history(events, blocks.clone(), 16);

        // The client applied block 2, the extractor's last committed block.
        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 6 });

        assert_eq!(spec, CatchUpSpec::Possible(blocks[2..4].to_vec()));
    }

    #[test]
    fn spec_a_reorg_during_downtime_falls_back() {
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let new_branch = vec![full_block(3, 1, 0), full_block(4, 1, 1), full_block(5, 1, 1)];
        let mut events = messages(&old_branch);
        events.push(restarted(&old_branch[1], RestartKind::Unchanged));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        // The client applied 4 on the abandoned branch; no revert message ever orphaned it.
        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 7 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::UnknownHash));
    }

    #[test]
    fn spec_forgets_the_reverts_on_a_restart() {
        // Had the journal survived the restart, its revert to 2 would lead from 4 to the parent
        // of the refilled window's oldest block.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let new_branch = vec![full_block(3, 1, 0), full_block(4, 1, 1), full_block(5, 1, 1)];
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(revert_to(2, 0, 0)));
        events.push(HistoryEvent::Message(new_branch[0].clone()));
        events.push(restarted(&old_branch[1], RestartKind::Unchanged));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 16);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 9 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::UnknownHash));
    }

    #[rstest]
    #[case::target_still_held(1, None)]
    #[case::target_evicted(2, Some(FallBackReason::UnknownHash))]
    fn spec_forgets_a_revert_once_its_target_leaves_the_window(
        #[case] committed: u64,
        #[case] expected: Option<FallBackReason>,
    ) {
        // The client has 4. The chain reverts to 2 and grows 3'..=7' on branch 1, whose
        // watermarks evict every block up to `committed`.
        let old_branch: Vec<_> = (1..=4)
            .map(|n| full_block(n, 0, 0))
            .collect();
        let revert = revert_to(2, 0, 0);
        let new_branch: Vec<_> = (3..=7)
            .map(|n| {
                Arc::new(BlockAggregatedChanges {
                    finalized_block_height: committed,
                    db_committed_block_height: Some(committed),
                    ..(*full_block(n, 1, if n == 3 { 0 } else { 1 })).clone()
                })
            })
            .collect();
        let mut events = messages(&old_branch);
        events.push(HistoryEvent::Message(revert.clone()));
        events.extend(messages(&new_branch));
        let canonical = [&old_branch[..2], &new_branch[..]].concat();
        let history = history(events, canonical, 2);

        let spec = catch_up_spec(&history, Gap { last_applied: 3, first_live: 9 });

        let expected = match expected {
            Some(reason) => CatchUpSpec::FallBack(reason),
            None => CatchUpSpec::Possible([&[revert][..], &new_branch[..4]].concat()),
        };
        assert_eq!(spec, expected);
    }

    #[test]
    fn spec_an_evicted_from_falls_back_below_floor() {
        let blocks: Vec<_> = (1..=10).map(committed_block).collect();
        let history = history(messages(&blocks), blocks, 2);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 9 });

        assert_eq!(spec, CatchUpSpec::FallBack(FallBackReason::BelowFloor));
    }

    #[rstest]
    #[case::depth_binds(10, Some(10), 8)]
    #[case::finalized_binds(5, Some(10), 5)]
    #[case::committed_binds(10, Some(4), 4)]
    #[case::nothing_is_evicted_before_the_first_commit(10, None, 1)]
    fn spec_keeps_a_block_until_it_is_finalized_committed_and_deeper_than_the_window(
        #[case] finalized: u64,
        #[case] committed: Option<u64>,
        #[case] oldest_from: u64,
    ) {
        // Blocks 1..=10 carry the same watermarks; the window is 2 blocks deep. The client's
        // new subscription starts at block 11.
        let blocks: Vec<_> = (1..=11)
            .map(|n| block_with_watermarks(n, finalized, committed))
            .collect();
        let history = history(messages(&blocks), blocks.clone(), 2);
        let from = |number: u64| Gap { last_applied: number as usize - 1, first_live: 10 };

        assert_eq!(
            catch_up_spec(&history, from(oldest_from)),
            CatchUpSpec::Possible(blocks[oldest_from as usize..10].to_vec())
        );
        if oldest_from > 1 {
            assert_eq!(
                catch_up_spec(&history, from(oldest_from - 1)),
                CatchUpSpec::FallBack(FallBackReason::BelowFloor)
            );
        }
    }

    #[test]
    fn spec_a_revert_leaves_the_watermarks_alone() {
        // Blocks 1..=6 let the window evict block 1. A revert to 5 carrying high watermarks would
        // evict up to 3 if the window applied them.
        let blocks: Vec<_> = (1..=6)
            .map(|n| block_with_watermarks(n, 1, Some(1)))
            .collect();
        let revert = Arc::new(BlockAggregatedChanges {
            finalized_block_height: 100,
            db_committed_block_height: Some(100),
            ..(*revert_to(5, 0, 0)).clone()
        });
        let mut events = messages(&blocks);
        events.push(HistoryEvent::Message(revert));
        events.push(HistoryEvent::Message(full_block(6, 1, 0)));
        let history = history(events, blocks.clone(), 2);

        let spec = catch_up_spec(&history, Gap { last_applied: 1, first_live: 7 });

        assert_eq!(spec, CatchUpSpec::Possible(blocks[2..5].to_vec()));
    }
}
