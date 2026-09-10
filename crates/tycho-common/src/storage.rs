//! Storage traits used by Tycho
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use thiserror::Error;

use crate::{
    dto,
    models::{
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
        Address, BlockHash, Chain, ComponentId, ContractId, EntryPointId, ExtractionState,
        PaginationParams, ProtocolSystem, ProtocolType, TxHash,
    },
    Bytes,
};

/// Identifies a block in storage.
#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub enum BlockIdentifier {
    /// Identifies the block by its position on a specified chain.
    ///
    /// This form of identification has potential risks as it may become
    /// ambiguous in certain situations. For example, if the block has not been
    /// finalised, there exists a possibility of forks occurring. As a result,
    /// the same number could refer to different blocks on different forks.
    Number((Chain, i64)),

    /// Identifies a block by its hash.
    ///
    /// The hash should be unique across multiple chains. Preferred method if
    /// the block is very recent.
    Hash(BlockHash),

    /// Latest stored block for the target chain
    ///
    /// Returns the block with the highest block number on the target chain.
    Latest(Chain),
}

impl Display for BlockIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Error, Debug, PartialEq, Clone)]
pub enum StorageError {
    #[error("Could not find {0} with id `{1}`!")]
    NotFound(String, String),
    #[error("The entity {0} with id {1} was already present!")]
    DuplicateEntry(String, String),
    #[error("Could not find related {0} for {1} with id `{2}`!")]
    NoRelatedEntity(String, String, String),
    #[error("DecodeError: {0}")]
    DecodeError(String),
    #[error("Unexpected storage error: {0}")]
    Unexpected(String),
    #[error("Currently unsupported operation: {0}")]
    Unsupported(String),
    #[error("Write cache unexpectedly dropped notification channel!")]
    WriteCacheGoneAway(),
    #[error("Invalid block range encountered")]
    InvalidBlockRange(),
}

/// Storage methods for chain specific objects.
///
/// This trait abstracts the specific implementation details of a blockchain's
/// entities, allowing the user to add and retrieve blocks and transactions in a
/// generic way.
///
/// For traceability protocol components and contracts changes are linked to
/// blocks of their respective chain if applicable. This means while indexing we
/// need to keep a lightweight and cross chain compatible representation of
/// blocks and transactions in storage.
///
/// It's defined generically over two associated types:
///
/// * `Block`: represents a block in the blockchain.
/// * `Transaction`: represents a transaction within a block.
#[async_trait]
pub trait ChainGateway {
    /// Upserts a new block to the blockchain's storage.
    ///
    /// Ignores any existing tx, if the new entry has different attributes
    /// no error is raised and the old entry is kept.
    ///
    /// # Parameters
    /// - `new`: An instance of `Self::Block`, representing the new block to be stored.
    ///
    /// # Returns
    /// - Empty ok result indicates success. Failure might occur if the block is already present.
    async fn upsert_block(&self, new: &[Block]) -> Result<(), StorageError>;
    /// Retrieves a block from storage.
    ///
    /// # Parameters
    /// - `id`: Block's unique identifier of type `BlockIdentifier`.
    ///
    /// # Returns
    /// - An Ok result containing the block. Might fail if the block does not exist yet.
    async fn get_block(&self, id: &BlockIdentifier) -> Result<Block, StorageError>;
    /// Upserts a transaction to storage.
    ///
    /// Ignores any existing tx, if the new entry has different attributes
    /// no error is raised and the old entry is kept.
    ///
    /// # Parameters
    /// - `new`: An instance of `Self::Transaction`, representing the new transaction to be stored.
    ///
    /// # Returns
    /// - Empty ok result indicates success. Failure might occur if the
    /// corresponding block does not exists yet, or if the transaction already
    /// exists.
    async fn upsert_tx(&self, new: &[Transaction]) -> Result<(), StorageError>;

    /// Tries to retrieve a transaction from the blockchain's storage using its
    /// hash.
    ///
    /// # Parameters
    /// - `hash`: The byte slice representing the hash of the transaction to be retrieved.
    ///
    /// # Returns
    /// - An Ok result containing the transaction. Might fail if the transaction does not exist yet.
    async fn get_tx(&self, hash: &TxHash) -> Result<Transaction, StorageError>;

