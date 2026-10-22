//! What one transaction wrote to the storage the venue's pricing depends on: the executor's
//! boards, anchors and pause flags (matched to the maker and pair its events name), the venue's
//! fee and maker list, and the tracked token balances and allowances.

use std::collections::{BTreeMap, HashMap};

use substreams_ethereum::pb::eth;

use crate::{
    biconomy::{
        decode_executor_log, paused_attribute, paused_slot, touched_slots, venue_slots, Address,
        ExecutorEvent, Word,
    },
    modules::config::Config,
};

/// Attribute name to its final value in this transaction.
pub type Writes = BTreeMap<String, Vec<u8>>;

/// The final value of every watched slot this transaction wrote on `address`. Calls and their
/// storage changes are in execution order, so the last write wins.
fn watched_writes(
    tx: &eth::v2::TransactionTrace,
    address: &Address,
    slots: &HashMap<Word, String>,
    out: &mut Writes,
) {
    if slots.is_empty() {
        return;
    }
    for change in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter(|change| change.address == address)
    {
        let Ok(key) = <Word>::try_from(change.key.as_slice()) else { continue };
        if let Some(name) = slots.get(&key) {
            out.insert(name.clone(), change.new_value.clone());
        }
    }
}

/// Executor slots this transaction's executor events name.
fn executor_slots(tx: &eth::v2::TransactionTrace, executor: &Address) -> HashMap<Word, String> {
    let mut slots = HashMap::new();
    for (log, _) in tx.logs_with_calls() {
        if log.address != executor {
            continue;
        }
        match decode_executor_log(&log.topics, &log.data) {
            Some(ExecutorEvent::Board(touch)) => slots.extend(touched_slots(&touch)),
            Some(ExecutorEvent::Paused(mm)) => {
                slots.insert(paused_slot(&mm), paused_attribute(&mm));
            }
            None => {}
        }
    }
    slots
}

/// Precomputed slot lookups for the venue and the tracked tokens.
pub struct Watch {
    venue: HashMap<Word, String>,
    tokens: Vec<(Address, HashMap<Word, String>)>,
}

impl Watch {
    pub fn new(config: &Config) -> Self {
        Self {
            venue: venue_slots().into_iter().collect(),
            tokens: config
                .tokens
                .iter()
                .map(|token| {
                    (
                        token.address,
                        config
                            .inventory_slots(token)
                            .into_iter()
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    /// Everything this transaction wrote that the venue's pricing reads. With `tokens` false,
    /// token storage is skipped (the creation block, whose token state the bootstrap values
    /// already describe).
    pub fn writes(&self, tx: &eth::v2::TransactionTrace, config: &Config, tokens: bool) -> Writes {
        let mut out = Writes::new();
        watched_writes(tx, &config.executor, &executor_slots(tx, &config.executor), &mut out);
        watched_writes(tx, &config.venue, &self.venue, &mut out);
        if tokens {
            for (token, slots) in &self.tokens {
                watched_writes(tx, token, slots, &mut out);
            }
        }
        out
    }
}
