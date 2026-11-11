use chrono::{Duration, NaiveDateTime, Utc};
use diesel::{
    prelude::*,
    result::{DatabaseErrorKind, Error},
    sql_query,
    sql_types::{BigInt, Bool, Int4, Text},
};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use tycho_common::Bytes;

use super::{orm::*, schema::tycho, InstanceId};
use crate::postgres::{db_fixtures, testing::run_against_db};

async fn setup_db() -> AsyncPgConnection {
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for DB tests");
    let mut conn = AsyncPgConnection::establish(&db_url)
        .await
        .expect("connecting to the test database failed");
    conn.begin_test_transaction()
        .await
        .expect("starting the test transaction failed");
    conn
}

async fn setup_instance(conn: &mut AsyncPgConnection) -> InstanceId {
    let protocol_system_id =
        db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
    db_fixtures::insert_instance(conn, protocol_system_id, "0.0.2").await
}

fn now() -> NaiveDateTime {
    Utc::now().naive_utc()
}

fn bytes(value: u64) -> Bytes {
    Bytes::from(value)
}

fn database_error_kind<T>(result: QueryResult<T>) -> DatabaseErrorKind {
    match result {
        Err(Error::DatabaseError(kind, _)) => kind,
        Err(other) => panic!("expected a database error, got {other}"),
        Ok(_) => panic!("expected the statement to fail"),
    }
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(QueryableByName)]
struct Presence {
    #[diesel(sql_type = Bool)]
    present: bool,
}

async fn count(conn: &mut AsyncPgConnection, query: &str, instance_id: InstanceId) -> i64 {
    sql_query(query)
        .bind::<Text, _>(format!("_i{instance_id}$"))
        .get_result::<Count>(conn)
        .await
        .unwrap_or_else(|e| panic!("query failed: {e}\n{query}"))
        .count
}

#[tokio::test]
async fn test_instance_gets_one_attached_leaf_per_table_with_check_and_fks() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;

    let attached = count(
        &mut conn,
        "SELECT count(*) AS count FROM pg_inherits i
         JOIN pg_class c ON c.oid = i.inhrelid
         WHERE c.relnamespace = 'tycho'::regnamespace AND c.relname ~ $1",
        instance_id,
    )
    .await;
    let checks = count(
        &mut conn,
        "SELECT count(*) AS count FROM pg_constraint
         WHERE contype = 'c' AND connamespace = 'tycho'::regnamespace
           AND conrelid::regclass::text ~ $1",
        instance_id,
    )
    .await;
    let leaf_indexes = count(
        &mut conn,
        "SELECT count(*) AS count FROM pg_index
         WHERE indrelid::regclass::text ~ $1
           AND indrelid::regclass::text LIKE 'tycho.%'",
        instance_id,
    )
    .await;
    let parent_indexes: i64 = sql_query(
        "SELECT count(*) AS count FROM pg_index i
         JOIN pg_class c ON c.oid = i.indrelid
         WHERE c.relnamespace = 'tycho'::regnamespace AND c.relkind = 'p'
           AND c.relname = ANY (tycho.instance_tables())",
    )
    .get_result::<Count>(&mut conn)
    .await
    .expect("counting parent indexes failed")
    .count;
    let foreign_keys = count(
        &mut conn,
        "SELECT count(*) AS count FROM pg_constraint
         WHERE contype = 'f' AND connamespace = 'tycho'::regnamespace
           AND conrelid::regclass::text ~ $1
           AND confrelid::regclass::text ~ $1",
        instance_id,
    )
    .await;

    assert_eq!(attached, 16, "every per-instance table gets one attached leaf");
    assert_eq!(
        leaf_indexes, parent_indexes,
        "each leaf gets exactly its parent's indexes; ATTACH must not build extra ones"
    );
    assert_eq!(checks, 16, "every leaf gets its instance CHECK");
    assert_eq!(foreign_keys, 17, "the leaf FKs all point inside the instance");
}

#[tokio::test]
async fn test_create_instance_leaves_rejects_unknown_instance() {
    let mut conn = setup_db().await;

    let result = sql_query("SELECT tycho.create_instance_leaves(-1)")
        .execute(&mut conn)
        .await;

    let Err(Error::DatabaseError(_, info)) = result else {
        panic!("expected create_instance_leaves to fail for an unknown instance");
    };
    assert!(
        info.message()
            .contains("instance -1 does not exist"),
        "{}",
        info.message()
    );
}

