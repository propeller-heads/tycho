//! The recorded streams, written with the protobuf test builders the way substreams sends a
//! stream: every block extends the current chain, finality trails the newest block, and undo
//! signals return to a block of the current chain.

use std::str::FromStr;

use chrono::NaiveDateTime;
use tycho_common::{
    keccak256,
    models::{Address, BlockHash, StoreKey},
    Bytes,
};
use tycho_protobuf::pb::tycho::evm::v1::{self as pb, entry_point_params::TraceData, ChangeType};
use tycho_storage::postgres::db_fixtures::yesterday_midnight;

use super::{
    rpc::{address_word, balance_slot, delegate, implementation_slot, slot},
    Delivery, Recorded, RecordedStream,
};
use crate::{
    pb::sf::substreams::{
        rpc::v2::{BlockScopedData, BlockUndoSignal},
        v1::{BlockRef, Clock},
    },
    testing::fixtures::pb_block_scoped_data,
};

/// Protocol type of the components in the native stream.
pub(super) const NATIVE_POOL: &str = "recorded_native_pool";

/// Protocol type of the components in the DCI stream.
pub(super) const VM_POOL: &str = "recorded_vm_pool";

/// How many blocks the finalized height trails the newest block by.
const FINALITY_LAG: u64 = 2;

/// Seconds between two blocks.
pub(super) const BLOCK_TIME: u64 = 12;

/// A token the recorded streams use.
pub(super) struct FixtureToken {
    /// Hex address without prefix.
    pub(super) address: &'static str,
    pub(super) symbol: &'static str,
    pub(super) decimals: u32,
    /// Price in the database before the extractor first starts. `None` for a token the database
    /// does not know yet: its metadata comes from the token pre-processor and its balances add
    /// nothing to TVL.
    pub(super) price: Option<f64>,
}

impl FixtureToken {
    pub(super) fn address(&self) -> Address {
        Address::from_str(self.address).expect("fixture token addresses are hex")
    }
}

// Prices are powers of two, so TVL is exact whatever order its terms are summed in.
pub(super) const WETH: FixtureToken = FixtureToken {
    address: "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
    symbol: "WETH",
    decimals: 18,
    price: Some(1.0),
};
const USDC: FixtureToken = FixtureToken {
    address: "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
    symbol: "USDC",
    decimals: 6,
    price: Some(0.5),
};
const DAI: FixtureToken = FixtureToken {
    address: "6b175474e89094c44da98b954eedeac495271d0f",
    symbol: "DAI",
    decimals: 18,
    price: Some(2.0),
};
const USDT: FixtureToken = FixtureToken {
    address: "dac17f958d2ee523a2206206994597c13d831ec7",
    symbol: "USDT",
    decimals: 6,
    price: Some(4.0),
};
const TKNA: FixtureToken = FixtureToken {
    address: "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a",
    symbol: "TKNA",
    decimals: 9,
    price: None,
};
const TKNB: FixtureToken = FixtureToken {
    address: "2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b",
    symbol: "TKNB",
    decimals: 12,
    price: None,
};

/// Every token the recorded streams use.
pub(super) const TOKENS: [&FixtureToken; 6] = [&WETH, &USDC, &DAI, &USDT, &TKNA, &TKNB];

/// An entry point of the DCI stream, traced with one calldata.
struct FixtureEntryPoint {
    id: &'static str,
    target: [u8; 20],
    calldata: u8,
}

impl FixtureEntryPoint {
    fn target(&self) -> Address {
        Address::from(self.target)
    }
}

const EP_A: FixtureEntryPoint =
    FixtureEntryPoint { id: "ep_a", target: [0x0a; 20], calldata: 0x01 };
const EP_B: FixtureEntryPoint =
    FixtureEntryPoint { id: "ep_b", target: [0x0b; 20], calldata: 0x02 };
const EP_C: FixtureEntryPoint =
    FixtureEntryPoint { id: "ep_c", target: [0x0c; 20], calldata: 0x03 };
const EP_D: FixtureEntryPoint =
    FixtureEntryPoint { id: "ep_d", target: [0x0d; 20], calldata: 0x04 };

/// Sender and recipient of every recorded transaction.
const SENDER: [u8; 20] = [0x5e; 20];
const ROUTER: [u8; 20] = [0x7e; 20];

