//! How a resubscribing synchronizer starts its new subscription, checked against the delta
//! history model (`tycho_common::testing::delta_history`) over random extractor histories.
//!
//! For every sampled gap, a synchronizer that applied the gap's last message resubscribes. Its new
//! subscription delivers the history's messages from the gap's first live message on, and the
//! synchronizer starts from `T`, the first of them above its block. The mocked server answers the
//! catch-up request as `catch_up_spec` requires for `T`, and serves snapshots of the canonical
//! chain.
//!
//! - Property 1, never wrong: the first message the synchronizer emits takes its consumer to
//!   exactly the fold of the messages it applied before the gap, the catch-up path and `T`, a state
//!   that matches the canonical chain. When it resyncs instead, the snapshot and `T` give the
//!   canonical state.
//! - Property 2, no needless snapshots: the synchronizer takes a snapshot exactly when the spec
//!   says catch-up is not possible, when the server answers with something that is not a path to
//!   `T`, or when `T` or a message of the path is a revert. Revert messages don't restore
//!   everything yet (deferred bugs 1, 2, 24 and 25), so the synchronizer doesn't catch up through
//!   one.
//!
//! The synchronizer tracks every component of the history by id, and every component names every
//! account as its contract, so the deltas it keeps depend on neither TVL nor DCI data. What a
//! client tracks through those depends on the branches it saw (deferred bug 2) and misses the
//! contracts a retrace reaches (deferred bug 4). Until deferred bug 3 is fixed, the state after a
//! merged catch-up message takes what the merge loses from the fold (`relax_merge`).

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    hash::Hash,
};

use rand::{rngs::StdRng, Rng, SeedableRng};
use tycho_common::{
    models::{contract::Account, Address, ComponentId},
    testing::delta_history::{
        catch_up_spec, follower_states, generate_history, sample_gaps, through_the_wire,
        CatchUpSpec, DeltaStateModel, FallbackReason, Gap, History, HistoryConfig, HistoryEvent,
        RestartKind, CHAIN, EXTRACTOR,
    },
};

use super::*;

/// Histories the properties run over.
const SEEDS: u64 = 64;
/// Random gaps drawn per history, besides the gaps around restarts.
const RANDOM_GAPS: usize = 40;
/// Chance that a catch-up is run a second time against a server that answers with something that
/// is not a path to `T`.
const CORRUPTION_PROBABILITY: f64 = 0.25;

/// A history and what a client of it receives.
struct Fixture {
    seed: u64,
    history: History,
    /// `follower[i]` is the state of a client that applied every message up to `events[i]`.
    follower: Vec<DeltaStateModel>,
    /// `received[i]` is `events[i]` as a client receives it; `None` for a restart.
    received: Vec<Option<BlockAggregatedChanges>>,
    /// Every component the history creates.
    component_ids: Vec<ComponentId>,
    /// Every account the history changes.
    accounts: Vec<Address>,
}

impl Fixture {
    fn new(config: &HistoryConfig, seed: u64) -> Self {
        let history = generate_history(config, seed);
        let follower = follower_states(&history);
        let mut received = Vec::with_capacity(history.events.len());
        let mut component_ids = HashSet::new();
        let mut accounts = HashSet::new();
        for event in &history.events {
            match event {
                HistoryEvent::Message(message) => {
                    component_ids.extend(
                        message
                            .new_protocol_components
                            .keys()
                            .cloned(),
                    );
                    accounts.extend(message.account_deltas.keys().cloned());
                    accounts.extend(message.account_balances.keys().cloned());
                    received.push(Some(through_the_wire(message)));
                }
                HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => {
                    received.push(None);
                }
            }
        }
        Self {
            seed,
            history,
            follower,
            received,
            component_ids: component_ids.into_iter().collect(),
            accounts: accounts.into_iter().collect(),
        }
    }

    /// The message of `events[index]` as the server sent it.
    fn sent(&self, index: usize) -> &BlockAggregatedChanges {
        match &self.history.events[index] {
            HistoryEvent::Message(message) => message,
            HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => {
                panic!("event {index} is a restart, not a message")
            }
        }
    }

    /// The message of `events[index]` as a client receives it.
    fn received(&self, index: usize) -> &BlockAggregatedChanges {
        self.received[index]
            .as_ref()
            .unwrap_or_else(|| panic!("event {index} is a restart, not a message"))
    }

