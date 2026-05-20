use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};

use chrono::NaiveDateTime;
use deepsize::DeepSizeOf;
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use tracing::warn;

use crate::{
    dto,
    models::{
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        token::Token,
        Address, Balance, BlockHash, Chain, Code, ComponentId, EntryPointId, MergeError, StoreKey,
        StoreVal,
    },
    Bytes,
};

#[derive(Clone, Default, PartialEq, Serialize, Deserialize, Debug)]
pub struct Block {
    pub number: u64,
    pub chain: Chain,
    pub hash: Bytes,
    pub parent_hash: Bytes,
    pub ts: NaiveDateTime,
}

impl Block {
    pub fn new(
        number: u64,
        chain: Chain,
        hash: Bytes,
        parent_hash: Bytes,
        ts: NaiveDateTime,
    ) -> Self {
        Block { hash, parent_hash, number, chain, ts }
    }
}

// Manual impl as `NaiveDateTime` structure referenced in `ts` does not implement DeepSizeOf
impl DeepSizeOf for Block {
    fn deep_size_of_children(&self, context: &mut deepsize::Context) -> usize {
        self.chain
            .deep_size_of_children(context) +
            self.hash.deep_size_of_children(context) +
            self.parent_hash
                .deep_size_of_children(context)
    }
}

#[derive(Clone, Default, PartialEq, Debug, Eq, Hash, DeepSizeOf)]
pub struct Transaction {
    pub hash: Bytes,
    pub block_hash: Bytes,
    pub from: Bytes,
    pub to: Option<Bytes>,
    pub index: u64,
}

impl Transaction {
    pub fn new(hash: Bytes, block_hash: Bytes, from: Bytes, to: Option<Bytes>, index: u64) -> Self {
        Transaction { hash, block_hash, from, to, index }
    }
}

/// Raw EVM log emitted by a contract during transaction execution.
///
/// Used as the primary input to [`TxDeltaIndexer`] implementations.
///
/// [`TxDeltaIndexer`]: crate::traits::TxDeltaIndexer
#[derive(Debug, Clone)]
pub struct LogInput {
    address: Bytes,
    topics: Vec<Bytes>,
    data: Bytes,
    log_index: u32,
}

impl LogInput {
    pub fn new(address: Bytes, topics: Vec<Bytes>, data: Bytes, log_index: u32) -> Self {
        Self { address, topics, data, log_index }
    }

    pub fn address(&self) -> &Bytes {
        &self.address
    }

    pub fn topics(&self) -> &[Bytes] {
        &self.topics
    }

    pub fn data(&self) -> &Bytes {
        &self.data
    }

    pub fn log_index(&self) -> u32 {
        self.log_index
    }
}

/// Raw EVM transaction with its associated logs.
///
/// The `succeeded` flag allows callers to pass all transactions in a block and
/// have the processor skip reverted ones, avoiding a separate pre-filter.
#[derive(Debug, Clone)]
pub struct TxInput {
    hash: Bytes,
    from: Bytes,
    to: Bytes,
    index: u64,
    logs: Vec<LogInput>,
    succeeded: bool,
}

impl TxInput {
    pub fn new(
        hash: Bytes,
        from: Bytes,
        to: Bytes,
        index: u64,
        logs: Vec<LogInput>,
        succeeded: bool,
    ) -> Self {
        Self { hash, from, to, index, logs, succeeded }
    }

    pub fn hash(&self) -> &Bytes {
        &self.hash
    }

    pub fn from(&self) -> &Bytes {
        &self.from
    }

    pub fn to(&self) -> &Bytes {
        &self.to
    }

    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn logs(&self) -> &[LogInput] {
        &self.logs
    }

    pub fn succeeded(&self) -> bool {
        self.succeeded
    }
}

pub struct BlockTransactionDeltas<T> {
    pub extractor: String,
    pub chain: Chain,
    pub block: Block,
    pub revert: bool,
    pub deltas: Vec<TransactionDeltaGroup<T>>,
}

#[allow(dead_code)]
pub struct TransactionDeltaGroup<T> {
    changes: T,
    protocol_component: HashMap<String, ProtocolComponent>,
    component_balances: HashMap<String, ComponentBalance>,
    component_tvl: HashMap<String, f64>,
    tx: Transaction,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, DeepSizeOf)]
pub struct BlockAggregatedChanges {
    pub extractor: String,
    pub chain: Chain,
    pub block: Block,
    pub finalized_block_height: u64,
    pub db_committed_block_height: Option<u64>,
    pub revert: bool,
    pub state_deltas: HashMap<String, ProtocolComponentStateDelta>,
    pub account_deltas: HashMap<Bytes, AccountDelta>,
    pub new_tokens: HashMap<Address, Token>,
    pub new_protocol_components: HashMap<String, ProtocolComponent>,
    pub deleted_protocol_components: HashMap<String, ProtocolComponent>,
    pub component_balances: HashMap<ComponentId, HashMap<Bytes, ComponentBalance>>,
    pub account_balances: HashMap<Address, HashMap<Address, AccountBalance>>,
    pub component_tvl: HashMap<String, f64>,
    pub dci_update: DCIUpdate,
    /// The index of the partial block. None if it's a full block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_block_index: Option<u32>,
}