/// The native stream: components created and deleted, attributes created, updated and deleted,
/// component balances of priced and unpriced tokens. A two-block branch is orphaned before the
/// halfway point, and nested undo signals orphan another branch after it.
///
/// Sent as partial blocks, block 1 arrives whole, as blocks behind the chain head do, and the
/// undo signals interrupt the last block of each orphaned branch, so one revert drops sealed
/// blocks and pending partials, and one drops pending partials only. One more undo signal names
/// block 3 while it streams: nothing sealed is invalid, so the extractor keeps its partials.
pub(super) fn native(delivery: Delivery) -> RecordedStream {
    use ChangeType::{Creation, Deletion, Update};

    let mut stream = StreamWriter::delivering(delivery);
    // 1
    stream.whole_block(vec![Tx::new()
        .create(native_pool("pool_a", [&WETH, &USDC]))
        .attributes(
            "pool_a",
            &[("reserve0", 100, Creation), ("reserve1", 200, Creation), ("fee", 30, Creation)],
        )
        .balance("pool_a", &WETH, 1_000)
        .balance("pool_a", &USDC, 2_000)]);
    // 2: a pool with a token the database does not know yet.
    stream.block(vec![
        Tx::new()
            .attributes("pool_a", &[("reserve0", 110, Update), ("reserve1", 190, Update)])
            .balance("pool_a", &WETH, 1_100)
            .balance("pool_a", &USDC, 1_900),
        Tx::new()
            .create(native_pool("pool_b", [&DAI, &TKNA]))
            .attributes("pool_b", &[("reserve0", 500, Creation), ("reserve1", 700, Creation)])
            .balance("pool_b", &DAI, 500)
            .balance("pool_b", &TKNA, 700),
    ]);
    // 3
    stream.block_undone_to_itself(vec![
        Tx::new()
            .attributes("pool_b", &[("reserve0", 520, Update)])
            .balance("pool_b", &DAI, 520),
        Tx::new()
            .attributes("pool_a", &[("reserve0", 120, Update)])
            .balance("pool_a", &WETH, 1_200),
    ]);
    // 4: an attribute replaced by another.
    stream.block(vec![Tx::new()
        .attributes("pool_a", &[("fee", 0, Deletion), ("fee_tier", 5, Creation)])
        .balance("pool_a", &USDC, 1_800)]);
    // 5
    stream.block(vec![Tx::new()
        .attributes("pool_a", &[("reserve1", 180, Update)])
        .attributes("pool_b", &[("reserve1", 650, Update)])
        .balance("pool_a", &USDC, 1_700)
        .balance("pool_b", &TKNA, 650)]);
    // 6 and 7, orphaned below: a pool created, another deleted. Deletions stay on orphaned
    // branches because committing one makes the database writer panic: it inserts the deleted
    // component again and finds no new row for it (`add_protocol_components`).
    stream.block(vec![
        Tx::new()
            .create(native_pool("pool_c", [&USDT, &TKNB]))
            .attributes("pool_c", &[("reserve0", 300, Creation)])
            .balance("pool_c", &USDT, 300)
            .balance("pool_c", &TKNB, 400),
        Tx::new()
            .attributes("pool_a", &[("reserve0", 999, Update)])
            .balance("pool_a", &WETH, 999),
        Tx::new()
            .delete(native_pool("pool_b", [&DAI, &TKNA]))
            .attributes("pool_b", &[("paused", 1, Creation)]),
    ]);
    stream.interrupted_block(vec![Tx::new()
        .attributes("pool_c", &[("reserve0", 310, Update)])
        .attributes("pool_a", &[("fee_tier", 7, Update)])
        .balance("pool_c", &USDT, 310)]);
    stream.undo_to(5);
    // 6: a token first seen on the orphaned branch.
    stream.block(vec![
        Tx::new()
            .attributes("pool_a", &[("reserve0", 130, Update)])
            .balance("pool_a", &WETH, 1_300),
        Tx::new()
            .create(native_pool("pool_d", [&USDT, &TKNB]))
            .attributes("pool_d", &[("reserve0", 800, Creation)])
            .balance("pool_d", &USDT, 800)
            .balance("pool_d", &TKNB, 900),
    ]);
    // 7: a token the database learnt in block 2.
    stream.block(vec![
        Tx::new()
            .create(native_pool("pool_e", [&TKNA, &WETH]))
            .attributes("pool_e", &[("reserve0", 50, Creation)])
            .balance("pool_e", &TKNA, 50)
            .balance("pool_e", &WETH, 60),
        Tx::new()
            .attributes("pool_d", &[("reserve0", 810, Update)])
            .balance("pool_d", &USDT, 810),
    ]);
    // 8
    stream.block(vec![Tx::new()
        .attributes("pool_b", &[("reserve0", 540, Update)])
        .attributes("pool_e", &[("reserve0", 55, Update)])
        .balance("pool_b", &DAI, 540)
        .balance("pool_e", &TKNA, 55)]);
    // 9
    stream.block(vec![Tx::new()
        .attributes("pool_a", &[("reserve1", 170, Update)])
        .balance("pool_a", &USDC, 1_600)]);
    // 10
    stream.block(vec![Tx::new()
        .attributes("pool_d", &[("reserve0", 0, Deletion)])
        .balance("pool_d", &USDT, 820)]);
    // 11 and 12, orphaned below by nested undo signals.
    stream.block(vec![
        Tx::new()
            .create(native_pool("pool_f", [&DAI, &USDC]))
            .attributes("pool_f", &[("reserve0", 10, Creation)])
            .balance("pool_f", &DAI, 10)
            .balance("pool_f", &USDC, 20),
        Tx::new().attributes("pool_a", &[("reserve0", 140, Update)]),
    ]);
    stream.interrupted_block(vec![Tx::new()
        .attributes("pool_f", &[("reserve0", 11, Update)])
        .balance("pool_f", &DAI, 11)]);
    stream.undo_to(11);
    stream.block(vec![Tx::new()
        .attributes("pool_b", &[("reserve1", 640, Update)])
        .attributes("pool_f", &[("reserve0", 12, Update)])
        .balance("pool_b", &TKNA, 640)]);
    stream.undo_to(10);
    // 11 to 14
    stream.block(vec![Tx::new()
        .attributes("pool_a", &[("reserve0", 150, Update)])
        .balance("pool_a", &WETH, 1_500)]);
    stream.block(vec![Tx::new()
        .attributes("pool_e", &[("reserve0", 56, Update)])
        .balance("pool_e", &WETH, 61)]);
    stream.block(vec![Tx::new()
        .attributes("pool_b", &[("reserve0", 550, Update)])
        .balance("pool_b", &DAI, 550)]);
    stream.block(vec![Tx::new().balance("pool_d", &USDT, 830)]);
    stream.finish()
}

