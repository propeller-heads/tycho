//! Random extractor histories, with reverts built the way the indexer builds them.

use std::{
    collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
    sync::Arc,
};

use chrono::NaiveDateTime;
use rand::{rngs::StdRng, Rng, SeedableRng};

use super::model::DeltaStateModel;
use crate::{
    models::{
        blockchain::{
            Block, BlockAggregatedChanges, DCIUpdate, EntryPoint, RPCTracerParams, TracingParams,
            TracingResult,
        },
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        token::Token,
        Address, AttrStoreKey, Chain, ChangeType, ComponentId, EntryPointId,
    },
    Bytes,
};

/// Extractor of every message [`generate_history`] produces.
pub const EXTRACTOR: &str = "delta_history";

/// Chain of every message [`generate_history`] produces.
pub const CHAIN: Chain = Chain::Ethereum;

/// Knobs of [`generate_history`].
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryConfig {
    /// Number of events to generate: messages and restarts.
    pub length: usize,
    /// Retention depth `W` of the server's delta window (`--delta-window-depth`), in blocks.
    /// Decides which blocks a catch-up can still reach.
    pub window_depth: u64,
    /// Blocks between the tip and the finalized block. Reverts never reach below finality. Below
    /// `window_depth`: a revert may target the finalized block, which the window then still holds,
    /// since it evicts only blocks at least `window_depth` below the tip.
    pub finality_depth: u64,
    /// Most blocks the committed height trails the finalized one by. Commits come in batches, so
    /// the lag varies up to this bound. The committed height is `None` until the first commit
    /// after every extractor start, and on every revert message.
    pub max_commit_lag: u64,
    /// Chance that the next event is a revert instead of a new block.
    pub revert_probability: f64,
    /// Deepest revert, in blocks below the tip. Reverts never reach below the finalized block or
    /// the first block of the extractor's current run either.
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
            max_commit_lag: 10,
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
/// When `finality_depth` is not below `window_depth`, a probability lies outside `[0, 1]`, or
/// `restart_probability` and `revert_probability` add up to more than 1.
pub fn generate_history(config: &HistoryConfig, seed: u64) -> History {
    assert!(
        config.finality_depth < config.window_depth,
        "finality_depth ({}) must be below window_depth ({}), or the window may evict the target \
         of a revert to the finalized block",
        config.finality_depth,
        config.window_depth
    );
    for (name, probability) in [
        ("revert_probability", config.revert_probability),
        ("restart_probability", config.restart_probability),
        ("fingerprint_change_probability", config.fingerprint_change_probability),
        ("downtime_reorg_probability", config.downtime_reorg_probability),
    ] {
        assert!((0.0..=1.0).contains(&probability), "{name} ({probability}) must lie in [0, 1]");
    }
    assert!(
        config.restart_probability + config.revert_probability <= 1.0,
        "restart_probability and revert_probability are chances of one event and add up to more \
         than 1"
    );
    let mut generator = Generator::new(config, seed);
    while generator.events.len() < config.length {
        generator.step();
    }
    generator.finish()
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
/// Chance that a message reports a new commit when the lag does not force one.
const COMMIT: f64 = 0.3;

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

    /// The committed height a message for block `number` carries: `None` until the run's first
    /// commit, then the height of the last commit. A commit jumps to a finalized block of the run
    /// that is not above `number`. One is forced when the committed height would otherwise trail
    /// finality by more than `max_commit_lag`.
    fn draw_committed(&mut self, number: u64) -> Option<u64> {
        let highest = self.finalized.min(number);
        if highest < self.run_start {
            return self.committed;
        }
        let lag_floor = self
            .finalized
            .saturating_sub(self.config.max_commit_lag);
        let forced = match self.committed {
            Some(committed) => committed < lag_floor,
            None => lag_floor >= self.run_start,
        };
        if !forced && !self.rng.gen_bool(COMMIT) {
            return self.committed;
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
        change_component(rng, &mut message, state, &id, ComponentAge::New);
        Some(id)
    } else {
        None
    };
    for id in &live {
        if rng.gen_bool(CHANGE_COMPONENT) {
            change_component(rng, &mut message, state, id, ComponentAge::Existing(young));
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
enum ComponentAge<'a> {
    /// The block creates the component.
    New,
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
    age: ComponentAge,
) {
    let held = state.component_attributes.get(id);
    let mut delta = ProtocolComponentStateDelta::new(id, HashMap::new(), HashSet::new());
    for name in ATTRIBUTES {
        if !rng.gen_bool(WRITE_ATTRIBUTE) {
            continue;
        }
        let name = name.to_string();
        let (exists, deletable) = match age {
            ComponentAge::New => (false, false),
            ComponentAge::Existing(young) => {
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

    let tokens = match age {
        ComponentAge::New => message.new_protocol_components[id]
            .tokens
            .clone(),
        ComponentAge::Existing(_) => state.components[id].tokens.clone(),
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
    use super::*;
    use crate::testing::delta_history::fixtures::*;

    #[test]
    #[should_panic(expected = "finality_depth (8) must be below window_depth (8)")]
    fn a_window_no_deeper_than_finality_is_rejected() {
        let config = HistoryConfig { window_depth: 8, finality_depth: 8, ..Default::default() };

        generate_history(&config, 0);
    }

    #[test]
    fn generation_is_deterministic_for_a_seed() {
        let config = HistoryConfig::default();

        assert_eq!(generate_history(&config, 7), generate_history(&config, 7));
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
    #[test]
    fn a_revert_restores_what_the_indexer_restores() {
        let (vm, holder, token) = (byte_address(1), byte_address(2), byte_address(9));
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
}