impl BlockAggregatedChanges {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        extractor: &str,
        chain: Chain,
        block: Block,
        db_committed_block_height: Option<u64>,
        finalized_block_height: u64,
        revert: bool,
        state_deltas: HashMap<String, ProtocolComponentStateDelta>,
        account_deltas: HashMap<Bytes, AccountDelta>,
        new_tokens: HashMap<Address, Token>,
        new_components: HashMap<String, ProtocolComponent>,
        deleted_components: HashMap<String, ProtocolComponent>,
        component_balances: HashMap<ComponentId, HashMap<Bytes, ComponentBalance>>,
        account_balances: HashMap<Address, HashMap<Address, AccountBalance>>,
        component_tvl: HashMap<String, f64>,
        dci_update: DCIUpdate,
    ) -> Self {
        Self {
            extractor: extractor.to_string(),
            chain,
            block,
            db_committed_block_height,
            finalized_block_height,
            revert,
            state_deltas,
            account_deltas,
            new_tokens,
            new_protocol_components: new_components,
            deleted_protocol_components: deleted_components,
            component_balances,
            account_balances,
            component_tvl,
            dci_update,
            partial_block_index: None,
        }
    }

    pub fn drop_state(&self) -> Self {
        Self {
            extractor: self.extractor.clone(),
            chain: self.chain,
            block: self.block.clone(),
            db_committed_block_height: self.db_committed_block_height,
            finalized_block_height: self.finalized_block_height,
            revert: self.revert,
            account_deltas: HashMap::new(),
            state_deltas: HashMap::new(),
            new_tokens: self.new_tokens.clone(),
            new_protocol_components: self.new_protocol_components.clone(),
            deleted_protocol_components: self.deleted_protocol_components.clone(),
            component_balances: self.component_balances.clone(),
            account_balances: self.account_balances.clone(),
            component_tvl: self.component_tvl.clone(),
            dci_update: self.dci_update.clone(),
            partial_block_index: self.partial_block_index,
        }
    }

    pub fn is_partial(&self) -> bool {
        self.partial_block_index.is_some()
    }

    pub fn get_block(&self) -> &Block {
        &self.block
    }

    pub fn n_changes(&self) -> usize {
        self.account_deltas.len() + self.state_deltas.len()
    }

    pub fn filter_by_component<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.state_deltas.retain(|k, _| keep(k));
        self.component_balances
            .retain(|k, _| keep(k));
        self.component_tvl
            .retain(|k, _| keep(k));
    }

    pub fn filter_by_contract<F: Fn(&Bytes) -> bool>(&mut self, keep: F) {
        self.account_deltas
            .retain(|k, _| keep(k));
        self.account_balances
            .retain(|k, _| keep(k));
    }

    /// Merges this update with another one, consuming both.
    ///
    /// `other` is assumed to be a more recent update than `self`.
    pub fn merge(mut self, other: Self) -> Self {
        for (k, v) in other.account_deltas {
            match self.account_deltas.entry(k) {
                Entry::Occupied(mut e) => {
                    // best-effort: ignore merge errors (address mismatch is a bug)
                    let _ = e.get_mut().merge(v);
                }
                Entry::Vacant(e) => {
                    e.insert(v);
                }
            }
        }

        for (k, v) in other.state_deltas {
            match self.state_deltas.entry(k) {
                Entry::Occupied(mut e) => {
                    let _ = e.get_mut().merge(v);
                }
                Entry::Vacant(e) => {
                    e.insert(v);
                }
            }
        }

        for (component_id, balances) in other.component_balances {
            self.component_balances
                .entry(component_id)
                .or_default()
                .extend(balances);
        }

        for (account, balances) in other.account_balances {
            self.account_balances
                .entry(account)
                .or_default()
                .extend(balances);
        }

        self.component_tvl
            .extend(other.component_tvl);
        self.new_protocol_components
            .extend(other.new_protocol_components);
        self.deleted_protocol_components
            .extend(other.deleted_protocol_components);
        self.revert = other.revert;
        self.block = other.block;
        self
    }
}

impl std::fmt::Display for BlockAggregatedChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "block_number: {}, extractor: {}", self.block.number, self.extractor)
    }
}

pub trait BlockScoped {
    fn block(&self) -> Block;
}

impl BlockScoped for BlockAggregatedChanges {
    fn block(&self) -> Block {
        self.block.clone()
    }
}

impl From<dto::Block> for Block {
    fn from(value: dto::Block) -> Self {
        Self {
            number: value.number,
            chain: value.chain.into(),
            hash: value.hash,
            parent_hash: value.parent_hash,
            ts: value.ts,
        }
    }
}

impl From<dto::AddressStorageLocation> for AddressStorageLocation {
    fn from(value: dto::AddressStorageLocation) -> Self {
        Self { key: value.key, offset: value.offset }
    }
}

impl From<dto::TracingResult> for TracingResult {
    fn from(value: dto::TracingResult) -> Self {
        Self {
            retriggers: value
                .retriggers
                .into_iter()
                .map(|(addr, loc)| (addr, loc.into()))
                .collect(),
            accessed_slots: value.accessed_slots,
        }
    }
}

impl From<dto::DCIUpdate> for DCIUpdate {
    fn from(value: dto::DCIUpdate) -> Self {
        Self {
            new_entrypoints: value
                .new_entrypoints
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.into_iter()
                            .map(EntryPoint::from)
                            .collect(),
                    )
                })
                .collect(),
            new_entrypoint_params: value
                .new_entrypoint_params
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.into_iter()
                            .map(|(p, c)| (TracingParams::from(p), c))
                            .collect(),
                    )
                })
                .collect(),
            trace_results: value
                .trace_results
                .into_iter()
                .map(|(k, v)| (k, TracingResult::from(v)))
                .collect(),
        }
    }
}