/// The DCI stream: VM components with contracts and entry points. Entry points are traced,
/// shared by several components, retriggered by a new implementation address, and traced on
/// orphaned branches; the traced contracts' storage changes, including a token's tracked and
/// untracked slots. Branches are orphaned like in the native stream.
pub(super) fn dci() -> RecordedStream {
    use ChangeType::{Creation, Update};

    let oracle_a = EP_A.target();
    let oracle_b = EP_B.target();
    let oracle_c = EP_C.target();
    let oracle_d = EP_D.target();
    let weth = WETH.address();

    let mut stream = StreamWriter::new();
    // 1: a pool whose entry point is traced.
    stream.block(vec![Tx::new()
        .create(vm_pool("vm_pool_a", [&WETH, &USDC], [0xa1; 20]))
        .contract(pool_contract([0xa1; 20], 100))
        .attributes("vm_pool_a", &[("fee", 30, Creation)])
        .balance("vm_pool_a", &WETH, 1_000)
        .balance("vm_pool_a", &USDC, 2_000)
        .entry_point("vm_pool_a", &EP_A)]);
    let delegate_a1 = delegate(&oracle_a, &stream.hash_at(1));
    // 2: the traced contracts change.
    stream.block(vec![Tx::new()
        .contract(pool_contract_update([0xa1; 20], 110))
        .balance("vm_pool_a", &WETH, 1_100)
        .storage(&oracle_a, slot(0), word(1), word(2))
        .storage(&weth, balance_slot(&oracle_a), word(10), word(20))
        .storage(&weth, slot(9), word(1), word(2))
        .native_balance(&delegate_a1, 5_000)]);
    // 3: a second pool shares the traced entry point and brings its own.
    stream.block(vec![Tx::new()
        .create(vm_pool("vm_pool_b", [&DAI, &TKNA], [0xb1; 20]))
        .contract(pool_contract([0xb1; 20], 200))
        .balance("vm_pool_b", &DAI, 500)
        .balance("vm_pool_b", &TKNA, 700)
        .entry_point("vm_pool_b", &EP_A)
        .entry_point("vm_pool_b", &EP_B)]);
    let delegate_b3 = delegate(&oracle_b, &stream.hash_at(3));
    // 4: the first oracle delegates to a new contract, which retriggers its trace.
    stream.block(vec![Tx::new()
        .storage(
            &oracle_a,
            implementation_slot(),
            address_word(&delegate_a1),
            address_word(&Address::from([0xee; 20])),
        )
        .balance("vm_pool_a", &USDC, 1_900)]);
    let delegate_a4 = delegate(&oracle_a, &stream.hash_at(4));
    // 5: the contracts of both traces stay tracked, and the second oracle delegates to a new
    // contract. Its retrace reads the WETH slot block 3 tracked while block 4's newer cache layer
    // holds another WETH slot, so only the older layer records that the slot is tracked.
    let implementation_b5 = Address::from([0xe5; 20]);
    stream.block(vec![Tx::new()
        .storage(&delegate_a1, slot(0), word(1), word(3))
        .storage(&delegate_a4, slot(0), word(1), word(4))
        .storage(&oracle_b, slot(0), word(1), word(5))
        .storage(
            &oracle_b,
            implementation_slot(),
            address_word(&delegate_b3),
            address_word(&implementation_b5),
        )
        .attributes("vm_pool_b", &[("fee", 5, Creation)])
        .balance("vm_pool_b", &DAI, 520)]);
    // 6 and 7, orphaned below: an entry point traced, then a trace retriggered.
    stream.block(vec![
        Tx::new()
            .create(vm_pool("vm_pool_c", [&USDT, &TKNB], [0xc1; 20]))
            .contract(pool_contract([0xc1; 20], 300))
            .balance("vm_pool_c", &USDT, 300)
            .balance("vm_pool_c", &TKNB, 400)
            .entry_point("vm_pool_c", &EP_C),
        Tx::new().storage(&oracle_a, slot(0), word(2), word(9)),
    ]);
    stream.block(vec![Tx::new()
        .storage(&oracle_c, slot(0), word(1), word(2))
        .storage(
            &oracle_b,
            implementation_slot(),
            address_word(&implementation_b5),
            address_word(&Address::from([0xef; 20])),
        )
        .balance("vm_pool_c", &USDT, 310)]);
    stream.undo_to(5);
    // 6: the entry point traced on the orphaned branch, traced again.
    stream.block(vec![Tx::new()
        .create(vm_pool("vm_pool_d", [&USDT, &TKNB], [0xd1; 20]))
        .contract(pool_contract([0xd1; 20], 400))
        .balance("vm_pool_d", &USDT, 800)
        .balance("vm_pool_d", &TKNB, 900)
        .entry_point("vm_pool_d", &EP_C)]);
    // 7: a pool shares an entry point traced in a committed block.
    stream.block(vec![
        Tx::new()
            .create(vm_pool("vm_pool_e", [&TKNA, &WETH], [0xe1; 20]))
            .contract(pool_contract([0xe1; 20], 60))
            .balance("vm_pool_e", &TKNA, 50)
            .balance("vm_pool_e", &WETH, 60)
            .entry_point("vm_pool_e", &EP_A),
        Tx::new().storage(&oracle_c, slot(0), word(1), word(3)),
    ]);
    // 8: the second oracle delegates to a new contract again.
    stream.block(vec![Tx::new()
        .storage(
            &oracle_b,
            implementation_slot(),
            address_word(&implementation_b5),
            address_word(&Address::from([0xf0; 20])),
        )
        .balance("vm_pool_b", &DAI, 540)]);
    let delegate_b8 = delegate(&oracle_b, &stream.hash_at(8));
    // 9
    stream.block(vec![Tx::new()
        .contract(pool_contract_update([0xa1; 20], 120))
        .storage(&delegate_b8, slot(0), word(1), word(6))
        .storage(&oracle_a, slot(0), word(2), word(7))]);
    // 10
    stream.block(vec![Tx::new()
        .attributes("vm_pool_a", &[("fee", 25, Update)])
        .balance("vm_pool_d", &USDT, 820)
        .storage(&oracle_c, slot(0), word(3), word(4))]);
    // 11 and 12, orphaned below by nested undo signals: an entry point traced, then retriggered.
    stream.block(vec![Tx::new()
        .entry_point("vm_pool_a", &EP_D)
        .storage(&oracle_a, slot(0), word(7), word(8))]);
    stream.block(vec![Tx::new().storage(&oracle_d, slot(0), word(1), word(2))]);
    stream.undo_to(11);
    let delegate_d11 = delegate(&oracle_d, &stream.hash_at(11));
    stream.block(vec![Tx::new().storage(
        &oracle_d,
        implementation_slot(),
        address_word(&delegate_d11),
        address_word(&Address::from([0xf1; 20])),
    )]);
    stream.undo_to(10);
    // 11 to 14: the entry point of the orphaned branch, traced on the canonical one.
    stream.block(vec![Tx::new()
        .storage(&oracle_a, slot(0), word(7), word(10))
        .balance("vm_pool_a", &WETH, 1_200)]);
    stream.block(vec![Tx::new().entry_point("vm_pool_b", &EP_D)]);
    stream.block(vec![Tx::new().storage(&oracle_d, slot(0), word(1), word(3))]);
    stream.block(vec![Tx::new()
        .attributes("vm_pool_b", &[("fee", 6, Update)])
        .balance("vm_pool_e", &WETH, 70)]);
    stream.finish()
}