    /// Reverts the blockchain storage to a previous version.
    ///
    /// Reverting state signifies deleting database history. Only the main branch will be kept.
    ///
    /// Blocks that are greater than the provided block (`to`) are deleted and any versioned rows
    /// which were invalidated in the deleted blocks are updated to be valid again.
    ///
    /// # Parameters
    /// - `to` The version to revert to. Given a block uses VersionKind::Last behaviour.
    /// - `db` The database gateway.
    ///
    /// # Returns
    /// - An Ok if the revert is successful, or a `StorageError` if not.
    async fn revert_state(&self, to: &BlockIdentifier) -> Result<(), StorageError>;
}

/// Store and retrieve state of Extractors.
///
/// Sometimes extractors may wish to persist their state across restart. E.g.
/// substreams based extractors need to store the cursor, so they can continue
/// processing where they left off.
///
/// Extractors are uniquely identified by a name and the respective chain which
/// they are indexing.
#[async_trait]
pub trait ExtractionStateGateway {
    /// Retrieves the state of an extractor instance from a storage.
    ///
    /// # Parameters
    /// - `name` A unique name for the extractor instance.
    /// - `chain` The chain this extractor is indexing.
    ///
    /// # Returns
    /// Ok if the corrsponding state was retrieved successfully, Err in
    /// case the state was not found.
    async fn get_state(&self, name: &str, chain: &Chain) -> Result<ExtractionState, StorageError>;

    /// Saves the state of an extractor instance to a storage.
    ///
    /// Creates an entry if not present yet, or updates an already existing
    /// entry.
    ///
    /// # Parameters
    /// - `state` The state of the extractor that needs to be saved.
    ///
    /// # Returns
    /// Ok, if state was stored successfully, Err if the state is not valid.
    async fn save_state(&self, state: &ExtractionState) -> Result<(), StorageError>;
}

/// Point in time as either block or timestamp. If a block is chosen it
/// timestamp attribute is used.
#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub enum BlockOrTimestamp {
    Block(BlockIdentifier),
    Timestamp(NaiveDateTime),
}

// TODO: remove once deprecated chain field is removed from VersionParam
#[allow(deprecated)]
impl TryFrom<&dto::VersionParam> for BlockOrTimestamp {
    type Error = anyhow::Error;

    fn try_from(version: &dto::VersionParam) -> Result<Self, Self::Error> {
        match (&version.timestamp, &version.block) {
            (_, Some(block)) => {
                // If a full block is provided, we prioritize hash over number and chain
                let block_identifier = match (&block.hash, &block.chain, &block.number) {
                    (Some(hash), _, _) => BlockIdentifier::Hash(hash.clone()),
                    (_, Some(chain), Some(number)) => {
                        BlockIdentifier::Number((Chain::from(*chain), *number))
                    }
                    _ => {
                        return Err(anyhow::format_err!("Insufficient block information".to_owned()))
                    }
                };
                Ok(BlockOrTimestamp::Block(block_identifier))
            }
            (Some(timestamp), None) => Ok(BlockOrTimestamp::Timestamp(*timestamp)),
            (None, None) => {
                Err(anyhow::format_err!("Missing timestamp or block identifier".to_owned()))
            }
        }
    }
}

/// References certain states within a single block.
///
/// **Note:** Not all methods that take a version will support all version kinds,
/// the versions here are included for completeness and to document the
/// retrieval behaviour that is possible with the storage layout. Please refer
/// to the individual implementation for information about which version kinds
/// it supports.
#[derive(Debug, Clone, Default)]
pub enum VersionKind {
    /// Represents the final state within a specific block. Essentially, it
    /// retrieves the state subsequent to the execution of the last transaction
    /// executed in that block.
    #[default]
    Last,
    /// Represents the initial state of a specific block. In other words,
    /// it is the state before any transaction has been executed within that block.
    First,
    /// Represents a specific transactions indexed position within a block.
    /// It includes the state after executing the transaction at that index.
    Index(i64),
}

/// A version desribes the state of the DB at a exact point in time.
/// See the module level docs for more information on how versioning works.
#[derive(Debug, Clone)]
pub struct Version(pub BlockOrTimestamp, pub VersionKind);

impl Version {
    pub fn from_block_number(chain: Chain, number: i64) -> Self {
        Self(BlockOrTimestamp::Block(BlockIdentifier::Number((chain, number))), VersionKind::Last)
    }
    pub fn from_ts(ts: NaiveDateTime) -> Self {
        Self(BlockOrTimestamp::Timestamp(ts), VersionKind::Last)
    }
}