impl From<dto::BlockAggregatedChanges> for BlockAggregatedChanges {
    fn from(value: dto::BlockAggregatedChanges) -> Self {
        use crate::models::{
            contract::{AccountBalance, AccountDelta},
            protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
            token::Token,
        };
        Self {
            extractor: value.extractor,
            chain: value.chain.into(),
            block: value.block.into(),
            finalized_block_height: value.finalized_block_height,
            db_committed_block_height: None,
            revert: value.revert,
            state_deltas: value
                .state_updates
                .into_iter()
                .map(|(k, v)| (k, ProtocolComponentStateDelta::from(v)))
                .collect(),
            account_deltas: value
                .account_updates
                .into_iter()
                .map(|(k, v)| (k, AccountDelta::from(v)))
                .collect(),
            new_tokens: value
                .new_tokens
                .into_iter()
                .map(|(k, v)| (k, Token::from(v)))
                .collect(),
            new_protocol_components: value
                .new_protocol_components
                .into_iter()
                .map(|(k, v)| (k, ProtocolComponent::from(v)))
                .collect(),
            deleted_protocol_components: value
                .deleted_protocol_components
                .into_iter()
                .map(|(k, v)| (k, ProtocolComponent::from(v)))
                .collect(),
            component_balances: value
                .component_balances
                .into_iter()
                .map(|(component_id, token_balances)| {
                    (
                        component_id,
                        token_balances
                            .0
                            .into_iter()
                            .map(|(k, v)| (k, ComponentBalance::from(v)))
                            .collect(),
                    )
                })
                .collect(),
            account_balances: value
                .account_balances
                .into_iter()
                .map(|(account, balances)| {
                    (
                        account,
                        balances
                            .into_iter()
                            .map(|(k, v)| (k, AccountBalance::from(v)))
                            .collect(),
                    )
                })
                .collect(),
            component_tvl: value.component_tvl,
            dci_update: DCIUpdate::from(value.dci_update),
            partial_block_index: value.partial_block_index,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, DeepSizeOf)]
pub struct DCIUpdate {
    pub new_entrypoints: HashMap<ComponentId, HashSet<EntryPoint>>,
    pub new_entrypoint_params: HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
    pub trace_results: HashMap<EntryPointId, TracingResult>,
}

/// Traced entry points for a set of components, as returned by the Tycho RPC.
///
/// Maps each component ID to the list of `(EntryPointWithTracingParams, TracingResult)` pairs
/// produced by the tracer for that component.
pub type TracedEntryPoints =
    HashMap<ComponentId, Vec<(EntryPointWithTracingParams, TracingResult)>>;

impl From<TracedEntryPoints> for DCIUpdate {
    fn from(traced_entry_points: TracedEntryPoints) -> Self {
        let mut new_entrypoints: HashMap<ComponentId, HashSet<EntryPoint>> = HashMap::new();
        let mut new_entrypoint_params: HashMap<
            EntryPointId,
            HashSet<(TracingParams, ComponentId)>,
        > = HashMap::new();
        let mut trace_results: HashMap<EntryPointId, TracingResult> = HashMap::new();

        for (component_id, traces) in traced_entry_points {
            let mut entrypoints = HashSet::new();

            for (ep_with_params, trace) in traces {
                let ep_id = ep_with_params
                    .entry_point
                    .external_id
                    .clone();

                entrypoints.insert(ep_with_params.entry_point.clone());

                new_entrypoint_params
                    .entry(ep_id.clone())
                    .or_default()
                    .insert((ep_with_params.params, component_id.clone()));

                trace_results
                    .entry(ep_id)
                    .and_modify(|existing: &mut TracingResult| {
                        existing
                            .retriggers
                            .extend(trace.retriggers.clone());
                        for (address, slots) in trace.accessed_slots.clone() {
                            existing
                                .accessed_slots
                                .entry(address)
                                .or_default()
                                .extend(slots);
                        }
                    })
                    .or_insert(trace);
            }

            if !entrypoints.is_empty() {
                new_entrypoints.insert(component_id, entrypoints);
            }
        }

        DCIUpdate { new_entrypoints, new_entrypoint_params, trace_results }
    }
}

/// Changes grouped by their respective transaction.
#[derive(Debug, Clone, PartialEq, Default, DeepSizeOf)]
pub struct TxWithChanges {
    pub tx: Transaction,
    pub protocol_components: HashMap<ComponentId, ProtocolComponent>,
    pub account_deltas: HashMap<Address, AccountDelta>,
    pub state_updates: HashMap<ComponentId, ProtocolComponentStateDelta>,
    pub balance_changes: HashMap<ComponentId, HashMap<Address, ComponentBalance>>,
    pub account_balance_changes: HashMap<Address, HashMap<Address, AccountBalance>>,
    pub entrypoints: HashMap<ComponentId, HashSet<EntryPoint>>,
    pub entrypoint_params: HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
}

impl TxWithChanges {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx: Transaction,
        protocol_components: HashMap<ComponentId, ProtocolComponent>,
        account_deltas: HashMap<Address, AccountDelta>,
        protocol_states: HashMap<ComponentId, ProtocolComponentStateDelta>,
        balance_changes: HashMap<ComponentId, HashMap<Address, ComponentBalance>>,
        account_balance_changes: HashMap<Address, HashMap<Address, AccountBalance>>,
        entrypoints: HashMap<ComponentId, HashSet<EntryPoint>>,
        entrypoint_params: HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
    ) -> Self {
        Self {
            tx,
            account_deltas,
            protocol_components,
            state_updates: protocol_states,
            balance_changes,
            account_balance_changes,
            entrypoints,
            entrypoint_params,
        }
    }