/// How a block is sent in a stream of partial blocks. In a stream of full blocks, every block is
/// one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One partial per transaction, the last one flagged.
    Partials,
    /// One partial per transaction, with an undo signal naming the block itself after the first.
    PartialsUndoneToItself,
    /// One partial per transaction, none flagged: an undo signal orphans the block next.
    Interrupted,
    /// One message, as substreams sends blocks behind the chain head.
    Whole,
}

/// Whether every message of a block was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockEnd {
    Sealed,
    /// The next message is an undo signal that orphans the block before its last partial. Only
    /// blocks sent as partial blocks can end this way; full blocks are always sealed.
    Interrupted,
}

/// Writes a stream block by block. Blocks written after an undo signal are on a new branch:
/// their hashes differ from the blocks they replace.
pub(super) struct StreamWriter {
    delivery: Delivery,
    /// Timestamp of block 0. The test database only has partitions for recent days.
    base_ts: NaiveDateTime,
    /// The current chain, oldest first.
    chain: Vec<ChainBlock>,
    /// Number of undo signals that orphaned blocks so far.
    branch: u64,
    final_height: u64,
    messages: Vec<Recorded>,
}

struct ChainBlock {
    number: u64,
    hash: BlockHash,
    /// Cursor of the block's last message.
    cursor: String,
    end: BlockEnd,
}

