use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use async_trait::async_trait;
use rand::Rng;
use thiserror::Error;
use tokio::{
    select,
    sync::{
        mpsc::{channel, error::SendError, Receiver, Sender},
        oneshot,
    },
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};
use tracing::{debug, error, info, instrument, warn};
use tycho_common::{
    dto::DeltaCatchUpUnavailable,
    models::{
        blockchain::{
            BlockAggregatedChanges, DCIUpdate, EntryPointWithTracingParams, TracingResult,
        },
        contract::Account,
        protocol::{ProtocolComponent, ProtocolComponentState},
        BlockHash, Chain, ExtractorFingerprint, ExtractorIdentity,
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
        DeltaCatchUp, DeltaCatchUpParams, RPCClient, RPCError, SnapshotParameters,
        TracedEntryPointsPaginatedParams, RPC_CLIENT_CONCURRENCY,
    },
    DeltasError,
};

/// Default maximum random delay before a restarted synchronizer requests a snapshot.
pub const DEFAULT_SNAPSHOT_JITTER: Duration = Duration::from_secs(60);
/// Lowest accepted maximum delay before a restarted synchronizer requests a snapshot.
pub const MIN_SNAPSHOT_JITTER: Duration = Duration::from_secs(5);

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
pub type SyncError = (SynchronizerError, Option<oneshot::Receiver<()>>);

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
    /// The fingerprint the subscription that emitted `last_synced_block` reported. `None` before
    /// the first sync, or when the server or deltas client predates fingerprints.
    synced_fingerprint: Option<ExtractorFingerprint>,
    timeout: u64,
    include_tvl: bool,
    compression: bool,
    partial_blocks: bool,
    uses_dci: bool,
    snapshot_jitter: Duration,
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

/// Why a resubscribed synchronizer resyncs from a snapshot instead of catching up on the messages
/// it missed.
#[derive(Debug, Error)]
enum CatchUpSkipped {
    /// Partial-block mode: the server keeps full blocks only, and a partial-block client never
    /// receives one, so it cannot tell whether it applied a whole block.
    #[error("partial-block subscriptions do not catch up")]
    PartialBlocks,
    /// The subscription reported no fingerprint (a server or deltas client that predates them),
    /// so the server cannot check that its deltas match the ones the client applied.
    #[error("the subscription reported no extractor fingerprint")]
    NoFingerprint,
    /// The server predates delta catch-up.
    #[error("the server does not support delta catch-up")]
    Unsupported,
    /// The server cannot serve the messages.
    #[error("the server cannot serve the missed deltas: {0:?}")]
    Unavailable(DeltaCatchUpUnavailable),
    /// The request failed, e.g. because the plan has no access to the protocol system.
    #[error("the delta catch-up request failed: {0}")]
    Rpc(RPCError),
    /// The server did not answer in time.
    #[error("the server did not answer the delta catch-up request within {0:?}")]
    TimedOut(Duration),
    /// A message belongs to another extractor or chain. Extractors on one chain share block
    /// hashes, so the hashes alone cannot tell their messages apart.
    #[error("the missed deltas include block {number} of extractor {chain}:{extractor}")]
    ForeignMessage { extractor: String, chain: Chain, number: u64 },
    /// A block does not extend the block the messages before it lead to: another parent, or a
    /// number that is not one higher.
    #[error("missed block {number} does not extend the blocks before it")]
    DetachedBlock { number: u64 },
    /// A revert's target is not an ancestor of the block the messages before it lead to: it is not
    /// lower, or it differs from the block the walk passed at its height.
    #[error("missed revert to block {number} does not go back to an earlier block of the path")]
    RevertToNonAncestor { number: u64 },
    /// The messages lead to another block than the one the first live message continues from.
    /// This includes an empty path when the last synced block is not that block.
    #[error("the missed deltas lead to block {reached}, not to {to}")]
    WrongEnd { reached: BlockHash, to: BlockHash },
}