    /// Merges this update with another one.
    ///
    /// The method combines two [`TxWithChanges`] instances if they are on the same block.
    ///
    /// NB: It is expected that `other` is a more recent update than `self` is and the two are
    /// combined accordingly.
    ///
    /// # Errors
    /// Returns a `MergeError` if any of the above conditions are violated.
    pub fn merge(&mut self, other: TxWithChanges) -> Result<(), MergeError> {
        if self.tx.block_hash != other.tx.block_hash {
            return Err(MergeError::BlockMismatch(
                "TxWithChanges".to_string(),
                self.tx.block_hash.clone(),
                other.tx.block_hash,
            ));
        }
        if self.tx.index > other.tx.index {
            return Err(MergeError::TransactionOrderError(
                "TxWithChanges".to_string(),
                self.tx.index,
                other.tx.index,
            ));
        }

        self.tx = other.tx;

        // Merge new protocol components
        // Log a warning if a new protocol component for the same id already exists, because this
        // should never happen.
        for (key, value) in other.protocol_components {
            match self.protocol_components.entry(key) {
                Entry::Occupied(mut entry) => {
                    warn!(
                        "Overwriting new protocol component for id {} with a new one. This should never happen! Please check logic",
                        entry.get().id
                    );
                    entry.insert(value);
                }
                Entry::Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }

        // Merge account deltas
        for (address, update) in other.account_deltas.clone().into_iter() {
            match self.account_deltas.entry(address) {
                Entry::Occupied(mut e) => {
                    e.get_mut().merge(update)?;
                }
                Entry::Vacant(e) => {
                    e.insert(update);
                }
            }
        }

        // Merge protocol state updates
        for (key, value) in other.state_updates {
            match self.state_updates.entry(key) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().merge(value)?;
                }
                Entry::Vacant(entry) => {
                    entry.insert(value);
                }
            }
        }

        // Merge component balance changes
        for (component_id, balance_changes) in other.balance_changes {
            let token_balances = self
                .balance_changes
                .entry(component_id)
                .or_default();
            for (token, balance) in balance_changes {
                token_balances.insert(token, balance);
            }
        }

        // Merge account balance changes
        for (account_addr, balance_changes) in other.account_balance_changes {
            let token_balances = self
                .account_balance_changes
                .entry(account_addr)
                .or_default();
            for (token, balance) in balance_changes {
                token_balances.insert(token, balance);
            }
        }

        // Merge new entrypoints
        for (component_id, entrypoints) in other.entrypoints {
            self.entrypoints
                .entry(component_id)
                .or_default()
                .extend(entrypoints);
        }

        // Merge new entrypoint params
        for (entrypoint_id, params) in other.entrypoint_params {
            self.entrypoint_params
                .entry(entrypoint_id)
                .or_default()
                .extend(params);
        }

        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum BlockTag {
    /// Finalized block
    Finalized,
    /// Safe block
    Safe,
    /// Latest block
    Latest,
    /// Earliest block (genesis)
    Earliest,
    /// Pending block (not yet part of the blockchain)
    Pending,
    /// Block by number
    Number(u64),
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, DeepSizeOf)]
pub struct EntryPoint {
    /// Entry point id
    pub external_id: String,
    /// The address of the contract to trace.
    pub target: Address,
    /// The signature of the function to trace.
    pub signature: String,
}

impl EntryPoint {
    pub fn new(external_id: String, target: Address, signature: String) -> Self {
        Self { external_id, target, signature }
    }
}

impl From<dto::EntryPoint> for EntryPoint {
    fn from(value: dto::EntryPoint) -> Self {
        Self { external_id: value.external_id, target: value.target, signature: value.signature }
    }
}

/// A struct that combines an entry point with its associated tracing params.
#[derive(Debug, Clone, PartialEq, Eq, Hash, DeepSizeOf)]
pub struct EntryPointWithTracingParams {
    /// The entry point to trace, containing the target contract address and function signature
    pub entry_point: EntryPoint,
    /// The tracing parameters for this entry point
    pub params: TracingParams,
}

impl From<dto::EntryPointWithTracingParams> for EntryPointWithTracingParams {
    fn from(value: dto::EntryPointWithTracingParams) -> Self {
        match value.params {
            dto::TracingParams::RPCTracer(ref tracer_params) => Self {
                entry_point: EntryPoint {
                    external_id: value.entry_point.external_id,
                    target: value.entry_point.target,
                    signature: value.entry_point.signature,
                },
                params: TracingParams::RPCTracer(RPCTracerParams {
                    caller: tracer_params.caller.clone(),
                    calldata: tracer_params.calldata.clone(),
                    state_overrides: tracer_params
                        .state_overrides
                        .clone()
                        .map(|s| {
                            s.into_iter()
                                .map(|(k, v)| (k, v.into()))
                                .collect()
                        }),
                    prune_addresses: tracer_params.prune_addresses.clone(),
                }),
            },
        }
    }
}

impl EntryPointWithTracingParams {
    pub fn new(entry_point: EntryPoint, params: TracingParams) -> Self {
        Self { entry_point, params }
    }
}

impl std::fmt::Display for EntryPointWithTracingParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tracer_type = match &self.params {
            TracingParams::RPCTracer(_) => "RPC",
        };
        write!(f, "{} [{}]", self.entry_point.external_id, tracer_type)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash, DeepSizeOf)]
