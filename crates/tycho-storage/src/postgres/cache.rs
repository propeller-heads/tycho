use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncConnection,
    AsyncPgConnection,
};
use lru::LruCache;
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
};
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};
use tycho_common::{
    models::{
        self,
        blockchain::{
            Block, EntryPoint, EntryPointWithTracingParams, TracedEntryPoint, TracingParams,
            TracingResult, Transaction,
        },
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{
            ComponentBalance, ProtocolComponent, ProtocolComponentState,
            ProtocolComponentStateDelta, QualityRange,
        },
        token::Token,
        Address, Chain, ComponentId, ContractId, EntryPointId, ExtractionState, PaginationParams,
        ProtocolType, TxHash,
    },
    storage::{
        BlockIdentifier, BlockOrTimestamp, ChainGateway, ContractStateGateway, EntryPointFilter,
        EntryPointGateway, ExtractionStateGateway, Gateway, ProtocolGateway, StorageError, Version,
        WithTotal,
    },
    Bytes,
};

use super::{PostgresError, PostgresGateway};

/// Represents different types of database write operations.
#[allow(clippy::large_enum_variant)]
#[derive(PartialEq, Clone, Debug)]
pub(crate) enum WriteOp {
    // Simply merge
    UpsertBlock(Vec<models::blockchain::Block>),
    // Simply merge
    UpsertTx(Vec<models::blockchain::Transaction>),
    // Simply keep last
    SaveExtractionState(ExtractionState),
    // Support saving a batch
    InsertContract(Vec<models::contract::Account>),
    // Simply merge
    UpdateContracts(Vec<(TxHash, models::contract::AccountDelta)>),
    // Simply merge
    InsertAccountBalances(Vec<models::contract::AccountBalance>),
    // Simply merge
    InsertProtocolComponents(Vec<models::protocol::ProtocolComponent>),
    // Simply merge
    InsertTokens(Vec<models::token::Token>),
    // Currently unused but supported, please see `CacheGateway.update_tokens` docs.
    #[allow(dead_code)]
    UpdateTokens(Vec<models::token::Token>),
    // Simply merge
    InsertComponentBalances(Vec<models::protocol::ComponentBalance>),
    // Simply merge
    UpsertProtocolState(Vec<(TxHash, models::protocol::ProtocolComponentStateDelta)>),
    // Simply merge
    InsertEntryPoints(HashMap<models::ComponentId, HashSet<models::blockchain::EntryPoint>>),
    // Simply merge
    InsertEntryPointTracingParams(
        HashMap<models::EntryPointId, HashSet<(TracingParams, ComponentId)>>,
    ),
    // Simply merge
    UpsertTracedEntryPoints(Vec<models::blockchain::TracedEntryPoint>),
}

impl WriteOp {
    fn variant_name(&self) -> &'static str {
        match self {
            WriteOp::UpsertBlock(_) => "UpsertBlock",
            WriteOp::UpsertTx(_) => "UpsertTx",
            WriteOp::SaveExtractionState(_) => "SaveExtractionState",
            WriteOp::InsertContract(_) => "InsertContract",
            WriteOp::UpdateContracts(_) => "UpdateContracts",
            WriteOp::InsertAccountBalances(_) => "InsertAccountBalances",
            WriteOp::InsertProtocolComponents(_) => "InsertProtocolComponents",
            WriteOp::InsertTokens(_) => "InsertTokens",
            WriteOp::UpdateTokens(_) => "UpdateTokens",
            WriteOp::InsertComponentBalances(_) => "InsertComponentBalances",
            WriteOp::UpsertProtocolState(_) => "UpsertProtocolState",
            WriteOp::InsertEntryPoints(_) => "InsertEntryPoints",
            WriteOp::InsertEntryPointTracingParams(_) => "InsertEntryPointTracingParams",
            WriteOp::UpsertTracedEntryPoints(_) => "UpsertTracedEntryPoints",
        }
    }

    fn order_key(&self) -> usize {
        match self {
            WriteOp::UpsertBlock(_) => 0,
            WriteOp::UpsertTx(_) => 1,
            WriteOp::InsertContract(_) => 2,
            WriteOp::UpdateContracts(_) => 3,
            WriteOp::InsertTokens(_) => 4,
            WriteOp::UpdateTokens(_) => 5,
            WriteOp::InsertAccountBalances(_) => 6,
            WriteOp::InsertProtocolComponents(_) => 7,
            WriteOp::InsertComponentBalances(_) => 8,
            WriteOp::UpsertProtocolState(_) => 9,
            WriteOp::InsertEntryPoints(_) => 10,
            WriteOp::InsertEntryPointTracingParams(_) => 11,
            WriteOp::UpsertTracedEntryPoints(_) => 12,
            WriteOp::SaveExtractionState(_) => 13,
        }
    }
}

#[derive(Debug)]
struct BlockRange {
    start: models::blockchain::Block,
    end: models::blockchain::Block,
}

impl BlockRange {
    fn new(start: &models::blockchain::Block, end: &models::blockchain::Block) -> Self {
        Self { start: start.clone(), end: end.clone() }
    }
}

impl std::fmt::Display for BlockRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}, {}] - [{:#x}, {:#x}]",
            self.start.number, self.end.number, self.start.hash, self.end.hash
        )
    }
}

/// Represents a transaction in the database, including the block information,
/// a list of operations to be performed, and a channel to send the result.
pub struct DBTransaction {
    block_range: BlockRange,
    size: usize,
    operations: Vec<WriteOp>,
    tx: oneshot::Sender<Result<(), StorageError>>,
    /// Purely used to add an attribute to the span when the transaction is commited
    owner: Option<String>,
    /// Span of the caller that created this transaction, used to link the db_write span
    /// back to the originating trace across the channel boundary.
    caller_span: tracing::Span,
}

