use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel::{
    dsl::{exists, sql},
    pg::Pg,
    prelude::*,
    query_builder::{BoxedSqlQuery, SqlQuery},
    sql_query,
    sql_types::{self, BigInt, Bool, Double},
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use diesel_derive_enum::DbEnum;
use tycho_common::{
    models::{
        self,
        blockchain::{
            EntryPointWithTracingParams as EntryPointWithTracingParamsCommon,
            TracingParams as EntryPointTracingParamsCommon,
        },
        Address, AttrStoreKey, Balance, BlockHash, Code, CodeHash, ComponentId, ContractId,
        EntryPointId, PaginationParams, StoreVal, TxHash,
    },
    storage::{BlockIdentifier, StorageError, WithTotal},
    Bytes,
};

use super::{
    schema::{
        account, account_balance, block, chain, component_balance, component_balance_default,
        component_tvl, contract_code, contract_storage, contract_storage_default, entry_point,
        entry_point_tracing_params, entry_point_tracing_params_calls_account,
        entry_point_tracing_result, extraction_state, protocol_component,
        protocol_component_has_entry_point_tracing_params, protocol_component_holds_contract,
        protocol_component_holds_token, protocol_component_uses_entry_point, protocol_state,
        protocol_state_default, protocol_system, protocol_type, token, transaction,
    },
    versioning::{StoredVersionedRow, VersionedRow},
    PostgresError, MAX_TS, MAX_VERSION_TS,
};
use crate::postgres::versioning::PartitionedVersionedRow;

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = chain)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Chain {
    pub id: i64,
    pub name: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// Represents the state of an extractor.
///
/// Note that static extraction parameters are usually defined through
/// infrastructure configuration tools (e.g., terraform). This struct only
/// maintains dynamic state that changes during runtime and has to be persisted
/// between restarts.
#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(Chain))]
#[diesel(table_name = extraction_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ExtractionState {
    /// Unique identifier for the extraction state.
    pub id: i64,

    /// Name of the extractor.
    pub name: String,

    // Version of the extractor.
    pub version: String,

    /// Chain identifier that the extractor instance is scoped to.
    pub chain_id: i64,

    /// Last block processed by the extractor.
    pub block_id: i64,

    /// Last fully extracted cursor for the corresponding substream.
    /// Can be null, indicating no cursor has been extracted yet.
    pub cursor: Option<Vec<u8>>,

    /// Additional attributes that the extractor needs to persist.
    /// Stored as a JSON binary object.
    pub attributes: Option<serde_json::Value>,

    /// Timestamp when this entry was inserted into the table.
    pub inserted_ts: NaiveDateTime,

    /// Timestamp when this entry was last modified.
    pub modified_ts: NaiveDateTime,
}

impl ExtractionState {
    /// Retrieves an `ExtractionState` based on the provided extractor name and chain ID.
    ///
    /// This method performs a join operation with the `chain` table and filters the results
    /// based on the given extractor name and chain ID. It then selects the matching
    /// `ExtractionState` and fetches the first result.
    ///
    /// # Parameters
    /// - `extractor`: The name of the extractor to filter by.
    /// - `chain_id`: The ID of the chain to filter by.
    /// - `conn`: A mutable reference to an asynchronous PostgreSQL connection.
    ///
    /// # Returns
    /// - `Ok(Some(ExtractionState))` if a matching `ExtractionState` is found.
    /// - `Ok(None)` if no matching entry is found in the database.
    /// - `Err(DieselError)` if a Diesel error occurs during the query.
    pub async fn by_name(
        extractor: &str,
        chain_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Option<(ExtractionState, Bytes)>> {
        extraction_state::table
            .inner_join(chain::table)
            .inner_join(block::table)
            .filter(extraction_state::name.eq(extractor))
            .filter(chain::id.eq(chain_id))
            .select((ExtractionState::as_select(), block::hash))
            .first::<(ExtractionState, Bytes)>(conn)
            .await
            .optional()
    }
}

#[derive(Insertable)]
#[diesel(table_name = extraction_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewExtractionState<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub chain_id: i64,
    pub block_id: i64,
    pub cursor: Option<&'a [u8]>,
    pub attributes: Option<&'a serde_json::Value>,
    pub modified_ts: NaiveDateTime,
}

#[derive(AsChangeset, Debug)]
#[diesel(table_name = extraction_state)]
pub struct ExtractionStateForm<'a> {
    pub cursor: Option<&'a [u8]>,
    pub attributes: Option<&'a serde_json::Value>,
    pub modified_ts: Option<NaiveDateTime>,
    pub block_id: Option<i64>,
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(Chain))]
#[diesel(table_name = block)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Block {
    pub id: i64,
    pub hash: BlockHash,
    pub parent_hash: BlockHash,
    pub chain_id: i64,
    pub main: bool,
    pub number: i64,
    pub ts: NaiveDateTime,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl Block {
    pub async fn by_number(
        chain: models::Chain,
        number: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Block> {
        block::table
            .inner_join(chain::table)
            .filter(block::number.eq(number))
            .filter(chain::name.eq(chain.to_string()))
            .select(Block::as_select())
            .first::<Block>(conn)
            .await
    }

    pub async fn by_hash(block_hash: &[u8], conn: &mut AsyncPgConnection) -> QueryResult<Block> {
        block::table
            .filter(block::hash.eq(block_hash))
            .select(Block::as_select())
            .first::<Block>(conn)
            .await
    }

    pub async fn most_recent(
        chain: models::Chain,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Block> {
        block::table
            .inner_join(chain::table)
            .filter(chain::name.eq(chain.to_string()))
            .order(block::number.desc())
            .select(Block::as_select())
            .first::<Block>(conn)
            .await
    }

    pub async fn by_id(id: &BlockIdentifier, conn: &mut AsyncPgConnection) -> QueryResult<Block> {
        match id {
            BlockIdentifier::Hash(hash) => Self::by_hash(hash, conn).await,
            BlockIdentifier::Number((chain, number)) => {
                Self::by_number(*chain, *number, conn).await
            }
            BlockIdentifier::Latest(chain) => Self::most_recent(*chain, conn).await,
        }
    }
}

#[derive(Insertable)]
#[diesel(table_name = block)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewBlock {
    pub hash: BlockHash,
    pub parent_hash: BlockHash,
    pub chain_id: i64,
    pub main: bool,
    pub number: i64,
    pub ts: NaiveDateTime,
}

impl NewBlock {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 6;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(Block))]
#[diesel(table_name = transaction)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Transaction {
    pub id: i64,
    pub hash: TxHash,
    pub block_id: i64,
    pub from: Address,
    pub to: Address,
    pub index: i64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl Transaction {
    pub async fn by_hash(hash: &[u8], conn: &mut AsyncPgConnection) -> QueryResult<Self> {
        transaction::table
            .filter(transaction::hash.eq(hash))
            .select(Self::as_select())
            .first::<Self>(conn)
            .await
    }

    pub async fn ids_by_hash(
        hashes: &[TxHash],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<TxHash, i64>, StorageError> {
        use super::schema::transaction::dsl::*;

        let unique_hashes: HashSet<&TxHash> = hashes.iter().collect();
        if unique_hashes.is_empty() {
            return Ok(HashMap::new());
        }

        transaction
            .filter(hash.eq_any(unique_hashes))
            .select((hash, id))
            .load::<(TxHash, i64)>(conn)
            .await
            .map_err(|e| StorageError::from(PostgresError::from(e)))
            .map(|results| results.into_iter().collect())
    }

    // fetches the transaction id, hash, index and block timestamp for a given set of hashes
    pub async fn ids_and_ts_by_hash(
        hashes: impl Iterator<Item = &TxHash>,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, Bytes, i64, NaiveDateTime)>> {
        let unique_hashes: HashSet<&TxHash> = hashes.collect();
        if unique_hashes.is_empty() {
            return Ok(vec![]);
        }
        transaction::table
            .inner_join(block::table)
            .filter(transaction::hash.eq_any(unique_hashes))
            .select((transaction::id, transaction::hash, transaction::index, block::ts))
            .get_results::<(i64, Bytes, i64, NaiveDateTime)>(conn)
            .await
    }
}

#[derive(Insertable)]
#[diesel(table_name = transaction)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewTransaction {
    pub hash: TxHash,
    pub block_id: i64,
    pub from: Address,
    pub to: Address,
    pub index: i64,
}