async fn insert_component(conn: &mut AsyncPgConnection, instance_id: InstanceId) -> i64 {
    diesel::insert_into(tycho::protocol_component::table)
        .values(NewProtocolComponent {
            instance_id,
            external_id: "0xpool".to_string(),
            protocol_type: "curve_pool".to_string(),
            attributes: None,
            created_at: now(),
            creation_block: 1,
            creation_tx_hash: bytes(1),
            deleted_at: None,
            deletion_tx_hash: None,
        })
        .returning(tycho::protocol_component::id)
        .get_result(conn)
        .await
        .expect("inserting a component failed")
}

async fn insert_account(conn: &mut AsyncPgConnection, instance_id: InstanceId) -> i64 {
    diesel::insert_into(tycho::account::table)
        .values(NewAccount {
            instance_id,
            address: bytes(0xaa),
            title: "pool".to_string(),
            created_at: Some(now()),
            creation_block: Some(1),
            creation_tx_hash: Some(bytes(1)),
            deleted_at: None,
            deletion_tx_hash: None,
        })
        .returning(tycho::account::id)
        .get_result(conn)
        .await
        .expect("inserting an account failed")
}

async fn round_trip_catalog_and_components(
    conn: &mut AsyncPgConnection,
    instance_id: InstanceId,
    component_id: i64,
) {
    let ts = now();

    diesel::insert_into(tycho::extraction_state::table)
        .values(NewExtractionState {
            instance_id,
            cursor: Some(bytes(7)),
            block_number: 100,
            block_hash: bytes(100),
            block_ts: ts,
            attributes: None,
        })
        .execute(conn)
        .await
        .expect("inserting extraction state failed");
    let state = tycho::extraction_state::table
        .find(instance_id)
        .select(ExtractionState::as_select())
        .get_result(conn)
        .await
        .expect("reading extraction state failed");
    assert_eq!(state.block_number, 100);

    let instance = tycho::protocol_instance::table
        .find(instance_id)
        .select(ProtocolInstance::as_select())
        .get_result(conn)
        .await
        .expect("reading the instance failed");
    assert_eq!((instance.id, instance.status), (instance_id, InstanceStatus::Ready));

    let component = tycho::protocol_component::table
        .find((instance_id, component_id))
        .select(ProtocolComponent::as_select())
        .get_result(conn)
        .await
        .expect("reading the component failed");
    assert_eq!(component.external_id, "0xpool");

    diesel::insert_into(tycho::protocol_component_holds_token::table)
        .values(NewProtocolComponentHoldsToken {
            instance_id,
            protocol_component_id: component_id,
            token: bytes(0xee),
            token_index: 0,
        })
        .execute(conn)
        .await
        .expect("inserting a held token failed");
    let held = tycho::protocol_component_holds_token::table
        .select(ProtocolComponentHoldsToken::as_select())
        .get_result(conn)
        .await
        .expect("reading held tokens failed");
    assert_eq!(held.token, bytes(0xee));

    diesel::insert_into(tycho::protocol_state::table)
        .values(NewProtocolState {
            instance_id,
            protocol_component_id: component_id,
            attribute_name: "reserve0".to_string(),
            attribute_value: bytes(2),
            previous_value: Some(bytes(1)),
            valid_from: ts,
            tx_index: 3,
        })
        .execute(conn)
        .await
        .expect("inserting protocol state failed");
    let attribute = tycho::protocol_state::table
        .select(ProtocolState::as_select())
        .get_result(conn)
        .await
        .expect("reading protocol state failed");
    assert_eq!((attribute.attribute_value, attribute.tx_index), (bytes(2), 3));

    let archived = ProtocolStateHistory {
        instance_id,
        protocol_component_id: component_id,
        attribute_name: "reserve0".to_string(),
        attribute_value: bytes(1),
        previous_value: None,
        valid_from: ts - Duration::seconds(12),
        valid_to: ts,
        tx_index: 1,
        inserted_ts: ts,
        modified_ts: ts,
    };
    diesel::insert_into(tycho::protocol_state_history::table)
        .values(&archived)
        .execute(conn)
        .await
        .expect("inserting protocol state history failed");
    let history = tycho::protocol_state_history::table
        .select(ProtocolStateHistory::as_select())
        .get_result(conn)
        .await
        .expect("reading protocol state history failed");
    assert_eq!(history.attribute_value, bytes(1));

    diesel::insert_into(tycho::component_balance::table)
        .values(NewComponentBalance {
            instance_id,
            protocol_component_id: component_id,
            token: bytes(0xee),
            new_balance: bytes(5),
            previous_value: bytes(4),
            balance_float: 5.0,
            valid_from: ts,
            tx_index: 3,
        })
        .execute(conn)
        .await
        .expect("inserting a component balance failed");
    let balance = tycho::component_balance::table
        .select(ComponentBalance::as_select())
        .get_result(conn)
        .await
        .expect("reading component balances failed");
    assert_eq!(balance.new_balance, bytes(5));

    diesel::insert_into(tycho::component_balance_history::table)
        .values(ComponentBalanceHistory {
            instance_id,
            protocol_component_id: component_id,
            token: bytes(0xee),
            new_balance: bytes(4),
            previous_value: bytes(0),
            balance_float: 4.0,
            valid_from: ts - Duration::seconds(12),
            valid_to: ts,
            tx_index: 1,
            inserted_ts: ts,
        })
        .execute(conn)
        .await
        .expect("inserting component balance history failed");
    let balance_history = tycho::component_balance_history::table
        .select(ComponentBalanceHistory::as_select())
        .get_result(conn)
        .await
        .expect("reading component balance history failed");
    assert_eq!(balance_history.new_balance, bytes(4));

    diesel::insert_into(tycho::component_tvl::table)
        .values(NewComponentTvl { instance_id, protocol_component_id: component_id, tvl: 1.5 })
        .execute(conn)
        .await
        .expect("inserting TVL failed");
    let tvl = tycho::component_tvl::table
        .select(ComponentTvl::as_select())
        .get_result(conn)
        .await
        .expect("reading TVL failed");
    assert_eq!(tvl.tvl, 1.5);
}