impl DBTransaction {
    /// Batch changes of the same kind.
    ///
    /// The final insertion order is determined via `WriteOp::order_key` and is fixed for all
    /// transaction.
    ///
    /// PERF: Use an array instead of a vec since the order is static.
    fn add_operation(&mut self, op: WriteOp) -> Result<(), StorageError> {
        for existing_op in self.operations.iter_mut() {
            match (existing_op, &op) {
                (WriteOp::UpsertBlock(l), WriteOp::UpsertBlock(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::UpsertTx(l), WriteOp::UpsertTx(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::SaveExtractionState(l), WriteOp::SaveExtractionState(r)) => {
                    l.clone_from(r);
                    return Ok(());
                }
                (WriteOp::InsertContract(l), WriteOp::InsertContract(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::UpdateContracts(l), WriteOp::UpdateContracts(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::InsertAccountBalances(l), WriteOp::InsertAccountBalances(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::InsertProtocolComponents(l), WriteOp::InsertProtocolComponents(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::InsertTokens(l), WriteOp::InsertTokens(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::UpdateTokens(l), WriteOp::InsertTokens(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::InsertComponentBalances(l), WriteOp::InsertComponentBalances(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::UpsertProtocolState(l), WriteOp::UpsertProtocolState(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                (WriteOp::InsertEntryPoints(l), WriteOp::InsertEntryPoints(r)) => {
                    for (component_id, entry_points) in r.iter() {
                        let entry = l
                            .entry(component_id.clone())
                            .or_insert_with(HashSet::new);
                        let len_before = entry.len();
                        entry.extend(entry_points.iter().cloned());
                        self.size += entry.len() - len_before;
                    }
                    return Ok(());
                }
                (
                    WriteOp::InsertEntryPointTracingParams(l),
                    WriteOp::InsertEntryPointTracingParams(r),
                ) => {
                    for (entry_point_id, params) in r.iter() {
                        let entry = l
                            .entry(entry_point_id.clone())
                            .or_insert_with(HashSet::new);
                        let len_before = entry.len();
                        entry.extend(params.iter().cloned());
                        self.size += entry.len() - len_before;
                    }
                    return Ok(());
                }
                (WriteOp::UpsertTracedEntryPoints(l), WriteOp::UpsertTracedEntryPoints(r)) => {
                    self.size += r.len();
                    l.extend(r.iter().cloned());
                    return Ok(());
                }
                _ => continue,
            }
        }
        // not quite accurate but currently all WriteOps are created with a single entry.
        self.size += 1;
        self.operations.push(op);
        Ok(())
    }
}

/// Represents different types of messages that can be sent to the DBCacheWriteExecutor.
pub enum DBCacheMessage {
    Write(DBTransaction),
}

/// Extractors can start transaction.
/// This will guarantee that a group of changes they provide is executed atomically.
///
/// The gateway keeps track of the blockchains progress.
/// A new transaction group finishes. This group has a block attached to it.
/// - If the block is old, we execute the transaction immediately.
/// - If the block is pending, we group the transaction with other transactions that finish before
///   we observe the next block.
///
/// # Write Cache
///
/// This struct handles writes in a centralised and sequential manner. It
/// provides a write-through cache through message passing. This means multiple
/// "writers" can send transactions of write operations simultaneously. Each of
/// those transactions is supposed to relate to a block. As soon as a new block
/// is observed, the currently pending changes are flushed to the database.
///
/// In case a new transaction with an older block comes in, the transaction is
/// immediately applied to the database.
///
/// In case the incoming transactions block is too far ahead / does not
/// connect with the last persisted block, an error is raised.
///
/// Transactions operations are deduplicated, but are executed as separate
/// database transactions therefore in case a transaction fails, it should not
/// affect any other pending transactions.
///
/// ## Deduplication
/// Block, transaction and revert operations are deduplicated. Meaning that if
/// they happen within a batch, they will only be sent once to the actual
/// database.
///
/// ## Design Decisions
/// The current design is bound to evm and diesel models. The bound is
/// purposefully kept somewhat decoupled but not entirely. The reason is to
/// ensure fast development but also have a path that shows how we could
/// decouple especially from evm bounds models, as most likely we will soon have
/// additional chains to deal with.
///
/// Read Operations
/// The class does provide read operations for completeness, but it will not consider any
/// cached changes while reading. Any reads are direct pass through to the database.
pub(crate) struct DBCacheWriteExecutor {
    name: String,
    chain: Chain,
    pool: Pool<AsyncPgConnection>,
    state_gateway: PostgresGateway,
    persisted_block: Option<models::blockchain::Block>,
    msg_receiver: mpsc::Receiver<DBCacheMessage>,
}

impl DBCacheWriteExecutor {
    pub(crate) async fn new(
        name: String,
        chain: Chain,
        pool: Pool<AsyncPgConnection>,
        state_gateway: PostgresGateway,
        msg_receiver: mpsc::Receiver<DBCacheMessage>,
    ) -> Self {
        let mut conn = pool
            .get()
            .await
            .expect("pool should be connected");

        let persisted_block = state_gateway
            .get_block(&BlockIdentifier::Latest(chain), &mut conn)
            .await
            .ok();

        debug!("Persisted block: {:?}", persisted_block);

        Self { name, chain, pool, state_gateway, persisted_block, msg_receiver }
    }

    /// Spawns a task to process incoming database messages (write requests or flush commands).
    pub fn run(mut self) -> JoinHandle<()> {
        info!(name = self.name, "DBCacheWriteExecutor started!");
        tokio::spawn(async move {
            while let Some(message) = self.msg_receiver.recv().await {
                match message {
                    DBCacheMessage::Write(db_tx) => {
                        // Process the write transaction
                        self.write(db_tx).await;
                    }
                }
            }
        })
    }

    #[instrument(name="db_write", skip_all, fields(block_range = %new_db_tx.block_range, extractor_id = tracing::field::Empty))]
    async fn write(&mut self, new_db_tx: DBTransaction) {
        tracing::Span::current().follows_from(new_db_tx.caller_span.id());
        debug!("NewDBTransactionStart");
        if let Some(extractor_id) = new_db_tx.owner.as_ref() {
            tracing::Span::current().record("extractor_id", extractor_id);
        }

        let mut conn = self
            .pool
            .get()
            .await
            .expect("pool should be connected");

        let mut retry_count = 0;
        let max_retries = 3;
        let mut res =
            Err(PostgresError(StorageError::Unexpected("default response error".to_string())));

        while retry_count < max_retries {
            res = conn
                .build_transaction()
                .repeatable_read()
                .run(|conn| {
                    async {
                        for op in new_db_tx.operations.iter() {
                            match self.execute_write_op(op, conn).await {
                                Err(PostgresError(StorageError::DuplicateEntry(entity, id))) => {
                                    // As this db transaction is old. It can contain
                                    // already stored txs, we log the duplicate entry
                                    // error and continue
                                    debug!(
                                        "Ignoring duplicate entry for {} with id {}",
                                        entity, id
                                    );
                                }
                                Err(e) => {
                                    return Err(e);
                                }
                                _ => {}
                            }
                        }
                        Result::<(), PostgresError>::Ok(())
                    }
                    .scope_boxed()
                })
                .await;

            match res {
                Ok(_) => break,
                Err(PostgresError(StorageError::Unexpected(ref e)))
                    if e.contains("deadlock detected") =>
                {
                    retry_count += 1;
                    if retry_count < max_retries {
                        let delay = std::time::Duration::from_secs(retry_count);
                        warn!(
                            "Deadlock detected, retrying in {:?} (attempt {}/{})",
                            delay,
                            retry_count + 1,
                            max_retries
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
                _ => break,
            }
        }

        if res.is_ok() {
            debug!("DBTransactionCommitted");
        }

        if let Err(e) = &res {
            error!(error = ?e, "DBTransactionFailed");
        }

        match self.persisted_block.as_ref() {
            None => {
                self.persisted_block = Some(new_db_tx.block_range.end);
            }
            Some(db_block) if db_block.number < new_db_tx.block_range.start.number => {
                self.persisted_block = Some(new_db_tx.block_range.end);
            }
            _ => {}
        }

        // Forward the result to the sender
        let _ = new_db_tx
            .tx
            .send(res.map_err(Into::into));
    }

    /// Executes an operation.
    ///
    /// This function handles different types of write operations such as
    /// upserts, updates, and reverts, ensuring data consistency in the database.
    #[instrument(skip_all, fields(op=operation.variant_name()))]
    async fn execute_write_op(
        &mut self,
        operation: &WriteOp,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), PostgresError> {
        trace!(op=?operation, name="ExecuteWriteOp");
        match operation {
            WriteOp::UpsertBlock(block) => {
                self.state_gateway
                    .upsert_block(block, conn)
                    .await?
            }
            WriteOp::UpsertTx(transaction) => {
                self.state_gateway
                    .upsert_tx(transaction, conn)
                    .await?
            }
            WriteOp::SaveExtractionState(state) => {
                self.state_gateway
                    .save_state(state, conn)
                    .await?
            }
            WriteOp::InsertContract(contracts) => {
                for contract in contracts.iter() {
                    self.state_gateway
                        .insert_contract(contract, conn)
                        .await?
                }
            }
            WriteOp::UpdateContracts(contracts) => {
                let collected_changes: Vec<(TxHash, &models::contract::AccountDelta)> = contracts
                    .iter()
                    .map(|(tx, update)| (tx.clone(), update))
                    .collect();
                let changes_slice = collected_changes.as_slice();
                self.state_gateway
                    .update_contracts(&self.chain, changes_slice, conn)
                    .await?
            }
            WriteOp::InsertAccountBalances(balances) => {
                self.state_gateway
                    .add_account_balances(balances.as_slice(), &self.chain, conn)
                    .await?
            }
            WriteOp::InsertProtocolComponents(components) => {
                self.state_gateway
                    .add_protocol_components(components.as_slice(), conn)
                    .await?
            }
            WriteOp::InsertTokens(tokens) => {
                self.state_gateway
                    .add_tokens(tokens.as_slice(), conn)
                    .await?
            }
            WriteOp::UpdateTokens(tokens) => {
                self.state_gateway
                    .update_tokens(tokens.as_slice(), conn)
                    .await?
            }
            WriteOp::InsertComponentBalances(balances) => {
                self.state_gateway
                    .add_component_balances(balances.as_slice(), &self.chain, conn)
                    .await?
            }
            WriteOp::UpsertProtocolState(deltas) => {
                let collected_changes: Vec<(
                    TxHash,
                    &models::protocol::ProtocolComponentStateDelta,
                )> = deltas
                    .iter()
                    .map(|(tx, update)| (tx.clone(), update))
                    .collect();
                let changes_slice = collected_changes.as_slice();
                self.state_gateway
                    .update_protocol_states(&self.chain, changes_slice, conn)
                    .await?
            }
            WriteOp::UpsertTracedEntryPoints(traced_entry_points) => {
                self.state_gateway
                    .upsert_traced_entry_points(traced_entry_points.as_slice(), conn)
                    .await?
            }
            WriteOp::InsertEntryPoints(new_entry_points) => {
                self.state_gateway
                    .insert_entry_points(new_entry_points, &self.chain, conn)
                    .await?
            }
            WriteOp::InsertEntryPointTracingParams(new_entry_point_tracing_params) => {
                self.state_gateway
                    .insert_entry_point_tracing_params(
                        new_entry_point_tracing_params,
                        &self.chain,
                        conn,
                    )
                    .await?
            }
        };
        Ok(())
    }
}

#[derive(Hash, Eq, PartialEq, Debug)]
struct RevertParameters {
    start_version: Option<BlockOrTimestamp>,
    end_version: BlockOrTimestamp,
}

type DeltasCache = LruCache<
    RevertParameters,
    (
        Vec<models::contract::AccountDelta>,
        Vec<models::protocol::ProtocolComponentStateDelta>,
        Vec<models::protocol::ComponentBalance>,
    ),
>;

type OpenTx = (DBTransaction, oneshot::Receiver<Result<(), StorageError>>);

pub struct CachedGateway {
    // Can we batch multiple block in here without breaking things?
    // Assuming we are still syncing?

    // TODO: Remove Mutex. It is not needed but avoids changing the Extractor trait.
    open_tx: Arc<Mutex<Option<OpenTx>>>,
    tx: mpsc::Sender<DBCacheMessage>,
    pool: Pool<AsyncPgConnection>,
    state_gateway: PostgresGateway,
    lru_cache: Arc<Mutex<DeltasCache>>,
}

impl Clone for CachedGateway {
    fn clone(&self) -> Self {
        Self {
            // create a separate open tx state for new instances
            open_tx: Arc::new(Mutex::new(None)),
            tx: self.tx.clone(),
            pool: self.pool.clone(),
            state_gateway: self.state_gateway.clone(),
            lru_cache: self.lru_cache.clone(),
        }
    }
}

impl CachedGateway {
    // Accumulating transactions does not drop previous data nor are transactions nested.
    pub async fn start_transaction(&self, block: &models::blockchain::Block, owner: Option<&str>) {
        let mut open_tx = self.open_tx.lock().await;

        if let Some(tx) = open_tx.as_mut() {
            tx.0.block_range.end = block.clone();
        } else {
            let (tx, rx) = oneshot::channel();
            *open_tx = Some((
                DBTransaction {
                    block_range: BlockRange::new(block, block),
                    size: 0,
                    operations: vec![],
                    tx,
                    owner: owner.map(String::from),
                    caller_span: tracing::Span::none(),
                },
                rx,
            ));
        }
    }

    async fn add_op(&self, op: WriteOp) -> Result<(), StorageError> {
        let mut open_tx = self.open_tx.lock().await;
        match open_tx.as_mut() {
            None => {
                Err(StorageError::Unexpected("Usage error: No transaction started".to_string()))
            }
            Some((tx, _)) => {
                tx.add_operation(op)?;
                Ok(())
            }
        }
    }

    pub async fn commit_transaction(&self, min_ops_batch_size: usize) -> Result<(), StorageError> {
        let mut open_tx = self.open_tx.lock().await;
        match open_tx.take() {
            None => {
                Err(StorageError::Unexpected("Usage error: Commit without transaction".to_string()))
            }
            Some((mut db_txn, rx)) => {
                if db_txn.size > min_ops_batch_size {
                    let span = info_span!("DatabaseCommit", size = db_txn.size);
                    db_txn.caller_span = tracing::Span::current();
                    async move {
                        db_txn
                            .operations
                            .sort_by_key(|e| e.order_key());
                        debug!(
                            size = db_txn.size,
                            ops = ?db_txn
                                .operations
                                .iter()
                                .map(WriteOp::variant_name)
                                .collect::<Vec<_>>(),
                            "Submitting db operation batch!"
                        );
                        self.tx
                            .send(DBCacheMessage::Write(db_txn))
                            .await
                            .expect("Send message to receiver ok");
                        rx.await
                            .map_err(|_| StorageError::WriteCacheGoneAway())??;

                        Ok::<(), StorageError>(())
                    }
                    .instrument(span)
                    .await?;
                } else {
                    // if we are not ready to commit, give the OpenTx struct back.
                    *open_tx = Some((db_txn, rx));
                }
                Ok(())
            }
        }
    }

    #[allow(private_interfaces)]
    pub fn new(
        tx: mpsc::Sender<DBCacheMessage>,
        pool: Pool<AsyncPgConnection>,
        state_gateway: PostgresGateway,
    ) -> Self {
        CachedGateway {
            tx,
            open_tx: Arc::new(Mutex::new(None)),
            pool,
            state_gateway,
            lru_cache: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(5).unwrap()))),
        }
    }

    pub async fn get_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<
        (
            Vec<models::contract::AccountDelta>,
            Vec<models::protocol::ProtocolComponentStateDelta>,
            Vec<models::protocol::ComponentBalance>,
        ),
        StorageError,
    > {
        let mut lru_cache = self.lru_cache.lock().await;

        if start_version.is_none() {
            tracing::warn!("Get delta called with start_version = None, this might be a bug in one of the extractors")
        }

        // Construct a key for the LRU cache
        let key = RevertParameters {
            start_version: start_version.cloned(),
            end_version: end_version.clone(),
        };

        // Check if the delta is already in the LRU cache
        if let Some(delta) = lru_cache.get(&key) {
            tracing::debug!("Cached delta hit for {:?}", key);
            return Ok(delta.clone());
        }

        tracing::debug!("Cache didn't hit delta. Getting delta for {:?}", key);

        // Fetch the delta from the database
        let mut db = self.pool.get().await.unwrap();
        let accounts_delta = self
            .state_gateway
            .get_accounts_delta(chain, start_version, end_version, &mut db)
            .await?;
        let protocol_delta = self
            .state_gateway
            .get_protocol_states_delta(chain, start_version, end_version, &mut db)
            .await?;
        let balance_deltas = self
            .state_gateway
            .get_balance_deltas(chain, start_version, end_version, &mut db)
            .await?;

        // Insert the new delta into the LRU cache
        lru_cache
            .put(key, (accounts_delta.clone(), protocol_delta.clone(), balance_deltas.clone()));

        Ok((accounts_delta, protocol_delta, balance_deltas))
    }
}

#[async_trait]
impl ExtractionStateGateway for CachedGateway {
    #[instrument(skip_all)]
    async fn get_state(&self, name: &str, chain: &Chain) -> Result<ExtractionState, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_state(name, chain, &mut conn)
            .await
    }
    #[instrument(skip_all)]
    async fn save_state(&self, new: &ExtractionState) -> Result<(), StorageError> {
        self.add_op(WriteOp::SaveExtractionState(new.clone()))
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ChainGateway for CachedGateway {
    #[instrument(skip_all)]
    async fn upsert_block(&self, new: &[Block]) -> Result<(), StorageError> {
        self.add_op(WriteOp::UpsertBlock(new.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_block(&self, id: &BlockIdentifier) -> Result<Block, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_block(id, &mut conn)
            .await
    }

    async fn upsert_tx(&self, new: &[Transaction]) -> Result<(), StorageError> {
        self.add_op(WriteOp::UpsertTx(new.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_tx(&self, hash: &TxHash) -> Result<Transaction, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_tx(hash, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn revert_state(&self, to: &BlockIdentifier) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .revert_state(to, &mut conn)
            .await
    }
}

#[async_trait]
impl ContractStateGateway for CachedGateway {
    #[instrument(skip_all)]
    async fn get_contract(
        &self,
        id: &ContractId,
        version: Option<&Version>,
        include_slots: bool,
    ) -> Result<Account, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_contract(id, version, include_slots, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_contracts(
        &self,
        chain: &Chain,
        addresses: Option<&[Address]>,
        version: Option<&Version>,
        include_slots: bool,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<Account>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_contracts(chain, addresses, version, include_slots, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn insert_contract(&self, new: &Account) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertContract(vec![new.clone()]))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_contracts(&self, new: &[(TxHash, AccountDelta)]) -> Result<(), StorageError> {
        self.add_op(WriteOp::UpdateContracts(new.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete_contract(&self, id: &ContractId, at_tx: &TxHash) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .delete_contract(id, at_tx, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_accounts_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<Vec<AccountDelta>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_accounts_delta(chain, start_version, end_version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_account_balances(
        &self,
        account_balances: &[AccountBalance],
    ) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertAccountBalances(account_balances.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_account_balances(
        &self,
        chain: &Chain,
        addresses: Option<&[Address]>,
        version: Option<&Version>,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_account_balances(chain, addresses, version, false, &mut conn)
            .await
    }
}

#[async_trait]
impl ProtocolGateway for CachedGateway {
    #[instrument(skip_all)]
    async fn get_protocol_components(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        min_tvl: Option<f64>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<ProtocolComponent>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_protocol_components(chain, system, ids, min_tvl, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_token_owners(
        &self,
        chain: &Chain,
        tokens: &[Address],
        min_balance: Option<f64>,
    ) -> Result<HashMap<Address, (ComponentId, Bytes)>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_token_owners(chain, tokens, min_balance, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_protocol_components(&self, new: &[ProtocolComponent]) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertProtocolComponents(new.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete_protocol_components(
        &self,
        to_delete: &[ProtocolComponent],
        block_ts: NaiveDateTime,
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .delete_protocol_components(to_delete, block_ts, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_protocol_types(
        &self,
        new_protocol_types: &[ProtocolType],
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .add_protocol_types(new_protocol_types, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_protocol_states(
        &self,
        chain: &Chain,
        at: Option<Version>,
        system: Option<String>,
        ids: Option<&[&str]>,
        retrieve_balances: bool,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_protocol_states(
                chain,
                at,
                system,
                ids,
                retrieve_balances,
                pagination_params,
                &mut conn,
            )
            .await
    }

    #[instrument(skip_all)]
    async fn update_protocol_states(
        &self,
        new: &[(TxHash, ProtocolComponentStateDelta)],
    ) -> Result<(), StorageError> {
        self.add_op(WriteOp::UpsertProtocolState(new.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_tokens(
        &self,
        chain: Chain,
        address: Option<&[&Address]>,
        quality: QualityRange,
        traded_n_days_ago: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_tokens(chain, address, quality, traded_n_days_ago, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_component_balances(
        &self,
        component_balances: &[ComponentBalance],
    ) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertComponentBalances(component_balances.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn add_tokens(&self, tokens: &[Token]) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertTokens(tokens.to_vec()))
            .await?;
        Ok(())
    }

    /// Updates tokens without using the write cache.
    ///
    /// This method is currently only used by the tycho-ethereum job and therefore does
    /// not use the write cache. It creates a single transaction and executes all
    /// updates immediately.
    ///
    /// ## Note
    /// This is a short term solution. Ideally we should have a simple gateway version
    /// for these use cases that creates a single transactions and emits them immediately.
    #[instrument(skip_all)]
    async fn update_tokens(&self, tokens: &[Token]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;

        conn.transaction(|conn| {
            async {
                self.state_gateway
                    .update_tokens(tokens, conn)
                    .await?;
                Result::<(), PostgresError>::Ok(())
            }
            .scope_boxed()
        })
        .await
        .map_err(|e| StorageError::Unexpected(format!("Failed to update tokens: {}", e.0)))
    }

    #[instrument(skip_all)]
    async fn get_protocol_states_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<Vec<ProtocolComponentStateDelta>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_protocol_states_delta(chain, start_version, end_version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_balance_deltas(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        target_version: &BlockOrTimestamp,
    ) -> Result<Vec<ComponentBalance>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_balance_deltas(chain, start_version, target_version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_component_balances(
        &self,
        chain: &Chain,
        ids: Option<&[&str]>,
        version: Option<&Version>,
    ) -> Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_component_balances(chain, ids, version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_token_prices(&self, chain: &Chain) -> Result<HashMap<Bytes, f64>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_token_prices(chain, &mut conn)
            .await
    }

    /// TODO: add to transaction instead
    #[instrument(skip_all)]
    async fn upsert_component_tvl(
        &self,
        chain: &Chain,
        tvl_values: &HashMap<String, f64>,
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .upsert_component_tvl(chain, tvl_values, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_protocol_systems(
        &self,
        chain: &Chain,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<String>>, StorageError> {
        self.state_gateway
            .get_protocol_systems(chain, pagination_params)
            .await
    }

    #[instrument(skip_all)]
    async fn get_component_tvls(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<String, f64>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_component_tvls(chain, system, ids, pagination_params, &mut conn)
            .await
    }
}

#[async_trait]
impl EntryPointGateway for CachedGateway {
    #[instrument(skip_all)]
    async fn insert_entry_points(
        &self,
        entry_points: &HashMap<models::ComponentId, HashSet<models::blockchain::EntryPoint>>,
    ) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertEntryPoints(entry_points.clone()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn insert_entry_point_tracing_params(
        &self,
        entry_points_params: &HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
    ) -> Result<(), StorageError> {
        self.add_op(WriteOp::InsertEntryPointTracingParams(entry_points_params.clone()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_entry_points(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPoint>>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_entry_points(filter, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_entry_points_tracing_params(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPointWithTracingParams>>>, StorageError>
    {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_entry_points_tracing_params(filter, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn upsert_traced_entry_points(
        &self,
        traced_entry_points: &[TracedEntryPoint],
    ) -> Result<(), StorageError> {
        self.add_op(WriteOp::UpsertTracedEntryPoints(traced_entry_points.to_vec()))
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_traced_entry_points(
        &self,
        entry_points: &HashSet<EntryPointId>,
    ) -> Result<HashMap<EntryPointId, HashMap<TracingParams, TracingResult>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_tracing_results(entry_points, &mut conn)
            .await
    }
}

impl Gateway for CachedGateway {}

#[cfg(test)]
mod test_serial_db {
    use std::{collections::HashSet, slice, str::FromStr, time::Duration};

    use tycho_common::models::ChangeType;

    use super::*;
    use crate::postgres::{db_fixtures, db_fixtures::yesterday_one_am, testing::run_against_db};

    #[tokio::test]
    async fn test_write_and_flush() {
        run_against_db(|connection_pool| async move {
            let mut connection = connection_pool
                .get()
                .await
                .expect("Failed to get a connection from the pool");
            let chain_id = db_fixtures::insert_chain(&mut connection, "ethereum").await;
            db_fixtures::insert_token(
                &mut connection,
                chain_id,
                "0000000000000000000000000000000000000000",
                "ETH",
                18,
                Some(100),
            )
            .await;
            let gateway: PostgresGateway = PostgresGateway::from_connection(&mut connection).await;
            let (tx, rx) = mpsc::channel(10);
            let write_executor = DBCacheWriteExecutor::new(
                "ethereum".to_owned(),
                Chain::Ethereum,
                connection_pool.clone(),
                gateway.clone(),
                rx,
            )
            .await;

            let handle = write_executor.run();

            // Send write block message
            let block = get_sample_block(1);
            let os_rx = send_write_message(
                &tx,
                block.clone(),
                vec![WriteOp::UpsertBlock(vec![block.clone()])],
            )
            .await;
            os_rx
                .await
                .expect("Response from channel ok")
                .expect("Transaction cached");

            handle.abort();

            let block_id = BlockIdentifier::Number((Chain::Ethereum, 1));
            let fetched_block = gateway
                .get_block(&block_id, &mut connection)
                .await
                .expect("Failed to fetch extraction state");

            assert_eq!(fetched_block, block);
        })
        .await;
    }

    #[tokio::test]
    async fn test_writes_and_new_blocks() {
        run_against_db(|connection_pool| async move {
            let mut connection = connection_pool
                .get()
                .await
                .expect("Failed to get a connection from the pool");
            let chain_id = db_fixtures::insert_chain(&mut connection, "ethereum").await;
            db_fixtures::insert_token(
                &mut connection,
                chain_id,
                "0000000000000000000000000000000000000000",
                "ETH",
                18,
                Some(100),
            )
            .await;
            db_fixtures::insert_protocol_system(&mut connection, "ambient".to_owned()).await;
            db_fixtures::insert_protocol_type(&mut connection, "ambient_pool", None, None, None)
                .await;
            let gateway: PostgresGateway = PostgresGateway::from_connection(&mut connection).await;
            let (tx, rx) = mpsc::channel(10);

            let write_executor = DBCacheWriteExecutor::new(
                "ethereum".to_owned(),
                Chain::Ethereum,
                connection_pool.clone(),
                gateway.clone(),
                rx,
            )
            .await;

            let handle = write_executor.run();

            // Send first block messages
            let block_1 = get_sample_block(1);
            let tx_1 = get_sample_transaction(1);
            let extraction_state_1 = get_sample_extraction(1);
            let usdc_address = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7");
            let token = models::token::Token::new(
                &usdc_address,
                "USDT",
                6,
                0,
                &[Some(64), None],
                Chain::Ethereum,
                100,
            );
            let protocol_component_id = "ambient_USDT-USDC".to_owned();
            let protocol_component = models::protocol::ProtocolComponent {
                id: protocol_component_id.clone(),
                protocol_system: "ambient".to_string(),
                protocol_type_name: "ambient_pool".to_string(),
                chain: Default::default(),
                tokens: vec![usdc_address.clone()],
                contract_addresses: vec![],
                change: ChangeType::Creation,
                creation_tx: tx_1.hash.clone(),
                static_attributes: Default::default(),
                created_at: Default::default(),
            };
            let component_balance = models::protocol::ComponentBalance {
                token: usdc_address.clone(),
                balance_float: 0.0,
                balance: Bytes::from(&[0u8]),
                modify_tx: tx_1.hash.clone(),
                component_id: protocol_component_id.clone(),
            };
            let os_rx_1 = send_write_message(
                &tx,
                block_1.clone(),
                vec![
                    WriteOp::UpsertBlock(vec![block_1.clone()]),
                    WriteOp::UpsertTx(vec![tx_1.clone()]),
                    WriteOp::SaveExtractionState(extraction_state_1.clone()),
                    WriteOp::InsertTokens(vec![token]),
                    WriteOp::InsertProtocolComponents(vec![protocol_component]),
                    WriteOp::InsertComponentBalances(vec![component_balance]),
                ],
            )
            .await;
            os_rx_1
                .await
                .expect("Response from channel ok")
                .expect("Transaction cached");

            // Send second block messages
            let block_2 = get_sample_block(2);
            let attributes: HashMap<String, Bytes> =
                vec![("reserve1".to_owned(), Bytes::from(1000u64).lpad(32, 0))]
                    .into_iter()
                    .collect();
            let protocol_state_delta = models::protocol::ProtocolComponentStateDelta::new(
                protocol_component_id.as_str(),
                attributes,
                HashSet::new(),
            );
            let os_rx_2 = send_write_message(
                &tx,
                block_2.clone(),
                vec![
                    WriteOp::UpsertBlock(vec![block_2.clone()]),
                    WriteOp::UpsertProtocolState(vec![(tx_1.hash.clone(), protocol_state_delta)]),
                ],
            )
            .await;
            os_rx_2
                .await
                .expect("Response from channel ok")
                .expect("Transaction cached");

            // Send third block messages
            let block_3 = get_sample_block(3);
            let os_rx_3 =
                send_write_message(&tx, block_3.clone(), vec![WriteOp::UpsertBlock(vec![block_3])])
                    .await;
            os_rx_3
                .await
                .expect("Response from channel ok")
                .expect("Transaction cached");

            handle.abort();

            // Assert that transactions have been flushed
            let block_id_1 = BlockIdentifier::Number((Chain::Ethereum, 1));
            let fetched_block_1 = gateway
                .get_block(&block_id_1, &mut connection)
                .await
                .expect("Failed to fetch block");

            let fetched_tx = gateway
                .get_tx(&tx_1.hash.clone(), &mut connection)
                .await
                .expect("Failed to fetch tx");

            let fetched_extraction_state = gateway
                .get_state("vm:test", &Chain::Ethereum, &mut connection)
                .await
                .expect("Failed to fetch extraction state");

            let block_id_2 = BlockIdentifier::Number((Chain::Ethereum, 2));
            let fetched_block_2 = gateway
                .get_block(&block_id_2, &mut connection)
                .await
                .expect("Failed to fetch block");

            let block_id_3 = BlockIdentifier::Number((Chain::Ethereum, 3));
            let block_3 = get_sample_block(3);
            let fetched_block_3 = gateway
                .get_block(&block_id_3, &mut connection)
                .await
                .expect("Failed to fetch block");

            // Assert block 1 messages have been flushed
            assert_eq!(fetched_block_1, block_1);
            assert_eq!(fetched_tx, tx_1);
            assert_eq!(fetched_extraction_state, extraction_state_1);
            // Assert block 2 messages have been flushed
            assert_eq!(fetched_block_2, block_2);
            // Assert block 3 messages have been flushed
            assert_eq!(fetched_block_3, block_3);
        })
        .await
    }

    #[test_log::test(tokio::test)]
    async fn test_cached_gateway() {
        // Setup
        run_against_db(|connection_pool| async move {
            let mut connection = connection_pool
                .get()
                .await
                .expect("Failed to get a connection from the pool");
            let chain_id = db_fixtures::insert_chain(&mut connection, "ethereum").await;
            db_fixtures::insert_token(
                &mut connection,
                chain_id,
                "0000000000000000000000000000000000000000",
                "ETH",
                18,
                Some(100),
            )
            .await;
            let gateway: PostgresGateway = PostgresGateway::from_connection(&mut connection).await;
            let (tx, rx) = mpsc::channel(10);

            let write_executor = DBCacheWriteExecutor::new(
                "ethereum".to_owned(),
                Chain::Ethereum,
                connection_pool.clone(),
                gateway.clone(),
                rx,
            )
            .await;

            let handle = write_executor.run();
            let cached_gw = CachedGateway::new(tx, connection_pool.clone(), gateway);

            // Send first block messages
            let block_1 = get_sample_block(1);
            let tx_1 = get_sample_transaction(1);
            cached_gw
                .start_transaction(&block_1, None)
                .await;
            cached_gw
                .upsert_block(slice::from_ref(&block_1))
                .await
                .expect("Upsert block 1 ok");
            cached_gw
                .upsert_tx(slice::from_ref(&tx_1))
                .await
                .expect("Upsert tx 1 ok");
            cached_gw
                .commit_transaction(0)
                .await
                .expect("committing tx failed");

            // Send second block messages
            let block_2 = get_sample_block(2);
            cached_gw
                .start_transaction(&block_2, None)
                .await;
            cached_gw
                .upsert_block(slice::from_ref(&block_2))
                .await
                .expect("Upsert block 2 ok");
            cached_gw
                .commit_transaction(0)
                .await
                .expect("committing tx failed");

            // Send third block messages
            let block_3 = get_sample_block(3);
            cached_gw
                .start_transaction(&block_3, None)
                .await;
            cached_gw
                .upsert_block(slice::from_ref(&block_3))
                .await
                .expect("Upsert block 3 ok");
            cached_gw
                .commit_transaction(0)
                .await
                .expect("committing tx failed");

            handle.abort();

            // Assert that messages from block 1,2 and 3 have been commited to the db.
            let block_id_1 = BlockIdentifier::Number((Chain::Ethereum, 1));
            let fetched_block_1 = cached_gw
                .get_block(&block_id_1)
                .await
                .expect("Failed to fetch block");

            let fetched_tx = cached_gw
                .get_tx(&tx_1.hash.clone())
                .await
                .expect("Failed to fetch tx");

            let block_id_2 = BlockIdentifier::Number((Chain::Ethereum, 2));
            let fetched_block_2 = cached_gw
                .get_block(&block_id_2)
                .await
                .expect("Failed to fetch block");

            let block_id_3 = BlockIdentifier::Number((Chain::Ethereum, 3));
            let fetched_block_3 = cached_gw
                .get_block(&block_id_3)
                .await
                .expect("Failed to fetch block");

            // Assert block 1 messages have been flushed
            assert_eq!(fetched_block_1, block_1);
            assert_eq!(fetched_tx, tx_1);
            // Assert block 2 messages have been flushed
            assert_eq!(fetched_block_2, block_2);
            // Assert block 3 is still pending in cache
            assert_eq!(fetched_block_3, block_3);
        })
        .await;
    }

    fn get_sample_block(version: usize) -> models::blockchain::Block {
        let ts1 = yesterday_one_am();
        let ts2 = ts1 + Duration::from_secs(3600);
        let ts3 = ts2 + Duration::from_secs(3600);
        match version {
            1 => models::blockchain::Block::new(
                1,
                Chain::Ethereum,
                "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6"
                    .parse()
                    .expect("Invalid hash"),
                Bytes::default(),
                ts1,
            ),
            2 => models::blockchain::Block::new(
                2,
                Chain::Ethereum,
                "0xb495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9"
                    .parse()
                    .expect("Invalid hash"),
                "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6"
                    .parse()
                    .expect("Invalid hash"),
                ts2,
            ),
            3 => models::blockchain::Block::new(
                3,
                Chain::Ethereum,
                "0x3d6122660cc824376f11ee842f83addc3525e2dd6756b9bcf0affa6aa88cf741"
                    .parse()
                    .expect("Invalid hash"),
                "0xb495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9"
                    .parse()
                    .expect("Invalid hash"),
                ts3,
            ),
            _ => panic!("Block version not found"),
        }
    }

    fn get_sample_transaction(version: usize) -> models::blockchain::Transaction {
        match version {
            1 => models::blockchain::Transaction {
                hash: Bytes::from(
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                block_hash: Bytes::from(
                    "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                ),
                from: Bytes::from("0x4648451b5F87FF8F0F7D622bD40574bb97E25980"),
                to: Some(Bytes::from("0x6B175474E89094C44Da98b954EedeAC495271d0F")),
                index: 1,
            },
            _ => panic!("Block version not found"),
        }
    }

    fn get_sample_extraction(version: usize) -> ExtractionState {
        match version {
            1 => ExtractionState::new(
                "vm:test".to_string(),
                Chain::Ethereum,
                None,
                "cursor@420".as_bytes(),
                Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                    .unwrap(),
            ),
            _ => panic!("Block version not found"),
        }
    }

    async fn send_write_message(
        tx: &mpsc::Sender<DBCacheMessage>,
        block: models::blockchain::Block,
        operations: Vec<WriteOp>,
    ) -> oneshot::Receiver<Result<(), StorageError>> {
        let (os_tx, os_rx) = oneshot::channel();
        let db_transaction = DBTransaction {
            block_range: BlockRange::new(&block, &block),
            size: operations.len(),
            operations,
            tx: os_tx,
            owner: None,
            caller_span: tracing::Span::none(),
        };

        tx.send(DBCacheMessage::Write(db_transaction))
            .await
            .expect("Failed to send write message through mpsc channel");
        os_rx
    }

    //noinspection SpellCheckingInspection
    #[allow(dead_code)]
    async fn setup_data(conn: &mut AsyncPgConnection) {
        // set up blocks and txns
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        let ts = chrono::Local::now().naive_utc() - Duration::from_secs(3600);
        let tx_hashes = [
            "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945".to_string(),
            "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54".to_string(),
            "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7".to_string(),
            "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388".to_string(),
        ];

        let txn = db_fixtures::insert_txns(
            conn,
            &[
                (blk[0], 1i64, &tx_hashes[0]),
                (blk[0], 2i64, &tx_hashes[1]),
                // ----- Block 01 LAST
                (blk[1], 1i64, &tx_hashes[2]),
                (blk[1], 2i64, &tx_hashes[3]),
                // ----- Block 02 LAST
            ],
        )
        .await;
        let (_, native_token) = db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;

        // set up contract data
        let c0 = db_fixtures::insert_account(
            conn,
            "6B175474E89094C44Da98b954EedeAC495271d0F",
            "account0",
            chain_id,
            Some(txn[0]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 0, native_token, txn[0], Some(&ts), c0).await;
        db_fixtures::insert_contract_code(conn, c0, txn[0], Bytes::from_str("C0C0C0").unwrap())
            .await;
        db_fixtures::insert_account_balance(
            conn,
            100,
            native_token,
            txn[1],
            Some(&(ts + Duration::from_secs(3600))),
            c0,
        )
        .await;
        db_fixtures::insert_slots(conn, c0, txn[1], &ts, None, &[(2, 1, None)]).await;
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[1],
            &ts,
            Some(&(ts + Duration::from_secs(3600))),
            &[(0, 1, None), (1, 5, None)],
        )
        .await;
        db_fixtures::insert_account_balance(conn, 101, native_token, txn[3], None, c0).await;
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[3],
            &(ts + Duration::from_secs(3600)),
            None,
            &[(0, 2, Some(1)), (1, 3, Some(5)), (5, 25, None), (6, 30, None)],
        )
        .await;

        let c1 = db_fixtures::insert_account(
            conn,
            "73BcE791c239c8010Cd3C857d96580037CCdd0EE",
            "c1",
            chain_id,
            Some(txn[2]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 50, native_token, txn[2], None, c1).await;
        db_fixtures::insert_contract_code(conn, c1, txn[2], Bytes::from_str("C1C1C1").unwrap())
            .await;
        db_fixtures::insert_slots(
            conn,
            c1,
            txn[3],
            &(ts + Duration::from_secs(3600)),
            None,
            &[(0, 128, None), (1, 255, None)],
        )
        .await;

        let c2 = db_fixtures::insert_account(
            conn,
            "94a3F312366b8D0a32A00986194053C0ed0CdDb1",
            "c2",
            chain_id,
            Some(txn[1]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 25, native_token, txn[1], None, c2).await;
        db_fixtures::insert_contract_code(conn, c2, txn[1], Bytes::from_str("C2C2C2").unwrap())
            .await;
        db_fixtures::insert_slots(
            conn,
            c2,
            txn[1],
            &(ts + Duration::from_secs(3600)),
            None,
            &[(1, 2, None), (2, 4, None)],
        )
        .await;
        db_fixtures::delete_account(conn, c2, &(ts + Duration::from_secs(3600))).await;

        // set up protocol state data
        let protocol_system_id =
            db_fixtures::insert_protocol_system(conn, "ambient".to_owned()).await;
        let protocol_type_id = db_fixtures::insert_protocol_type(
            conn,
            "Pool",
            Some(models::FinancialType::Swap),
            None,
            Some(models::ImplementationType::Custom),
        )
        .await;
        let protocol_component_id = db_fixtures::insert_protocol_component(
            conn,
            "state1",
            chain_id,
            protocol_system_id,
            protocol_type_id,
            txn[0],
            None,
            None,
        )
        .await;
        // protocol state for state1-reserve1
        db_fixtures::insert_protocol_state(
            conn,
            protocol_component_id,
            txn[0],
            "reserve1".to_owned(),
            Bytes::from(1100u64).lpad(32, 0),
            None,
            Some(txn[2]),
        )
        .await;
    }
}
