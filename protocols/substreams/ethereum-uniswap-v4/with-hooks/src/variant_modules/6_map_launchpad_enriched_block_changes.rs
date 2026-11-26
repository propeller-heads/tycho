//! The Elysium manifest's terminal module: the block changes the shared Uniswap V4 pipeline
//! aggregates, plus each launchpad pool's hook state decoded from its hook's storage writes: the
//! `montra_*` attributes and the pause state of a Montra pool, the `ascend_*` attributes of an
//! Ascend pool.
//!
//! Every write to a watched word of a pool counts, whatever the transaction, its caller or the
//! function that made it. This module does not use hook events as triggers, because Montra's stage
//! and LP-line hold and Ascend's observation change with no event.
//!
//! Ascend pools: the watched words are listed in [`crate::ascend`], and their writers are swaps
//! (the observation, and on a fill the bid), the launch and `fundBid` (the hook's 30 `SSTORE`
//! sites, mapped from its bytecode, cover exactly these writers). A pool is created with all eleven
//! `ascend_*` attributes from the launch transaction's writes, a word it did not write read as zero
//! (`Bid` words 0 and 1 start at zero). `fundBid` on a pool id the hook has not launched reverts
//! `UnknownPool()`, so no `Bid` write precedes a pool's creation transaction; the Readme's
//! "Deployment assumption for Ascend" paragraph has the evidence. No `paused` is emitted for Ascend
//! pools: the hook's `setPaused` stops launches only, and the Readme says so.
//!
//! Montra pools: the watched words are `PoolConfig` words 0 to 2 and the `PoolKey` words, and their
//! writers are swaps (stage, LP-line hold, anti-snipe), launches, `lowerTotalFee`, `lockFee` and
//! any path a `LaunchFactory` upgrade adds.
//!
//! Deployment assumption for Montra pools: a launch registers the pool, initialises it on the
//! `PoolManager` and places its seed in one transaction. Each of the 403 launch transactions
//! checked, those in blocks 4963600-4987299 (356) and 5990000-6001999 (47), emits
//! `PoolRegistered`, `Initialize` and `PoolSeeded` together. `Initialize` creates the component,
//! and this module creates its `montra_*` attributes from the `PoolConfig` words written in that
//! transaction, zero for a word it did not write. If a `LaunchFactory` upgrade ever registers a
//! Montra pool in an earlier transaction, this module never sees the words written then, and the
//! pool would be quoted with a zero fee. So this module pauses a Montra pool until a transaction
//! places its seed (`MontraWrites::places_seed`), and unpauses it in that transaction: `seed`
//! rewrites `PoolConfig` words 0, 1 and 2 in full, so it alone carries every value the attributes
//! need.
//!
//! This module also pauses a Montra pool when a `PoolKey` word changes after the pool's creation
//! transaction. `register` writes the key once and refuses a registered pool, so no known path
//! does this. Only a placed seed unpauses a pool, and `seed` sets the `seeded` flag once: a pool
//! whose key changes after its seed stays paused.
//!
//! This module emits no entrypoint, contract or storage payload: all hook state travels as
//! attributes. The package Readme ("What the Elysium manifest emits") states why.

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, Result};
use ethereum_uniswap_v4_shared::{
    abi::pool_manager::events::ModifyLiquidity,
    pb::uniswap::v4::{Events, LiquidityChanges, TickDeltas},
    utils::protocol_changes::collect_transaction_changes,
};
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetString},
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

use crate::{
    ascend::AscendWord,
    launchpad::{
        params::Params,
        slots::{slot_store_key, HookWord, SlotOwner},
        storage_writes::{storage_writes, SlotWrite},
        Layout,
    },
    montra::pool_config::{sets_seeded_flag, ConfigWord},
    storage::StateWord,
};

#[substreams::handlers::map]
// One parameter per module input the manifest wires in; the handler macro takes them separately.
#[allow(clippy::too_many_arguments)]
pub fn map_launchpad_enriched_block_changes(
    params: String,
    block: eth::Block,
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
    pool_liquidity_changes: LiquidityChanges,
    pool_liquidity_store_deltas: StoreDeltas,
    launchpad_slots: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse(&params)?;
    let launches = created_launches(&params, &created_pools)?;
    let hook_writes = collect_hook_writes(&block, &params, &launchpad_slots, &launches)?;

    let changes = collect_transaction_changes(
        created_pools,
        events,
        balances_map_deltas,
        balances_store_deltas,
        ticks_map_deltas,
        ticks_store_deltas,
        pool_liquidity_changes,
        pool_liquidity_store_deltas,
    );
    let changes = with_hook_state(changes, &launches, hook_writes)?;

    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

/// A pool `map_launchpad_pools_created` created in this block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    /// The creation transaction.
    tx: Transaction,
    /// The layout of the pool's hook.
    layout: Layout,
}

/// The creation of every pool `map_launchpad_pools_created` created in this block, by component
/// id. Fails when a change of `created_pools` has no transaction, and when a component's `hooks`
/// is missing or in neither hook list: the creation module reads the same parameters and creates
/// pools of listed hooks only.
fn created_launches(
    params: &Params,
    created_pools: &BlockEntityChanges,
) -> Result<HashMap<String, Launch>> {
    let mut launches = HashMap::new();
    for change in &created_pools.changes {
        let tx = change.tx.as_ref().ok_or_else(|| {
            anyhow!("map_launchpad_pools_created emitted a change without a transaction")
        })?;
        for component in &change.component_changes {
            let layout = component
                .get_attribute_value("hooks")
                .and_then(|hook| params.layout(&hook))
                .ok_or_else(|| {
                    anyhow!("created pool {} has no hook of a listed layout", component.id)
                })?;
            launches.insert(component.id.clone(), Launch { tx: tx.clone(), layout });
        }
    }
    Ok(launches)
}

/// One transaction's writes to the watched words of one layout's struct, by word.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Writes<W: StateWord>(HashMap<W, SlotWrite>);

impl<W: StateWord> Default for Writes<W> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<W: StateWord> Writes<W> {
    /// Every attribute of the layout as the writes left it, a word they did not write read as
    /// zero.
    fn final_attributes(&self, change: ChangeType) -> Vec<Attribute> {
        W::ALL
            .iter()
            .flat_map(|word| {
                let value = self
                    .0
                    .get(word)
                    .map_or([0u8; 32], |write| write.new_value);
                word.attributes(&value)
            })
            .map(|(name, value)| attribute(name, value, change))
            .collect()
    }

    /// The attributes whose decoded value the writes changed.
    fn changed_attributes(&self) -> Vec<Attribute> {
        W::ALL
            .iter()
            .filter_map(|word| {
                self.0
                    .get(word)
                    .map(|write| (word, write))
            })
            .flat_map(|(word, write)| {
                word.attributes(&write.old_value)
                    .into_iter()
                    .zip(word.attributes(&write.new_value))
                    .filter(|((_, old), (_, new))| old != new)
                    .map(|(_, (name, new))| attribute(name, new, ChangeType::Update))
            })
            .collect()
    }
}

/// One transaction's writes to the watched words of one Montra pool.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MontraWrites {
    config: Writes<ConfigWord>,
    /// Whether the transaction changed a `PoolKey` word.
    changes_key: bool,
    /// Whether the transaction placed this pool's seed: it made the seed's writes
    /// ([`MontraWrites::writes_seed_words`]) and the hook added liquidity to this pool on the
    /// `PoolManager` ([`hook_added_liquidity`]). `seed(poolId, startTick, supply)` takes no key:
    /// the hook calls `PoolManager.modifyLiquidity` with the `PoolKey` it stored (launch tx
    /// 0xe69565883dcf7ff890325eb1bf34bed5c281be88e89ffebdbdddd9837134a255, log 4), and the
    /// `PoolManager` names the pool by that key's hash. So the liquidity lands on this pool only
    /// while the stored key is the one the pool was initialised with.
    places_seed: bool,
}

impl MontraWrites {
    /// Whether the transaction made the writes of `seed`: word 0's `seeded` flag went from clear
    /// to set ([`sets_seeded_flag`]), and words 1 and 2, which `seed` rewrites in full, were
    /// written.
    fn writes_seed_words(&self) -> bool {
        let config = &self.config.0;
        match (
            config.get(&ConfigWord::Terms),
            config.get(&ConfigWord::SeedRange),
            config.get(&ConfigWord::EarlyBuyFee),
        ) {
            (Some(word_0), Some(_), Some(_)) => {
                sets_seeded_flag(&word_0.old_value, &word_0.new_value)
            }
            _ => false,
        }
    }
}

/// One transaction's writes to the watched words of one pool, in the layout of its hook. The store
/// keys a pool's slots in one layout, so a pool's writes are of that layout only.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PoolWrites {
    Montra(MontraWrites),
    Ascend(Writes<AscendWord>),
}

impl PoolWrites {
    fn new(layout: Layout) -> Self {
        match layout {
            Layout::Montra => Self::Montra(MontraWrites::default()),
            Layout::Ascend => Self::Ascend(Writes::default()),
        }
    }

    /// The layout a write to `word` belongs to.
    fn layout_of(word: HookWord) -> Layout {
        match word {
            HookWord::Config(_) | HookWord::Key(_) => Layout::Montra,
            HookWord::Ascend(_) => Layout::Ascend,
        }
    }

