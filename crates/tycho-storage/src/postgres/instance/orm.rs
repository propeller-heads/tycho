//! Diesel structs for the tables of the `tycho` schema.
//!
//! Each table has a row struct for reads. Tables whose rows get database-filled columns (a
//! sequence id, `inserted_ts`, `modified_ts`) also have a `New*` struct for inserts that leaves
//! those columns out. Link tables without such columns use one struct for both. So do the history
//! tables that no writer fills: only tests insert into them.
//!
//! A writer also loads rows into a `New*` struct when it writes them again. `QueryableByName`
//! reads the columns of the struct by name and ignores the other columns.
//!
//! Field order follows the column order of `schema.rs`, which `Queryable` relies on.
use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_derive_enum::DbEnum;
use tycho_common::{models::blockchain::TracingParams, Bytes};

use super::{schema::tycho, InstanceId};

/// Physical state of an instance. Which instance readers see is recorded only in
/// `protocol_system.active_instance_id`.
#[derive(Debug, DbEnum, Clone, Copy, PartialEq, Eq)]
#[ExistingTypePath = "crate::postgres::instance::schema::tycho::sql_types::InstanceStatus"]
pub enum InstanceStatus {
    /// Leaves being built, loaded or attached; readers and writers ignore the instance.
    Loading,
    /// All leaves attached.
    Ready,
    /// Leaves detached and dropped; the row stays so the id and label are never reused.
    Dropped,
}

#[derive(Debug, DbEnum, Clone, Copy, PartialEq, Eq, Hash)]
#[ExistingTypePath = "crate::postgres::instance::schema::tycho::sql_types::EntryPointTracingType"]
pub enum EntryPointTracingType {
    RpcTracer,
}

impl From<&TracingParams> for EntryPointTracingType {
    fn from(params: &TracingParams) -> Self {
        match params {
            TracingParams::RPCTracer(_) => Self::RpcTracer,
        }
    }
}

// Catalog

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::protocol_instance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolInstance {
    pub id: InstanceId,
    pub protocol_system_id: i64,
    pub label: String,
    pub status: InstanceStatus,
    pub base_block_number: Option<i64>,
    pub base_block_hash: Option<Bytes>,
    pub base_block_ts: Option<NaiveDateTime>,
    pub source: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::protocol_instance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolInstance {
    pub protocol_system_id: i64,
    pub label: String,
    pub status: InstanceStatus,
    pub base_block_number: Option<i64>,
    pub base_block_hash: Option<Bytes>,
    pub base_block_ts: Option<NaiveDateTime>,
    pub source: String,
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::extraction_state)]
#[diesel(primary_key(instance_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ExtractionState {
    pub instance_id: InstanceId,
    pub cursor: Option<Bytes>,
    pub block_number: i64,
    pub block_hash: Bytes,
    pub block_ts: NaiveDateTime,
    pub attributes: Option<serde_json::Value>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::extraction_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewExtractionState {
    pub instance_id: InstanceId,
    pub cursor: Option<Bytes>,
    pub block_number: i64,
    pub block_hash: Bytes,
    pub block_ts: NaiveDateTime,
    pub attributes: Option<serde_json::Value>,
}

// Components and protocol state

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::protocol_component)]
#[diesel(primary_key(instance_id, id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponent {
    pub instance_id: InstanceId,
    pub id: i64,
    pub external_id: String,
    /// Name of a `public.protocol_type` row.
    pub protocol_type: String,
    pub attributes: Option<serde_json::Value>,
    pub created_at: NaiveDateTime,
    pub creation_block: i64,
    pub creation_tx_hash: Bytes,
    pub deleted_at: Option<NaiveDateTime>,
    pub deletion_tx_hash: Option<Bytes>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// The id comes from `tycho.protocol_component_id_seq`.
#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::protocol_component)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolComponent {
    pub instance_id: InstanceId,
    pub external_id: String,
    pub protocol_type: String,
    pub attributes: Option<serde_json::Value>,
    pub created_at: NaiveDateTime,
    pub creation_block: i64,
    pub creation_tx_hash: Bytes,
    pub deleted_at: Option<NaiveDateTime>,
    pub deletion_tx_hash: Option<Bytes>,
}

impl NewProtocolComponent {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 9;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::protocol_component_holds_token)]
#[diesel(primary_key(instance_id, protocol_component_id, token))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponentHoldsToken {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    /// Token address.
    pub token: Bytes,
    pub token_index: i16,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::protocol_component_holds_token)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolComponentHoldsToken {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub token: Bytes,
    pub token_index: i16,
}