async fn round_trip_contracts(
    conn: &mut AsyncPgConnection,
    instance_id: InstanceId,
    component_id: i64,
    account_id: i64,
) {
    let ts = now();

    let account = tycho::account::table
        .find((instance_id, account_id))
        .select(Account::as_select())
        .get_result(conn)
        .await
        .expect("reading the account failed");
    assert_eq!(account.address, bytes(0xaa));

    diesel::insert_into(tycho::protocol_component_holds_contract::table)
        .values(NewProtocolComponentHoldsContract {
            instance_id,
            protocol_component_id: component_id,
            account_id,
        })
        .execute(conn)
        .await
        .expect("inserting a held contract failed");
    let held = tycho::protocol_component_holds_contract::table
        .select(ProtocolComponentHoldsContract::as_select())
        .get_result(conn)
        .await
        .expect("reading held contracts failed");
    assert_eq!(held.account_id, account_id);

    diesel::insert_into(tycho::contract_code::table)
        .values(NewContractCode {
            instance_id,
            account_id,
            code: bytes(0x60),
            hash: bytes(0x61),
            modify_tx_hash: bytes(1),
            valid_from: ts,
            tx_index: 0,
        })
        .execute(conn)
        .await
        .expect("inserting code failed");
    let code = tycho::contract_code::table
        .select(ContractCode::as_select())
        .get_result(conn)
        .await
        .expect("reading code failed");
    assert_eq!(code.hash, bytes(0x61));

    diesel::insert_into(tycho::contract_code_history::table)
        .values(ContractCodeHistory {
            instance_id,
            account_id,
            code: bytes(0x50),
            hash: bytes(0x51),
            modify_tx_hash: bytes(1),
            valid_from: ts - Duration::seconds(12),
            valid_to: ts,
            tx_index: 0,
            inserted_ts: ts,
            modified_ts: ts,
        })
        .execute(conn)
        .await
        .expect("inserting code history failed");
    let code_history = tycho::contract_code_history::table
        .select(ContractCodeHistory::as_select())
        .get_result(conn)
        .await
        .expect("reading code history failed");
    assert_eq!(code_history.hash, bytes(0x51));

    diesel::insert_into(tycho::contract_storage::table)
        .values(NewContractStorage {
            instance_id,
            account_id,
            slot: bytes(1),
            value: Some(bytes(9)),
            previous_value: Some(bytes(8)),
            valid_from: ts,
            tx_index: 3,
        })
        .execute(conn)
        .await
        .expect("inserting a slot failed");
    let slot = tycho::contract_storage::table
        .select(ContractStorage::as_select())
        .get_result(conn)
        .await
        .expect("reading slots failed");
    assert_eq!(slot.value, Some(bytes(9)));

    diesel::insert_into(tycho::contract_storage_history::table)
        .values(ContractStorageHistory {
            instance_id,
            account_id,
            slot: bytes(1),
            value: Some(bytes(8)),
            previous_value: None,
            valid_from: ts - Duration::seconds(12),
            valid_to: ts,
            tx_index: 1,
            inserted_ts: ts,
            modified_ts: ts,
        })
        .execute(conn)
        .await
        .expect("inserting slot history failed");
    let slot_history = tycho::contract_storage_history::table
        .select(ContractStorageHistory::as_select())
        .get_result(conn)
        .await
        .expect("reading slot history failed");
    assert_eq!(slot_history.value, Some(bytes(8)));

    diesel::insert_into(tycho::account_balance::table)
        .values(NewAccountBalance {
            instance_id,
            account_id,
            token: bytes(0),
            balance: bytes(3),
            valid_from: ts,
            tx_index: 3,
        })
        .execute(conn)
        .await
        .expect("inserting an account balance failed");
    let account_balance = tycho::account_balance::table
        .select(AccountBalance::as_select())
        .get_result(conn)
        .await
        .expect("reading account balances failed");
    assert_eq!(account_balance.balance, bytes(3));

    diesel::insert_into(tycho::account_balance_history::table)
        .values(AccountBalanceHistory {
            instance_id,
            account_id,
            token: bytes(0),
            balance: bytes(2),
            valid_from: ts - Duration::seconds(12),
            valid_to: ts,
            tx_index: 1,
            inserted_ts: ts,
            modified_ts: ts,
        })
        .execute(conn)
        .await
        .expect("inserting account balance history failed");
    let account_balance_history = tycho::account_balance_history::table
        .select(AccountBalanceHistory::as_select())
        .get_result(conn)
        .await
        .expect("reading account balance history failed");
    assert_eq!(account_balance_history.balance, bytes(2));
}

