// Copyright (c) 2026 Everlong Labs Limited
//! Storage words: the writes of a block, the code of the contracts it creates, and the view that
//! answers what a word is worth after a given transaction (block writes first, then the words store
//! as of the start of the block, then the manifest seeds).
use std::{cell::RefCell, collections::HashMap};

use substreams_ethereum::pb::eth::v2::Block;

use crate::flamm::{
    keys::{hex_address, hex_word, keccak256, Address, Word},
    pad_word, Role,
};

/// One storage write of a tracked contract, in block order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockWrite {
    pub tx_index: u64,
    pub ordinal: u64,
    pub address: Address,
    pub key: Word,
    pub value: Word,
}

/// The storage writes of successful transactions and non-reverted calls whose `(address, key)`
/// passes the filter, sorted by transaction then ordinal.
pub fn block_writes(
    block: &Block,
    mut keep: impl FnMut(&Address, &Word) -> bool,
) -> Vec<BlockWrite> {
    let mut out = Vec::new();
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                let Ok(address) = Address::try_from(change.address.as_slice()) else { continue };
                let key = pad_word(&change.key);
                if !keep(&address, &key) {
                    continue;
                }
                out.push(BlockWrite {
                    tx_index: tx.index as u64,
                    ordinal: change.ordinal,
                    address,
                    key,
                    value: pad_word(&change.new_value),
                });
            }
        }
    }
    out.sort_by_key(|w| (w.tx_index, w.ordinal));
    out
}

/// A contract created in the block whose runtime codehash the registry names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub tx_index: u64,
    pub ordinal: u64,
    pub address: Address,
    pub role: Role,
    pub codehash: Word,
}

/// `keccak256(new_code)` of every code change in a successful, non-reverted call, matched against
/// the registry. The hash is recomputed from the code rather than trusted from the block model.
pub fn deployments_in_block(block: &Block, registry: &HashMap<Word, Role>) -> Vec<Deployment> {
    let mut out = Vec::new();
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.code_changes {
                if change.new_code.is_empty() {
                    continue;
                }
                let codehash = keccak256(&change.new_code);
                let Some(role) = registry.get(&codehash) else { continue };
                let Ok(address) = Address::try_from(change.address.as_slice()) else { continue };
                out.push(Deployment {
                    tx_index: tx.index as u64,
                    ordinal: change.ordinal,
                    address,
                    role: *role,
                    codehash,
                });
            }
        }
    }
    out.sort_by_key(|d| (d.tx_index, d.ordinal));
    out
}

/// The words store key of `(address, slot)`.
pub fn store_key(address: &Address, key: &Word) -> String {
    format!("word:{}:{}", hex_address(address), hex_word(key))
}

/// The deployments store key of an address.
pub fn deploy_key(address: &Address) -> String {
    format!("deploy:{}", hex_address(address))
}

/// `(tx_index, ordinal, value)` writes of one word, in block order.
type Writes = Vec<(u64, u64, Word)>;
/// The words store as of the start of the block.
type FirstWord<'a> = Box<dyn Fn(&Address, &Word) -> Option<Word> + 'a>;

/// A word's value after transaction `tx_index` of the block: the last write up to that transaction,
/// else the store's value at the start of the block, else the manifest seed. The store lookup is a
/// substreams host call, so each `(address, key)` is asked once per view (a block asks for the
/// same rotation and hot words on every transaction that touches a feed).
pub struct WordView<'a> {
    writes: HashMap<(Address, Word), Writes>,
    first: FirstWord<'a>,
    before: RefCell<HashMap<(Address, Word), Option<Word>>>,
    seeds: &'a HashMap<(Address, Word), Word>,
}

impl<'a> WordView<'a> {
    pub fn new(
        writes: &[BlockWrite],
        first: impl Fn(&Address, &Word) -> Option<Word> + 'a,
        seeds: &'a HashMap<(Address, Word), Word>,
    ) -> Self {
        let mut map: HashMap<(Address, Word), Writes> = HashMap::new();
        for w in writes {
            map.entry((w.address, w.key))
                .or_default()
                .push((w.tx_index, w.ordinal, w.value));
        }
        Self { writes: map, first: Box::new(first), before: RefCell::new(HashMap::new()), seeds }
    }

    /// The value before any write of this block.
    pub fn before_block(&self, address: &Address, key: &Word) -> Option<Word> {
        *self
            .before
            .borrow_mut()
            .entry((*address, *key))
            .or_insert_with(|| {
                (self.first)(address, key).or_else(|| {
                    self.seeds
                        .get(&(*address, *key))
                        .copied()
                })
            })
    }

    /// The value after every write of transactions `<= tx_index`.
    pub fn at(&self, address: &Address, key: &Word, tx_index: u64) -> Option<Word> {
        self.writes
            .get(&(*address, *key))
            .and_then(|ws| {
                ws.iter()
                    .rev()
                    .find(|(t, _, _)| *t <= tx_index)
                    .map(|(_, _, v)| *v)
            })
            .or_else(|| self.before_block(address, key))
    }

    /// The value before transaction `tx_index`: after every write of transactions `< tx_index`,
    /// else the value before the block.
    pub fn before_tx(&self, address: &Address, key: &Word, tx_index: u64) -> Option<Word> {
        self.writes
            .get(&(*address, *key))
            .and_then(|ws| {
                ws.iter()
                    .rev()
                    .find(|(t, _, _)| *t < tx_index)
                    .map(|(_, _, v)| *v)
            })
            .or_else(|| self.before_block(address, key))
    }

    /// Whether the word was written in transaction `tx_index`.
    pub fn written_in(&self, address: &Address, key: &Word, tx_index: u64) -> bool {
        self.writes
            .get(&(*address, *key))
            .is_some_and(|ws| {
                ws.iter()
                    .any(|(t, _, _)| *t == tx_index)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_prefers_block_writes_then_store_then_seed() {
        let a = [1u8; 20];
        let k = [2u8; 32];
        let writes = vec![
            BlockWrite { tx_index: 3, ordinal: 30, address: a, key: k, value: [3u8; 32] },
            BlockWrite { tx_index: 5, ordinal: 50, address: a, key: k, value: [5u8; 32] },
        ];
        let mut seeds = HashMap::new();
        seeds.insert((a, k), [9u8; 32]);
        let view = WordView::new(&writes, |_, _| None, &seeds);
        assert_eq!(view.at(&a, &k, 2), Some([9u8; 32]));
        assert_eq!(view.at(&a, &k, 3), Some([3u8; 32]));
        assert_eq!(view.at(&a, &k, 4), Some([3u8; 32]));
        assert_eq!(view.at(&a, &k, 9), Some([5u8; 32]));
        assert_eq!(view.before_tx(&a, &k, 3), Some([9u8; 32]));
        assert_eq!(view.before_tx(&a, &k, 5), Some([3u8; 32]));
        assert_eq!(view.before_tx(&a, &k, 6), Some([5u8; 32]));
        assert!(view.written_in(&a, &k, 5));
        assert!(!view.written_in(&a, &k, 4));
        let calls = std::cell::Cell::new(0);
        let view = WordView::new(
            &writes,
            |_, _| {
                calls.set(calls.get() + 1);
                Some([7u8; 32])
            },
            &seeds,
        );
        assert_eq!(view.at(&a, &k, 0), Some([7u8; 32]));
        assert_eq!(view.before_tx(&a, &k, 3), Some([7u8; 32]));
        assert_eq!(view.at(&[3u8; 20], &k, 0), Some([7u8; 32]));
        assert_eq!(calls.get(), 2, "one store lookup per (address, key)");
    }
}
