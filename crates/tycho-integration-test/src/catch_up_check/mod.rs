//! Forces a `tycho-client` stream to resubscribe after missing blocks, and checks the state it
//! holds afterwards against a snapshot of the same components at the same block.
//!
//! The check runs a stream of its own for the protocol stream's protocols: one
//! `ProtocolStateSynchronizer` per protocol under a `BlockSynchronizer`, wired like
//! `TychoStreamBuilder::build` but over a [`GapInjector`]. Every `every_n_blocks` blocks, each
//! subscription drops its next [`GAP_BLOCKS`] messages and ends. The synchronizers resubscribe
//! having missed those blocks, which they catch up on through the server's delta catch-up
//! endpoint, or recover from with a snapshot when the server cannot serve them.
//!
//! Every resubscription is classified, forced or not. After a catch-up or a snapshot, the state
//! held for the protocol is compared with a snapshot of its components at that block.

pub mod gap_injector;
pub mod held_state;

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use miette::{miette, IntoDiagnostic, WrapErr};
use tokio::{
    sync::mpsc::{self, Receiver, UnboundedReceiver},
    task::JoinHandle,
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
/// blocks.
pub const GAP_BLOCKS: usize = 3;

/// How long the protocols get to resubscribe after a gap. Covers the client's snapshot delay of up
/// to a minute when it cannot catch up.
const RESYNC_DEADLINE: Duration = Duration::from_secs(300);

/// Differences logged one by one per comparison; the rest are only counted.
const MAX_LOGGED_DIFFERENCES: usize = 10;

/// Retry settings of the protocol stream, so the check's stream recovers the same way.
const WS_RETRY_COOLDOWN: Duration = Duration::from_secs(1);
const SYNC_RETRY_COOLDOWN: Duration = Duration::from_secs(3);

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

/// The stream's output.
type Feed = Receiver<Result<FeedMessage<BlockHeader>, BlockSynchronizerError>>;

/// Sets up the check's stream. Returns once every synchronizer delivered its first message or gave
/// up.
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
    let rpc = HttpRPCClient::new(
        &format!("{rpc_scheme}://{tycho_url}"),
        HttpRPCClientOptions::new()
            .with_auth_key(Some(api_key))
            .with_compression(true),
    )
    .into_diagnostic()
    .wrap_err("Failed to create the catch-up check's RPC client")?;

    let (resubscriptions_sender, resubscriptions) = mpsc::unbounded_channel();
    let (deltas, trigger) = GapInjector::new(ws_client, GAP_BLOCKS, resubscriptions_sender);
    let requested: HashSet<String> = protocols.iter().cloned().collect();
    let dci_protocols = ProtocolSystemsInfo::fetch(&rpc, chain, &requested)
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
            rpc.clone(),
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

    let check = CatchUpCheck {
        chain,
        rpc,
        dci_protocols,
        every_n_blocks,
        trigger,
        resubscriptions,
        held: HashMap::new(),
        awaiting: HashMap::new(),
        phase: Phase::Starting,
    };
    Ok((check, feed, ws_task))
}

/// The timing `TychoStreamBuilder` uses for a chain, in seconds and blocks, so the check's stream
/// waits for and gives up on synchronizers like a production one.
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
    /// The new subscription's first block followed the last one synced: nothing was missed.
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

/// When the check forces the next gap.
enum Phase {
    /// Waiting for the stream's first message.
    Starting,
    /// Following the stream until block `gap_at`.
    Following { gap_at: u64 },
    /// A gap was forced; waiting until `deadline` for the protocols in `pending` to resubscribe.
    Resyncing { pending: HashSet<String>, deadline: Instant },
}

struct CatchUpCheck {
    chain: Chain,
    rpc: HttpRPCClient,
    dci_protocols: HashSet<String>,
    every_n_blocks: u64,
    trigger: GapTrigger,
    resubscriptions: UnboundedReceiver<Resubscription>,
    /// The state held for each protocol, built from the stream's messages.
    held: HashMap<String, ExtractorState>,
    /// Resubscriptions whose first message has not reached the stream's output yet, by protocol.
    awaiting: HashMap<String, Resubscription>,
    phase: Phase,
}

