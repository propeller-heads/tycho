//! Forces a `tycho-client` stream to resubscribe after missing blocks, and checks the state it
//! holds afterwards against a snapshot of the same components at the same block.
//!
//! The check runs a stream of its own for the protocol stream's protocols: one
//! `ProtocolStateSynchronizer` per protocol under a `BlockSynchronizer`, wired like
//! `TychoStreamBuilder::build` but over a [`GapInjector`]. Every `every_n_blocks` blocks, each
//! subscription drops its next [`GAP_BLOCKS`] messages and ends. The synchronizers resubscribe
//! having missed those blocks, which they catch up on through the server's delta catch-up
//! endpoint, or recover from with a snapshot when the server cannot serve them, when a reorg
//! replaced a block they had delivered, or when the first message after the gap is a revert.
//!
//! Every resubscription is classified, forced or not. After each recovery of a protocol that
//! tracks components, the state held for it is compared with a snapshot of its components at that
//! block. When a forced gap's comparison finds differences, the state held when the gap was forced
//! is compared with a snapshot at its own block too, and the differences both comparisons find are
//! reported as pre-existing rather than as caused by the gap.

pub mod gap_injector;
pub mod held_state;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use miette::{miette, IntoDiagnostic, WrapErr};
use tokio::{
    sync::mpsc::{self, Receiver, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, error, info, warn};
use tycho_client::{
    deltas::DeltasClient,
    feed::{
        component_tracker::ComponentFilter,
        synchronizer::{ProtocolStateSynchronizer, StateSyncMessage},
        BlockHeader, BlockSynchronizer, BlockSynchronizerError, FeedMessage,
    },
    rpc::{
        HttpRPCClientOptions, RPCClient, SnapshotParameters, TracedEntryPointsPaginatedParams,
        RPC_CLIENT_CONCURRENCY,
    },
    stream::ProtocolSystemsInfo,
    DeltasError, HttpRPCClient, RPCError, WsDeltasClient,
};
use tycho_common::{
    models::{blockchain::TracingResult, Chain, ExtractorIdentity},
    Bytes,
};

use crate::{
    catch_up_check::{
        gap_injector::{GapInjector, GapTrigger, Resubscription},
        held_state::{diff, Difference, ExtractorState, Field, Status},
    },
    metrics,
};

/// Messages each subscription drops before it ends, so a resubscription misses at least as many
/// blocks. Enough that a gap usually changes some compared state, and well inside the server's
/// window of at least 128 blocks.
pub const GAP_BLOCKS: usize = 10;

/// How long the protocols get to resubscribe after a gap. Covers the client's snapshot delay of up
/// to a minute when it cannot catch up.
const RESYNC_DEADLINE: Duration = Duration::from_secs(300);

/// How long a comparison may take. The RPC client has no request timeout, and a comparison keeps
/// a copy of the protocol's state and its baseline alive.
const COMPARISON_TIMEOUT: Duration = RESYNC_DEADLINE;

/// Differences logged one by one per comparison; the rest are only counted.
const MAX_LOGGED_DIFFERENCES: usize = 10;

/// Retry settings of the protocol stream, so the check's stream recovers the same way.
const WS_RETRY_COOLDOWN: Duration = Duration::from_secs(1);
const SYNC_RETRY_COOLDOWN: Duration = Duration::from_secs(3);

/// The stream's output.
type Feed = Receiver<Result<FeedMessage<BlockHeader>, BlockSynchronizerError>>;

/// What the check streams and how often it forces a gap.
pub struct CatchUpCheckConfig {
    pub chain: Chain,
    pub tycho_url: String,
    pub api_key: String,
    pub no_tls: bool,
    pub protocols: Vec<String>,
    pub component_filter: ComponentFilter,
    pub every_n_blocks: u64,
}

/// Starts the check in a task, which sets up the check's stream and then runs the check. The task
/// only ends when the check stops, with the reason it stopped: the stream could not be set up (a
/// client could not be created or connect, or no synchronizer delivered its first message) or it
/// ended.
pub fn spawn(config: CatchUpCheckConfig) -> JoinHandle<miette::Report> {
    tokio::spawn(async move {
        match start(config).await {
            Ok((check, feed, ws_task)) => check.run(feed, ws_task).await,
            Err(e) => e,
        }
    })
}

/// Sets up the check's stream and the task that runs its comparisons. Returns once every
/// synchronizer delivered its first message or gave up.
async fn start(
    config: CatchUpCheckConfig,
) -> miette::Result<(CatchUpCheck, Feed, JoinHandle<Result<(), DeltasError>>)> {
    let CatchUpCheckConfig {
        chain,
        tycho_url,
        api_key,
        no_tls,
        protocols,
        component_filter,
        every_n_blocks,
    } = config;
    let (ws_scheme, rpc_scheme) = if no_tls { ("ws", "http") } else { ("wss", "https") };
    let ws_client = WsDeltasClient::new_with_reconnects(
        &format!("{ws_scheme}://{tycho_url}"),
        Some(&api_key),
        u64::MAX,
        WS_RETRY_COOLDOWN,
    )
    .into_diagnostic()
    .wrap_err("Failed to create the catch-up check's WebSocket client")?;
    let rpc_client = || {
        HttpRPCClient::new(
            &format!("{rpc_scheme}://{tycho_url}"),
            HttpRPCClientOptions::new()
                .with_auth_key(Some(api_key.clone()))
                .with_compression(true),
        )
        .into_diagnostic()
        .wrap_err("Failed to create an RPC client for the catch-up check")
    };
    let sync_rpc = rpc_client()?;
    // Comparisons get a client of their own: a client waits out a rate limit before its next
    // request, which could push the synchronizers' catch-up requests past their timeout.
    let comparison_rpc = rpc_client()?;

    let (resubscriptions_sender, resubscriptions) = mpsc::unbounded_channel();
    let (deltas, trigger) = GapInjector::new(ws_client, GAP_BLOCKS, resubscriptions_sender);
    let requested: HashSet<String> = protocols.iter().cloned().collect();
    let dci_protocols = ProtocolSystemsInfo::fetch(&sync_rpc, chain, &requested)
        .await
        .dci_protocols;
    let StreamTiming { block_time, latency, max_missed_blocks } = StreamTiming::of(chain);
    let mut block_sync = BlockSynchronizer::new(
        Duration::from_secs(block_time),
        Duration::from_secs(latency),
        max_missed_blocks,
    );
    // The defaults `TychoStreamBuilder` passes.
    let (retrieve_balances, include_snapshots, include_tvl, compression) =
        (true, true, false, true);
    for protocol in &protocols {
        let extractor = ExtractorIdentity::new(chain, protocol);
        let synchronizer = ProtocolStateSynchronizer::new(
            extractor.clone(),
            retrieve_balances,
            component_filter.clone(),
            u64::MAX,
            SYNC_RETRY_COOLDOWN,
            include_snapshots,
            include_tvl,
            compression,
            sync_rpc.clone(),
            deltas.clone(),
            block_time + latency,
        )
        .with_dci(dci_protocols.contains(protocol));
        block_sync = block_sync.register_synchronizer(extractor, synchronizer);
    }

    let ws_task = deltas
        .connect()
        .await
        .into_diagnostic()
        .wrap_err("Failed to connect the catch-up check's WebSocket client")?;
    let feed = match block_sync.run().await {
        Ok((_, feed)) => feed,
        Err(e) => {
            if let Err(close_error) = deltas.close().await {
                warn!(error = %close_error, "Failed to close the check's WebSocket client");
            }
            return Err(miette!(e).wrap_err("Failed to start the catch-up check's stream"));
        }
    };

    let (comparisons, requests) = mpsc::unbounded_channel();
    let snapshots =
        RpcSnapshots { chain, rpc: comparison_rpc, dci_protocols: Arc::new(dci_protocols) };
    tokio::spawn(run_comparisons(snapshots, requests));
    let check = CatchUpCheck::new(every_n_blocks, trigger, resubscriptions, comparisons);
    Ok((check, feed, ws_task))
}

/// The timing `TychoStreamBuilder` uses for a chain, in seconds and blocks, so the check's stream
/// waits for and gives up on synchronizers like a production one. A copy of the private
/// `TychoStreamBuilder::default_timing` in `tycho-client/src/stream.rs`.
struct StreamTiming {
    block_time: u64,
    latency: u64,
    max_missed_blocks: u64,
}

impl StreamTiming {
    fn of(chain: Chain) -> Self {
        let (block_time, latency, max_missed_blocks) = match chain {
            Chain::Ethereum => (12, 36, 50),
            Chain::Starknet => (2, 8, 50),
            Chain::ZkSync => (3, 12, 50),
            Chain::Arbitrum => (1, 9, 100),
            Chain::Base => (2, 12, 50),
            Chain::Bsc => (1, 12, 50),
            Chain::Unichain => (1, 10, 100),
            Chain::Polygon => (2, 12, 50),
            Chain::Plasma => (1, 10, 100),
            Chain::Robinhood => (1, 5, 100),
            Chain::Arc => (1, 5, 100),
            // `Chain` is non-exhaustive; custom chains take the builder's generic timing.
            _ => {
                let block_time = chain.block_time_secs();
                (block_time, block_time * 3, 50)
            }
        };
        Self { block_time, latency, max_missed_blocks }
    }
}

/// How a protocol's synchronizer recovered after resubscribing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resync {
    /// The new subscription's first block followed the last one delivered: nothing was missed.
    Resumed,
    /// The client applied the missed blocks without a snapshot.
    CaughtUp,
    /// The client fell back to a snapshot.
    Snapshot,
    /// The protocol tracks no component, so a catch-up and a snapshot look the same.
    NothingTracked,
}