/// An entry point to trace. Different types of entry points tracing will be supported in the
/// future. Like RPC debug tracing, symbolic execution, etc.
pub enum TracingParams {
    /// Uses RPC calls to retrieve the called addresses and retriggers
    RPCTracer(RPCTracerParams),
}

impl std::fmt::Display for TracingParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TracingParams::RPCTracer(params) => write!(f, "RPC: {params}"),
        }
    }
}

impl From<dto::TracingParams> for TracingParams {
    fn from(value: dto::TracingParams) -> Self {
        match value {
            dto::TracingParams::RPCTracer(tracer_params) => {
                TracingParams::RPCTracer(tracer_params.into())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash, DeepSizeOf)]
pub enum StorageOverride {
    Diff(BTreeMap<StoreKey, StoreVal>),
    Replace(BTreeMap<StoreKey, StoreVal>),
}

impl From<dto::StorageOverride> for StorageOverride {
    fn from(value: dto::StorageOverride) -> Self {
        match value {
            dto::StorageOverride::Diff(diff) => StorageOverride::Diff(diff),
            dto::StorageOverride::Replace(replace) => StorageOverride::Replace(replace),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash, DeepSizeOf)]
pub struct AccountOverrides {
    pub slots: Option<StorageOverride>,
    pub native_balance: Option<Balance>,
    pub code: Option<Code>,
}

impl From<dto::AccountOverrides> for AccountOverrides {
    fn from(value: dto::AccountOverrides) -> Self {
        Self {
            slots: value.slots.map(|s| s.into()),
            native_balance: value.native_balance,
            code: value.code,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Eq, Hash, DeepSizeOf)]
pub struct RPCTracerParams {
    /// The caller address of the transaction, if not provided tracing will use the default value
    /// for an address defined by the VM.
    pub caller: Option<Address>,
    /// The call data used for the tracing call, this needs to include the function selector
    pub calldata: Bytes,
    /// Optionally allow for state overrides so that the call works as expected
    pub state_overrides: Option<BTreeMap<Address, AccountOverrides>>,
    /// Addresses to prune from trace results. Useful for hooks that use mock
    /// accounts/routers that shouldn't be tracked in the final DCI results.
    pub prune_addresses: Option<Vec<Address>>,
}

impl From<dto::RPCTracerParams> for RPCTracerParams {
    fn from(value: dto::RPCTracerParams) -> Self {
        Self {
            caller: value.caller,
            calldata: value.calldata,
            state_overrides: value.state_overrides.map(|overrides| {
                overrides
                    .into_iter()
                    .map(|(address, account_overrides)| (address, account_overrides.into()))
                    .collect()
            }),
            prune_addresses: value.prune_addresses,
        }
    }
}

impl RPCTracerParams {
    pub fn new(caller: Option<Address>, calldata: Bytes) -> Self {
        Self { caller, calldata, state_overrides: None, prune_addresses: None }
    }

    pub fn with_state_overrides(mut self, state: BTreeMap<Address, AccountOverrides>) -> Self {
        self.state_overrides = Some(state);
        self
    }

    pub fn with_prune_addresses(mut self, addresses: Vec<Address>) -> Self {
        self.prune_addresses = Some(addresses);
        self
    }
}

impl std::fmt::Display for RPCTracerParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let caller_str = match &self.caller {
            Some(addr) => format!("caller={addr}"),
            None => String::new(),
        };

        let calldata_str = if self.calldata.len() >= 8 {
            format!(
                "calldata=0x{}..({} bytes)",
                hex::encode(&self.calldata[..8]),
                self.calldata.len()
            )
        } else {
            format!("calldata={}", self.calldata)
        };

        let overrides_str = match &self.state_overrides {
            Some(overrides) if !overrides.is_empty() => {
                format!(", {} state override(s)", overrides.len())
            }
            _ => String::new(),
        };

        write!(f, "{caller_str}, {calldata_str}{overrides_str}")
    }
}

// Ensure serialization order, required by the storage layer
impl Serialize for RPCTracerParams {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Count fields: always serialize caller and calldata, plus optional fields
        let mut field_count = 2;
        if self.state_overrides.is_some() {
            field_count += 1;
        }
        if self.prune_addresses.is_some() {
            field_count += 1;
        }

        let mut state = serializer.serialize_struct("RPCTracerEntryPoint", field_count)?;
        state.serialize_field("caller", &self.caller)?;
        state.serialize_field("calldata", &self.calldata)?;

        // Only serialize optional fields if they are present
        if let Some(ref overrides) = self.state_overrides {
            state.serialize_field("state_overrides", overrides)?;
        }
        if let Some(ref prune_addrs) = self.prune_addresses {
            state.serialize_field("prune_addresses", prune_addrs)?;
        }

        state.end()
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, DeepSizeOf,
)]
pub struct AddressStorageLocation {
    pub key: StoreKey,
    pub offset: u8,
}

impl AddressStorageLocation {
    pub fn new(key: StoreKey, offset: u8) -> Self {
        Self { key, offset }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, DeepSizeOf)]
pub struct TracingResult {
    /// A set of (address, storage slot) pairs representing state that contain a called address.
    /// If any of these storage slots change, the execution path might change.
    pub retriggers: HashSet<(Address, AddressStorageLocation)>,
    /// A map of all addresses that were called during the trace with a list of storage slots that
    /// were accessed.
    pub accessed_slots: HashMap<Address, HashSet<StoreKey>>,
}

impl TracingResult {
    pub fn new(
        retriggers: HashSet<(Address, AddressStorageLocation)>,
        accessed_slots: HashMap<Address, HashSet<StoreKey>>,
    ) -> Self {
        Self { retriggers, accessed_slots }
    }