impl CatchUpCheck {
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
    /// first after a resubscription, records how it recovered and, after a catch-up or a
    /// snapshot, starts a comparison with a snapshot at the message's block.
    fn apply(&mut self, protocol: &str, message: &StateSyncMessage<BlockHeader>) {
        let held = self
            .held
            .entry(protocol.to_string())
            .or_default();
        let recovers_here = self
            .awaiting
            .get(protocol)
            .is_some_and(|resubscription| message.header.number >= resubscription.first_block);
        let resubscription = if recovers_here { self.awaiting.remove(protocol) } else { None };
        let Some(resubscription) = resubscription else {
            held.apply(message);
            return;
        };
        let resync = Resync::classify(held, message, resubscription.follows_previous);
        held.apply(message);
        let state = held.clone();
        let block = message.header.number;
        info!(protocol, block, resync = resync.label(), "Protocol resubscribed");
        metrics::record_catch_up_resync(protocol, resync.label());
        if let Phase::Resyncing { pending, deadline: _ } = &mut self.phase {
            pending.remove(protocol);
        }
        match resync {
            Resync::CaughtUp | Resync::Snapshot => {
                self.spawn_comparison(protocol, state, block, resync)
            }
            Resync::Resumed | Resync::NothingTracked => {}
        }
    }

    fn advance(&mut self, block: u64) {
        let next = match &mut self.phase {
            Phase::Starting => Some(Phase::Following { gap_at: block + self.every_n_blocks }),
            Phase::Following { gap_at } if block >= *gap_at => {
                info!(
                    block,
                    gap_blocks = GAP_BLOCKS,
                    "Forcing the catch-up check's protocols to resubscribe after a gap"
                );
                self.trigger.request_gap();
                Some(Phase::Resyncing {
                    pending: self.held.keys().cloned().collect(),
                    deadline: Instant::now() + RESYNC_DEADLINE,
                })
            }
            Phase::Following { gap_at: _ } => None,
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
                Some(Phase::Following { gap_at: block + self.every_n_blocks })
            }
        };
        if let Some(next) = next {
            self.phase = next;
        }
    }

    fn spawn_comparison(&self, protocol: &str, held: ExtractorState, block: u64, resync: Resync) {
        let rpc = self.rpc.clone();
        let chain = self.chain;
        let protocol = protocol.to_string();
        let uses_dci = self.dci_protocols.contains(&protocol);
        tokio::spawn(async move {
            match fetch_snapshot(&rpc, chain, &protocol, uses_dci, &held, block).await {
                Ok(snapshot) => report(&protocol, block, resync, &diff(&held, &snapshot)),
                Err(e) => {
                    warn!(
                        protocol,
                        block,
                        resync = resync.label(),
                        error = %e,
                        "Failed to fetch a snapshot to compare the catch-up check's state with"
                    );
                    metrics::record_catch_up_comparison(&protocol, resync.label(), "error");
                }
            }
        });
    }
}