impl Resync {
    /// Classifies the first message a protocol's synchronizer emitted after resubscribing, given
    /// the state held before it. A snapshot re-sends the held components; a catch-up only sends
    /// snapshots of components that became tracked during the gap.
    pub fn classify(
        held: &ExtractorState,
        message: &StateSyncMessage<BlockHeader>,
        follows_previous: bool,
    ) -> Self {
        if held.has_no_components() {
            return Resync::NothingTracked;
        }
        let resends_held_components = message
            .snapshots
            .states
            .keys()
            .any(|component_id| held.holds_component(component_id));
        if resends_held_components {
            Resync::Snapshot
        } else if follows_previous {
            Resync::Resumed
        } else {
            Resync::CaughtUp
        }
    }

    /// The outcome's name in metrics and logs.
    pub fn label(self) -> &'static str {
        match self {
            Resync::Resumed => "resumed",
            Resync::CaughtUp => "caught_up",
            Resync::Snapshot => "snapshot",
            Resync::NothingTracked => "nothing_tracked",
        }
    }
}

/// The state held for a protocol and the block of the last message applied to it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Held {
    state: ExtractorState,
    header: BlockHeader,
}

/// A request to compare the state held for a protocol after a recovery with a snapshot.
struct Comparison {
    protocol: String,
    resync: Resync,
    held: Held,
    /// What the protocol held when the gap it recovered from was forced; `None` after a
    /// resubscription nobody forced.
    baseline: Option<Held>,
}