async fn round_trip_entry_points(
    conn: &mut AsyncPgConnection,
    instance_id: InstanceId,
    component_id: i64,
    account_id: i64,
) {
    let entry_point_id: i64 = diesel::insert_into(tycho::entry_point::table)
        .values(NewEntryPoint {
            instance_id,
            external_id: "0xaa:getRate()".to_string(),
            target: bytes(0xaa),
            signature: "getRate()".to_string(),
        })
        .returning(tycho::entry_point::id)
        .get_result(conn)
        .await
        .expect("inserting an entry point failed");
    let entry_point = tycho::entry_point::table
        .find((instance_id, entry_point_id))
        .select(EntryPoint::as_select())
        .get_result(conn)
        .await
        .expect("reading the entry point failed");
    assert_eq!(entry_point.signature, "getRate()");

    let data = serde_json::json!({"caller": null, "calldata": "0x679aefce"});
    let params_id: i64 = diesel::insert_into(tycho::entry_point_tracing_params::table)
        .values(NewEntryPointTracingParams {
            instance_id,
            entry_point_id,
            tracing_type: EntryPointTracingType::RpcTracer,
            data: Some(data),
        })
        .returning(tycho::entry_point_tracing_params::id)
        .get_result(conn)
        .await
        .expect("inserting tracing params failed");
    let params = tycho::entry_point_tracing_params::table
        .find((instance_id, params_id))
        .select(EntryPointTracingParams::as_select())
        .get_result(conn)
        .await
        .expect("reading tracing params failed");
    assert_eq!(params.tracing_type, EntryPointTracingType::RpcTracer);
    assert!(params.data_hash.is_some(), "the trigger sets data_hash on insert");

    diesel::insert_into(tycho::entry_point_tracing_params_calls_account::table)
        .values(EntryPointTracingParamsCallsAccount {
            instance_id,
            entry_point_tracing_params_id: params_id,
            account_id,
        })
        .execute(conn)
        .await
        .expect("inserting a called account failed");
    let called = tycho::entry_point_tracing_params_calls_account::table
        .select(EntryPointTracingParamsCallsAccount::as_select())
        .get_result(conn)
        .await
        .expect("reading called accounts failed");
    assert_eq!(called.account_id, account_id);

    diesel::insert_into(tycho::entry_point_tracing_result::table)
        .values(NewEntryPointTracingResult {
            instance_id,
            entry_point_tracing_params_id: params_id,
            detection_block_number: 100,
            detection_block_hash: bytes(100),
            detection_data: serde_json::json!({"accessed_slots": {}}),
        })
        .execute(conn)
        .await
        .expect("inserting a tracing result failed");
    let result = tycho::entry_point_tracing_result::table
        .select(EntryPointTracingResult::as_select())
        .get_result(conn)
        .await
        .expect("reading tracing results failed");
    assert_eq!(result.detection_block_number, 100);

    diesel::insert_into(tycho::protocol_component_uses_entry_point::table)
        .values(ProtocolComponentUsesEntryPoint {
            instance_id,
            protocol_component_id: component_id,
            entry_point_id,
        })
        .execute(conn)
        .await
        .expect("linking an entry point failed");
    let uses = tycho::protocol_component_uses_entry_point::table
        .select(ProtocolComponentUsesEntryPoint::as_select())
        .get_result(conn)
        .await
        .expect("reading entry point links failed");
    assert_eq!(uses.entry_point_id, entry_point_id);

    diesel::insert_into(tycho::protocol_component_has_entry_point_tracing_params::table)
        .values(ProtocolComponentHasEntryPointTracingParams {
            instance_id,
            protocol_component_id: component_id,
            entry_point_tracing_params_id: params_id,
        })
        .execute(conn)
        .await
        .expect("linking tracing params failed");
    let has = tycho::protocol_component_has_entry_point_tracing_params::table
        .select(ProtocolComponentHasEntryPointTracingParams::as_select())
        .get_result(conn)
        .await
        .expect("reading tracing params links failed");
    assert_eq!(has.entry_point_tracing_params_id, params_id);
}