impl NewProtocolComponentHoldsToken {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 4;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::protocol_state)]
#[diesel(primary_key(instance_id, protocol_component_id, attribute_name))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolState {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable, QueryableByName)]
#[diesel(table_name = tycho::protocol_state)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolState {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
}

impl NewProtocolState {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[expect(dead_code, reason = "only tests read history rows, and only some fields")]
#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = tycho::protocol_state_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolStateHistory {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::protocol_state_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolStateHistory {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub attribute_name: String,
    pub attribute_value: Bytes,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
}

impl NewProtocolStateHistory {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::component_balance)]
#[diesel(primary_key(instance_id, protocol_component_id, token))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentBalance {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    /// Token address.
    pub token: Bytes,
    pub new_balance: Bytes,
    pub previous_value: Bytes,
    pub balance_float: f64,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
}

#[derive(Debug, Insertable, QueryableByName)]
#[diesel(table_name = tycho::component_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewComponentBalance {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub token: Bytes,
    pub new_balance: Bytes,
    pub previous_value: Bytes,
    pub balance_float: f64,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
}

impl NewComponentBalance {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[expect(dead_code, reason = "only tests read history rows, and only some fields")]
#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = tycho::component_balance_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentBalanceHistory {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub token: Bytes,
    pub new_balance: Bytes,
    pub previous_value: Bytes,
    pub balance_float: f64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::component_balance_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewComponentBalanceHistory {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub token: Bytes,
    pub new_balance: Bytes,
    pub previous_value: Bytes,
    pub balance_float: f64,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
}

impl NewComponentBalanceHistory {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 9;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::component_tvl)]
#[diesel(primary_key(instance_id, protocol_component_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ComponentTvl {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub tvl: f64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::component_tvl)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewComponentTvl {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub tvl: f64,
}

// Contracts

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::account)]
#[diesel(primary_key(instance_id, id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Account {
    pub instance_id: InstanceId,
    pub id: i64,
    pub address: Bytes,
    pub title: String,
    pub created_at: Option<NaiveDateTime>,
    pub creation_block: Option<i64>,
    pub creation_tx_hash: Option<Bytes>,
    pub deleted_at: Option<NaiveDateTime>,
    pub deletion_tx_hash: Option<Bytes>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// The id comes from `tycho.account_id_seq`.
#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::account)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccount {
    pub instance_id: InstanceId,
    pub address: Bytes,
    pub title: String,
    pub created_at: Option<NaiveDateTime>,
    pub creation_block: Option<i64>,
    pub creation_tx_hash: Option<Bytes>,
    pub deleted_at: Option<NaiveDateTime>,
    pub deletion_tx_hash: Option<Bytes>,
}

impl NewAccount {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::protocol_component_holds_contract)]
#[diesel(primary_key(instance_id, protocol_component_id, account_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponentHoldsContract {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub account_id: i64,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::protocol_component_holds_contract)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewProtocolComponentHoldsContract {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub account_id: i64,
}

impl NewProtocolComponentHoldsContract {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 3;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::contract_code)]
#[diesel(primary_key(instance_id, account_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractCode {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub code: Bytes,
    pub hash: Bytes,
    pub modify_tx_hash: Bytes,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable, Queryable, Selectable)]
#[diesel(table_name = tycho::contract_code)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewContractCode {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub code: Bytes,
    pub hash: Bytes,
    pub modify_tx_hash: Bytes,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
}

impl NewContractCode {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = tycho::contract_code_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractCodeHistory {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub code: Bytes,
    pub hash: Bytes,
    pub modify_tx_hash: Bytes,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::contract_code_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewContractCodeHistory {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub code: Bytes,
    pub hash: Bytes,
    pub modify_tx_hash: Bytes,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
}

impl NewContractCodeHistory {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::contract_storage)]
#[diesel(primary_key(instance_id, account_id, slot))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractStorage {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable, QueryableByName)]
#[diesel(table_name = tycho::contract_storage)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewContractStorage {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
}

impl NewContractStorage {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = tycho::contract_storage_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ContractStorageHistory {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::contract_storage_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewContractStorageHistory {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub slot: Bytes,
    pub value: Option<Bytes>,
    pub previous_value: Option<Bytes>,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
}

impl NewContractStorageHistory {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 8;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::account_balance)]
#[diesel(primary_key(instance_id, account_id, token))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AccountBalance {
    pub instance_id: InstanceId,
    pub account_id: i64,
    /// Token address.
    pub token: Bytes,
    pub balance: Bytes,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable, QueryableByName)]