impl NewTransaction {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 5;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = protocol_system)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolSystem {
    pub id: i64,
    pub name: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Insertable, Debug)]
#[diesel(table_name=protocol_system)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolSystem {
    pub name: String,
}

#[derive(Debug, DbEnum, Clone, PartialEq)]
#[ExistingTypePath = "crate::postgres::schema::sql_types::FinancialType"]
pub enum FinancialType {
    Swap,
    Psm,
    Debt,
    Leverage,
}

impl From<models::FinancialType> for FinancialType {
    fn from(value: models::FinancialType) -> Self {
        match value {
            models::FinancialType::Swap => Self::Swap,
            models::FinancialType::Psm => Self::Psm,
            models::FinancialType::Debt => Self::Debt,
            models::FinancialType::Leverage => Self::Leverage,
        }
    }
}

#[derive(Debug, DbEnum, Clone, PartialEq)]
#[ExistingTypePath = "crate::postgres::schema::sql_types::ImplementationType"]
pub enum ImplementationType {
    Custom,
    Vm,
}

impl From<models::ImplementationType> for ImplementationType {
    fn from(value: models::ImplementationType) -> Self {
        match value {
            models::ImplementationType::Vm => Self::Vm,
            models::ImplementationType::Custom => Self::Custom,
        }
    }
}

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = protocol_type)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolType {
    pub id: i64,
    pub name: String,
    pub financial_type: FinancialType,
    pub attribute_schema: Option<serde_json::Value>,
    pub implementation: ImplementationType,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Identifiable, Queryable, Selectable, Debug, QueryableByName)]
#[diesel(table_name = component_balance)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(primary_key(protocol_component_id, token_id, modify_tx))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentBalance {
    pub token_id: i64,
    pub new_balance: Balance,
    pub balance_float: f64,
    pub previous_value: Balance,
    pub modify_tx: i64,
    pub protocol_component_id: i64,
    pub inserted_ts: NaiveDateTime,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

#[derive(AsChangeset, Insertable, Clone, Debug)]
#[diesel(table_name = component_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewComponentBalance {
    pub token_id: i64,
    pub new_balance: Balance,
    pub previous_value: Balance,
    pub balance_float: f64,
    pub modify_tx: i64,
    pub protocol_component_id: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

impl NewComponentBalance {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;

    pub fn new(
        token_id: i64,
        new_balance: Balance,
        balance_float: f64,
        previous_value: Option<Balance>,
        modify_tx: i64,
        protocol_component_id: i64,
        valid_from: NaiveDateTime,
    ) -> Self {
        Self {
            token_id,
            new_balance,
            previous_value: previous_value.unwrap_or_else(|| Bytes::from("0x00")),
            balance_float,
            modify_tx,
            protocol_component_id,
            valid_from,
            valid_to: MAX_TS,
        }
    }
}

impl From<ComponentBalance> for NewComponentBalance {
    fn from(value: ComponentBalance) -> Self {
        Self {
            token_id: value.token_id,
            new_balance: value.new_balance,
            previous_value: value.previous_value,
            balance_float: value.balance_float,
            modify_tx: value.modify_tx,
            protocol_component_id: value.protocol_component_id,
            valid_from: value.valid_from,
            valid_to: value.valid_to,
        }
    }
}

impl PartitionedVersionedRow for NewComponentBalance {
    type EntityId = (i64, i64);

    fn get_id(&self) -> Self::EntityId {
        (self.protocol_component_id, self.token_id)
    }

    fn get_valid_to(&self) -> NaiveDateTime {
        self.valid_to
    }

    fn get_valid_from(&self) -> NaiveDateTime {
        self.valid_from
    }

    fn archive(&mut self, next_version: &mut Self) {
        next_version.previous_value = self.new_balance.clone();
        self.valid_to = next_version.valid_from;
    }

    fn delete(&mut self, delete_version: NaiveDateTime) {
        self.valid_to = delete_version;
    }

    async fn latest_versions_by_ids(
        ids: Vec<Self::EntityId>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError>
    where
        Self: Sized,
    {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let (component_ids, token_ids): (Vec<_>, Vec<_>) = ids.into_iter().unzip();

        let results: Vec<ComponentBalance> = diesel::sql_query(
            "SELECT cb.*
             FROM component_balance_default cb
             INNER JOIN unnest($1::bigint[], $2::bigint[]) AS pairs(comp_id, tok_id)
               ON cb.protocol_component_id = pairs.comp_id
               AND cb.token_id = pairs.tok_id",
        )
        .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&component_ids)
        .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&token_ids)
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

        Ok(results
            .into_iter()
            .map(NewComponentBalance::from)
            .collect())
    }
}

#[derive(AsChangeset, Insertable, Debug)]
#[diesel(table_name = component_balance_default)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewComponentBalanceLatest {
    pub token_id: i64,
    pub new_balance: Balance,
    pub previous_value: Balance,
    pub balance_float: f64,
    pub modify_tx: i64,
    pub protocol_component_id: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

impl NewComponentBalanceLatest {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

impl From<NewComponentBalance> for NewComponentBalanceLatest {
    fn from(value: NewComponentBalance) -> Self {
        Self {
            token_id: value.token_id,
            new_balance: value.new_balance,
            previous_value: value.previous_value,
            balance_float: value.balance_float,
            modify_tx: value.modify_tx,
            protocol_component_id: value.protocol_component_id,
            valid_from: value.valid_from,
            valid_to: MAX_TS,
        }
    }
}

#[derive(AsChangeset, Insertable)]
#[diesel(table_name = protocol_type)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolType {
    pub name: String,
    pub financial_type: FinancialType,
    pub attribute_schema: Option<serde_json::Value>,
    pub implementation: ImplementationType,
}
impl ProtocolType {
    pub async fn id_by_name(name: &String, conn: &mut AsyncPgConnection) -> QueryResult<i64> {
        protocol_type::table
            .filter(protocol_type::name.eq(name))
            .select(protocol_type::id)
            .first::<i64>(conn)
            .await
    }
}
#[derive(Identifiable, Queryable, Associations, Selectable, Clone, Debug, PartialEq)]
#[diesel(belongs_to(Chain))]
#[diesel(belongs_to(ProtocolType))]
#[diesel(belongs_to(ProtocolSystem))]
#[diesel(belongs_to(Transaction, foreign_key = creation_tx))]
#[diesel(table_name = protocol_component)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponent {
    pub id: i64,
    pub chain_id: i64,
    pub external_id: String,
    pub protocol_type_id: i64,
    pub protocol_system_id: i64,
    pub attributes: Option<serde_json::Value>,
    pub created_at: NaiveDateTime,
    pub deleted_at: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
    pub creation_tx: i64,
    pub deletion_tx: Option<i64>,
}

