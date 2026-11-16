//! What one transaction wrote to the storage the venue's pricing depends on: the executor's
//! boards, anchors and pause flags (matched to the maker and pair its events name), the venue's
//! fee and maker list, the providers' signer and executor and the vaults' router, and the
//! tracked token balances and allowances.

use std::collections::{BTreeMap, HashMap};

use substreams_ethereum::pb::eth;

use crate::{
    biconomy::{
        decode_executor_log, paused_attribute, paused_slot, touched_slots, venue_slots, Address,
        ExecutorEvent, Touch, Word,
    },
    modules::config::Config,
};

/// Attribute name to its final value in this transaction.
pub type Writes = BTreeMap<String, Vec<u8>>;

/// Watched storage slots by contract, each with its attribute.
pub type Watched = HashMap<Address, HashMap<Word, String>>;

/// The final value of every watched slot this transaction wrote, in one pass over its storage
/// changes. A parent call that writes after its child returns comes before the child in `calls`,
/// so writes are ordered by their ordinal and the last one wins.
fn watched_writes(tx: &eth::v2::TransactionTrace, watched: &Watched, out: &mut Writes) {
    if watched.is_empty() {
        return;
    }
    let mut changes: Vec<(&eth::v2::StorageChange, &String)> = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter_map(|change| {
            let address = <Address>::try_from(change.address.as_slice()).ok()?;
            let key = <Word>::try_from(change.key.as_slice()).ok()?;
            watched
                .get(&address)?
                .get(&key)
                .map(|name| (change, name))
        })
        .collect();
    changes.sort_by_key(|(change, _)| change.ordinal);
    for (change, name) in changes {
        out.insert(name.clone(), change.new_value.clone());
    }
}

/// Executor slots this transaction's executor events name, for makers `listed` returns true
/// for and pairs of tracked tokens. Commits are permissionless, so anything else is ignored.
fn executor_slots(
    tx: &eth::v2::TransactionTrace,
    config: &Config,
    listed: &dyn Fn(&Address) -> bool,
) -> HashMap<Word, String> {
    let mut slots = HashMap::new();
    for (log, _) in tx.logs_with_calls() {
        if log.address != config.executor {
            continue;
        }
        match decode_executor_log(&log.topics, &log.data) {
            Some(ExecutorEvent::Board(touch)) if tracked(&touch, config, listed) => {
                slots.extend(touched_slots(&touch))
            }
            Some(ExecutorEvent::Paused(mm)) if listed(&mm) => {
                slots.insert(paused_slot(&mm), paused_attribute(&mm));
            }
            _ => {}
        }
    }
    slots
}

fn tracked(touch: &Touch, config: &Config, listed: &dyn Fn(&Address) -> bool) -> bool {
    listed(&touch.mm) && config.tracks_token(&touch.token_a) && config.tracks_token(&touch.token_b)
}

/// Board, anchor and pause words this transaction wrote for listed makers on tracked pairs.
pub fn executor_writes(
    tx: &eth::v2::TransactionTrace,
    config: &Config,
    listed: &dyn Fn(&Address) -> bool,
) -> Writes {
    let mut out = Writes::new();
    let slots = executor_slots(tx, config, listed);
    if !slots.is_empty() {
        watched_writes(tx, &Watched::from([(config.executor, slots)]), &mut out);
    }
    out
}

/// Precomputed slot lookups: the venue, providers and vaults, and with them the tracked tokens.
pub struct Watch {
    state: Watched,
    with_tokens: Watched,
}

impl Watch {
    pub fn new(config: &Config) -> Self {
        let mut state = Watched::new();
        state
            .entry(config.venue)
            .or_default()
            .extend(venue_slots());
        for (address, slots) in config.provider_state_slots() {
            state
                .entry(address)
                .or_default()
                .extend(slots);
        }
        let mut with_tokens = state.clone();
        for token in &config.tokens {
            with_tokens
                .entry(token.address)
                .or_default()
                .extend(config.inventory_slots(token));
        }
        Self { state, with_tokens }
    }

    /// Everything this transaction wrote to the venue, provider and vault slots, and with
    /// `tokens` the tracked token storage. Token storage is skipped in the creation block,
    /// whose token state the bootstrap values already describe.
    pub fn state_writes(&self, tx: &eth::v2::TransactionTrace, tokens: bool) -> Writes {
        let mut out = Writes::new();
        watched_writes(tx, if tokens { &self.with_tokens } else { &self.state }, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(address: u8, key: u8, value: u8, ordinal: u64) -> eth::v2::StorageChange {
        let mut word = [0u8; 32];
        word[31] = key;
        eth::v2::StorageChange {
            address: [address; 20].to_vec(),
            key: word.to_vec(),
            new_value: vec![value],
            ordinal,
            ..Default::default()
        }
    }

    fn slot(key: u8) -> Word {
        let mut word = [0u8; 32];
        word[31] = key;
        word
    }

    #[test]
    fn one_pass_keeps_the_last_write_by_ordinal_across_contracts() {
        // The parent call writes slot 1 after its child returns, so it comes first in `calls`
        // but has the later ordinal. Contract 3 is not watched.
        let tx = eth::v2::TransactionTrace {
            calls: vec![
                eth::v2::Call {
                    storage_changes: vec![change(1, 1, 9, 30), change(3, 1, 7, 31)],
                    ..Default::default()
                },
                eth::v2::Call {
                    storage_changes: vec![change(1, 1, 5, 20), change(2, 4, 6, 21)],
                    ..Default::default()
                },
                eth::v2::Call {
                    storage_changes: vec![change(2, 4, 8, 40)],
                    state_reverted: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let watched = Watched::from([
            ([1u8; 20], HashMap::from([(slot(1), "a".to_owned())])),
            ([2u8; 20], HashMap::from([(slot(4), "b".to_owned())])),
        ]);
        let mut out = Writes::new();
        watched_writes(&tx, &watched, &mut out);

        assert_eq!(out.get("a"), Some(&vec![9]));
        // The reverted call's write is ignored.
        assert_eq!(out.get("b"), Some(&vec![6]));
        assert_eq!(out.len(), 2);
    }
}