    /// Records `write` to `word`. Fails when `word` is of another layout than the pool's writes so
    /// far, which the store's one layout per pool rules out.
    fn insert(&mut self, word: HookWord, write: SlotWrite) -> Result<()> {
        match (self, word) {
            (Self::Montra(writes), HookWord::Config(word)) => {
                writes.config.0.insert(word, write);
            }
            (Self::Montra(writes), HookWord::Key(_)) => {
                writes.changes_key |= write.old_value != write.new_value;
            }
            (Self::Ascend(writes), HookWord::Ascend(word)) => {
                writes.0.insert(word, write);
            }
            (_, word) => {
                return Err(anyhow!("a write to {word:?} joins a pool of another hook layout"))
            }
        }
        Ok(())
    }

    /// The pool's state in its creation transaction: every attribute of its hook's layout,
    /// created. A Montra pool is created paused unless the transaction placed its seed; an Ascend
    /// pool is never paused.
    fn launch_state(&self) -> HookStateChange {
        match self {
            Self::Ascend(writes) => HookStateChange {
                attributes: writes.final_attributes(ChangeType::Creation),
                pause_change: None,
            },
            Self::Montra(writes) => HookStateChange {
                attributes: writes
                    .config
                    .final_attributes(ChangeType::Creation),
                pause_change: (!writes.places_seed).then_some(PauseChange::Pause),
            },
        }
    }

    /// The state change of a pool created in an earlier transaction: the attributes whose decoded
    /// value changed. An Ascend pool never pauses.
    ///
    /// A transaction that places the seed of a Montra pool created paused re-emits every attribute
    /// and unpauses it; it can do so once, since nothing clears the `seeded` flag. So `lockFee`,
    /// which writes a byte the simulation does not read, emits nothing. A changed `PoolKey` word
    /// pauses a Montra pool, also when the same transaction places the seed.
    fn update_state(&self) -> HookStateChange {
        let writes = match self {
            Self::Ascend(writes) => {
                return HookStateChange {
                    attributes: writes.changed_attributes(),
                    pause_change: None,
                }
            }
            Self::Montra(writes) => writes,
        };
        let (attributes, mut pause_change) = if writes.places_seed {
            (
                writes
                    .config
                    .final_attributes(ChangeType::Update),
                Some(PauseChange::Unpause),
            )
        } else {
            (writes.config.changed_attributes(), None)
        };
        if writes.changes_key {
            // On a pool already paused (unseeded, or by an earlier key change) this emits
            // `paused` [1] as a Creation a second time, and a revert of this block would then
            // delete the attribute. No known path changes a `PoolKey` (the module doc states why),
            // and telling an already-paused pool apart would take a store of pause states.
            pause_change = Some(PauseChange::Pause);
        }
        HookStateChange { attributes, pause_change }
    }
}

fn attribute(name: &str, value: Vec<u8>, change: ChangeType) -> Attribute {
    Attribute { name: name.to_string(), value, change: change.into() }
}

/// The transaction and its writes per pool, by transaction index.
type HookWrites = BTreeMap<u64, (Transaction, BTreeMap<String, PoolWrites>)>;

/// Every write `block` made to a watched word of a known pool, grouped by transaction and pool,
/// plus an empty entry for the creation transaction of every pool created in this block, so that
/// pool gets its attributes even when the hook wrote nothing.
///
/// This skips a write by a contract other than the listed hooks, and a write to a slot
/// `launchpad_slots` does not list for the hook that made it (another hook mapping: fee tabs,
/// ownership, or a slot of a pool of another listed hook). It also skips a write made in the
/// creation block before the pool's creation transaction: the component does not exist yet.
///
/// Fails when a transaction has a malformed storage change ([`storage_writes`]), when a
/// `launchpad_slots` value does not parse ([`SlotOwner::from_store_value`]) or names a word of
/// another layout than the pool's other writes ([`PoolWrites::insert`]), and when the
/// transaction that sets a pool's `seeded` flag has no receipt. Extended Firehose blocks carry a
/// receipt for every transaction, so its absence means the block source cannot show whether the
/// seed was placed.
fn collect_hook_writes(
    block: &eth::Block,
    params: &Params,
    launchpad_slots: &impl StoreGet<String>,
    launches: &HashMap<String, Launch>,
) -> Result<HookWrites> {
    let mut hook_writes = HookWrites::new();
    let hooks = params.all_hooks();
    for trace in block.transactions() {
        let writes = storage_writes(trace, &hooks)?;
        if writes.is_empty() {
            continue;
        }
        let tx_index: u64 = trace.index.into();
        let mut pools: BTreeMap<String, PoolWrites> = BTreeMap::new();
        for write in writes {
            let Some(value) =
                launchpad_slots.get_last(slot_store_key(&write.contract, &write.slot))
            else {
                continue;
            };
            let owner = SlotOwner::from_store_value(&value)?;
            if launches
                .get(&owner.component_id)
                .is_some_and(|launch| launch.tx.index > tx_index)
            {
                continue;
            }
            pools
                .entry(owner.component_id)
                .or_insert_with(|| PoolWrites::new(PoolWrites::layout_of(owner.word)))
                .insert(owner.word, write)?;
        }
        if pools.is_empty() {
            continue;
        }
        for (component_id, pool_writes) in &mut pools {
            if let PoolWrites::Montra(writes) = pool_writes {
                // The receipt is read only in the one transaction of a pool's life that sets the
                // `seeded` flag.
                writes.places_seed = writes.writes_seed_words() &&
                    hook_added_liquidity(trace, params, component_id)?;
            }
        }
        hook_writes.insert(tx_index, (Transaction::from(trace), pools));
    }
    for (component_id, launch) in launches {
        hook_writes
            .entry(launch.tx.index)
            .or_insert_with(|| (launch.tx.clone(), BTreeMap::new()))
            .1
            .entry(component_id.clone())
            .or_insert_with(|| PoolWrites::new(launch.layout));
    }
    Ok(hook_writes)
}

/// Whether `trace`'s receipt holds a `ModifyLiquidity` the `PoolManager` emitted for the pool
/// `component_id`, sent by a listed hook, with a positive liquidity delta. The function checks the
/// emitter first because any contract can emit a `ModifyLiquidity` with a real pool id. Fails when
/// `trace` has no receipt.
fn hook_added_liquidity(
    trace: &eth::TransactionTrace,
    params: &Params,
    component_id: &str,
) -> Result<bool> {
    let receipt = trace.receipt.as_ref().ok_or_else(|| {
        anyhow!(
            "transaction 0x{} sets the seeded flag of {component_id} but has no receipt",
            hex::encode(&trace.hash)
        )
    })?;
    Ok(receipt
        .logs
        .iter()
        .filter(|log| log.address == *params.pool_manager())
        .filter_map(ModifyLiquidity::match_and_decode)
        .any(|event| {
            let adds_liquidity = event.liquidity_delta > BigInt::zero();
            adds_liquidity &&
                params.is_montra_hook(&event.sender) &&
                event.id.to_hex() == component_id
        }))
}

/// Whether a transaction pauses or unpauses a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PauseChange {
    Pause,
    Unpause,
}

impl PauseChange {
    /// The `paused` attribute change, as tycho-substreams' `change_component_pause_state` (from
    /// release 0.8) writes it: `[1]` as a Creation to pause, a Deletion (value `[0]`) to unpause.
    /// The builder's `add_entity_change` merges it into the pool's other attributes the same way
    /// that helper's `set_attribute` does.
    fn attribute(self) -> Attribute {
        let (value, change) = match self {
            Self::Pause => (1u8, ChangeType::Creation),
            Self::Unpause => (0u8, ChangeType::Deletion),
        };
        Attribute { name: "paused".to_string(), value: vec![value], change: change.into() }
    }
}

/// What one transaction changes in one pool's hook state.
#[derive(Debug, PartialEq)]
struct HookStateChange {
    attributes: Vec<Attribute>,
    /// `None` leaves the pool's pause state as it is.
    pause_change: Option<PauseChange>,
}

/// `changes` with the hook state of `hook_writes` merged into the transaction it belongs to, in
/// transaction order. Only the transactions that carry hook state are rebuilt.
///
/// Fails when an entry of `changes` has no transaction, when two entries name the same
/// transaction index (the shared pipeline builds one per transaction, so the second would
/// otherwise replace the first), and when a transaction's shared changes hold a field
/// [`builder_with`] does not copy.
fn with_hook_state(
    changes: Vec<TransactionChanges>,
    launches: &HashMap<String, Launch>,
    hook_writes: HookWrites,
) -> Result<Vec<TransactionChanges>> {
    let mut by_index = BTreeMap::new();
    for change in changes {
        let index = change
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("the shared V4 pipeline emitted changes without a transaction"))?
            .index;
        if by_index.insert(index, change).is_some() {
            return Err(anyhow!(
                "the shared V4 pipeline emitted two changes for transaction index {index}"
            ));
        }
    }

    for (index, (tx, pools)) in hook_writes {
        let mut builder = match by_index.remove(&index) {
            Some(change) => builder_with(&tx, change)?,
            None => TransactionChangesBuilder::new(&tx),
        };
        for (component_id, pool_writes) in pools {
            let state = match launches.get(&component_id) {
                Some(launch) if launch.tx.index == index => pool_writes.launch_state(),
                _ => pool_writes.update_state(),
            };
            if !state.attributes.is_empty() {
                builder.add_entity_change(&EntityChanges {
                    component_id: component_id.clone(),
                    attributes: state.attributes,
                });
            }
            if let Some(pause_change) = state.pause_change {
                builder.add_entity_change(&EntityChanges {
                    component_id,
                    attributes: vec![pause_change.attribute()],
                });
            }
        }
        if let Some(change) = builder.build() {
            by_index.insert(index, change);
        }
    }
    Ok(by_index.into_values().collect())
}