/// When the check forces the next gap.
#[derive(Debug, PartialEq)]
enum Phase {
    /// Waiting for the stream's first message.
    Starting,
    /// Following the stream until block `gap_at`.
    Following { gap_at: u64 },
    /// A gap was forced; waiting until `deadline` for the protocols in `pending` to resubscribe.
    Resyncing { pending: HashSet<String>, deadline: Instant },
}

struct CatchUpCheck {
    every_n_blocks: u64,
    trigger: GapTrigger,
    resubscriptions: UnboundedReceiver<Resubscription>,
    comparisons: UnboundedSender<Comparison>,
    protocols: HashMap<String, Held>,
    /// Resubscriptions whose first message has not reached the stream's output yet, by protocol.
    awaiting: HashMap<String, Resubscription>,
    /// What each protocol held when the last gap was forced, until it recovers.
    baselines: HashMap<String, Held>,
    phase: Phase,
}

impl CatchUpCheck {
    fn new(
        every_n_blocks: u64,
        trigger: GapTrigger,
        resubscriptions: UnboundedReceiver<Resubscription>,
        comparisons: UnboundedSender<Comparison>,
    ) -> Self {
        Self {
            every_n_blocks,
            trigger,
            resubscriptions,
            comparisons,
            protocols: HashMap::new(),
            awaiting: HashMap::new(),
            baselines: HashMap::new(),
            phase: Phase::Starting,
        }
    }

    /// Runs the check until the stream ends, and returns why it ended.
    async fn run(
        mut self,
        mut feed: Feed,
        mut ws_task: JoinHandle<Result<(), DeltasError>>,
    ) -> miette::Report {
        info!(
            every_n_blocks = self.every_n_blocks,
            gap_blocks = GAP_BLOCKS,
            "Catch-up check started"
        );
        loop {
            let message = tokio::select! {
                message = feed.recv() => message,
                result = &mut ws_task => {
                    return miette!("The catch-up check's WebSocket client stopped: {result:?}");
                }
            };
            match message {
                Some(Ok(message)) => self.handle(message),
                Some(Err(e)) => return miette!(e).wrap_err("The catch-up check's stream failed"),
                None => return miette!("The catch-up check's stream ended"),
            }
        }
    }

    fn handle(&mut self, message: FeedMessage<BlockHeader>) {
        // A forwarder reports a resubscription before it passes the message on, so every report
        // about the messages below has arrived.
        while let Ok(resubscription) = self.resubscriptions.try_recv() {
            self.awaiting
                .insert(resubscription.extractor.clone(), resubscription);
        }
        let FeedMessage { state_msgs, sync_states: _ } = message;
        let mut newest_block = None;
        for (protocol, message) in &state_msgs {
            self.apply(protocol, message);
            newest_block = newest_block.max(Some(message.header.number));
        }
        if let Some(block) = newest_block {
            self.advance(block);
        }
    }

    /// Applies a protocol's message to the state held for it. When the message is the protocol's
    /// first after a resubscription, records how it recovered and, unless it tracks nothing,
    /// requests a comparison with a snapshot at the message's block.
    fn apply(&mut self, protocol: &str, message: &StateSyncMessage<BlockHeader>) {
        let held = self
            .protocols
            .entry(protocol.to_string())
            .or_default();
        let recovers_here = self
            .awaiting
            .get(protocol)
            .is_some_and(|resubscription| message.header.number >= resubscription.first_block);
        let resubscription = if recovers_here { self.awaiting.remove(protocol) } else { None };
        let resync = resubscription.map(|resubscription| {
            Resync::classify(&held.state, message, resubscription.follows_previous)
        });
        held.state.apply(message);
        held.header = message.header.clone();
        let Some(resync) = resync else {
            return;
        };
        let held = held.clone();
        info!(
            protocol,
            block = held.header.number,
            resync = resync.label(),
            "Protocol resubscribed"
        );
        metrics::record_catch_up_resync(protocol, resync.label());
        if let Phase::Resyncing { pending, deadline: _ } = &mut self.phase {
            pending.remove(protocol);
        }
        let baseline = self.baselines.remove(protocol);
        match resync {
            Resync::CaughtUp | Resync::Snapshot | Resync::Resumed => {
                let comparison =
                    Comparison { protocol: protocol.to_string(), resync, held, baseline };
                if self
                    .comparisons
                    .send(comparison)
                    .is_err()
                {
                    error!(protocol, "The catch-up check's comparisons stopped; skipping one");
                    metrics::record_catch_up_comparison(protocol, resync.label(), "error");
                }
            }
            Resync::NothingTracked => {}
        }
    }