// Helper type to retrieve entities with their total retrievable count.
#[derive(Debug)]
pub struct WithTotal<T> {
    pub entity: T,
    pub total: Option<i64>,
}

/// Store and retrieve protocol related structs.
///
/// This trait defines how to retrieve protocol components, state as well as
/// tokens from storage.
#[async_trait]
pub trait ProtocolGateway {
    /// Retrieve ProtocolComponent from the db
    ///
    /// # Parameters
    /// - `chain` The chain of the component
    /// - `system` Allows to optionally filter by system.
    /// - `ids` Allows to optionally filter by id.
    /// - `min_tvl` Allows to optionally filter by min tvl.
    /// - `pagination_params` Optional pagination parameters to control the number of results.
    ///
    /// # Returns
    /// Ok, if found else Err
    async fn get_protocol_components(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        min_tvl: Option<f64>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<ProtocolComponent>>, StorageError>;

    /// Retrieves owners of tokens
    ///
    /// Queries for owners (protocol components) of tokens that have a certain minimum
    /// balance and returns a maximum aggregate of those in case there are multiple
    /// owners.
    ///
    /// # Parameters
    /// - `chain` The chain of the component
    /// - `tokens` The tokens to query for, any component with at least one of these tokens is
    ///   returned.
    /// - `min_balance` A minimum balance we expect the component to have on any of the tokens
    ///   mentioned in `tokens`.
    async fn get_token_owners(
        &self,
        chain: &Chain,
        tokens: &[Address],
        min_balance: Option<f64>,
    ) -> Result<HashMap<Address, (ComponentId, Bytes)>, StorageError>;

    async fn add_protocol_components(&self, new: &[ProtocolComponent]) -> Result<(), StorageError>;

    async fn delete_protocol_components(
        &self,
        to_delete: &[ProtocolComponent],
        block_ts: NaiveDateTime,
    ) -> Result<(), StorageError>;

    /// Stores new found ProtocolTypes.
    ///
    /// # Parameters
    /// - `new_protocol_types`  The new protocol types.
    ///
    /// # Returns
    /// Ok if stored successfully.
    async fn add_protocol_types(
        &self,
        new_protocol_types: &[ProtocolType],
    ) -> Result<(), StorageError>;

    /// Retrieve protocol component states
    ///
    /// This resource is versioned, the version can be specified by either block
    /// or timestamp, for off-chain components, a block version will error.
    ///
    /// As the state is retained on a transaction basis on blockchain systems, a
    /// single version may relate to more than one state. In these cases a
    /// versioned result is returned, if requesting `Version:All` with the
    /// latest entry being the state at the end of the block and the first entry
    /// represents the first change to the state within the block.
    ///
    /// # Parameters
    /// - `chain` The chain of the component
    /// - `at` The version at which the state is valid at.
    /// - `system` The protocol system this component belongs to
    /// - `ids` The external ids of the components e.g. addresses, or the pairs
    /// - `retrieve_balances` Whether to retrieve the balances for the components.
    /// - `pagination_params` Optional pagination parameters to control the number of results.
    async fn get_protocol_states(
        &self,
        chain: &Chain,
        at: Option<Version>,
        system: Option<String>,
        ids: Option<&[&str]>,
        retrieve_balances: bool,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError>;

    async fn update_protocol_states(
        &self,
        new: &[(TxHash, ProtocolComponentStateDelta)],
    ) -> Result<(), StorageError>;

    /// Retrieves a tokens from storage
    ///
    /// # Parameters
    /// - `chain` The chain this token is implemented on.
    /// - `address` The address for the token within the chain.
    /// - `quality` The quality of the token.
    /// - `traded_n_days_ago` The number of days ago the token was traded.
    /// - `pagination_params` Optional pagination parameters to control the number of results.
    ///
    /// # Returns
    /// Ok if the results could be retrieved from the storage, else errors.
    async fn get_tokens(
        &self,
        chain: Chain,
        address: Option<&[&Address]>,
        quality: QualityRange,
        traded_n_days_ago: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<Token>>, StorageError>;

    /// Saves multiple component balances to storage.
    ///
    /// # Parameters
    /// - `component_balances` The component balances to insert.
    ///
    /// # Return
    /// Ok if all component balances could be inserted, Err if at least one token failed to
    /// insert.
    async fn add_component_balances(
        &self,
        component_balances: &[ComponentBalance],
    ) -> Result<(), StorageError>;

    /// Saves multiple tokens to storage.
    ///
    /// Inserts token into storage. Tokens and their properties are assumed to
    /// be immutable.
    ///
    /// # Parameters
    /// - `tokens` The tokens to insert.
    ///
    /// # Return
    /// Ok if all tokens could be inserted, Err if at least one token failed to
    /// insert.
    async fn add_tokens(&self, tokens: &[Token]) -> Result<(), StorageError>;

    /// Updates multiple tokens in storage.
    ///
    /// Updates token in storage. Will warn if one of the tokens does not exist in the
    /// database. Currently assumes that token addresses are unique across chains.
    ///
    /// # Parameters
    /// - `tokens` The tokens to update.
    ///
    /// # Return
    /// Ok if all tokens could be inserted, Err if at least one token failed to
    /// insert.
    async fn update_tokens(&self, tokens: &[Token]) -> Result<(), StorageError>;

    /// Retrieve protocol state changes
    ///
    /// Fetches all state changes that occurred for the given chain
    ///
    /// # Parameters
    /// - `chain` The chain of the component
    /// - `start_version` The version at which to start looking for changes at.
    /// - `end_version` The version at which to stop looking for changes.
    ///
    /// # Return
    /// A list of ProtocolStateDeltas containing all state changes, Err if no changes were found.
    async fn get_protocol_states_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<Vec<ProtocolComponentStateDelta>, StorageError>;

    /// Retrieve protocol component balance changes
    ///
    /// Fetches all balance changes that occurred for the given protocol system
    ///
    /// # Parameters
    /// - `chain` The chain of the component
    /// - `start_version` The version at which to start looking for changes at.
    /// - `target_version` The version at which to stop looking for changes.
    ///
    /// # Return
    /// A vec containing ComponentBalance objects for changed components.
    async fn get_balance_deltas(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        target_version: &BlockOrTimestamp,
    ) -> Result<Vec<ComponentBalance>, StorageError>;

    async fn get_component_balances(
        &self,
        chain: &Chain,
        ids: Option<&[&str]>,
        version: Option<&Version>,
    ) -> Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, StorageError>;

    async fn get_token_prices(&self, chain: &Chain) -> Result<HashMap<Bytes, f64>, StorageError>;

    async fn upsert_component_tvl(
        &self,
        chain: &Chain,
        tvl_values: &HashMap<String, f64>,
    ) -> Result<(), StorageError>;

    /// Retrieve a list of actively supported protocol systems
    ///
    /// Fetches the list of protocol systems supported by the Tycho indexing service.
    ///
    /// # Parameters
    /// - `chain` The chain for which to retrieve supported protocol systems.
    /// - `pagination_params` Optional pagination parameters to control the number of results.
    ///
    /// # Return
    /// A paginated list of supported protocol systems, along with the total count.
    async fn get_protocol_systems(
        &self,
        chain: &Chain,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<String>>, StorageError>;

    /// Retrieve the components total value locked (TVL).
    ///
    /// # Parameters
    /// - `chain` The chain for which to retrieve the total value locked
    /// - `system` The protocol system for which to retrieve the total value locked
    /// - `ids` The ids of the components to retrieve the total value locked for
    /// - `pagination_params` Optional pagination parameters to control the number of results.
    ///
    /// # Return
    /// A result with a map of component ids to their TVL. Err if storage access failed.
    async fn get_component_tvls(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<String, f64>>, StorageError>;
}

/// Filters for entry points queries in the database.
// Shalow but can be used to add more filters without breaking backwards compatibility in the future
#[derive(Debug, Clone)]
pub struct EntryPointFilter {
    pub protocol_system: ProtocolSystem,
    pub component_ids: Option<Vec<ComponentId>>,
}

impl EntryPointFilter {
    pub fn new(protocol: ProtocolSystem) -> Self {
        Self { protocol_system: protocol, component_ids: None }
    }

    pub fn with_component_ids(mut self, component_ids: Vec<ComponentId>) -> Self {
        self.component_ids = Some(component_ids);
        self
    }
}

// Trait for entry point gateway operations.
#[async_trait]
pub trait EntryPointGateway {
    /// Inserts a list of entry points into the database.
    ///
    /// # Arguments
    /// * `entry_points` - The map of component ids to their entry points to insert.
    ///
    /// Note: This function ignores conflicts on inserts.
    async fn insert_entry_points(
        &self,
        entry_points: &HashMap<ComponentId, HashSet<EntryPoint>>,
    ) -> Result<(), StorageError>;

    /// Inserts a list of entry points with their tracing params into the database.
    ///
    /// # Arguments
    /// * `entry_points_params` - The map of entry points to their tracing params to insert and
    ///   component id for the component linked to the params.
    ///
    /// Note: This function ignores conflicts on inserts.
    async fn insert_entry_point_tracing_params(
        &self,
        entry_points_params: &HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
    ) -> Result<(), StorageError>;

    /// Retrieves a map of component ids to a set of entry points from the database.
    ///
    /// # Arguments
    /// * `filter` - The EntryPointFilter to apply to the query.
    /// * `pagination_params` - The pagination parameters to apply to the query, if None, all
    ///   results are returned.
    ///
    /// # Returns
    /// A map of component ids to a set of entry points.
    async fn get_entry_points(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPoint>>>, StorageError>;

    /// Retrieves a map of component ids to a set of entry points with their tracing data from the
    /// database.
    ///
    /// # Arguments
    /// * `filter` - The EntryPointFilter to apply to the query.
    /// * `pagination_params` - The pagination parameters to apply to the query, if None, all
    ///   results are returned.
    ///
    /// # Returns
    /// A map of component ids to a set of entry points with their tracing params.
    async fn get_entry_points_tracing_params(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPointWithTracingParams>>>, StorageError>;

    /// Upserts a list of traced entry points into the database. Updates the result if it already
    /// exists for the same entry point and tracing params.
    ///
    /// # Arguments
    /// * `traced_entry_points` - The list of traced entry points to upsert.
    async fn upsert_traced_entry_points(
        &self,
        traced_entry_points: &[TracedEntryPoint],
    ) -> Result<(), StorageError>;

    /// Retrieves all tracing results for a set of entry points from the database.
    ///
    /// # Arguments
    /// * `entry_points` - The set of entry points to retrieve tracing results for.
    ///
    /// # Returns
    /// A map of entry point ids to a map of tracing params to tracing results.
    async fn get_traced_entry_points(
        &self,
        entry_points: &HashSet<EntryPointId>,
    ) -> Result<HashMap<EntryPointId, HashMap<TracingParams, TracingResult>>, StorageError>;
}

/// Manage contracts and their state in storage.
///
/// Specifies how to retrieve, add and update contracts in storage.
#[async_trait]
pub trait ContractStateGateway {
    /// Get a contracts state from storage
    ///
    /// This method retrieves a single contract from the database.
    ///
    /// # Parameters
    /// - `id` The identifier for the contract.
    /// - `version` Version at which to retrieve state for. None retrieves the latest state.
    /// - `include_slots`: Flag to determine whether to include slot changes. If set to `true`, it
    ///   includes storage slot.
    async fn get_contract(
        &self,
        id: &ContractId,
        version: Option<&Version>,
        include_slots: bool,
    ) -> Result<Account, StorageError>;

    /// Get multiple contracts' states from storage.
    ///
    /// This method retrieves balance and code, and optionally storage, of
    /// multiple contracts in a chain. It can optionally filter by given
    /// addresses and retrieve state for specific versions.
    ///
    /// # Parameters:
    /// - `chain`: The blockchain where the contracts reside.
    /// - `addresses`: Filter for specific addresses. If set to `None`, it retrieves all indexed
    ///   contracts in the chain.
    /// - `version`: Version at which to retrieve state for. If set to `None`, it retrieves the
    ///   latest state.
    /// - `include_slots`: Flag to determine whether to include slot changes. If set to `true`, it
    ///   includes storage slot.
    /// - `pagination_params`: Optional pagination parameters to control the number of results.
    ///
    /// # Returns:
    /// A `Result` with a list of contract states if the operation is
    /// successful, or a `StorageError` if the operation fails.
    async fn get_contracts(
        &self,
        chain: &Chain,
        addresses: Option<&[Address]>,
        version: Option<&Version>,
        include_slots: bool,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<Account>>, StorageError>;

    /// Inserts a new contract into the database.
    ///
    /// Inserts only the static values of the contract. To insert the contract slots, balance and
    /// code please use the `update_contracts` method.
    ///
    /// # Arguments
    /// - `new`: A reference to the new contract state to be inserted.
    ///
    /// # Returns
    /// - A Result with Ok if the operation was successful, and an Err containing `StorageError` if
    ///   there was an issue inserting the contract into the database. E.g. if the contract already
    ///   existed.
    async fn insert_contract(&self, new: &Account) -> Result<(), StorageError>;

    /// Update multiple contracts
    ///
    /// Given contract deltas, this method will batch all updates to contracts across a single
    /// chain.
    ///
    /// As changes are versioned by transaction, each changeset needs to be associated with a
    /// transaction hash. All references transaction are assumed to be already persisted.
    ///
    /// # Arguments
    ///
    /// - `new`: A reference to a slice of tuples where each tuple has a transaction hash (`TxHash`)
    ///   and a reference to the state delta (`&Self::Delta`) for that transaction.
    ///
    /// # Returns
    ///
    /// A Result with `Ok` if the operation was successful, and an `Err` containing
    /// `StorageError` if there was an issue updating the contracts in the database. E.g. if a
    /// transaction can't be located by it's reference or accounts refer to a different chain then
    /// the one specified.
    async fn update_contracts(&self, new: &[(TxHash, AccountDelta)]) -> Result<(), StorageError>;

    /// Mark a contract as deleted
    ///
    /// Issues a soft delete of the contract.
    ///
    /// # Parameters
    /// - `id` The identifier for the contract.
    /// - `at_tx` The transaction hash which deleted the contract. This transaction is assumed to be
    ///   in storage already. None retrieves the latest state.
    ///
    /// # Returns
    /// Ok if the deletion was successful, might Err if:
    ///  - Contract is not present in storage.
    ///  - Deletion transaction is not present in storage.
    ///  - Contract was already deleted.
    async fn delete_contract(&self, id: &ContractId, at_tx: &TxHash) -> Result<(), StorageError>;

    /// Retrieve a account delta between two versions.
    ///
    /// Given start version V1 and end version V2, this method will return the
    /// changes necessary to move from V1 to V2. So if V1 < V2, it will contain
    /// the changes of all accounts that changed between the two versions with the
    /// values corresponding to V2. If V2 < V1 then it will contain all the
    /// slots that changed between the two versions with the values corresponding to V1.
    ///
    /// This method is mainly meant to handle reverts, but can also be used to create delta changes
    /// between two historical version thus providing the basis for creating a backtestable stream
    /// of messages.
    ///
    /// # Parameters
    ///
    /// - `chain` The chain for which to generate the delta changes.
    /// - `start_version` The deltas start version, given a block uses VersionKind::Last behaviour.
    ///   If None the latest version is assumed.
    /// - `end_version` The deltas end version, given a block uses VersionKind::Last behaviour.
    ///
    /// # Note
    ///
    /// A choice to utilize `BlockOrTimestamp` has been made intentionally in
    /// this scenario as passing a `Version` by user isn't quite logical.
    /// Support for deltas is limited to the states at the start or end of
    /// blocks because blockchain reorganization at the transaction level is not
    /// common.
    ///
    /// The decision to use either the beginning or end state of a block is
    /// automatically determined by the underlying logic. For example, if we are
    /// tracing back, `VersionKind::First` retrieval mode will be used.
    /// Conversely, if we're progressing forward, we would apply the
    /// `VersionKind::Last` semantics.
    ///
    /// # Returns
    /// A map containing the necessary changes to update a state from start_version to end_version.
    /// Errors if:
    ///     - The versions can't be located in storage.
    ///     - There was an error with the database
    async fn get_accounts_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<Vec<AccountDelta>, StorageError>;

    /// Saves multiple account balances to storage.
    ///
    /// # Parameters
    /// - `account_balances` The account balances to insert.
    ///
    /// # Return
    /// Ok if all account balances could be inserted, Err if at least one token failed to insert.
    async fn add_account_balances(
        &self,
        account_balances: &[AccountBalance],
    ) -> Result<(), StorageError>;

    /// Retrieve account balances
    ///
    /// # Parameters
    /// - `chain` The chain of the account balances
    /// - `accounts` The accounts to query for. If set to `None`, it retrieves balances for all
    ///   indexed
    ///  accounts in the chain.
    /// - `version` Version at which to retrieve balances for. If set to `None`, it retrieves the
    ///   latest balances.
    async fn get_account_balances(
        &self,
        chain: &Chain,
        accounts: Option<&[Address]>,
        version: Option<&Version>,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError>;
}

pub trait Gateway:
    ChainGateway
    + ContractStateGateway
    + ExtractionStateGateway
    + ProtocolGateway
    + ContractStateGateway
    + EntryPointGateway
    + Send
    + Sync
{
}
