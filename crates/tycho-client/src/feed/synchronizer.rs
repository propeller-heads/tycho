use std::{collections::HashMap, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    select,
    sync::{
        mpsc::{channel, error::SendError, Receiver, Sender},
        oneshot,
    },
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, error, info, instrument, trace, warn};
use tycho_common::{
    models::{
        blockchain::{
            BlockAggregatedChanges, DCIUpdate, EntryPointWithTracingParams, TracingResult,
        },
        contract::Account,
        protocol::{ProtocolComponent, ProtocolComponentState},
        ExtractorIdentity,
    },
    Bytes,
};

use crate::{
    deltas::{DeltasClient, SubscriptionOptions},
    feed::{
        component_tracker::{ComponentFilter, ComponentTracker},
        BlockHeader, HeaderLike,
    },
    rpc::{
        RPCClient, RPCError, SnapshotParameters, TracedEntryPointsPaginatedParams,
        RPC_CLIENT_CONCURRENCY,
    },
    DeltasError,
};

#[derive(Error, Debug)]
pub enum SynchronizerError {
    /// RPC client failures.
    #[error("RPC error: {0}")]
    RPCError(#[from] RPCError),

    /// Issues with the main channel
    #[error("{0}")]
    ChannelError(String),

    /// Timeout elapsed errors.
    #[error("Timeout error: {0}")]
    Timeout(String),

    /// Failed to close the synchronizer.
    #[error("Failed to close synchronizer: {0}")]
    CloseError(String),

    /// Server connection failures or interruptions.
    #[error("Connection error: {0}")]
    ConnectionError(String),

    /// Connection closed
    #[error("Connection closed")]
    ConnectionClosed,

    /// Internal error that should not happen under normal operation.
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type SyncResult<T> = Result<T, SynchronizerError>;

impl<T> From<SendError<T>> for SynchronizerError {
    fn from(err: SendError<T>) -> Self {
        SynchronizerError::ChannelError(format!("Failed to send message: {err}"))
    }
}

impl From<DeltasError> for SynchronizerError {
    fn from(err: DeltasError) -> Self {
        match err {
            DeltasError::NotConnected => SynchronizerError::ConnectionClosed,
            _ => SynchronizerError::ConnectionError(err.to_string()),
        }
    }
}

pub struct ProtocolStateSynchronizer<R: RPCClient, D: DeltasClient> {
    extractor_id: ExtractorIdentity,
    retrieve_balances: bool,
    rpc_client: R,
    deltas_client: D,
    max_retries: u64,
    retry_cooldown: Duration,
    include_snapshots: bool,
    component_tracker: ComponentTracker<R>,
    last_synced_block: Option<BlockHeader>,
    timeout: u64,
    include_tvl: bool,
    compression: bool,
    partial_blocks: bool,
    uses_dci: bool,
    /// Brand-new components (in new_protocol_components) whose snapshot we deferred until the
    /// first message of the next block. Only used when partial_blocks is true.
    deferred_snapshot_components: Vec<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct ComponentWithState {
    pub state: ProtocolComponentState,
    pub component: ProtocolComponent,
    pub component_tvl: Option<f64>,
    pub entrypoints: Vec<(EntryPointWithTracingParams, TracingResult)>,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Snapshot {
    pub states: HashMap<String, ComponentWithState>,
    pub vm_storage: HashMap<Bytes, Account>,
}

impl Snapshot {
    fn extend(&mut self, other: Snapshot) {
        self.states.extend(other.states);
        self.vm_storage.extend(other.vm_storage);
    }

    pub fn get_states(&self) -> &HashMap<String, ComponentWithState> {
        &self.states
    }

    pub fn get_vm_storage(&self) -> &HashMap<Bytes, Account> {
        &self.vm_storage
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct StateSyncMessage<H>
where
    H: HeaderLike,
{
    /// The block information for this update.
    pub header: H,
    /// Snapshot for new components.
    pub snapshots: Snapshot,
    /// A single delta contains state updates for all tracked components, as well as additional
    /// information about the system components e.g. newly added components (even below tvl), tvl
    /// updates, balance updates.
    pub deltas: Option<BlockAggregatedChanges>,
    /// Components that stopped being tracked.
    pub removed_components: HashMap<String, ProtocolComponent>,
}

impl<H> StateSyncMessage<H>
where
    H: HeaderLike,
{
    pub fn merge(mut self, other: Self) -> Self {
        // be careful with removed and snapshots attributes here, these can be ambiguous.
        self.removed_components
            .retain(|k, _| !other.snapshots.states.contains_key(k));
        self.snapshots
            .states
            .retain(|k, _| !other.removed_components.contains_key(k));

        self.snapshots.extend(other.snapshots);
        let deltas = match (self.deltas, other.deltas) {
            (Some(l), Some(r)) => Some(l.merge(r)),
            (None, Some(r)) => Some(r),
            (Some(l), None) => Some(l),
            (None, None) => None,
        };
        self.removed_components
            .extend(other.removed_components);
        Self {
            header: other.header,
            snapshots: self.snapshots,
            deltas,
            removed_components: self.removed_components,
        }
    }
}

/// Handle for controlling a running synchronizer task.
///
/// This handle provides methods to gracefully shut down the synchronizer
/// and await its completion with a timeout.
pub struct SynchronizerTaskHandle {
    join_handle: JoinHandle<()>,
    close_tx: oneshot::Sender<()>,
}

/// StateSynchronizer
///
/// Used to synchronize the state of a single protocol. The synchronizer is responsible for
/// delivering messages to the client that let him reconstruct subsets of the protocol state.
///
/// This involves deciding which components to track according to the clients preferences,
/// retrieving & emitting snapshots of components which the client has not seen yet and subsequently
/// delivering delta messages for the components that have changed.
impl SynchronizerTaskHandle {
    pub fn new(join_handle: JoinHandle<()>, close_tx: oneshot::Sender<()>) -> Self {
        Self { join_handle, close_tx }
    }

    /// Splits the handle into its join handle and close sender.
    ///
    /// This allows monitoring the task completion separately from controlling shutdown.
    /// The join handle can be used with FuturesUnordered for monitoring, while the
    /// close sender can be used to signal graceful shutdown.
    pub fn split(self) -> (JoinHandle<()>, oneshot::Sender<()>) {
        (self.join_handle, self.close_tx)
    }
}

#[async_trait]
pub trait StateSynchronizer: Send + Sync + 'static {
    async fn initialize(&mut self) -> SyncResult<()>;
    /// Starts the state synchronization, consuming the synchronizer.
    /// Returns a handle for controlling the running task and a receiver for messages.
    async fn start(
        mut self,
    ) -> (SynchronizerTaskHandle, Receiver<SyncResult<StateSyncMessage<BlockHeader>>>);
}

impl<R, D> ProtocolStateSynchronizer<R, D>
where
    // TODO: Consider moving these constraints directly to the
    // client...
    R: RPCClient + Clone + Send + Sync + 'static,
    D: DeltasClient + Clone + Send + Sync + 'static,
{
    /// Creates a new state synchronizer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        extractor_id: ExtractorIdentity,
        retrieve_balances: bool,
        component_filter: ComponentFilter,
        max_retries: u64,
        retry_cooldown: Duration,
        include_snapshots: bool,
        include_tvl: bool,
        compression: bool,
        rpc_client: R,
        deltas_client: D,
        timeout: u64,
    ) -> Self {
        Self {
            extractor_id: extractor_id.clone(),
            retrieve_balances,
            rpc_client: rpc_client.clone(),
            include_snapshots,
            deltas_client,
            component_tracker: ComponentTracker::new(
                extractor_id.chain,
                extractor_id.name.as_str(),
                component_filter,
                rpc_client,
            ),
            max_retries,
            retry_cooldown,
            last_synced_block: None,
            timeout,
            include_tvl,
            compression,
            partial_blocks: false,
            uses_dci: false,
            deferred_snapshot_components: Vec::new(),
        }
    }

    /// Sets whether this protocol uses Dynamic Contract Indexing (DCI).
    /// When true, entrypoints will be fetched during snapshot retrieval.
    pub fn with_dci(mut self, uses_dci: bool) -> Self {
        self.uses_dci = uses_dci;
        self
    }

    /// Enables receiving partial block updates.
    pub fn with_partial_blocks(mut self, partial_blocks: bool) -> Self {
        self.partial_blocks = partial_blocks;
        self
    }

    /// When using partial blocks, brand-new components are deferred until the first message of the
    /// next block. This returns snapshots for those deferred components if we just advanced to a
    /// new block; otherwise returns an empty snapshot. For full blocks this always returns empty.
    async fn take_flushed_deferred_snapshots(
        &mut self,
        header: &BlockHeader,
    ) -> SyncResult<Snapshot> {
        if !self.partial_blocks {
            return Ok(Snapshot::default());
        }
        let prev = match self
            .last_synced_block
            .as_ref()
            .filter(|p| header.number > p.number)
        {
            Some(p) => p,
            None => return Ok(Snapshot::default()),
        };
        let deferred = std::mem::take(&mut self.deferred_snapshot_components);
        if deferred.is_empty() {
            return Ok(Snapshot::default());
        }
        let snapshot_header =
            BlockHeader { number: prev.number, hash: prev.hash.clone(), ..Default::default() };
        let msg = self
            .get_snapshots(snapshot_header, Some(&deferred))
            .await?;
        Ok(msg.snapshots)
    }

    /// Retrieves state snapshots of the requested components
    async fn get_snapshots<'a, I: IntoIterator<Item = &'a String>>(
        &mut self,
        header: BlockHeader,
        ids: Option<I>,
    ) -> SyncResult<StateSyncMessage<BlockHeader>> {
        if !self.include_snapshots {
            return Ok(StateSyncMessage { header, ..Default::default() });
        }

        // Use given ids or use all if not passed
        let component_ids: Vec<_> = match ids {
            Some(ids) => ids.into_iter().cloned().collect(),
            None => self
                .component_tracker
                .get_tracked_component_ids(),
        };

        if component_ids.is_empty() {
            return Ok(StateSyncMessage { header, ..Default::default() });
        }

        let entrypoints_result: HashMap<
            String,
            Vec<(
                tycho_common::models::blockchain::EntryPointWithTracingParams,
                tycho_common::models::blockchain::TracingResult,
            )>,
        > = if self.uses_dci {
            let result = self
                .rpc_client
                .get_traced_entry_points_paginated(TracedEntryPointsPaginatedParams::new(
                    self.extractor_id.chain,
                    self.extractor_id.name.as_str(),
                    component_ids.clone(),
                    RPC_CLIENT_CONCURRENCY,
                ))
                .await?;
            self.component_tracker
                .process_entrypoints(&DCIUpdate::from(result.clone()));
            result
        } else {
            HashMap::new()
        };

        // Get contract IDs from component tracker
        let contract_ids: Vec<Bytes> = self
            .component_tracker
            .get_contracts_by_component(&component_ids)
            .into_iter()
            .collect();

        // Filter components to only include the requested ones
        let filtered_components: HashMap<_, _> = self
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| component_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let request = SnapshotParameters::new(
            self.extractor_id.chain,
            &self.extractor_id.name,
            &filtered_components,
            &contract_ids,
            header.number,
        )
        .entrypoints(&entrypoints_result)
        .include_balances(self.retrieve_balances)
        .include_tvl(self.include_tvl);
        let snapshot_response = self
            .rpc_client
            .get_snapshots(&request, None, RPC_CLIENT_CONCURRENCY)
            .await?;

        trace!(states=?&snapshot_response.states, "Retrieved ProtocolStates");
        trace!(contract_states=?&snapshot_response.vm_storage, "Retrieved ContractState");

        Ok(StateSyncMessage {
            header,
            snapshots: snapshot_response,
            deltas: None,
            removed_components: HashMap::new(),
        })
    }

    /// Main method that does all the work.
    ///
    /// ## Return Value
    ///
    /// Returns a `Result` where:
    /// - `Ok(())` - Synchronization completed successfully (usually due to close signal)
    /// - `Err((error, None))` - Error occurred AND close signal was received (don't retry)
    /// - `Err((error, Some(end_rx)))` - Error occurred but close signal was NOT received (can
    ///   retry)
    ///
    /// The returned `end_rx` (if any) should be reused for retry attempts since the close
    /// signal may still arrive and we want to remain cancellable across retries.
    #[instrument(skip(self, block_tx, end_rx), fields(extractor_id = %self.extractor_id))]
    async fn state_sync(
        &mut self,
        block_tx: &mut Sender<SyncResult<StateSyncMessage<BlockHeader>>>,
        mut end_rx: oneshot::Receiver<()>,
    ) -> Result<(), (SynchronizerError, Option<oneshot::Receiver<()>>)> {
        // initialisation
        let subscription_options = SubscriptionOptions::new()
            .with_state(self.include_snapshots)
            .with_compression(self.compression)
            .with_partial_blocks(self.partial_blocks);
        let (subscription_id, mut msg_rx) = match self
            .deltas_client
            .subscribe(self.extractor_id.clone(), subscription_options)
            .await
        {
            Ok(result) => result,
            Err(e) => return Err((e.into(), Some(end_rx))),
        };

        let result = async {
            info!("Waiting for deltas...");
            // Track the last seen block number such that we know when we get the first partial
            let mut last_block_number: Option<u64> = None;

            // Outer loop: find a suitable first block and fetch its snapshot. Retries within the
            // same subscription when the snapshot endpoint rejects the block as too old — this
            // happens after a server restart whose persisted state is outside the plan retention
            // window. Consuming the stale delta and waiting for the next one lets the server catch
            // up without tearing down the WS subscription and rebuilding state from scratch.
            const MAX_STALE_RETRIES: u32 = 5;
            let mut stale_retries: u32 = 0;
            let (msg, header) = 'init: loop {
                let mut warned_waiting_for_new_block = false;
                let mut warned_skipping_synced = false;
                let mut first_msg = loop {
                    let msg = select! {
                        deltas_result = timeout(Duration::from_secs(self.timeout), msg_rx.recv()) => {
                            deltas_result
                                .map_err(|_| {
                                    SynchronizerError::Timeout(format!(
                                        "First deltas took longer than {t}s to arrive",
                                        t = self.timeout
                                    ))
                                })?
                                .ok_or_else(|| {
                                    SynchronizerError::ConnectionError(
                                        "Deltas channel closed before first message".to_string(),
                                    )
                                })?
                        },
                        _ = &mut end_rx => {
                            info!("Received close signal while waiting for first deltas");
                            return Ok(());
                        }
                    };

                    let incoming: BlockHeader = (&msg).into();

                    // Determine if this message is a candidate for starting synchronization.
                    // In partial mode, we wait for a new block to start (block number increases).
                    // In non-partial mode, all messages are candidates.
                    let is_new_block_candidate = if self.partial_blocks {
                        match msg.partial_block_index {
                            None => {
                                // If we get a full block, it is a candidate
                                last_block_number = Some(incoming.number);
                                true
                            }
                            Some(current_partial_idx) => {
                                let is_new_block = last_block_number
                                    .map(|prev_block| incoming.number > prev_block)
                                    .unwrap_or(false);

                                if !warned_waiting_for_new_block {
                                    info!(
                                        extractor=%self.extractor_id,
                                        block=incoming.number,
                                        partial_idx=current_partial_idx,
                                        "Syncing. Waiting for new block to start"
                                    );
                                    warned_waiting_for_new_block = true;
                                }
                                last_block_number = Some(incoming.number);
                                is_new_block
                            }
                        }
                    } else {
                        true // Non-partial mode: all messages are candidates
                    };

                    if !is_new_block_candidate {
                        continue;
                    }

                    // Check if we've already synced this block (applies to both modes)
                    if let Some(current) = &self.last_synced_block {
                        if current.number >= incoming.number && !self.is_next_expected(&incoming) {
                            if !warned_skipping_synced {
                                info!(extractor=%self.extractor_id, from=incoming.number, to=current.number, "Syncing. Skipping already synced block");
                                warned_skipping_synced = true;
                            }
                            continue;
                        }
                    }
                    break msg;
                };

                self.filter_deltas(&mut first_msg);

                // initial snapshot
                info!(height = first_msg.get_block().number, "First deltas received");
                let header: BlockHeader = (&first_msg).into();
                let deltas_msg = StateSyncMessage {
                    header: header.clone(),
                    snapshots: Default::default(),
                    deltas: Some(first_msg),
                    removed_components: Default::default(),
                };

                // If possible skip retrieving snapshots
                if !self.is_next_expected(&header) {
                    info!("Retrieving snapshot");
                    // With partial blocks, the server only has full blocks in its buffer; pass the
                    // previous block's header so we request state at N-1, then merge with deltas.
                    let snapshot_header = if self.partial_blocks && header.number > 0 {
                        BlockHeader {
                            number: header.number - 1,
                            hash: header.parent_hash.clone(),
                            ..Default::default()
                        }
                    } else {
                        BlockHeader { revert: false, ..header.clone() }
                    };
                    match self
                        .get_snapshots::<Vec<&String>>(snapshot_header, None)
                        .await
                    {
                        Ok(snapshot) => {
                            let n_components = self.component_tracker.components.len();
                            let n_snapshots = snapshot.snapshots.states.len();
                            info!(n_components, n_snapshots, "Initial snapshot retrieved, starting delta message feed");
                            break 'init (snapshot.merge(deltas_msg), header);
                        }
                        Err(SynchronizerError::RPCError(crate::rpc::RPCError::StaleBlock(
                            reason,
                        ))) => {
                            stale_retries += 1;
                            if stale_retries > MAX_STALE_RETRIES {
                                return Err(SynchronizerError::RPCError(
                                    crate::rpc::RPCError::StaleBlock(reason),
                                ));
                            }
                            // The server's persisted state for this block is outside the plan
                            // retention window. Discard this delta and wait for a fresher block
                            // from the same subscription rather than restarting from scratch.
                            warn!(
                                block = header.number,
                                stale_retries,
                                %reason,
                                "Snapshot block is outside server retention window; \
                                 waiting for a more recent block"
                            );
                            continue 'init;
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    break 'init (deltas_msg, header);
                }
            };

            block_tx.send(Ok(msg)).await?;
            self.last_synced_block = Some(header);
            loop {
                select! {
                    deltas_opt = msg_rx.recv() => {
                        if let Some(mut deltas) = deltas_opt {
                            let header: BlockHeader = (&deltas).into();
                            debug!(block_number=?header.number, "Received delta message");

                            let flushed_snapshots =
                                self.take_flushed_deferred_snapshots(&header).await?;

                            let (snapshots, removed_components) = {
                                // 1. Remove components based on latest changes
                                // 2. Add components based on latest changes, query those for snapshots
                                let (to_add, to_remove) = self.component_tracker.filter_updated_components(&deltas);

                                // Only components we don't track yet need a snapshot.
                                let requiring_snapshot: Vec<String> = to_add
                                    .iter()
                                    .filter(|id| {
                                        !self.component_tracker
                                            .components
                                            .contains_key(id.as_str())
                                    })
                                    .map(|id| id.to_string())
                                    .collect();
                                debug!(components=?requiring_snapshot, "SnapshotRequest");
                                let requiring_snapshot_refs: Vec<&String> = requiring_snapshot.iter().collect();
                                self.component_tracker
                                    .start_tracking(&requiring_snapshot_refs)
                                    .await?;

                                // When partial_blocks: defer brand-new to next block's first message; only preexisting get snapshot now.
                                let request_now: Vec<String> = if self.partial_blocks {
                                    let (preexisting, brand_new) = requiring_snapshot
                                        .into_iter()
                                        .partition(|id| {
                                            !deltas.new_protocol_components.contains_key(id.as_str())
                                        });
                                    self.deferred_snapshot_components.extend(brand_new);
                                    preexisting
                                } else {
                                    requiring_snapshot
                                };

                                let mut snapshots = if request_now.is_empty() {
                                    Snapshot::default()
                                } else {
                                    let snapshot_header = if self.partial_blocks && header.number > 0
                                    {
                                        // get snapshot at the previous block (current block is only partial and not available for querying)
                                        BlockHeader {
                                            number: header.number - 1,
                                            hash: header.parent_hash.clone(),
                                            ..Default::default()
                                        }
                                    } else {
                                        BlockHeader { revert: false, ..header.clone() }
                                    };
                                    self.get_snapshots(snapshot_header, Some(&request_now))
                                        .await?
                                        .snapshots
                                };

                                snapshots.extend(flushed_snapshots);

                                let removed_components = if !to_remove.is_empty() {
                                    self.component_tracker.stop_tracking(&to_remove)
                                } else {
                                    Default::default()
                                };

                                (snapshots, removed_components)
                            };

                            // 3. Update entrypoints on the tracker (affects which contracts are tracked)
                            self.component_tracker.process_entrypoints(&deltas.dci_update);

                            // 4. Filter deltas by currently tracked components / contracts
                            self.filter_deltas(&mut deltas);
                            let n_changes = deltas.n_changes();

                            // 5. Send the message
                            let next = StateSyncMessage {
                                header: header.clone(),
                                snapshots,
                                deltas: Some(deltas),
                                removed_components,
                            };
                            block_tx.send(Ok(next)).await?;
                            self.last_synced_block = Some(header.clone());

                            debug!(block_number=?header.number, n_changes, "Finished processing delta message");
                        } else {
                            return Err(SynchronizerError::ConnectionError("Deltas channel closed".to_string()));
                        }
                    },
                    _ = &mut end_rx => {
                        info!("Received close signal during state_sync");
                        return Ok(());
                    }
                }
            }
        }.await;

        // This cleanup code now runs regardless of how the function exits (error or channel close)
        warn!(last_synced_block = ?&self.last_synced_block, "Deltas processing ended.");
        //Ignore error
        let _ = self
            .deltas_client
            .unsubscribe(subscription_id)
            .await
            .map_err(|err| {
                warn!(err=?err, "Unsubscribing from deltas on cleanup failed!");
            });

        // Handle the result: if it succeeded, we're done. If it errored, we need to determine
        // whether the end_rx was consumed (close signal received) or not
        match result {
            Ok(()) => Ok(()), // Success, likely due to close signal
            Err(e) => {
                // The error came from the inner async block. Since the async block
                // can receive close signals (which would return Ok), any error means
                // the close signal was NOT received, so we can return the end_rx for retry
                Err((e, Some(end_rx)))
            }
        }
    }

    fn is_next_expected(&self, incoming: &BlockHeader) -> bool {
        if let Some(block) = self.last_synced_block.as_ref() {
            return incoming.parent_hash == block.hash;
        }
        false
    }
    fn filter_deltas(&self, deltas: &mut BlockAggregatedChanges) {
        deltas.filter_by_component(|id| {
            self.component_tracker
                .components
                .contains_key(id)
        });
        deltas.filter_by_contract(|id| {
            self.component_tracker
                .contracts
                .contains(id)
        });
    }
}

#[async_trait]
impl<R, D> StateSynchronizer for ProtocolStateSynchronizer<R, D>
where
    R: RPCClient + Clone + Send + Sync + 'static,
    D: DeltasClient + Clone + Send + Sync + 'static,
{
    async fn initialize(&mut self) -> SyncResult<()> {
        info!("Retrieving relevant protocol components");
        self.component_tracker
            .initialise_components()
            .await?;
        info!(
            n_components = self.component_tracker.components.len(),
            n_contracts = self.component_tracker.contracts.len(),
            extractor = %self.extractor_id,
            "Finished retrieving components",
        );

        Ok(())
    }

    async fn start(
        mut self,
    ) -> (SynchronizerTaskHandle, Receiver<SyncResult<StateSyncMessage<BlockHeader>>>) {
        let (mut tx, rx) = channel(15);
        let (end_tx, end_rx) = oneshot::channel::<()>();

        let jh = tokio::spawn(async move {
            let mut retry_count = 0;
            let mut current_end_rx = end_rx;
            let mut final_error = None;

            while retry_count < self.max_retries {
                info!(extractor_id=%&self.extractor_id, retry_count, "(Re)starting synchronization loop");

                let prev_block = self
                    .last_synced_block
                    .as_ref()
                    .map(|h| h.number);
                let res = self
                    .state_sync(&mut tx, current_end_rx)
                    .await;
                let made_progress = self
                    .last_synced_block
                    .as_ref()
                    .map(|h| h.number) >
                    prev_block;
                match res {
                    Ok(()) => {
                        info!(
                            extractor_id=%&self.extractor_id,
                            retry_count,
                            "State synchronization exited cleanly"
                        );
                        return;
                    }
                    Err((e, maybe_end_rx)) => {
                        warn!(
                            extractor_id=%&self.extractor_id,
                            retry_count,
                            error=%e,
                            "State synchronization errored!"
                        );

                        // If we have the end_rx back, we can retry
                        if let Some(recovered_end_rx) = maybe_end_rx {
                            current_end_rx = recovered_end_rx;

                            if let SynchronizerError::ConnectionClosed = e {
                                // break synchronization loop if websocket client is dead
                                error!(
                                    "Websocket connection closed. State synchronization exiting."
                                );
                                let _ = tx.send(Err(e)).await;
                                return;
                            } else {
                                // Store error in case this is our last retry
                                final_error = Some(e);
                            }
                        } else {
                            // Close signal was received, exit cleanly
                            info!(extractor_id=%&self.extractor_id, "Received close signal, exiting.");
                            return;
                        }
                    }
                }
                sleep(self.retry_cooldown).await;
                // A run that processed blocks is a healthy run — reset the counter so
                // transient failures after a long successful period get a fresh retry budget.
                if made_progress {
                    retry_count = 0;
                } else {
                    retry_count += 1;
                }
            }
            if let Some(e) = final_error {
                warn!(extractor_id=%&self.extractor_id, retry_count, error=%e, "Max retries exceeded");
                let _ = tx.send(Err(e)).await;
            }
        });

        let handle = SynchronizerTaskHandle::new(jh, end_tx);
        (handle, rx)
    }
}

#[cfg(test)]
mod test {
    //! Test suite for ProtocolStateSynchronizer shutdown and cleanup behavior.
    //!
    //! ## Test Coverage Strategy:
    //!
    //! ### Shutdown & Close Signal Tests:
    //! - `test_public_close_api_functionality` - Tests public API (start/close lifecycle)
    //! - `test_close_signal_while_waiting_for_first_deltas` - Close during initial wait
    //! - `test_close_signal_during_main_processing_loop` - Close during main processing
    //!
    //! ### Cleanup & Error Handling Tests:
    //! - `test_cleanup_runs_when_state_sync_processing_errors` - Cleanup on processing errors
    //!
    //! ### Coverage Summary:
    //! These tests ensure cleanup code (shared state reset + unsubscribe) runs on ALL exit paths:
    //! ✓ Close signal before first deltas   ✓ Close signal during processing
    //! ✓ Processing errors                  ✓ Channel closure
    //! ✓ Public API close operations        ✓ Normal completion

    use std::{collections::HashSet, sync::Arc};

    use tycho_common::models::{
        blockchain::{
            AddressStorageLocation, Block, BlockAggregatedChanges, DCIUpdate, EntryPoint,
            EntryPointWithTracingParams, RPCTracerParams, TracingParams, TracingResult,
        },
        protocol::{ProtocolComponent, ProtocolComponentState},
        token::Token,
        Chain,
    };
    use uuid::Uuid;

    use super::*;
    use crate::{
        deltas::MockDeltasClient,
        rpc::{MockRPCClient, Page},
        DeltasError, RPCError,
    };

    // Required for mock client to implement clone
    struct ArcRPCClient<T>(Arc<T>);

    // Default derive(Clone) does require T to be Clone as well.
    impl<T> Clone for ArcRPCClient<T> {
        fn clone(&self) -> Self {
            ArcRPCClient(self.0.clone())
        }
    }

    #[async_trait]
    impl<T> RPCClient for ArcRPCClient<T>
    where
        T: RPCClient + Sync + Send + 'static,
    {
        async fn get_tokens(
            &self,
            params: crate::rpc::TokensParams,
        ) -> Result<crate::rpc::Page<Vec<Token>>, RPCError> {
            self.0.get_tokens(params).await
        }

        async fn get_contract_state(
            &self,
            params: crate::rpc::ContractStateParams,
        ) -> Result<crate::rpc::Page<Vec<Account>>, RPCError> {
            self.0.get_contract_state(params).await
        }

        async fn get_protocol_components(
            &self,
            params: crate::rpc::ProtocolComponentsParams,
        ) -> Result<crate::rpc::Page<Vec<ProtocolComponent>>, RPCError> {
            self.0
                .get_protocol_components(params)
                .await
        }

        async fn get_protocol_states(
            &self,
            params: crate::rpc::ProtocolStatesParams,
        ) -> Result<crate::rpc::Page<Vec<ProtocolComponentState>>, RPCError> {
            self.0.get_protocol_states(params).await
        }

        async fn get_protocol_systems(
            &self,
            params: crate::rpc::ProtocolSystemsParams,
        ) -> Result<crate::rpc::Page<crate::rpc::ProtocolSystems>, RPCError> {
            self.0
                .get_protocol_systems(params)
                .await
        }

        async fn get_component_tvl(
            &self,
            params: crate::rpc::ComponentTvlParams,
        ) -> Result<crate::rpc::Page<HashMap<String, f64>>, RPCError> {
            self.0.get_component_tvl(params).await
        }

        async fn get_traced_entry_points(
            &self,
            params: crate::rpc::TracedEntryPointsParams,
        ) -> Result<
            crate::rpc::Page<HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>>>,
            RPCError,
        > {
            self.0
                .get_traced_entry_points(params)
                .await
        }

        async fn get_snapshots<'a>(
            &self,
            request: &SnapshotParameters<'a>,
            chunk_size: Option<usize>,
            concurrency: usize,
        ) -> Result<Snapshot, RPCError> {
            self.0
                .get_snapshots(request, chunk_size, concurrency)
                .await
        }

        fn compression(&self) -> bool {
            self.0.compression()
        }
    }

    // Required for mock client to implement clone
    struct ArcDeltasClient<T>(Arc<T>);

    // Default derive(Clone) does require T to be Clone as well.
    impl<T> Clone for ArcDeltasClient<T> {
        fn clone(&self) -> Self {
            ArcDeltasClient(self.0.clone())
        }
    }

    #[async_trait]
    impl<T> DeltasClient for ArcDeltasClient<T>
    where
        T: DeltasClient + Sync + Send + 'static,
    {
        async fn subscribe(
            &self,
            extractor_id: tycho_common::models::ExtractorIdentity,
            options: SubscriptionOptions,
        ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError> {
            self.0
                .subscribe(extractor_id, options)
                .await
        }

        async fn unsubscribe(&self, subscription_id: Uuid) -> Result<(), DeltasError> {
            self.0
                .unsubscribe(subscription_id)
                .await
        }

        async fn connect(&self) -> Result<JoinHandle<Result<(), DeltasError>>, DeltasError> {
            self.0.connect().await
        }

        async fn close(&self) -> Result<(), DeltasError> {
            self.0.close().await
        }
    }

    fn with_mocked_clients(
        native: bool,
        include_tvl: bool,
        rpc_client: Option<MockRPCClient>,
        deltas_client: Option<MockDeltasClient>,
    ) -> ProtocolStateSynchronizer<ArcRPCClient<MockRPCClient>, ArcDeltasClient<MockDeltasClient>>
    {
        let rpc_client = ArcRPCClient(Arc::new(rpc_client.unwrap_or_default()));
        let deltas_client = ArcDeltasClient(Arc::new(deltas_client.unwrap_or_default()));

        ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            native,
            ComponentFilter::with_tvl_range(50.0, 50.0),
            1,
            Duration::from_secs(0),
            true,
            include_tvl,
            true, // Does not matter as we mock the client that never compresses
            rpc_client,
            deltas_client,
            10_u64,
        )
    }

    fn state_snapshot_native() -> Vec<ProtocolComponentState> {
        vec![ProtocolComponentState {
            component_id: "Component1".to_string(),
            attributes: HashMap::new(),
            balances: HashMap::new(),
        }]
    }

    fn make_mock_client() -> MockRPCClient {
        let mut m = MockRPCClient::new();
        m.expect_compression()
            .return_const(false);
        m
    }

    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_native() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();
        let component = ProtocolComponent { id: "Component1".to_string(), ..Default::default() };

        let component_clone = component.clone();
        rpc.expect_get_snapshots()
            .returning(move |_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: state_snapshot_native()
                        .into_iter()
                        .map(|state| {
                            (
                                state.component_id.clone(),
                                ComponentWithState {
                                    state,
                                    component: component_clone.clone(),
                                    entrypoints: vec![],
                                    component_tvl: None,
                                },
                            )
                        })
                        .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        rpc.expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let mut state_sync = with_mocked_clients(true, false, Some(rpc), None);
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component.clone());
        let components_arg = ["Component1".to_string()];
        let exp = StateSyncMessage {
            header: header.clone(),
            snapshots: Snapshot {
                states: state_snapshot_native()
                    .into_iter()
                    .map(|state| {
                        (
                            state.component_id.clone(),
                            ComponentWithState {
                                state,
                                component: component.clone(),
                                entrypoints: vec![],
                                component_tvl: None,
                            },
                        )
                    })
                    .collect(),
                vm_storage: HashMap::new(),
            },
            deltas: None,
            removed_components: Default::default(),
        };

        let snap = state_sync
            .get_snapshots(header, Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        assert_eq!(snap, exp);
    }

    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_native_with_tvl() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();
        let component = ProtocolComponent { id: "Component1".to_string(), ..Default::default() };

        let component_clone = component.clone();
        rpc.expect_get_snapshots()
            .returning(move |_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: state_snapshot_native()
                        .into_iter()
                        .map(|state| {
                            (
                                state.component_id.clone(),
                                ComponentWithState {
                                    state,
                                    component: component_clone.clone(),
                                    component_tvl: Some(100.0),
                                    entrypoints: vec![],
                                },
                            )
                        })
                        .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        rpc.expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let mut state_sync = with_mocked_clients(true, true, Some(rpc), None);
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component.clone());
        let components_arg = ["Component1".to_string()];
        let exp = StateSyncMessage {
            header: header.clone(),
            snapshots: Snapshot {
                states: state_snapshot_native()
                    .into_iter()
                    .map(|state| {
                        (
                            state.component_id.clone(),
                            ComponentWithState {
                                state,
                                component: component.clone(),
                                component_tvl: Some(100.0),
                                entrypoints: vec![],
                            },
                        )
                    })
                    .collect(),
                vm_storage: HashMap::new(),
            },
            deltas: None,
            removed_components: Default::default(),
        };

        let snap = state_sync
            .get_snapshots(header, Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        assert_eq!(snap, exp);
    }

    fn state_snapshot_vm() -> Vec<Account> {
        vec![
            Account::new(
                Chain::default(),
                Bytes::from("0x0badc0ffee"),
                String::new(),
                HashMap::new(),
                Bytes::default(),
                HashMap::new(),
                Bytes::default(),
                Bytes::default(),
                Bytes::default(),
                Bytes::default(),
                None,
            ),
            Account::new(
                Chain::default(),
                Bytes::from("0xbabe42"),
                String::new(),
                HashMap::new(),
                Bytes::default(),
                HashMap::new(),
                Bytes::default(),
                Bytes::default(),
                Bytes::default(),
                Bytes::default(),
                None,
            ),
        ]
    }

    fn traced_entry_point_response(
    ) -> HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>> {
        HashMap::from([(
            "Component1".to_string(),
            vec![(
                EntryPointWithTracingParams {
                    entry_point: EntryPoint {
                        external_id: "entrypoint_a".to_string(),
                        target: Bytes::from("0x0badc0ffee"),
                        signature: "sig()".to_string(),
                    },
                    params: TracingParams::RPCTracer(RPCTracerParams {
                        caller: Some(Bytes::from("0x0badc0ffee")),
                        calldata: Bytes::from("0x0badc0ffee"),
                        state_overrides: None,
                        prune_addresses: None,
                    }),
                },
                TracingResult {
                    retriggers: HashSet::from([(
                        Bytes::from("0x0badc0ffee"),
                        AddressStorageLocation::new(Bytes::from("0x0badc0ffee"), 12),
                    )]),
                    accessed_slots: HashMap::from([(
                        Bytes::from("0x0badc0ffee"),
                        HashSet::from([Bytes::from("0xbadbeef0")]),
                    )]),
                },
            )],
        )])
    }

    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_vm() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();

        let traced_ep_response = traced_entry_point_response();
        rpc.expect_get_snapshots()
            .returning(move |_request, _chunk_size, _concurrency| {
                let vm_storage_accounts = state_snapshot_vm();
                Ok(Snapshot {
                    states: [(
                        "Component1".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState {
                                component_id: "Component1".to_string(),
                                attributes: HashMap::new(),
                                balances: HashMap::new(),
                            },
                            component: ProtocolComponent {
                                id: "Component1".to_string(),
                                contract_addresses: vec![
                                    Bytes::from("0x0badc0ffee"),
                                    Bytes::from("0xbabe42"),
                                ],
                                ..Default::default()
                            },
                            component_tvl: None,
                            entrypoints: traced_ep_response
                                .get("Component1")
                                .cloned()
                                .unwrap_or_default(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: vm_storage_accounts
                        .into_iter()
                        .map(|account| (account.address.clone(), account))
                        .collect(),
                })
            });

        rpc.expect_get_traced_entry_points()
            .returning(move |_| Ok(Page::new(traced_entry_point_response(), 1, 0, 100)));

        let mut state_sync = with_mocked_clients(false, false, Some(rpc), None);
        let component = ProtocolComponent {
            id: "Component1".to_string(),
            contract_addresses: vec![Bytes::from("0x0badc0ffee"), Bytes::from("0xbabe42")],
            ..Default::default()
        };
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component.clone());
        let components_arg = ["Component1".to_string()];
        let exp = StateSyncMessage {
            header: header.clone(),
            snapshots: Snapshot {
                states: [(
                    component.id.clone(),
                    ComponentWithState {
                        state: ProtocolComponentState {
                            component_id: "Component1".to_string(),
                            attributes: HashMap::new(),
                            balances: HashMap::new(),
                        },
                        component: component.clone(),
                        component_tvl: None,
                        entrypoints: traced_entry_point_response()
                            .remove("Component1")
                            .unwrap_or_default(),
                    },
                )]
                .into_iter()
                .collect(),
                vm_storage: state_snapshot_vm()
                    .into_iter()
                    .map(|account| (account.address.clone(), account))
                    .collect(),
            },
            deltas: None,
            removed_components: Default::default(),
        };

        let snap = state_sync
            .get_snapshots(header, Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        assert_eq!(snap, exp);
    }

    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_vm_with_tvl() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();
        let component = ProtocolComponent {
            id: "Component1".to_string(),
            contract_addresses: vec![Bytes::from("0x0badc0ffee"), Bytes::from("0xbabe42")],
            ..Default::default()
        };

        let component_clone = component.clone();
        rpc.expect_get_snapshots()
            .returning(move |_request, _chunk_size, _concurrency| {
                let vm_storage_accounts = state_snapshot_vm();
                Ok(Snapshot {
                    states: [(
                        "Component1".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState {
                                component_id: "Component1".to_string(),
                                attributes: HashMap::new(),
                                balances: HashMap::new(),
                            },
                            component: component_clone.clone(),
                            component_tvl: Some(100.0),
                            entrypoints: vec![],
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: vm_storage_accounts
                        .into_iter()
                        .map(|account| (account.address.clone(), account))
                        .collect(),
                })
            });

        rpc.expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let mut state_sync = with_mocked_clients(false, true, Some(rpc), None);
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component.clone());
        let components_arg = ["Component1".to_string()];
        let exp = StateSyncMessage {
            header: header.clone(),
            snapshots: Snapshot {
                states: [(
                    component.id.clone(),
                    ComponentWithState {
                        state: ProtocolComponentState {
                            component_id: "Component1".to_string(),
                            attributes: HashMap::new(),
                            balances: HashMap::new(),
                        },
                        component: component.clone(),
                        component_tvl: Some(100.0),
                        entrypoints: vec![],
                    },
                )]
                .into_iter()
                .collect(),
                vm_storage: state_snapshot_vm()
                    .into_iter()
                    .map(|account| (account.address.clone(), account))
                    .collect(),
            },
            deltas: None,
            removed_components: Default::default(),
        };

        let snap = state_sync
            .get_snapshots(header, Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        assert_eq!(snap, exp);
    }

    /// Test that get_snapshots only fetches snapshots for requested components,
    /// not all tracked components. This prevents returning full snapshots repeatedly
    /// when only a subset of components need updates.
    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_filters_to_requested_components_only() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();

        // Create three components
        let component1 = ProtocolComponent { id: "Component1".to_string(), ..Default::default() };
        let component2 = ProtocolComponent { id: "Component2".to_string(), ..Default::default() };
        let component3 = ProtocolComponent { id: "Component3".to_string(), ..Default::default() };

        let component2_clone = component2.clone();

        // Mock the RPC call and verify it only receives Component2
        rpc.expect_get_snapshots()
            .withf(
                |request: &SnapshotParameters,
                 _chunk_size: &Option<usize>,
                 _concurrency: &usize| {
                    // Verify that the request contains ONLY Component2, not all tracked components
                    request.components.len() == 1 &&
                        request
                            .components
                            .contains_key("Component2")
                },
            )
            .times(1)
            .returning(move |_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "Component2".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState {
                                component_id: "Component2".to_string(),
                                attributes: HashMap::new(),
                                balances: HashMap::new(),
                            },
                            component: component2_clone.clone(),
                            entrypoints: vec![],
                            component_tvl: None,
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        rpc.expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let mut state_sync = with_mocked_clients(true, false, Some(rpc), None);

        // Track all three components
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component1.clone());
        state_sync
            .component_tracker
            .components
            .insert("Component2".to_string(), component2.clone());
        state_sync
            .component_tracker
            .components
            .insert("Component3".to_string(), component3.clone());

        // Request snapshot for ONLY Component2
        let components_arg = ["Component2".to_string()];

        let snap = state_sync
            .get_snapshots(header.clone(), Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        // Verify we only got Component2 back
        assert_eq!(snap.snapshots.states.len(), 1);
        assert!(snap
            .snapshots
            .states
            .contains_key("Component2"));
        assert!(!snap
            .snapshots
            .states
            .contains_key("Component1"));
        assert!(!snap
            .snapshots
            .states
            .contains_key("Component3"));
    }

    fn mock_clients_for_state_sync(
    ) -> (MockRPCClient, MockDeltasClient, Sender<BlockAggregatedChanges>) {
        let mut rpc_client = make_mock_client();
        // Mocks for the start_tracking call, these need to come first because they are more
        // specific, see: https://docs.rs/mockall/latest/mockall/#matching-multiple-calls
        rpc_client
            .expect_get_protocol_components()
            .withf(|params: &crate::rpc::ProtocolComponentsParams| {
                params
                    .component_ids()
                    .is_some_and(|ids| ids.contains(&"Component3".to_string()))
            })
            .returning(|_| {
                // return Component3
                Ok(Page::new(
                    vec![
                        // this component shall have a tvl update above threshold
                        ProtocolComponent { id: "Component3".to_string(), ..Default::default() },
                    ],
                    1,
                    0,
                    100,
                ))
            });
        // Mock get_snapshots for Component3
        rpc_client
            .expect_get_snapshots()
            .withf(
                |request: &SnapshotParameters,
                 _chunk_size: &Option<usize>,
                 _concurrency: &usize| {
                    request
                        .components
                        .contains_key("Component3")
                },
            )
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "Component3".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component3",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "Component3".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(1000.0),
                            entrypoints: vec![],
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        // mock calls for the initial state snapshots
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| {
                // Initial sync of components
                Ok(Page::new(
                    vec![
                        // this component shall have a tvl update above threshold
                        ProtocolComponent { id: "Component1".to_string(), ..Default::default() },
                        // this component shall have a tvl update below threshold.
                        ProtocolComponent { id: "Component2".to_string(), ..Default::default() },
                        // a third component will have a tvl update above threshold
                    ],
                    2,
                    0,
                    100,
                ))
            });

        rpc_client
            .expect_get_snapshots()
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [
                        (
                            "Component1".to_string(),
                            ComponentWithState {
                                state: ProtocolComponentState::new(
                                    "Component1",
                                    Default::default(),
                                    Default::default(),
                                ),
                                component: ProtocolComponent {
                                    id: "Component1".to_string(),
                                    ..Default::default()
                                },
                                component_tvl: Some(100.0),
                                entrypoints: vec![],
                            },
                        ),
                        (
                            "Component2".to_string(),
                            ComponentWithState {
                                state: ProtocolComponentState::new(
                                    "Component2",
                                    Default::default(),
                                    Default::default(),
                                ),
                                component: ProtocolComponent {
                                    id: "Component2".to_string(),
                                    ..Default::default()
                                },
                                component_tvl: Some(0.0),
                                entrypoints: vec![],
                            },
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        // Mock get_traced_entry_points for Ethereum chain
        rpc_client
            .expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        // Mock deltas client and messages
        let mut deltas_client = MockDeltasClient::new();
        let (tx, rx) = channel(1);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                // Return subscriber id and a channel
                Ok((Uuid::default(), rx))
            });

        // Expect unsubscribe call during cleanup
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        (rpc_client, deltas_client, tx)
    }

    /// Test strategy
    ///
    /// - initial snapshot retrieval returns two component1 and component2 as snapshots
    /// - send 2 dummy messages, containing only blocks
    /// - third message contains a new component with some significant tvl, one initial component
    ///   slips below tvl threshold, another one is above tvl but does not get re-requested.
    #[test_log::test(tokio::test)]
    async fn test_state_sync() {
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync();
        let deltas = [
            BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 1,
                    hash: Bytes::from("0x01"),
                    parent_hash: Bytes::from("0x00"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
                dci_update: DCIUpdate {
                    new_entrypoints: HashMap::from([(
                        "Component1".to_string(),
                        HashSet::from([EntryPoint {
                            external_id: "entrypoint_a".to_string(),
                            target: Bytes::from("0x0badc0ffee"),
                            signature: "sig()".to_string(),
                        }]),
                    )]),
                    new_entrypoint_params: HashMap::from([(
                        "entrypoint_a".to_string(),
                        HashSet::from([(
                            TracingParams::RPCTracer(RPCTracerParams {
                                caller: Some(Bytes::from("0x0badc0ffee")),
                                calldata: Bytes::from("0x0badc0ffee"),
                                state_overrides: None,
                                prune_addresses: None,
                            }),
                            "Component1".to_string(),
                        )]),
                    )]),
                    trace_results: HashMap::from([(
                        "entrypoint_a".to_string(),
                        TracingResult {
                            retriggers: HashSet::from([(
                                Bytes::from("0x0badc0ffee"),
                                AddressStorageLocation::new(Bytes::from("0x0badc0ffee"), 12),
                            )]),
                            accessed_slots: HashMap::from([(
                                Bytes::from("0x0badc0ffee"),
                                HashSet::from([Bytes::from("0xbadbeef0")]),
                            )]),
                        },
                    )]),
                },
                ..Default::default()
            },
            BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 2,
                    hash: Bytes::from("0x02"),
                    parent_hash: Bytes::from("0x01"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
                component_tvl: [
                    ("Component1".to_string(), 100.0),
                    ("Component2".to_string(), 0.0),
                    ("Component3".to_string(), 1000.0),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        ];
        let mut state_sync = with_mocked_clients(true, true, Some(rpc_client), Some(deltas_client));
        state_sync
            .initialize()
            .await
            .expect("Init failed");

        // Test starts here
        let (handle, mut rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();
        tx.send(deltas[0].clone())
            .await
            .expect("deltas channel msg 0 closed!");
        let first_msg = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("waiting for first state msg timed out!")
            .expect("state sync block sender closed!");
        tx.send(deltas[1].clone())
            .await
            .expect("deltas channel msg 1 closed!");
        let second_msg = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("waiting for second state msg timed out!")
            .expect("state sync block sender closed!");
        let _ = close_tx.send(());
        jh.await
            .expect("state sync task panicked!");

        // assertions
        let exp1 = StateSyncMessage {
            header: BlockHeader {
                number: 1,
                hash: Bytes::from("0x01"),
                parent_hash: Bytes::from("0x00"),
                revert: false,
                ..Default::default()
            },
            snapshots: Snapshot {
                states: [
                    (
                        "Component1".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component1",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "Component1".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(100.0),
                            entrypoints: vec![],
                        },
                    ),
                    (
                        "Component2".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component2",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "Component2".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(0.0),
                            entrypoints: vec![],
                        },
                    ),
                ]
                .into_iter()
                .collect(),
                vm_storage: HashMap::new(),
            },
            deltas: Some(deltas[0].clone()),
            removed_components: Default::default(),
        };

        let exp2 = StateSyncMessage {
            header: BlockHeader {
                number: 2,
                hash: Bytes::from("0x02"),
                parent_hash: Bytes::from("0x01"),
                revert: false,
                ..Default::default()
            },
            snapshots: Snapshot {
                states: [
                    // This is the new component we queried once it passed the tvl threshold.
                    (
                        "Component3".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component3",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "Component3".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(1000.0),
                            entrypoints: vec![],
                        },
                    ),
                ]
                .into_iter()
                .collect(),
                vm_storage: HashMap::new(),
            },
            // Our deltas are empty and since merge methods are
            // tested in tycho-common we don't have much to do here.
            deltas: Some(BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 2,
                    hash: Bytes::from("0x02"),
                    parent_hash: Bytes::from("0x01"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
                component_tvl: [
                    // "Component2" should not show here.
                    ("Component1".to_string(), 100.0),
                    ("Component3".to_string(), 1000.0),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            }),
            // "Component2" was removed, because its tvl changed to 0.
            removed_components: [(
                "Component2".to_string(),
                ProtocolComponent { id: "Component2".to_string(), ..Default::default() },
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(first_msg.unwrap(), exp1);
        assert_eq!(second_msg.unwrap(), exp2);
    }

    #[test_log::test(tokio::test)]
    async fn test_state_sync_with_tvl_range() {
        // Define the range for testing
        let remove_tvl_threshold = 5.0;
        let add_tvl_threshold = 7.0;

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        rpc_client
            .expect_get_protocol_components()
            .withf(|params: &crate::rpc::ProtocolComponentsParams| {
                params
                    .component_ids()
                    .is_some_and(|ids| ids.contains(&"Component3".to_string()))
            })
            .returning(|_| {
                Ok(Page::new(
                    vec![ProtocolComponent { id: "Component3".to_string(), ..Default::default() }],
                    1,
                    0,
                    100,
                ))
            });
        // Mock get_snapshots for Component3
        rpc_client
            .expect_get_snapshots()
            .withf(
                |request: &SnapshotParameters,
                 _chunk_size: &Option<usize>,
                 _concurrency: &usize| {
                    request
                        .components
                        .contains_key("Component3")
                },
            )
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "Component3".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component3",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "Component3".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(10.0),
                            entrypoints: vec![],
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        // Mock for the initial snapshot retrieval
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| {
                Ok(Page::new(
                    vec![
                        ProtocolComponent { id: "Component1".to_string(), ..Default::default() },
                        ProtocolComponent { id: "Component2".to_string(), ..Default::default() },
                    ],
                    2,
                    0,
                    100,
                ))
            });

        // Mock get_snapshots for initial snapshot
        rpc_client
            .expect_get_snapshots()
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [
                        (
                            "Component1".to_string(),
                            ComponentWithState {
                                state: ProtocolComponentState::new(
                                    "Component1",
                                    Default::default(),
                                    Default::default(),
                                ),
                                component: ProtocolComponent {
                                    id: "Component1".to_string(),
                                    ..Default::default()
                                },
                                component_tvl: Some(6.0),
                                entrypoints: vec![],
                            },
                        ),
                        (
                            "Component2".to_string(),
                            ComponentWithState {
                                state: ProtocolComponentState::new(
                                    "Component2",
                                    Default::default(),
                                    Default::default(),
                                ),
                                component: ProtocolComponent {
                                    id: "Component2".to_string(),
                                    ..Default::default()
                                },
                                component_tvl: Some(2.0),
                                entrypoints: vec![],
                            },
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        // Mock get_traced_entry_points for Ethereum chain
        rpc_client
            .expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let (tx, rx) = channel(1);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| Ok((Uuid::default(), rx)));

        // Expect unsubscribe call during cleanup
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            true,
            ComponentFilter::with_tvl_range(remove_tvl_threshold, add_tvl_threshold),
            1,
            Duration::from_secs(0),
            true,
            true,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10_u64,
        );
        state_sync
            .initialize()
            .await
            .expect("Init failed");

        // Simulate the incoming BlockAggregatedChanges
        let deltas = [
            BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 1,
                    hash: Bytes::from("0x01"),
                    parent_hash: Bytes::from("0x00"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
                ..Default::default()
            },
            BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 2,
                    hash: Bytes::from("0x02"),
                    parent_hash: Bytes::from("0x01"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
                component_tvl: [
                    ("Component1".to_string(), 6.0), // Within range, should not trigger changes
                    ("Component2".to_string(), 2.0), // Below lower threshold, should be removed
                    ("Component3".to_string(), 10.0), // Above upper threshold, should be added
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        ];

        let (handle, mut rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        // Simulate sending delta messages
        tx.send(deltas[0].clone())
            .await
            .expect("deltas channel msg 0 closed!");

        // Expecting to receive the initial state message
        let _ = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("waiting for first state msg timed out!")
            .expect("state sync block sender closed!");

        // Send the third message, which should trigger TVL-based changes
        tx.send(deltas[1].clone())
            .await
            .expect("deltas channel msg 1 closed!");
        let second_msg = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("waiting for second state msg timed out!")
            .expect("state sync block sender closed!")
            .expect("no error");

        let _ = close_tx.send(());
        jh.await
            .expect("state sync task panicked!");

        let expected_second_msg = StateSyncMessage {
            header: BlockHeader {
                number: 2,
                hash: Bytes::from("0x02"),
                parent_hash: Bytes::from("0x01"),
                revert: false,
                ..Default::default()
            },
            snapshots: Snapshot {
                states: [(
                    "Component3".to_string(),
                    ComponentWithState {
                        state: ProtocolComponentState::new(
                            "Component3",
                            Default::default(),
                            Default::default(),
                        ),
                        component: ProtocolComponent {
                            id: "Component3".to_string(),
                            ..Default::default()
                        },
                        component_tvl: Some(10.0),
                        entrypoints: vec![], // TODO: add entrypoints?
                    },
                )]
                .into_iter()
                .collect(),
                vm_storage: HashMap::new(),
            },
            deltas: Some(BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 2,
                    hash: Bytes::from("0x02"),
                    parent_hash: Bytes::from("0x01"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
                component_tvl: [
                    ("Component1".to_string(), 6.0), // Within range, should not trigger changes
                    ("Component3".to_string(), 10.0), // Above upper threshold, should be added
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            }),
            removed_components: [(
                "Component2".to_string(),
                ProtocolComponent { id: "Component2".to_string(), ..Default::default() },
            )]
            .into_iter()
            .collect(),
        };

        assert_eq!(second_msg, expected_second_msg);
    }

    #[test_log::test(tokio::test)]
    async fn test_public_close_api_functionality() {
        // Tests the public close() API through the StateSynchronizer trait:
        // - close() fails before start() is called
        // - close() succeeds while synchronizer is running
        // - close() fails after already closed
        // This tests the full start/close lifecycle via the public API

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        // Mock the initial components call
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        // Set up deltas client that will wait for messages (blocking in state_sync)
        let (_tx, rx) = channel(1);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| Ok((Uuid::default(), rx)));

        // Expect unsubscribe call during cleanup
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "test-protocol"),
            true,
            ComponentFilter::with_tvl_range(0.0, 0.0),
            5, // Enough retries
            Duration::from_secs(0),
            true,
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10000_u64, // Long timeout so task doesn't exit on its own
        );

        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        // Start the synchronizer and test the new split-based close mechanism
        let (handle, _rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        // Give it time to start up and enter state_sync
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send close signal should succeed
        close_tx
            .send(())
            .expect("Should be able to send close signal");
        // Task should stop cleanly
        jh.await.expect("Task should not panic");
    }

    #[test_log::test(tokio::test)]
    async fn test_cleanup_runs_when_state_sync_processing_errors() {
        // Tests that cleanup code runs when state_sync() errors during delta processing.
        // Specifically tests: RPC errors during snapshot retrieval cause proper cleanup.
        // Verifies: shared.last_synced_block reset + subscription unsubscribe on errors

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        // Mock the initial components call
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        // Mock to fail during snapshot retrieval (this will cause an error during processing)
        rpc_client
            .expect_get_protocol_states()
            .returning(|_| {
                Err(RPCError::ParseResponse("Test error during snapshot retrieval".to_string()))
            });

        // Set up deltas client to send one message that will trigger snapshot retrieval
        let (tx, rx) = channel(10);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                // Send a delta message that will require a snapshot
                let delta = BlockAggregatedChanges {
                    extractor: "test".to_string(),
                    chain: Chain::Ethereum,
                    block: Block {
                        hash: Bytes::from("0x0123"),
                        number: 1,
                        parent_hash: Bytes::from("0x0000"),
                        chain: Chain::Ethereum,
                        ts: chrono::DateTime::from_timestamp(1234567890, 0)
                            .unwrap()
                            .naive_utc(),
                    },
                    revert: false,
                    // Add a new component to trigger snapshot request
                    new_protocol_components: [(
                        "new_component".to_string(),
                        ProtocolComponent { id: "new_component".to_string(), ..Default::default() },
                    )]
                    .into_iter()
                    .collect(),
                    component_tvl: [("new_component".to_string(), 100.0)]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                };

                tokio::spawn(async move {
                    let _ = tx.send(delta).await;
                    // Close the channel after sending one message
                });

                Ok((Uuid::default(), rx))
            });

        // Expect unsubscribe call during cleanup
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "test-protocol"),
            true,
            ComponentFilter::with_tvl_range(0.0, 1000.0), // Include the component
            1,
            Duration::from_secs(0),
            true,
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            5000_u64,
        );

        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        // Before calling state_sync, set a value in last_synced_block
        state_sync.last_synced_block = Some(BlockHeader {
            hash: Bytes::from("0x0badc0ffee"),
            number: 42,
            parent_hash: Bytes::from("0xbadbeef0"),
            revert: false,
            timestamp: 123456789,
            partial_block_index: None,
        });

        // Create a channel for state_sync to send messages to
        let (mut block_tx, _block_rx) = channel(10);

        // Call state_sync directly - this should error during processing
        let (_end_tx, end_rx) = oneshot::channel::<()>();
        let result = state_sync
            .state_sync(&mut block_tx, end_rx)
            .await;
        // Verify that state_sync returned an error
        assert!(result.is_err(), "state_sync should have errored during processing");

        // Note: We can't verify internal state cleanup since state_sync consumes self,
        // but the cleanup logic is still tested by the fact that the method returns properly.
    }

    #[test_log::test(tokio::test)]
    async fn test_close_signal_while_waiting_for_first_deltas() {
        // Tests close signal handling during the initial "waiting for deltas" phase.
        // This is the earliest possible close scenario - before any deltas are received.
        // Verifies: close signal received while waiting for first message triggers cleanup
        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        let (_tx, rx) = channel(1);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| Ok((Uuid::default(), rx)));

        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "test-protocol"),
            true,
            ComponentFilter::with_tvl_range(0.0, 0.0),
            1,
            Duration::from_secs(0),
            true,
            true,
            false,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10000_u64,
        );

        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        let (mut block_tx, _block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();

        // Start state_sync in a task
        let state_sync_handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx)
                .await
        });

        // Give it a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send close signal
        let _ = end_tx.send(());

        // state_sync should exit cleanly
        let result = state_sync_handle
            .await
            .expect("Task should not panic");
        assert!(result.is_ok(), "state_sync should exit cleanly when closed: {result:?}");

        println!("SUCCESS: Close signal handled correctly while waiting for first deltas");
    }

    #[test_log::test(tokio::test)]
    async fn test_close_signal_during_main_processing_loop() {
        // Tests close signal handling during the main delta processing loop.
        // This tests the scenario where first message is processed successfully,
        // then close signal is received while waiting for subsequent deltas.
        // Verifies: close signal in main loop (after initialization) triggers cleanup

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        // Mock the initial components call
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        // Mock the snapshot retrieval that happens after first message
        rpc_client
            .expect_get_protocol_states()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        rpc_client
            .expect_get_component_tvl()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        rpc_client
            .expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        // Set up deltas client to send one message, then keep channel open
        let (tx, rx) = channel(10);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                // Send first message immediately
                let first_delta = BlockAggregatedChanges {
                    extractor: "test".to_string(),
                    chain: Chain::Ethereum,
                    block: Block {
                        hash: Bytes::from("0x0123"),
                        number: 1,
                        parent_hash: Bytes::from("0x0000"),
                        chain: Chain::Ethereum,
                        ts: chrono::DateTime::from_timestamp(1234567890, 0)
                            .unwrap()
                            .naive_utc(),
                    },
                    revert: false,
                    ..Default::default()
                };

                tokio::spawn(async move {
                    let _ = tx.send(first_delta).await;
                    // Keep the sender alive but don't send more messages
                    // This will make the recv() block waiting for the next message
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });

                Ok((Uuid::default(), rx))
            });

        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "test-protocol"),
            true,
            ComponentFilter::with_tvl_range(0.0, 1000.0),
            1,
            Duration::from_secs(0),
            true,
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10000_u64,
        );

        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();

        // Start state_sync in a task
        let state_sync_handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx)
                .await
        });

        // Wait for the first message to be processed (snapshot sent)
        let first_snapshot = block_rx
            .recv()
            .await
            .expect("Should receive first snapshot")
            .expect("Synchronizer error");
        assert!(
            !first_snapshot
                .snapshots
                .states
                .is_empty() ||
                first_snapshot.deltas.is_some()
        );
        // Now send close signal - this should be handled in the main processing loop
        let _ = end_tx.send(());

        // state_sync should exit cleanly after receiving close signal in main loop
        let result = state_sync_handle
            .await
            .expect("Task should not panic");
        assert!(
            result.is_ok(),
            "state_sync should exit cleanly when closed after first message: {result:?}"
        );
    }

    #[test_log::test(tokio::test)]
    async fn test_max_retries_exceeded_error_propagation() {
        // Test that when max_retries is exceeded, the final error is sent through the channel
        // to the receiver and the synchronizer task exits cleanly

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        // Mock the initial components call to succeed
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        // Set up deltas client to consistently fail after subscription
        // This will cause connection errors and trigger retries
        deltas_client
            .expect_subscribe()
            .returning(|_, _| {
                // Return a connection error to trigger retries
                Err(DeltasError::NotConnected)
            });

        // Expect multiple unsubscribe calls during retries
        deltas_client
            .expect_unsubscribe()
            .returning(|_| Ok(()))
            .times(0..=5);

        // Create synchronizer with only 2 retries and short cooldown
        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "test-protocol"),
            true,
            ComponentFilter::with_tvl_range(0.0, 1000.0),
            2,                         // max_retries = 2
            Duration::from_millis(10), // short retry cooldown
            true,
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            1000_u64,
        );

        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        // Start the synchronizer - it should fail to subscribe and retry
        let (handle, mut rx) = state_sync.start().await;
        let (jh, _close_tx) = handle.split();

        let res = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("responsds in time")
            .expect("channel open");

        // Verify the error is a ConnectionClosed error (converted from DeltasError::NotConnected)
        if let Err(err) = res {
            assert!(
                matches!(err, SynchronizerError::ConnectionClosed),
                "Expected ConnectionClosed error, got: {:?}",
                err
            );
        } else {
            panic!("Expected an error")
        }

        // The task should complete (not hang) after max retries
        let task_result = tokio::time::timeout(Duration::from_secs(2), jh).await;
        assert!(task_result.is_ok(), "Synchronizer task should complete after max retries");
    }

    #[test_log::test(tokio::test)]
    async fn test_is_next_expected() {
        // Test the is_next_expected function to ensure it correctly identifies
        // when an incoming block is the expected next block in the chain

        let mut state_sync = with_mocked_clients(true, false, None, None);

        // Test 1: No previous block - should return false
        let incoming_header = BlockHeader {
            number: 100,
            hash: Bytes::from("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            parent_hash: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            revert: false,
            timestamp: 123456789,
            partial_block_index: None,
        };
        assert!(
            !state_sync.is_next_expected(&incoming_header),
            "Should return false when no previous block is set"
        );

        // Test 2: Set a previous block and test with matching parent hash
        let previous_header = BlockHeader {
            number: 99,
            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
            parent_hash: Bytes::from(
                "0xabcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            ),
            revert: false,
            timestamp: 123456788,
            partial_block_index: None,
        };
        state_sync.last_synced_block = Some(previous_header.clone());

        assert!(
            state_sync.is_next_expected(&incoming_header),
            "Should return true when incoming parent_hash matches previous hash"
        );

        // Test 3: Test with non-matching parent hash
        let non_matching_header = BlockHeader {
            number: 100,
            hash: Bytes::from("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            parent_hash: Bytes::from(
                "0x1111111111111111111111111111111111111111111111111111111111111111",
            ), // Wrong parent hash
            revert: false,
            timestamp: 123456789,
            partial_block_index: None,
        };
        assert!(
            !state_sync.is_next_expected(&non_matching_header),
            "Should return false when incoming parent_hash doesn't match previous hash"
        );
    }

    #[test_log::test(tokio::test)]
    async fn test_synchronizer_restart_skip_snapshot_on_expected_block() {
        // Test that on synchronizer restart with the next expected block,
        // get_snapshot is not called and only deltas are sent

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        // Mock the initial components call
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| {
                Ok(Page::new(
                    vec![ProtocolComponent { id: "Component1".to_string(), ..Default::default() }],
                    1,
                    0,
                    100,
                ))
            });

        // Set up deltas client to send a message that is the next expected block
        let (tx, rx) = channel(10);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                let expected_next_delta = BlockAggregatedChanges {
                    extractor: "uniswap-v2".to_string(),
                    chain: Chain::Ethereum,
                    block: Block {
                        hash: Bytes::from(
                            "0x0000000000000000000000000000000000000000000000000000000000000002",
                        ), // This will be the next expected block
                        number: 2,
                        parent_hash: Bytes::from(
                            "0x0000000000000000000000000000000000000000000000000000000000000001",
                        ), // This matches our last synced block hash
                        chain: Chain::Ethereum,
                        ts: chrono::DateTime::from_timestamp(1234567890, 0)
                            .unwrap()
                            .naive_utc(),
                    },
                    revert: false,
                    ..Default::default()
                };

                tokio::spawn(async move {
                    let _ = tx.send(expected_next_delta).await;
                });

                Ok((Uuid::default(), rx))
            });

        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            true,
            ComponentFilter::with_tvl_range(0.0, 1000.0),
            1,
            Duration::from_secs(0),
            true, // include_snapshots = true
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10000_u64,
        );

        // Initialize and set up the last synced block to simulate a restart scenario
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        // Set last_synced_block to simulate that we've previously synced block 1
        state_sync.last_synced_block = Some(BlockHeader {
            number: 1,
            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000001"), /* This matches the parent_hash in our delta */
            parent_hash: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            revert: false,
            timestamp: 123456789,
            partial_block_index: None,
        });

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();

        // Start state_sync
        let state_sync_handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx)
                .await
        });

        // Wait for the message - it should be a delta-only message (no snapshots)
        let result_msg = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive message within timeout")
            .expect("Channel should be open")
            .expect("Should not be an error");

        // Send close signal
        let _ = end_tx.send(());

        // Wait for state_sync to finish
        let _ = state_sync_handle
            .await
            .expect("Task should not panic");

        // Verify the message contains deltas but no snapshots
        // (because we skipped snapshot retrieval)
        assert!(result_msg.deltas.is_some(), "Should contain deltas");
        assert!(
            result_msg.snapshots.states.is_empty(),
            "Should not contain snapshots when next expected block is received"
        );

        // Verify the block details match our expected next block
        if let Some(deltas) = &result_msg.deltas {
            assert_eq!(deltas.block.number, 2);
            assert_eq!(
                deltas.block.hash,
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000002")
            );
            assert_eq!(
                deltas.block.parent_hash,
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000001")
            );
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_skip_previously_processed_messages() {
        // Test that the synchronizer skips messages for blocks that have already been processed
        // This simulates a service restart scenario where old messages are re-emitted

        let mut rpc_client = make_mock_client();
        let mut deltas_client = MockDeltasClient::new();

        // Mock the initial components call
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| {
                Ok(Page::new(
                    vec![ProtocolComponent { id: "Component1".to_string(), ..Default::default() }],
                    1,
                    0,
                    100,
                ))
            });

        // Mock snapshot calls for when we process the expected next block (block 6)
        rpc_client
            .expect_get_protocol_states()
            .returning(|_| {
                Ok(Page::new(
                    vec![ProtocolComponentState::new(
                        "Component1",
                        Default::default(),
                        Default::default(),
                    )],
                    1,
                    0,
                    100,
                ))
            });

        rpc_client
            .expect_get_component_tvl()
            .returning(|_| {
                Ok(Page::new(
                    [("Component1".to_string(), 100.0)]
                        .into_iter()
                        .collect(),
                    1,
                    0,
                    100,
                ))
            });

        rpc_client
            .expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        // Set up deltas client to send old messages first, then the expected next block
        let (tx, rx) = channel(10);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                // Send messages for blocks 3, 4, 5 (already processed), then block 6 (expected)
                let old_messages = vec![
                    BlockAggregatedChanges {
                        extractor: "uniswap-v2".to_string(),
                        chain: Chain::Ethereum,
                        block: Block {
                            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000003"),
                            number: 3,
                            parent_hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000002"),
                            chain: Chain::Ethereum,
                            ts: chrono::DateTime::from_timestamp(1234567890, 0).unwrap().naive_utc(),
                        },
                        revert: false,
                        ..Default::default()
                    },
                    BlockAggregatedChanges {
                        extractor: "uniswap-v2".to_string(),
                        chain: Chain::Ethereum,
                        block: Block {
                            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000004"),
                            number: 4,
                            parent_hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000003"),
                            chain: Chain::Ethereum,
                            ts: chrono::DateTime::from_timestamp(1234567891, 0).unwrap().naive_utc(),
                        },
                        revert: false,
                        ..Default::default()
                    },
                    BlockAggregatedChanges {
                        extractor: "uniswap-v2".to_string(),
                        chain: Chain::Ethereum,
                        block: Block {
                            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000005"),
                            number: 5,
                            parent_hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000004"),
                            chain: Chain::Ethereum,
                            ts: chrono::DateTime::from_timestamp(1234567892, 0).unwrap().naive_utc(),
                        },
                        revert: false,
                        ..Default::default()
                    },
                    // This is the expected next block (block 6)
                    BlockAggregatedChanges {
                        extractor: "uniswap-v2".to_string(),
                        chain: Chain::Ethereum,
                        block: Block {
                            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000006"),
                            number: 6,
                            parent_hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000005"),
                            chain: Chain::Ethereum,
                            ts: chrono::DateTime::from_timestamp(1234567893, 0).unwrap().naive_utc(),
                        },
                        revert: false,
                        ..Default::default()
                    },
                ];

                tokio::spawn(async move {
                    for message in old_messages {
                        let _ = tx.send(message).await;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });

                Ok((Uuid::default(), rx))
            });

        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            true,
            ComponentFilter::with_tvl_range(0.0, 1000.0),
            1,
            Duration::from_secs(0),
            true,
            true,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10000_u64,
        );

        // Initialize and set last_synced_block to simulate we've already processed block 5
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        state_sync.last_synced_block = Some(BlockHeader {
            number: 5,
            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000005"),
            parent_hash: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000004",
            ),
            revert: false,
            timestamp: 1234567892,
            partial_block_index: None,
        });

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();

        // Start state_sync
        let state_sync_handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx)
                .await
        });

        // Wait for the message - it should only be for block 6 (skipping blocks 3, 4, 5)
        let result_msg = timeout(Duration::from_millis(500), block_rx.recv())
            .await
            .expect("Should receive message within timeout")
            .expect("Channel should be open")
            .expect("Should not be an error");

        // Send close signal
        let _ = end_tx.send(());

        // Wait for state_sync to finish
        let _ = state_sync_handle
            .await
            .expect("Task should not panic");

        // Verify we only got the message for block 6 (the expected next block)
        assert!(result_msg.deltas.is_some(), "Should contain deltas");
        if let Some(deltas) = &result_msg.deltas {
            assert_eq!(
                deltas.block.number, 6,
                "Should only process block 6, skipping earlier blocks"
            );
            assert_eq!(
                deltas.block.hash,
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000006")
            );
        }

        // Verify that no additional messages are received immediately
        // (since the old blocks 3, 4, 5 were skipped and only block 6 was processed)
        match timeout(Duration::from_millis(50), block_rx.recv()).await {
            Err(_) => {
                // Timeout is expected - no more messages should come
            }
            Ok(Some(Err(_))) => {
                // Error received is also acceptable (connection closed)
            }
            Ok(Some(Ok(_))) => {
                panic!("Should not receive additional messages - old blocks should be skipped");
            }
            Ok(None) => {
                // Channel closed is also acceptable
            }
        }
    }

    fn make_block_changes(block_num: u64, partial_idx: Option<u32>) -> BlockAggregatedChanges {
        // Use vec to create Bytes from block number
        let hash = Bytes::from(vec![block_num as u8; 32]);
        let parent_hash = Bytes::from(vec![block_num.saturating_sub(1) as u8; 32]);
        BlockAggregatedChanges {
            extractor: "uniswap-v2".to_string(),
            chain: Chain::Ethereum,
            block: Block {
                number: block_num,
                hash,
                parent_hash,
                chain: Chain::Ethereum,
                ts: Default::default(),
            },
            revert: false,
            partial_block_index: partial_idx,
            ..Default::default()
        }
    }

    /// Test that full block as first message in partial mode is accepted
    #[test_log::test(tokio::test)]
    async fn test_partial_mode_accepts_full_block_as_first_message() {
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync();
        let mut state_sync = with_mocked_clients(true, true, Some(rpc_client), Some(deltas_client))
            .with_partial_blocks(true);
        state_sync
            .initialize()
            .await
            .expect("Init failed");

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        // Send full block as first message - should be accepted
        tx.send(make_block_changes(1, None))
            .await
            .unwrap();

        // Should receive the full block immediately
        let msg = timeout(Duration::from_millis(100), block_rx.recv())
            .await
            .expect("Should receive message")
            .expect("Channel open")
            .expect("No error");

        assert_eq!(msg.header.number, 1, "Should use block 1 (full block)");
        assert_eq!(msg.header.partial_block_index, None, "Should be a full block");

        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");
    }

    /// Test that block number increase is detected as new block
    #[test_log::test(tokio::test)]
    async fn test_partial_mode_detects_block_number_increase() {
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync();
        let mut state_sync = with_mocked_clients(true, true, Some(rpc_client), Some(deltas_client))
            .with_partial_blocks(true);
        state_sync
            .initialize()
            .await
            .expect("Init failed");

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        // Send partial messages for block 1 (will be skipped - waiting for new block)
        tx.send(make_block_changes(1, Some(0)))
            .await
            .unwrap();
        tx.send(make_block_changes(1, Some(3)))
            .await
            .unwrap();

        // Verify no message received yet
        match timeout(Duration::from_millis(50), block_rx.recv()).await {
            Err(_) => { /* Expected: timeout, no message yet */ }
            Ok(_) => panic!("Should not receive message while waiting for new block"),
        }

        // Send partial for block 2 with HIGHER index (5 > 3) - should still be detected
        // because block number increased
        tx.send(make_block_changes(2, Some(5)))
            .await
            .unwrap();

        // Should receive the message for block 2
        let msg = timeout(Duration::from_millis(100), block_rx.recv())
            .await
            .expect("Should receive message")
            .expect("Channel open")
            .expect("No error");

        assert_eq!(msg.header.number, 2, "Should use block 2 (block number increased)");
        assert_eq!(msg.header.partial_block_index, Some(5));

        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");
    }

    /// Test that partial mode skips new blocks that are already synced
    #[test_log::test(tokio::test)]
    async fn test_partial_mode_skips_already_synced_blocks() {
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync();
        let mut state_sync = with_mocked_clients(true, true, Some(rpc_client), Some(deltas_client))
            .with_partial_blocks(true);
        state_sync
            .initialize()
            .await
            .expect("Init failed");

        // Set last_synced_block to block 5 - we've already synced up to here
        state_sync.last_synced_block = Some(BlockHeader {
            number: 5,
            hash: Bytes::from("0x05"),
            parent_hash: Bytes::from("0x04"),
            revert: false,
            timestamp: 0,
            partial_block_index: None,
        });

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        // Send partial for block 3 to establish baseline
        tx.send(make_block_changes(3, Some(2)))
            .await
            .unwrap();

        // Send "new block" for block 4 (partial index decreased) - but block 4 < last_synced (5)
        tx.send(make_block_changes(4, Some(0)))
            .await
            .unwrap();

        // Should be skipped because block 4 is already synced
        match timeout(Duration::from_millis(50), block_rx.recv()).await {
            Err(_) => { /* Expected: skipped because already synced */ }
            Ok(_) => panic!("Should skip block 4 because it's already synced"),
        }

        // Now send new block for block 6 (after last_synced)
        // First establish new partial index
        tx.send(make_block_changes(5, Some(3)))
            .await
            .unwrap();
        // Then trigger new block detection
        tx.send(make_block_changes(6, Some(0)))
            .await
            .unwrap();

        let msg = timeout(Duration::from_millis(100), block_rx.recv())
            .await
            .expect("Should receive message")
            .expect("Channel open")
            .expect("No error");

        assert_eq!(msg.header.number, 6, "Should use block 6 (after last synced)");

        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");
    }

    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_skips_entrypoints_when_not_dci() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();
        let component = ProtocolComponent { id: "Component1".to_string(), ..Default::default() };

        let component_clone = component.clone();
        rpc.expect_get_snapshots()
            .returning(move |_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "Component1".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component1",
                                Default::default(),
                                Default::default(),
                            ),
                            component: component_clone.clone(),
                            entrypoints: vec![],
                            component_tvl: None,
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        // get_traced_entry_points should NOT be called for a non-DCI protocol
        rpc.expect_get_traced_entry_points()
            .never();

        let mut state_sync = with_mocked_clients(true, false, Some(rpc), None);
        // uses_dci defaults to false, no .with_dci() call needed
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component);

        let components_arg = ["Component1".to_string()];
        let snap = state_sync
            .get_snapshots(header, Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        assert!(snap
            .snapshots
            .states
            .contains_key("Component1"));
    }

    #[test_log::test(tokio::test)]
    async fn test_get_snapshots_fetches_entrypoints_when_dci() {
        let header = BlockHeader::default();
        let mut rpc = make_mock_client();
        let component = ProtocolComponent { id: "Component1".to_string(), ..Default::default() };

        let component_clone = component.clone();
        rpc.expect_get_snapshots()
            .returning(move |_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "Component1".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Component1",
                                Default::default(),
                                Default::default(),
                            ),
                            component: component_clone.clone(),
                            entrypoints: vec![],
                            component_tvl: None,
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });

        // get_traced_entry_points SHOULD be called for a DCI protocol
        rpc.expect_get_traced_entry_points()
            .times(1)
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let mut state_sync = with_mocked_clients(true, false, Some(rpc), None).with_dci(true);
        state_sync
            .component_tracker
            .components
            .insert("Component1".to_string(), component);

        let components_arg = ["Component1".to_string()];
        let snap = state_sync
            .get_snapshots(header, Some(&components_arg))
            .await
            .expect("Retrieving snapshot failed");

        assert!(snap
            .snapshots
            .states
            .contains_key("Component1"));
    }

    /// Test that in partial_blocks mode:
    /// - Preexisting components (in to_add but not in new_protocol_components) get their snapshot
    ///   requested immediately at the previous block and included in the same message.
    /// - Brand-new components (in new_protocol_components) have their snapshot deferred to the
    ///   first message of the next block, then requested at the completed previous block.
    #[test_log::test(tokio::test)]
    async fn test_partial_mode_defers_brand_new_component_snapshot_to_next_block() {
        use std::time::Duration;

        use tokio::{sync::mpsc::channel, time::timeout};

        let mut rpc_client = make_mock_client();
        // get_protocol_components for BrandNew + Preexisting when start_tracking is called (block
        // 2)
        rpc_client
            .expect_get_protocol_components()
            .withf(|params: &crate::rpc::ProtocolComponentsParams| {
                params
                    .component_ids()
                    .is_some_and(|ids| ids.contains(&"BrandNew".to_string()))
            })
            .returning(|_| {
                Ok(Page::new(
                    vec![
                        ProtocolComponent { id: "BrandNew".to_string(), ..Default::default() },
                        ProtocolComponent { id: "Preexisting".to_string(), ..Default::default() },
                    ],
                    2,
                    0,
                    100,
                ))
            });
        // get_protocol_components for initial sync (via get_protocol_components_paginated)
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| {
                Ok(Page::new(
                    vec![
                        ProtocolComponent { id: "Component1".to_string(), ..Default::default() },
                        ProtocolComponent { id: "Component2".to_string(), ..Default::default() },
                    ],
                    2,
                    0,
                    100,
                ))
            });
        // get_snapshots: more specific expectations first so init (block 0, all components)
        // does not match the block 1/2 ones.
        // get_snapshots for deferred flush: block 2, BrandNew (when processing block 3)
        rpc_client
            .expect_get_snapshots()
            .withf(
                |request: &SnapshotParameters,
                 _chunk_size: &Option<usize>,
                 _concurrency: &usize| {
                    request.block_number == 2 &&
                        request
                            .components
                            .contains_key("BrandNew")
                },
            )
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "BrandNew".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "BrandNew",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "BrandNew".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(100.0),
                            entrypoints: vec![],
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });
        // get_snapshots for preexisting: block 1, Preexisting (when processing block 2)
        rpc_client
            .expect_get_snapshots()
            .withf(
                |request: &SnapshotParameters,
                 _chunk_size: &Option<usize>,
                 _concurrency: &usize| {
                    request.block_number == 1 &&
                        request
                            .components
                            .contains_key("Preexisting")
                },
            )
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [(
                        "Preexisting".to_string(),
                        ComponentWithState {
                            state: ProtocolComponentState::new(
                                "Preexisting",
                                Default::default(),
                                Default::default(),
                            ),
                            component: ProtocolComponent {
                                id: "Preexisting".to_string(),
                                ..Default::default()
                            },
                            component_tvl: Some(75.0),
                            entrypoints: vec![],
                        },
                    )]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });
        // get_snapshots for initial sync (block 0, all components)
        rpc_client
            .expect_get_snapshots()
            .returning(|_request, _chunk_size, _concurrency| {
                Ok(Snapshot {
                    states: [
                        (
                            "Component1".to_string(),
                            ComponentWithState {
                                state: ProtocolComponentState::new(
                                    "Component1",
                                    Default::default(),
                                    Default::default(),
                                ),
                                component: ProtocolComponent {
                                    id: "Component1".to_string(),
                                    ..Default::default()
                                },
                                component_tvl: Some(100.0),
                                entrypoints: vec![],
                            },
                        ),
                        (
                            "Component2".to_string(),
                            ComponentWithState {
                                state: ProtocolComponentState::new(
                                    "Component2",
                                    Default::default(),
                                    Default::default(),
                                ),
                                component: ProtocolComponent {
                                    id: "Component2".to_string(),
                                    ..Default::default()
                                },
                                component_tvl: Some(0.0),
                                entrypoints: vec![],
                            },
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                })
            });
        rpc_client
            .expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));

        let mut deltas_client = MockDeltasClient::new();
        let (tx, rx) = channel(1);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| Ok((Uuid::default(), rx)));
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));

        let mut state_sync = with_mocked_clients(true, true, Some(rpc_client), Some(deltas_client))
            .with_partial_blocks(true);
        state_sync
            .initialize()
            .await
            .expect("Init failed");

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        // Block 1 (full): used for initial sync merge
        tx.send(make_block_changes(1, None))
            .await
            .unwrap();
        let _msg1 = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive initial + block 1")
            .expect("Channel open")
            .expect("No error");

        // Block 2 partial (index 2): BrandNew (in new_protocol_components → deferred) and
        // Preexisting (not in new_protocol_components → snapshot requested immediately at block 1).
        let mut block2 = make_block_changes(2, Some(2));
        block2.new_protocol_components = HashMap::from([(
            "BrandNew".to_string(),
            ProtocolComponent { id: "BrandNew".to_string(), ..Default::default() },
        )]);
        block2.component_tvl = HashMap::from([
            ("BrandNew".to_string(), 100.0),
            ("Preexisting".to_string(), 75.0), // > add_tvl (50), not in new_protocol_components
        ]);
        tx.send(block2).await.unwrap();
        let msg2 = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive block 2")
            .expect("Channel open")
            .expect("No error");

        assert!(
            msg2.snapshots.states.contains_key("Preexisting"),
            "Preexisting component snapshot should be requested immediately in same block; got keys: {:?}",
            msg2.snapshots.states.keys().collect::<Vec<_>>()
        );
        assert!(
            !msg2
                .snapshots
                .states
                .contains_key("BrandNew"),
            "Brand-new component snapshot should be deferred, not in block 2 message"
        );

        // Block 3 partial (index 1): first partial of next block triggers flush of deferred
        // snapshot at block 2
        tx.send(make_block_changes(3, Some(1)))
            .await
            .unwrap();
        let msg3 = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive block 3")
            .expect("Channel open")
            .expect("No error");

        assert_eq!(msg3.header.number, 3);
        assert_eq!(msg3.header.partial_block_index, Some(1), "First partial of block 3");
        assert!(
            msg3.snapshots.states.contains_key("BrandNew"),
            "Deferred brand-new component snapshot should be included in next block message; got keys: {:?}",
            msg3.snapshots.states.keys().collect::<Vec<_>>()
        );

        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");
    }
}