    /// The fingerprint of the extractor run that sent `events[index]`: each upgrade changes it.
    fn fingerprint_at(&self, index: usize) -> ExtractorFingerprint {
        let mut upgrades: u8 = 0;
        for event in &self.history.events[..index] {
            match event {
                HistoryEvent::ExtractorRestarted {
                    last_committed: _,
                    kind: RestartKind::Upgraded,
                } => upgrades += 1,
                HistoryEvent::ExtractorRestarted {
                    last_committed: _,
                    kind: RestartKind::Unchanged,
                } |
                HistoryEvent::Message(_) => {}
            }
        }
        fingerprint(upgrades)
    }

    /// Whether an extractor restart of `kind` lies among `events[range]`.
    fn restarted_in(&self, range: std::ops::Range<usize>, restart_kind: RestartKind) -> bool {
        self.history.events[range]
            .iter()
            .any(|event| match event {
                HistoryEvent::ExtractorRestarted { last_committed: _, kind } => {
                    *kind == restart_kind
                }
                HistoryEvent::Message(_) => false,
            })
    }
}

/// How a synchronizer must start a new subscription.
#[derive(Debug)]
enum Start {
    /// Emit `T` alone: it follows the synchronizer's block under the synced fingerprint.
    Resume,
    /// Ask to catch up, receive the path and emit it merged with `T`.
    CatchUp(Vec<Arc<BlockAggregatedChanges>>),
    /// Ask to catch up, learn why it is not possible, and resync from a snapshot at `T`.
    Unavailable(FallbackReason),
    /// Resync from a snapshot at `T` without asking: an upgraded extractor restarted in the gap.
    FingerprintChanged,
    /// Resync from a snapshot at `T` without asking: `T` is a revert.
    RevertFirst,
    /// Ask to catch up, receive a path through a revert and resync from a snapshot at `T`.
    PathThroughARevert(Vec<Arc<BlockAggregatedChanges>>),
}

/// A synchronizer that applied `events[gap.last_applied]` resubscribes, and its new subscription
/// delivers `events[gap.first_live..=t]`. It skips the messages at or below its block, so `t` is
/// the first message above it: `T`.
struct Case<'a> {
    fixture: &'a Fixture,
    gap: Gap,
    t: usize,
    /// What the server answers to a catch-up from the synchronizer's block to `T`, with the path's
    /// messages as a client receives them.
    spec: CatchUpSpec,
}

impl<'a> Case<'a> {
    /// The case of `gap`, or `None` when no message above the synchronizer's block arrives before
    /// a restart ends the new subscription.
    fn new(fixture: &'a Fixture, gap: Gap) -> Option<Self> {
        let number = fixture
            .sent(gap.last_applied)
            .block
            .number;
        let mut t = None;
        for (index, event) in fixture
            .history
            .events
            .iter()
            .enumerate()
            .skip(gap.first_live)
        {
            match event {
                HistoryEvent::Message(message) if message.block.number > number => {
                    t = Some(index);
                    break;
                }
                HistoryEvent::Message(_) => {}
                HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => return None,
            }
        }
        let t = t?;
        let spec = match catch_up_spec(
            &fixture.history,
            Gap { last_applied: gap.last_applied, first_live: t },
        ) {
            CatchUpSpec::Possible(path) => CatchUpSpec::Possible(
                path.iter()
                    .map(|message| Arc::new(through_the_wire(message)))
                    .collect(),
            ),
            CatchUpSpec::Fallback(reason) => CatchUpSpec::Fallback(reason),
        };
        Some(Self { fixture, gap, t, spec })
    }

    fn last(&self) -> &BlockAggregatedChanges {
        self.fixture
            .received(self.gap.last_applied)
    }

    /// `T` as the synchronizer receives it.
    fn first(&self) -> &BlockAggregatedChanges {
        self.fixture.received(self.t)
    }

    fn synced_fingerprint(&self) -> ExtractorFingerprint {
        self.fixture
            .fingerprint_at(self.gap.last_applied)
    }

    fn live_fingerprint(&self) -> ExtractorFingerprint {
        self.fixture
            .fingerprint_at(self.gap.first_live)
    }

    /// How the synchronizer must start. A `T` that follows its block needs no catch-up, whatever
    /// the window holds: the spec may then name a reason the server would give, such as
    /// `ToNotInWindow` for a client at the last committed block after a restart.
    fn start(&self) -> Start {
        let fingerprint_changed = self.synced_fingerprint() != self.live_fingerprint();
        assert_eq!(
            fingerprint_changed,
            self.spec == CatchUpSpec::Fallback(FallbackReason::FingerprintChanged),
            "{}: the fingerprints and the spec disagree on an upgrade in the gap",
            self.context()
        );
        if fingerprint_changed {
            return Start::FingerprintChanged;
        }
        if self.t_follows() {
            return Start::Resume;
        }
        if self.first().revert {
            return Start::RevertFirst;
        }
        match &self.spec {
            CatchUpSpec::Possible(path)
                if path
                    .iter()
                    .any(|message| message.revert) =>
            {
                Start::PathThroughARevert(path.clone())
            }
            CatchUpSpec::Possible(path) => Start::CatchUp(path.clone()),
            CatchUpSpec::Fallback(reason) => Start::Unavailable(*reason),
        }
    }