impl StreamWriter {
    /// A writer that sends every block as one message.
    pub(super) fn new() -> Self {
        Self::delivering(Delivery::FullBlocks)
    }

    /// A writer that sends blocks the way `delivery` says.
    pub(super) fn delivering(delivery: Delivery) -> Self {
        Self {
            delivery,
            base_ts: yesterday_midnight(),
            chain: Vec::new(),
            branch: 0,
            final_height: 0,
            messages: Vec::new(),
        }
    }

    /// Appends the next block of the current chain, with `txs` in order. The first block is
    /// final: an extractor with the dynamic contract indexer needs its first block to be.
    pub(super) fn block(&mut self, txs: Vec<Tx>) {
        self.append(txs, Shape::Partials);
    }

    /// Appends the next block like [`Self::block`], sent as one message even in a stream of
    /// partial blocks, as substreams sends blocks behind the chain head.
    pub(super) fn whole_block(&mut self, txs: Vec<Tx>) {
        self.append(txs, Shape::Whole);
    }

    /// Appends the next block like [`Self::block`]. Sent as partial blocks, an undo signal naming
    /// the block itself follows its first partial, and its remaining partials follow the undo:
    /// substreams rewinds to the block it is streaming, which the extractor has not sealed.
    pub(super) fn block_undone_to_itself(&mut self, txs: Vec<Tx>) {
        self.append(txs, Shape::PartialsUndoneToItself);
    }

    /// Appends the next block like [`Self::block`], for an undo signal to orphan next. Sent as
    /// partial blocks, the block's last partial never arrives.
    pub(super) fn interrupted_block(&mut self, txs: Vec<Tx>) {
        self.append(txs, Shape::Interrupted);
    }