/// Writes and reads one row per table through the ORM structs, which also shows that the
/// generated schema matches the migration.
#[tokio::test]
async fn test_rows_round_trip_through_orm_structs() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;
    let component_id = insert_component(&mut conn, instance_id).await;
    let account_id = insert_account(&mut conn, instance_id).await;

    round_trip_catalog_and_components(&mut conn, instance_id, component_id).await;
    round_trip_contracts(&mut conn, instance_id, component_id, account_id).await;
    round_trip_entry_points(&mut conn, instance_id, component_id, account_id).await;
}

#[tokio::test]
async fn test_leaf_fk_rejects_state_of_missing_component() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;

    let result = diesel::insert_into(tycho::protocol_state::table)
        .values(NewProtocolState {
            instance_id,
            protocol_component_id: 42,
            attribute_name: "reserve0".to_string(),
            attribute_value: bytes(1),
            previous_value: None,
            valid_from: now(),
            tx_index: 0,
        })
        .execute(&mut conn)
        .await;

    assert_eq!(database_error_kind(result), DatabaseErrorKind::ForeignKeyViolation);
}

#[tokio::test]
async fn test_instance_without_leaves_has_no_partition() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;
    insert_component(&mut conn, instance_id).await;

    let result = diesel::insert_into(tycho::protocol_component::table)
        .values(NewProtocolComponent {
            instance_id: InstanceId(i32::MAX),
            external_id: "0xpool".to_string(),
            protocol_type: "curve_pool".to_string(),
            attributes: None,
            created_at: now(),
            creation_block: 1,
            creation_tx_hash: bytes(1),
            deleted_at: None,
            deletion_tx_hash: None,
        })
        .execute(&mut conn)
        .await;

    assert_eq!(
        database_error_kind(result),
        DatabaseErrorKind::CheckViolation,
        "with no DEFAULT partition, a row for an instance without leaves is rejected"
    );
}

#[tokio::test]
async fn test_leaf_check_rejects_rows_of_another_instance() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;

    let result = sql_query(format!(
        "INSERT INTO tycho.component_tvl_i{instance_id} (instance_id, protocol_component_id, tvl)
         VALUES ($1, 1, 0)"
    ))
    .bind::<Int4, _>(i32::MAX)
    .execute(&mut conn)
    .await;

    assert_eq!(database_error_kind(result), DatabaseErrorKind::CheckViolation);
}