    /// Whether `T` follows the synchronizer's block. A revert never does: it carries only what
    /// the blocks it reverts changed, not its own block's changes.
    fn t_follows(&self) -> bool {
        let first = self.first();
        !first.revert && first.block.parent_hash == self.last().block.hash
    }

    /// The catch-up request for the spec's gap: from the synchronizer's block to `T`'s parent,
    /// under the fingerprint the synchronizer synced under.
    fn request(&self) -> DeltaCatchUpParams {
        let (last, first) = (&self.last().block, &self.first().block);
        DeltaCatchUpParams::new(
            CHAIN,
            EXTRACTOR,
            last.hash.clone(),
            last.number,
            first.parent_hash.clone(),
            self.synced_fingerprint(),
        )
    }

    /// The server's answer to `request`, as the spec requires it.
    fn answer(&self) -> DeltaCatchUp {
        let reason = match &self.spec {
            CatchUpSpec::Possible(path) => return DeltaCatchUp::Available(owned(path)),
            CatchUpSpec::Fallback(reason) => reason,
        };
        let unavailable = match reason {
            FallbackReason::FingerprintChanged => DeltaCatchUpUnavailable::FingerprintChanged,
            FallbackReason::EmptyWindow => DeltaCatchUpUnavailable::EmptyWindow,
            FallbackReason::BelowFloor => DeltaCatchUpUnavailable::BelowFloor,
            FallbackReason::UnknownHash => DeltaCatchUpUnavailable::UnknownHash,
            FallbackReason::ToNotInWindow => DeltaCatchUpUnavailable::ToNotInWindow,
            FallbackReason::ToNotAhead => DeltaCatchUpUnavailable::ToNotAhead,
        };
        DeltaCatchUp::Unavailable(unavailable)
    }

    fn canonical(&self) -> &DeltaStateModel {
        &self.fixture.history.canonical_states[self.t]
    }

    fn connected(&self) -> &DeltaStateModel {
        &self.fixture.history.connected_states[self.t]
    }

    fn context(&self) -> String {
        format!("seed {}, {:?}, T at event {}", self.fixture.seed, self.gap, self.t)
    }
}

fn owned(path: &[Arc<BlockAggregatedChanges>]) -> Vec<BlockAggregatedChanges> {
    path.iter()
        .map(|message| BlockAggregatedChanges::clone(message))
        .collect()
}

/// What a resubscribed synchronizer did.
struct Resubscription {
    /// The first message it emitted.
    first: StateSyncMessage<BlockHeader>,
    catch_up_requests: Vec<DeltaCatchUpParams>,
    snapshot_blocks: Vec<u64>,
}

/// Mocks a server that knows every component of `fixture`, each naming every account as its
/// contract, answers every catch-up request with `answer`, and serves snapshots of `canonical`.
fn mock_server(
    fixture: &Fixture,
    answer: DeltaCatchUp,
    canonical: DeltaStateModel,
) -> (MockRPCClient, RpcCalls) {
    let mut rpc_client = make_mock_client();
    let accounts = fixture.accounts.clone();
    rpc_client
        .expect_get_protocol_components()
        .returning(move |params| {
            let ids = params
                .component_ids()
                .cloned()
                .unwrap_or_default();
            let components: Vec<_> = ids
                .into_iter()
                .map(|id| ProtocolComponent {
                    id,
                    contract_addresses: accounts.clone(),
                    ..Default::default()
                })
                .collect();
            let total = components.len() as i64;
            Ok(Page::new(components, total, 0, 100))
        });
    let calls =
        RpcCalls { catch_up_requests: Default::default(), snapshot_blocks: Default::default() };
    let requests = calls.catch_up_requests.clone();
    rpc_client
        .expect_get_delta_catch_up()
        .returning(move |params| {
            requests.lock().unwrap().push(params);
            Ok(answer.clone())
        });
    let snapshot_blocks = calls.snapshot_blocks.clone();
    rpc_client
        .expect_get_snapshots()
        .returning(move |request, _chunk_size, _concurrency| {
            snapshot_blocks
                .lock()
                .unwrap()
                .push(request.block_number);
            Ok(snapshot_of(&canonical, request))
        });
    (rpc_client, calls)
}

