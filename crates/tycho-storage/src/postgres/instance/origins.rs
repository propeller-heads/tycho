//! Block and order of each change in a write batch.
//!
//! Instance rows carry the version they start (`valid_from`, the block timestamp) and the index of
//! the transaction that made them (`tx_index`), instead of a reference into a transaction table.
//! Every change in a batch names its transaction by hash, and the batch carries those
//! transactions and their blocks, so both values are known before anything is written.
use std::collections::HashMap;

use chrono::NaiveDateTime;
use tycho_common::{
    models::{
        blockchain::{Block, Transaction},
        BlockHash, TxHash,
    },
    storage::StorageError,
};

use crate::postgres::{cache::WriteOp, convert_int};

/// Position of a change on the chain.
///
/// Orders like the versions it produces: by block, then by transaction inside the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangeOrigin {
    /// Timestamp of the block of the change, which is the `valid_from` of the row it writes.
    pub valid_from: NaiveDateTime,
    /// Index of the transaction in its block.
    pub tx_index: i32,
}

/// Number and timestamp of a block, as instance rows store them inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockOrigin {
    pub number: i64,
    pub ts: NaiveDateTime,
}

/// The blocks and transactions of one write batch, by hash.
#[derive(Debug, Default)]
pub struct BatchOrigins {
    blocks: HashMap<BlockHash, BlockOrigin>,
    txs: HashMap<TxHash, ChangeOrigin>,
}

impl BatchOrigins {
    /// Collects the blocks of the `UpsertBlock` ops and the transactions of the `UpsertTx` ops.
    ///
    /// Fails with [`StorageError::NoRelatedEntity`] when a transaction's block is not in the
    /// batch, and with [`StorageError::DecodeError`] when a block number or a transaction index
    /// does not fit its column.
    pub fn from_ops(ops: &[WriteOp]) -> Result<Self, StorageError> {
        let mut origins = Self::default();
        for op in ops {
            if let WriteOp::UpsertBlock(blocks) = op {
                for block in blocks {
                    origins.add_block(block)?;
                }
            }
        }
        for op in ops {
            if let WriteOp::UpsertTx(txs) = op {
                for tx in txs {
                    origins.add_tx(tx)?;
                }
            }
        }
        Ok(origins)
    }

    fn add_block(&mut self, block: &Block) -> Result<(), StorageError> {
        let origin =
            BlockOrigin { number: convert_int(block.number, "block number")?, ts: block.ts };
        self.blocks
            .insert(block.hash.clone(), origin);
        Ok(())
    }

    fn add_tx(&mut self, tx: &Transaction) -> Result<(), StorageError> {
        let Some(block) = self.blocks.get(&tx.block_hash) else {
            return Err(StorageError::NoRelatedEntity(
                "Block".to_owned(),
                "Transaction".to_owned(),
                tx.hash.to_string(),
            ));
        };
        let origin = ChangeOrigin {
            valid_from: block.ts,
            tx_index: convert_int(tx.index, "transaction index")?,
        };
        self.txs.insert(tx.hash.clone(), origin);
        Ok(())
    }

    /// Returns the origin of a change made by transaction `tx`.
    ///
    /// Fails with [`StorageError::NotFound`] when the batch doesn't carry the transaction.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the instance writer writes no versioned rows yet")
    )]
    pub fn change(&self, tx: &TxHash) -> Result<ChangeOrigin, StorageError> {
        self.txs
            .get(tx)
            .copied()
            .ok_or_else(|| StorageError::NotFound("Transaction".to_owned(), tx.to_string()))
    }

    /// Returns the block with hash `hash`, or `None` when it is not in the batch.
    pub fn block(&self, hash: &BlockHash) -> Option<BlockOrigin> {
        self.blocks.get(hash).copied()
    }
}