fn history_row(instance_id: InstanceId, valid_to: NaiveDateTime) -> ProtocolStateHistory {
    ProtocolStateHistory {
        instance_id,
        protocol_component_id: 1,
        attribute_name: "reserve0".to_string(),
        attribute_value: bytes(1),
        previous_value: None,
        valid_from: valid_to - Duration::seconds(12),
        valid_to,
        tx_index: 0,
        inserted_ts: now(),
        modified_ts: now(),
    }
}

#[tokio::test]
async fn test_history_rejects_a_day_that_is_not_premade() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;

    let result = diesel::insert_into(tycho::protocol_state_history::table)
        .values(history_row(instance_id, now() + Duration::days(30)))
        .execute(&mut conn)
        .await;

    assert_eq!(database_error_kind(result), DatabaseErrorKind::CheckViolation);
}

#[tokio::test]
async fn test_history_pk_rejects_the_same_version_archived_twice() {
    let mut conn = setup_db().await;
    let instance_id = setup_instance(&mut conn).await;
    let row = history_row(instance_id, now());
    diesel::insert_into(tycho::protocol_state_history::table)
        .values(&row)
        .execute(&mut conn)
        .await
        .expect("the first archive succeeds");

    let result = diesel::insert_into(tycho::protocol_state_history::table)
        .values(&row)
        .execute(&mut conn)
        .await;

    assert_eq!(database_error_kind(result), DatabaseErrorKind::UniqueViolation);
}

const HISTORY_TABLES: [&str; 5] = [
    "protocol_state_history",
    "component_balance_history",
    "contract_code_history",
    "contract_storage_history",
    "account_balance_history",
];

/// Whether the daily partition of `table` covering now() - 3 months exists.
async fn expired_day_exists(conn: &mut AsyncPgConnection, table: &str) -> bool {
    sql_query(
        "SELECT to_regclass('tycho.' || $1 || '_p' ||
             to_char(now() - interval '3 months', 'YYYYMMDD')) IS NOT NULL AS present",
    )
    .bind::<Text, _>(table)
    .get_result::<Presence>(conn)
    .await
    .expect("querying partition existence failed")
    .present
}

#[tokio::test]
async fn test_drop_expired_partitions_covers_history_tables_serial_db() {
    run_against_db(|connection_pool| async move {
        let mut conn = connection_pool
            .get()
            .await
            .expect("Failed to get a connection from the pool");
        for table in HISTORY_TABLES {
            conn.batch_execute(&format!(
                "SELECT partman.create_partition_time(
                     'tycho.{table}', ARRAY[now() - interval '3 months']);"
            ))
            .await
            .expect("creating an expired partition failed");
            assert!(expired_day_exists(&mut conn, table).await, "{table}: planted day must exist");
        }

        // The procedure commits internally, so it runs as one autocommit statement.
        conn.batch_execute("CALL drop_expired_partitions(p_max_attempts => 1);")
            .await
            .expect("drop procedure should run");

        for table in HISTORY_TABLES {
            assert!(!expired_day_exists(&mut conn, table).await, "{table}: expired day must go");
        }
    })
    .await;
}

#[tokio::test]
async fn test_premake_makes_days_for_empty_history_tables() {
    let mut conn = setup_db().await;
    // The history tables have no rows, as they do until the data migration.
    let day_ahead = "to_char(now() + interval '7 days', 'YYYYMMDD')";
    for table in HISTORY_TABLES {
        conn.batch_execute(&format!(
            "DO $$ BEGIN EXECUTE 'DROP TABLE tycho.{table}_p' || {day_ahead}; END $$;"
        ))
        .await
        .expect("dropping the last premade day failed");
    }

    conn.batch_execute("SELECT tycho.premake_history_partitions();")
        .await
        .expect("premaking history days failed");

    for table in HISTORY_TABLES {
        let present = sql_query(format!(
            "SELECT to_regclass('tycho.{table}_p' || {day_ahead}) IS NOT NULL AS present"
        ))
        .get_result::<Presence>(&mut conn)
        .await
        .expect("querying partition existence failed")
        .present;
        assert!(present, "{table}: the day 7 days ahead must be made again");
    }
}