#[derive(Insertable, AsChangeset, Debug)]
#[diesel(belongs_to(Chain))]
#[diesel(belongs_to(ProtocolType))]
#[diesel(belongs_to(ProtocolSystem))]
#[diesel(table_name = protocol_component)]
pub struct NewProtocolComponent {
    pub external_id: String,
    pub chain_id: i64,
    pub protocol_type_id: i64,
    pub protocol_system_id: i64,
    pub creation_tx: i64,
    pub created_at: NaiveDateTime,
    pub attributes: Option<serde_json::Value>,
}

impl NewProtocolComponent {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;

    pub fn new(
        external_id: &str,
        chain_id: i64,
        protocol_type_id: i64,
        protocol_system_id: i64,
        creation_tx: i64,
        created_at: NaiveDateTime,
        attributes: &HashMap<String, Bytes>,
    ) -> Self {
        let attributes =
            (!attributes.is_empty()).then(|| serde_json::to_value(attributes).unwrap());
        Self {
            external_id: external_id.to_string(),
            chain_id,
            protocol_type_id,
            protocol_system_id,
            creation_tx,
            created_at,
            attributes,
        }
    }
}

impl ProtocolComponent {
    pub async fn ids_by_external_ids(
        external_ids: impl Iterator<Item = &str>,
        chain_db_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, String)>> {
        let unique_ids: HashSet<&str> = external_ids.collect();
        if unique_ids.is_empty() {
            return Ok(vec![]);
        }
        protocol_component::table
            .filter(protocol_component::external_id.eq_any(unique_ids))
            .filter(protocol_component::chain_id.eq(chain_db_id))
            .select((protocol_component::id, protocol_component::external_id))
            .get_results::<(i64, String)>(conn)
            .await
    }

