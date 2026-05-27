use std::collections::{HashMap, HashSet};

use crate::lunarbase::{
    component::ProtocolComponent,
    decoder::StateDelta,
    events::{event_to_delta, EventApplyContext, EventApplyError, LunarBaseEvent},
    evm_log::{decode_lunarbase_state_log, EvmLog, LogDecodeError},
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
        self.known_components
            .insert(component.id.clone());
        let tx_changes = self.transaction_mut(tx);
        tx_changes
            .new_protocol_components
            .push(component);
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
        if delta.updated_attributes.is_empty() && delta.deleted_attributes.is_empty() {
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
        log: &EvmLog,
        context: EventApplyContext,
        token_x: Address,
        token_y: Address,
    ) -> Result<(), BlockChangesError> {
        let Some(event) = decode_lunarbase_state_log(log)? else {
            return Ok(());
        };
        let component_id = crate::lunarbase::component::component_id(log.address);
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

fn merge_state_delta(tx: &mut TransactionChanges, component_id: &str, delta: StateDelta) {
    let entry = tx
        .state_updates
        .entry(component_id.to_owned())
        .or_default();

    for deleted in delta.deleted_attributes {
        entry
            .updated_attributes
            .remove(&deleted);
        entry.deleted_attributes.insert(deleted);
    }

    for (name, value) in delta.updated_attributes {
        entry.deleted_attributes.remove(&name);
        entry
            .updated_attributes
            .insert(name, value);
    }
}