    /// # Panics
    /// If the current chain ends in an interrupted block, or a block sent as partial blocks has
    /// too few transactions for its shape.
    fn append(&mut self, txs: Vec<Tx>, shape: Shape) {
        if let Some(tip) = self.chain.last() {
            assert_eq!(
                tip.end,
                BlockEnd::Sealed,
                "block {} was interrupted: only an undo signal can follow it",
                tip.number
            );
        }
        let number = self
            .chain
            .last()
            .map_or(1, |tip| tip.number + 1);
        let parent_hash = self
            .chain
            .last()
            .map_or_else(|| Bytes::zero(32), |tip| tip.hash.clone());
        let hash =
            Bytes::from([self.branch.to_be_bytes(), [0; 8], [0; 8], number.to_be_bytes()].concat());
        self.final_height = if self.chain.is_empty() {
            number
        } else {
            self.final_height
                .max(number.saturating_sub(FINALITY_LAG))
        };

        let mut parts = Vec::with_capacity(txs.len());
        for (index, tx) in txs.into_iter().enumerate() {
            let index = index as u64;
            let transaction = pb::Transaction {
                hash: keccak256([hash.as_ref(), &index.to_be_bytes()].concat()).to_vec(),
                from: SENDER.to_vec(),
                to: ROUTER.to_vec(),
                index,
            };
            let mut part = BlockPart::default();
            if !tx.storage.is_empty() {
                part.storage_changes
                    .push(pb::TransactionStorageChanges {
                        tx: Some(transaction.clone()),
                        storage_changes: tx.storage,
                    });
            }
            if tx.changes != pb::TransactionChanges::default() {
                part.changes
                    .push(pb::TransactionChanges { tx: Some(transaction), ..tx.changes });
            }
            parts.push(part);
        }

        let block = pb::Block {
            hash: hash.to_vec(),
            parent_hash: parent_hash.to_vec(),
            number,
            ts: self.base_ts.and_utc().timestamp() as u64 + number * BLOCK_TIME,
        };
        let shape = match self.delivery {
            Delivery::FullBlocks => Shape::Whole,
            Delivery::PartialBlocks => shape,
        };
        let cursor = match shape {
            Shape::Whole => {
                let cursor = format!("cursor@{number}:{}", self.branch);
                let whole = parts
                    .into_iter()
                    .fold(BlockPart::default(), BlockPart::extend);
                let data = self.block_scoped_data(block, whole, &cursor);
                self.messages
                    .push(Recorded::Block(Box::new(data)));
                cursor
            }
            Shape::Partials | Shape::PartialsUndoneToItself | Shape::Interrupted => {
                self.send_partials(block, parts, shape)
            }
        };
        let end = match shape {
            Shape::Interrupted => BlockEnd::Interrupted,
            Shape::Partials | Shape::PartialsUndoneToItself | Shape::Whole => BlockEnd::Sealed,
        };
        self.chain
            .push(ChainBlock { number, hash, cursor, end });
    }

    /// Sends `block` as one partial per part, shaped by `shape`, and returns the last partial's
    /// cursor. Every partial but a sealed block's last one carries a temporary hash that reads
    /// like the block's, with ones in the second word and the partial's index in the third.
    ///
    /// # Panics
    /// If there are too few parts for `shape`, or `shape` is [`Shape::Whole`].
    fn send_partials(&mut self, block: pb::Block, parts: Vec<BlockPart>, shape: Shape) -> String {
        let number = block.number;
        let (needed, sealed) = match shape {
            Shape::Partials => (1, true),
            Shape::PartialsUndoneToItself => (2, true),
            Shape::Interrupted => (1, false),
            Shape::Whole => panic!("block {number} is sent whole, not as partials"),
        };
        assert!(parts.len() >= needed, "block {number} needs {needed} transactions as {shape:?}");
        let last = parts.len() - 1;
        let mut cursor = String::new();
        for (index, part) in parts.into_iter().enumerate() {
            let flagged = sealed && index == last;
            let hash = if flagged {
                block.hash.clone()
            } else {
                let words = [self.branch, u64::MAX, index as u64, number].map(u64::to_be_bytes);
                words.concat()
            };
            cursor = format!("cursor@{number}:{}:{index}", self.branch);
            let mut data =
                self.block_scoped_data(pb::Block { hash, ..block.clone() }, part, &cursor);
            data.is_partial = true;
            data.partial_index = Some(index as u32);
            data.is_last_partial = Some(flagged);
            self.messages
                .push(Recorded::Block(Box::new(data)));
            if index == 0 && shape == Shape::PartialsUndoneToItself {
                let id = Bytes::from(block.hash.clone()).to_string();
                self.messages
                    .push(Recorded::Undo(BlockUndoSignal {
                        last_valid_block: Some(BlockRef { id, number }),
                        last_valid_cursor: cursor.clone(),
                    }));
            }
        }
        cursor
    }