/// A builder for transaction `tx` holding everything `change` holds, so the hook state can be
/// added to the same transaction with the builder's merge rules.
fn builder_with(tx: &Transaction, change: TransactionChanges) -> Result<TransactionChangesBuilder> {
    // The shared V4 pipeline emits components, attributes and balances only; this builder copies
    // nothing else, so any other field fails loudly instead of going missing.
    for (name, is_empty) in [
        ("contract_changes", change.contract_changes.is_empty()),
        ("entrypoints", change.entrypoints.is_empty()),
        ("entrypoint_params", change.entrypoint_params.is_empty()),
    ] {
        if !is_empty {
            return Err(anyhow!(
                "the shared V4 pipeline emitted {name}, which builder_with does not copy"
            ));
        }
    }
    let mut builder = TransactionChangesBuilder::new(tx);
    for component in &change.component_changes {
        builder.add_protocol_component(component);
    }
    for entity_change in &change.entity_changes {
        builder.add_entity_change(entity_change);
    }
    for balance_change in &change.balance_changes {
        builder.add_balance_change(balance_change);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use substreams_ethereum::pb::eth::v2::{
        Call, Log, StorageChange, TransactionReceipt, TransactionTraceStatus,
    };

    use rstest::rstest;

    use super::*;
    use crate::{
        address_list::Address,
        launchpad::{
            slots::pool_slots,
            test_fixtures::{
                params, params_with_montra_hooks, ASCEND_HOOK, ASCEND_LAUNCH_OBSERVATION_WORD,
                ASCEND_POOL, ASCEND_POOL_ID, ASCEND_TERMS_WORD, HOOK, POOL_ID, POOL_MANAGER, TOKEN,
            },
        },
        montra::pool_config::KeyWord,
        storage::{pad32, Word},
    };

    const POOL: &str = "0x35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2";
    const OTHER_POOL_ID: Word =
        hex!("06602fee3eef46c193ab60f7b658a1254aa382dd0545d18a66c94f4f8a6337af");

    /// `cast index bytes32 <pool id> 2` and `... 3` for pool 0x35bd0e6a...: the slots of its
    /// `PoolConfig` and `PoolKey` word 0.
    const CONFIG_SLOT: Word =
        hex!("0c1ddd7c3d6b64b1484514fd76865a01842ad2fea618d73d863145fd73feed26");
    const KEY_SLOT: Word = hex!("fda350181ec97f129d6c42370da0c529351cef42b0af7a63cfc4117f18404354");

    // Pool 0x35bd0e6a...'s hook words, read with eth_getStorageAt: PoolConfig words 0 to 3 and
    // PoolKey words 1 and 2 after its launch at 1029110 (word 3 is the recipient), and word 0 at
    // 1030048 (stage 1), 1037445 (stage 3) and 1037446 (stage 4, the LP fee switched on).
    const LAUNCH_WORD_0: Word =
        hex!("0000000000000001d58802292000000000000001000101000fa00003e8002710");
    const LAUNCH_WORD_1: Word =
        hex!("029748f276600000000000000000000000000000000000000000002710002710");
    const LAUNCH_WORD_2: Word =
        hex!("0000000000000000000000000000002b89a04b995f1542499d00000000000000");
    const RECIPIENT_WORD_3: Word =
        hex!("000000000000000000000000c73108fcd3db530d9ada5ceb8ff9447a888e30e9");
    const KEY_WORD_1: Word =
        hex!("0000000000000000c880000032193afd14c53f12c45a4c04eca9f7ba98e3ec71");
    const KEY_WORD_2: Word =
        hex!("0000000000000000000000006b1216a40b65c83e9c9e18a562c446697b8cf8ec");
    const STAGE_1_WORD_0: Word =
        hex!("0000000000000101d58802292000000000000001000101000fa00003e8002710");
    const STAGE_3_WORD_0: Word =
        hex!("01476aecd8160301d58802292000000000000001000101000fa00003e8002710");
    const STAGE_4_WORD_0: Word =
        hex!("01476aecd8160401d58802292000000000000001000101000fa00003e8002710");
    /// Word 0 after `register` alone (offsets in place of line ticks, `seeded` clear), from an
    /// anvil fork of Elysium at 6024400 replaying launch tx
    /// 0xcfeabcb8807834089cfbcb83192c137230074ea8492225890851081f289c0149 call by call, with this
    /// pool's terms.
    const REGISTERED_WORD_0: Word =
        hex!("0000000000000000c1c0006e2800000000000000000101000fa00003e8002710");
    /// `ModifyLiquidity(bytes32,address,int24,int24,int256,bytes32)`, from `cast keccak`.
    const MODIFY_LIQUIDITY_TOPIC: Word =
        hex!("f208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec");

    struct FakeStore(HashMap<String, String>);

    impl StoreGet<String> for FakeStore {
        fn new(_idx: u32) -> Self {
            Self(HashMap::new())
        }
        fn get_at<K: AsRef<str>>(&self, _ord: u64, key: K) -> Option<String> {
            self.get_last(key)
        }
        fn get_last<K: AsRef<str>>(&self, key: K) -> Option<String> {
            self.0.get(key.as_ref()).cloned()
        }
        fn get_first<K: AsRef<str>>(&self, key: K) -> Option<String> {
            self.get_last(key)
        }
        fn has_at<K: AsRef<str>>(&self, _ord: u64, key: K) -> bool {
            self.0.contains_key(key.as_ref())
        }
        fn has_last<K: AsRef<str>>(&self, key: K) -> bool {
            self.0.contains_key(key.as_ref())
        }
        fn has_first<K: AsRef<str>>(&self, key: K) -> bool {
            self.0.contains_key(key.as_ref())
        }
    }

    /// The store as `store_launchpad_slots` leaves it once the pool exists, written out literally.
    fn store() -> FakeStore {
        let hook = "0x6b1216a40b65c83e9c9e18a562c446697b8cf8ec";
        let config = format!(
            "slot:{hook}:0x0c1ddd7c3d6b64b1484514fd76865a01842ad2fea618d73d863145fd73feed2"
        );
        let key = format!(
            "slot:{hook}:0xfda350181ec97f129d6c42370da0c529351cef42b0af7a63cfc4117f1840435"
        );
        FakeStore(
            [
                (format!("{config}6"), 0),
                (format!("{config}7"), 1),
                (format!("{config}8"), 2),
                (format!("{key}4"), 4),
                (format!("{key}5"), 5),
                (format!("{key}6"), 6),
            ]
            .into_iter()
            .map(|(slot, index)| (slot, format!("{POOL}:{index}")))
            .collect(),
        )
    }

    /// A write by the hook to the slot `offset` words after `base`.
    fn write_at(base: Word, offset: u8, old: Word, new: Word, ordinal: u64) -> StorageChange {
        let mut slot = base;
        slot[31] += offset;
        StorageChange {
            address: HOOK.to_vec(),
            key: slot.to_vec(),
            old_value: old.to_vec(),
            new_value: new.to_vec(),
            ordinal,
        }
    }

    fn config_write(word: ConfigWord, old: Word, new: Word, ordinal: u64) -> StorageChange {
        write_at(CONFIG_SLOT, word as u8, old, new, ordinal)
    }

    fn key_write(word: KeyWord, old: Word, new: Word, ordinal: u64) -> StorageChange {
        write_at(KEY_SLOT, word as u8, old, new, ordinal)
    }

    /// A write to `PoolConfig` word 3, the recipient, which is not watched.
    fn recipient_write(old: Word, new: Word, ordinal: u64) -> StorageChange {
        write_at(CONFIG_SLOT, 3, old, new, ordinal)
    }

    /// The `ModifyLiquidity` the `PoolManager` emitted for the seed of pool 0x35bd0e6a... (launch
    /// tx 0xe6956588..., log 4: the hook adds 205600513195795932137885 over [-887200, 169800]),
    /// with its emitter, pool id and sender replaceable.
    fn seed_log(emitter: &Address, pool_id: &Word, sender: &Address) -> Log {
        let sender_topic = pad32(sender).expect("an address fits a word");
        Log {
            address: emitter.to_vec(),
            topics: vec![MODIFY_LIQUIDITY_TOPIC.to_vec(), pool_id.to_vec(), sender_topic.to_vec()],
            data: hex!(
                "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffff27660"
                "0000000000000000000000000000000000000000000000000000000000029748"
                "000000000000000000000000000000000000000000002b89a04b995f1542499d"
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .to_vec(),
            ..Default::default()
        }
    }

    fn trace(index: u32, changes: Vec<StorageChange>, logs: Vec<Log>) -> eth::TransactionTrace {
        eth::TransactionTrace {
            hash: vec![index as u8; 32],
            index,
            status: i32::from(TransactionTraceStatus::Succeeded),
            calls: vec![Call { storage_changes: changes, ..Default::default() }],
            receipt: Some(TransactionReceipt { logs, ..Default::default() }),
            ..Default::default()
        }
    }

    fn block(traces: Vec<eth::TransactionTrace>) -> eth::Block {
        eth::Block { number: 1029110, transaction_traces: traces, ..Default::default() }
    }

    fn tx(index: u64) -> Transaction {
        Transaction { hash: vec![index as u8; 32], index, ..Default::default() }
    }

    /// The pool created in transaction `index`, as `map_launchpad_pools_created` emits it.
    fn created_pools(index: u64) -> BlockEntityChanges {
        BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                tx: Some(tx(index)),
                component_changes: vec![ProtocolComponent::new(POOL)
                    .with_tokens(&[[0u8; 20], TOKEN])
                    .with_attributes(&[
                        ("tick_spacing", hex!("00c8").to_vec()),
                        ("pool_id", POOL_ID.to_vec()),
                        ("hooks", HOOK.to_vec()),
                        ("key_lp_fee", hex!("00800000").to_vec()),
                    ])],
                ..Default::default()
            }],
        }
    }

    /// The real launch's writes: `register` writes word 0 with offsets, `seed` rewrites it with
    /// the seeded flag and writes words 1 and 2; the unwatched word 3 and the two non-zero PoolKey
    /// words too.
    fn launch_writes() -> Vec<StorageChange> {
        let registered_word_1 =
            hex!("0000000000000000000000000000000000000000000000000000002710002710");
        vec![
            config_write(ConfigWord::Terms, [0; 32], REGISTERED_WORD_0, 10),
            config_write(ConfigWord::SeedRange, [0; 32], registered_word_1, 11),
            recipient_write([0; 32], RECIPIENT_WORD_3, 12),
            key_write(KeyWord::Currency1, [0; 32], KEY_WORD_1, 13),
            key_write(KeyWord::FeeTickSpacingHooks, [0; 32], KEY_WORD_2, 14),
            config_write(ConfigWord::Terms, REGISTERED_WORD_0, LAUNCH_WORD_0, 20),
            config_write(ConfigWord::SeedRange, registered_word_1, LAUNCH_WORD_1, 21),
            config_write(ConfigWord::EarlyBuyFee, [0; 32], LAUNCH_WORD_2, 22),
        ]
    }

    /// The writes of a `seed` in a transaction after the creation: words 0 to 2 in full.
    fn later_seed_writes() -> Vec<StorageChange> {
        vec![
            config_write(ConfigWord::Terms, REGISTERED_WORD_0, LAUNCH_WORD_0, 1),
            config_write(ConfigWord::SeedRange, [0; 32], LAUNCH_WORD_1, 2),
            config_write(ConfigWord::EarlyBuyFee, [0; 32], LAUNCH_WORD_2, 3),
        ]
    }

    /// A `PoolKey` word 2 that names another hook.
    fn changed_key_word_2() -> Word {
        let mut wrong_key = KEY_WORD_2;
        wrong_key[31] ^= 1;
        wrong_key
    }

    fn u64_bytes(value: u64) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    /// The attributes of `change` for `component`, sorted by name, as (name, value, change).
    fn attributes_of(
        change: &TransactionChanges,
        component: &str,
    ) -> Vec<(String, Vec<u8>, ChangeType)> {
        let mut attributes: Vec<_> = change
            .entity_changes
            .iter()
            .filter(|entity| entity.component_id == component)
            .flat_map(|entity| &entity.attributes)
            .map(|attribute| {
                (
                    attribute.name.clone(),
                    attribute.value.clone(),
                    ChangeType::from_i32(attribute.change).unwrap(),
                )
            })
            .collect();
        attributes.sort();
        attributes
    }

    /// The `paused` attribute of `change` for the pool, if any.
    fn paused_of(change: &TransactionChanges) -> Option<(Vec<u8>, ChangeType)> {
        attributes_of(change, POOL)
            .into_iter()
            .find(|(name, _, _)| name == "paused")
            .map(|(_, value, change)| (value, change))
    }

    /// The module's output for `block` with `launchpad_slots` as the store and `shared` as the
    /// shared pipeline's changes.
    fn run_with(
        launchpad_slots: &FakeStore,
        block: &eth::Block,
        created_pools: &BlockEntityChanges,
        shared: Vec<TransactionChanges>,
    ) -> Vec<TransactionChanges> {
        let launches = created_launches(&params(), created_pools).unwrap();
        let hook_writes =
            collect_hook_writes(block, &params(), launchpad_slots, &launches).unwrap();
        with_hook_state(shared, &launches, hook_writes).unwrap()
    }

    fn run(block: &eth::Block, created_pools: &BlockEntityChanges) -> Vec<TransactionChanges> {
        // The shared pipeline's changes for the creation, as `collect_transaction_changes` builds
        // them from `created_pools`.
        let shared = created_pools
            .changes
            .iter()
            .map(|change| {
                let mut builder = TransactionChangesBuilder::new(change.tx.as_ref().unwrap());
                change
                    .component_changes
                    .iter()
                    .for_each(|component| builder.add_protocol_component(component));
                builder.build().unwrap()
            })
            .collect();
        run_with(&store(), block, created_pools, shared)
    }

    #[test]
    fn test_store_fixture_lists_the_slots_store_launchpad_slots_writes() {
        let slots: Vec<String> = pool_slots(Layout::Montra, &POOL_ID)
            .into_iter()
            .map(|(_, slot)| slot_store_key(&HOOK, &slot))
            .collect();
        let mut fixture: Vec<String> = store().0.into_keys().collect();
        fixture.sort();
        let mut computed = slots.clone();
        computed.sort();
        assert_eq!(computed, fixture);
    }

    #[test]
    fn test_launch_creates_every_attribute_from_its_final_words() {
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);
        let changes = run(&block(vec![trace(1, launch_writes(), vec![seed])]), &created_pools(1));

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].component_changes.len(), 1);
        let created = ChangeType::Creation;
        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![
                ("montra_early_buy_fee_start_pips".to_string(), u64_bytes(0), created),
                ("montra_early_buy_fee_until".to_string(), u64_bytes(0), created),
                ("montra_early_buy_fee_window_seconds".to_string(), u64_bytes(0), created),
                ("montra_lp_crossed_at".to_string(), u64_bytes(0), created),
                ("montra_lp_fee_pips".to_string(), u64_bytes(1000), created),
                ("montra_lp_held_seconds".to_string(), u64_bytes(0), created),
                ("montra_quote_is_currency0".to_string(), vec![1], created),
                ("montra_seed_tick_lower".to_string(), hex!("fff27660").to_vec(), created),
                ("montra_seed_tick_upper".to_string(), hex!("00029748").to_vec(), created),
                ("montra_stage".to_string(), vec![0], created),
                ("montra_total_fee_pips".to_string(), u64_bytes(10000), created),
            ]
        );
    }

    #[test]
    fn test_launch_without_its_seed_is_created_paused_with_zero_for_unwritten_words() {
        let register_only = vec![
            config_write(ConfigWord::Terms, [0; 32], REGISTERED_WORD_0, 10),
            key_write(KeyWord::Currency1, [0; 32], KEY_WORD_1, 13),
        ];

        let changes = run(&block(vec![trace(1, register_only, vec![])]), &created_pools(1));

        let attributes = attributes_of(&changes[0], POOL);
        assert_eq!(paused_of(&changes[0]), Some((vec![1], ChangeType::Creation)));
        assert!(attributes.contains(&(
            "montra_total_fee_pips".to_string(),
            u64_bytes(10000),
            ChangeType::Creation
        )));
        assert!(attributes.contains(&(
            "montra_seed_tick_lower".to_string(),
            vec![0, 0, 0, 0],
            ChangeType::Creation
        )));
        assert_eq!(attributes.len(), 12);
    }

    #[test]
    fn test_launch_whose_hook_wrote_nothing_is_created_paused_with_zeros() {
        let changes = run(&block(vec![]), &created_pools(1));

        let attributes = attributes_of(&changes[0], POOL);
        assert_eq!(attributes.len(), 12);
        assert_eq!(paused_of(&changes[0]), Some((vec![1], ChangeType::Creation)));
        assert!(attributes.contains(&(
            "montra_total_fee_pips".to_string(),
            u64_bytes(0),
            ChangeType::Creation
        )));
    }

    /// `log` with its `liquidityDelta` (data word 2) set to `delta`, as a 32-byte two's complement.
    fn with_liquidity_delta(mut log: Log, delta: i8) -> Log {
        let fill = if delta < 0 { 0xff } else { 0x00 };
        log.data[64..96].fill(fill);
        log.data[95] = delta as u8;
        log
    }

    #[rstest]
    #[case::no_liquidity_event(vec![])]
    #[case::liquidity_on_another_pool(vec![seed_log(&POOL_MANAGER, &OTHER_POOL_ID, &HOOK)])]
    #[case::liquidity_from_another_sender(vec![seed_log(&POOL_MANAGER, &POOL_ID, &TOKEN)])]
    #[case::event_from_another_contract(vec![seed_log(&TOKEN, &POOL_ID, &HOOK)])]
    #[case::liquidity_removed(vec![with_liquidity_delta(seed_log(&POOL_MANAGER, &POOL_ID, &HOOK), -1)])]
    #[case::zero_liquidity_delta(vec![with_liquidity_delta(seed_log(&POOL_MANAGER, &POOL_ID, &HOOK), 0)])]
    fn test_launch_setting_the_seeded_flag_without_hook_liquidity_on_the_pool_is_paused(
        #[case] logs: Vec<Log>,
    ) {
        let changes = run(&block(vec![trace(1, launch_writes(), logs)]), &created_pools(1));

        assert_eq!(paused_of(&changes[0]), Some((vec![1], ChangeType::Creation)));
    }

    /// A field of the shared pipeline's output that `builder_with` does not copy fails the
    /// transaction by name rather than going missing.
    #[rstest]
    #[case::contract_changes(
        TransactionChanges { contract_changes: vec![Default::default()], ..Default::default() },
        "contract_changes"
    )]
    #[case::entrypoints(
        TransactionChanges { entrypoints: vec![Default::default()], ..Default::default() },
        "entrypoints"
    )]
    #[case::entrypoint_params(
        TransactionChanges {
            entrypoint_params: vec![Default::default()],
            ..Default::default()
        },
        "entrypoint_params"
    )]
    fn test_builder_with_rejects_a_field_it_does_not_copy(
        #[case] change: TransactionChanges,
        #[case] field: &str,
    ) {
        let tx = Transaction::default();

        let error = builder_with(&tx, change)
            .err()
            .expect("a field builder_with does not copy fails");

        assert_eq!(
            error.to_string(),
            format!("the shared V4 pipeline emitted {field}, which builder_with does not copy")
        );
    }

    #[test]
    fn test_created_pool_change_without_a_transaction_fails() {
        let mut created = created_pools(1);
        created.changes[0].tx = None;

        let error = created_launches(&params(), &created)
            .expect_err("a change without a transaction fails");

        assert_eq!(
            error.to_string(),
            "map_launchpad_pools_created emitted a change without a transaction"
        );
    }

    #[test]
    fn test_shared_changes_without_a_transaction_fail() {
        let shared = TransactionChanges {
            tx: None,
            entity_changes: vec![EntityChanges {
                component_id: POOL.to_string(),
                attributes: vec![],
            }],
            ..Default::default()
        };

        let error = with_hook_state(vec![shared], &HashMap::new(), HookWrites::new())
            .expect_err("shared changes without a transaction fail");

        assert_eq!(
            error.to_string(),
            "the shared V4 pipeline emitted changes without a transaction"
        );
    }

    /// `builder_with` keeps everything the shared pipeline emits for a pool's creation, as
    /// `pool_creation` builds it (the component with its tokens, static attributes and protocol
    /// type, the state attributes and both balances), and adds nothing.
    #[test]
    fn test_builder_with_keeps_the_production_creation_shape() {
        let created = BlockEntityChanges {
            block: None,
            changes: crate::variant_modules::map_launchpad_pools_created::pools_created(
                &params(),
                &crate::launchpad::test_fixtures::launch_block(vec![
                    crate::launchpad::test_fixtures::initialize_log(&POOL_MANAGER, &HOOK),
                ]),
            ),
        };
        let shared = collect_transaction_changes(
            created,
            Events::default(),
            BlockBalanceDeltas::default(),
            StoreDeltas::default(),
            TickDeltas::default(),
            StoreDeltas::default(),
            LiquidityChanges::default(),
            StoreDeltas::default(),
        );
        assert_eq!(shared.len(), 1);
        let change = shared[0].clone();
        assert!(!change.component_changes[0]
            .static_att
            .is_empty());
        assert!(change.component_changes[0]
            .protocol_type
            .is_some());
        assert_eq!(change.balance_changes.len(), 2);

        let rebuilt = builder_with(change.tx.as_ref().unwrap(), change.clone())
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(sorted(rebuilt), sorted(change));
    }

    /// `change` with every repeated field sorted, so two builds compare whatever their map order.
    fn sorted(mut change: TransactionChanges) -> TransactionChanges {
        change
            .entity_changes
            .sort_by(|a, b| a.component_id.cmp(&b.component_id));
        for entity in &mut change.entity_changes {
            entity
                .attributes
                .sort_by(|a, b| a.name.cmp(&b.name));
        }
        change
            .component_changes
            .sort_by(|a, b| a.id.cmp(&b.id));
        change
            .balance_changes
            .sort_by(|a, b| (&a.component_id, &a.token).cmp(&(&b.component_id, &b.token)));
        change
    }

    /// Two entries of the shared pipeline for one transaction fail instead of the second replacing
    /// the first.
    #[test]
    fn test_shared_changes_for_one_transaction_twice_fail() {
        let shared = TransactionChanges { tx: Some(tx(3)), ..Default::default() };

        let error =
            with_hook_state(vec![shared.clone(), shared], &HashMap::new(), HookWrites::new())
                .expect_err("a transaction index twice fails");

        assert_eq!(
            error.to_string(),
            "the shared V4 pipeline emitted two changes for transaction index 3"
        );
    }

    #[test]
    fn test_seed_liquidity_of_one_wei_unpauses_the_launch() {
        // The positive edge of the delta check that `liquidity_removed` and `zero_liquidity_delta`
        // hold on the other side.
        let seed = with_liquidity_delta(seed_log(&POOL_MANAGER, &POOL_ID, &HOOK), 1);

        let changes = run(&block(vec![trace(1, launch_writes(), vec![seed])]), &created_pools(1));

        assert_eq!(paused_of(&changes[0]), None);
    }

    #[test]
    fn test_seed_transaction_without_a_receipt_fails_the_block() {
        let mut launch = trace(1, launch_writes(), vec![seed_log(&POOL_MANAGER, &POOL_ID, &HOOK)]);
        launch.receipt = None;
        let launches = created_launches(&params(), &created_pools(1)).unwrap();

        let error = collect_hook_writes(&block(vec![launch]), &params(), &store(), &launches)
            .expect_err("a seed transaction without a receipt fails");

        assert_eq!(
            error.to_string(),
            format!(
                "transaction 0x{} sets the seeded flag of {POOL} but has no receipt",
                "01".repeat(32)
            )
        );
    }

    #[test]
    fn test_transaction_without_a_receipt_that_sets_no_seeded_flag_is_decoded() {
        let mut swap = trace(
            4,
            vec![config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 3)],
            vec![],
        );
        swap.receipt = None;

        let changes = run(&block(vec![swap]), &BlockEntityChanges::default());

        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![("montra_stage".to_string(), vec![4], ChangeType::Update)]
        );
    }

    #[test]
    fn test_seeded_flag_set_without_words_1_and_2_emits_no_unpause() {
        // Word 0 alone goes from registered to seeded, with the hook's liquidity on the pool: not
        // the writes `seed` makes, so the pool is not unpaused. The word's decoded fields are
        // equal before and after (only the seeded flag and the line ticks differ), so the
        // transaction emits nothing at all: no unpause, which a seed would emit with every
        // attribute.
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);

        let changes = run(
            &block(vec![trace(
                7,
                vec![config_write(ConfigWord::Terms, REGISTERED_WORD_0, LAUNCH_WORD_0, 1)],
                vec![seed],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes, vec![]);
    }

    #[test]
    fn test_swap_updates_only_the_attributes_it_changed() {
        let changes = run(
            &block(vec![trace(
                4,
                vec![config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 3)],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].tx, Some(tx(4)));
        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![("montra_stage".to_string(), vec![4], ChangeType::Update)]
        );
    }

    #[test]
    fn test_lower_total_fee_updates_only_the_total_fee() {
        // `lowerTotalFee` rewrites bytes [0..3) of word 0: TotalFeeUpdated(30000 -> 25000) took
        // them from 0x007530 to 0x0061a8 on pool 0x06cdc9d3... in block 5956162. The other bytes
        // are this pool's word 0 at stage 4.
        let mut before = STAGE_4_WORD_0;
        before[29..].copy_from_slice(&hex!("007530"));
        let mut after = STAGE_4_WORD_0;
        after[29..].copy_from_slice(&hex!("0061a8"));

        let changes = run(
            &block(vec![trace(5, vec![config_write(ConfigWord::Terms, before, after, 1)], vec![])]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![("montra_total_fee_pips".to_string(), u64_bytes(25000), ChangeType::Update)]
        );
    }

    #[test]
    fn test_line_crossing_updates_stage_and_hold_together() {
        let changes = run(
            &block(vec![trace(
                0,
                vec![config_write(ConfigWord::Terms, STAGE_1_WORD_0, STAGE_3_WORD_0, 3)],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![
                ("montra_lp_crossed_at".to_string(), u64_bytes(1793906710), ChangeType::Update),
                ("montra_lp_held_seconds".to_string(), u64_bytes(327), ChangeType::Update),
                ("montra_stage".to_string(), vec![3], ChangeType::Update),
            ]
        );
    }

    #[test]
    fn test_writes_to_bytes_the_simulation_does_not_read_emit_nothing() {
        // `lockFee` sets word 0's feeLocked byte (13 from the low end) and `updateRecipient`
        // rewrites word 3, which is not watched; neither writes anything else (anvil fork of
        // Elysium at 6024400, pool 0x07184505...).
        let mut locked = STAGE_4_WORD_0;
        locked[18] = 0x01;
        let changes = run(
            &block(vec![trace(
                2,
                vec![
                    config_write(ConfigWord::Terms, STAGE_4_WORD_0, locked, 1),
                    recipient_write(RECIPIENT_WORD_3, [0xbe; 32], 2),
                ],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes, vec![]);
    }

    #[test]
    fn test_later_seed_unpauses_and_rewrites_every_attribute() {
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);

        let changes = run(
            &block(vec![trace(7, later_seed_writes(), vec![seed])]),
            &BlockEntityChanges::default(),
        );

        // The values of pool 0x35bd0e6a...'s words after its seed, as the launch test reads them,
        // here as updates, and the pause deleted.
        let updated = ChangeType::Update;
        assert_eq!(changes.len(), 1);
        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![
                ("montra_early_buy_fee_start_pips".to_string(), u64_bytes(0), updated),
                ("montra_early_buy_fee_until".to_string(), u64_bytes(0), updated),
                ("montra_early_buy_fee_window_seconds".to_string(), u64_bytes(0), updated),
                ("montra_lp_crossed_at".to_string(), u64_bytes(0), updated),
                ("montra_lp_fee_pips".to_string(), u64_bytes(1000), updated),
                ("montra_lp_held_seconds".to_string(), u64_bytes(0), updated),
                ("montra_quote_is_currency0".to_string(), vec![1], updated),
                ("montra_seed_tick_lower".to_string(), hex!("fff27660").to_vec(), updated),
                ("montra_seed_tick_upper".to_string(), hex!("00029748").to_vec(), updated),
                ("montra_stage".to_string(), vec![0], updated),
                ("montra_total_fee_pips".to_string(), u64_bytes(10000), updated),
                ("paused".to_string(), vec![0], ChangeType::Deletion),
            ]
        );
    }

    #[test]
    fn test_changed_pool_key_pauses_a_live_pool() {
        let changes = run(
            &block(vec![trace(
                2,
                vec![key_write(KeyWord::FeeTickSpacingHooks, KEY_WORD_2, changed_key_word_2(), 1)],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![("paused".to_string(), vec![1], ChangeType::Creation)]
        );
    }

    #[test]
    fn test_pool_key_rewritten_unchanged_leaves_the_pool_as_it_is() {
        let changes = run(
            &block(vec![trace(
                2,
                vec![key_write(KeyWord::FeeTickSpacingHooks, KEY_WORD_2, KEY_WORD_2, 1)],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes, vec![]);
    }

    #[test]
    fn test_seed_after_a_key_change_leaves_the_pool_paused() {
        // A key changed in transaction 2 makes `seed` add its liquidity to the pool of the new key
        // in transaction 5, so nothing unpauses this pool.
        let elsewhere = seed_log(&POOL_MANAGER, &OTHER_POOL_ID, &HOOK);

        let changes = run(
            &block(vec![
                trace(
                    2,
                    vec![key_write(
                        KeyWord::FeeTickSpacingHooks,
                        KEY_WORD_2,
                        changed_key_word_2(),
                        1,
                    )],
                    vec![],
                ),
                trace(5, later_seed_writes(), vec![elsewhere]),
            ]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes.len(), 2);
        assert_eq!(paused_of(&changes[0]), Some((vec![1], ChangeType::Creation)));
        assert_eq!(paused_of(&changes[1]), None);
        // The attributes still follow the words the seed changed.
        assert!(attributes_of(&changes[1], POOL).contains(&(
            "montra_seed_tick_lower".to_string(),
            hex!("fff27660").to_vec(),
            ChangeType::Update
        )));
    }

    #[test]
    fn test_key_change_in_the_seed_transaction_pauses() {
        let mut writes = later_seed_writes();
        writes.push(key_write(KeyWord::FeeTickSpacingHooks, KEY_WORD_2, changed_key_word_2(), 4));
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);

        let changes =
            run(&block(vec![trace(5, writes, vec![seed])]), &BlockEntityChanges::default());

        assert_eq!(paused_of(&changes[0]), Some((vec![1], ChangeType::Creation)));
    }

    #[test]
    fn test_writes_before_the_creation_transaction_are_skipped() {
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);
        let changes = run(
            &block(vec![
                trace(
                    0,
                    vec![config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 1)],
                    vec![],
                ),
                trace(1, launch_writes(), vec![seed]),
            ]),
            &created_pools(1),
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].tx, Some(tx(1)));
        assert_eq!(paused_of(&changes[0]), None);
    }

    #[test]
    fn test_writes_of_other_contracts_and_unwatched_slots_are_ignored() {
        let mut other_contract = config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 1);
        other_contract.address = POOL_MANAGER.to_vec();
        let mut unwatched = config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 2);
        unwatched.key = vec![0x05];

        let changes = run(
            &block(vec![trace(2, vec![other_contract, unwatched], vec![])]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes, vec![]);
    }

    #[test]
    fn test_hook_state_joins_the_shared_changes_of_its_transaction_in_order() {
        let mut swap = TransactionChangesBuilder::new(&tx(3));
        swap.add_entity_change(&EntityChanges {
            component_id: POOL.to_string(),
            attributes: vec![Attribute {
                name: "tick".to_string(),
                value: vec![0x02, 0x97, 0x49],
                change: ChangeType::Update.into(),
            }],
        });
        let mut other = TransactionChangesBuilder::new(&tx(9));
        other.add_entity_change(&EntityChanges {
            component_id: "0x01".to_string(),
            attributes: vec![Attribute {
                name: "tick".to_string(),
                value: vec![0x01],
                change: ChangeType::Update.into(),
            }],
        });
        let launches = HashMap::new();
        let hook_writes = collect_hook_writes(
            &block(vec![
                trace(
                    3,
                    vec![config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 1)],
                    vec![],
                ),
                trace(
                    5,
                    vec![config_write(ConfigWord::Terms, STAGE_1_WORD_0, STAGE_3_WORD_0, 1)],
                    vec![],
                ),
            ]),
            &params(),
            &store(),
            &launches,
        )
        .unwrap();

        let changes = with_hook_state(
            vec![other.build().unwrap(), swap.build().unwrap()],
            &launches,
            hook_writes,
        )
        .unwrap();

        assert_eq!(
            changes
                .iter()
                .map(|change| change.tx.as_ref().unwrap().index)
                .collect::<Vec<_>>(),
            vec![3, 5, 9]
        );
        assert_eq!(
            attributes_of(&changes[0], POOL),
            vec![
                ("montra_stage".to_string(), vec![4], ChangeType::Update),
                ("tick".to_string(), vec![0x02, 0x97, 0x49], ChangeType::Update),
            ]
        );
        // Transaction 5 has hook state only: its own builder, with the line crossing's updates.
        assert_eq!(changes[1].tx, Some(tx(5)));
        assert_eq!(
            attributes_of(&changes[1], POOL),
            vec![
                ("montra_lp_crossed_at".to_string(), u64_bytes(1793906710), ChangeType::Update),
                ("montra_lp_held_seconds".to_string(), u64_bytes(327), ChangeType::Update),
                ("montra_stage".to_string(), vec![3], ChangeType::Update),
            ]
        );
        assert_eq!(changes[1].entity_changes.len(), 1);
        assert_eq!(
            attributes_of(&changes[2], "0x01"),
            vec![("tick".to_string(), vec![0x01], ChangeType::Update)]
        );
    }

    /// Words 1 and 2 written in full beside the hook's liquidity on the pool, while word 0 is
    /// not written, or is written without its `seeded` flag going from clear to set: neither is
    /// the writes of `seed`, so no unpause is emitted, only the attributes that changed.
    #[rstest]
    #[case::word_0_not_written(vec![])]
    #[case::word_0_already_seeded(vec![config_write(
        ConfigWord::Terms,
        STAGE_3_WORD_0,
        STAGE_4_WORD_0,
        1
    )])]
    #[case::word_0_not_seeded_after(vec![config_write(
        ConfigWord::Terms,
        [0; 32],
        REGISTERED_WORD_0,
        1
    )])]
    fn test_seed_words_without_the_seeded_flag_set_emit_no_unpause(
        #[case] word_0: Vec<StorageChange>,
    ) {
        let mut writes = word_0;
        writes.push(config_write(ConfigWord::SeedRange, [0; 32], LAUNCH_WORD_1, 2));
        writes.push(config_write(ConfigWord::EarlyBuyFee, [0; 32], LAUNCH_WORD_2, 3));
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);

        let changes =
            run(&block(vec![trace(7, writes, vec![seed])]), &BlockEntityChanges::default());

        assert_eq!(changes.len(), 1);
        assert_eq!(paused_of(&changes[0]), None);
        assert!(attributes_of(&changes[0], POOL)
            .iter()
            .all(|(_, _, change)| *change == ChangeType::Update));
    }

    /// The component id of [`OTHER_POOL_ID`].
    fn other_pool() -> String {
        format!("0x{}", hex::encode(OTHER_POOL_ID))
    }

    /// [`store`] with the slots of [`OTHER_POOL_ID`] too, as `store_launchpad_slots` writes them.
    fn store_with_other_pool() -> FakeStore {
        let mut store = store();
        for (word, slot) in pool_slots(Layout::Montra, &OTHER_POOL_ID) {
            let owner = SlotOwner { component_id: other_pool(), word };
            store
                .0
                .insert(slot_store_key(&HOOK, &slot), owner.to_store_value());
        }
        store
    }

    /// A write by the hook to `word` of [`OTHER_POOL_ID`]'s `PoolConfig`.
    fn other_config_write(word: ConfigWord, old: Word, new: Word, ordinal: u64) -> StorageChange {
        let (_, slot) = pool_slots(Layout::Montra, &OTHER_POOL_ID)
            .into_iter()
            .find(|(hook_word, _)| *hook_word == HookWord::Config(word))
            .unwrap();
        hook_write(&HOOK, slot, old, new, ordinal)
    }

    /// A write by `hook` to `slot`.
    fn hook_write(hook: &Address, slot: Word, old: Word, new: Word, ordinal: u64) -> StorageChange {
        StorageChange {
            address: hook.to_vec(),
            key: slot.to_vec(),
            old_value: old.to_vec(),
            new_value: new.to_vec(),
            ordinal,
        }
    }

    /// One transaction that seeds pool 0x35bd0e6a... and moves another Montra pool's stage, as a
    /// route through both would: each pool gets its own attributes, and only the pool the hook
    /// added liquidity to is unpaused, although both had their seed-shaped words written.
    #[test]
    fn test_two_pools_in_one_transaction_keep_their_own_writes() {
        let mut writes = later_seed_writes();
        writes.push(other_config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 4));
        writes.push(other_config_write(ConfigWord::SeedRange, [0; 32], LAUNCH_WORD_1, 5));
        writes.push(other_config_write(ConfigWord::EarlyBuyFee, [0; 32], LAUNCH_WORD_2, 6));
        let seed = seed_log(&POOL_MANAGER, &POOL_ID, &HOOK);
        let launches = HashMap::new();
        let hook_writes = collect_hook_writes(
            &block(vec![trace(7, writes, vec![seed])]),
            &params(),
            &store_with_other_pool(),
            &launches,
        )
        .unwrap();

        let changes = with_hook_state(vec![], &launches, hook_writes).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(paused_of(&changes[0]), Some((vec![0], ChangeType::Deletion)));
        assert_eq!(attributes_of(&changes[0], POOL).len(), 12);
        let other = attributes_of(&changes[0], &other_pool());
        assert!(
            !other
                .iter()
                .any(|(name, _, _)| name == "paused"),
            "{other:?}"
        );
        assert!(other.contains(&("montra_stage".to_string(), vec![4], ChangeType::Update)));
        assert!(other.contains(&(
            "montra_seed_tick_lower".to_string(),
            hex!("fff27660").to_vec(),
            ChangeType::Update
        )));
    }

    /// A second listed hook writing a slot of a pool of the first is not that pool's write: the
    /// store keys a slot by the hook it belongs to.
    #[test]
    fn test_write_by_another_listed_hook_to_a_pools_slot_is_ignored() {
        let second_hook = hex!("0000000000000000000000000000000000000002");
        let mut write = config_write(ConfigWord::Terms, STAGE_3_WORD_0, STAGE_4_WORD_0, 1);
        write.address = second_hook.to_vec();
        let params = params_with_montra_hooks(&[HOOK, second_hook]);

        let hook_writes = collect_hook_writes(
            &block(vec![trace(2, vec![write], vec![])]),
            &params,
            &store(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(hook_writes, HookWrites::new());
    }

    // Ascend pools: pool 0xa138ea3b... of the hook 0xe0def422..., written with the layouts of
    // `crate::ascend`.
    const ASCEND_WORD_2: Word = ASCEND_TERMS_WORD;
    const ASCEND_WORD_3: Word = ASCEND_LAUNCH_OBSERVATION_WORD;
    // Bid word 2: windowStart 0, maxClip 0xa49e728e240f3a9d.
    const ASCEND_BID_WORD_2: Word =
        hex!("00000000000000000000000000000000000000a49e728e240f3a9d0000000000");

    fn ascend_store() -> FakeStore {
        let mut store = FakeStore(HashMap::new());
        for (word, slot) in pool_slots(Layout::Ascend, &ASCEND_POOL_ID) {
            let owner = SlotOwner { component_id: ASCEND_POOL.to_string(), word };
            store
                .0
                .insert(slot_store_key(&ASCEND_HOOK, &slot), owner.to_store_value());
        }
        store
    }

    /// A write by the Ascend hook to `word` of the pool.
    fn ascend_write(word: AscendWord, old: Word, new: Word, ordinal: u64) -> StorageChange {
        let (_, slot) = pool_slots(Layout::Ascend, &ASCEND_POOL_ID)
            .into_iter()
            .find(|(pool_word, _)| *pool_word == HookWord::Ascend(word))
            .unwrap();
        hook_write(&ASCEND_HOOK, slot, old, new, ordinal)
    }

    fn created_ascend_pool(index: u64) -> BlockEntityChanges {
        BlockEntityChanges {
            block: None,
            changes: vec![TransactionEntityChanges {
                tx: Some(tx(index)),
                component_changes: vec![ProtocolComponent::new(ASCEND_POOL)
                    .with_tokens(&[TOKEN, [0xaa; 20]])
                    .with_attributes(&[
                        ("pool_id", ASCEND_POOL_ID.to_vec()),
                        ("hooks", ASCEND_HOOK.to_vec()),
                    ])],
                ..Default::default()
            }],
        }
    }

    fn run_ascend(
        block: &eth::Block,
        created_pools: &BlockEntityChanges,
    ) -> Vec<TransactionChanges> {
        run_with(&ascend_store(), block, created_pools, vec![])
    }

    fn attributes_of_ascend(change: &TransactionChanges) -> Vec<(String, Vec<u8>, ChangeType)> {
        attributes_of(change, ASCEND_POOL)
    }

    /// The launch writes `PoolData` words 2 and 3 and `Bid` word 2, and not `Bid`
    /// words 0 and 1: every attribute is created, those two as zero, and the pool is not paused.
    #[test]
    fn test_ascend_launch_creates_every_attribute_from_its_final_words() {
        let launch = vec![
            ascend_write(AscendWord::Terms, [0; 32], ASCEND_WORD_2, 10),
            ascend_write(AscendWord::Observation, [0; 32], ASCEND_WORD_3, 11),
            ascend_write(AscendWord::Window, [0; 32], ASCEND_BID_WORD_2, 12),
        ];

        let changes = run_ascend(&block(vec![trace(1, launch, vec![])]), &created_ascend_pool(1));

        assert_eq!(changes.len(), 1);
        let created = ChangeType::Creation;
        let zero_word = vec![0u8; 32];
        assert_eq!(
            attributes_of_ascend(&changes[0]),
            vec![
                ("ascend_bid_filled_in_window".to_string(), zero_word.clone(), created),
                ("ascend_bid_inventory".to_string(), zero_word, created),
                (
                    "ascend_bid_max_clip".to_string(),
                    hex!("0000000000000000a49e728e240f3a9d").to_vec(),
                    created
                ),
                ("ascend_bid_window_start".to_string(), u64_bytes(0), created),
                ("ascend_discount".to_string(), u64_bytes(0), created),
                (
                    "ascend_ema_sqrt_price_x96".to_string(),
                    hex!("00000000000000000883956ab34ccb0338ab12cd").to_vec(),
                    created
                ),
                ("ascend_ema_updated_at".to_string(), u64_bytes(0x6afecd23), created),
                ("ascend_fee_pips".to_string(), u64_bytes(10000), created),
                ("ascend_observed_tick".to_string(), hex!("fff9e260").to_vec(), created),
                ("ascend_start_tick".to_string(), hex!("fffd6d68").to_vec(), created),
                ("ascend_token_is_currency0".to_string(), vec![0], created),
            ]
        );
        assert_eq!(paused_of(&changes[0]), None);
    }

    /// A pool whose hook wrote nothing in the creation transaction still gets every attribute, as
    /// zero, and no pause. The simulation refuses such a pool for its zero EMA.
    #[test]
    fn test_ascend_launch_whose_hook_wrote_nothing_is_created_with_zeros() {
        let changes = run_ascend(&block(vec![]), &created_ascend_pool(1));

        let created = ChangeType::Creation;
        let zero_word = vec![0u8; 32];
        assert_eq!(
            attributes_of_ascend(&changes[0]),
            vec![
                ("ascend_bid_filled_in_window".to_string(), zero_word.clone(), created),
                ("ascend_bid_inventory".to_string(), zero_word, created),
                ("ascend_bid_max_clip".to_string(), vec![0; 16], created),
                ("ascend_bid_window_start".to_string(), u64_bytes(0), created),
                ("ascend_discount".to_string(), u64_bytes(0), created),
                ("ascend_ema_sqrt_price_x96".to_string(), vec![0; 20], created),
                ("ascend_ema_updated_at".to_string(), u64_bytes(0), created),
                ("ascend_fee_pips".to_string(), u64_bytes(0), created),
                ("ascend_observed_tick".to_string(), vec![0; 4], created),
                ("ascend_start_tick".to_string(), vec![0; 4], created),
                ("ascend_token_is_currency0".to_string(), vec![0], created),
            ]
        );
        assert_eq!(paused_of(&changes[0]), None);
    }

    /// A swap with dt > 0 rewrites word 3 only: the EMA and its timestamp change, the tick and the
    /// discount do not, and only the changed attributes are updated.
    #[test]
    fn test_ascend_swap_updates_only_the_attributes_it_changed() {
        let mut after = ASCEND_WORD_3;
        after[12..32].copy_from_slice(&hex!("00000000000000000883956ab34ccb0338ab12ce"));
        after[7..12].copy_from_slice(&hex!("006afecd24"));

        let changes = run_ascend(
            &block(vec![trace(
                4,
                vec![ascend_write(AscendWord::Observation, ASCEND_WORD_3, after, 3)],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].tx, Some(tx(4)));
        assert_eq!(
            attributes_of_ascend(&changes[0]),
            vec![
                (
                    "ascend_ema_sqrt_price_x96".to_string(),
                    hex!("00000000000000000883956ab34ccb0338ab12ce").to_vec(),
                    ChangeType::Update
                ),
                ("ascend_ema_updated_at".to_string(), u64_bytes(0x6afecd24), ChangeType::Update),
            ]
        );
    }

    /// A fill consumes the inventory, adds to the window's filled amount and moves the window:
    /// `Bid` words 0, 1 and 2 in one transaction, word 0 written twice (the first old value and the
    /// last new value net by ordinal).
    #[test]
    fn test_ascend_fill_updates_the_bid_words_netted_by_ordinal() {
        let word = |value: u8| {
            let mut word = [0u8; 32];
            word[31] = value;
            word
        };
        let mut window_after = ASCEND_BID_WORD_2;
        window_after[27..32].copy_from_slice(&hex!("006b000000"));
        let writes = vec![
            ascend_write(AscendWord::Inventory, word(9), word(5), 1),
            ascend_write(AscendWord::Inventory, word(5), word(4), 4),
            ascend_write(AscendWord::Filled, word(0), word(3), 2),
            ascend_write(AscendWord::Window, ASCEND_BID_WORD_2, window_after, 3),
        ];

        let changes =
            run_ascend(&block(vec![trace(6, writes, vec![])]), &BlockEntityChanges::default());

        let updated = ChangeType::Update;
        assert_eq!(
            attributes_of_ascend(&changes[0]),
            vec![
                ("ascend_bid_filled_in_window".to_string(), word(3).to_vec(), updated),
                ("ascend_bid_inventory".to_string(), word(4).to_vec(), updated),
                ("ascend_bid_window_start".to_string(), u64_bytes(0x6b000000), updated),
            ]
        );
    }

    /// The word values of the fill tx 0xffd66b8f... in block 3527702 on pool 0x21e12450...: its
    /// storage diff of the hook (debug_traceTransaction, prestateTracer in diff mode, on the
    /// Elysium node) holds `PoolData` word 3 and `Bid` words 0, 1 and 2 with these old and new
    /// values, except the old `Bid` word 2, which is this file's fixture. The test writes them to
    /// the slots of the fixture pool, since decoding does not depend on the pool.
    #[test]
    fn test_ascend_fill_of_the_chain_updates_the_observation_and_the_bid() {
        let writes = vec![
            ascend_write(
                AscendWord::Observation,
                hex!("a6faee65fdd4eb006af2315200000000000000000035c99cb59808be1c46b249"),
                hex!("9c77377cfdd4e0006af23cac00000000000000000035caf9bfe08f36589a8b9c"),
                1,
            ),
            ascend_write(
                AscendWord::Inventory,
                hex!("00000000000000000000000000000000000000000000000000006f5cd6b3fb24"),
                hex!("0000000000000000000000000000000000000000000000000000000000000001"),
                2,
            ),
            ascend_write(
                AscendWord::Filled,
                [0; 32],
                hex!("00000000000000000000000000000000000000000000000000006f5cd6b3fb23"),
                3,
            ),
            ascend_write(
                AscendWord::Window,
                ASCEND_BID_WORD_2,
                hex!("00000000000000000000000000000000000000a49e728e240f3a9d006af23cac"),
                4,
            ),
        ];

        let changes =
            run_ascend(&block(vec![trace(5, writes, vec![])]), &BlockEntityChanges::default());

        let updated = ChangeType::Update;
        assert_eq!(
            attributes_of_ascend(&changes[0]),
            vec![
                (
                    "ascend_bid_filled_in_window".to_string(),
                    hex!("00000000000000000000000000000000000000000000000000006f5cd6b3fb23")
                        .to_vec(),
                    updated
                ),
                (
                    "ascend_bid_inventory".to_string(),
                    hex!("0000000000000000000000000000000000000000000000000000000000000001")
                        .to_vec(),
                    updated
                ),
                ("ascend_bid_window_start".to_string(), u64_bytes(0x6af23cac), updated),
                ("ascend_discount".to_string(), u64_bytes(0x9c77377c), updated),
                (
                    "ascend_ema_sqrt_price_x96".to_string(),
                    hex!("00000000000000000035caf9bfe08f36589a8b9c").to_vec(),
                    updated
                ),
                ("ascend_ema_updated_at".to_string(), u64_bytes(0x6af23cac), updated),
                ("ascend_observed_tick".to_string(), hex!("fffdd4e0").to_vec(), updated),
            ]
        );
    }

    /// `fundBid` writes `Bid` word 0 only.
    #[test]
    fn test_ascend_fund_bid_updates_the_inventory() {
        let mut funded = [0u8; 32];
        funded[24..].copy_from_slice(&1_000_000_000_000_000_000u64.to_be_bytes());

        let changes = run_ascend(
            &block(vec![trace(
                2,
                vec![ascend_write(AscendWord::Inventory, [0; 32], funded, 1)],
                vec![],
            )]),
            &BlockEntityChanges::default(),
        );

        assert_eq!(
            attributes_of_ascend(&changes[0]),
            vec![("ascend_bid_inventory".to_string(), funded.to_vec(), ChangeType::Update)]
        );
    }

    /// A write by another contract to a watched slot.
    fn foreign_write() -> StorageChange {
        let mut write = ascend_write(AscendWord::Terms, ASCEND_WORD_2, ASCEND_WORD_3, 1);
        write.address = POOL_MANAGER.to_vec();
        write
    }

    /// A write to an unwatched slot of the hook (`accruedFees` at slot 6).
    fn unwatched_write() -> StorageChange {
        let mut write = ascend_write(AscendWord::Terms, ASCEND_WORD_2, ASCEND_WORD_3, 1);
        write.key = vec![0x06];
        write
    }

    /// A write that changes no decoded value: the creator bytes of `PoolData` word 2.
    fn undecoded_write() -> StorageChange {
        let mut creator_changed = ASCEND_WORD_2;
        creator_changed[31] ^= 1;
        ascend_write(AscendWord::Terms, ASCEND_WORD_2, creator_changed, 1)
    }

    #[rstest]
    #[case::foreign_contract(foreign_write())]
    #[case::unwatched_slot(unwatched_write())]
    #[case::undecoded_bytes(undecoded_write())]
    fn test_ascend_write_that_changes_no_attribute_emits_nothing(#[case] write: StorageChange) {
        let changes =
            run_ascend(&block(vec![trace(2, vec![write], vec![])]), &BlockEntityChanges::default());

        assert_eq!(changes, vec![]);
    }

    /// The Montra hook writing the Ascend pool's slot is not that pool's write: the store keys a
    /// slot by the hook it belongs to.
    #[test]
    fn test_ascend_slot_written_by_the_montra_hook_is_ignored() {
        let mut write = ascend_write(AscendWord::Terms, ASCEND_WORD_2, ASCEND_WORD_3, 1);
        write.address = HOOK.to_vec();

        let hook_writes = collect_hook_writes(
            &block(vec![trace(2, vec![write], vec![])]),
            &params(),
            &ascend_store(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(hook_writes, HookWrites::new());
    }

    /// A write before the pool's creation transaction in the same block is skipped, as for Montra.
    #[test]
    fn test_ascend_writes_before_the_creation_transaction_are_skipped() {
        let launch = vec![
            ascend_write(AscendWord::Terms, [0; 32], ASCEND_WORD_2, 10),
            ascend_write(AscendWord::Observation, [0; 32], ASCEND_WORD_3, 11),
        ];
        let early = vec![ascend_write(AscendWord::Inventory, [0; 32], ASCEND_WORD_2, 1)];

        let changes = run_ascend(
            &block(vec![trace(0, early, vec![]), trace(1, launch, vec![])]),
            &created_ascend_pool(1),
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].tx, Some(tx(1)));
        assert!(attributes_of_ascend(&changes[0]).contains(&(
            "ascend_bid_inventory".to_string(),
            vec![0; 32],
            ChangeType::Creation
        )));
    }
}