/// The part of `state` that `request` asks for, as a server's snapshot carries it.
fn snapshot_of(state: &DeltaStateModel, request: &SnapshotParameters) -> Snapshot {
    let mut states = HashMap::new();
    for id in request.components.keys() {
        let Some(component) = state.components.get(id) else {
            continue;
        };
        let attributes = state
            .component_attributes
            .get(id)
            .cloned()
            .unwrap_or_default();
        let balances = state
            .component_balances
            .get(id)
            .cloned()
            .unwrap_or_default();
        let with_state = ComponentWithState {
            state: ProtocolComponentState::new(id, attributes, balances),
            component: component.clone(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        states.insert(id.clone(), with_state);
    }
    let mut vm_storage = HashMap::new();
    for address in request.contract_ids {
        let slots = state.account_storage.get(address);
        let native_balance = state
            .account_native_balances
            .get(address);
        let code = state.account_code.get(address);
        if slots.is_none() && native_balance.is_none() && code.is_none() {
            continue;
        }
        let account = Account::new(
            CHAIN,
            address.clone(),
            String::new(),
            slots.cloned().unwrap_or_default(),
            native_balance
                .cloned()
                .unwrap_or_default(),
            HashMap::new(),
            code.cloned().unwrap_or_default(),
            Bytes::new(),
            Bytes::new(),
            Bytes::new(),
            None,
        );
        vm_storage.insert(address.clone(), account);
    }
    Snapshot { states, vm_storage }
}

/// Runs a synchronizer that synced up to `case`'s last message through `case`'s new subscription,
/// with a server that answers its catch-up request with `answer`. Returns once it emitted its first
/// message.
async fn resubscribe_to(case: &Case<'_>, answer: DeltaCatchUp) -> Resubscription {
    let fixture = case.fixture;
    let (rpc_client, calls) = mock_server(fixture, answer, case.canonical().clone());
    let (deltas_client, deltas_tx) = mock_deltas_channel();
    let deltas_client = FingerprintedDeltasClient {
        inner: ArcDeltasClient(Arc::new(deltas_client)),
        fingerprint: Some(case.live_fingerprint()),
    };
    let mut state_sync = ProtocolStateSynchronizer::new(
        ExtractorIdentity::new(CHAIN, EXTRACTOR),
        true,
        ComponentFilter::Ids(fixture.component_ids.clone()),
        1,
        Duration::ZERO,
        true,
        false,
        false,
        ArcRPCClient(Arc::new(rpc_client)),
        deltas_client,
        10,
    );
    state_sync.synced_fingerprint = Some(case.synced_fingerprint());
    let mut sync = RunningSync::start(state_sync, Some(header_of(case.last()))).await;

    for index in case.gap.first_live..=case.t {
        deltas_tx
            .send(fixture.received(index).clone())
            .await
            .unwrap();
    }
    let first = timeout(Duration::from_secs(1), sync.block_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("{}: the synchronizer emitted nothing", case.context()))
        .unwrap_or_else(|| panic!("{}: the synchronizer stopped", case.context()))
        .unwrap_or_else(|err| panic!("{}: the synchronizer failed: {err}", case.context()));
    sync.stop().await;

    Resubscription {
        first,
        catch_up_requests: calls.catch_up_requests(),
        snapshot_blocks: calls.snapshot_blocks(),
    }
}

/// Checks a synchronizer that starts without a snapshot: from `T` alone when `path` is `None`,
/// or after catching up on `path`. Returns the number of components the merge put in both
/// component maps.
async fn check_resume_or_catch_up(
    case: &Case<'_>,
    path: Option<&[Arc<BlockAggregatedChanges>]>,
) -> usize {
    let context = case.context();
    let run = resubscribe_to(case, case.answer()).await;

    let requests = if path.is_some() { vec![case.request()] } else { Vec::new() };
    assert_eq!(run.catch_up_requests, requests, "{context}: catch-up requests");
    assert_eq!(run.snapshot_blocks, Vec::<u64>::new(), "{context}: Property 2, needless snapshot");
    let StateSyncMessage { header, snapshots, deltas, removed_components } = run.first;
    assert_eq!(header, header_of(case.first()), "{context}: header");
    assert_eq!(snapshots, Snapshot::default(), "{context}: snapshots");
    assert!(removed_components.is_empty(), "{context}: removed {removed_components:?}");
    let merged = deltas.unwrap_or_else(|| panic!("{context}: no deltas"));

    let prior = &case.fixture.follower[case.gap.last_applied];
    let mut expected = prior.clone();
    let path = path.unwrap_or_default();
    let mut received: Vec<&BlockAggregatedChanges> = path.iter().map(Arc::as_ref).collect();
    received.push(case.first());
    for message in &received {
        expected.apply(message);
    }
    // The model applies a component by its `change`, whichever map holds it.
    if let Some((map, id)) = invented_component_entry(&merged, &received) {
        panic!("{context}: Property 1, the emitted {map} holds {id} as no received message does");
    }
    let mut actual = prior.clone();
    actual.apply(&merged);
    let in_both_maps =
        if path.is_empty() { 0 } else { relax_merge(&mut actual, &expected, &received) };

    if let Some(difference) = difference(&actual, &expected) {
        panic!("{context}: Property 1, the consumer's state differs from the fold: {difference}");
    }
    assert_eq!(
        actual.check_against(case.canonical(), case.connected()),
        Ok(()),
        "{context}: Property 1, the consumer's state is not canonical"
    );
    in_both_maps
}

/// An entry of `merged`'s component maps that no message of `received` holds in the same map, as
/// the map's name and the component id.
fn invented_component_entry<'a>(
    merged: &'a BlockAggregatedChanges,
    received: &[&BlockAggregatedChanges],
) -> Option<(&'static str, &'a ComponentId)> {
    for (id, component) in &merged.new_protocol_components {
        if !received
            .iter()
            .any(|message| message.new_protocol_components.get(id) == Some(component))
        {
            return Some(("new_protocol_components", id));
        }
    }
    for (id, component) in &merged.deleted_protocol_components {
        if !received.iter().any(|message| {
            message
                .deleted_protocol_components
                .get(id) ==
                Some(component)
        }) {
            return Some(("deleted_protocol_components", id));
        }
    }
    None
}

/// Takes from `expected` what `StateSyncMessage::merge` loses when the synchronizer merges the
/// `received` messages into one (deferred bug 3): the DCI data of every message but the first,
/// and the fate of every component that one of the messages names in `deleted_protocol_components`
/// and one names in `new_protocol_components`, which the merge puts in both maps, together with
/// its attributes and balances. The components come from the messages the synchronizer received,
/// not from the one it emitted, so a synchronizer that puts other components in both maps fails
/// the comparison. Returns the number of such components.
fn relax_merge(
    actual: &mut DeltaStateModel,
    expected: &DeltaStateModel,
    received: &[&BlockAggregatedChanges],
) -> usize {
    actual.component_entrypoints = expected.component_entrypoints.clone();
    actual.entrypoint_contracts = expected.entrypoint_contracts.clone();
    let mut named = HashSet::new();
    let mut deleted = HashSet::new();
    for message in received {
        named.extend(message.new_protocol_components.keys());
        deleted.extend(
            message
                .deleted_protocol_components
                .keys(),
        );
    }
    let mut in_both_maps = 0;
    for id in named.intersection(&deleted) {
        in_both_maps += 1;
        copy_entry(&mut actual.components, &expected.components, id);
        copy_entry(&mut actual.component_attributes, &expected.component_attributes, id);
        copy_entry(&mut actual.component_balances, &expected.component_balances, id);
    }
    in_both_maps
}

fn copy_entry<V: Clone>(
    to: &mut HashMap<ComponentId, V>,
    from: &HashMap<ComponentId, V>,
    id: &ComponentId,
) {
    match from.get(id) {
        Some(value) => {
            to.insert(id.clone(), value.clone());
        }
        None => {
            to.remove(id);
        }
    }
}

/// Checks a synchronizer that resyncs from a snapshot at `T`, after asking to catch up when
/// `asks`, and gets `answer` if it asks.
async fn check_resync(case: &Case<'_>, answer: DeltaCatchUp, asks: bool) {
    let context = case.context();
    let run = resubscribe_to(case, answer).await;

    let requests = if asks { vec![case.request()] } else { Vec::new() };
    assert_eq!(run.catch_up_requests, requests, "{context}: catch-up requests");
    let first = case.first();
    assert_eq!(
        run.snapshot_blocks,
        vec![first.block.number],
        "{context}: Property 2, the synchronizer must resync from a snapshot at T"
    );
    assert_eq!(run.first.header, header_of(first), "{context}: header");
    assert_eq!(run.first.deltas.as_ref(), Some(first), "{context}: the deltas are not T's alone");
    assert!(run.first.removed_components.is_empty(), "{context}: removed components");

    let resynced = snapshot_fields(&resynced(&run.first));
    if let Some(difference) = difference(&resynced, &snapshot_fields(case.canonical())) {
        panic!("{context}: Property 1, the resynced state is not canonical: {difference}");
    }
}

/// The state a consumer holds after resyncing from `message`: its snapshot replaces what the
/// consumer held, then its deltas apply.
fn resynced(message: &StateSyncMessage<BlockHeader>) -> DeltaStateModel {
    let StateSyncMessage { header: _, snapshots, deltas, removed_components: _ } = message;
    let Snapshot { states, vm_storage } = snapshots;
    let mut state = DeltaStateModel::default();
    for (id, with_state) in states {
        let ComponentWithState {
            state: component_state,
            component,
            component_tvl: _,
            entrypoints: _,
        } = with_state;
        let ProtocolComponentState { component_id: _, attributes, balances } = component_state;
        state
            .components
            .insert(id.clone(), component.clone());
        if !attributes.is_empty() {
            state
                .component_attributes
                .insert(id.clone(), attributes.clone());
        }
        if !balances.is_empty() {
            state
                .component_balances
                .insert(id.clone(), balances.clone());
        }
    }
    for (address, account) in vm_storage {
        let Account {
            chain: _,
            address: _,
            title: _,
            slots,
            native_balance,
            token_balances: _,
            code,
            code_hash: _,
            balance_modify_tx: _,
            code_modify_tx: _,
            creation_tx: _,
        } = account;
        if !slots.is_empty() {
            state
                .account_storage
                .insert(address.clone(), slots.clone());
        }
        if !native_balance.is_empty() {
            state
                .account_native_balances
                .insert(address.clone(), native_balance.clone());
        }
        if !code.is_empty() {
            state
                .account_code
                .insert(address.clone(), code.clone());
        }
    }
    if let Some(deltas) = deltas {
        state.apply(deltas);
    }
    state
}

/// The part of `state` a snapshot carries.
fn snapshot_fields(state: &DeltaStateModel) -> DeltaStateModel {
    let DeltaStateModel {
        extractor,
        head,
        components,
        component_attributes,
        component_balances,
        account_storage,
        account_native_balances,
        account_code,
        // The snapshot request asks for none of these; consumers keep the tokens they hold.
        account_token_balances: _,
        tokens: _,
        component_tvl: _,
        component_entrypoints: _,
        entrypoint_contracts: _,
    } = state;
    DeltaStateModel {
        extractor: extractor.clone(),
        head: head.clone(),
        components: components.clone(),
        component_attributes: component_attributes.clone(),
        component_balances: component_balances.clone(),
        account_storage: account_storage.clone(),
        account_native_balances: account_native_balances.clone(),
        account_code: account_code.clone(),
        ..Default::default()
    }
}

/// Names the first field in which `actual` differs from `expected`, with the entries that differ.
fn difference(actual: &DeltaStateModel, expected: &DeltaStateModel) -> Option<String> {
    let DeltaStateModel {
        extractor,
        head,
        components,
        component_attributes,
        component_balances,
        account_storage,
        account_native_balances,
        account_code,
        account_token_balances,
        tokens,
        component_tvl,
        component_entrypoints,
        entrypoint_contracts,
    } = actual;
    let differences = [
        ("extractor", differing_value(extractor, &expected.extractor)),
        ("head", differing_value(head, &expected.head)),
        ("components", differing_entries(components, &expected.components)),
        (
            "component_attributes",
            differing_entries(component_attributes, &expected.component_attributes),
        ),
        ("component_balances", differing_entries(component_balances, &expected.component_balances)),
        ("account_storage", differing_entries(account_storage, &expected.account_storage)),
        (
            "account_native_balances",
            differing_entries(account_native_balances, &expected.account_native_balances),
        ),
        ("account_code", differing_entries(account_code, &expected.account_code)),
        (
            "account_token_balances",
            differing_entries(account_token_balances, &expected.account_token_balances),
        ),
        ("tokens", differing_entries(tokens, &expected.tokens)),
        ("component_tvl", differing_entries(component_tvl, &expected.component_tvl)),
        (
            "component_entrypoints",
            differing_entries(component_entrypoints, &expected.component_entrypoints),
        ),
        (
            "entrypoint_contracts",
            differing_entries(entrypoint_contracts, &expected.entrypoint_contracts),
        ),
    ];
    for (field, difference) in differences {
        if let Some(difference) = difference {
            return Some(format!("{field}: {difference}"));
        }
    }
    None
}

fn differing_value<T: PartialEq + Debug>(actual: &T, expected: &T) -> Option<String> {
    (actual != expected).then(|| format!("{actual:?}, expected {expected:?}"))
}

/// The entries under keys whose values differ, as `key: actual, expected`.
fn differing_entries<K: Eq + Hash + Debug, V: PartialEq + Debug>(
    actual: &HashMap<K, V>,
    expected: &HashMap<K, V>,
) -> Option<String> {
    let mut entries = Vec::new();
    for key in actual.keys().chain(expected.keys()) {
        let (held, wanted) = (actual.get(key), expected.get(key));
        if held != wanted {
            entries.push(format!("{key:?}: {held:?}, expected {wanted:?}"));
        }
    }
    entries.sort();
    entries.dedup();
    (!entries.is_empty()).then(|| entries.join("; "))
}

/// Ways a server's answer can fail to be a path to `T`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Corruption {
    /// The path without its last message: it ends short of the target.
    ShortOfTheTarget,
    /// The path followed by `T`: it ends past the target.
    PastTheTarget,
    /// The path with its last message from another extractor.
    ForeignMessage,
    /// The path, then a revert to a sibling of the block its first block extends, then its blocks
    /// again from that sibling. Every block extends the one before it and the walk ends at the
    /// target; only the revert names a block other than the one the walk passed at its height.
    RevertToASibling,
}

