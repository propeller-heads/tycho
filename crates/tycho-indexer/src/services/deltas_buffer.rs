use std::{
    collections::{HashMap, HashSet},
    sync::{mpsc::SyncSender, Arc, Mutex},
    time::Duration,
};

use deepsize::DeepSizeOf;
use futures03::{stream, StreamExt};
use metrics::gauge;
use thiserror::Error;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, instrument, trace, warn, Level};
use tycho_common::{
    models::{
        blockchain::BlockAggregatedChanges,
        contract::Account,
        protocol::{ProtocolComponent, ProtocolComponentState},
        MergeError,
    },
    storage::StorageError,
    Bytes,
};

use crate::extractor::reorg_buffer::{BlockNumberOrTimestamp, CommitStatus, ReorgBuffer};

/// The `PendingDeltas` struct manages access to the reorg buffers maintained by each extractor.
///
/// The main responsibilities of `PendingDeltas` include:
/// - Inserting new blocks and deltas into the correct `ReorgBuffer`.
/// - Managing and applying deltas to state data, which includes merging buffered changes with data
///   fetched from the database.
/// - Retrieving commit status for blocks, which is used to determine whether to fetch data from the
///   database and/or from the buffer.
#[derive(Default, Clone, DeepSizeOf)]
pub struct PendingDeltas {
    // Map with the protocol system name as key and a `ReorgBuffer` as value.
    buffers: HashMap<String, Arc<Mutex<ReorgBuffer<BlockAggregatedChanges>>>>,
}

#[derive(Error, Debug, PartialEq)]
pub enum PendingDeltasError {
    #[error("Failed to acquire {0} lock: {1}")]
    LockError(String, String),
    #[error("ReorgBufferError: {0}")]
    ReorgBufferError(#[from] StorageError),
    #[error("Unknown extractor: {0}")]
    UnknownExtractor(String),
    #[error("Failed applying deltas: {0}")]
    DeltaApplicationFailure(#[from] MergeError),
}

pub type Result<T> = std::result::Result<T, PendingDeltasError>;

#[async_trait::async_trait]
pub trait PendingDeltasBuffer {
    fn merge_native_states(
        &self,
        protocol_ids: Option<&[&str]>,
        db_states: &mut Vec<ProtocolComponentState>,
        version: Option<BlockNumberOrTimestamp>,
        protocol_system: &str,
    ) -> Result<()>;

    fn update_vm_states(
        &self,
        addresses: Option<&[Bytes]>,
        db_states: &mut Vec<Account>,
        version: Option<BlockNumberOrTimestamp>,
        protocol_system: &str,
    ) -> Result<()>;

    fn get_new_components(
        &self,
        ids: Option<&[&str]>,
        protocol_system: &str,
        min_tvl: Option<f64>,
    ) -> Result<Vec<ProtocolComponent>>;

    fn get_block_commit_status(
        &self,
        version: BlockNumberOrTimestamp,
        protocol_system: &str,
    ) -> Result<Option<CommitStatus>>;

    fn search_block(
        &self,
        f: &dyn Fn(&BlockAggregatedChanges) -> bool,
        protocol_system: &str,
    ) -> Result<Option<BlockAggregatedChanges>>;
}

impl PendingDeltas {
    pub fn new<'a>(extractors: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            buffers: extractors
                .into_iter()
                .map(|e| {
                    debug!("Creating new ReorgBuffer for {}", e);
                    (e.to_string(), Arc::new(Mutex::new(ReorgBuffer::new())))
                })
                .collect(),
        }
    }