    fn advance(&mut self, block: u64) {
        let next = match &mut self.phase {
            Phase::Starting => Phase::Following { gap_at: block + self.every_n_blocks },
            Phase::Following { gap_at } if block >= *gap_at => {
                info!(
                    block,
                    gap_blocks = GAP_BLOCKS,
                    "Forcing the catch-up check's protocols to resubscribe after a gap"
                );
                self.trigger.request_gap();
                self.baselines = self.protocols.clone();
                Phase::Resyncing {
                    pending: self.protocols.keys().cloned().collect(),
                    deadline: Instant::now() + RESYNC_DEADLINE,
                }
            }
            Phase::Following { gap_at: _ } => return,
            Phase::Resyncing { pending, deadline } => {
                if !pending.is_empty() && Instant::now() < *deadline {
                    return;
                }
                for protocol in pending.iter() {
                    warn!(
                        protocol,
                        deadline = ?RESYNC_DEADLINE,
                        "Protocol did not resubscribe after a forced gap in time"
                    );
                    metrics::record_catch_up_resync(protocol, "no_resubscription");
                }
                self.baselines.clear();
                Phase::Following { gap_at: block + self.every_n_blocks }
            }
        };
        self.phase = next;
    }
}

/// Where comparisons get snapshots from.
#[async_trait]
trait SnapshotSource {
    /// The differences between `held` and a snapshot of its components at its block.
    async fn differences(&self, protocol: &str, held: &Held) -> Result<Vec<Difference>, RPCError>;
}

/// Snapshots from the server's RPC.
#[derive(Clone)]
struct RpcSnapshots {
    chain: Chain,
    rpc: HttpRPCClient,
    dci_protocols: Arc<HashSet<String>>,
}

#[async_trait]
impl SnapshotSource for RpcSnapshots {
    async fn differences(&self, protocol: &str, held: &Held) -> Result<Vec<Difference>, RPCError> {
        let snapshot = self
            .fetch_snapshot(protocol, &held.state, held.header.number)
            .await?;
        Ok(diff(&held.state, &snapshot))
    }
}

impl RpcSnapshots {
    /// Fetches the state at `block` of the components `held` holds, with the contracts they name
    /// and, for a DCI protocol, the contracts their traced entry points reach.
    async fn fetch_snapshot(
        &self,
        protocol: &str,
        held: &ExtractorState,
        block: u64,
    ) -> Result<ExtractorState, RPCError> {
        let components = held.components();
        let mut contracts = held.component_contracts();
        if self.dci_protocols.contains(protocol) {
            let traced = self
                .rpc
                .get_traced_entry_points_paginated(TracedEntryPointsPaginatedParams::new(
                    self.chain,
                    protocol,
                    components.keys().cloned().collect(),
                    RPC_CLIENT_CONCURRENCY,
                ))
                .await?;
            for traces in traced.values() {
                for (_, TracingResult { retriggers: _, accessed_slots }) in traces {
                    contracts.extend(accessed_slots.keys().cloned());
                }
            }
        }
        let contracts: Vec<Bytes> = contracts.into_iter().collect();
        let request = SnapshotParameters::new(self.chain, protocol, &components, &contracts, block)
            .include_tvl(false);
        let snapshot = self
            .rpc
            .get_snapshots(&request, None, RPC_CLIENT_CONCURRENCY)
            .await?;
        Ok(ExtractorState::from_snapshot(&snapshot))
    }
}

/// Runs each comparison it receives in a task of its own, for at most [`COMPARISON_TIMEOUT`], and
/// records what it found.
async fn run_comparisons<S>(snapshots: S, mut requests: UnboundedReceiver<Comparison>)
where
    S: SnapshotSource + Clone + Send + Sync + 'static,
{
    while let Some(comparison) = requests.recv().await {
        let snapshots = snapshots.clone();
        tokio::spawn(async move {
            let Comparison { protocol, resync, held, baseline: _ } = &comparison;
            let block = held.header.number;
            let resync_label = resync.label();
            let findings = match timeout(COMPARISON_TIMEOUT, compare(&snapshots, &comparison)).await
            {
                Ok(Ok(findings)) => findings,
                Ok(Err(e)) => {
                    warn!(
                        protocol,
                        block,
                        resync = resync_label,
                        error = %e,
                        "Failed to fetch a snapshot to compare the catch-up check's state with"
                    );
                    metrics::record_catch_up_comparison(protocol, resync_label, "error");
                    return;
                }
                Err(_) => {
                    warn!(
                        protocol,
                        block,
                        resync = resync_label,
                        timeout = ?COMPARISON_TIMEOUT,
                        "A catch-up check comparison timed out"
                    );
                    metrics::record_catch_up_comparison(protocol, resync_label, "error");
                    return;
                }
            };
            if findings.changed_nothing {
                info!(
                    protocol,
                    block,
                    "The state held changed nothing between the baseline and the recovery, so the \
                     comparison can't catch a catch-up bug"
                );
                metrics::record_catch_up_unchanged_recovery(protocol);
            }
            report(protocol, &held.header, *resync, &findings.assessed);
        });
    }
}