const CORRUPTIONS: [Corruption; 4] = [
    Corruption::ShortOfTheTarget,
    Corruption::PastTheTarget,
    Corruption::ForeignMessage,
    Corruption::RevertToASibling,
];

fn corrupt(
    mut path: Vec<BlockAggregatedChanges>,
    first: &BlockAggregatedChanges,
    corruption: Corruption,
) -> Vec<BlockAggregatedChanges> {
    match corruption {
        Corruption::ShortOfTheTarget => {
            path.pop();
        }
        Corruption::PastTheTarget => path.push(first.clone()),
        Corruption::ForeignMessage => {
            if let Some(last) = path.last_mut() {
                last.extractor = "another_extractor".to_string();
            }
        }
        Corruption::RevertToASibling => {
            // A spec path lists its reverts before its blocks, and a path to a block above the
            // synchronizer's holds at least one block.
            let first_block = path
                .iter()
                .position(|message| !message.revert)
                .expect("a catch-up path holds a block");
            let mut blocks = path[first_block..].to_vec();
            let opening_block = &blocks[0].block;
            let mut sibling = opening_block.parent_hash.to_vec();
            if let Some(byte) = sibling.last_mut() {
                *byte ^= 0xff;
            }
            let sibling = Bytes::from(sibling);
            let revert = BlockAggregatedChanges {
                extractor: blocks[0].extractor.clone(),
                chain: blocks[0].chain,
                block: Block {
                    number: opening_block.number - 1,
                    hash: sibling.clone(),
                    parent_hash: Bytes::new(),
                    chain: opening_block.chain,
                    ts: opening_block.ts,
                },
                revert: true,
                ..Default::default()
            };
            blocks[0].block.parent_hash = sibling;
            path.push(revert);
            path.extend(blocks);
        }
    }
    path
}

