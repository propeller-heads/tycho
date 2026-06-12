use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use deepsize::DeepSizeOf;
use mockall::automock;
use prost::DecodeError;
use thiserror::Error;
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, BlockScoped},
        contract::AccountBalance,
        protocol::ComponentBalance,
        Address, BlockHash, ExtractorIdentity, MergeError,
    },
    storage::StorageError,
    Bytes,
};

use crate::{
    extractor::{
        dynamic_contract_indexer::cache::DCICacheError,
        models::BlockChanges,
        reorg_buffer::{
            AccountStateIdType, AccountStateKeyType, AccountStateValueType, ProtocolStateIdType,
            ProtocolStateKeyType, ProtocolStateValueType, StateUpdateBufferEntry,
        },
    },
    pb::sf::substreams::{
        rpc::v2::{BlockScopedData, BlockUndoSignal, ModulesProgress},
        v1::Clock,
    },
};

pub mod chain_state;
mod dynamic_contract_indexer;
pub mod models;
pub mod post_processors;
pub mod protobuf_deserialisation;
pub mod protocol_cache;
pub mod protocol_extractor;
pub mod reorg_buffer;
pub mod runner;
pub mod token_analysis_cron;
mod u256_num;

#[derive(Error, Debug, PartialEq)]
pub enum ExtractionError {
    #[error("Extractor setup failed: {0}")]
    Setup(String),
    #[error("Failed to decode: {0}")]
    DecodeError(String),
    #[error("Protobuf error: {0}")]
    ProtobufError(#[from] DecodeError),
    #[error("Can't decode an empty message")]
    Empty,
    #[error("Unexpected extraction error: {0}")]
    Unknown(String),
    #[error("Storage failure: {0}")]
    Storage(#[from] StorageError),
    #[error("Stream errored: {0}")]
    SubstreamsError(String),
    #[error("Service error: {0}")]
    ServiceError(String),
    #[error("Merge error: {0}")]
    MergeError(#[from] MergeError),
    #[error("Reorg buffer error: {0}")]
    ReorgBufferError(String),
    #[error("Partial block buffer error: {0}")]
    PartialBlockBufferError(String),
    #[error("Tracing error: {0}")]
    TracingError(String),
    #[error("Account extraction error: {0}")]
    AccountExtractionError(String),
    #[error("DCI cache error: {0}")]
    DCICacheError(#[from] DCICacheError),
}

#[derive(Error, Debug)]
pub enum RPCError {
    #[error("RPC setup error: {0}")]
    SetupError(String),
    #[error("RPC error: {0}")]
    RequestError(String),
}

pub type ExtractorMsg = Arc<BlockAggregatedChanges>;

#[automock]
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Returns the unique identity of this extractor.
    fn get_id(&self) -> ExtractorIdentity;

    /// Ensures all protocol types this extractor needs are registered in
    /// storage. Safe to call multiple times.
    ///
    /// # Errors
    /// Returns an [`ExtractionError`] if the protocol types could not be persisted.
    async fn ensure_protocol_types(&self) -> Result<(), ExtractionError>;

    /// Returns the current stream cursor, or an empty string if no block has
    /// been processed yet. At startup this reflects the last persisted cursor;
    /// during runtime it advances with every incoming block.
    async fn get_cursor(&self) -> String;

    /// Returns the last block processed by this extractor, or `None` if no
    /// block has been processed yet. At startup this reflects the last
    /// persisted block; during runtime it advances with every incoming block.
    async fn get_last_processed_block(&self) -> Option<Block>;

    /// Processes a single block-scoped data message from the source stream.
    async fn handle_tick_scoped_data(
        &self,
        inp: BlockScopedData,
    ) -> Result<Option<ExtractorMsg>, ExtractionError>;

    /// Drains the partial block buffer and processes the accumulated block as a full block.
    /// The runner calls this when it has sent the last partial for a block.
    async fn collect_and_process_full_block(
        &self,
        cursor: String,
        final_block_height: u64,
        clock: Option<Clock>,
    ) -> Result<Option<ExtractorMsg>, ExtractionError>;

    /// Processes a chain reorg signal.
    async fn handle_revert(
        &self,
        inp: BlockUndoSignal,
    ) -> Result<Option<ExtractorMsg>, ExtractionError>;

    /// Processes a progress report from the source stream.
    async fn handle_progress(&self, inp: ModulesProgress) -> Result<(), ExtractionError>;
}

#[automock]
#[async_trait]
pub trait ExtractorExtension: Send + Sync {
    /// Process a block update message and update it in-place.
    async fn process_block_update(
        &mut self,
        block_changes: &mut BlockChanges,
    ) -> Result<(), ExtractionError>;

    /// Process a revert
    async fn process_revert(&mut self, target_block: &BlockHash) -> Result<(), ExtractionError>;

    /// Returns the approximate size of the internal cache used by this extension, in bytes.
    fn cache_size(&self) -> usize;

    /// Emits granular cache metrics (per-sub-cache size, key counts, top tracked contracts).
    fn emit_cache_metrics(&self, _chain: &str, _extractor: &str) {}
}

/// Wrapper to carry a cursor along with another struct.
#[derive(Debug, DeepSizeOf)]
pub(crate) struct BlockUpdateWithCursor<B: std::fmt::Debug> {
    block_update: B,
    cursor: String,
}

impl<B: std::fmt::Debug + DeepSizeOf> BlockUpdateWithCursor<B> {
    pub(crate) fn new(block_update: B, cursor: String) -> Self {
        Self { block_update, cursor }
    }

    pub(crate) fn cursor(&self) -> &String {
        &self.cursor
    }

    pub(crate) fn block_update(&self) -> &B {
        &self.block_update
    }
}

impl<B> BlockScoped for BlockUpdateWithCursor<B>
where
    B: BlockScoped + std::fmt::Debug,
{
    fn block(&self) -> Block {
        self.block_update.block()
    }
}

impl<B> StateUpdateBufferEntry for BlockUpdateWithCursor<B>
where
    B: StateUpdateBufferEntry,
{
    fn get_filtered_component_balance_update(
        &self,
        keys: Vec<(&String, &Bytes)>,
    ) -> HashMap<(String, Bytes), ComponentBalance> {
        self.block_update
            .get_filtered_component_balance_update(keys)
    }

    fn get_filtered_account_balance_update(
        &self,
        keys: Vec<(&Address, &Address)>,
    ) -> HashMap<(Address, Address), AccountBalance> {
        self.block_update
            .get_filtered_account_balance_update(keys)
    }

    fn get_filtered_protocol_state_update(
        &self,
        keys: Vec<(&ProtocolStateIdType, &ProtocolStateKeyType)>,
    ) -> HashMap<(ProtocolStateIdType, ProtocolStateKeyType), ProtocolStateValueType> {
        self.block_update
            .get_filtered_protocol_state_update(keys)
    }

    fn get_filtered_account_state_update(
        &self,
        keys: Vec<(&AccountStateIdType, &AccountStateKeyType)>,
    ) -> HashMap<(AccountStateIdType, AccountStateKeyType), AccountStateValueType> {
        self.block_update
            .get_filtered_account_state_update(keys)
    }
}