/// What a comparison found.
#[derive(Debug, PartialEq)]
struct Findings {
    /// Every difference from a snapshot at the recovery's block, with its status.
    assessed: Vec<(Difference, Status)>,
    /// Whether the state held is the same at the baseline and at the recovery, so the comparison
    /// can't catch a catch-up bug. False without a baseline.
    changed_nothing: bool,
}

/// Compares the state held after a recovery with a snapshot at its block. When they differ and the
/// recovery has a baseline, compares the baseline with a snapshot at the baseline's block too, and
/// counts the differences it shares exactly as pre-existing. A failed baseline fetch is logged and
/// leaves every difference new.
///
/// # Errors
///
/// When the snapshot at the recovery's block cannot be fetched.
async fn compare(
    snapshots: &impl SnapshotSource,
    comparison: &Comparison,
) -> Result<Findings, RPCError> {
    let Comparison { protocol, resync: _, held, baseline } = comparison;
    let changed_nothing = baseline
        .as_ref()
        .is_some_and(|baseline| baseline.state == held.state);
    let differences = snapshots
        .differences(protocol, held)
        .await?;
    let pre_existing = match baseline {
        Some(baseline) if !differences.is_empty() => {
            match snapshots
                .differences(protocol, baseline)
                .await
            {
                Ok(differences) => differences,
                Err(e) => {
                    warn!(
                        protocol,
                        block = baseline.header.number,
                        error = %e,
                        "Failed to fetch the snapshot at the baseline; no difference counts as \
                         pre-existing"
                    );
                    Vec::new()
                }
            }
        }
        Some(_) | None => Vec::new(),
    };
    Ok(Findings { assessed: assess(differences, &pre_existing), changed_nothing })
}

/// Pairs each difference with its status: [`Status::PreExisting`] when `pre_existing` has an equal
/// difference (same entry, same values on both sides), otherwise its field's status. A stale value
/// the gap changed therefore counts as new.
fn assess(differences: Vec<Difference>, pre_existing: &[Difference]) -> Vec<(Difference, Status)> {
    let pre_existing: BTreeSet<&Difference> = pre_existing.iter().collect();
    let mut assessed = Vec::with_capacity(differences.len());
    for difference in differences {
        let status = if pre_existing.contains(&difference) {
            Status::PreExisting
        } else {
            difference.field.status()
        };
        assessed.push((difference, status));
    }
    assessed
}

/// How a comparison ended, judged by the differences that are not pre-existing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComparisonResult {
    Equal,
    /// Differs only in fields the server does not restore on reverts yet.
    KnownGaps,
    Mismatch,
}

impl ComparisonResult {
    fn of(assessed: &[(Difference, Status)]) -> Self {
        let mut result = ComparisonResult::Equal;
        for (_, status) in assessed {
            match status {
                Status::Mismatch => return ComparisonResult::Mismatch,
                Status::KnownGap => result = ComparisonResult::KnownGaps,
                Status::PreExisting => {}
            }
        }
        result
    }

    fn label(self) -> &'static str {
        match self {
            ComparisonResult::Equal => "equal",
            ComparisonResult::KnownGaps => "known_gaps",
            ComparisonResult::Mismatch => "mismatch",
        }
    }
}