    /// Merges this tracing result with another one.
    ///
    /// The method combines two [`TracingResult`] instances.
    pub fn merge(&mut self, other: TracingResult) {
        self.retriggers.extend(other.retriggers);
        for (address, slots) in other.accessed_slots {
            self.accessed_slots
                .entry(address)
                .or_default()
                .extend(slots);
        }
    }
}

#[derive(Debug, Clone, PartialEq, DeepSizeOf)]
/// Represents a traced entry point and the results of the tracing operation.
pub struct TracedEntryPoint {
    /// The combined entry point and tracing params that was traced
    pub entry_point_with_params: EntryPointWithTracingParams,
    /// The block hash of the block that the entry point was traced on.
    pub detection_block_hash: BlockHash,
    /// The results of the tracing operation
    pub tracing_result: TracingResult,
}

impl TracedEntryPoint {
    pub fn new(
        entry_point_with_params: EntryPointWithTracingParams,
        detection_block_hash: BlockHash,
        result: TracingResult,
    ) -> Self {
        Self { entry_point_with_params, detection_block_hash, tracing_result: result }
    }

    pub fn entry_point_id(&self) -> String {
        self.entry_point_with_params
            .entry_point
            .external_id
            .clone()
    }
}

impl std::fmt::Display for TracedEntryPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}: {} retriggers, {} accessed addresses]",
            self.entry_point_id(),
            self.tracing_result.retriggers.len(),
            self.tracing_result.accessed_slots.len()
        )
    }
}

#[cfg(test)]
pub mod fixtures {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;
    use crate::models::ChangeType;

    // PERF: duplicated in crate::extractor::models::fixtures — consider a `test-utils`
    // feature flag to share test fixtures cross-crate.
    pub fn create_transaction(hash: &str, block: &str, index: u64) -> Transaction {
        Transaction::new(
            hash.parse().unwrap(),
            block.parse().unwrap(),
            Bytes::zero(20),
            Some(Bytes::zero(20)),
            index,
        )
    }