#[diesel(table_name = tycho::account_balance)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccountBalance {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub token: Bytes,
    pub balance: Bytes,
    pub valid_from: NaiveDateTime,
    pub tx_index: i32,
}

impl NewAccountBalance {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 6;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = tycho::account_balance_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AccountBalanceHistory {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub token: Bytes,
    pub balance: Bytes,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::account_balance_history)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewAccountBalanceHistory {
    pub instance_id: InstanceId,
    pub account_id: i64,
    pub token: Bytes,
    pub balance: Bytes,
    pub valid_from: NaiveDateTime,
    pub valid_to: NaiveDateTime,
    pub tx_index: i32,
}

impl NewAccountBalanceHistory {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 7;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

// Entry points (DCI)

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::entry_point)]
#[diesel(primary_key(instance_id, id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPoint {
    pub instance_id: InstanceId,
    pub id: i64,
    pub external_id: String,
    pub target: Bytes,
    pub signature: String,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// The id comes from `tycho.entry_point_id_seq`.
#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::entry_point)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPoint {
    pub instance_id: InstanceId,
    pub external_id: String,
    pub target: Bytes,
    pub signature: String,
}

impl NewEntryPoint {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 4;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::entry_point_tracing_params)]
#[diesel(primary_key(instance_id, id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPointTracingParams {
    pub instance_id: InstanceId,
    pub id: i64,
    pub entry_point_id: i64,
    pub tracing_type: EntryPointTracingType,
    pub data: Option<serde_json::Value>,
    pub data_hash: Option<Bytes>,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// The id comes from `tycho.entry_point_tracing_params_id_seq`; a trigger sets `data_hash`.
#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::entry_point_tracing_params)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPointTracingParams {
    pub instance_id: InstanceId,
    pub entry_point_id: i64,
    pub tracing_type: EntryPointTracingType,
    pub data: Option<serde_json::Value>,
}

impl NewEntryPointTracingParams {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 4;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable, Insertable)]
#[diesel(table_name = tycho::entry_point_tracing_params_calls_account)]
#[diesel(primary_key(instance_id, entry_point_tracing_params_id, account_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPointTracingParamsCallsAccount {
    pub instance_id: InstanceId,
    pub entry_point_tracing_params_id: i64,
    pub account_id: i64,
}

impl EntryPointTracingParamsCallsAccount {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 3;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable)]
#[diesel(table_name = tycho::entry_point_tracing_result)]
#[diesel(primary_key(instance_id, id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EntryPointTracingResult {
    pub instance_id: InstanceId,
    pub id: i64,
    pub entry_point_tracing_params_id: i64,
    pub detection_block_number: i64,
    pub detection_block_hash: Bytes,
    pub detection_data: serde_json::Value,
    pub inserted_ts: NaiveDateTime,
    pub modified_ts: NaiveDateTime,
}

/// The id comes from `tycho.entry_point_tracing_result_id_seq`.
#[derive(Debug, Insertable)]
#[diesel(table_name = tycho::entry_point_tracing_result)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct NewEntryPointTracingResult {
    pub instance_id: InstanceId,
    pub entry_point_tracing_params_id: i64,
    pub detection_block_number: i64,
    pub detection_block_hash: Bytes,
    pub detection_data: serde_json::Value,
}

impl NewEntryPointTracingResult {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 5;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable, Insertable)]
#[diesel(table_name = tycho::protocol_component_uses_entry_point)]
#[diesel(primary_key(instance_id, protocol_component_id, entry_point_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponentUsesEntryPoint {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub entry_point_id: i64,
}

impl ProtocolComponentUsesEntryPoint {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 3;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}

#[derive(Debug, Identifiable, Queryable, Selectable, Insertable)]
#[diesel(table_name = tycho::protocol_component_has_entry_point_tracing_params)]
#[diesel(primary_key(instance_id, protocol_component_id, entry_point_tracing_params_id))]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct ProtocolComponentHasEntryPointTracingParams {
    pub instance_id: InstanceId,
    pub protocol_component_id: i64,
    pub entry_point_tracing_params_id: i64,
}

impl ProtocolComponentHasEntryPointTracingParams {
    /// Number of fields, which is the number of bind parameters per row. Update it when a field
    /// changes.
    pub const FIELD_COUNT: usize = 3;
    /// Maximum number of rows in one insert. Postgres accepts at most 32767 bind parameters per
    /// statement.
    pub const MAX_BATCH_SIZE: usize = 32767 / Self::FIELD_COUNT;
}