/// How often each way of starting a subscription occurs.
#[derive(Debug, Default)]
struct Outcomes {
    /// Gaps whose new subscription holds no message above the synchronizer's block before a
    /// restart ends it.
    without_t: usize,
    /// Subscriptions that start with messages at or below the synchronizer's block.
    skipped_live_messages: usize,
    resumed: usize,
    resumed_across_an_unchanged_restart: usize,
    caught_up: usize,
    caught_up_across_an_unchanged_restart: usize,
    /// Resyncs for a path through one revert or more.
    through_a_revert: usize,
    /// Resyncs for a path through two reverts or more.
    through_nested_reverts: usize,
    /// Resyncs for a `T` that is a revert.
    revert_first: usize,
    /// Components the merge put in both component maps, compared through the relaxation.
    components_in_both_maps: usize,
    /// Upgrades in the gap with a `T` that follows the synchronizer's block: only the fingerprint
    /// gate keeps the synchronizer from resuming.
    upgraded_with_t_following: usize,
    fallbacks: HashMap<FallbackReason, usize>,
    rejected_answers: HashMap<Corruption, usize>,
}

impl Outcomes {
    fn add(&mut self, case: &Case, start: &Start) {
        let Gap { last_applied, first_live } = case.gap;
        let fixture = case.fixture;
        let unchanged_restart =
            fixture.restarted_in(last_applied..first_live, RestartKind::Unchanged);
        self.skipped_live_messages += usize::from(case.t > first_live);
        match start {
            Start::Resume => {
                self.resumed += 1;
                self.resumed_across_an_unchanged_restart += usize::from(unchanged_restart);
            }
            Start::CatchUp(_) => {
                self.caught_up += 1;
                self.caught_up_across_an_unchanged_restart += usize::from(unchanged_restart);
            }
            Start::PathThroughARevert(path) => {
                let reverts = path
                    .iter()
                    .filter(|message| message.revert)
                    .count();
                self.through_a_revert += 1;
                self.through_nested_reverts += usize::from(reverts >= 2);
            }
            Start::RevertFirst => self.revert_first += 1,
            Start::Unavailable(reason) => {
                *self
                    .fallbacks
                    .entry(*reason)
                    .or_default() += 1;
            }
            Start::FingerprintChanged => {
                *self
                    .fallbacks
                    .entry(FallbackReason::FingerprintChanged)
                    .or_default() += 1;
                self.upgraded_with_t_following += usize::from(case.t_follows());
            }
        }
    }
}