    /// Returns a pre-built `TxWithChanges` for testing.
    ///
    /// Both indices share the same keys (component `"pool_0"`, token `0xaa..`, contract `0xbb..`)
    /// but with different values, so "later wins" precedence can be verified across all fields:
    ///
    /// - Index 0: tx_index=1, component_balance {token=800, token2=300}, account_balance
    ///   {token=500, token2=150}, slots {1=>100, 2=>200}, state {"reserve"=>1000, "fee"=>50}, 1
    ///   entrypoint, ChangeType::Creation
    /// - Index 1: tx_index=2, component_balance {token=1000}, account_balance {token=700}, slots
    ///   {1=>300} (overlaps slot 1), state {"reserve"=>2000} (overlaps), 2 entrypoints (superset),
    ///   ChangeType::Update
    // PERF: duplicated in crate::extractor::models::fixtures — consider a `test-utils`
    // feature flag to share test fixtures cross-crate.
    pub fn tx_with_changes(index: u8) -> TxWithChanges {
        let token = Bytes::from(vec![0xaa; 20]);
        let token2 = Bytes::from(vec![0xcc; 20]);
        let contract = Bytes::from(vec![0xbb; 20]);
        let c_id = "pool_0".to_string();

        match index {
            0 => {
                let tx = create_transaction("0x01", "0x00", 1);
                TxWithChanges {
                    tx: tx.clone(),
                    protocol_components: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponent { id: c_id.clone(), ..Default::default() },
                    )]),
                    account_deltas: HashMap::from([(
                        contract.clone(),
                        AccountDelta::new(
                            Chain::Ethereum,
                            contract.clone(),
                            HashMap::from([
                                (
                                    Bytes::from(1u64).lpad(32, 0),
                                    Some(Bytes::from(100u64).lpad(32, 0)),
                                ),
                                (
                                    Bytes::from(2u64).lpad(32, 0),
                                    Some(Bytes::from(200u64).lpad(32, 0)),
                                ),
                            ]),
                            None,
                            Some(Bytes::from(vec![0; 4])),
                            ChangeType::Creation,
                        ),
                    )]),
                    state_updates: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponentStateDelta::new(
                            &c_id,
                            HashMap::from([
                                ("reserve".into(), Bytes::from(1000u64).lpad(32, 0)),
                                ("fee".into(), Bytes::from(50u64).lpad(32, 0)),
                            ]),
                            HashSet::new(),
                        ),
                    )]),
                    balance_changes: HashMap::from([(
                        c_id.clone(),
                        HashMap::from([
                            (
                                token.clone(),
                                ComponentBalance {
                                    token: token.clone(),
                                    balance: Bytes::from(800_u64).lpad(32, 0),
                                    balance_float: 800.0,
                                    component_id: c_id.clone(),
                                    modify_tx: tx.hash.clone(),
                                },
                            ),
                            (
                                token2.clone(),
                                ComponentBalance {
                                    token: token2.clone(),
                                    balance: Bytes::from(300_u64).lpad(32, 0),
                                    balance_float: 300.0,
                                    component_id: c_id.clone(),
                                    modify_tx: tx.hash.clone(),
                                },
                            ),
                        ]),
                    )]),
                    account_balance_changes: HashMap::from([(
                        contract.clone(),
                        HashMap::from([
                            (
                                token.clone(),
                                AccountBalance {
                                    token: token.clone(),
                                    balance: Bytes::from(500_u64).lpad(32, 0),
                                    modify_tx: tx.hash.clone(),
                                    account: contract.clone(),
                                },
                            ),
                            (
                                token2,
                                AccountBalance {
                                    token: Bytes::from(vec![0xcc; 20]),
                                    balance: Bytes::from(150_u64).lpad(32, 0),
                                    modify_tx: tx.hash,
                                    account: contract,
                                },
                            ),
                        ]),
                    )]),
                    entrypoints: HashMap::from([(
                        c_id.clone(),
                        HashSet::from([EntryPoint::new(
                            "ep_0".into(),
                            Bytes::zero(20),
                            "fn_a()".into(),
                        )]),
                    )]),
                    entrypoint_params: HashMap::from([(
                        "ep_0".into(),
                        HashSet::from([(
                            TracingParams::RPCTracer(RPCTracerParams::new(
                                None,
                                Bytes::from(vec![1]),
                            )),
                            c_id,
                        )]),
                    )]),
                }
            }
            1 => {
                let tx = create_transaction("0x02", "0x00", 2);
                TxWithChanges {
                    tx: tx.clone(),
                    protocol_components: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponent { id: c_id.clone(), ..Default::default() },
                    )]),
                    account_deltas: HashMap::from([(
                        contract.clone(),
                        AccountDelta::new(
                            Chain::Ethereum,
                            contract.clone(),
                            HashMap::from([(
                                Bytes::from(1u64).lpad(32, 0),
                                Some(Bytes::from(300u64).lpad(32, 0)),
                            )]),
                            None,
                            None,
                            ChangeType::Update,
                        ),
                    )]),
                    state_updates: HashMap::from([(
                        c_id.clone(),
                        ProtocolComponentStateDelta::new(
                            &c_id,
                            HashMap::from([("reserve".into(), Bytes::from(2000u64).lpad(32, 0))]),
                            HashSet::new(),
                        ),
                    )]),
                    balance_changes: HashMap::from([(
                        c_id.clone(),
                        HashMap::from([(
                            token.clone(),
                            ComponentBalance {
                                token: token.clone(),
                                balance: Bytes::from(1000_u64).lpad(32, 0),
                                balance_float: 1000.0,
                                component_id: c_id.clone(),
                                modify_tx: tx.hash.clone(),
                            },
                        )]),
                    )]),
                    account_balance_changes: HashMap::from([(
                        contract.clone(),
                        HashMap::from([(
                            token.clone(),
                            AccountBalance {
                                token: token.clone(),
                                balance: Bytes::from(700_u64).lpad(32, 0),
                                modify_tx: tx.hash,
                                account: contract,
                            },
                        )]),
                    )]),
                    entrypoints: HashMap::from([(
                        c_id.clone(),
                        HashSet::from([
                            EntryPoint::new("ep_0".into(), Bytes::zero(20), "fn_a()".into()),
                            EntryPoint::new("ep_1".into(), Bytes::zero(20), "fn_b()".into()),
                        ]),
                    )]),
                    entrypoint_params: HashMap::from([(
                        "ep_1".into(),
                        HashSet::from([(
                            TracingParams::RPCTracer(RPCTracerParams::new(
                                None,
                                Bytes::from(vec![2]),
                            )),
                            c_id,
                        )]),
                    )]),
                }
            }
            _ => panic!("tx_with_changes: index must be 0 or 1, got {index}"),
        }
    }

    #[test]
    fn test_merge_tx_with_changes() {
        let mut changes1 = tx_with_changes(0);
        let changes2 = tx_with_changes(1);

        let token = Bytes::from(vec![0xaa; 20]);
        let contract = Bytes::from(vec![0xbb; 20]);
        let c_id = "pool_0".to_string();

        assert!(changes1.merge(changes2).is_ok());

        // After merge, balances should reflect changes2 ("later wins")
        assert_eq!(
            changes1.balance_changes[&c_id][&token].balance,
            Bytes::from(1000_u64).lpad(32, 0),
        );
        assert_eq!(
            changes1.account_balance_changes[&contract][&token].balance,
            Bytes::from(700_u64).lpad(32, 0),
        );
        // tx should be updated to changes2's tx
        assert_eq!(changes1.tx.hash, Bytes::from(vec![2]));
        // Entrypoints should be merged (union of both)
        assert_eq!(changes1.entrypoints[&c_id].len(), 2);
        let mut sigs: Vec<_> = changes1.entrypoints[&c_id]
            .iter()
            .map(|ep| ep.signature.clone())
            .collect();
        sigs.sort();
        assert_eq!(sigs, vec!["fn_a()", "fn_b()"]);
    }

    #[rstest]
    #[case::mismatched_blocks(
        fixtures::create_transaction("0x01", "0x0abc", 1),
        fixtures::create_transaction("0x02", "0x0def", 2)
    )]
    #[case::older_transaction(
        fixtures::create_transaction("0x02", "0x0abc", 2),
        fixtures::create_transaction("0x01", "0x0abc", 1)
    )]
    fn test_merge_errors(#[case] tx1: Transaction, #[case] tx2: Transaction) {
        let mut changes1 = TxWithChanges { tx: tx1, ..Default::default() };

        let changes2 = TxWithChanges { tx: tx2, ..Default::default() };

        assert!(changes1.merge(changes2).is_err());
    }

    #[test]
    fn test_rpc_tracer_entry_point_serialization_order() {
        use std::str::FromStr;

        use serde_json;

        let entry_point = RPCTracerParams::new(
            Some(Address::from_str("0x1234567890123456789012345678901234567890").unwrap()),
            Bytes::from_str("0xabcdef").unwrap(),
        );

        let serialized = serde_json::to_string(&entry_point).unwrap();

        // Verify that "caller" comes before "calldata" in the serialized output
        assert!(serialized.find("\"caller\"").unwrap() < serialized.find("\"calldata\"").unwrap());

        // Verify we can deserialize it back
        let deserialized: RPCTracerParams = serde_json::from_str(&serialized).unwrap();
        assert_eq!(entry_point, deserialized);
    }

    #[test]
    fn test_tracing_result_merge() {
        let address1 = Address::from_str("0x1234567890123456789012345678901234567890").unwrap();
        let address2 = Address::from_str("0x2345678901234567890123456789012345678901").unwrap();
        let address3 = Address::from_str("0x3456789012345678901234567890123456789012").unwrap();

        let store_key1 = StoreKey::from(vec![1, 2, 3, 4]);
        let store_key2 = StoreKey::from(vec![5, 6, 7, 8]);

        let mut result1 = TracingResult::new(
            HashSet::from([(
                address1.clone(),
                AddressStorageLocation::new(store_key1.clone(), 12),
            )]),
            HashMap::from([
                (address2.clone(), HashSet::from([store_key1.clone()])),
                (address3.clone(), HashSet::from([store_key2.clone()])),
            ]),
        );

        let result2 = TracingResult::new(
            HashSet::from([(
                address3.clone(),
                AddressStorageLocation::new(store_key2.clone(), 12),
            )]),
            HashMap::from([
                (address1.clone(), HashSet::from([store_key1.clone()])),
                (address2.clone(), HashSet::from([store_key2.clone()])),
            ]),
        );

        result1.merge(result2);

        // Verify retriggers were merged
        assert_eq!(result1.retriggers.len(), 2);
        assert!(result1
            .retriggers
            .contains(&(address1.clone(), AddressStorageLocation::new(store_key1.clone(), 12))));
        assert!(result1
            .retriggers
            .contains(&(address3.clone(), AddressStorageLocation::new(store_key2.clone(), 12))));

        // Verify accessed slots were merged
        assert_eq!(result1.accessed_slots.len(), 3);
        assert!(result1
            .accessed_slots
            .contains_key(&address1));
        assert!(result1
            .accessed_slots
            .contains_key(&address2));
        assert!(result1
            .accessed_slots
            .contains_key(&address3));

        assert_eq!(
            result1
                .accessed_slots
                .get(&address2)
                .unwrap(),
            &HashSet::from([store_key1.clone(), store_key2.clone()])
        );
    }

    #[test]
    fn test_entry_point_with_tracing_params_display() {
        use std::str::FromStr;

        let entry_point = EntryPoint::new(
            "uniswap_v3_pool_swap".to_string(),
            Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            "swapExactETHForTokens(uint256,address[],address,uint256)".to_string(),
        );

        let tracing_params = TracingParams::RPCTracer(RPCTracerParams::new(
            Some(Address::from_str("0x9876543210987654321098765432109876543210").unwrap()),
            Bytes::from_str("0xabcdef").unwrap(),
        ));

        let entry_point_with_params = EntryPointWithTracingParams::new(entry_point, tracing_params);

        let display_output = entry_point_with_params.to_string();
        assert_eq!(display_output, "uniswap_v3_pool_swap [RPC]");
    }

    #[test]
    fn test_traced_entry_point_display() {
        use std::str::FromStr;

        let entry_point = EntryPoint::new(
            "uniswap_v3_pool_swap".to_string(),
            Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            "swapExactETHForTokens(uint256,address[],address,uint256)".to_string(),
        );

        let tracing_params = TracingParams::RPCTracer(RPCTracerParams::new(
            Some(Address::from_str("0x9876543210987654321098765432109876543210").unwrap()),
            Bytes::from_str("0xabcdef").unwrap(),
        ));

        let entry_point_with_params = EntryPointWithTracingParams::new(entry_point, tracing_params);

        // Create tracing result with 2 retriggers and 3 accessed addresses
        let address1 = Address::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let address2 = Address::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let address3 = Address::from_str("0x3333333333333333333333333333333333333333").unwrap();

        let store_key1 = StoreKey::from(vec![1, 2, 3, 4]);
        let store_key2 = StoreKey::from(vec![5, 6, 7, 8]);

        let tracing_result = TracingResult::new(
            HashSet::from([
                (address1.clone(), AddressStorageLocation::new(store_key1.clone(), 0)),
                (address2.clone(), AddressStorageLocation::new(store_key2.clone(), 12)),
            ]),
            HashMap::from([
                (address1.clone(), HashSet::from([store_key1.clone()])),
                (address2.clone(), HashSet::from([store_key2.clone()])),
                (address3.clone(), HashSet::from([store_key1.clone()])),
            ]),
        );

        let traced_entry_point = TracedEntryPoint::new(
            entry_point_with_params,
            Bytes::from_str("0xabcdef1234567890").unwrap(),
            tracing_result,
        );

        let display_output = traced_entry_point.to_string();
        assert_eq!(display_output, "[uniswap_v3_pool_swap: 2 retriggers, 3 accessed addresses]");
    }
}