    fn insert(&self, message: Arc<BlockAggregatedChanges>) -> Result<()> {
        let maybe_buffer = self.buffers.get(&message.extractor);

        match maybe_buffer {
            Some(buffer) => {
                let mut guard = buffer.lock().map_err(|e| {
                    PendingDeltasError::LockError(message.extractor.to_string(), e.to_string())
                })?;
                if message.revert {
                    // Skip partial reverts: PendingDeltas only holds full-block messages
                    if message.partial_block_index.is_none() {
                        trace!(
                            block_number = message.block.number,
                            extractor = message.extractor,
                            "DeltaBufferPurge"
                        );
                        guard.purge(message.block.hash.clone())?;
                    }
                } else {
                    trace!(
                        block_number = message.block.number,
                        finality = message.finalized_block_height,
                        db_commit_upto = message.db_committed_block_height,
                        extractor = message.extractor,
                        "DeltaBufferInsertion"
                    );
                    guard.insert_block((*message).clone())?;
                    if let Some(height) = message.db_committed_block_height {
                        guard.drain_blocks_until(height + 1)?;
                    }
                }
            }
            _ => return Err(PendingDeltasError::UnknownExtractor(message.extractor.clone())),
        }
        Ok(())
    }

    fn update_native_state(
        &self,
        db_state: &mut ProtocolComponentState,
        version: Option<BlockNumberOrTimestamp>,
        protocol_system: &str,
    ) -> Result<bool> {
        let mut change_found = false;

        let buffer = self
            .buffers
            .get(protocol_system)
            .ok_or_else(|| {
                error!("Missing reorg buffer for {}", protocol_system);
                PendingDeltasError::UnknownExtractor(protocol_system.to_string())
            })?;

        let guard = buffer.lock().map_err(|e| {
            PendingDeltasError::LockError(protocol_system.to_string(), e.to_string())
        })?;

        for entry in guard.get_block_range(None, version)? {
            // Apply state deltas if found
            if let Some(delta) = entry
                .state_deltas
                .get(&db_state.component_id)
            {
                db_state.apply_state_delta(delta)?;
                change_found = true;
            }

            // Apply balance deltas if found
            if let Some(delta) = entry
                .component_balances
                .get(&db_state.component_id)
            {
                db_state.apply_balance_delta(delta)?;
                change_found = true;
            }
        }

        Ok(change_found)
    }

    // Updates a given db account state with the buffered deltas.
    fn update_vm_state(
        &self,
        db_state: &mut Account,
        version: Option<BlockNumberOrTimestamp>,
        protocol_system: &str,
    ) -> Result<bool> {
        let mut change_found = false;

        let buffer = self
            .buffers
            .get(protocol_system)
            .ok_or_else(|| {
                error!("Missing reorg buffer for {}", protocol_system);
                PendingDeltasError::UnknownExtractor(protocol_system.to_string())
            })?;

        let guard = buffer.lock().map_err(|e| {
            PendingDeltasError::LockError(protocol_system.to_string(), e.to_string())
        })?;

        for entry in guard.get_block_range(None, version)? {
            if let Some(delta) = entry
                .account_deltas
                .get(&db_state.address)
            {
                db_state.apply_delta(delta)?;
                change_found = true
            }

            // TODO: currently it is impossible to apply balance changes and state deltas since
            //  we don't know the component_id of the contract.
        }

        Ok(change_found)
    }

    // Creates a new account state from the buffered deltas only.
    fn get_account(
        &self,
        address: Bytes,
        version: Option<BlockNumberOrTimestamp>,
    ) -> Result<Account> {
        let mut account: Option<Account> = None;
        for buffer in self.buffers.values() {
            let guard = buffer
                .lock()
                .map_err(|e| PendingDeltasError::LockError("VM".to_string(), e.to_string()))?;
            for entry in guard.get_block_range(None, version)? {
                if let Some(delta) = entry.account_deltas.get(&address) {
                    // Update account state or create a new one if not present
                    let account_ref =
                        account.get_or_insert_with(|| delta.clone().into_account_without_tx());
                    account_ref.apply_delta(delta)?;
                }
            }
        }

        account.ok_or(PendingDeltasError::ReorgBufferError(StorageError::NotFound(
            "Contract".to_string(),
            address.to_string(),
        )))
    }

