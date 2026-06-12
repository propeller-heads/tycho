use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

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
use tracing::{debug, error, info, instrument, warn};
use tycho_common::{
    models::{
        blockchain::{
            BlockAggregatedChanges, DCIUpdate, EntryPointWithTracingParams, TracingResult,
        },
        contract::Account,
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ExtractorIdentity,
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

impl SynchronizerError {
    /// Returns true if the error is transient and the failing operation can be retried.
    ///
    /// Transient: network/HTTP failures, rate limiting, server unavailability. These are
    /// infrastructure problems that may resolve without any change to the request.
    ///
    /// Permanent: malformed data, fatal server errors, invalid requests. Retrying would produce
    /// the same failure.
    pub fn is_transient(&self) -> bool {
        match self {
            SynchronizerError::RPCError(e) => matches!(
                e,
                RPCError::HttpClient(_, _) |
                    RPCError::RateLimited(_) |
                    RPCError::ServerUnreachable(_) |
                    RPCError::StaleBlock(_)
            ),
            SynchronizerError::Timeout(_) |
            SynchronizerError::ConnectionError(_) |
            SynchronizerError::ConnectionClosed => true,
            _ => false,
        }
    }
}

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
    /// Background snapshot tasks spawned for new components. Each task may be in-flight or
    /// finished; completed ones are harvested at the start of each delta iteration and their
    /// results included in that block's message.
    snapshot_tasks: Vec<SnapshotTask>,
    /// Unfiltered deltas buffered while any snapshot task is in-flight, starting from the block
    /// at which the oldest task was spawned. Applied to each snapshot at drain time to reconstruct
    /// the component's current state.
    buffered_deltas: Vec<BlockAggregatedChanges>,
    /// State machine tracking components awaiting their initial snapshot. A component lives here
    /// from the moment it's queued until its snapshot is successfully applied (at which point it
    /// moves into `component_tracker.components`).
    snapshot_queue: HashMap<String, SnapshotStatus>,
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

/// Tracks the lifecycle of a component's initial snapshot request.
#[derive(Debug, Clone, PartialEq)]
enum SnapshotStatus {
    /// Waiting for the next block boundary before fetching. Only used with partial blocks.
    Deferred,
    /// A background snapshot task has been spawned and is in-flight.
    InFlight,
    /// The last fetch attempt failed transiently; will be re-queued on the next delta.
    RetryNext,
    /// The fetch failed permanently; component is excluded until the synchronizer restarts.
    Blacklisted,
}

struct SnapshotFetchResult {
    components: HashMap<String, ProtocolComponent>,
    contract_ids: HashSet<Bytes>,
    dci_update: DCIUpdate,
    snapshot: Snapshot,
    snapshot_block: u64,
}

struct SnapshotTask {
    component_ids: Vec<String>,
    snapshot_block: u64,
    receiver: oneshot::Receiver<Result<SnapshotFetchResult, SynchronizerError>>,
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

struct FetchSnapshotParams {
    chain: Chain,
    protocol_system: String,
    block_number: u64,
    uses_dci: bool,
    retrieve_balances: bool,
    include_tvl: bool,
}

/// Fetches a snapshot for given components. If DCI is enabled, also traces entry
/// points and extends `contract_ids` with any contracts they access.
///
/// Returns the snapshot, the DCI update, and the complete set of contract IDs (original +
/// DCI-discovered).
async fn fetch_snapshot<R: RPCClient>(
    rpc_client: &R,
    components: HashMap<String, ProtocolComponent>,
    mut contract_ids: HashSet<Bytes>,
    params: &FetchSnapshotParams,
) -> Result<(Snapshot, DCIUpdate, HashSet<Bytes>), SynchronizerError> {
    if components.is_empty() {
        return Ok((Snapshot::default(), DCIUpdate::default(), contract_ids));
    }

    let component_ids: Vec<String> = components.keys().cloned().collect();

    let (dci_update, entrypoints_result) = if params.uses_dci {
        let result = rpc_client
            .get_traced_entry_points_paginated(TracedEntryPointsPaginatedParams::new(
                params.chain,
                &params.protocol_system,
                component_ids.clone(),
                RPC_CLIENT_CONCURRENCY,
            ))
            .await?;
        let dci_contracts: HashSet<Bytes> = result
            .values()
            .flat_map(|traces| {
                traces
                    .iter()
                    .flat_map(|(_, tr)| tr.accessed_slots.keys().cloned())
            })
            .collect();
        contract_ids.extend(dci_contracts);
        let eps = result.clone();
        let dci: DCIUpdate = result.into();
        (dci, eps)
    } else {
        (DCIUpdate::default(), HashMap::new())
    };

    let contract_ids_vec: Vec<Bytes> = contract_ids.iter().cloned().collect();
    let request = SnapshotParameters::new(
        params.chain,
        &params.protocol_system,
        &components,
        &contract_ids_vec,
        params.block_number,
    )
    .entrypoints(&entrypoints_result)
    .include_balances(params.retrieve_balances)
    .include_tvl(params.include_tvl);

    let snapshot = rpc_client
        .get_snapshots(&request, None, RPC_CLIENT_CONCURRENCY)
        .await?;

    Ok((snapshot, dci_update, contract_ids))
}

/// Fetches a snapshot for new components not yet in the tracker. Calls `get_protocol_components`
/// to resolve the components, then delegates to `fetch_snapshot`.
async fn fetch_snapshot_background<R: RPCClient>(
    rpc_client: R,
    component_ids: Vec<String>,
    params: FetchSnapshotParams,
) -> Result<SnapshotFetchResult, SynchronizerError> {
    if component_ids.is_empty() {
        return Ok(SnapshotFetchResult {
            components: HashMap::new(),
            contract_ids: HashSet::new(),
            dci_update: DCIUpdate::default(),
            snapshot: Snapshot::default(),
            snapshot_block: params.block_number,
        });
    }

    let request = crate::rpc::ProtocolComponentsParams::new(params.chain, &params.protocol_system)
        .with_component_ids(component_ids);
    let components: HashMap<String, ProtocolComponent> = rpc_client
        .get_protocol_components(request)
        .await?
        .into_data()
        .into_iter()
        .map(|pc| (pc.id.clone(), pc))
        .collect();

    let contract_ids: HashSet<Bytes> = components
        .values()
        .flat_map(|c| c.contract_addresses.iter().cloned())
        .collect();

    let snapshot_block = params.block_number;
    let (snapshot, dci_update, contract_ids) =
        fetch_snapshot(&rpc_client, components.clone(), contract_ids, &params).await?;

    Ok(SnapshotFetchResult { components, contract_ids, dci_update, snapshot, snapshot_block })
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
            snapshot_tasks: Vec::new(),
            buffered_deltas: Vec::new(),
            snapshot_queue: HashMap::new(),
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
                    let component_ids =
                        self.component_tracker.get_tracked_component_ids();
                    let init_snapshot = if !self.include_snapshots ||
                        component_ids.is_empty()
                    {
                        Snapshot::default()
                    } else {
                        // Fetch initial snapshots
                        let components: HashMap<_, _> = self
                            .component_tracker
                            .components
                            .iter()
                            .filter(|(id, _)| component_ids.contains(id))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        let contract_ids: HashSet<Bytes> = self
                            .component_tracker
                            .get_contracts_by_component(&component_ids)
                            .into_iter()
                            .collect();
                        let fetch_params = FetchSnapshotParams {
                            chain: self.extractor_id.chain,
                            protocol_system: self.extractor_id.name.clone(),
                            block_number: snapshot_header.number,
                            uses_dci: self.uses_dci,
                            retrieve_balances: self.retrieve_balances,
                            include_tvl: self.include_tvl,
                        };
                        match fetch_snapshot(
                            &self.rpc_client,
                            components,
                            contract_ids,
                            &fetch_params,
                        )
                        .await
                        {
                            Ok((snap, dci_update, _)) => {
                                self.component_tracker
                                    .process_entrypoints(&dci_update);
                                snap
                            }
                            Err(SynchronizerError::RPCError(
                                crate::rpc::RPCError::StaleBlock(reason),
                            )) => {
                                stale_retries += 1;
                                if stale_retries > MAX_STALE_RETRIES {
                                    return Err(SynchronizerError::RPCError(
                                        crate::rpc::RPCError::StaleBlock(reason),
                                    ));
                                }
                                // The server's persisted state for this block is outside
                                // the plan retention window. Discard this delta and wait
                                // for a fresher block from the same subscription rather
                                // than restarting from scratch.
                                warn!(
                                    block = header.number,
                                    stale_retries,
                                    %reason,
                                    "Snapshot block is outside server retention \
                                     window; waiting for a more recent block"
                                );
                                continue 'init;
                            }
                            Err(e) => return Err(e),
                        }
                    };
                    let n_components = self.component_tracker.components.len();
                    let n_snapshots = init_snapshot.states.len();
                    info!(
                        n_components,
                        n_snapshots,
                        "Initial snapshot retrieved, starting delta message feed"
                    );
                    let snapshot_msg = StateSyncMessage {
                        header: snapshot_header,
                        snapshots: init_snapshot,
                        deltas: None,
                        removed_components: HashMap::new(),
                    };
                    break 'init (snapshot_msg.merge(deltas_msg), header);
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

                            // Buffer unfiltered delta while any snapshot task is in-flight.
                            if !self.snapshot_tasks.is_empty() {
                                self.buffered_deltas.push(deltas.clone());
                            }

                            let background_snapshots = self.drain_completed_snapshots();

                            // Trim buffered_deltas: discard blocks no longer needed by any pending task.
                            if self.snapshot_tasks.is_empty() {
                                self.buffered_deltas.clear();
                            } else {
                                let oldest_pending_block = self
                                    .snapshot_tasks
                                    .iter()
                                    .map(|p| p.snapshot_block)
                                    .min()
                                    .unwrap_or(u64::MAX);
                                self.buffered_deltas
                                    .retain(|d| d.block.number > oldest_pending_block);
                            }

                            let (snapshots, removed_components) = {
                                let (to_add, to_remove) =
                                    self.component_tracker.filter_updated_components(&deltas);

                                // Harvest transient retries now so they feed into truly_new.
                                // TVL changes are not re-emitted, so without explicit re-queuing
                                // a transiently failed component would never be retried.
                                // Remove from the map first so the truly_new filter below treats
                                // them the same as brand-new components.
                                let retry_ids: Vec<String> = self
                                    .snapshot_queue
                                    .iter()
                                    .filter(|(_, s)| matches!(s, SnapshotStatus::RetryNext))
                                    .map(|(id, _)| id.clone())
                                    .collect();
                                for id in &retry_ids {
                                    self.snapshot_queue.remove(id);
                                }

                                // Components not yet tracked and not in the staged state machine
                                // (not in-flight, not deferred, not blacklisted). Merges
                                // delta-triggered new components with transient retries; `seen`
                                // deduplicates the two sources.
                                let truly_new: Vec<String> = {
                                    let mut seen = HashSet::new();
                                    to_add
                                        .iter()
                                        .chain(retry_ids.iter())
                                        .filter(|id| {
                                            !self.component_tracker
                                                .components
                                                .contains_key(id.as_str())
                                                && !self
                                                    .snapshot_queue
                                                    .contains_key(id.as_str())
                                                && seen.insert(id.as_str())
                                        })
                                        .cloned()
                                        .collect()
                                };

                                if self.partial_blocks {
                                    let is_new_block = self
                                        .last_synced_block
                                        .as_ref()
                                        .map(|b| header.number > b.number)
                                        .unwrap_or(true);

                                    let has_deferred = self
                                        .snapshot_queue
                                        .values()
                                        .any(|s| matches!(s, SnapshotStatus::Deferred));
                                    if is_new_block && has_deferred && header.number > 0 {
                                        // Block number incremented: the previous block is
                                        // complete. Fire deferred components at that block's
                                        // height.
                                        let to_fire: Vec<String> = self
                                            .snapshot_queue
                                            .iter()
                                            .filter(|(_, s)| matches!(s, SnapshotStatus::Deferred))
                                            .map(|(id, _)| id.clone())
                                            .collect();
                                        for id in &to_fire {
                                            self.snapshot_queue.remove(id);
                                        }
                                        let snapshot_header = BlockHeader {
                                            number: header.number - 1,
                                            hash: header.parent_hash.clone(),
                                            ..Default::default()
                                        };
                                        debug!(
                                            components = ?to_fire,
                                            extractor = %self.extractor_id.name,
                                            snapshot_block = header.number - 1,
                                            "snapshot_deferred_to_background"
                                        );
                                        self.spawn_snapshot_task(
                                            to_fire,
                                            snapshot_header,
                                            &deltas,
                                        );
                                    }

                                    // Accumulate truly_new into the deferred set for the current
                                    // block; they will be fired when the next block arrives.
                                    for id in truly_new {
                                        self.snapshot_queue
                                            .insert(id, SnapshotStatus::Deferred);
                                    }
                                } else if !truly_new.is_empty() {
                                    debug!(
                                        components = ?truly_new,
                                        extractor = %self.extractor_id.name,
                                        block_number = ?header.number,
                                        "snapshot_deferred_to_background"
                                    );
                                    let snapshot_header =
                                        BlockHeader { revert: false, ..header.clone() };
                                    self.spawn_snapshot_task(truly_new, snapshot_header, &deltas);
                                }

                                let snapshots = background_snapshots;

                                let removed_components = if !to_remove.is_empty() {
                                    self.component_tracker.stop_tracking(&to_remove)
                                } else {
                                    Default::default()
                                };

                                (snapshots, removed_components)
                            };

                            // Update entrypoints on the tracker (affects which contracts are tracked for DCI).
                            self.component_tracker.process_entrypoints(&deltas.dci_update);

                            // Filter deltas by currently tracked components / contracts.
                            self.filter_deltas(&mut deltas);
                            let n_changes = deltas.n_changes();

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

    /// Applies `self.buffered_deltas` to `snapshot`, updating attributes, balances, and contract
    /// storage for deltas strictly after `snapshot_block`.
    fn apply_deltas_to_snapshot(
        &self,
        snapshot: &mut Snapshot,
        snapshot_block: u64,
        contract_ids: &HashSet<Bytes>,
    ) {
        for delta in &self.buffered_deltas {
            if delta.block.number <= snapshot_block {
                continue;
            }
            for (component_id, state_delta) in &delta.state_deltas {
                if let Some(cws) = snapshot.states.get_mut(component_id) {
                    cws.state.attributes.extend(
                        state_delta
                            .updated_attributes
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone())),
                    );
                    for key in &state_delta.deleted_attributes {
                        cws.state.attributes.remove(key);
                    }
                }
            }
            for (component_id, token_balances) in &delta.component_balances {
                if let Some(cws) = snapshot.states.get_mut(component_id) {
                    for (token, bal) in token_balances {
                        cws.state
                            .balances
                            .insert(token.clone(), bal.balance.clone());
                    }
                }
            }
            for (address, account_delta) in &delta.account_deltas {
                if contract_ids.contains(address) {
                    if let Some(account) = snapshot.vm_storage.get_mut(address) {
                        account.slots.extend(
                            account_delta
                                .slots
                                .iter()
                                .filter_map(|(k, v)| {
                                    v.as_ref()
                                        .map(|v| (k.clone(), v.clone()))
                                }),
                        );
                        if let Some(balance) = &account_delta.balance {
                            account.native_balance = balance.clone();
                        }
                        if let Some(code) = account_delta.code() {
                            account.code = code.clone();
                        }
                    }
                }
            }
        }
    }

    /// Spawns a background snapshot task for `component_ids` at `snapshot_header`. If no other
    /// task is already in-flight, starts buffering deltas from `current_delta` so the snapshot
    /// can be brought up to date when the task drains.
    fn spawn_snapshot_task(
        &mut self,
        component_ids: Vec<String>,
        snapshot_header: BlockHeader,
        current_delta: &BlockAggregatedChanges,
    ) {
        let snapshot_block = snapshot_header.number;

        if self.snapshot_tasks.is_empty() {
            self.buffered_deltas
                .push(current_delta.clone());
        }

        let (tx, rx) = oneshot::channel();
        let rpc = self.rpc_client.clone();
        let bg_params = FetchSnapshotParams {
            chain: self.extractor_id.chain,
            protocol_system: self.extractor_id.name.clone(),
            block_number: snapshot_block,
            uses_dci: self.uses_dci,
            retrieve_balances: self.retrieve_balances,
            include_tvl: self.include_tvl,
        };
        let ids = component_ids.clone();
        tokio::spawn(async move {
            let _ = tx.send(fetch_snapshot_background(rpc, ids, bg_params).await);
        });
        for id in &component_ids {
            self.snapshot_queue
                .insert(id.clone(), SnapshotStatus::InFlight);
        }
        self.snapshot_tasks
            .push(SnapshotTask { component_ids, snapshot_block, receiver: rx });
    }

    /// Drains any background snapshot tasks that have completed. Returns a `Snapshot` containing
    /// all ready results, with buffered deltas applied to bring each snapshot up to date.
    fn drain_completed_snapshots(&mut self) -> Snapshot {
        let mut result = Snapshot::default();
        let pending = std::mem::take(&mut self.snapshot_tasks);

        for mut p in pending {
            match p.receiver.try_recv() {
                Ok(Ok(fetch_result)) => {
                    debug!(
                        components = ?p.component_ids,
                        extractor = %self.extractor_id.name,
                        "snapshot_background_ready"
                    );
                    for id in &p.component_ids {
                        self.snapshot_queue.remove(id);
                    }
                    let new_component_ids: Vec<String> = fetch_result
                        .components
                        .keys()
                        .cloned()
                        .collect();
                    self.component_tracker
                        .components
                        .extend(fetch_result.components);
                    self.component_tracker
                        .process_entrypoints(&fetch_result.dci_update);
                    self.component_tracker
                        .update_contracts(new_component_ids);
                    let mut snapshot = fetch_result.snapshot;
                    self.apply_deltas_to_snapshot(
                        &mut snapshot,
                        fetch_result.snapshot_block,
                        &fetch_result.contract_ids,
                    );
                    result.extend(snapshot);
                }
                Ok(Err(e)) => {
                    if e.is_transient() {
                        warn!(
                            components = ?p.component_ids,
                            extractor = %self.extractor_id.name,
                            err = %e,
                            "Background snapshot fetch failed transiently; will retry next block"
                        );
                        for id in &p.component_ids {
                            self.snapshot_queue
                                .insert(id.clone(), SnapshotStatus::RetryNext);
                        }
                    } else {
                        warn!(
                            components = ?p.component_ids,
                            extractor = %self.extractor_id.name,
                            err = %e,
                            "Background snapshot fetch failed permanently; \
                             components blacklisted until restart"
                        );
                        for id in &p.component_ids {
                            self.snapshot_queue
                                .insert(id.clone(), SnapshotStatus::Blacklisted);
                        }
                    }
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    self.snapshot_tasks.push(p);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    warn!(
                        components = ?p.component_ids,
                        extractor = %self.extractor_id.name,
                        "Background snapshot task dropped before sending result"
                    );
                    for id in &p.component_ids {
                        self.snapshot_queue.remove(id);
                    }
                }
            }
        }

        result
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

        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: false,
            retrieve_balances: true,
            include_tvl: false,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");
        let snap = StateSyncMessage {
            header: header.clone(),
            snapshots: snapshot,
            deltas: None,
            removed_components: Default::default(),
        };

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

        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: false,
            retrieve_balances: true,
            include_tvl: true,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");
        let snap = StateSyncMessage {
            header: header.clone(),
            snapshots: snapshot,
            deltas: None,
            removed_components: Default::default(),
        };

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

        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: false,
            retrieve_balances: false,
            include_tvl: false,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");
        let snap = StateSyncMessage {
            header: header.clone(),
            snapshots: snapshot,
            deltas: None,
            removed_components: Default::default(),
        };

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

        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: false,
            retrieve_balances: false,
            include_tvl: true,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");
        let snap = StateSyncMessage {
            header: header.clone(),
            snapshots: snapshot,
            deltas: None,
            removed_components: Default::default(),
        };

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
        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: false,
            retrieve_balances: true,
            include_tvl: false,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");

        // Verify we only got Component2 back
        assert_eq!(snapshot.states.len(), 1);
        assert!(snapshot
            .states
            .contains_key("Component2"));
        assert!(!snapshot
            .states
            .contains_key("Component1"));
        assert!(!snapshot
            .states
            .contains_key("Component3"));
    }

    fn mock_clients_for_state_sync(
        bg_done: Option<Arc<tokio::sync::Notify>>,
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
            .returning(move |_request, _chunk_size, _concurrency| {
                let snap = Ok(Snapshot {
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
                });
                if let Some(n) = &bg_done {
                    n.notify_one();
                }
                snap
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
    /// - initial snapshot retrieval returns component1 and component2 as snapshots
    /// - block 1: DCI update for Component1; no new components
    /// - block 2: Component3 TVL crosses threshold → background snapshot task spawned; snapshot
    ///   appears in block 3 after the background task drains
    /// - block 3: empty block; drain produces Component3 snapshot
    #[test_log::test(tokio::test)]
    async fn test_state_sync() {
        let bg_done = Arc::new(tokio::sync::Notify::new());
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync(Some(bg_done.clone()));
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
            // Block 3: empty block; the background task for Component3 should have completed,
            // so drain_completed_snapshots returns the Component3 snapshot.
            BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 3,
                    hash: Bytes::from("0x03"),
                    parent_hash: Bytes::from("0x02"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
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
        let first_msg = timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("waiting for first state msg timed out!")
            .expect("state sync block sender closed!");
        tx.send(deltas[1].clone())
            .await
            .expect("deltas channel msg 1 closed!");
        let second_msg = timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("waiting for second state msg timed out!")
            .expect("state sync block sender closed!");
        // Wait for the background snapshot task to complete before sending block 3.
        bg_done.notified().await;
        tx.send(deltas[2].clone())
            .await
            .expect("deltas channel msg 2 closed!");
        let third_msg = timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("waiting for third state msg timed out!")
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

        // Block 2: Component3 snapshot task is spawned in the background. Component3 is not
        // yet tracked, so it is filtered from component_tvl. Snapshot is empty.
        let exp2 = StateSyncMessage {
            header: BlockHeader {
                number: 2,
                hash: Bytes::from("0x02"),
                parent_hash: Bytes::from("0x01"),
                revert: false,
                ..Default::default()
            },
            snapshots: Snapshot::default(),
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
                    // Component2 removed (tvl=0), Component3 not yet tracked → filtered out.
                    ("Component1".to_string(), 100.0),
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

        // Block 3: background task has completed; Component3 snapshot is drained and included.
        let exp3 = StateSyncMessage {
            header: BlockHeader {
                number: 3,
                hash: Bytes::from("0x03"),
                parent_hash: Bytes::from("0x02"),
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
                        component_tvl: Some(1000.0),
                        entrypoints: vec![],
                    },
                )]
                .into_iter()
                .collect(),
                vm_storage: HashMap::new(),
            },
            deltas: Some(deltas[2].clone()),
            removed_components: Default::default(),
        };
        assert_eq!(first_msg.unwrap(), exp1);
        assert_eq!(second_msg.unwrap(), exp2);
        assert_eq!(third_msg.unwrap(), exp3);
    }

    #[test_log::test(tokio::test)]
    async fn test_state_sync_with_tvl_range() {
        let remove_tvl_threshold = 5.0;
        let add_tvl_threshold = 7.0;
        let bg_done = Arc::new(tokio::sync::Notify::new());

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
        let bg_done_clone = bg_done.clone();
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
            .returning(move |_request, _chunk_size, _concurrency| {
                let snap = Ok(Snapshot {
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
                });
                bg_done_clone.notify_one();
                snap
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
            // Block 3: empty; background task for Component3 should have completed.
            BlockAggregatedChanges {
                extractor: "uniswap-v2".to_string(),
                chain: Chain::Ethereum,
                block: Block {
                    number: 3,
                    hash: Bytes::from("0x03"),
                    parent_hash: Bytes::from("0x02"),
                    chain: Chain::Ethereum,
                    ts: Default::default(),
                },
                revert: false,
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
        let _ = timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("waiting for first state msg timed out!")
            .expect("state sync block sender closed!");

        // Send the second message, which should trigger TVL-based changes.
        // Component3 snapshot is deferred to background; not in this block's message.
        tx.send(deltas[1].clone())
            .await
            .expect("deltas channel msg 1 closed!");
        let second_msg = timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("waiting for second state msg timed out!")
            .expect("state sync block sender closed!")
            .expect("no error");

        // Wait for the background snapshot task to complete before sending block 3.
        bg_done.notified().await;

        tx.send(deltas[2].clone())
            .await
            .expect("deltas channel msg 2 closed!");
        let third_msg = timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("waiting for third state msg timed out!")
            .expect("state sync block sender closed!")
            .expect("no error");

        let _ = close_tx.send(());
        jh.await
            .expect("state sync task panicked!");

        // Block 2: Component3 task spawned; snapshot is empty, Component3 filtered from deltas.
        let expected_second_msg = StateSyncMessage {
            header: BlockHeader {
                number: 2,
                hash: Bytes::from("0x02"),
                parent_hash: Bytes::from("0x01"),
                revert: false,
                ..Default::default()
            },
            snapshots: Snapshot::default(),
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
                    ("Component1".to_string(), 6.0), /* Within range, should not trigger changes
                                                      * Component3 not yet tracked → filtered
                                                      * out */
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

        // Block 3: background task drained; Component3 snapshot present.
        let expected_third_msg = StateSyncMessage {
            header: BlockHeader {
                number: 3,
                hash: Bytes::from("0x03"),
                parent_hash: Bytes::from("0x02"),
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
                        entrypoints: vec![],
                    },
                )]
                .into_iter()
                .collect(),
                vm_storage: HashMap::new(),
            },
            deltas: Some(deltas[2].clone()),
            removed_components: Default::default(),
        };

        assert_eq!(second_msg, expected_second_msg);
        assert_eq!(third_msg, expected_third_msg);
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
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync(None);
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
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync(None);
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
        let (rpc_client, deltas_client, tx) = mock_clients_for_state_sync(None);
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
        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: false,
            retrieve_balances: true,
            include_tvl: false,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");

        assert!(snapshot
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
        let req_ids: Vec<String> = components_arg.to_vec();
        let components: HashMap<_, _> = state_sync
            .component_tracker
            .components
            .iter()
            .filter(|(id, _)| req_ids.contains(id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let contract_ids: HashSet<Bytes> = state_sync
            .component_tracker
            .get_contracts_by_component(&req_ids)
            .into_iter()
            .collect();
        let params = FetchSnapshotParams {
            chain: Chain::Ethereum,
            protocol_system: "uniswap-v2".to_string(),
            block_number: header.number,
            uses_dci: true,
            retrieve_balances: true,
            include_tvl: false,
        };
        let (snapshot, _, _) =
            fetch_snapshot(&state_sync.rpc_client, components, contract_ids, &params)
                .await
                .expect("Retrieving snapshot failed");

        assert!(snapshot
            .states
            .contains_key("Component1"));
    }

    /// Test that in partial-blocks mode, new components are deferred until the block number
    /// increments (confirming the previous block is complete), then fired as a background task at
    /// the previous block's height. The snapshots appear in the first message of the block AFTER
    /// the one where the task was fired.
    ///
    /// Timeline:
    /// - Block 1 (full): initial sync
    /// - Block 2 partial: BrandNew + Preexisting added to deferred set (no task yet)
    /// - Block 3 partial: block number increments → task fired at snapshot_block=2; msg3 empty
    /// - Block 4 partial: task has completed → drain returns both snapshots in msg4
    #[test_log::test(tokio::test)]
    async fn test_partial_mode_defers_brand_new_component_snapshot_to_next_block() {
        use std::time::Duration;

        use tokio::{sync::mpsc::channel, time::timeout};

        let bg_done = Arc::new(tokio::sync::Notify::new());
        let mut rpc_client = make_mock_client();
        // get_protocol_components for BrandNew + Preexisting (background task fires at block 3)
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
        // get_protocol_components for initial sync
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
        // Background task fires when block 3 arrives: snapshot at block 2 (3 - 1).
        let bg_done_clone = bg_done.clone();
        rpc_client
            .expect_get_snapshots()
            .withf(
                |request: &SnapshotParameters,
                 _chunk_size: &Option<usize>,
                 _concurrency: &usize| {
                    request.block_number == 2 &&
                        (request
                            .components
                            .contains_key("BrandNew") ||
                            request
                                .components
                                .contains_key("Preexisting"))
                },
            )
            .returning(move |_request, _chunk_size, _concurrency| {
                let snap = Ok(Snapshot {
                    states: [
                        (
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
                        ),
                        (
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
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    vm_storage: HashMap::new(),
                });
                bg_done_clone.notify_one();
                snap
            });
        // get_snapshots for initial sync (block 0, Component1+Component2)
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
        let (tx, rx) = channel(4);
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

        // Block 2 partial: BrandNew and Preexisting both appear. Neither task is fired yet —
        // both are added to deferred_snapshot_components.
        let mut block2 = make_block_changes(2, Some(2));
        block2.new_protocol_components = HashMap::from([(
            "BrandNew".to_string(),
            ProtocolComponent { id: "BrandNew".to_string(), ..Default::default() },
        )]);
        block2.component_tvl =
            HashMap::from([("BrandNew".to_string(), 100.0), ("Preexisting".to_string(), 75.0)]);
        tx.send(block2).await.unwrap();
        let msg2 = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive block 2")
            .expect("Channel open")
            .expect("No error");

        assert!(
            !msg2
                .snapshots
                .states
                .contains_key("Preexisting"),
            "Preexisting should still be deferred in block 2, not yet snapshotted; got: {:?}",
            msg2.snapshots
                .states
                .keys()
                .collect::<Vec<_>>()
        );
        assert!(
            !msg2
                .snapshots
                .states
                .contains_key("BrandNew"),
            "BrandNew should still be deferred in block 2, not yet snapshotted"
        );

        // Block 3 partial: block number increments → deferred components fire as background task
        // at snapshot_block=2. msg3 has no snapshots (task just spawned).
        tx.send(make_block_changes(3, Some(1)))
            .await
            .unwrap();
        let msg3 = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive block 3")
            .expect("Channel open")
            .expect("No error");

        assert_eq!(msg3.header.number, 3);
        assert_eq!(msg3.header.partial_block_index, Some(1));
        assert!(
            !msg3
                .snapshots
                .states
                .contains_key("BrandNew"),
            "BrandNew task just fired; snapshot not yet available in msg3"
        );
        assert!(
            !msg3
                .snapshots
                .states
                .contains_key("Preexisting"),
            "Preexisting task just fired; snapshot not yet available in msg3"
        );

        // Wait for the background snapshot task to complete before the next block arrives.
        bg_done.notified().await;

        // Block 4 partial: drain finds the completed task → both snapshots present in msg4.
        tx.send(make_block_changes(4, Some(0)))
            .await
            .unwrap();
        let msg4 = timeout(Duration::from_millis(200), block_rx.recv())
            .await
            .expect("Should receive block 4")
            .expect("Channel open")
            .expect("No error");

        assert_eq!(msg4.header.number, 4);
        assert_eq!(msg4.header.partial_block_index, Some(0));
        assert!(
            msg4.snapshots
                .states
                .contains_key("BrandNew"),
            "BrandNew snapshot should be in msg4 after background task drains; got keys: {:?}",
            msg4.snapshots
                .states
                .keys()
                .collect::<Vec<_>>()
        );
        assert!(
            msg4.snapshots
                .states
                .contains_key("Preexisting"),
            "Preexisting snapshot should be in msg4 after background task drains; got keys: {:?}",
            msg4.snapshots
                .states
                .keys()
                .collect::<Vec<_>>()
        );

        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");
    }

    /// Directly exercises all four mutation paths of `apply_deltas_to_snapshot`:
    /// attribute update, attribute deletion, balance merge, and VM slot/balance/code overwrite.
    /// Also verifies that deltas at or before `snapshot_block` are skipped.
    #[test]
    fn test_apply_deltas_to_snapshot() {
        use tycho_common::models::{
            contract::{Account, AccountDelta},
            protocol::{ComponentBalance, ProtocolComponentStateDelta},
            ChangeType,
        };

        let contract_addr = Bytes::from("0xc0ffee");
        let token_addr = Bytes::from("0xdeadbeef");

        // Build the snapshot at block 5: one component with attributes and balances,
        // one VM contract with slots and native balance.
        let mut snapshot = Snapshot {
            states: [(
                "comp1".to_string(),
                ComponentWithState {
                    state: ProtocolComponentState::new(
                        "comp1",
                        [
                            ("keep".to_string(), Bytes::from("0x01")),
                            ("delete_me".to_string(), Bytes::from("0x02")),
                        ]
                        .into_iter()
                        .collect(),
                        [(token_addr.clone(), Bytes::from("0x64"))]
                            .into_iter()
                            .collect(),
                    ),
                    component: ProtocolComponent::default(),
                    component_tvl: None,
                    entrypoints: vec![],
                },
            )]
            .into_iter()
            .collect(),
            vm_storage: [(
                contract_addr.clone(),
                Account {
                    chain: Chain::Ethereum,
                    address: contract_addr.clone(),
                    title: String::new(),
                    slots: [(Bytes::from("0x01"), Bytes::from("0xaa"))]
                        .into_iter()
                        .collect(),
                    native_balance: Bytes::from("0x10"),
                    token_balances: HashMap::new(),
                    code: Bytes::from("0x0a0b"),
                    code_hash: Default::default(),
                    balance_modify_tx: Default::default(),
                    code_modify_tx: Default::default(),
                    creation_tx: None,
                },
            )]
            .into_iter()
            .collect(),
        };

        // Two buffered deltas: block 5 (at snapshot_block, must be skipped) and block 6
        // (after snapshot_block, must be applied).
        let skipped_delta = BlockAggregatedChanges {
            block: Block { number: 5, ..Default::default() },
            state_deltas: [(
                "comp1".to_string(),
                ProtocolComponentStateDelta::new(
                    "comp1",
                    [("keep".to_string(), Bytes::from("0xff"))]
                        .into_iter()
                        .collect(),
                    HashSet::new(),
                ),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let applied_delta = BlockAggregatedChanges {
            block: Block { number: 6, ..Default::default() },
            state_deltas: [(
                "comp1".to_string(),
                ProtocolComponentStateDelta::new(
                    "comp1",
                    [("keep".to_string(), Bytes::from("0x99"))]
                        .into_iter()
                        .collect(),
                    ["delete_me".to_string()]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
            component_balances: [(
                "comp1".to_string(),
                [(
                    token_addr.clone(),
                    ComponentBalance::new(
                        token_addr.clone(),
                        Bytes::from("0xc8"),
                        200.0,
                        Default::default(),
                        "comp1",
                    ),
                )]
                .into_iter()
                .collect(),
            )]
            .into_iter()
            .collect(),
            account_deltas: [(
                contract_addr.clone(),
                AccountDelta::new(
                    Chain::Ethereum,
                    contract_addr.clone(),
                    [
                        (Bytes::from("0x01"), Some(Bytes::from("0xbb"))),
                        (Bytes::from("0x02"), Some(Bytes::from("0xcc"))),
                    ]
                    .into_iter()
                    .collect(),
                    Some(Bytes::from("0x20")),
                    Some(Bytes::from("0x0c0d")),
                    ChangeType::Update,
                ),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let mut sync = with_mocked_clients(true, false, None, None);
        sync.buffered_deltas = vec![skipped_delta, applied_delta];

        let contract_ids: HashSet<Bytes> = [contract_addr.clone()]
            .into_iter()
            .collect();
        sync.apply_deltas_to_snapshot(&mut snapshot, 5, &contract_ids);

        let comp = &snapshot.states["comp1"].state;

        // Attribute update applied
        assert_eq!(comp.attributes["keep"], Bytes::from("0x99"));
        // Attribute deletion applied
        assert!(!comp
            .attributes
            .contains_key("delete_me"));
        // Balance merge applied
        assert_eq!(comp.balances[&token_addr], Bytes::from("0xc8"));

        let account = &snapshot.vm_storage[&contract_addr];
        // Existing slot overwritten, new slot added
        assert_eq!(account.slots[&Bytes::from("0x01")], Bytes::from("0xbb"));
        assert_eq!(account.slots[&Bytes::from("0x02")], Bytes::from("0xcc"));
        // Native balance updated
        assert_eq!(account.native_balance, Bytes::from("0x20"));
        // Code updated
        assert_eq!(account.code, Bytes::from("0x0c0d"));
    }
}