    /// One message of the stream: `part` of `block`, with the current final height.
    fn block_scoped_data(
        &self,
        block: pb::Block,
        part: BlockPart,
        cursor: &str,
    ) -> BlockScopedData {
        let clock = Clock {
            id: Bytes::from(block.hash.clone()).to_string(),
            number: block.number,
            timestamp: Some(prost_types::Timestamp { seconds: block.ts as i64, nanos: 0 }),
        };
        let mut data = pb_block_scoped_data(
            pb::BlockChanges {
                block: Some(block),
                changes: part.changes,
                storage_changes: part.storage_changes,
            },
            Some(cursor),
            Some(self.final_height),
        );
        data.clock = Some(clock);
        data
    }

    /// Returns to block `number` of the current chain, orphaning the blocks above it.
    ///
    /// # Panics
    /// If `number` is below the finalized height, which substreams never reverts, or is not a
    /// sealed block of the current chain.
    pub(super) fn undo_to(&mut self, number: u64) {
        assert!(
            number >= self.final_height,
            "cannot undo to block {number} below the finalized height {}",
            self.final_height
        );
        while self
            .chain
            .last()
            .is_some_and(|tip| tip.number > number)
        {
            self.chain.pop();
        }
        let target = self
            .chain
            .last()
            .filter(|tip| tip.number == number && tip.end == BlockEnd::Sealed)
            .unwrap_or_else(|| panic!("block {number} is not a sealed block of the current chain"));
        self.messages
            .push(Recorded::Undo(BlockUndoSignal {
                last_valid_block: Some(BlockRef { id: target.hash.to_string(), number }),
                last_valid_cursor: target.cursor.clone(),
            }));
        self.branch += 1;
    }

    /// The hash of block `number` of the current chain.
    ///
    /// # Panics
    /// If `number` is not on the current chain.
    fn hash_at(&self, number: u64) -> BlockHash {
        self.chain
            .iter()
            .find(|block| block.number == number)
            .map(|block| block.hash.clone())
            .unwrap_or_else(|| panic!("block {number} is not on the current chain"))
    }

    pub(super) fn finish(self) -> RecordedStream {
        RecordedStream { delivery: self.delivery, base_ts: self.base_ts, messages: self.messages }
    }
}

/// The changes of some of a block's transactions, in order.
#[derive(Default)]
struct BlockPart {
    changes: Vec<pb::TransactionChanges>,
    storage_changes: Vec<pb::TransactionStorageChanges>,
}

impl BlockPart {
    fn extend(mut self, later: BlockPart) -> BlockPart {
        self.changes.extend(later.changes);
        self.storage_changes
            .extend(later.storage_changes);
        self
    }
}

/// One transaction of a recorded block: the changes the substreams module reports for it and
/// its raw storage changes, which only the dynamic contract indexer reads.
#[derive(Default)]
pub(super) struct Tx {
    changes: pb::TransactionChanges,
    storage: Vec<pb::StorageChanges>,
}

impl Tx {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Creates `component`.
    fn create(mut self, component: pb::ProtocolComponent) -> Self {
        self.changes
            .component_changes
            .push(component);
        self
    }

    /// Deletes `component`.
    fn delete(mut self, component: pb::ProtocolComponent) -> Self {
        self.changes
            .component_changes
            .push(pb::ProtocolComponent { change: ChangeType::Deletion.into(), ..component });
        self
    }

    /// Changes attributes of `component_id`: each with its name, value and change type. Calls for
    /// the same component share one entry, since the conversion keeps one per component.
    fn attributes(mut self, component_id: &str, attributes: &[(&str, u64, ChangeType)]) -> Self {
        let attributes = attributes
            .iter()
            .map(|(name, value, change)| pb::Attribute {
                name: name.to_string(),
                value: word(*value).to_vec(),
                change: (*change).into(),
            });
        match self
            .changes
            .entity_changes
            .iter_mut()
            .find(|entity| entity.component_id == component_id)
        {
            Some(entity) => entity.attributes.extend(attributes),
            None => self
                .changes
                .entity_changes
                .push(pb::EntityChanges {
                    component_id: component_id.to_string(),
                    attributes: attributes.collect(),
                }),
        }
        self
    }

    /// Sets the balance of `token` in `component_id` to `amount`.
    fn balance(mut self, component_id: &str, token: &FixtureToken, amount: u64) -> Self {
        self.changes
            .balance_changes
            .push(pb::BalanceChange {
                token: token.address().to_vec(),
                balance: Bytes::from(amount).to_vec(),
                component_id: component_id.as_bytes().to_vec(),
            });
        self
    }

    /// Creates or updates a contract the substreams module tracks itself.
    fn contract(mut self, change: pb::ContractChange) -> Self {
        self.changes
            .contract_changes
            .push(change);
        self
    }

