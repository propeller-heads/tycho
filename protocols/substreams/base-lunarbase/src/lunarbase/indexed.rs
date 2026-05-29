use std::collections::{HashMap, HashSet};

use substreams_ethereum::pb::eth;

use crate::lunarbase::{
    component::ProtocolComponent,
    events::{
        decode_lunarbase_state_log, event_to_delta, EventApplyContext, EventApplyError,
        LogDecodeError, LunarBaseEvent, StateDelta,
    },
    state::{attrs, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64, AttributeMap},
    Address,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalanceChange {
    pub token: Address,
    pub balance: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionChanges {
    pub tx: IndexedTransaction,
    pub new_protocol_components: Vec<ProtocolComponent>,
    pub state_updates: HashMap<String, StateDelta>,
    pub balance_changes: HashMap<String, Vec<BalanceChange>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IndexedTransaction {
    pub hash: [u8; 32],
    pub from: Address,
    pub to: Address,
    pub index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockChanges {
    pub transactions: Vec<TransactionChanges>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockChangesError {
    UnknownComponent(String),
    Event(EventApplyError),
    Log(LogDecodeError),
}

impl From<EventApplyError> for BlockChangesError {
    fn from(value: EventApplyError) -> Self {
        BlockChangesError::Event(value)
    }
}

impl From<LogDecodeError> for BlockChangesError {
    fn from(value: LogDecodeError) -> Self {
        BlockChangesError::Log(value)
    }
}

#[derive(Clone, Debug)]
pub struct BlockChangesBuilder {
    known_components: HashSet<String>,
    transactions: Vec<TransactionChanges>,
}

impl BlockChangesBuilder {
    pub fn new(known_components: impl IntoIterator<Item = String>) -> Self {
        Self { known_components: known_components.into_iter().collect(), transactions: Vec::new() }
    }

    pub fn register_component(&mut self, tx: IndexedTransaction, component: ProtocolComponent) {
        let component_id = component.id.clone();
        self.known_components
            .insert(component_id.clone());
        let tx_changes = self.transaction_mut(tx);
        tx_changes
            .new_protocol_components
            .push(component);
        merge_state_delta(tx_changes, &component_id, initial_state_delta());
    }

    pub fn apply_event(
        &mut self,
        tx: IndexedTransaction,
        component_id: &str,
        event: &LunarBaseEvent,
        context: EventApplyContext,
        token_x: Address,
        token_y: Address,
    ) -> Result<(), BlockChangesError> {
        if !self
            .known_components
            .contains(component_id)
        {
            return Err(BlockChangesError::UnknownComponent(component_id.to_owned()));
        }

        let delta = event_to_delta(event, context)?;
        if delta.updated_attributes.is_empty() {
            return Ok(());
        }
        let tx_changes = self.transaction_mut(tx);
        merge_state_delta(tx_changes, component_id, delta);

        if let LunarBaseEvent::Sync { reserve_x, reserve_y } = event {
            tx_changes.balance_changes.insert(
                component_id.to_owned(),
                vec![
                    BalanceChange { token: token_x, balance: *reserve_x },
                    BalanceChange { token: token_y, balance: *reserve_y },
                ],
            );
        }

        Ok(())
    }

    pub fn apply_log(
        &mut self,
        tx: IndexedTransaction,
        log: &eth::v2::Log,
        context: EventApplyContext,
        token_x: Address,
        token_y: Address,
    ) -> Result<(), BlockChangesError> {
        let Some(event) = decode_lunarbase_state_log(log)? else {
            return Ok(());
        };
        let Some(address) = fixed_20(&log.address) else {
            return Ok(());
        };
        let component_id = crate::lunarbase::component::component_id(address);
        self.apply_event(tx, &component_id, &event, context, token_x, token_y)
    }

    pub fn finish(self) -> BlockChanges {
        BlockChanges { transactions: self.transactions }
    }

    fn transaction_mut(&mut self, tx: IndexedTransaction) -> &mut TransactionChanges {
        if let Some(idx) = self
            .transactions
            .iter()
            .position(|changes| changes.tx.hash == tx.hash)
        {
            return &mut self.transactions[idx];
        }

        self.transactions
            .push(TransactionChanges {
                tx,
                new_protocol_components: Vec::new(),
                state_updates: HashMap::new(),
                balance_changes: HashMap::new(),
            });
        self.transactions
            .last_mut()
            .expect("transaction was just pushed")
    }
}

fn fixed_20(value: &[u8]) -> Option<[u8; 20]> {
    value.try_into().ok()
}

fn merge_state_delta(tx: &mut TransactionChanges, component_id: &str, delta: StateDelta) {
    let entry = tx
        .state_updates
        .entry(component_id.to_owned())
        .or_default();

    for (name, value) in delta.updated_attributes {
        entry
            .updated_attributes
            .insert(name, value);
    }
}

fn initial_state_delta() -> StateDelta {
    let mut updated_attributes = AttributeMap::new();
    insert_u128(&mut updated_attributes, attrs::ANCHOR_PRICE_X96, 0);
    insert_u32(&mut updated_attributes, attrs::FEE_ASK_X24, 0);
    insert_u32(&mut updated_attributes, attrs::FEE_BID_X24, 0);
    insert_u64(&mut updated_attributes, attrs::LATEST_UPDATE_BLOCK, 0);
    insert_u128(&mut updated_attributes, attrs::RESERVE_X, 0);
    insert_u128(&mut updated_attributes, attrs::RESERVE_Y, 0);
    insert_u32(&mut updated_attributes, attrs::CONCENTRATION_K, 0);
    insert_u64(&mut updated_attributes, attrs::BLOCK_DELAY, 2);
    insert_bool(&mut updated_attributes, attrs::PAUSED, true);
    insert_u256(&mut updated_attributes, attrs::BLACKLIST_FEE_MULTIPLIER, 1.into());
    insert_bool(&mut updated_attributes, attrs::EXECUTOR_WHITELISTED, false);
    StateDelta { updated_attributes }
}