    pub async fn id_by_external_id(
        external_id: &str,
        chain_db_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<i64> {
        protocol_component::table
            .filter(protocol_component::external_id.eq(external_id))
            .filter(protocol_component::chain_id.eq(chain_db_id))
            .select(protocol_component::id)
            .first::<i64>(conn)
            .await
    }
}

#[derive(Insertable)]
#[diesel(table_name = protocol_component_holds_contract)]
pub struct NewProtocolComponentHoldsContract {
    pub protocol_component_id: i64,
    pub contract_code_id: i64,
}

#[derive(Identifiable, Queryable, Associations, Selectable, Clone, Debug, QueryableByName)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(table_name = protocol_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(primary_key(protocol_component_id, attribute_name, modify_tx))]
pub struct ProtocolState {
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl ProtocolState {
    /// Used to fetch the full state of a component at a given version, filtered by component ids.
    ///
    /// Retrieves all matching protocol states and their component id, filtered by component id.
    /// If no version is provided, the latest state is returned. The results are grouped by
    /// component id to allow for easy state reconstruction. It can be trusted that all state
    /// updates for a given component are sequential.
    pub async fn by_id(
        component_ids: &[&str],
        chain_id: &i64,
        version_ts: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> WithTotal<QueryResult<Vec<(Self, ComponentId)>>> {
        // Subquery to get distinct component external IDs based on pagination
        let mut component_query = protocol_component::table
            .filter(protocol_component::external_id.eq_any(component_ids))
            .filter(protocol_component::chain_id.eq(chain_id))
            .select(protocol_component::id)
            .into_boxed();

        // Apply pagination and fetch total count
        let count: Option<i64> = if let Some(pagination) = pagination_params {
            component_query = component_query
                .limit(pagination.page_size)
                .offset(pagination.page * pagination.page_size);

            // Count the total number of matching components
            Some(
                protocol_component::table
                    .filter(protocol_component::external_id.eq_any(component_ids))
                    .filter(protocol_component::chain_id.eq(chain_id))
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .unwrap_or(0),
            )
        } else {
            None
        };

        // Main query to get ProtocolStates for the selected component external IDs
        let mut query = protocol_state::table
            .inner_join(
                protocol_component::table
                    .on(protocol_state::protocol_component_id.eq(protocol_component::id)),
            )
            .filter(protocol_component::id.eq_any(component_query))
            .filter(protocol_state::valid_to.gt(version_ts.unwrap_or(*MAX_VERSION_TS)))
            .into_boxed();

        // Apply additional filtering by timestamp if provided
        if let Some(ts) = version_ts {
            query = query.filter(protocol_state::valid_from.le(ts));
        }

        // Fetch the results
        let res = query
            .order_by(protocol_component::external_id)
            .select((Self::as_select(), protocol_component::external_id))
            .get_results::<(Self, String)>(conn)
            .await;

        WithTotal { entity: res, total: count }
    }

    /// Used to fetch the full state of a component at a given version, filtered by protocol system
    /// and optionally component ids.
    ///
    /// Retrieves all matching protocol states and their component id, filtered by protocol system
    /// and optionally component ids.
    /// If no version is provided, the latest state is returned. The results are grouped by
    /// component id to allow for easy state reconstruction. It can be trusted that all state
    /// updates for a given component are sequential.
    ///
    /// Note - follows the same logic as by_ids, but filters by protocol system and optionally by
    /// component ids.
    pub async fn by_protocol(
        component_ids: Option<&[&str]>,
        system: &str,
        chain_id: &i64,
        version_ts: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> WithTotal<QueryResult<Vec<(Self, ComponentId)>>> {
        // Subquery to get distinct component IDs based on pagination
        let mut component_query = protocol_component::table
            .inner_join(
                protocol_system::table
                    .on(protocol_component::protocol_system_id.eq(protocol_system::id)),
            )
            .filter(protocol_system::name.eq(system))
            .filter(protocol_component::chain_id.eq(chain_id))
            .select(protocol_component::id)
            .into_boxed();

        if let Some(ids) = component_ids {
            component_query = component_query.filter(protocol_component::external_id.eq_any(ids));
        }

        // Apply pagination and fetch total count
        let count: Option<i64> = if let Some(pagination) = pagination_params {
            component_query = component_query
                .order_by(protocol_component::id)
                .limit(pagination.page_size)
                .offset(pagination.page * pagination.page_size);

            Some(
                protocol_component::table
                    .inner_join(
                        protocol_system::table
                            .on(protocol_component::protocol_system_id.eq(protocol_system::id)),
                    )
                    .filter(protocol_system::name.eq(system))
                    .filter(protocol_component::chain_id.eq(chain_id))
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .unwrap_or(0),
            )
        } else {
            None
        };

        // Main query to get ProtocolStates for the selected components
        let mut query = protocol_state::table
            .inner_join(
                protocol_component::table
                    .on(protocol_state::protocol_component_id.eq(protocol_component::id)),
            )
            .filter(protocol_component::id.eq_any(component_query))
            .filter(protocol_state::valid_to.gt(version_ts.unwrap_or(*MAX_VERSION_TS)))
            .into_boxed();

        // Apply additional filtering by timestamp if provided
        if let Some(ts) = version_ts {
            query = query.filter(protocol_state::valid_from.le(ts));
        }

        // Fetch the results
        let res = query
            .order_by(protocol_state::protocol_component_id)
            .select((Self::as_select(), protocol_component::external_id))
            .get_results::<(Self, String)>(conn)
            .await;

        WithTotal { entity: res, total: count }
    }

    /// Used to fetch the full state of a component at a given version, filtered by chain.
    ///
    /// Retrieves all matching protocol states and their component id, filtered by chain.
    /// If no version is provided, the latest state is returned. The results are grouped by
    /// component id to allow for easy state reconstruction. It can be trusted that all state
    /// updates for a given component are sequential.
    ///
    /// Note - follows the same logic as by_ids, but filters by chain instead of component ids.
    pub async fn by_chain(
        chain_id: &i64,
        version_ts: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> WithTotal<QueryResult<Vec<(Self, ComponentId)>>> {
        let mut count_query = protocol_component::table
            .filter(protocol_component::chain_id.eq(chain_id))
            .filter(exists(
                protocol_state::table
                    .filter(protocol_state::protocol_component_id.eq(protocol_component::id))
                    .filter(protocol_state::valid_to.gt(version_ts.unwrap_or(*MAX_VERSION_TS))),
            ))
            .into_boxed();

        // Step 1: Get IDs of components that have associated states
        let mut component_ids_query = protocol_component::table
            .filter(protocol_component::chain_id.eq(chain_id))
            .filter(exists(
                protocol_state::table
                    .filter(protocol_state::protocol_component_id.eq(protocol_component::id))
                    .filter(protocol_state::valid_to.gt(version_ts.unwrap_or(*MAX_VERSION_TS))),
            ))
            .select(protocol_component::id)
            .order_by(protocol_component::id)
            .into_boxed();

        // Step 2: Conditionally apply the `valid_from` filter within EXISTS
        if let Some(ts) = version_ts {
            component_ids_query = component_ids_query.filter(exists(
                protocol_state::table
                    .filter(protocol_state::protocol_component_id.eq(protocol_component::id))
                    .filter(protocol_state::valid_from.le(ts)),
            ));

            count_query = count_query.filter(exists(
                protocol_state::table
                    .filter(protocol_state::protocol_component_id.eq(protocol_component::id))
                    .filter(protocol_state::valid_from.le(ts)),
            ));
        }

        // Step 3: Apply pagination and fetch total count
        let count: Option<i64> = if let Some(pagination) = pagination_params {
            component_ids_query = component_ids_query
                .limit(pagination.page_size)
                .offset(pagination.page * pagination.page_size);
            Some(
                count_query
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .unwrap_or(0),
            )
        } else {
            None
        };

        // Fetch the component IDs
        let component_ids = component_ids_query
            .load::<i64>(conn)
            .await;

        let component_ids = match component_ids {
            Ok(ids) => ids,
            Err(err) => {
                return WithTotal { entity: Err(err), total: None };
            }
        };

        // If no components found, return empty result
        if component_ids.is_empty() {
            return WithTotal { entity: Ok(Vec::new()), total: Some(0) };
        }

        // Step 4: Fetch all ProtocolStates for the selected components
        let mut query = protocol_state::table
            .inner_join(
                protocol_component::table
                    .on(protocol_state::protocol_component_id.eq(protocol_component::id)),
            )
            .filter(protocol_component::id.eq_any(&component_ids))
            .filter(protocol_state::valid_to.gt(version_ts.unwrap_or(*MAX_VERSION_TS)))
            .into_boxed();

        // Apply additional filtering by timestamp if provided
        if let Some(ts) = version_ts {
            query = query.filter(protocol_state::valid_from.le(ts));
        }

        // Fetch the results
        let res = query
            .order_by(protocol_state::protocol_component_id)
            .select((Self::as_select(), protocol_component::external_id))
            .get_results::<(Self, String)>(conn)
            .await;

        WithTotal { entity: res, total: count }
    }

    /// Used to fetch all protocol state changes within the given timeframe.
    ///
    /// Retrieves all state updates applied after start_ts and still valid by end_ts, filtered by
    /// chain. Please note - this function is intended to be used to fetch forward changes/deltas.
    /// The results are grouped by component id to allow for easy state reconstruction. It can be
    /// trusted that all state updates for a given component are together and only one update per
    /// component's attribute is returned.
    ///
    /// Note: If the attribute was updated twice within the timeframe, only the one that is still
    /// valid at end is returned.
    pub async fn forward_deltas_by_chain(
        chain_id: i64,
        start_ts: NaiveDateTime,
        end_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(Self, ComponentId)>> {
        protocol_state::table
            .inner_join(protocol_component::table)
            .filter(protocol_component::chain_id.eq(chain_id))
            // only consider attributes that were updated after start_ts and before end_ts
            .filter(protocol_state::valid_from.gt(start_ts))
            .filter(protocol_state::valid_from.le(end_ts))
            // only consider attributes that are still valid by end_ts
            .filter(protocol_state::valid_to.gt(end_ts))
            .order_by(protocol_state::protocol_component_id)
            .select((Self::as_select(), protocol_component::external_id))
            .get_results::<(Self, String)>(conn)
            .await
    }

    /// Used to detect attributes that were deleted within a given timeframe.
    ///
    /// Retrieves all component-attribute pairs that have a valid version at start_ts and have no
    /// valid version at end_ts. The results are grouped by component id to allow for easy state
    /// reconstruction. It can be trusted that all state updates for a given component are together.
    pub async fn deleted_attributes_by_chain(
        chain_id: i64,
        start_ts: NaiveDateTime,
        end_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(ComponentId, AttrStoreKey)>> {
        // subquery to exclude entities that have a valid version at end_ts (haven't been deleted)
        // TODO: use parameter binding instead of string interpolation
        let sub_query = format!("NOT EXISTS (
                                SELECT 1 FROM protocol_state ps2
                                WHERE ps2.protocol_component_id = protocol_state.protocol_component_id
                                AND ps2.attribute_name = protocol_state.attribute_name
                                AND ps2.valid_from <= '{end_ts}'
                                AND ps2.valid_to > '{end_ts}'
                            )");

        // query for all state updates that have a valid_to between start_ts and end_ts (potentially
        // have been deleted) and filter it by the subquery for attributes that exist at end_ts
        // (were therefore not deleted)
        // i.e. potentially_deleted - not_deleted = deleted
        protocol_state::table
            .inner_join(protocol_component::table)
            .filter(protocol_component::chain_id.eq(chain_id))
            // validity ends during the timeframe (potentially deleted)
            .filter(protocol_state::valid_to.le(end_ts))
            .filter(protocol_state::valid_to.gt(start_ts))
            // subquery to remove those that weren't deleted (valid version exists at end_ts)
            .filter(sql::<Bool>(&sub_query))
            .order_by(protocol_state::protocol_component_id)
            .select((protocol_component::external_id, protocol_state::attribute_name))
            .get_results::<(String, String)>(conn)
            .await
    }

    /// Used to retrieve the original state of all component attributes that were updated within the
    /// given timeframe.
    ///
    /// Retrieves the previous values (reverse deltas) of all state updates that were applied after
    /// target_ts and before start_ts, filtered by chain. Please note - this function is intended to
    /// be used to fetch beckwards changes/revert deltas. Start_ts represents the more recent ts
    /// and target_ts represents the ts of the block to be reverted to. The results are grouped by
    /// component id to allow for easy state reconstruction.
    pub async fn reverse_delta_by_chain(
        chain_id: i64,
        start_ts: NaiveDateTime,
        target_ts: NaiveDateTime,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(ComponentId, AttrStoreKey, Option<StoreVal>)>> {
        let query = protocol_state::table
            .inner_join(protocol_component::table)
            .inner_join(transaction::table.on(transaction::id.eq(protocol_state::modify_tx)))
            .filter(protocol_component::chain_id.eq(chain_id));

        // We query all states that were added between the start and target timestamps, filtered by
        // chain. We group it by component and attribute and order it by tx. Then we deduplicate by
        // taking the first row per group. This gives us the first state update for each
        // component-attribute pair. Finally, we return the component id, attribute name and
        // previous value for each component-attribute pair. Note, previous values are null for
        // state updates where they are the first update of that attribute (attribute creation).
        let reverted_query = query
            .filter(protocol_state::valid_from.gt(target_ts))
            .filter(protocol_state::valid_from.le(start_ts))
            .order_by((
                protocol_state::protocol_component_id,
                protocol_state::attribute_name,
                transaction::block_id,
                transaction::index,
            ))
            .select(
                sql::<(sql_types::Text, sql_types::Text, sql_types::Nullable<sql_types::Bytea>)>(
                    "external_id, attribute_name, previous_value AS value",
                ),
            )
            .distinct_on((protocol_state::protocol_component_id, protocol_state::attribute_name));

        // subquery to exclude entities that have a valid version at start_ts (weren't deleted)
        // TODO: use parameter binding instead of string interpolation
        let sub_query = format!("NOT EXISTS (
                                SELECT 1 FROM protocol_state ps2
                                WHERE ps2.protocol_component_id = protocol_state.protocol_component_id
                                AND ps2.attribute_name = protocol_state.attribute_name
                                AND ps2.valid_from <= '{start_ts}'
                                AND ps2.valid_to > '{start_ts}'
                            )");

        // We query all states that were deleted between the start and target timestamps. Deleted
        // states need to be reinstated so we return the component id, attribute name and latest
        // value of each component-attribute pair here.
        let deleted_query = query
            // validity ends during the timeframe (potentially deleted)
            .filter(protocol_state::valid_to.le(start_ts))
            .filter(protocol_state::valid_to.gt(target_ts))
            // validity starts before the timeframe (is valid at target_ts)
            .filter(protocol_state::valid_from.le(target_ts))
            // subquery to remove those that weren't deleted (valid version exists at start_ts)
            .filter(sql::<Bool>(&sub_query))
            .order_by((protocol_state::protocol_component_id, protocol_state::attribute_name))
            .select(
                sql::<(sql_types::Text, sql_types::Text, sql_types::Nullable<sql_types::Bytea>)>(
                    "external_id, attribute_name, attribute_value AS value",
                ),
            );

        // query and merge results for both reverted updates and deleted states
        reverted_query
            .union_all(deleted_query)
            .get_results::<(String, String, Option<Bytes>)>(conn)
            .await
    }
}

#[derive(Insertable, Clone, Debug, PartialEq)]
#[diesel(table_name = protocol_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolState {
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

impl NewProtocolState {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

impl From<ProtocolState> for NewProtocolState {
    fn from(value: ProtocolState) -> Self {
        Self {
            protocol_component_id: value.protocol_component_id,
            attribute_name: value.attribute_name,
            attribute_value: value.attribute_value,
            previous_value: value.previous_value,
            modify_tx: value.modify_tx,
            valid_from: value.valid_from,
            valid_to: value.valid_to,
        }
    }
}

impl PartitionedVersionedRow for NewProtocolState {
    type EntityId = (i64, String);

    fn get_id(&self) -> Self::EntityId {
        (self.protocol_component_id, self.attribute_name.clone())
    }

    fn get_valid_to(&self) -> NaiveDateTime {
        self.valid_to
    }

    fn get_valid_from(&self) -> NaiveDateTime {
        self.valid_from
    }

    fn archive(&mut self, next_version: &mut Self) {
        next_version.previous_value = Some(self.attribute_value.clone());
        self.valid_to = next_version.valid_from;
    }

    fn delete(&mut self, delete_version: NaiveDateTime) {
        self.valid_to = delete_version;
    }

    async fn latest_versions_by_ids(
        ids: Vec<Self::EntityId>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError>
    where
        Self: Sized,
    {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let (pc_ids, attr_names): (Vec<_>, Vec<_>) = ids.into_iter().unzip();

        let results: Vec<ProtocolState> = diesel::sql_query(
            "SELECT ps.* 
             FROM protocol_state_default ps
             INNER JOIN unnest($1::bigint[], $2::text[]) AS pairs(pc_id, attr_name)
               ON ps.protocol_component_id = pairs.pc_id 
               AND ps.attribute_name = pairs.attr_name",
        )
        .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&pc_ids)
        .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(&attr_names)
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

        Ok(results
            .into_iter()
            .map(NewProtocolState::from)
            .collect())
    }
}

impl NewProtocolState {
    pub fn new(
        protocol_component_id: i64,
        attribute_name: &str,
        attribute_value: &Bytes,
        modify_tx: i64,
        valid_from: NaiveDateTime,
    ) -> Self {
        Self {
            protocol_component_id,
            attribute_name: attribute_name.to_string(),
            attribute_value: attribute_value.clone(),
            previous_value: None,
            modify_tx,
            valid_from,
            valid_to: MAX_TS,
        }
    }
}

#[derive(Insertable, Clone, Debug)]
#[diesel(table_name = protocol_state_default)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolStateLatest {
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

impl NewProtocolStateLatest {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

impl From<NewProtocolState> for NewProtocolStateLatest {
    fn from(value: NewProtocolState) -> Self {
        Self {
            protocol_component_id: value.protocol_component_id,
            attribute_name: value.attribute_name,
            attribute_value: value.attribute_value,
            previous_value: value.previous_value,
            modify_tx: value.modify_tx,
            valid_from: value.valid_from,
            valid_to: MAX_TS,
        }
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug, PartialEq)]
#[diesel(belongs_to(Chain))]
#[diesel(belongs_to(Transaction, foreign_key = creation_tx))]
#[diesel(table_name = account)]
#[diesel(check_for_backend(diesel::pg::Pg))]
/// Represents an account on a blockchain.
///
/// An `Account` is identified by its blockchain (`Chain`) and address (`H160`). It may have a
/// descriptive `title` and contains information about storage slots, balance, associated code
/// (bytecode), code hash, and transaction hashes related to balance modification, code
/// modification, and optional creation. Additional information about accounts.
/// - A Contract is also an Account, but an Account is not necessarily a Contract.
/// - An account is considered a contract if it has associated code.
pub struct Account {
    pub id: i64,
    pub title: String,
    pub address: Address,
    pub chain_id: i64,
    pub creation_tx: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
    pub deleted_at: Option<NaiveDateTime>,
    pub deletion_tx: Option<i64>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl Account {
    pub async fn by_id(
        account_id: &ContractId,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Account> {
        account::table
            .inner_join(chain::table)
            .filter(account::address.eq(&account_id.address))
            .filter(chain::name.eq(account_id.chain.to_string()))
            .select(Account::as_select())
            .first::<Account>(conn)
            .await
    }

    /// retrieves an account by address
    pub async fn by_address(
        address: &Address,
        chain_db_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<Self>> {
        account::table
            .filter(account::address.eq(address))
            .filter(account::chain_id.eq(chain_db_id))
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
    }

    /// retrieves account ids by addresses
    pub async fn ids_by_addresses(
        addresses: impl Iterator<Item = &Address>,
        chain_db_id: i64,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, Address)>> {
        let unique_addresses: HashSet<&Address> = addresses.collect();
        if unique_addresses.is_empty() {
            return Ok(vec![]);
        }
        account::table
            .filter(account::address.eq_any(unique_addresses))
            .filter(account::chain_id.eq(chain_db_id))
            .select((account::id, account::address))
            .get_results::<(i64, Address)>(conn)
            .await
    }

    pub async fn get_addresses_by_id(
        ids: impl Iterator<Item = &i64>,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<(i64, Address)>> {
        let unique_ids: HashSet<&i64> = ids.collect();
        if unique_ids.is_empty() {
            return Ok(vec![]);
        }
        account::table
            .filter(account::id.eq_any(unique_ids))
            .select((account::id, account::address))
            .get_results::<(i64, Address)>(conn)
            .await
    }
}

#[derive(AsChangeset, Insertable, Debug)]
#[diesel(table_name = account)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccount<'a> {
    pub title: &'a str,
    pub address: &'a [u8],
    pub chain_id: i64,
    pub creation_tx: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
    pub deleted_at: Option<NaiveDateTime>,
}

impl<'a> NewAccount<'a> {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 6;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug, PartialEq)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Token {
    pub id: i64,
    pub account_id: i64,
    pub symbol: String,
    pub decimals: i32,
    pub tax: i64,
    pub gas: Vec<Option<i64>>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
    pub quality: i32,
}

#[derive(AsChangeset, Insertable, Debug)]
#[diesel(table_name = token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewToken {
    pub account_id: i64,
    pub symbol: String,
    pub decimals: i32,
    pub tax: i64,
    pub gas: Vec<Option<i64>>,
    pub quality: i32,
}

impl NewToken {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 6;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;

    pub fn from_token(account_id: i64, token: &models::token::Token) -> Self {
        Self {
            account_id,
            symbol: token.symbol.clone(),
            decimals: token.decimals as i32,
            tax: token.tax as i64,
            gas: token
                .gas
                .iter()
                .map(|g| g.map(|u| u as i64))
                .collect(),
            quality: token.quality as i32,
        }
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug, QueryableByName)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = account_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AccountBalance {
    pub id: i64,
    pub balance: Balance,
    pub account_id: i64,
    pub token_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl AccountBalance {
    /// retrieves all balances from a certain account
    pub async fn all_versions(
        address: &Address,
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<Self>> {
        account_balance::table
            .inner_join(account::table)
            .filter(account::address.eq(address))
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
    }
}

#[async_trait]
impl StoredVersionedRow for AccountBalance {
    type EntityId = (i64, i64);
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        (self.account_id, self.token_id)
    }

    fn get_valid_from(&self) -> NaiveDateTime {
        self.valid_from
    }

    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        let (accounts, tokens): (Vec<_>, Vec<_>) = ids.into_iter().unzip();

        if accounts.is_empty() {
            return Ok(vec![]);
        }

        let results: Vec<AccountBalance> = diesel::sql_query(
            "SELECT ab.*
             FROM account_balance ab
             INNER JOIN unnest($1::bigint[], $2::bigint[]) AS pairs(acc_id, tok_id)
               ON ab.account_id = pairs.acc_id
               AND ab.token_id = pairs.tok_id
             WHERE ab.valid_to IS NULL",
        )
        .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&accounts)
        .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&tokens)
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

        Ok(results
            .into_iter()
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "account_balance"
    }
}

#[derive(Insertable, Debug, PartialEq, Clone)]
#[diesel(table_name = account_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccountBalance {
    pub balance: Balance,
    pub account_id: i64,
    pub token_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

impl NewAccountBalance {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 6;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

impl VersionedRow for NewAccountBalance {
    type SortKey = (i64, NaiveDateTime, i64);
    type EntityId = (i64, i64);
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        (self.account_id, self.token_id)
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = contract_code)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractCode {
    pub id: i64,
    pub code: Code,
    pub hash: CodeHash,
    pub account_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl ContractCode {
    /// retrieves all codes from a certain account
    pub async fn all_versions(
        address: &[u8],
        conn: &mut AsyncPgConnection,
    ) -> QueryResult<Vec<Self>> {
        contract_code::table
            .inner_join(account::table)
            .filter(account::address.eq(address))
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
    }
}

#[async_trait]
impl StoredVersionedRow for ContractCode {
    type EntityId = i64;
    type PrimaryKey = i64;
    type Version = NaiveDateTime;

    fn get_pk(&self) -> Self::PrimaryKey {
        self.id
    }

    fn get_entity_id(&self) -> Self::EntityId {
        self.account_id
    }

    fn get_valid_from(&self) -> NaiveDateTime {
        self.valid_from
    }

    async fn latest_versions_by_ids<I: IntoIterator<Item = Self::EntityId> + Send + Sync>(
        ids: I,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Box<Self>>, StorageError> {
        Ok(contract_code::table
            .filter(
                contract_code::account_id
                    .eq_any(ids)
                    .and(contract_code::valid_to.is_null()),
            )
            .select(Self::as_select())
            .get_results::<Self>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .map(Box::new)
            .collect())
    }

    fn table_name() -> &'static str {
        "contract_code"
    }
}

#[derive(Insertable, Debug)]
#[diesel(table_name = contract_code)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewContractCode<'a> {
    pub code: &'a Code,
    pub hash: CodeHash,
    pub account_id: i64,
    pub modify_tx: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: Option<NaiveDateTime>,
}

#[allow(clippy::needless_lifetimes)]
impl<'a> VersionedRow for NewContractCode<'a> {
    type SortKey = (i64, NaiveDateTime, i64);
    type EntityId = i64;
    type Version = NaiveDateTime;

    fn get_entity_id(&self) -> Self::EntityId {
        self.account_id
    }

    fn set_valid_to(&mut self, end_version: Self::Version) {
        self.valid_to = Some(end_version);
    }

    fn get_valid_from(&self) -> Self::Version {
        self.valid_from
    }
}

// theoretically this struct could also simply reference the original struct.
// Unfortunately that really doesn't play nicely with async_trait on the Gateway
// and makes the types a lot more complicted. Once the system is up and running
// this could be improved though.
pub struct NewContract {
    pub title: String,
    pub address: Address,
    pub chain_id: i64,
    pub creation_tx: Option<i64>,
    pub created_at: Option<NaiveDateTime>,
    #[allow(dead_code)]
    pub deleted_at: Option<NaiveDateTime>,
}

impl NewContract {
    pub fn new_account(&self) -> NewAccount<'_> {
        NewAccount {
            title: &self.title,
            address: &self.address,
            chain_id: self.chain_id,
            creation_tx: self.creation_tx,
            created_at: self.created_at,
            deleted_at: None,
        }
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug, QueryableByName)]
#[diesel(belongs_to(Account))]
#[diesel(table_name = contract_storage)]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(primary_key(account_id, slot, modify_tx))]
pub struct ContractStorage {
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub account_id: i64,
    pub modify_tx: i64,
    pub ordinal: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = contract_storage)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewSlot {
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub account_id: i64,
    pub modify_tx: i64,
    pub ordinal: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

impl PartitionedVersionedRow for NewSlot {
    type EntityId = (i64, Bytes);

    fn get_id(&self) -> Self::EntityId {
        (self.account_id, self.slot.clone())
    }

    fn get_valid_to(&self) -> NaiveDateTime {
        self.valid_to
    }

    fn get_valid_from(&self) -> NaiveDateTime {
        self.valid_from
    }

    fn archive(&mut self, next_version: &mut Self) {
        next_version
            .previous_value
            .clone_from(&self.value);
        self.valid_to = next_version.valid_from;
    }

    fn delete(&mut self, delete_version: NaiveDateTime) {
        self.valid_to = delete_version
    }

    async fn latest_versions_by_ids(
        ids: Vec<Self::EntityId>,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<Self>, StorageError>
    where
        Self: Sized,
    {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let (accounts, slots): (Vec<_>, Vec<_>) = ids.into_iter().unzip();

        let results: Vec<ContractStorage> = diesel::sql_query(
            "SELECT cs.*
             FROM contract_storage_default cs
             INNER JOIN unnest($1::bigint[], $2::bytea[]) AS pairs(acc_id, slot_val)
               ON cs.account_id = pairs.acc_id
               AND cs.slot = pairs.slot_val",
        )
        .bind::<diesel::sql_types::Array<diesel::sql_types::BigInt>, _>(&accounts)
        .bind::<diesel::sql_types::Array<diesel::sql_types::Binary>, _>(&slots)
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

        Ok(results
            .into_iter()
            .map(NewSlot::from)
            .collect())
    }
}

impl From<ContractStorage> for NewSlot {
    fn from(value: ContractStorage) -> Self {
        Self {
            slot: value.slot,
            value: value.value,
            previous_value: value.previous_value,
            account_id: value.account_id,
            modify_tx: value.modify_tx,
            ordinal: value.ordinal,
            valid_from: value.valid_from,
            valid_to: value.valid_to,
        }
    }
}

#[derive(Insertable, Debug)]
#[diesel(table_name = contract_storage_default)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewSlotLatest {
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub account_id: i64,
    pub modify_tx: i64,
    pub ordinal: i64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
}

impl From<NewSlot> for NewSlotLatest {
    fn from(value: NewSlot) -> Self {
        Self {
            slot: value.slot,
            value: value.value,
            previous_value: value.previous_value,
            account_id: value.account_id,
            modify_tx: value.modify_tx,
            ordinal: value.ordinal,
            valid_from: value.valid_from,
            valid_to: MAX_TS,
        }
    }
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(primary_key(protocol_component_id, token_id))]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(belongs_to(Token))]
#[diesel(table_name = protocol_component_holds_token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolHoldsToken {
    protocol_component_id: i64,
    token_id: i64,
    #[allow(dead_code)]
    token_index: i16,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Insertable)]
#[diesel(table_name = protocol_component_holds_token)]
pub struct NewProtocolComponentHoldsToken {
    pub protocol_component_id: i64,
    pub token_id: i64,
    pub token_index: i16,
}

#[derive(Identifiable, Queryable, Associations, Selectable, Debug)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(table_name = component_tvl)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentTVL {
    id: i64,
    protocol_component_id: i64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl ComponentTVL {
    pub fn upsert_many(new_tvl_values: &HashMap<i64, f64>) -> BoxedSqlQuery<'static, Pg, SqlQuery> {
        // Generate bind parameter 2-tuples the result will look like '($1, $2), ($3, $4), ...'
        // These are later subsituted with the primary key and valid to values.
        let bind_params = (1..=new_tvl_values.len() * 2)
            .map(|i| if i.is_multiple_of(2) { format!("${i}") } else { format!("(${i}") })
            .collect::<Vec<String>>()
            .chunks(2)
            .map(|chunk| chunk.join(", ") + ")")
            .collect::<Vec<String>>()
            .join(", ");
        let query_tmpl = format!(
            r#"
            INSERT INTO component_tvl (protocol_component_id, tvl)
            VALUES {bind_params}
            ON CONFLICT (protocol_component_id) 
            DO UPDATE SET tvl = EXCLUDED.tvl;
            "#
        );
        let mut q = sql_query(query_tmpl).into_boxed();
        for (k, v) in new_tvl_values.iter() {
            q = q.bind::<BigInt, _>(*k);
            q = q.bind::<Double, _>(*v);
        }
        q
    }
}

#[derive(Debug, DbEnum, Clone, PartialEq, Eq, Hash)]
#[ExistingTypePath = "crate::postgres::schema::sql_types::EntryPointTracingType"]
pub enum EntryPointTracingType {
    RpcTracer,
}

impl From<&models::blockchain::TracingParams> for EntryPointTracingType {
    fn from(value: &models::blockchain::TracingParams) -> Self {
        match value {
            models::blockchain::TracingParams::RPCTracer(_) => Self::RpcTracer,
        }
    }
}

#[derive(Identifiable, Queryable, Selectable)]
#[diesel(table_name = entry_point)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPoint {
    pub id: i64,
    pub external_id: String,
    pub target: Bytes,
    pub signature: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl From<EntryPoint> for models::blockchain::EntryPoint {
    fn from(value: EntryPoint) -> Self {
        Self {
            external_id: value.external_id.clone(),
            target: value.target.clone(),
            signature: value.signature.clone(),
        }
    }
}

impl EntryPoint {
    /// Retrieves the database ids of entry points from a list of external ids.
    pub(crate) async fn ids_by_external_ids(
        external_ids: &[String],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<String, i64>, StorageError> {
        use crate::postgres::schema::entry_point::dsl::*;

        let existing_entries = entry_point
            .filter(external_id.eq_any(external_ids))
            .select((id, external_id))
            .load::<(i64, String)>(conn)
            .await
            .map_err(PostgresError::from)?;

        Ok(existing_entries
            .into_iter()
            .map(|(ep_id, ext_id)| (ext_id, ep_id))
            .collect::<HashMap<_, _>>())
    }

    /// Retrieves the database id of an entry point from a target address and function signature.
    #[allow(dead_code)]
    pub(crate) async fn id_by_target_and_signature(
        target_: &Bytes,
        signature_: &String,
        conn: &mut AsyncPgConnection,
    ) -> Result<i64, StorageError> {
        use crate::postgres::schema::entry_point::dsl::*;

        Ok(entry_point
            .select(id)
            .filter(target.eq(target_))
            .filter(signature.eq(signature_))
            .first::<i64>(conn)
            .await
            .map_err(PostgresError::from)?)
    }
}

#[derive(Insertable, AsChangeset, Eq, Hash, PartialEq)]
#[diesel(table_name = entry_point)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPoint {
    pub external_id: String,
    pub target: Bytes,
    pub signature: String,
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(EntryPoint))]
#[diesel(table_name = entry_point_tracing_params)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPointTracingParams {
    pub id: i64,
    pub entry_point_id: i64,
    pub tracing_type: EntryPointTracingType,
    pub data: Option<serde_json::Value>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

impl From<&EntryPointTracingParams> for models::blockchain::TracingParams {
    fn from(value: &EntryPointTracingParams) -> Self {
        match value.tracing_type {
            EntryPointTracingType::RpcTracer => {
                let rpc_tracer: models::blockchain::RPCTracerParams =
                    serde_json::from_value(value.data.clone().unwrap()).unwrap();
                models::blockchain::TracingParams::RPCTracer(rpc_tracer)
            }
        }
    }
}

impl EntryPointTracingParams {
    /// Retrieves the database id of an entry point tracing params from an
    /// `EntryPointWithTracingParams`.
    #[allow(dead_code)]
    pub(crate) async fn id_from_entry_point_with_tracing_params(
        entry_point: &EntryPointWithTracingParamsCommon,
        conn: &mut AsyncPgConnection,
    ) -> Result<i64, StorageError> {
        let tracing_type = EntryPointTracingType::from(&entry_point.params);
        let data = match &entry_point.params {
            EntryPointTracingParamsCommon::RPCTracer(rpc_tracer) => {
                serde_json::to_value(rpc_tracer).map_err(|e| {
                    StorageError::Unexpected(format!(
                        "Failed to serialize RPCTracerEntryPoint: {e}"
                    ))
                })?
            }
        };

        let id_ = super::schema::entry_point_tracing_params::table
            .inner_join(super::schema::entry_point::table)
            .filter(super::schema::entry_point::target.eq(entry_point.entry_point.target.clone()))
            .filter(
                super::schema::entry_point::signature.eq(entry_point
                    .entry_point
                    .signature
                    .clone()),
            )
            .filter(super::schema::entry_point_tracing_params::tracing_type.eq(tracing_type))
            .filter(super::schema::entry_point_tracing_params::data.eq(data))
            .select(super::schema::entry_point_tracing_params::id)
            .first::<i64>(conn)
            .await
            .map_err(PostgresError::from)?;

        Ok(id_)
    }

    // Retrieves the database ids of an entry point tracing params (linked to the entrypoint id)
    // from a list of entry points ids and tracing params.
    pub(crate) async fn ids_by_entry_point_with_tracing_params(
        entry_points_with_tracing_params: &[(EntryPointId, EntryPointTracingParamsCommon)],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<(EntryPointId, EntryPointTracingParamsCommon), i64>, StorageError> {
        if entry_points_with_tracing_params.is_empty() {
            return Ok(HashMap::new());
        }

        let (external_ids, params): (HashSet<_>, HashSet<_>) = entry_points_with_tracing_params
            .iter()
            .map(|(ep_id, params)| {
                let params = match params {
                    EntryPointTracingParamsCommon::RPCTracer(rpc_tracer) => {
                        serde_json::to_value(rpc_tracer).map_err(|e| {
                            StorageError::Unexpected(format!(
                                "Failed to serialize RPCTracerEntryPoint: {e}"
                            ))
                        })?
                    }
                };
                Ok((ep_id, params))
            })
            .collect::<Result<Vec<_>, StorageError>>()?
            .into_iter()
            .unzip();

        let tuples = entry_points_with_tracing_params
            .iter()
            .map(|(ep_id, params)| (ep_id, EntryPointTracingType::from(params), params))
            .collect::<Vec<_>>();

        // Stores the tuple of (entry point id, tracing type, tracing params) to the tuple of (entry
        // point id, tracing params) This is used below for application side filtering.
        let tuples_to_tracing_params: HashMap<
            (&EntryPointId, &EntryPointTracingType, &EntryPointTracingParamsCommon),
            &(EntryPointId, EntryPointTracingParamsCommon),
        > = tuples
            .iter()
            .zip(entry_points_with_tracing_params)
            .map(|((ep_id, tracing_type, params), ep)| ((*ep_id, tracing_type, *params), ep))
            .collect();

        let res = entry_point_tracing_params::table
            .inner_join(entry_point::table)
            .filter(
                // Not filtered by tracing type eq_any because of a diesel bug with OID, shouldn't
                // affect results much because of data filtering and the application side filtering
                // below.
                entry_point::external_id
                    .eq_any(external_ids)
                    .and(entry_point_tracing_params::data.eq_any(params)),
            )
            .select((
                entry_point::external_id,
                entry_point_tracing_params::tracing_type,
                entry_point_tracing_params::data,
                entry_point_tracing_params::id,
            ))
            .load::<(EntryPointId, EntryPointTracingType, Option<serde_json::Value>, i64)>(conn)
            .await
            .map_err(PostgresError::from)?
            .into_iter()
            .filter_map(|(ext_id, tracing_type, params, id)| {
                let tracing_params = match tracing_type {
                    EntryPointTracingType::RpcTracer => {
                        let params_value = match params {
                            Some(params) => params,
                            None => {
                                return Some(Err(StorageError::Unexpected(
                                    "Data should be Some for RpcTracer".to_string(),
                                )));
                            }
                        };

                        match serde_json::from_value(params_value) {
                            Ok(p) => EntryPointTracingParamsCommon::RPCTracer(p),
                            Err(e) => {
                                return Some(Err(StorageError::Unexpected(format!(
                                    "Failed to deserialize RPCTracerEntryPoint: {e}"
                                ))))
                            }
                        }
                    }
                };

                tuples_to_tracing_params
                    .get(&(&ext_id, &tracing_type, &tracing_params))
                    .map(|ep| Ok(((*ep).clone(), id)))
            })
            .collect::<Result<HashMap<_, _>, StorageError>>()?;

        Ok(res)
    }
}

#[derive(Insertable)]
#[diesel(table_name = entry_point_tracing_params)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPointTracingParams {
    pub entry_point_id: i64,
    pub tracing_type: EntryPointTracingType,
    pub data: Option<serde_json::Value>,
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = entry_point_tracing_result)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPointTracingResult {
    pub entry_point_tracing_params_id: i64,
    pub detection_block: i64,
    pub detection_data: serde_json::Value,
}

impl NewEntryPointTracingResult {
    /// Number of fields in this struct for database insertion.
    /// IMPORTANT: Update this if you add/remove fields!
    /// Used to calculate MAX_BATCH_SIZE to avoid exceeding PostgreSQL's i16 parameter limit
    /// (32767).
    pub const FIELD_COUNT: usize = 3;
    /// Maximum number of rows that can be inserted in a single batch.
    /// PostgreSQL wire protocol uses i16 for parameter count (max 32767).
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Insertable)]
#[diesel(table_name = protocol_component_has_entry_point_tracing_params)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolComponentHasEntryPointTracingParams {
    pub protocol_component_id: i64,
    pub entry_point_tracing_params_id: i64,
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(ProtocolComponent))]
#[diesel(belongs_to(EntryPoint))]
#[diesel(table_name = protocol_component_uses_entry_point)]
#[diesel(primary_key(protocol_component_id, entry_point_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponentUsesEntryPoint {
    pub protocol_component_id: i64,
    pub entry_point_id: i64,
}

#[derive(Insertable)]
#[diesel(table_name = protocol_component_uses_entry_point)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolComponentUsesEntryPoint {
    pub protocol_component_id: i64,
    pub entry_point_id: i64,
}

#[derive(Identifiable, Queryable, Associations, Selectable)]
#[diesel(belongs_to(EntryPointTracingParams))]
#[diesel(belongs_to(Account))]
#[diesel(table_name = entry_point_tracing_params_calls_account)]
#[diesel(primary_key(entry_point_tracing_params_id, account_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPointTracingParamsCallsAccount {
    pub entry_point_tracing_params_id: i64,
    pub account_id: i64,
}

#[derive(Insertable)]
#[diesel(table_name = entry_point_tracing_params_calls_account)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPointTracingParamsCallsAccount {
    pub entry_point_tracing_params_id: i64,
    pub account_id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that MAX_BATCH_SIZE is calculated correctly and doesn't exceed i16 limit
    #[test]
    fn test_max_batch_size_within_limits() {
        // Verify that MAX_BATCH_SIZE * FIELD_COUNT doesn't exceed i16::MAX (32767)
        const {
            assert!(
                NewComponentBalance::MAX_BATCH_SIZE * NewComponentBalance::FIELD_COUNT <= 32767,
                "NewComponentBalance: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewComponentBalanceLatest::MAX_BATCH_SIZE * NewComponentBalanceLatest::FIELD_COUNT <=
                    32767,
                "NewComponentBalanceLatest: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewProtocolState::MAX_BATCH_SIZE * NewProtocolState::FIELD_COUNT <= 32767,
                "NewProtocolState: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewProtocolStateLatest::MAX_BATCH_SIZE * NewProtocolStateLatest::FIELD_COUNT <=
                    32767,
                "NewProtocolStateLatest: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewAccountBalance::MAX_BATCH_SIZE * NewAccountBalance::FIELD_COUNT <= 32767,
                "NewAccountBalance: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewBlock::MAX_BATCH_SIZE * NewBlock::FIELD_COUNT <= 32767,
                "NewBlock: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewTransaction::MAX_BATCH_SIZE * NewTransaction::FIELD_COUNT <= 32767,
                "NewTransaction: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewAccount::MAX_BATCH_SIZE * NewAccount::FIELD_COUNT <= 32767,
                "NewAccount: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewToken::MAX_BATCH_SIZE * NewToken::FIELD_COUNT <= 32767,
                "NewToken: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewProtocolComponent::MAX_BATCH_SIZE * NewProtocolComponent::FIELD_COUNT <= 32767,
                "NewProtocolComponent: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
        const {
            assert!(
                NewEntryPointTracingResult::MAX_BATCH_SIZE *
                    NewEntryPointTracingResult::FIELD_COUNT <=
                    32767,
                "NewEntryPointTracingResult: MAX_BATCH_SIZE * FIELD_COUNT exceeds i16::MAX"
            );
        }
    }
}