#[tokio::test]
async fn test_resubscriptions_match_the_delta_history_model() {
    let config = HistoryConfig::default();
    let mut gap_rng = StdRng::seed_from_u64(0);
    let mut corruption_rng = StdRng::seed_from_u64(1);
    let mut outcomes = Outcomes::default();
    for seed in 0..SEEDS {
        let fixture = Fixture::new(&config, seed);
        for gap in sample_gaps(&fixture.history, RANDOM_GAPS, &mut gap_rng) {
            let Some(case) = Case::new(&fixture, gap) else {
                outcomes.without_t += 1;
                continue;
            };
            let start = case.start();
            outcomes.add(&case, &start);
            match start {
                Start::Resume => {
                    check_resume_or_catch_up(&case, None).await;
                }
                Start::CatchUp(path) => {
                    outcomes.components_in_both_maps +=
                        check_resume_or_catch_up(&case, Some(&path)).await;
                    if corruption_rng.gen_bool(CORRUPTION_PROBABILITY) {
                        let corruption =
                            CORRUPTIONS[corruption_rng.gen_range(0..CORRUPTIONS.len())];
                        let corrupted = corrupt(owned(&path), case.first(), corruption);
                        // The synchronizer refuses every path through a revert, so the resync
                        // alone would not show that the path check rejects a revert to a sibling.
                        if corruption == Corruption::RevertToASibling {
                            let checked = check_catch_up_path(
                                &corrupted,
                                &ExtractorIdentity::new(CHAIN, EXTRACTOR),
                                &header_of(case.last()),
                                &case.first().block.parent_hash,
                            );
                            assert!(
                                matches!(checked, Err(CatchUpSkipped::RevertToNonAncestor { .. })),
                                "{}: the path check accepts a revert to a sibling: {checked:?}",
                                case.context()
                            );
                        }
                        check_resync(&case, DeltaCatchUp::Available(corrupted), true).await;
                        *outcomes
                            .rejected_answers
                            .entry(corruption)
                            .or_default() += 1;
                    }
                }
                Start::Unavailable(_) | Start::PathThroughARevert(_) => {
                    check_resync(&case, case.answer(), true).await
                }
                Start::FingerprintChanged | Start::RevertFirst => {
                    check_resync(&case, case.answer(), false).await
                }
            }
        }
    }

    let Outcomes {
        without_t: _,
        skipped_live_messages,
        resumed,
        resumed_across_an_unchanged_restart,
        caught_up,
        caught_up_across_an_unchanged_restart,
        through_a_revert,
        through_nested_reverts,
        revert_first,
        components_in_both_maps: _,
        upgraded_with_t_following,
        fallbacks,
        rejected_answers,
    } = &outcomes;
    for (shape, count) in [
        ("subscriptions starting at or below the synchronizer's block", skipped_live_messages),
        ("resumes", resumed),
        ("resumes across an unchanged restart", resumed_across_an_unchanged_restart),
        ("catch-ups", caught_up),
        ("catch-ups across an unchanged restart", caught_up_across_an_unchanged_restart),
        ("resyncs for a path through a revert", through_a_revert),
        ("resyncs for a path through nested reverts", through_nested_reverts),
        ("resyncs for a T that is a revert", revert_first),
        ("upgrades with a T that follows the synchronizer's block", upgraded_with_t_following),
    ] {
        assert!(*count > 0, "no {shape} in {outcomes:?}");
    }
    // A `T` above the synchronizer's block that does not follow it extends the window's tip or
    // reverts to a block the window holds, and the walk from the synchronizer's block ends below
    // it: `ToNotInWindow` and `ToNotAhead` cannot occur.
    for reason in [
        FallbackReason::FingerprintChanged,
        FallbackReason::EmptyWindow,
        FallbackReason::BelowFloor,
        FallbackReason::UnknownHash,
    ] {
        assert!(fallbacks.contains_key(&reason), "no {reason:?} in {outcomes:?}");
    }
    for corruption in CORRUPTIONS {
        assert!(rejected_answers.contains_key(&corruption), "no {corruption:?} in {outcomes:?}");
    }
}
