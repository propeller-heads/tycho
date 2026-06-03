use std::collections::{HashMap, HashSet};

use ethabi::ethereum_types::U256;
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::lunarbase::{
    events::{
        decode_lunarbase_state_log, event_to_delta, EventApplyContext, EventApplyError,
        LogDecodeError, LunarBaseEvent, StateDelta,
    },
    state::{attrs, insert_bool, insert_u128, insert_u256, insert_u32, insert_u64, AttributeMap},
    Address,
};

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

pub struct BlockChangesBuilder {
    known_components: HashSet<String>,
    transactions: HashMap<Vec<u8>, tycho::TransactionChangesBuilder>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapState {
    pub blacklist_fee_multiplier: U256,
}

impl Default for BootstrapState {
    fn default() -> Self {
        Self { blacklist_fee_multiplier: 1.into() }
    }
}

impl BlockChangesBuilder {
    pub fn new(known_components: impl IntoIterator<Item = String>) -> Self {
        Self {
            known_components: known_components.into_iter().collect(),
            transactions: HashMap::new(),
        }
    }

    pub fn is_known_component(&self, component_id: &str) -> bool {
        self.known_components
            .contains(component_id)
    }

    pub fn register_component(
        &mut self,
        tx: &tycho::Transaction,
        component: tycho::ProtocolComponent,
        bootstrap_state: BootstrapState,
    ) {
        let component_id = component.id.clone();
        self.known_components
            .insert(component_id.clone());

        let tx_changes = self.transaction_mut(tx);
        tx_changes.add_protocol_component(&component);
        add_state_delta(tx_changes, &component_id, initial_state_delta(bootstrap_state));
    }

    pub fn apply_event(
        &mut self,
        tx: &tycho::Transaction,
        component_id: &str,
        event: &LunarBaseEvent,
        context: EventApplyContext,
        token_x: Address,
        token_y: Address,
    ) -> Result<(), BlockChangesError> {
        if !self.is_known_component(component_id) {
            return Err(BlockChangesError::UnknownComponent(component_id.to_owned()));
        }

        let delta = event_to_delta(event, context)?;
        if delta.updated_attributes.is_empty() {
            return Ok(());
        }
        let tx_changes = self.transaction_mut(tx);
        add_state_delta(tx_changes, component_id, delta);

        if let LunarBaseEvent::Sync { reserve_x, reserve_y } = event {
            tx_changes.add_balance_change(&tycho::BalanceChange {
                token: token_x.to_vec(),
                balance: reserve_x.to_be_bytes().to_vec(),
                component_id: component_id.as_bytes().to_vec(),
            });
            tx_changes.add_balance_change(&tycho::BalanceChange {
                token: token_y.to_vec(),
                balance: reserve_y.to_be_bytes().to_vec(),
                component_id: component_id.as_bytes().to_vec(),
            });
        }

        Ok(())
    }

    pub fn apply_log(
        &mut self,
        tx: &tycho::Transaction,
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

    pub fn finish(self, block: tycho::Block) -> tycho::BlockChanges {
        let mut changes = self
            .transactions
            .into_values()
            .filter_map(tycho::TransactionChangesBuilder::build)
            .collect::<Vec<_>>();
        changes.sort_unstable_by_key(|changes| {
            changes
                .tx
                .as_ref()
                .map(|tx| tx.index)
                .unwrap_or_default()
        });
        tycho::BlockChanges { block: Some(block), changes, storage_changes: Vec::new() }
    }

    fn transaction_mut(
        &mut self,
        tx: &tycho::Transaction,
    ) -> &mut tycho::TransactionChangesBuilder {
        self.transactions
            .entry(tx.hash.clone())
            .or_insert_with(|| tycho::TransactionChangesBuilder::new(tx))
    }
}

fn fixed_20(value: &[u8]) -> Option<[u8; 20]> {
    value.try_into().ok()
}

fn add_state_delta(
    tx: &mut tycho::TransactionChangesBuilder,
    component_id: &str,
    delta: StateDelta,
) {
    let mut attributes = delta
        .updated_attributes
        .into_iter()
        .map(|(name, value)| tycho::Attribute {
            name,
            value,
            change: tycho::ChangeType::Update.into(),
        })
        .collect::<Vec<_>>();
    attributes.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    tx.add_entity_change(&tycho::EntityChanges {
        component_id: component_id.to_owned(),
        attributes,
    });
}

fn initial_state_delta(bootstrap_state: BootstrapState) -> StateDelta {
    let mut updated_attributes = AttributeMap::new();
    insert_u128(&mut updated_attributes, attrs::ANCHOR_PRICE_X96, 0);
    insert_u32(&mut updated_attributes, attrs::FEE_ASK_X24, 0);
    insert_u32(&mut updated_attributes, attrs::FEE_BID_X24, 0);
    insert_u64(&mut updated_attributes, attrs::LATEST_UPDATE_BLOCK, 0);
    insert_u128(&mut updated_attributes, attrs::RESERVE_X, 0);
    insert_u128(&mut updated_attributes, attrs::RESERVE_Y, 0);
    insert_u32(&mut updated_attributes, attrs::CONCENTRATION_K, 0);
    insert_u64(&mut updated_attributes, attrs::BLOCK_DELAY, 2);
    insert_bool(&mut updated_attributes, attrs::PAUSED, false);
    insert_u256(
        &mut updated_attributes,
        attrs::BLACKLIST_FEE_MULTIPLIER,
        bootstrap_state.blacklist_fee_multiplier,
    );
    StateDelta { updated_attributes }
}