    /// Runs the PendingDeltas message loop.
    ///
    /// Accepts pre-built receivers from the supervisor (one per extractor) and a reset
    /// channel. When a reset signal arrives for an extractor, its buffer is cleared so that
    /// restarted extractors can replay blocks without hitting parent-hash mismatches.
    pub async fn run(
        self,
        receivers: Vec<tokio::sync::mpsc::Receiver<crate::extractor::ExtractorMsg>>,
        mut reset_rx: tokio::sync::mpsc::Receiver<String>,
        start_tx: SyncSender<()>,
    ) -> anyhow::Result<()> {
        let rxs: Vec<ReceiverStream<_>> = receivers
            .into_iter()
            .map(ReceiverStream::new)
            .collect();

        // Send the start signal to the startup task
        start_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("Failed to send PendingDeltas start signal"))?;

        // Start a reporting task to log buffer sizes every 60 seconds
        let self_clone = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                gauge!("pending_deltas_buffer_size").set(self_clone.deep_size_of() as f64);
            }
        });

        let mut all_messages = stream::select_all(rxs);

        loop {
            tokio::select! {
                Some(message) = all_messages.next() => {
                    // Skip partial messages - only full-block updates go to the reorg buffer.
                    if message.partial_block_index.is_none() {
                            self.insert(message).map_err(|e| {
                                error!(error = %e, "Failed to insert into PendingDeltas buffer");
                                e
                            })?;
                        }
                }
                Some(extractor_name) = reset_rx.recv() => {
                    debug!(extractor = %extractor_name, "Resetting PendingDeltas buffer for extractor");
                    if let Some(buffer) = self.buffers.get(&extractor_name) {
                        match buffer.lock() {
                            Ok(mut guard) => {
                                *guard = ReorgBuffer::new();
                                debug!(extractor = %extractor_name, "PendingDeltas buffer reset");
                            }
                            Err(err) => {
                                error!(
                                    extractor = %extractor_name,
                                    error = %err,
                                    "Failed to lock buffer for reset"
                                );
                                return Err(PendingDeltasError::LockError(extractor_name.to_string(), err.to_string()).into());
                            }
                        }
                    } else {
                        warn!(extractor = %extractor_name, "No buffer found for reset — extractor unknown");
                    }
                }
                else => {
                    info!("All PendingDeltas streams ended");
                    break;
                }
            }
        }

        Ok(())
    }
}

impl PendingDeltasBuffer for PendingDeltas {
    /// Merges the buffered deltas with given db states. If a requested component is not in the
    /// db yet, it creates a new state for it out of the buffered deltas.
    ///
    /// Arguments:
    ///
    /// * `protocol_ids`: A list of the requested protocol ids. Note: `None` is not supported yet.
    /// * `db_states`: A mutable reference to the states fetched from the db.
    /// * `version`: The version of the state to be fetched. If `None`, the latest state will be
    ///   fetched.
    #[instrument(level = Level::TRACE, skip_all)]
    fn merge_native_states(
        &self,
        protocol_ids: Option<&[&str]>,
        db_states: &mut Vec<ProtocolComponentState>,
        version: Option<BlockNumberOrTimestamp>,
        protocol_system: &str,
    ) -> Result<()> {
        // TODO: handle when no id is specified with filters

        let mut missing_ids: HashSet<&str> = protocol_ids
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();

        // update db states with buffered deltas
        for state in db_states.iter_mut() {
            self.update_native_state(state, version, protocol_system)?;
            missing_ids.remove(state.component_id.as_str());
        }

        // for new components (not in the db yet), create an empty state and apply buffered deltas
        // to it
        for id in missing_ids {
            let mut state = ProtocolComponentState::new(id, HashMap::new(), HashMap::new());
            self.update_native_state(&mut state, version, protocol_system)?;
            db_states.push(state);
        }

        Ok(())
    }

