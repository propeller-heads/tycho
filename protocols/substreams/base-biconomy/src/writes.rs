//! What one transaction wrote to the executor's boards, anchors and pause flags, matched to the
//! maker and pair its events name.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use substreams_ethereum::pb::eth;

use crate::biconomy::{
    decode_executor_log, is_board_header, paused_attribute, paused_slot, provider_of,
    touched_slots, Address, ExecutorEvent, Word,
};

#[derive(Default)]
pub struct ExecutorWrites {
    /// Attribute name to its final value in this transaction.
    pub attributes: BTreeMap<String, Vec<u8>>,
    /// Providers this transaction bound to a board or filled through.
    pub providers: BTreeSet<Address>,
}

pub fn executor_writes(tx: &eth::v2::TransactionTrace, executor: &Address) -> ExecutorWrites {
    let mut slots: HashMap<Word, String> = HashMap::new();
    let mut writes = ExecutorWrites::default();
    for (log, _) in tx.logs_with_calls() {
        if log.address != executor {
            continue;
        }
        match decode_executor_log(&log.topics, &log.data) {
            Some(ExecutorEvent::Board(touch)) => slots.extend(touched_slots(&touch)),
            Some(ExecutorEvent::Fill { touch, provider }) => {
                slots.extend(touched_slots(&touch));
                writes.providers.insert(provider);
            }
            Some(ExecutorEvent::Paused(mm)) => {
                slots.insert(paused_slot(&mm), paused_attribute(&mm));
            }
            None => {}
        }
    }
    if slots.is_empty() {
        return writes;
    }

    // Calls and their storage changes are in execution order, so the last write wins.
    for change in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter(|change| change.address == executor)
    {
        let Ok(key) = <Word>::try_from(change.key.as_slice()) else { continue };
        if let Some(name) = slots.get(&key) {
            writes
                .attributes
                .insert(name.clone(), change.new_value.clone());
        }
    }
    for (name, value) in &writes.attributes {
        if is_board_header(name) {
            if let Some(provider) = provider_of(value) {
                writes.providers.insert(provider);
            }
        }
    }
    writes
}