/// Checks that `path` takes a consumer of `extractor` from block `from` to block `to`.
///
/// Every message must belong to `extractor` and its chain. Walking from `from`, each block must
/// extend the block reached so far: its parent is that block and its number is one higher. A
/// revert moves back to its target block (the block its header names), which must be lower than
/// the block reached and, when the walk passed a block at the target's height and has not reverted
/// it, must be that block. The walk must end at `to`, so an empty path is valid only when `from` is
/// `to`, and a path may end with a revert to `to`.
fn check_catch_up_path(
    path: &[BlockAggregatedChanges],
    extractor: &ExtractorIdentity,
    from: &BlockHeader,
    to: &BlockHash,
) -> Result<(), CatchUpSkipped> {
    let mut reached_number = from.number;
    let mut reached_hash = &from.hash;
    // The blocks below the one reached that the walk passed and has not reverted, at consecutive
    // heights, oldest first. A revert can only check targets the walk passed: the client keeps no
    // history below `from`.
    let mut passed: Vec<(u64, &BlockHash)> = Vec::new();
    for message in path {
        let block = &message.block;
        if message.extractor != extractor.name || message.chain != extractor.chain {
            return Err(CatchUpSkipped::ForeignMessage {
                extractor: message.extractor.clone(),
                chain: message.chain,
                number: block.number,
            });
        }
        if message.revert {
            if block.number >= reached_number {
                return Err(CatchUpSkipped::RevertToNonAncestor { number: block.number });
            }
            passed.retain(|&(number, _)| number <= block.number);
            if let Some(&(number, hash)) = passed.last() {
                if number == block.number {
                    if hash != &block.hash {
                        return Err(CatchUpSkipped::RevertToNonAncestor { number: block.number });
                    }
                    passed.pop();
                }
            }
        } else {
            if &block.parent_hash != reached_hash || block.number != reached_number + 1 {
                return Err(CatchUpSkipped::DetachedBlock { number: block.number });
            }
            passed.push((reached_number, reached_hash));
        }
        reached_number = block.number;
        reached_hash = &block.hash;
    }
    if reached_hash != to {
        return Err(CatchUpSkipped::WrongEnd { reached: reached_hash.clone(), to: to.clone() });
    }
    Ok(())
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
            synced_fingerprint: None,
            timeout,
            include_tvl,
            compression,
            partial_blocks: false,
            uses_dci: false,
            snapshot_jitter: DEFAULT_SNAPSHOT_JITTER,
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

    /// Sets the maximum random delay before a restarted synchronizer requests a snapshot.
    ///
    /// After a restart that needs a snapshot, the synchronizer waits a random time in
    /// `[0, jitter]` before it requests the snapshot. The first start has no delay.
    /// A `jitter` below [`MIN_SNAPSHOT_JITTER`] is raised to it.
    pub fn with_snapshot_jitter(mut self, jitter: Duration) -> Self {
        if jitter < MIN_SNAPSHOT_JITTER {
            warn!(
                extractor_id=%self.extractor_id,
                requested_ms=jitter.as_millis(),
                min_ms=MIN_SNAPSHOT_JITTER.as_millis(),
                "Snapshot jitter is below the minimum; using the minimum"
            );
        }
        self.snapshot_jitter = jitter.max(MIN_SNAPSHOT_JITTER);
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
    ///
    /// If the first block does not follow `last_synced_block`, the synchronizer first asks the
    /// server for the messages it missed and, when they lead from `last_synced_block` to that
    /// block, emits them merged with the block as one message. Otherwise it discards deltas for
    /// `snapshot_delay` and requests a snapshot at the first block after the delay.
    #[instrument(skip(self, block_tx, end_rx), fields(extractor_id = %self.extractor_id))]
    async fn state_sync(
        &mut self,
        block_tx: &mut Sender<SyncResult<StateSyncMessage<BlockHeader>>>,
        mut end_rx: oneshot::Receiver<()>,
        snapshot_delay: Duration,
    ) -> Result<(), Box<SyncError>> {
        // initialisation
        let subscription_options = SubscriptionOptions::new()
            .with_state(self.include_snapshots)
            .with_compression(self.compression)
            .with_partial_blocks(self.partial_blocks);
        let (subscription_id, mut msg_rx, fingerprint) = match self
            .deltas_client
            .subscribe_with_fingerprint(self.extractor_id.clone(), subscription_options)
            .await
        {
            Ok(result) => result,
            Err(e) => return Err(Box::new((e.into(), Some(end_rx)))),
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
            // Set when the first candidate that cannot resume the stream arrives, so later
            // candidates (after a stale snapshot, for instance) don't decide again. Messages keep
            // arriving during the delay, so we discard them instead of sleeping: a sleep would
            // overflow the subscription buffer on fast chains.
            let mut snapshot_not_before: Option<Instant> = None;
            // An upgraded extractor may have emitted other deltas for the blocks the consumer
            // already applied, so a block that follows `last_synced_block` resumes the stream only
            // when the fingerprint is unchanged. Otherwise the consumer resyncs from a snapshot.
            let may_resume = self.may_resume(fingerprint.as_ref());
            if !may_resume && self.last_synced_block.is_some() {
                info!(
                    synced = ?self.synced_fingerprint,
                    reported = ?fingerprint,
                    "Extractor fingerprint changed; resyncing from a snapshot"
                );
            }
            let (msg, header) = 'init: loop {
                let mut warned_waiting_for_new_block = false;
                let mut warned_skipping_synced = false;
                let first_msg = loop {
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
                        if current.number >= incoming.number &&
                            !(may_resume && self.is_next_expected(&incoming))
                        {
                            if !warned_skipping_synced {
                                info!(extractor=%self.extractor_id, from=incoming.number, to=current.number, "Syncing. Skipping already synced block");
                                warned_skipping_synced = true;
                            }
                            continue;
                        }
                    }
                    match snapshot_not_before {
                        Some(not_before) if Instant::now() < not_before => continue,
                        Some(_) => {}
                        None if may_resume && self.is_next_expected(&incoming) => {}
                        None => {
                            // Catching up needs no snapshot, so it runs before the snapshot delay
                            // starts; the delay applies only when it falls back.
                            let last_synced =
                                self.last_synced_block.as_ref().filter(|_| may_resume);
                            if let Some(last_synced) = last_synced {
                                match self
                                    .catch_up_path(last_synced, fingerprint.as_ref(), &incoming)
                                    .await
                                {
                                    Ok(path) => {
                                        info!(
                                            height = incoming.number,
                                            n_missed = path.len(),
                                            "Caught up on missed deltas"
                                        );
                                        break 'init (self.apply_catch_up(path, msg), incoming);
                                    }
                                    Err(skipped) => {
                                        info!(
                                            %skipped,
                                            "Delta catch-up not possible; resyncing from a snapshot"
                                        );
                                    }
                                }
                            }
                            snapshot_not_before = Some(Instant::now() + snapshot_delay);
                            if !snapshot_delay.is_zero() {
                                info!(
                                    extractor=%self.extractor_id,
                                    delay_ms=snapshot_delay.as_millis(),
                                    "Syncing. Delaying snapshot request"
                                );
                                continue;
                            }
                        }
                    }
                    break msg;
                };

                // initial snapshot
                info!(height = first_msg.get_block().number, "First deltas received");
                let header: BlockHeader = (&first_msg).into();

                // If possible skip retrieving snapshots
                if !(may_resume && self.is_next_expected(&header)) {
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
                    // The snapshot's DCI data comes from the server's database, which lags
                    // behind the subscription, so the first message's DCI and TVL changes are
                    // applied on top of it. Only after a successful fetch: a stale snapshot
                    // discards this message, which must then leave the tracker untouched.
                    let deltas_msg = self.handle_block(first_msg);
                    break 'init (snapshot_msg.merge(deltas_msg), header);
                } else {
                    // The consumer holds the state at `last_synced_block` and this message
                    // follows it, so it is handled like a live message.
                    break 'init (self.handle_block(first_msg), header);
                }
            };

            block_tx.send(Ok(msg)).await?;
            self.last_synced_block = Some(header);
            self.synced_fingerprint = fingerprint;
            loop {
                select! {
                    deltas_opt = msg_rx.recv() => {
                        if let Some(deltas) = deltas_opt {
                            let header: BlockHeader = (&deltas).into();
                            debug!(block_number=?header.number, "Received delta message");

                            let next = self.handle_block(deltas);
                            let n_changes = next
                                .deltas
                                .as_ref()
                                .map_or(0, BlockAggregatedChanges::n_changes);
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
                Err(Box::new((e, Some(end_rx))))
            }
        }
    }

    /// Runs one block's message through the per-block handling and returns the message to emit
    /// for it.
    ///
    /// Buffers the unfiltered message for in-flight background snapshots and harvests the
    /// finished ones, updates tracking from the message's TVL (removing components below the
    /// threshold and spawning background snapshots for new ones), applies its DCI entry points,
    /// and filters it to the tracked components and contracts. The caller sends the result and
    /// then advances `last_synced_block`, which this reads to detect a new block in partial-block
    /// mode.
    fn handle_block(
        &mut self,
        mut deltas: BlockAggregatedChanges,
    ) -> StateSyncMessage<BlockHeader> {
        let header: BlockHeader = (&deltas).into();

        // Buffer unfiltered delta while any snapshot task is in-flight.
        if !self.snapshot_tasks.is_empty() {
            self.buffered_deltas
                .push(deltas.clone());
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
            let (to_add, to_remove) = self
                .component_tracker
                .filter_updated_components(&deltas);

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
                        !self
                            .component_tracker
                            .components
                            .contains_key(id.as_str()) &&
                            !self
                                .snapshot_queue
                                .contains_key(id.as_str()) &&
                            seen.insert(id.as_str())
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
                    self.spawn_snapshot_task(to_fire, snapshot_header, &deltas);
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
                let snapshot_header = BlockHeader { revert: false, ..header.clone() };
                self.spawn_snapshot_task(truly_new, snapshot_header, &deltas);
            }

            let snapshots = background_snapshots;

            let removed_components = if !to_remove.is_empty() {
                self.component_tracker
                    .stop_tracking(&to_remove)
            } else {
                Default::default()
            };

            (snapshots, removed_components)
        };

        // Update entrypoints on the tracker (affects which contracts are tracked for DCI).
        self.component_tracker
            .process_entrypoints(&deltas.dci_update);

        // Filter deltas by currently tracked components / contracts.
        self.filter_deltas(&mut deltas);

        StateSyncMessage { header, snapshots, deltas: Some(deltas), removed_components }
    }

    /// Asks the server for the messages that take the consumer from `last_synced` to the block the
    /// new subscription's first message `first` continues from, and checks that they do. Runs
    /// nothing through [`Self::handle_block`], so a failure leaves the synchronizer untouched.
    ///
    /// The target is `first`'s parent, or `first`'s own block when it is a revert: a revert
    /// carries its target block's header but not that block's own changes, which the path brings.
    ///
    /// # Errors
    ///
    /// The [`CatchUpSkipped`] reason when catch-up is not possible or the answer is not a path
    /// from `last_synced` to the target; the caller then resyncs from a snapshot.
    async fn catch_up_path(
        &self,
        last_synced: &BlockHeader,
        fingerprint: Option<&ExtractorFingerprint>,
        first: &BlockHeader,
    ) -> Result<Vec<BlockAggregatedChanges>, CatchUpSkipped> {
        if self.partial_blocks {
            return Err(CatchUpSkipped::PartialBlocks);
        }
        let Some(fingerprint) = fingerprint else {
            return Err(CatchUpSkipped::NoFingerprint);
        };
        let to = if first.revert { &first.hash } else { &first.parent_hash };
        let params = DeltaCatchUpParams::new(
            self.extractor_id.chain,
            self.extractor_id.name.clone(),
            last_synced.hash.clone(),
            last_synced.number,
            to.clone(),
            fingerprint.clone(),
        )
        .with_include_state(self.include_snapshots);
        // The RPC client waits 120 s before retrying a 502/503/504, which would hold back the
        // snapshot fallback. The synchronizer's timeout spans a few blocks (the stream builder
        // sets `block_time + timeout`), so the subscription buffers only a few messages meanwhile.
        let limit = Duration::from_secs(self.timeout);
        let answer = timeout(
            limit,
            self.rpc_client
                .get_delta_catch_up(params),
        )
        .await
        .map_err(|_| CatchUpSkipped::TimedOut(limit))?
        .map_err(CatchUpSkipped::Rpc)?;
        let path = match answer {
            DeltaCatchUp::Available(path) => path,
            DeltaCatchUp::Unavailable(reason) => return Err(CatchUpSkipped::Unavailable(reason)),
            DeltaCatchUp::Unsupported => return Err(CatchUpSkipped::Unsupported),
        };
        check_catch_up_path(&path, &self.extractor_id, last_synced, to)?;
        Ok(path)
    }

    /// Runs each message of a checked catch-up `path`, then the subscription's first message
    /// `first`, through [`Self::handle_block`], and merges the results, in that order, into one
    /// message with `first`'s header.
    fn apply_catch_up(
        &mut self,
        path: Vec<BlockAggregatedChanges>,
        first: BlockAggregatedChanges,
    ) -> StateSyncMessage<BlockHeader> {
        // Merging into an empty message yields the merged message unchanged.
        let mut merged = StateSyncMessage::default();
        for message in path {
            let handled = self.handle_block(message);
            merged = merged.merge(handled);
        }
        let first = self.handle_block(first);
        merged.merge(first)
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

    /// Whether a subscription that reported `fingerprint` may continue the consumer's stream from
    /// `last_synced_block` without a snapshot: the fingerprint equals the one the consumer synced
    /// under, or neither subscription reported one (servers that predate fingerprints).
    fn may_resume(&self, fingerprint: Option<&ExtractorFingerprint>) -> bool {
        self.synced_fingerprint.as_ref() == fingerprint
    }

    /// Whether `incoming` follows `last_synced_block`. Never true for a revert.
    fn is_next_expected(&self, incoming: &BlockHeader) -> bool {
        // A revert to the block after `last_synced_block` has the right parent, but it carries
        // only the keys the reverted blocks above it changed, not the target block's own changes.
        if incoming.revert {
            return false;
        }
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
                // Restarts of many clients happen at once after a server or extractor restart;
                // a random delay spreads their snapshot requests. A stream that requests no
                // snapshot puts no load to spread and would only lose deltas.
                let requests_snapshot = self.include_snapshots &&
                    !self
                        .component_tracker
                        .components
                        .is_empty();
                let snapshot_delay = if prev_block.is_some() && requests_snapshot {
                    rand::thread_rng().gen_range(Duration::ZERO..=self.snapshot_jitter)
                } else {
                    Duration::ZERO
                };
                let res = self
                    .state_sync(&mut tx, current_end_rx, snapshot_delay)
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
                    Err(e) => {
                        let (e, maybe_end_rx) = *e;
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
                // A run that processed blocks is a healthy run — reset the counter so
                // transient failures after a long successful period get a fresh retry budget,
                // and restart at once: the resubscribe waits for the websocket anyway.
                if made_progress {
                    retry_count = 0;
                } else {
                    retry_count += 1;
                    sleep(self.retry_cooldown).await;
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

    use rstest::rstest;
    use tycho_common::models::{
        blockchain::{
            AddressStorageLocation, Block, BlockAggregatedChanges, DCIUpdate, EntryPoint,
            EntryPointWithTracingParams, RPCTracerParams, TracingParams, TracingResult,
        },
        contract::AccountDelta,
        protocol::{ProtocolComponent, ProtocolComponentState, ProtocolComponentStateDelta},
        token::Token,
        Chain, ChangeType,
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

        async fn get_delta_catch_up(
            &self,
            params: DeltaCatchUpParams,
        ) -> Result<DeltaCatchUp, RPCError> {
            self.0.get_delta_catch_up(params).await
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
            .state_sync(&mut block_tx, end_rx, Duration::ZERO)
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
                .state_sync(&mut block_tx, end_rx, Duration::ZERO)
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
                .state_sync(&mut block_tx, end_rx, Duration::ZERO)
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
    async fn test_restart_after_healthy_run_skips_cooldown() {
        let mut rpc_client = make_mock_client();
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 0)));

        let mut deltas_client = MockDeltasClient::new();
        let (resubscribed_tx, resubscribed_rx) = oneshot::channel();
        let mut resubscribed_tx = Some(resubscribed_tx);
        let mut open_senders = Vec::new();
        let mut calls = 0;
        deltas_client
            .expect_subscribe()
            .returning(move |_, _| {
                calls += 1;
                let (tx, rx) = channel(10);
                if calls == 1 {
                    // Deliver one block, then close the channel like a websocket reset does.
                    let block = Block { number: 1, ..Default::default() };
                    tx.try_send(BlockAggregatedChanges { block, ..Default::default() })
                        .unwrap();
                } else {
                    if let Some(notify) = resubscribed_tx.take() {
                        let _ = notify.send(std::time::Instant::now());
                    }
                    open_senders.push(tx);
                }
                Ok((Uuid::default(), rx))
            });
        deltas_client
            .expect_unsubscribe()
            .returning(|_| Ok(()));

        let cooldown = Duration::from_secs(5);
        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "test-protocol"),
            true,
            ComponentFilter::with_tvl_range(0.0, 1000.0),
            3,
            cooldown,
            false,
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            ArcDeltasClient(Arc::new(deltas_client)),
            10_u64,
        );
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        let started = std::time::Instant::now();
        let (handle, _rx) = state_sync.start().await;
        let resubscribed_at = timeout(cooldown * 2, resubscribed_rx)
            .await
            .expect("synchronizer should resubscribe")
            .unwrap();
        let (_jh, close_tx) = handle.split();
        let _ = close_tx.send(());

        assert!(resubscribed_at - started < Duration::from_secs(1));
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
    async fn test_a_revert_is_never_next_expected() {
        let mut state_sync = with_mocked_clients(true, false, None, None);
        state_sync.last_synced_block = Some(header_of(&make_block_changes(4, None)));
        // A revert to block 5 carries block 5's header, whose parent is block 4.
        let revert_to_5 =
            header_of(&BlockAggregatedChanges { revert: true, ..make_block_changes(5, None) });

        assert!(!state_sync.is_next_expected(&revert_to_5));
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
                .state_sync(&mut block_tx, end_rx, Duration::ZERO)
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
                .state_sync(&mut block_tx, end_rx, Duration::ZERO)
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

    fn header_of(changes: &BlockAggregatedChanges) -> BlockHeader {
        changes.into()
    }

    /// Mocks one tracked component and records the block of every snapshot request.
    fn mock_rpc_recording_snapshot_blocks() -> (MockRPCClient, Arc<std::sync::Mutex<Vec<u64>>>) {
        let mut rpc_client = make_mock_client();
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
        let snapshot_blocks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = snapshot_blocks.clone();
        rpc_client
            .expect_get_snapshots()
            .returning(move |request, _chunk_size, _concurrency| {
                recorded
                    .lock()
                    .unwrap()
                    .push(request.block_number);
                Ok(Snapshot::default())
            });
        (rpc_client, snapshot_blocks)
    }

    /// Returns a deltas client whose subscription emits `blocks` 50 ms apart.
    fn mock_deltas_emitting(blocks: Vec<u64>) -> MockDeltasClient {
        let mut deltas_client = MockDeltasClient::new();
        let (tx, rx) = channel(128);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                tokio::spawn(async move {
                    for block in blocks {
                        let _ = tx
                            .send(make_block_changes(block, None))
                            .await;
                        sleep(Duration::from_millis(50)).await;
                    }
                });
                Ok((Uuid::default(), rx))
            });
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));
        deltas_client
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_snapshot_delay_discards_deltas_until_it_ends() {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let deltas_client = mock_deltas_emitting((10..=20).collect());
        let mut state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(5, None)));

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx, Duration::from_millis(300))
                .await
        });

        let first = timeout(Duration::from_secs(1), block_rx.recv())
            .await
            .expect("Should receive first message")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let second = timeout(Duration::from_secs(1), block_rx.recv())
            .await
            .expect("Should receive second message")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = end_tx.send(());
        let _ = handle
            .await
            .expect("Task should not panic");

        // Blocks 10-15 arrive within the 300 ms delay, 50 ms apart.
        let snapshot_blocks = snapshot_blocks.lock().unwrap().clone();
        assert_eq!(snapshot_blocks.len(), 1, "Should request one snapshot");
        assert!(
            snapshot_blocks[0] >= 16,
            "Snapshot at block {} is inside the delay",
            snapshot_blocks[0]
        );
        assert_eq!(first.header.number, snapshot_blocks[0]);
        assert_eq!(second.header.number, snapshot_blocks[0] + 1);
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_snapshot_delay_skipped_when_next_block_expected() {
        let mut rpc_client = make_mock_client();
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
        rpc_client
            .expect_get_snapshots()
            .never();
        let deltas_client = mock_deltas_emitting(vec![10, 11]);
        let mut state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(9, None)));

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx, Duration::from_millis(300))
                .await
        });

        let msg = timeout(Duration::from_millis(10), block_rx.recv())
            .await
            .expect("Should receive the next expected block at once")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = end_tx.send(());
        let _ = handle
            .await
            .expect("Task should not panic");

        assert_eq!(msg.header.number, 10);
        assert!(msg.snapshots.states.is_empty());
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_first_start_has_no_snapshot_delay() {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let deltas_client = mock_deltas_emitting(vec![1]);
        let mut state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        assert_eq!(state_sync.snapshot_jitter, DEFAULT_SNAPSHOT_JITTER);
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        let msg = timeout(Duration::from_millis(10), block_rx.recv())
            .await
            .expect("Should receive the first block at once")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");

        assert_eq!(msg.header.number, 1);
        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![1]);
    }

    #[test]
    fn test_snapshot_jitter_has_a_minimum() {
        let state_sync = with_mocked_clients(true, false, None, None);
        let state_sync = state_sync.with_snapshot_jitter(Duration::ZERO);
        assert_eq!(state_sync.snapshot_jitter, MIN_SNAPSHOT_JITTER);

        let state_sync = state_sync.with_snapshot_jitter(Duration::from_secs(10));
        assert_eq!(state_sync.snapshot_jitter, Duration::from_secs(10));
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_retry_before_first_block_has_no_snapshot_delay() {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let mut deltas_client = MockDeltasClient::new();
        deltas_client
            .expect_subscribe()
            .times(1)
            .returning(|_, _| Err(DeltasError::ServerError("unavailable".to_string())));
        let (tx, rx) = channel(128);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| {
                tokio::spawn(async move {
                    for block in 1..=5 {
                        let _ = tx
                            .send(make_block_changes(block, None))
                            .await;
                        sleep(Duration::from_millis(50)).await;
                    }
                });
                Ok((Uuid::default(), rx))
            });
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));
        let mut state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        state_sync.max_retries = 2;
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();

        let msg = timeout(Duration::from_secs(1), block_rx.recv())
            .await
            .expect("Should receive the first block")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");

        assert_eq!(msg.header.number, 1);
        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![1]);
    }

    /// Starts `state_sync` as a restart after block 5 and returns the first block it emits.
    async fn first_block_after_restart(
        mut state_sync: ProtocolStateSynchronizer<
            ArcRPCClient<MockRPCClient>,
            ArcDeltasClient<MockDeltasClient>,
        >,
    ) -> u64 {
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(5, None)));

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();
        let msg = timeout(Duration::from_millis(10), block_rx.recv())
            .await
            .expect("Should receive the first block at once")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");
        msg.header.number
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_no_state_restart_has_no_snapshot_delay() {
        let mut rpc_client = make_mock_client();
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
        rpc_client
            .expect_get_snapshots()
            .never();
        let deltas_client = mock_deltas_emitting((10..=14).collect());
        let mut state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        state_sync.include_snapshots = false;

        assert_eq!(first_block_after_restart(state_sync).await, 10);
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_restart_without_components_has_no_snapshot_delay() {
        let mut rpc_client = make_mock_client();
        rpc_client
            .expect_get_protocol_components()
            .returning(|_| Ok(Page::new(vec![], 0, 0, 100)));
        rpc_client
            .expect_get_snapshots()
            .never();
        let deltas_client = mock_deltas_emitting((10..=14).collect());
        let state_sync = with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));

        assert_eq!(first_block_after_restart(state_sync).await, 10);
    }

    /// Returns a deltas client whose subscription receives what the returned sender sends.
    fn mock_deltas_channel() -> (MockDeltasClient, Sender<BlockAggregatedChanges>) {
        let mut deltas_client = MockDeltasClient::new();
        let (tx, rx) = channel(10);
        deltas_client
            .expect_subscribe()
            .return_once(move |_, _| Ok((Uuid::default(), rx)));
        deltas_client
            .expect_unsubscribe()
            .return_once(|_| Ok(()));
        (deltas_client, tx)
    }

    /// A synchronizer running one `state_sync` subscription, without a snapshot delay.
    struct RunningSync {
        block_rx: Receiver<SyncResult<StateSyncMessage<BlockHeader>>>,
        end_tx: oneshot::Sender<()>,
        task: JoinHandle<Result<(), Box<SyncError>>>,
    }

    impl RunningSync {
        /// Initializes `state_sync` and subscribes as if it had synced up to `last_synced`.
        async fn start<R, D>(
            mut state_sync: ProtocolStateSynchronizer<R, D>,
            last_synced: Option<BlockHeader>,
        ) -> Self
        where
            R: RPCClient + Clone + Send + Sync + 'static,
            D: DeltasClient + Clone + Send + Sync + 'static,
        {
            state_sync
                .initialize()
                .await
                .expect("Init should succeed");
            state_sync.last_synced_block = last_synced;
            let (mut block_tx, block_rx) = channel(10);
            let (end_tx, end_rx) = oneshot::channel();
            let task = tokio::spawn(async move {
                state_sync
                    .state_sync(&mut block_tx, end_rx, Duration::ZERO)
                    .await
            });
            Self { block_rx, end_tx, task }
        }

        async fn next(&mut self) -> StateSyncMessage<BlockHeader> {
            timeout(Duration::from_secs(1), self.block_rx.recv())
                .await
                .expect("Should emit a message")
                .expect("Channel should be open")
                .expect("Should not be an error")
        }

        /// Panics if the synchronizer emits a message within 100 ms.
        async fn assert_quiet(&mut self) {
            if let Ok(message) = timeout(Duration::from_millis(100), self.block_rx.recv()).await {
                panic!("Unexpected message: {message:?}");
            }
        }

        async fn stop(self) {
            let _ = self.end_tx.send(());
            let _ = self
                .task
                .await
                .expect("Task should not panic");
        }
    }

    /// A block whose DCI update links a new entry point to `component` and traces `contract`, and
    /// that changes a slot of `contract`.
    fn block_tracing_contract(
        block_num: u64,
        component: &str,
        contract: &Bytes,
    ) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            dci_update: DCIUpdate {
                new_entrypoints: HashMap::from([(
                    component.to_string(),
                    HashSet::from([EntryPoint {
                        external_id: "entrypoint_x".to_string(),
                        target: contract.clone(),
                        signature: "sig()".to_string(),
                    }]),
                )]),
                trace_results: HashMap::from([(
                    "entrypoint_x".to_string(),
                    TracingResult {
                        retriggers: HashSet::new(),
                        accessed_slots: HashMap::from([(
                            contract.clone(),
                            HashSet::from([Bytes::from("0x01")]),
                        )]),
                    },
                )]),
                ..Default::default()
            },
            ..block_changing_slot(block_num, contract)
        }
    }

    /// A block that only changes a slot of `contract`.
    fn block_changing_slot(block_num: u64, contract: &Bytes) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            account_deltas: HashMap::from([(
                contract.clone(),
                AccountDelta::new(
                    Chain::Ethereum,
                    contract.clone(),
                    HashMap::from([(
                        Bytes::from("0x01"),
                        Some(Bytes::from(vec![block_num as u8])),
                    )]),
                    None,
                    None,
                    ChangeType::Update,
                ),
            )]),
            ..make_block_changes(block_num, None)
        }
    }

    fn has_account_delta(msg: &StateSyncMessage<BlockHeader>, contract: &Bytes) -> bool {
        msg.deltas
            .as_ref()
            .is_some_and(|d| d.account_deltas.contains_key(contract))
    }

    /// A snapshot with an empty state for every component `request` asks for.
    fn snapshot_of_requested(request: &SnapshotParameters) -> Snapshot {
        let states = request
            .components
            .values()
            .map(|component| {
                let state = ComponentWithState {
                    state: ProtocolComponentState::new(
                        &component.id,
                        Default::default(),
                        Default::default(),
                    ),
                    component: component.clone(),
                    component_tvl: None,
                    entrypoints: vec![],
                };
                (component.id.clone(), state)
            })
            .collect();
        Snapshot { states, vm_storage: HashMap::new() }
    }

    /// A block that puts `component`'s TVL below the remove threshold (50) of
    /// `with_mocked_clients`.
    fn block_dropping_tvl(block_num: u64, component: &str) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            component_tvl: HashMap::from([(component.to_string(), 10.0)]),
            ..make_block_changes(block_num, None)
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_resume_tracks_contract_traced_in_first_message() {
        let mut rpc_client = make_mock_client();
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
        rpc_client
            .expect_get_snapshots()
            .never();
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client)).with_dci(true);
        let contract_x = Bytes::from("0xc0ffee");
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(5, None)))).await;

        // Block 6 follows block 5, so the subscription resumes without a snapshot.
        deltas_tx
            .send(block_tracing_contract(6, "Component1", &contract_x))
            .await
            .unwrap();
        deltas_tx
            .send(block_changing_slot(7, &contract_x))
            .await
            .unwrap();
        let first = sync.next().await;
        let second = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 6);
        assert!(has_account_delta(&first, &contract_x), "The traced contract's delta is dropped");
        assert_eq!(second.header.number, 7);
        assert!(has_account_delta(&second, &contract_x), "The traced contract is not tracked");
    }

    #[test_log::test(tokio::test)]
    async fn test_resume_tracks_component_crossing_tvl_in_first_message() {
        let bg_done = Arc::new(tokio::sync::Notify::new());
        let mut rpc_client = make_mock_client();
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
        let snapshot_blocks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = snapshot_blocks.clone();
        let notify = bg_done.clone();
        rpc_client
            .expect_get_snapshots()
            .returning(move |request, _chunk_size, _concurrency| {
                recorded
                    .lock()
                    .unwrap()
                    .push(request.block_number);
                notify.notify_one();
                Ok(snapshot_of_requested(request))
            });
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync = with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(5, None)))).await;

        // Component3 crosses the TVL threshold (50) in block 6, which follows block 5.
        deltas_tx
            .send(BlockAggregatedChanges {
                component_tvl: HashMap::from([("Component3".to_string(), 100.0)]),
                ..make_block_changes(6, None)
            })
            .await
            .unwrap();
        let first = sync.next().await;
        timeout(Duration::from_secs(1), bg_done.notified())
            .await
            .expect("Component3's snapshot should be requested");
        deltas_tx
            .send(BlockAggregatedChanges {
                state_deltas: HashMap::from([(
                    "Component3".to_string(),
                    ProtocolComponentStateDelta::new(
                        "Component3",
                        HashMap::from([("reserve".to_string(), Bytes::from("0x07"))]),
                        HashSet::new(),
                    ),
                )]),
                ..make_block_changes(7, None)
            })
            .await
            .unwrap();
        let second = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 6);
        assert!(first.snapshots.states.is_empty());
        // The resume itself needs no snapshot; Component3's is fetched at the block it crossed in.
        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![6]);
        assert!(second
            .snapshots
            .states
            .contains_key("Component3"));
        assert!(second.deltas.as_ref().is_some_and(|d| d
            .state_deltas
            .contains_key("Component3")));
    }

    #[test_log::test(tokio::test)]
    async fn test_resume_from_revert_to_next_block_takes_snapshot() {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync = with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(5, None)))).await;

        // A revert to block 6 has block 6's header, whose parent is block 5, but carries only
        // the keys the reverted blocks above 6 changed, not block 6's own changes.
        deltas_tx
            .send(BlockAggregatedChanges { revert: true, ..make_block_changes(6, None) })
            .await
            .unwrap();
        let first = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 6);
        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![6]);
    }

    #[test_log::test(tokio::test)]
    async fn test_snapshot_start_tracks_contract_traced_in_first_message() {
        let mut rpc_client = make_mock_client();
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
        // The server reads traced entry points from its database, which lags behind the
        // subscription: the snapshot knows nothing of the first message's DCI update.
        rpc_client
            .expect_get_traced_entry_points()
            .returning(|_| Ok(Page::new(HashMap::new(), 0, 0, 0)));
        rpc_client
            .expect_get_snapshots()
            .returning(|_request, _chunk_size, _concurrency| Ok(Snapshot::default()));
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync =
            with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client)).with_dci(true);
        let contract_x = Bytes::from("0xc0ffee");
        let mut sync = RunningSync::start(state_sync, None).await;

        deltas_tx
            .send(block_tracing_contract(1, "Component1", &contract_x))
            .await
            .unwrap();
        deltas_tx
            .send(block_changing_slot(2, &contract_x))
            .await
            .unwrap();
        let first = sync.next().await;
        let second = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 1);
        assert!(has_account_delta(&first, &contract_x), "The traced contract's delta is dropped");
        assert_eq!(second.header.number, 2);
        assert!(has_account_delta(&second, &contract_x), "The traced contract is not tracked");
    }

    #[test_log::test(tokio::test)]
    async fn test_snapshot_restart_removes_component_dropping_below_tvl_in_first_message() {
        let mut rpc_client = make_mock_client();
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
        rpc_client
            .expect_get_snapshots()
            .returning(|request, _chunk_size, _concurrency| Ok(snapshot_of_requested(request)));
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync = with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(5, None)))).await;

        // Block 7 doesn't follow block 5, so the snapshot is taken at block 7 while Component1 is
        // still tracked; block 7 then drops Component1 below the threshold.
        deltas_tx
            .send(block_dropping_tvl(7, "Component1"))
            .await
            .unwrap();
        let first = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 7);
        assert!(first
            .removed_components
            .contains_key("Component1"));
        assert!(!first
            .snapshots
            .states
            .contains_key("Component1"));
    }

    #[test_log::test(tokio::test)]
    async fn test_stale_snapshot_discards_first_message_without_side_effects() {
        let mut rpc_client = make_mock_client();
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
        let snapshot_blocks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = snapshot_blocks.clone();
        rpc_client
            .expect_get_snapshots()
            .returning(move |request, _chunk_size, _concurrency| {
                let mut blocks = recorded.lock().unwrap();
                blocks.push(request.block_number);
                if blocks.len() == 1 {
                    return Err(RPCError::StaleBlock("outside the retention window".to_string()));
                }
                Ok(snapshot_of_requested(request))
            });
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync = with_mocked_clients(true, false, Some(rpc_client), Some(deltas_client));
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(5, None)))).await;

        // Block 7 drops Component1 below the threshold, but its snapshot is stale: the
        // synchronizer discards block 7 and takes the snapshot at block 8 instead.
        deltas_tx
            .send(block_dropping_tvl(7, "Component1"))
            .await
            .unwrap();
        deltas_tx
            .send(make_block_changes(8, None))
            .await
            .unwrap();
        let first = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 8);
        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![7, 8]);
        assert!(
            first
                .snapshots
                .states
                .contains_key("Component1"),
            "The discarded block 7 stopped tracking Component1"
        );
        assert!(first.removed_components.is_empty());
    }

    /// A deltas client whose subscriptions all report `fingerprint`. `ArcDeltasClient` keeps the
    /// trait's default, which reports none.
    #[derive(Clone)]
    struct FingerprintedDeltasClient {
        inner: ArcDeltasClient<MockDeltasClient>,
        fingerprint: Option<ExtractorFingerprint>,
    }

    #[async_trait]
    impl DeltasClient for FingerprintedDeltasClient {
        async fn subscribe(
            &self,
            extractor_id: ExtractorIdentity,
            options: SubscriptionOptions,
        ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError> {
            self.inner
                .subscribe(extractor_id, options)
                .await
        }

        async fn subscribe_with_fingerprint(
            &self,
            extractor_id: ExtractorIdentity,
            options: SubscriptionOptions,
        ) -> Result<
            (Uuid, Receiver<BlockAggregatedChanges>, Option<ExtractorFingerprint>),
            DeltasError,
        > {
            let (subscription_id, receiver) = self
                .inner
                .subscribe(extractor_id, options)
                .await?;
            Ok((subscription_id, receiver, self.fingerprint.clone()))
        }

        async fn unsubscribe(&self, subscription_id: Uuid) -> Result<(), DeltasError> {
            self.inner
                .unsubscribe(subscription_id)
                .await
        }

        async fn connect(&self) -> Result<JoinHandle<Result<(), DeltasError>>, DeltasError> {
            self.inner.connect().await
        }

        async fn close(&self) -> Result<(), DeltasError> {
            self.inner.close().await
        }
    }

    fn fingerprint(byte: u8) -> ExtractorFingerprint {
        ExtractorFingerprint(Bytes::from(vec![byte]))
    }

    /// A deltas client that forwards every method to the mock, `subscribe_with_fingerprint`
    /// included, so each subscription's expectation sets the fingerprint it reports.
    #[derive(Clone)]
    struct ForwardingDeltasClient(Arc<MockDeltasClient>);

    #[async_trait]
    impl DeltasClient for ForwardingDeltasClient {
        async fn subscribe(
            &self,
            extractor_id: ExtractorIdentity,
            options: SubscriptionOptions,
        ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError> {
            self.0
                .subscribe(extractor_id, options)
                .await
        }

        async fn subscribe_with_fingerprint(
            &self,
            extractor_id: ExtractorIdentity,
            options: SubscriptionOptions,
        ) -> Result<
            (Uuid, Receiver<BlockAggregatedChanges>, Option<ExtractorFingerprint>),
            DeltasError,
        > {
            self.0
                .subscribe_with_fingerprint(extractor_id, options)
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

    /// Like `with_mocked_clients(true, false, ..)`, but with any deltas client.
    fn with_deltas_client<D>(
        rpc_client: MockRPCClient,
        deltas_client: D,
    ) -> ProtocolStateSynchronizer<ArcRPCClient<MockRPCClient>, D>
    where
        D: DeltasClient + Clone + Send + Sync + 'static,
    {
        ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            true,
            ComponentFilter::with_tvl_range(50.0, 50.0),
            1,
            Duration::from_secs(0),
            true,
            false,
            true,
            ArcRPCClient(Arc::new(rpc_client)),
            deltas_client,
            10_u64,
        )
    }

    /// Like `with_mocked_clients(true, false, ..)`, but every subscription reports `fingerprint`.
    fn with_fingerprinted_clients(
        rpc_client: MockRPCClient,
        deltas_client: MockDeltasClient,
        fingerprint: Option<ExtractorFingerprint>,
    ) -> ProtocolStateSynchronizer<ArcRPCClient<MockRPCClient>, FingerprintedDeltasClient> {
        with_deltas_client(
            rpc_client,
            FingerprintedDeltasClient {
                inner: ArcDeltasClient(Arc::new(deltas_client)),
                fingerprint,
            },
        )
    }

    #[rstest]
    #[case::unchanged(Some(fingerprint(1)), Some(fingerprint(1)), vec![])]
    #[case::upgraded(Some(fingerprint(1)), Some(fingerprint(2)), vec![6])]
    #[case::server_predates_fingerprints(None, None, vec![])]
    #[case::server_stopped_reporting_one(Some(fingerprint(1)), None, vec![6])]
    #[case::server_started_reporting_one(None, Some(fingerprint(1)), vec![6])]
    #[test_log::test(tokio::test)]
    async fn test_restart_resumes_only_under_the_synced_fingerprint(
        #[case] synced: Option<ExtractorFingerprint>,
        #[case] reported: Option<ExtractorFingerprint>,
        #[case] expected_snapshot_blocks: Vec<u64>,
    ) {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let mut state_sync = with_fingerprinted_clients(rpc_client, deltas_client, reported);
        state_sync.synced_fingerprint = synced;
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(5, None)))).await;

        // The restarted extractor replays blocks 4 and 5, which the consumer already applied,
        // then block 6, which follows block 5. An upgraded extractor may have emitted other
        // deltas for blocks 4 and 5 than the ones the consumer applied.
        for block in [4, 5, 6] {
            deltas_tx
                .send(make_block_changes(block, None))
                .await
                .unwrap();
        }
        let first = sync.next().await;
        sync.stop().await;

        assert_eq!(first.header.number, 6);
        assert_eq!(*snapshot_blocks.lock().unwrap(), expected_snapshot_blocks);
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_snapshot_delay_applies_when_the_fingerprint_changed() {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let deltas_client = mock_deltas_emitting((10..=20).collect());
        let mut state_sync =
            with_fingerprinted_clients(rpc_client, deltas_client, Some(fingerprint(2)));
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(9, None)));
        state_sync.synced_fingerprint = Some(fingerprint(1));

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx, Duration::from_millis(300))
                .await
        });

        let first = timeout(Duration::from_secs(1), block_rx.recv())
            .await
            .expect("Should receive first message")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = end_tx.send(());
        let _ = handle
            .await
            .expect("Task should not panic");

        // Block 10 follows block 9, but the fingerprint changed. Blocks 10-15 arrive within the
        // 300 ms delay, 50 ms apart.
        let snapshot_blocks = snapshot_blocks.lock().unwrap().clone();
        assert_eq!(snapshot_blocks.len(), 1, "Should request one snapshot");
        assert!(
            snapshot_blocks[0] >= 16,
            "Snapshot at block {} is inside the delay",
            snapshot_blocks[0]
        );
        assert_eq!(first.header.number, snapshot_blocks[0]);
    }

    #[test_log::test(tokio::test)]
    async fn test_restart_after_a_resync_resumes_under_the_new_fingerprint() {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let mut deltas_client = MockDeltasClient::new();
        // Two subscriptions, each ending after one block.
        for block in [6, 7] {
            let (tx, rx) = channel(1);
            tx.try_send(make_block_changes(block, None))
                .unwrap();
            deltas_client
                .expect_subscribe()
                .times(1)
                .return_once(move |_, _| Ok((Uuid::default(), rx)));
        }
        deltas_client
            .expect_unsubscribe()
            .returning(|_| Ok(()));
        let mut state_sync =
            with_fingerprinted_clients(rpc_client, deltas_client, Some(fingerprint(2)));
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(5, None)));
        state_sync.synced_fingerprint = Some(fingerprint(1));

        let (mut block_tx, mut block_rx) = channel(10);
        let (_end_tx, end_rx) = oneshot::channel::<()>();
        let ended = state_sync
            .state_sync(&mut block_tx, end_rx, Duration::ZERO)
            .await
            .expect_err("The first subscription ends");
        let (_, Some(end_rx)) = *ended else {
            panic!("The close signal was never sent");
        };
        state_sync
            .state_sync(&mut block_tx, end_rx, Duration::ZERO)
            .await
            .expect_err("The second subscription ends");

        let mut emitted = Vec::new();
        while let Ok(message) = block_rx.try_recv() {
            emitted.push(
                message
                    .expect("Should not be an error")
                    .header
                    .number,
            );
        }
        assert_eq!(emitted, vec![6, 7]);
        // The fingerprint changed before block 6, so the consumer resynced under the new one;
        // block 7 then follows block 6 under the same fingerprint.
        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![6]);
    }

    /// Runs a synchronizer through `start()` with a 5 s snapshot jitter. Its first subscription
    /// delivers blocks 1 and 2 under fingerprint 1, then ends. The second reports
    /// `second_fingerprint` and delivers block 3, which follows block 2, then a block per second.
    /// Returns the blocks of the first three messages it emits and of every snapshot request.
    async fn restart_reporting(second_fingerprint: ExtractorFingerprint) -> (Vec<u64>, Vec<u64>) {
        let (rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        let mut deltas_client = MockDeltasClient::new();
        let (first_tx, first_rx) = channel(2);
        for block in [1, 2] {
            first_tx
                .try_send(make_block_changes(block, None))
                .unwrap();
        }
        drop(first_tx);
        deltas_client
            .expect_subscribe_with_fingerprint()
            .times(1)
            .return_once(move |_, _| Ok((Uuid::default(), first_rx, Some(fingerprint(1)))));
        let (second_tx, second_rx) = channel(16);
        deltas_client
            .expect_subscribe_with_fingerprint()
            .times(1)
            .return_once(move |_, _| {
                tokio::spawn(async move {
                    for block in 3..=10 {
                        let _ = second_tx
                            .send(make_block_changes(block, None))
                            .await;
                        sleep(Duration::from_secs(1)).await;
                    }
                    second_tx.closed().await;
                });
                Ok((Uuid::default(), second_rx, Some(second_fingerprint)))
            });
        deltas_client
            .expect_unsubscribe()
            .returning(|_| Ok(()));
        let mut state_sync =
            with_deltas_client(rpc_client, ForwardingDeltasClient(Arc::new(deltas_client)))
                .with_snapshot_jitter(MIN_SNAPSHOT_JITTER);
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");

        let (handle, mut block_rx) = state_sync.start().await;
        let (jh, close_tx) = handle.split();
        let mut emitted = Vec::new();
        for _ in 0..3 {
            let message = timeout(Duration::from_secs(30), block_rx.recv())
                .await
                .expect("Should emit a message")
                .expect("Channel should be open")
                .expect("Should not be an error");
            emitted.push(message.header.number);
        }
        let _ = close_tx.send(());
        jh.await.expect("Task should not panic");

        let snapshot_blocks = snapshot_blocks.lock().unwrap().clone();
        (emitted, snapshot_blocks)
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_restart_through_start_resumes_under_an_unchanged_fingerprint() {
        let (emitted, snapshot_blocks) = restart_reporting(fingerprint(1)).await;

        assert_eq!(emitted, vec![1, 2, 3]);
        assert_eq!(snapshot_blocks, vec![1]);
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_restart_through_start_takes_a_delayed_snapshot_after_an_upgrade() {
        let (emitted, snapshot_blocks) = restart_reporting(fingerprint(2)).await;

        assert_eq!(emitted[..2], [1, 2]);
        // Block 3 follows block 2 but starts the delay instead; the snapshot comes at the first
        // block at least the delay (at most 5 s, a block per second) after it.
        let restarted_at = emitted[2];
        assert!((4..=8).contains(&restarted_at), "Snapshot at block {restarted_at}");
        assert_eq!(snapshot_blocks, vec![1, restarted_at]);
    }

    /// Hash of block `number` on fork `fork`. Fork 0 is the chain of `make_block_changes`.
    fn fork_hash(number: u64, fork: u8) -> Bytes {
        let mut hash = vec![number as u8; 32];
        hash[31] ^= fork;
        Bytes::from(hash)
    }

    /// Block `number` of fork `fork`, on top of block `number - 1` of fork `parent_fork`.
    fn fork_block(number: u64, fork: u8, parent_fork: u8) -> BlockAggregatedChanges {
        let mut block = make_block_changes(number, None);
        block.block.hash = fork_hash(number, fork);
        block.block.parent_hash = fork_hash(number - 1, parent_fork);
        block
    }

    /// The revert to `target`: it carries the target block's header.
    fn revert_to(target: BlockAggregatedChanges) -> BlockAggregatedChanges {
        BlockAggregatedChanges { revert: true, ..target }
    }

    /// `block`, also setting Component1's attribute `name` to `value`.
    fn setting(
        mut block: BlockAggregatedChanges,
        name: &str,
        value: &str,
    ) -> BlockAggregatedChanges {
        block
            .state_deltas
            .entry("Component1".to_string())
            .or_insert_with(|| {
                ProtocolComponentStateDelta::new("Component1", HashMap::new(), HashSet::new())
            })
            .updated_attributes
            .insert(name.to_string(), Bytes::from(value));
        block
    }

    fn attributes(pairs: &[(&str, &str)]) -> HashMap<String, Bytes> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), Bytes::from(*value)))
            .collect()
    }

    /// The attributes of Component1 that `message` sets.
    fn attributes_set(message: &StateSyncMessage<BlockHeader>) -> HashMap<String, Bytes> {
        message
            .deltas
            .as_ref()
            .and_then(|deltas| deltas.state_deltas.get("Component1"))
            .map(|delta| delta.updated_attributes.clone())
            .unwrap_or_default()
    }

    /// The requests the RPC mock of `mock_rpc_answering_catch_up` received.
    struct RpcCalls {
        catch_up_requests: Arc<std::sync::Mutex<Vec<DeltaCatchUpParams>>>,
        snapshot_blocks: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl RpcCalls {
        fn catch_up_requests(&self) -> Vec<DeltaCatchUpParams> {
            self.catch_up_requests
                .lock()
                .unwrap()
                .clone()
        }

        fn snapshot_blocks(&self) -> Vec<u64> {
            self.snapshot_blocks
                .lock()
                .unwrap()
                .clone()
        }
    }

    /// Mocks an RPC that tracks Component1, knows every component asked for by id, serves a
    /// snapshot with an empty state for every component it is asked for, and answers one catch-up
    /// request with `answer`.
    fn mock_rpc_answering_catch_up(
        answer: Result<DeltaCatchUp, RPCError>,
    ) -> (MockRPCClient, RpcCalls) {
        let mut rpc_client = make_mock_client();
        rpc_client
            .expect_get_protocol_components()
            .returning(|params| {
                let ids = params
                    .component_ids()
                    .cloned()
                    .unwrap_or_else(|| vec!["Component1".to_string()]);
                let components: Vec<_> = ids
                    .into_iter()
                    .map(|id| ProtocolComponent { id, ..Default::default() })
                    .collect();
                let total = components.len() as i64;
                Ok(Page::new(components, total, 0, 100))
            });
        let calls =
            RpcCalls { catch_up_requests: Default::default(), snapshot_blocks: Default::default() };
        let requests = calls.catch_up_requests.clone();
        rpc_client
            .expect_get_delta_catch_up()
            .return_once(move |params| {
                requests.lock().unwrap().push(params);
                answer
            });
        let snapshot_blocks = calls.snapshot_blocks.clone();
        rpc_client
            .expect_get_snapshots()
            .returning(move |request, _chunk_size, _concurrency| {
                snapshot_blocks
                    .lock()
                    .unwrap()
                    .push(request.block_number);
                Ok(snapshot_of_requested(request))
            });
        (rpc_client, calls)
    }

    /// A synchronizer that synced under fingerprint 1 and whose subscriptions report it again.
    fn synced_under_an_unchanged_fingerprint(
        rpc_client: MockRPCClient,
        deltas_client: MockDeltasClient,
    ) -> ProtocolStateSynchronizer<ArcRPCClient<MockRPCClient>, FingerprintedDeltasClient> {
        let mut state_sync =
            with_fingerprinted_clients(rpc_client, deltas_client, Some(fingerprint(1)));
        state_sync.synced_fingerprint = Some(fingerprint(1));
        state_sync
    }

    /// Starts a synchronizer that synced up to `last_synced` and resubscribes under an unchanged
    /// fingerprint. Returns it and the sender of its new subscription.
    async fn resubscribe(
        rpc_client: MockRPCClient,
        last_synced: &BlockAggregatedChanges,
    ) -> (RunningSync, Sender<BlockAggregatedChanges>) {
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync = synced_under_an_unchanged_fingerprint(rpc_client, deltas_client);
        (RunningSync::start(state_sync, Some(header_of(last_synced))).await, deltas_tx)
    }

    /// The catch-up request of a client at `last_synced`, under fingerprint 1, for block `to`.
    fn catch_up_request(last_synced: &BlockAggregatedChanges, to: &Bytes) -> DeltaCatchUpParams {
        DeltaCatchUpParams::new(
            Chain::Ethereum,
            "uniswap-v2",
            last_synced.block.hash.clone(),
            last_synced.block.number,
            to.clone(),
            fingerprint(1),
        )
    }

    /// A client that synced up to `last_synced` resubscribes; its subscription delivers `live`,
    /// and the server answers the catch-up request with `path`. The client must ask for the path
    /// to block `to` and emit one message, with the last live message's header, that sets
    /// `expected` on Component1.
    struct CatchUpScenario {
        last_synced: BlockAggregatedChanges,
        path: Vec<BlockAggregatedChanges>,
        live: Vec<BlockAggregatedChanges>,
        to: Bytes,
        expected: HashMap<String, Bytes>,
    }

    /// The client applied blocks 3 and 4. While it was away, the chain reverted to block 2 and
    /// built 3', 4' and 5' (fork 1); its new subscription starts at 5'.
    fn revert_below_the_client() -> CatchUpScenario {
        CatchUpScenario {
            last_synced: make_block_changes(4, None),
            path: vec![
                setting(revert_to(make_block_changes(2, None)), "x", "0x02"),
                setting(fork_block(3, 1, 0), "y", "0x13"),
                setting(fork_block(4, 1, 1), "z", "0x14"),
            ],
            live: vec![setting(fork_block(5, 1, 1), "w", "0x15")],
            to: fork_hash(4, 1),
            expected: attributes(&[("x", "0x02"), ("y", "0x13"), ("z", "0x14"), ("w", "0x15")]),
        }
    }

    /// The client applied 3a of a branch (fork 1) that grew to 5a before the chain reverted to
    /// block 2 and built 3' to 5' (fork 2). The revert also restores y, which only 4a, a block
    /// the client never saw, changed.
    fn client_mid_branch() -> CatchUpScenario {
        CatchUpScenario {
            last_synced: fork_block(3, 1, 0),
            path: vec![
                setting(setting(revert_to(make_block_changes(2, None)), "x", "0x02"), "y", "0x"),
                setting(fork_block(3, 2, 0), "z", "0x23"),
                setting(fork_block(4, 2, 2), "z", "0x24"),
            ],
            live: vec![setting(fork_block(5, 2, 2), "w", "0x25")],
            to: fork_hash(4, 2),
            expected: attributes(&[("x", "0x02"), ("y", "0x"), ("z", "0x24"), ("w", "0x25")]),
        }
    }

    /// The client applied 3a and 4a (fork 1). While it was away, the chain reverted to 3a, built
    /// 4b and 5b (fork 2), reverted to block 2 and built 3c to 5c (fork 3). The path walks
    /// through the revert that orphaned 4a, then the one that orphaned 3a.
    fn nested_reverts() -> CatchUpScenario {
        CatchUpScenario {
            last_synced: fork_block(4, 1, 1),
            path: vec![
                setting(revert_to(fork_block(3, 1, 0)), "x", "0x13"),
                setting(revert_to(make_block_changes(2, None)), "x", "0x02"),
                setting(fork_block(3, 3, 0), "y", "0x33"),
                setting(fork_block(4, 3, 3), "z", "0x34"),
            ],
            live: vec![setting(fork_block(5, 3, 3), "w", "0x35")],
            to: fork_hash(4, 3),
            expected: attributes(&[("x", "0x02"), ("y", "0x33"), ("z", "0x34"), ("w", "0x35")]),
        }
    }

    /// Like `revert_below_the_client`, but the new subscription starts with the revert, 3' and 4'.
    /// They are at or below the client's block, so it skips them and catches up at 5'.
    fn first_live_message_reverts_below_the_client() -> CatchUpScenario {
        let scenario = revert_below_the_client();
        let mut live = scenario.path.clone();
        live.extend(scenario.live);
        CatchUpScenario { live, ..scenario }
    }

    /// The client applied block 4. While it was away, the chain built 5 and 6, then reverted to
    /// 5; the revert is the first message of its new subscription. The revert restores only what
    /// 6 changed, so block 5's own changes come from the path.
    fn first_live_message_reverts_to_the_block_after_the_client() -> CatchUpScenario {
        CatchUpScenario {
            last_synced: make_block_changes(4, None),
            path: vec![setting(make_block_changes(5, None), "x", "0x05")],
            live: vec![setting(revert_to(make_block_changes(5, None)), "y", "0x05")],
            to: fork_hash(5, 0),
            expected: attributes(&[("x", "0x05"), ("y", "0x05")]),
        }
    }

    /// Like `first_live_message_reverts_to_the_block_after_the_client`, but the server's window
    /// is ahead of the client: it already applied the revert the client receives live, so the
    /// path ends with that revert.
    fn path_ends_with_the_live_revert() -> CatchUpScenario {
        let revert = setting(revert_to(make_block_changes(5, None)), "y", "0x05");
        CatchUpScenario {
            last_synced: make_block_changes(4, None),
            path: vec![
                setting(make_block_changes(5, None), "x", "0x05"),
                setting(make_block_changes(6, None), "y", "0x06"),
                revert.clone(),
            ],
            live: vec![revert],
            to: fork_hash(5, 0),
            expected: attributes(&[("x", "0x05"), ("y", "0x05")]),
        }
    }

    /// The client stopped at block 4, the extractor's last committed block. The extractor
    /// restarted unchanged and replayed from block 5, so the server's window starts there; the
    /// client's new subscription starts at 8.
    fn restart_at_the_last_committed_block() -> CatchUpScenario {
        CatchUpScenario {
            last_synced: make_block_changes(4, None),
            path: vec![
                setting(make_block_changes(5, None), "x", "0x05"),
                setting(make_block_changes(6, None), "y", "0x06"),
                setting(make_block_changes(7, None), "x", "0x07"),
            ],
            live: vec![setting(make_block_changes(8, None), "z", "0x08")],
            to: fork_hash(7, 0),
            expected: attributes(&[("x", "0x07"), ("y", "0x06"), ("z", "0x08")]),
        }
    }

    #[rstest]
    #[case::revert_below_the_client(revert_below_the_client())]
    #[case::client_mid_branch(client_mid_branch())]
    #[case::nested_reverts(nested_reverts())]
    #[case::first_live_message_reverts_below_the_client(
        first_live_message_reverts_below_the_client()
    )]
    #[case::first_live_message_reverts_to_the_block_after_the_client(
        first_live_message_reverts_to_the_block_after_the_client()
    )]
    #[case::path_ends_with_the_live_revert(path_ends_with_the_live_revert())]
    #[case::restart_at_the_last_committed_block(restart_at_the_last_committed_block())]
    #[test_log::test(tokio::test)]
    async fn test_catch_up_emits_one_message_with_the_missed_deltas(
        #[case] scenario: CatchUpScenario,
    ) {
        let CatchUpScenario { last_synced, path, live, to, expected } = scenario;
        let first = live
            .last()
            .expect("A live message")
            .clone();
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(path)));
        let (mut sync, deltas_tx) = resubscribe(rpc_client, &last_synced).await;

        for message in live {
            deltas_tx.send(message).await.unwrap();
        }
        let message = sync.next().await;
        sync.assert_quiet().await;
        sync.stop().await;

        assert_eq!(calls.catch_up_requests(), vec![catch_up_request(&last_synced, &to)]);
        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
        assert_eq!(message.header, header_of(&first));
        assert!(message.snapshots.states.is_empty());
        assert_eq!(attributes_set(&message), expected);
    }

    #[test_log::test(tokio::test)]
    async fn test_catch_up_merges_the_missed_blocks_into_the_first_message() {
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            setting(make_block_changes(5, None), "a", "0x05"),
            setting(make_block_changes(6, None), "b", "0x06"),
        ])));
        let (mut sync, deltas_tx) = resubscribe(rpc_client, &make_block_changes(4, None)).await;

        deltas_tx
            .send(setting(make_block_changes(7, None), "c", "0x07"))
            .await
            .unwrap();
        let first = sync.next().await;
        sync.assert_quiet().await;
        deltas_tx
            .send(setting(make_block_changes(8, None), "d", "0x08"))
            .await
            .unwrap();
        let second = sync.next().await;
        sync.stop().await;

        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
        assert_eq!(first.header, header_of(&make_block_changes(7, None)));
        assert_eq!(
            attributes_set(&first),
            attributes(&[("a", "0x05"), ("b", "0x06"), ("c", "0x07")])
        );
        assert_eq!(second.header.number, 8);
        assert_eq!(attributes_set(&second), attributes(&[("d", "0x08")]));
    }

    #[test_log::test(tokio::test)]
    async fn test_restart_after_a_catch_up_resumes_after_its_first_message() {
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
        ])));
        let mut deltas_client = MockDeltasClient::new();
        // Two subscriptions, each ending after one block.
        for block in [7, 8] {
            let (tx, rx) = channel(1);
            tx.try_send(make_block_changes(block, None))
                .unwrap();
            deltas_client
                .expect_subscribe()
                .times(1)
                .return_once(move |_, _| Ok((Uuid::default(), rx)));
        }
        deltas_client
            .expect_unsubscribe()
            .returning(|_| Ok(()));
        let mut state_sync = synced_under_an_unchanged_fingerprint(rpc_client, deltas_client);
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(4, None)));

        let (mut block_tx, mut block_rx) = channel(10);
        let (_end_tx, end_rx) = oneshot::channel::<()>();
        let ended = state_sync
            .state_sync(&mut block_tx, end_rx, Duration::ZERO)
            .await
            .expect_err("The first subscription ends");
        let (_, Some(end_rx)) = *ended else {
            panic!("The close signal was never sent");
        };
        state_sync
            .state_sync(&mut block_tx, end_rx, Duration::ZERO)
            .await
            .expect_err("The second subscription ends");

        let mut emitted = Vec::new();
        while let Ok(message) = block_rx.try_recv() {
            emitted.push(
                message
                    .expect("Should not be an error")
                    .header
                    .number,
            );
        }
        assert_eq!(emitted, vec![7, 8]);
        // Block 8 follows block 7, the first message of the subscription that caught up.
        assert_eq!(calls.catch_up_requests().len(), 1);
        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
    }

    fn contract_x() -> Bytes {
        Bytes::from("0xc0ffee")
    }

    #[rstest]
    #[case::by_the_first_message(
        vec![make_block_changes(5, None), make_block_changes(6, None)],
        block_tracing_contract(7, "Component1", &contract_x()),
    )]
    // The missed block traces the contract without changing it, so the first message carries the
    // only change the caught-up message can hold for it.
    #[case::by_a_missed_block(
        vec![
            BlockAggregatedChanges {
                account_deltas: HashMap::new(),
                ..block_tracing_contract(5, "Component1", &contract_x())
            },
            make_block_changes(6, None),
        ],
        block_changing_slot(7, &contract_x()),
    )]
    #[test_log::test(tokio::test)]
    async fn test_catch_up_keeps_the_deltas_of_a_newly_traced_contract(
        #[case] path: Vec<BlockAggregatedChanges>,
        #[case] first: BlockAggregatedChanges,
    ) {
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(path)));
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let state_sync =
            synced_under_an_unchanged_fingerprint(rpc_client, deltas_client).with_dci(true);
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(4, None)))).await;

        deltas_tx.send(first).await.unwrap();
        deltas_tx
            .send(block_changing_slot(8, &contract_x()))
            .await
            .unwrap();
        let caught_up = sync.next().await;
        let next = sync.next().await;
        sync.stop().await;

        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
        assert_eq!(caught_up.header.number, 7);
        assert!(
            has_account_delta(&caught_up, &contract_x()),
            "The traced contract's delta is dropped"
        );
        assert_eq!(next.header.number, 8);
        assert!(has_account_delta(&next, &contract_x()), "The traced contract is not tracked");
    }

    #[test_log::test(tokio::test)]
    async fn test_catch_up_over_the_deletion_of_a_component_the_client_never_had() {
        // The client applied block 4. While it was away, block 5 created Component9, then the
        // chain reverted to block 2, which deletes Component9, and built 3' and 4' (fork 1).
        let component9 = ProtocolComponent { id: "Component9".to_string(), ..Default::default() };
        let revert = BlockAggregatedChanges {
            deleted_protocol_components: HashMap::from([("Component9".to_string(), component9)]),
            ..revert_to(make_block_changes(2, None))
        };
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            revert,
            fork_block(3, 1, 0),
            fork_block(4, 1, 1),
        ])));
        let (mut sync, deltas_tx) = resubscribe(rpc_client, &make_block_changes(4, None)).await;

        deltas_tx
            .send(fork_block(5, 1, 1))
            .await
            .unwrap();
        deltas_tx
            .send(setting(fork_block(6, 1, 1), "x", "0x16"))
            .await
            .unwrap();
        let caught_up = sync.next().await;
        let next = sync.next().await;
        sync.stop().await;

        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
        assert_eq!(caught_up.header.number, 5);
        // The consumer learns of the deletion, as a consumer that stayed connected does.
        assert!(caught_up
            .deltas
            .as_ref()
            .is_some_and(|deltas| deltas
                .deleted_protocol_components
                .contains_key("Component9")));
        assert!(caught_up.removed_components.is_empty());
        assert_eq!(attributes_set(&next), attributes(&[("x", "0x16")]));
    }

    #[test_log::test(tokio::test)]
    async fn test_catch_up_stops_tracking_a_component_a_missed_block_drops() {
        // Block 6, the second missed block, drops Component1 below the TVL threshold.
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            make_block_changes(5, None),
            block_dropping_tvl(6, "Component1"),
        ])));
        let (mut sync, deltas_tx) = resubscribe(rpc_client, &make_block_changes(4, None)).await;

        deltas_tx
            .send(setting(make_block_changes(7, None), "x", "0x07"))
            .await
            .unwrap();
        let message = sync.next().await;
        sync.stop().await;

        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
        assert_eq!(message.header.number, 7);
        assert!(message
            .removed_components
            .contains_key("Component1"));
        assert_eq!(attributes_set(&message), HashMap::new(), "An untracked component's change");
    }

    #[test_log::test(tokio::test)]
    async fn test_catch_up_filters_the_missed_blocks_to_the_tracked_components() {
        // Block 6, the second missed block, also changes Component2 and a contract, neither of
        // which the client tracks.
        let untracked_contract = Bytes::from("0xdead");
        let mut block_6 = setting(block_changing_slot(6, &untracked_contract), "x", "0x06");
        block_6.state_deltas.insert(
            "Component2".to_string(),
            ProtocolComponentStateDelta::new(
                "Component2",
                HashMap::from([("x".to_string(), Bytes::from("0x06"))]),
                HashSet::new(),
            ),
        );
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            make_block_changes(5, None),
            block_6,
        ])));
        let (mut sync, deltas_tx) = resubscribe(rpc_client, &make_block_changes(4, None)).await;

        deltas_tx
            .send(make_block_changes(7, None))
            .await
            .unwrap();
        let message = sync.next().await;
        sync.stop().await;

        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
        assert_eq!(attributes_set(&message), attributes(&[("x", "0x06")]));
        assert!(!message
            .deltas
            .as_ref()
            .is_some_and(|deltas| deltas
                .state_deltas
                .contains_key("Component2")));
        assert!(!has_account_delta(&message, &untracked_contract));
    }

    #[test_log::test(tokio::test)]
    async fn test_catch_up_brings_a_background_snapshot_from_before_the_gap_up_to_date() {
        // Missed block 5 changes Component3, whose background snapshot block 3 started before the
        // gap. The snapshot completes while the synchronizer is away.
        let mut block_5 = make_block_changes(5, None);
        block_5.state_deltas.insert(
            "Component3".to_string(),
            ProtocolComponentStateDelta::new(
                "Component3",
                HashMap::from([("x".to_string(), Bytes::from("0x05"))]),
                HashSet::new(),
            ),
        );
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            block_5,
            make_block_changes(6, None),
        ])));
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let mut state_sync = synced_under_an_unchanged_fingerprint(rpc_client, deltas_client);
        let block_3 = make_block_changes(3, None);
        state_sync.spawn_snapshot_task(
            vec!["Component3".to_string()],
            header_of(&block_3),
            &block_3,
        );
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(4, None)))).await;
        timeout(Duration::from_secs(1), async {
            while calls.snapshot_blocks().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("The background snapshot should complete");

        deltas_tx
            .send(make_block_changes(7, None))
            .await
            .unwrap();
        let message = sync.next().await;
        sync.stop().await;

        assert_eq!(calls.snapshot_blocks(), vec![3]);
        let component3 = message
            .snapshots
            .states
            .get("Component3")
            .expect("The background snapshot is dropped");
        assert_eq!(
            component3.state.attributes,
            HashMap::from([("x".to_string(), Bytes::from("0x05"))]),
            "The snapshot misses the missed block's change"
        );
    }

    #[rstest]
    #[case::with_snapshots(true)]
    #[case::without_snapshots(false)]
    #[test_log::test(tokio::test)]
    async fn test_catch_up_asks_for_state_only_when_the_client_takes_snapshots(
        #[case] include_snapshots: bool,
    ) {
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
        ])));
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let mut state_sync = synced_under_an_unchanged_fingerprint(rpc_client, deltas_client);
        state_sync.include_snapshots = include_snapshots;
        let last_synced = make_block_changes(4, None);
        let mut sync = RunningSync::start(state_sync, Some(header_of(&last_synced))).await;

        deltas_tx
            .send(make_block_changes(7, None))
            .await
            .unwrap();
        let message = sync.next().await;
        sync.stop().await;

        assert_eq!(
            calls.catch_up_requests(),
            vec![catch_up_request(&last_synced, &fork_hash(6, 0))
                .with_include_state(include_snapshots)]
        );
        assert_eq!(message.header.number, 7);
    }

    /// A message of `extractor` on `chain` dropping Component1 below the TVL threshold at block 5.
    fn foreign_block_dropping_tvl(extractor: &str, chain: Chain) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            extractor: extractor.to_string(),
            chain,
            ..block_dropping_tvl(5, "Component1")
        }
    }

    // Every path below starts with a block that drops Component1 below the TVL threshold, so a
    // path message that went through `handle_block` would stop tracking it.
    #[rstest]
    #[case::no_window(Ok(DeltaCatchUp::Unavailable(DeltaCatchUpUnavailable::NoWindow)))]
    #[case::fingerprint_changed(Ok(DeltaCatchUp::Unavailable(
        DeltaCatchUpUnavailable::FingerprintChanged
    )))]
    #[case::empty_window(Ok(DeltaCatchUp::Unavailable(DeltaCatchUpUnavailable::EmptyWindow)))]
    #[case::to_not_in_window(Ok(DeltaCatchUp::Unavailable(
        DeltaCatchUpUnavailable::ToNotInWindow
    )))]
    #[case::below_floor(Ok(DeltaCatchUp::Unavailable(DeltaCatchUpUnavailable::BelowFloor)))]
    #[case::unknown_hash(Ok(DeltaCatchUp::Unavailable(DeltaCatchUpUnavailable::UnknownHash)))]
    #[case::to_not_ahead(Ok(DeltaCatchUp::Unavailable(DeltaCatchUpUnavailable::ToNotAhead)))]
    #[case::reason_unknown_to_the_client(Ok(DeltaCatchUp::Unavailable(
        DeltaCatchUpUnavailable::Unknown
    )))]
    #[case::old_server(Ok(DeltaCatchUp::Unsupported))]
    #[case::plan_without_access(Err(RPCError::ParseResponse(
        "plan has no access to uniswap-v2".to_string()
    )))]
    #[case::server_unreachable(Err(RPCError::ServerUnreachable("restarting".to_string())))]
    #[case::message_of_another_extractor(Ok(DeltaCatchUp::Available(vec![
        foreign_block_dropping_tvl("sushiswap-v2", Chain::Ethereum),
        make_block_changes(6, None),
    ])))]
    #[case::message_of_another_chain(Ok(DeltaCatchUp::Available(vec![
        foreign_block_dropping_tvl("uniswap-v2", Chain::Base),
        make_block_changes(6, None),
    ])))]
    #[case::detached_block(Ok(DeltaCatchUp::Available(vec![
        block_dropping_tvl(5, "Component1"),
        fork_block(6, 0, 1),
    ])))]
    #[case::revert_to_a_block_the_path_did_not_pass(Ok(DeltaCatchUp::Available(vec![
        block_dropping_tvl(5, "Component1"),
        make_block_changes(6, None),
        revert_to(fork_block(5, 1, 0)),
        fork_block(6, 0, 1),
    ])))]
    #[case::path_short_of_the_first_message(Ok(DeltaCatchUp::Available(vec![
        block_dropping_tvl(5, "Component1"),
    ])))]
    #[case::path_past_the_first_message(Ok(DeltaCatchUp::Available(vec![
        block_dropping_tvl(5, "Component1"),
        make_block_changes(6, None),
        make_block_changes(7, None),
    ])))]
    #[case::empty_path(Ok(DeltaCatchUp::Available(vec![])))]
    #[test_log::test(tokio::test)]
    async fn test_catch_up_falls_back_to_a_snapshot(
        #[case] answer: Result<DeltaCatchUp, RPCError>,
    ) {
        let last_synced = make_block_changes(4, None);
        let (rpc_client, calls) = mock_rpc_answering_catch_up(answer);
        let (mut sync, deltas_tx) = resubscribe(rpc_client, &last_synced).await;

        deltas_tx
            .send(make_block_changes(7, None))
            .await
            .unwrap();
        let message = sync.next().await;
        sync.stop().await;

        assert_eq!(
            calls.catch_up_requests(),
            vec![catch_up_request(&last_synced, &fork_hash(6, 0))]
        );
        assert_eq!(calls.snapshot_blocks(), vec![7]);
        assert_eq!(message.header.number, 7);
        assert!(
            message
                .snapshots
                .states
                .contains_key("Component1"),
            "A path message went through handle_block"
        );
        assert!(message.removed_components.is_empty());
    }

    /// An RPC client that forwards every request to `inner`, and holds back `inner`'s answer to a
    /// catch-up request for `delay`.
    #[derive(Clone)]
    struct SlowCatchUpRPCClient {
        inner: ArcRPCClient<MockRPCClient>,
        delay: Duration,
    }

    #[async_trait]
    impl RPCClient for SlowCatchUpRPCClient {
        async fn get_tokens(
            &self,
            params: crate::rpc::TokensParams,
        ) -> Result<crate::rpc::Page<Vec<Token>>, RPCError> {
            self.inner.get_tokens(params).await
        }

        async fn get_contract_state(
            &self,
            params: crate::rpc::ContractStateParams,
        ) -> Result<crate::rpc::Page<Vec<Account>>, RPCError> {
            self.inner
                .get_contract_state(params)
                .await
        }

        async fn get_protocol_components(
            &self,
            params: crate::rpc::ProtocolComponentsParams,
        ) -> Result<crate::rpc::Page<Vec<ProtocolComponent>>, RPCError> {
            self.inner
                .get_protocol_components(params)
                .await
        }

        async fn get_protocol_states(
            &self,
            params: crate::rpc::ProtocolStatesParams,
        ) -> Result<crate::rpc::Page<Vec<ProtocolComponentState>>, RPCError> {
            self.inner
                .get_protocol_states(params)
                .await
        }

        async fn get_protocol_systems(
            &self,
            params: crate::rpc::ProtocolSystemsParams,
        ) -> Result<crate::rpc::Page<crate::rpc::ProtocolSystems>, RPCError> {
            self.inner
                .get_protocol_systems(params)
                .await
        }

        async fn get_component_tvl(
            &self,
            params: crate::rpc::ComponentTvlParams,
        ) -> Result<crate::rpc::Page<HashMap<String, f64>>, RPCError> {
            self.inner
                .get_component_tvl(params)
                .await
        }

        async fn get_traced_entry_points(
            &self,
            params: crate::rpc::TracedEntryPointsParams,
        ) -> Result<
            crate::rpc::Page<HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>>>,
            RPCError,
        > {
            self.inner
                .get_traced_entry_points(params)
                .await
        }

        async fn get_snapshots<'a>(
            &self,
            request: &SnapshotParameters<'a>,
            chunk_size: Option<usize>,
            concurrency: usize,
        ) -> Result<Snapshot, RPCError> {
            self.inner
                .get_snapshots(request, chunk_size, concurrency)
                .await
        }

        async fn get_delta_catch_up(
            &self,
            params: DeltaCatchUpParams,
        ) -> Result<DeltaCatchUp, RPCError> {
            let answer = self
                .inner
                .get_delta_catch_up(params)
                .await;
            sleep(self.delay).await;
            answer
        }

        fn compression(&self) -> bool {
            self.inner.compression()
        }
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_catch_up_falls_back_to_a_snapshot_when_the_server_answers_too_late() {
        // A valid path, but only after the 120 s the RPC client waits for a restarting server.
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
        ])));
        let rpc_client = SlowCatchUpRPCClient {
            inner: ArcRPCClient(Arc::new(rpc_client)),
            delay: Duration::from_secs(120),
        };
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let deltas_client = FingerprintedDeltasClient {
            inner: ArcDeltasClient(Arc::new(deltas_client)),
            fingerprint: Some(fingerprint(1)),
        };
        let synchronizer_timeout = 10;
        let mut state_sync = ProtocolStateSynchronizer::new(
            ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            true,
            ComponentFilter::with_tvl_range(50.0, 50.0),
            1,
            Duration::from_secs(0),
            true,
            false,
            true,
            rpc_client,
            deltas_client,
            synchronizer_timeout,
        );
        state_sync.synced_fingerprint = Some(fingerprint(1));
        let mut sync =
            RunningSync::start(state_sync, Some(header_of(&make_block_changes(4, None)))).await;

        let sent_at = Instant::now();
        deltas_tx
            .send(make_block_changes(7, None))
            .await
            .unwrap();
        let message = timeout(Duration::from_secs(60), sync.block_rx.recv())
            .await
            .expect("Should fall back before the server answers")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let waited = sent_at.elapsed();
        sync.stop().await;

        assert_eq!(calls.catch_up_requests().len(), 1);
        assert_eq!(calls.snapshot_blocks(), vec![7]);
        assert!(message
            .snapshots
            .states
            .contains_key("Component1"));
        assert!(
            waited >= Duration::from_secs(synchronizer_timeout) && waited < Duration::from_secs(20),
            "Fell back after {waited:?}"
        );
    }

    #[rstest]
    #[case::partial_blocks(true, Some(fingerprint(1)), Some(fingerprint(1)), 6)]
    #[case::no_fingerprint(false, None, None, 7)]
    #[case::fingerprint_changed(false, Some(fingerprint(1)), Some(fingerprint(2)), 7)]
    #[test_log::test(tokio::test)]
    async fn test_catch_up_is_not_attempted(
        #[case] partial_blocks: bool,
        #[case] synced: Option<ExtractorFingerprint>,
        #[case] reported: Option<ExtractorFingerprint>,
        #[case] expected_snapshot_block: u64,
    ) {
        let (mut rpc_client, snapshot_blocks) = mock_rpc_recording_snapshot_blocks();
        rpc_client
            .expect_get_delta_catch_up()
            .never();
        let (deltas_client, deltas_tx) = mock_deltas_channel();
        let mut state_sync = with_fingerprinted_clients(rpc_client, deltas_client, reported)
            .with_partial_blocks(partial_blocks);
        state_sync.synced_fingerprint = synced;
        // A partial-block client last applied a partial block.
        let last_synced = make_block_changes(4, partial_blocks.then_some(2));
        let mut sync = RunningSync::start(state_sync, Some(header_of(&last_synced))).await;

        deltas_tx
            .send(make_block_changes(7, None))
            .await
            .unwrap();
        let message = sync.next().await;
        sync.stop().await;

        assert_eq!(*snapshot_blocks.lock().unwrap(), vec![expected_snapshot_block]);
        assert_eq!(message.header.number, 7);
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_catch_up_runs_before_the_snapshot_delay() {
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Available(vec![
            make_block_changes(8, None),
            make_block_changes(9, None),
        ])));
        let deltas_client = mock_deltas_emitting(vec![10, 11]);
        let mut state_sync = synced_under_an_unchanged_fingerprint(rpc_client, deltas_client);
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        state_sync.last_synced_block = Some(header_of(&make_block_changes(7, None)));

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx, Duration::from_millis(300))
                .await
        });

        let msg = timeout(Duration::from_millis(10), block_rx.recv())
            .await
            .expect("Should catch up at the first block at once")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = end_tx.send(());
        let _ = handle
            .await
            .expect("Task should not panic");

        assert_eq!(msg.header.number, 10);
        assert!(msg.snapshots.states.is_empty());
        assert_eq!(calls.snapshot_blocks(), Vec::<u64>::new());
    }

    #[test_log::test(tokio::test(start_paused = true))]
    async fn test_failed_catch_up_waits_for_the_snapshot_delay() {
        let (rpc_client, calls) = mock_rpc_answering_catch_up(Ok(DeltaCatchUp::Unavailable(
            DeltaCatchUpUnavailable::BelowFloor,
        )));
        let deltas_client = mock_deltas_emitting((10..=20).collect());
        let mut state_sync = synced_under_an_unchanged_fingerprint(rpc_client, deltas_client);
        state_sync
            .initialize()
            .await
            .expect("Init should succeed");
        let last_synced = make_block_changes(5, None);
        state_sync.last_synced_block = Some(header_of(&last_synced));

        let (mut block_tx, mut block_rx) = channel(10);
        let (end_tx, end_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            state_sync
                .state_sync(&mut block_tx, end_rx, Duration::from_millis(300))
                .await
        });

        let first = timeout(Duration::from_secs(1), block_rx.recv())
            .await
            .expect("Should receive first message")
            .expect("Channel should be open")
            .expect("Should not be an error");
        let _ = end_tx.send(());
        let _ = handle
            .await
            .expect("Task should not panic");

        // Catch-up is asked for at block 10, the first block; blocks 10-15 then arrive within the
        // 300 ms delay, 50 ms apart.
        assert_eq!(
            calls.catch_up_requests(),
            vec![catch_up_request(&last_synced, &fork_hash(9, 0))]
        );
        let snapshot_blocks = calls.snapshot_blocks();
        assert_eq!(snapshot_blocks.len(), 1, "Should request one snapshot");
        assert!(
            snapshot_blocks[0] >= 16,
            "Snapshot at block {} is inside the delay",
            snapshot_blocks[0]
        );
        assert_eq!(first.header.number, snapshot_blocks[0]);
    }

    #[rstest]
    #[case::blocks(vec![make_block_changes(5, None), make_block_changes(6, None)], fork_hash(6, 0), true)]
    #[case::reverts_then_blocks(
        vec![revert_to(make_block_changes(2, None)), fork_block(3, 1, 0), fork_block(4, 1, 1)],
        fork_hash(4, 1),
        true
    )]
    #[case::ending_with_a_revert_to_the_target(
        vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
            revert_to(make_block_changes(5, None)),
        ],
        fork_hash(5, 0),
        true
    )]
    #[case::nested_reverts(
        vec![
            revert_to(make_block_changes(3, None)),
            revert_to(make_block_changes(2, None)),
            fork_block(3, 1, 0),
            fork_block(4, 1, 1),
        ],
        fork_hash(4, 1),
        true
    )]
    #[case::revert_to_the_last_synced_block(
        vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
            revert_to(make_block_changes(4, None)),
            fork_block(5, 1, 0),
        ],
        fork_hash(5, 1),
        true
    )]
    #[case::empty_from_the_target(vec![], fork_hash(4, 0), true)]
    #[case::empty_from_another_block(vec![], fork_hash(6, 0), false)]
    #[case::short_of_the_target(vec![make_block_changes(5, None)], fork_hash(6, 0), false)]
    #[case::past_the_target(
        vec![make_block_changes(5, None), make_block_changes(6, None), make_block_changes(7, None)],
        fork_hash(6, 0),
        false
    )]
    #[case::detached_block(
        vec![make_block_changes(5, None), fork_block(6, 0, 1)],
        fork_hash(6, 0),
        false
    )]
    #[case::block_at_the_wrong_height(
        vec![BlockAggregatedChanges {
            block: Block { number: 6, ..make_block_changes(5, None).block },
            ..make_block_changes(5, None)
        }],
        fork_hash(5, 0),
        false
    )]
    #[case::revert_ahead_of_the_walk(
        vec![revert_to(make_block_changes(7, None))],
        fork_hash(7, 0),
        false
    )]
    #[case::revert_to_a_sibling_of_the_last_synced_block(
        vec![revert_to(fork_block(4, 1, 0))],
        fork_hash(4, 1),
        false
    )]
    #[case::revert_to_a_sibling_of_a_passed_block(
        vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
            revert_to(fork_block(5, 1, 0)),
            fork_block(6, 1, 1),
        ],
        fork_hash(6, 1),
        false
    )]
    #[case::revert_to_a_sibling_of_the_last_synced_block_after_blocks(
        vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
            revert_to(fork_block(4, 1, 0)),
            fork_block(5, 1, 1),
        ],
        fork_hash(5, 1),
        false
    )]
    #[case::revert_ending_elsewhere(
        vec![
            make_block_changes(5, None),
            make_block_changes(6, None),
            revert_to(make_block_changes(5, None)),
        ],
        fork_hash(6, 0),
        false
    )]
    #[case::message_of_another_extractor(
        vec![foreign_block_dropping_tvl("sushiswap-v2", Chain::Ethereum), make_block_changes(6, None)],
        fork_hash(6, 0),
        false
    )]
    #[case::message_of_another_chain(
        vec![foreign_block_dropping_tvl("uniswap-v2", Chain::Base), make_block_changes(6, None)],
        fork_hash(6, 0),
        false
    )]
    fn test_check_catch_up_path_from_block_4(
        #[case] path: Vec<BlockAggregatedChanges>,
        #[case] to: Bytes,
        #[case] valid: bool,
    ) {
        let checked = check_catch_up_path(
            &path,
            &ExtractorIdentity::new(Chain::Ethereum, "uniswap-v2"),
            &header_of(&make_block_changes(4, None)),
            &to,
        );

        assert_eq!(checked.is_ok(), valid, "{checked:?}");
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
        // This test checks block selection, not the restart delay.
        state_sync.snapshot_jitter = Duration::ZERO;

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