/// Records a comparison's assessed differences in metrics and logs. Mismatches are logged with the
/// hash of the block the state was held at: the snapshot is fetched by block number, so a reorg
/// between the recovery and the request shows as a different hash at that number.
fn report(protocol: &str, header: &BlockHeader, resync: Resync, assessed: &[(Difference, Status)]) {
    let mut counts: HashMap<(Field, Status), usize> = HashMap::new();
    for (difference, status) in assessed {
        *counts
            .entry((difference.field, *status))
            .or_default() += 1;
    }
    for ((field, status), count) in &counts {
        metrics::record_catch_up_differences(protocol, field.label(), status.label(), *count);
    }
    let result = ComparisonResult::of(assessed);
    metrics::record_catch_up_comparison(protocol, resync.label(), result.label());

    let count = |wanted: Status| {
        assessed
            .iter()
            .filter(|(_, status)| *status == wanted)
            .count()
    };
    let (mismatches, known_gaps, pre_existing) =
        (count(Status::Mismatch), count(Status::KnownGap), count(Status::PreExisting));
    let block = header.number;
    let resync = resync.label();
    match result {
        ComparisonResult::Equal => {
            info!(
                protocol,
                block, resync, pre_existing, "State matches a snapshot at the same block"
            );
        }
        ComparisonResult::KnownGaps => {
            info!(
                protocol,
                block,
                resync,
                known_gaps,
                pre_existing,
                "State differs from a snapshot at the same block only in known gaps"
            );
        }
        ComparisonResult::Mismatch => {
            error!(
                protocol,
                block,
                hash = %header.hash,
                resync,
                mismatches,
                known_gaps,
                pre_existing,
                "State differs from a snapshot at the same block"
            );
        }
    }
    let mut logged: HashMap<Status, usize> = HashMap::new();
    for (difference, status) in assessed {
        let logged = logged.entry(*status).or_default();
        if *logged == MAX_LOGGED_DIFFERENCES {
            continue;
        }
        *logged += 1;
        match status {
            Status::Mismatch => {
                error!(protocol, block, hash = %header.hash, %difference, "Catch-up check mismatch")
            }
            Status::KnownGap => debug!(protocol, block, %difference, "Catch-up check known gap"),
            Status::PreExisting => {
                debug!(protocol, block, %difference, "Catch-up check pre-existing difference")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_client::feed::synchronizer::{ComponentWithState, Snapshot};
    use tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState};

    use super::{held_state::DifferenceKind, *};

    const EVERY_N_BLOCKS: u64 = 50;

    fn component_state(component_id: &str) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState::new(component_id, HashMap::new(), HashMap::new()),
            component: ProtocolComponent { id: component_id.to_string(), ..Default::default() },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn hash(number: u64) -> Bytes {
        Bytes::from(number).lpad(32, 0)
    }

    /// A message at block `number` carrying snapshots of `component_ids`.
    fn message(number: u64, component_ids: &[&str]) -> StateSyncMessage<BlockHeader> {
        StateSyncMessage {
            header: BlockHeader {
                hash: hash(number),
                number,
                parent_hash: hash(number - 1),
                ..BlockHeader::default()
            },
            snapshots: Snapshot {
                states: component_ids
                    .iter()
                    .map(|component_id| (component_id.to_string(), component_state(component_id)))
                    .collect(),
                vm_storage: HashMap::new(),
            },
            ..StateSyncMessage::default()
        }
    }

    fn holding(component_ids: &[&str]) -> ExtractorState {
        let mut state = ExtractorState::default();
        state.apply(&message(1, component_ids));
        state
    }

    fn feed(messages: Vec<(&str, StateSyncMessage<BlockHeader>)>) -> FeedMessage<BlockHeader> {
        FeedMessage {
            state_msgs: messages
                .into_iter()
                .map(|(protocol, message)| (protocol.to_string(), message))
                .collect(),
            sync_states: HashMap::new(),
        }
    }

    struct Setup {
        check: CatchUpCheck,
        trigger: GapTrigger,
        resubscriptions: UnboundedSender<Resubscription>,
        comparisons: UnboundedReceiver<Comparison>,
    }

    impl Setup {
        fn resubscribed(&self, protocol: &str, first_block: u64, follows_previous: bool) {
            self.resubscriptions
                .send(Resubscription {
                    extractor: protocol.to_string(),
                    first_block,
                    follows_previous,
                })
                .unwrap();
        }
    }

    /// A check whose protocols `a` and `b` started at block 10, `a` tracking a component; its
    /// first gap is due at block 60.
    fn started() -> Setup {
        let (resubscriptions, resubscriptions_receiver) = mpsc::unbounded_channel();
        let (comparisons_sender, comparisons) = mpsc::unbounded_channel();
        let (_, trigger) = GapInjector::new((), GAP_BLOCKS, mpsc::unbounded_channel().0);
        let mut check = CatchUpCheck::new(
            EVERY_N_BLOCKS,
            trigger.clone(),
            resubscriptions_receiver,
            comparisons_sender,
        );
        check.handle(feed(vec![("a", message(10, &["pool"])), ("b", message(10, &[]))]));
        Setup { check, trigger, resubscriptions, comparisons }
    }

    /// A started check after its first gap was forced at block 60.
    fn resyncing() -> Setup {
        let mut setup = started();
        setup
            .check
            .handle(feed(vec![("a", message(60, &[])), ("b", message(60, &[]))]));
        setup
    }

    #[test]
    fn test_the_first_gap_is_due_every_n_blocks_after_the_newest_first_block() {
        // A feed message's protocols come in hash-map order, so the newest block must decide
        // wherever it comes: one of six protocols has it, in each of eight fresh feed messages.
        for _ in 0..8 {
            let (_, trigger) = GapInjector::new((), GAP_BLOCKS, mpsc::unbounded_channel().0);
            let mut check = CatchUpCheck::new(
                EVERY_N_BLOCKS,
                trigger,
                mpsc::unbounded_channel().1,
                mpsc::unbounded_channel().0,
            );
            let mut messages: Vec<(&str, StateSyncMessage<BlockHeader>)> =
                ["a", "b", "c", "d", "e"]
                    .into_iter()
                    .map(|protocol| (protocol, message(10, &[])))
                    .collect();
            messages.push(("newest", message(12, &[])));

            check.handle(feed(messages));

            assert_eq!(check.phase, Phase::Following { gap_at: 12 + EVERY_N_BLOCKS });
        }
    }

    #[test]
    fn test_a_gap_is_forced_at_its_block_with_a_baseline_per_protocol() {
        let mut setup = started();

        setup
            .check
            .handle(feed(vec![("a", message(59, &[]))]));
        assert_eq!(setup.trigger.requested_gaps(), 0);

        setup
            .check
            .handle(feed(vec![("a", message(60, &[])), ("b", message(60, &[]))]));
        assert_eq!(setup.trigger.requested_gaps(), 1);
        let Phase::Resyncing { pending, deadline: _ } = &setup.check.phase else {
            panic!("expected to be resyncing, got {:?}", setup.check.phase);
        };
        assert_eq!(pending, &HashSet::from(["a".to_string(), "b".to_string()]));
        assert_eq!(setup.check.baselines["a"].header.number, 60);
        assert!(setup.check.baselines["a"]
            .state
            .holds_component("pool"));
    }

    #[test]
    fn test_a_recovery_is_compared_from_its_first_block_with_the_baseline() {
        let mut setup = resyncing();
        setup.resubscribed("a", 75, false);

        setup
            .check
            .handle(feed(vec![("a", message(74, &[]))]));
        assert!(setup.check.awaiting.contains_key("a"), "a message below the first block");
        assert!(setup.comparisons.try_recv().is_err());

        setup
            .check
            .handle(feed(vec![("a", message(75, &[]))]));
        let comparison = setup.comparisons.try_recv().unwrap();
        assert_eq!(comparison.protocol, "a");
        assert_eq!(comparison.resync, Resync::CaughtUp);
        assert_eq!(comparison.held.header.number, 75);
        assert_eq!(
            comparison
                .baseline
                .map(|baseline| baseline.header.number),
            Some(60)
        );
        assert!(setup.check.awaiting.is_empty());
    }

    #[test]
    fn test_resyncing_ends_once_every_protocol_recovered() {
        let mut setup = resyncing();
        setup.resubscribed("a", 75, false);
        setup.resubscribed("b", 75, false);

        setup
            .check
            .handle(feed(vec![("a", message(75, &[]))]));
        assert!(matches!(setup.check.phase, Phase::Resyncing { .. }), "b has not recovered");

        setup
            .check
            .handle(feed(vec![("b", message(76, &[]))]));
        assert_eq!(setup.check.phase, Phase::Following { gap_at: 76 + EVERY_N_BLOCKS });
        assert!(setup.check.baselines.is_empty());
    }

    #[test]
    fn test_resyncing_gives_up_at_its_deadline() {
        let mut setup = resyncing();
        if let Phase::Resyncing { pending: _, deadline } = &mut setup.check.phase {
            *deadline = Instant::now();
        }

        setup
            .check
            .handle(feed(vec![("a", message(61, &[]))]));

        assert_eq!(setup.check.phase, Phase::Following { gap_at: 61 + EVERY_N_BLOCKS });
        assert!(setup.check.baselines.is_empty());
    }

    #[test]
    fn test_resumed_recoveries_are_compared_and_untracked_ones_are_not() {
        let mut setup = started();
        setup.resubscribed("a", 20, true);
        setup.resubscribed("b", 20, false);

        setup
            .check
            .handle(feed(vec![("a", message(20, &[])), ("b", message(20, &[]))]));

        let comparison = setup.comparisons.try_recv().unwrap();
        assert_eq!((comparison.protocol.as_str(), comparison.resync), ("a", Resync::Resumed));
        assert!(comparison.baseline.is_none(), "nobody forced this resubscription");
        assert!(setup.comparisons.try_recv().is_err(), "b tracks nothing to compare");
    }

    #[test]
    fn test_a_message_resending_held_components_is_a_snapshot() {
        let held = holding(&["a", "b"]);

        assert_eq!(Resync::classify(&held, &message(2, &["a", "b"]), true), Resync::Snapshot);
    }

    #[test]
    fn test_a_message_after_a_gap_without_held_components_is_a_catch_up() {
        let held = holding(&["a"]);

        assert_eq!(Resync::classify(&held, &message(2, &["new"]), false), Resync::CaughtUp);
    }

    #[test]
    fn test_a_message_following_the_last_delivered_block_is_a_resume() {
        let held = holding(&["a"]);

        assert_eq!(Resync::classify(&held, &message(2, &[]), true), Resync::Resumed);
    }

    #[test]
    fn test_nothing_tracked_cannot_be_classified() {
        assert_eq!(
            Resync::classify(&ExtractorState::default(), &message(2, &["a"]), false),
            Resync::NothingTracked
        );
    }

    fn value_difference(field: Field, key: &str, held: u64) -> Difference {
        Difference {
            field,
            entity: "pool".to_string(),
            kind: DifferenceKind::Value {
                key: key.to_string(),
                held: Some(Bytes::from(held)),
                snapshot: None,
            },
        }
    }

    fn statuses(assessed: &[(Difference, Status)]) -> Vec<Status> {
        assessed
            .iter()
            .map(|(_, status)| *status)
            .collect()
    }

    #[test]
    fn test_differences_the_baseline_has_too_are_pre_existing() {
        let differences = vec![
            value_difference(Field::Attribute, "fee", 1),
            value_difference(Field::Attribute, "tick", 1),
            value_difference(Field::Code, "code", 1),
        ];
        let baseline = vec![
            value_difference(Field::Attribute, "fee", 1),
            value_difference(Field::ComponentBalance, "tick", 1),
        ];

        let assessed = assess(differences, &baseline);

        assert_eq!(
            statuses(&assessed),
            vec![Status::PreExisting, Status::Mismatch, Status::KnownGap]
        );
    }

    #[test]
    fn test_a_stale_value_the_gap_changed_counts_as_new() {
        let differences = vec![value_difference(Field::Attribute, "fee", 2)];
        let baseline = vec![value_difference(Field::Attribute, "fee", 1)];

        let assessed = assess(differences, &baseline);

        assert_eq!(statuses(&assessed), vec![Status::Mismatch]);
    }

    #[test]
    fn test_comparison_result_ignores_pre_existing_differences() {
        let attribute = value_difference(Field::Attribute, "fee", 1);
        let code = value_difference(Field::Code, "code", 1);

        assert_eq!(ComparisonResult::of(&[]), ComparisonResult::Equal);
        assert_eq!(
            ComparisonResult::of(&[(attribute.clone(), Status::PreExisting)]),
            ComparisonResult::Equal
        );
        assert_eq!(
            ComparisonResult::of(&[
                (attribute.clone(), Status::PreExisting),
                (code.clone(), Status::KnownGap)
            ]),
            ComparisonResult::KnownGaps
        );
        assert_eq!(
            ComparisonResult::of(&[(code, Status::KnownGap), (attribute, Status::Mismatch)]),
            ComparisonResult::Mismatch
        );
    }

    /// Snapshots whose differences from the held state the test sets per block.
    #[derive(Clone, Default)]
    struct ScriptedSnapshots {
        /// The differences the snapshot at each block shows; a block without an entry fails.
        differences: Arc<HashMap<u64, Vec<Difference>>>,
        /// The state and block of every request, in order.
        requests: Arc<std::sync::Mutex<Vec<Held>>>,
    }

    impl ScriptedSnapshots {
        fn new(differences: Vec<(u64, Vec<Difference>)>) -> Self {
            Self { differences: Arc::new(differences.into_iter().collect()), ..Self::default() }
        }

        fn requests(&self) -> Vec<Held> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SnapshotSource for ScriptedSnapshots {
        async fn differences(
            &self,
            _protocol: &str,
            held: &Held,
        ) -> Result<Vec<Difference>, RPCError> {
            self.requests
                .lock()
                .unwrap()
                .push(held.clone());
            self.differences
                .get(&held.header.number)
                .cloned()
                .ok_or_else(|| RPCError::Fatal(format!("no snapshot at {}", held.header.number)))
        }
    }

    fn held_at(number: u64, component_ids: &[&str]) -> Held {
        let message = message(number, component_ids);
        let mut state = ExtractorState::default();
        state.apply(&message);
        Held { state, header: message.header }
    }

    /// A comparison of a recovery at block 75 holding `pool` and `new`, after a gap forced at
    /// block 60 holding only `pool`.
    fn recovery_with_baseline() -> Comparison {
        Comparison {
            protocol: "a".to_string(),
            resync: Resync::CaughtUp,
            held: held_at(75, &["pool", "new"]),
            baseline: Some(held_at(60, &["pool"])),
        }
    }

    #[tokio::test]
    async fn test_the_baseline_is_compared_with_a_snapshot_at_its_own_block() {
        let stale = value_difference(Field::Attribute, "fee", 1);
        let new = value_difference(Field::Attribute, "tick", 1);
        let snapshots = ScriptedSnapshots::new(vec![
            (75, vec![stale.clone(), new.clone()]),
            (60, vec![stale.clone()]),
        ]);
        let comparison = recovery_with_baseline();

        let findings = compare(&snapshots, &comparison)
            .await
            .unwrap();

        assert_eq!(findings.assessed, vec![(stale, Status::PreExisting), (new, Status::Mismatch)]);
        assert_eq!(snapshots.requests(), vec![comparison.held, comparison.baseline.unwrap()]);
    }

    #[tokio::test]
    async fn test_a_recovery_without_differences_fetches_no_baseline_snapshot() {
        let snapshots = ScriptedSnapshots::new(vec![(75, Vec::new()), (60, Vec::new())]);
        let comparison = recovery_with_baseline();

        let findings = compare(&snapshots, &comparison)
            .await
            .unwrap();

        assert_eq!(findings.assessed, Vec::new());
        assert_eq!(snapshots.requests(), vec![comparison.held]);
    }

    #[tokio::test]
    async fn test_a_failed_baseline_fetch_counts_every_difference_as_new() {
        let difference = value_difference(Field::Attribute, "fee", 1);
        let snapshots = ScriptedSnapshots::new(vec![(75, vec![difference.clone()])]);

        let findings = compare(&snapshots, &recovery_with_baseline())
            .await
            .unwrap();

        assert_eq!(findings.assessed, vec![(difference, Status::Mismatch)]);
    }

    #[tokio::test]
    async fn test_a_failed_recovery_fetch_fails_the_comparison() {
        let snapshots = ScriptedSnapshots::new(Vec::new());

        assert!(compare(&snapshots, &recovery_with_baseline())
            .await
            .is_err());
    }

    #[rstest]
    #[case::same_state(&["pool"], true)]
    #[case::changed_state(&["pool", "new"], false)]
    #[tokio::test]
    async fn test_a_recovery_that_changed_nothing_since_the_baseline_is_flagged(
        #[case] held_components: &[&str],
        #[case] expected: bool,
    ) {
        let snapshots = ScriptedSnapshots::new(vec![(75, Vec::new())]);
        let comparison =
            Comparison { held: held_at(75, held_components), ..recovery_with_baseline() };

        let findings = compare(&snapshots, &comparison)
            .await
            .unwrap();

        assert_eq!(findings.changed_nothing, expected);
    }

    #[tokio::test]
    async fn test_a_recovery_without_baseline_is_never_flagged_unchanged() {
        let snapshots = ScriptedSnapshots::new(vec![(75, Vec::new())]);
        let comparison = Comparison { baseline: None, ..recovery_with_baseline() };

        let findings = compare(&snapshots, &comparison)
            .await
            .unwrap();

        assert!(!findings.changed_nothing);
    }
}