    /// Adds `entry_point` to `component_id`, with its tracing params.
    fn entry_point(mut self, component_id: &str, entry_point: &FixtureEntryPoint) -> Self {
        self.changes
            .entrypoints
            .push(pb::EntryPoint {
                id: entry_point.id.to_string(),
                target: entry_point.target.to_vec(),
                signature: "getRate()".to_string(),
                component_id: component_id.to_string(),
            });
        self.changes
            .entrypoint_params
            .push(pb::EntryPointParams {
                entrypoint_id: entry_point.id.to_string(),
                component_id: Some(component_id.to_string()),
                trace_data: Some(TraceData::Rpc(pb::RpcTraceData {
                    caller: None,
                    calldata: vec![0x67, 0x9a, 0xef, 0xce, entry_point.calldata],
                })),
            });
        self
    }

    /// Changes `slot` of `address` from `previous` to `value` in the raw storage changes.
    fn storage(mut self, address: &Address, slot: StoreKey, previous: Bytes, value: Bytes) -> Self {
        let slot = pb::ContractSlot {
            slot: slot.to_vec(),
            value: value.to_vec(),
            previous_value: previous.to_vec(),
        };
        self.storage_of(address)
            .slots
            .push(slot);
        self
    }

    /// Changes the native balance of `address` to `balance` in the raw storage changes.
    fn native_balance(mut self, address: &Address, balance: u64) -> Self {
        self.storage_of(address).native_balance = Some(word(balance).to_vec());
        self
    }

    /// The raw storage changes of `address`; the conversion keeps one entry per address.
    fn storage_of(&mut self, address: &Address) -> &mut pb::StorageChanges {
        let position = match self
            .storage
            .iter()
            .position(|changes| changes.address == address.as_ref())
        {
            Some(position) => position,
            None => {
                self.storage.push(pb::StorageChanges {
                    address: address.to_vec(),
                    slots: Vec::new(),
                    native_balance: None,
                });
                self.storage.len() - 1
            }
        };
        &mut self.storage[position]
    }
}

fn native_pool(id: &str, tokens: [&FixtureToken; 2]) -> pb::ProtocolComponent {
    component(id, NATIVE_POOL, tokens, Vec::new())
}

fn vm_pool(id: &str, tokens: [&FixtureToken; 2], contract: [u8; 20]) -> pb::ProtocolComponent {
    component(id, VM_POOL, tokens, vec![contract.to_vec()])
}

fn component(
    id: &str,
    protocol_type: &str,
    tokens: [&FixtureToken; 2],
    contracts: Vec<Vec<u8>>,
) -> pb::ProtocolComponent {
    pb::ProtocolComponent {
        id: id.to_string(),
        tokens: tokens
            .iter()
            .map(|token| token.address().to_vec())
            .collect(),
        contracts,
        static_att: vec![pb::Attribute {
            name: "pool_id".to_string(),
            value: id.as_bytes().to_vec(),
            change: ChangeType::Creation.into(),
        }],
        change: ChangeType::Creation.into(),
        protocol_type: Some(pb::ProtocolType {
            name: protocol_type.to_string(),
            ..Default::default()
        }),
    }
}

/// Creates the pool contract at `address` with code, a native balance, slot 0 and a WETH
/// balance of `weth`.
fn pool_contract(address: [u8; 20], weth: u64) -> pb::ContractChange {
    pb::ContractChange {
        address: address.to_vec(),
        balance: word(1_000).to_vec(),
        code: vec![0x60, 0x80, 0x60, 0x40],
        slots: vec![pb::ContractSlot {
            slot: slot(0).to_vec(),
            value: word(weth).to_vec(),
            previous_value: Vec::new(),
        }],
        change: ChangeType::Creation.into(),
        token_balances: vec![pb::AccountBalanceChange {
            token: WETH.address().to_vec(),
            balance: Bytes::from(weth).to_vec(),
        }],
    }
}

/// Updates slot 0 and the WETH balance of the pool contract at `address` to `weth`.
fn pool_contract_update(address: [u8; 20], weth: u64) -> pb::ContractChange {
    pb::ContractChange {
        change: ChangeType::Update.into(),
        balance: Vec::new(),
        code: Vec::new(),
        ..pool_contract(address, weth)
    }
}

/// `value` as a 32-byte word.
fn word(value: u64) -> Bytes {
    Bytes::from(value).lpad(32, 0)
}