/// Fetches the state at `block` of the components `held` holds, with the contracts they name and,
/// for a DCI protocol, the contracts their traced entry points reach.
async fn fetch_snapshot(
    rpc: &HttpRPCClient,
    chain: Chain,
    protocol: &str,
    uses_dci: bool,
    held: &ExtractorState,
    block: u64,
) -> Result<ExtractorState, RPCError> {
    let components = held.components();
    let mut contracts = held.component_contracts();
    if uses_dci {
        let traced = rpc
            .get_traced_entry_points_paginated(TracedEntryPointsPaginatedParams::new(
                chain,
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
    let request =
        SnapshotParameters::new(chain, protocol, &components, &contracts, block).include_tvl(false);
    let snapshot = rpc
        .get_snapshots(&request, None, RPC_CLIENT_CONCURRENCY)
        .await?;
    Ok(ExtractorState::from_snapshot(&snapshot))
}

/// How a comparison ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComparisonResult {
    Equal,
    /// Differs only in fields the server does not restore on reverts yet.
    KnownGaps,
    Mismatch,
}

impl ComparisonResult {
    fn of(differences: &[Difference]) -> Self {
        let mut result = ComparisonResult::Equal;
        for difference in differences {
            match difference.field.status() {
                Status::Mismatch => return ComparisonResult::Mismatch,
                Status::KnownGap => result = ComparisonResult::KnownGaps,
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

/// Records a comparison's differences in metrics and logs.
fn report(protocol: &str, block: u64, resync: Resync, differences: &[Difference]) {
    let mut by_field: HashMap<Field, usize> = HashMap::new();
    for difference in differences {
        *by_field
            .entry(difference.field)
            .or_default() += 1;
    }
    for (field, count) in &by_field {
        metrics::record_catch_up_differences(
            protocol,
            field.label(),
            field.status().label(),
            *count,
        );
    }
    let result = ComparisonResult::of(differences);
    metrics::record_catch_up_comparison(protocol, resync.label(), result.label());

    let (mismatches, known_gaps): (Vec<&Difference>, Vec<&Difference>) = differences
        .iter()
        .partition(|difference| difference.field.status() == Status::Mismatch);
    match result {
        ComparisonResult::Equal => {
            info!(
                protocol,
                block,
                resync = resync.label(),
                "State matches a snapshot at the same block"
            );
        }
        ComparisonResult::KnownGaps => {
            info!(
                protocol,
                block,
                resync = resync.label(),
                known_gaps = known_gaps.len(),
                "State differs from a snapshot at the same block only in known gaps"
            );
        }
        ComparisonResult::Mismatch => {
            error!(
                protocol,
                block,
                resync = resync.label(),
                mismatches = mismatches.len(),
                known_gaps = known_gaps.len(),
                "State differs from a snapshot at the same block"
            );
            for difference in mismatches
                .iter()
                .take(MAX_LOGGED_DIFFERENCES)
            {
                error!(protocol, block, %difference, "Catch-up check mismatch");
            }
        }
    }
    for difference in known_gaps
        .iter()
        .take(MAX_LOGGED_DIFFERENCES)
    {
        debug!(protocol, block, %difference, "Catch-up check known gap");
    }
}

#[cfg(test)]
mod tests {
    use tycho_client::feed::synchronizer::{ComponentWithState, Snapshot};
    use tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState};

    use super::{held_state::DifferenceKind, *};

    fn component_state(component_id: &str) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState::new(component_id, HashMap::new(), HashMap::new()),
            component: ProtocolComponent { id: component_id.to_string(), ..Default::default() },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn message_with_snapshots(component_ids: &[&str]) -> StateSyncMessage<BlockHeader> {
        StateSyncMessage {
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
        state.apply(&message_with_snapshots(component_ids));
        state
    }

    #[test]
    fn test_a_message_resending_held_components_is_a_snapshot() {
        let held = holding(&["a", "b"]);

        assert_eq!(
            Resync::classify(&held, &message_with_snapshots(&["a", "b"]), true),
            Resync::Snapshot
        );
    }

    #[test]
    fn test_a_message_after_a_gap_without_held_components_is_a_catch_up() {
        let held = holding(&["a"]);

        assert_eq!(
            Resync::classify(&held, &message_with_snapshots(&["new"]), false),
            Resync::CaughtUp
        );
    }

    #[test]
    fn test_a_message_following_the_last_synced_block_is_a_resume() {
        let held = holding(&["a"]);

        assert_eq!(Resync::classify(&held, &message_with_snapshots(&[]), true), Resync::Resumed);
    }

    #[test]
    fn test_nothing_tracked_cannot_be_classified() {
        assert_eq!(
            Resync::classify(&ExtractorState::default(), &message_with_snapshots(&["a"]), false),
            Resync::NothingTracked
        );
    }

    fn difference(field: Field) -> Difference {
        Difference { field, entity: "a".to_string(), kind: DifferenceKind::OnlyHeld }
    }

    #[test]
    fn test_comparison_result_is_the_worst_difference() {
        assert_eq!(ComparisonResult::of(&[]), ComparisonResult::Equal);
        assert_eq!(
            ComparisonResult::of(&[difference(Field::Code), difference(Field::DciAccount)]),
            ComparisonResult::KnownGaps
        );
        assert_eq!(
            ComparisonResult::of(&[difference(Field::Code), difference(Field::Storage)]),
            ComparisonResult::Mismatch
        );
    }
}