    /// Updates the given db states with the buffered deltas. If a requested account is not in the
    /// db yet, it creates a new state for it out of the buffered deltas.
    ///
    /// Arguments:
    ///
    /// * `addresses`: A list of the requested account addresses.
    /// * `db_states`: A mutable reference to the states fetched from the db.
    /// * `version`: The version of the state to be fetched. If `None`, the latest state will be
    ///   fetched.
    #[instrument(level = Level::TRACE, skip_all)]
    fn update_vm_states(
        &self,
        addresses: Option<&[Bytes]>,
        db_states: &mut Vec<Account>,
        version: Option<BlockNumberOrTimestamp>,
        protocol_system: &str,
    ) -> Result<()> {
        let mut missing_addresses: HashSet<Bytes> = addresses
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();

        // update db states with buffered deltas
        for state in db_states.iter_mut() {
            self.update_vm_state(state, version, protocol_system)?;
            missing_addresses.remove(&state.address);
        }

        // for new accounts (not in the db yet), build a new state from the buffered deltas
        // and add it to the db states
        for address in missing_addresses {
            let account = self.get_account(address, version)?;
            db_states.push(account);
        }

        Ok(())
    }

    /// Retrieves a list of new protocol components that match all the provided criteria.
    /// (The filters are combined using an AND logic.)
    ///
    /// # Parameters
    /// - `ids`: `Option<&[&str]>`
    ///     - A list of component IDs to filter the results. If `None`, all components will be
    ///       considered.
    /// - `protocol_system`: `Option<&str>`
    ///     - The protocol system to filter the components by. If `None`, components from all
    ///       protocol systems will be considered.
    ///
    /// # Returns
    /// - `Result<Vec<ProtocolComponent>>`
    ///     - A `Result` which is `Ok` containing a vector of `ProtocolComponent` objects if
    ///       successful, or an error otherwise.
    ///
    /// # Example
    /// ```
    /// let components = get_new_components(Some(&["id1", "id2"]), Some("system1"))?;
    /// ```
    #[instrument(level = Level::TRACE, skip_all)]
    fn get_new_components(
        &self,
        ids: Option<&[&str]>,
        protocol_system: &str,
        min_tvl: Option<f64>,
    ) -> Result<Vec<ProtocolComponent>> {
        let requested_ids: Option<HashSet<&str>> = ids.map(|ids| ids.iter().cloned().collect());
        let mut new_components = Vec::new();

        let buffer = self
            .buffers
            .get(protocol_system)
            .ok_or_else(|| {
                error!("Missing reorg buffer for {}", protocol_system);
                PendingDeltasError::UnknownExtractor(protocol_system.to_string())
            })?;

        let guard = buffer.lock().map_err(|e| {
            PendingDeltasError::LockError(protocol_system.to_string(), e.to_string())
        })?;

        for entry in guard.get_block_range(None, None)? {
            let components_tvls = &entry.component_tvl;

            new_components.extend(
                entry
                    .new_protocol_components
                    .values()
                    .filter(|comp| {
                        let id_matches = requested_ids
                            .as_ref()
                            .is_none_or(|ids| ids.contains(comp.id.as_str()));

                        let tvl_matches = min_tvl.as_ref().is_none_or(|tvl| {
                            components_tvls
                                .get(&comp.id)
                                .unwrap_or(&0.0) >=
                                tvl
                        });

                        id_matches && tvl_matches
                    })
                    .cloned(),
            );
        }

        Ok(new_components)
    }

    /// Returns commit status for any extractor, can error if lock is poisened. Returns None
    /// if buffer is empty.
    /// Returns an error if the provided protocol system isn't found in the buffer or the specified
    /// block version is unseen. If a timestamp version is provided, the latest block before that
    /// timestamp is used.
    #[instrument(level = Level::TRACE, skip_all)]
    fn get_block_commit_status(
        &self,
        version: BlockNumberOrTimestamp,
        protocol_system: &str,
    ) -> Result<Option<CommitStatus>> {
        let buffer = self
            .buffers
            .get(protocol_system)
            .ok_or_else(|| {
                error!("Missing reorg buffer for {}", protocol_system);
                PendingDeltasError::UnknownExtractor(protocol_system.to_string())
            })?;
        let guard = buffer.lock().map_err(|e| {
            PendingDeltasError::LockError(protocol_system.to_string(), e.to_string())
        })?;

        Ok(guard.get_commit_status(version))
    }

    fn search_block(
        &self,
        f: &dyn Fn(&BlockAggregatedChanges) -> bool,
        protocol_system: &str,
    ) -> Result<Option<BlockAggregatedChanges>> {
        let buffer = self
            .buffers
            .get(protocol_system)
            .ok_or_else(|| {
                error!("Missing reorg buffer for {}", protocol_system);
                PendingDeltasError::UnknownExtractor(protocol_system.to_string())
            })?;
        let guard = buffer.lock().map_err(|e| {
            PendingDeltasError::LockError(protocol_system.to_string(), e.to_string())
        })?;

        for block in guard.get_block_range(None, None)? {
            if f(block) {
                return Ok(Some(block.clone()));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_common::models::{
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Chain, ChangeType,
    };

    use super::*;
    use crate::{extractor::models::fixtures, testing::block};

    fn vm_state() -> Account {
        Account::new(
            Chain::Ethereum,
            Bytes::from("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05"),
            "Contract1".to_string(),
            fixtures::slots([(2, 2)]),
            Bytes::from("0x1999"),
            HashMap::new(),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0xbabe"),
            Bytes::from("0x4200"),
            Bytes::from("0x4200"),
            None,
        )
    }

    fn vm_block_deltas() -> BlockAggregatedChanges {
        let address = Bytes::from_str("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05").unwrap();
        BlockAggregatedChanges {
            extractor: "vm:extractor".to_string(),
            block: block(1),
            db_committed_block_height: None,
            finalized_block_height: 1,
            revert: false,
            account_deltas: HashMap::from([
                (
                    address.clone(),
                    AccountDelta::new(
                        Chain::Ethereum,
                        address.clone(),
                        fixtures::optional_slots([(1, 1), (2, 1)]),
                        Some(Bytes::from(1999u32).lpad(32, 0)),
                        None,
                        ChangeType::Update,
                    ),
                ),
                (
                    Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                    AccountDelta::new(
                        Chain::Ethereum,
                        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        fixtures::optional_slots([(1, 1), (2, 1)]),
                        Some(Bytes::from(200u32).lpad(32, 0)),
                        Some(Bytes::from("0x0c0c0c")),
                        ChangeType::Creation,
                    ),
                ),
            ]),
            new_protocol_components: HashMap::from([
                (
                    "component2".to_string(),
                    ProtocolComponent {
                        id: "component2".to_string(),
                        protocol_system: "vm_swap".to_string(),
                        protocol_type_name: "swap".to_string(),
                        chain: Chain::Ethereum,
                        tokens: Vec::new(),
                        contract_addresses: Vec::new(),
                        static_attributes: HashMap::new(),
                        change: ChangeType::Creation,
                        creation_tx: Bytes::new(),
                        created_at: "2020-01-01T00:00:00".parse().unwrap(),
                    },
                ),
                (
                    "component5".to_string(),
                    ProtocolComponent {
                        id: "component5".to_string(),
                        protocol_system: "vm_swap".to_string(),
                        protocol_type_name: "swap".to_string(),
                        chain: Chain::Ethereum,
                        tokens: Vec::new(),
                        contract_addresses: Vec::new(),
                        static_attributes: HashMap::new(),
                        change: ChangeType::Creation,
                        creation_tx: Bytes::new(),
                        created_at: "2020-01-01T00:00:00".parse().unwrap(),
                    },
                ),
                (
                    "component4".to_string(),
                    ProtocolComponent {
                        id: "component4".to_string(),
                        protocol_system: "vm_swap".to_string(),
                        protocol_type_name: "swap".to_string(),
                        chain: Chain::Ethereum,
                        tokens: Vec::new(),
                        contract_addresses: Vec::new(),
                        static_attributes: HashMap::new(),
                        change: ChangeType::Creation,
                        creation_tx: Bytes::new(),
                        created_at: "2020-01-01T00:00:00".parse().unwrap(),
                    },
                ),
            ]),
            account_balances: HashMap::from([
                (
                    address.clone(),
                    [(
                        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        AccountBalance {
                            token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
                                .unwrap(),
                            balance: Bytes::from("0x01"),
                            modify_tx: Bytes::zero(32),
                            account: address.clone(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                (
                    address.clone(),
                    [(
                        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        AccountBalance {
                            token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
                                .unwrap(),
                            balance: Bytes::from("0x02"),
                            modify_tx: Bytes::zero(32),
                            account: address,
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
            ]),
            component_tvl: HashMap::from([
                ("component2".to_string(), 1.5),
                ("component3".to_string(), 0.5),
            ]),
            ..Default::default()
        }
    }

    fn native_state() -> ProtocolComponentState {
        ProtocolComponentState::new(
            "component1",
            [("attr2", Bytes::from("0x02")), ("attr1", Bytes::from("0x00"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x02"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        )
    }

    fn native_block_deltas() -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            extractor: "native:extractor".to_string(),
            block: block(1),
            db_committed_block_height: None,
            finalized_block_height: 1,
            revert: false,
            state_deltas: HashMap::from([
                (
                    "component1".to_string(),
                    ProtocolComponentStateDelta::new(
                        "component1",
                        [("attr1", Bytes::from("0x01"))]
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect(),
                        HashSet::new(),
                    ),
                ),
                (
                    "component3".to_string(),
                    ProtocolComponentStateDelta::new(
                        "component3",
                        [("attr2", Bytes::from("0x05"))]
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v))
                            .collect(),
                        HashSet::new(),
                    ),
                ),
            ]),
            new_protocol_components: HashMap::from([
                (
                    "component3".to_string(),
                    ProtocolComponent {
                        id: "component3".to_string(),
                        protocol_system: "native_swap".to_string(),
                        protocol_type_name: "swap".to_string(),
                        chain: Chain::Ethereum,
                        tokens: Vec::new(),
                        contract_addresses: Vec::new(),
                        static_attributes: HashMap::new(),
                        change: ChangeType::Creation,
                        creation_tx: Bytes::new(),
                        created_at: "2020-01-01T00:00:00".parse().unwrap(),
                    },
                ),
                (
                    "component4".to_string(),
                    ProtocolComponent {
                        id: "component4".to_string(),
                        protocol_system: "native_swap".to_string(),
                        protocol_type_name: "swap".to_string(),
                        chain: Chain::Ethereum,
                        tokens: Vec::new(),
                        contract_addresses: Vec::new(),
                        static_attributes: HashMap::new(),
                        change: ChangeType::Creation,
                        creation_tx: Bytes::new(),
                        created_at: "2020-01-01T00:00:00".parse().unwrap(),
                    },
                ),
            ]),
            component_balances: HashMap::from([
                (
                    "component1".to_string(),
                    [(
                        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        ComponentBalance {
                            token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
                                .unwrap(),
                            balance_float: 1.0,
                            balance: Bytes::from("0x01"),
                            modify_tx: Bytes::zero(32),
                            component_id: "component1".to_string(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                (
                    "component3".to_string(),
                    [(
                        Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                        ComponentBalance {
                            token: Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
                                .unwrap(),
                            balance_float: 2.0,
                            balance: Bytes::from("0x02"),
                            modify_tx: Bytes::zero(32),
                            component_id: "component3".to_string(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
            ]),
            ..Default::default()
        }
    }

    fn simple_block_changes(
        block_number: u64,
        db_committed_block_height: Option<u64>,
    ) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            extractor: "vm:extractor".to_string(),
            chain: Chain::Ethereum,
            block: block(block_number),
            finalized_block_height: block_number,
            db_committed_block_height,
            ..Default::default()
        }
    }

    #[test]
    fn test_insert_extractor() {
        let buffer = PendingDeltas::new(["vm:extractor"]);
        let exp: BlockAggregatedChanges = vm_block_deltas();

        buffer
            .insert(Arc::new(vm_block_deltas()))
            .expect("insert failed");

        let reorg_buffer = buffer
            .buffers
            .get("vm:extractor")
            .expect("extractor buffer missing");
        let binding = reorg_buffer.lock().unwrap();
        let res = binding
            .get_block_range(None, None)
            .expect("Failed to get block range")
            .collect::<Vec<_>>();
        assert_eq!(res[0], &exp);
    }

    #[test]
    fn test_merge_native_states() {
        let mut state = vec![native_state()]; // db state
        let buffer = PendingDeltas::new(["native:extractor"]);
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();
        // expected state after applying buffer deltas to the given db state
        let exp1 = ProtocolComponentState::new(
            "component1",
            [("attr2", Bytes::from("0x02")), ("attr1", Bytes::from("0x01"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x01"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );
        // expected state for a new component not yet in the db
        let exp3 = ProtocolComponentState::new(
            "component3",
            [("attr2", Bytes::from("0x05"))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
            [(Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), Bytes::from("0x02"))]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );

        buffer
            .merge_native_states(
                Some(&["component1", "component3"]),
                &mut state,
                Some(BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap())),
                "native:extractor",
            )
            .unwrap();

        assert_eq!(state.len(), 2);
        assert_eq!(&state[0], &exp1);
        assert_eq!(&state[1], &exp3);
    }

    #[test]
    fn test_update_vm_states() {
        let mut state = vec![vm_state()];
        let buffer = PendingDeltas::new(["vm:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .unwrap();
        let address0 = Bytes::from("0x6F4Feb566b0f29e2edC231aDF88Fe7e1169D7c05");
        let exp0 = Account::new(
            Chain::Ethereum,
            address0.clone(),
            "Contract1".to_string(),
            fixtures::slots([(1, 1), (2, 1)]),
            Bytes::from("0x00000000000000000000000000000000000000000000000000000000000007cf"),
            HashMap::new(),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0xbabe"),
            Bytes::from("0x4200"),
            Bytes::from("0x4200"),
            None,
        );
        let address1 = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let exp1 = Account::new(
            Chain::Ethereum,
            address1.clone(),
            address1.clone().to_string(),
            fixtures::slots([(1, 1), (2, 1)]),
            Bytes::from("0x00000000000000000000000000000000000000000000000000000000000000c8"),
            HashMap::new(),
            Bytes::from("0x0c0c0c"),
            Bytes::from("0x58ca1e123f83094287ae82a842f4f49e064d6f2fa946a2130335ff131ebd010b"),
            Bytes::from("0x00"),
            Bytes::from("0x00"),
            None,
        );

        buffer
            .update_vm_states(
                Some(&[address0, address1]),
                &mut state,
                Some(BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap())),
                "vm:extractor",
            )
            .unwrap();

        assert_eq!(&state[0], &exp0);
        assert_eq!(&state[1], &exp1);
    }

    #[test]
    fn test_get_new_components() {
        let exp = [
            ProtocolComponent::new(
                "component3",
                "native_swap",
                "swap",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::new(),
                "2020-01-01T00:00:00".parse().unwrap(),
            ),
            ProtocolComponent::new(
                "component2",
                "vm_swap",
                "swap",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::new(),
                "2020-01-01T00:00:00".parse().unwrap(),
            ),
            ProtocolComponent::new(
                "component5",
                "vm_swap",
                "swap",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::new(),
                "2020-01-01T00:00:00".parse().unwrap(),
            ),
            ProtocolComponent::new(
                "component4",
                "vm_swap",
                "swap",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::new(),
                "2020-01-01T00:00:00".parse().unwrap(),
            ),
        ];
        let buffer = PendingDeltas::new(["vm:extractor", "native:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .unwrap();
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();

        let new_components = buffer
            .get_new_components(Some(&["component3"]), "native:extractor", None)
            .unwrap();

        assert_eq!(new_components, vec![exp[0].clone()]);

        let mut new_components = buffer
            .get_new_components(None, "vm:extractor", None)
            .unwrap();

        new_components.sort_by_key(|comp| comp.id.clone());
        assert_eq!(new_components, vec![exp[1].clone(), exp[3].clone(), exp[2].clone()]);

        let new_components_tvl_filtered = buffer
            .get_new_components(None, "vm:extractor", Some(1.0))
            .unwrap();

        assert_eq!(new_components_tvl_filtered, vec![exp[1].clone()]);
    }

    #[test]
    fn test_insert_respects_db_committed_height() {
        let buffer = PendingDeltas::new(["vm:extractor"]);

        let exp1 = simple_block_changes(1, None);
        let exp2 = simple_block_changes(2, None);
        let exp3 = simple_block_changes(3, Some(2));

        buffer
            .insert(Arc::new(exp1.clone()))
            .expect("first insert failed");
        buffer
            .insert(Arc::new(exp2.clone()))
            .expect("second insert failed");

        {
            let reorg_buffer = buffer
                .buffers
                .get("vm:extractor")
                .expect("extractor buffer missing");
            let guard = reorg_buffer
                .lock()
                .expect("lock poisoned");
            let block_numbers: Vec<&BlockAggregatedChanges> = guard
                .get_block_range(None, None)
                .expect("Failed to get block range")
                .collect();
            assert_eq!(
                block_numbers,
                vec![&exp1, &exp2],
                "blocks should remain when commit height is None"
            );
        }

        buffer
            .insert(Arc::new(exp3.clone()))
            .expect("third insert failed");

        let reorg_buffer = buffer
            .buffers
            .get("vm:extractor")
            .expect("extractor buffer missing");
        let guard = reorg_buffer
            .lock()
            .expect("lock poisoned");
        let block_numbers: Vec<&BlockAggregatedChanges> = guard
            .get_block_range(None, None)
            .expect("Failed to get block range")
            .collect();

        assert_eq!(block_numbers, vec![&exp3], "blocks <= commit height should be drained");
    }

    use rstest::rstest;

    #[rstest]
    // native extractor
    #[case("native:extractor".to_string(), None)]
    // vm extractor
    #[case("vm:extractor".to_string(), None)]
    // bad input
    #[case("unknown_system".to_string(), Some(PendingDeltasError::UnknownExtractor("unknown_system".to_string())))]
    fn test_get_block_commit_status(
        #[case] protocol_system: String,
        #[case] expected_error: Option<PendingDeltasError>,
    ) {
        let buffer = PendingDeltas::new(["vm:extractor", "native:extractor"]);
        buffer
            .insert(Arc::new(vm_block_deltas()))
            .expect("vm insert failed");
        buffer
            .insert(Arc::new(native_block_deltas()))
            .expect("native insert failed");

        let version = BlockNumberOrTimestamp::Timestamp("2020-01-01T00:00:00".parse().unwrap());

        let result = buffer.get_block_commit_status(version, &protocol_system);

        match expected_error {
            Some(expected_err) => {
                assert!(matches!(result, Err(ref err) if err == &expected_err));
            }
            None => {
                let commit_status = result.expect("Failed to get block commit status");
                assert!(commit_status.is_some());
            }
        }
    }

    #[rstest]
    // cached block
    #[case(Bytes::from(1u8).lpad(32, 0), Some(native_block_deltas()))]
    // missing block
    #[case(Bytes::from(9u8).lpad(32, 0), None)]
    fn test_search_block(
        #[case] block_hash: Bytes,
        #[case] expected_res: Option<BlockAggregatedChanges>,
    ) {
        let buffer = PendingDeltas::new(["native:extractor"]);
        buffer
            .insert(Arc::new(native_block_deltas()))
            .unwrap();

        let res = buffer
            .search_block(&|b| b.block.hash == block_hash, "native:extractor")
            .unwrap();

        assert_eq!(res, expected_res);
    }
}
