//! # Active-instance filters
//!
//! Readers see the instance that `public.protocol_system.active_instance_id` points to.
//!
//! Look up the instances once per read and filter every query of the read on the bound ids:
//!
//! - one protocol: [`active_instance`], then `instance_id.eq(id)`.
//! - all protocols: [`active_instance_ids`], then `instance_id.eq_any(&ids)`.
//!
//! Postgres prunes a subselect only during execution, so with one it would plan and lock every
//! leaf of each table. Each statement also takes a new snapshot, so a subselect in each query of a
//! read could see a different pointer.
//!
//! A protocol with no active instance yields no rows, and so does an unknown protocol name:
//! check the name first where it must exist.

use chrono::NaiveDateTime;
use diesel::{prelude::*, sql_types::Timestamptz};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::postgres::{
    instance::{
        schema::tycho::{extraction_state, protocol_instance},
        InstanceId,
    },
    schema::protocol_system,
    PostgresError, StorageError,
};

/// The active instance of a protocol system, with its head, its base block time and the retention
/// horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveInstance {
    pub id: InstanceId,
    /// The block timestamp of the instance's extraction state. `None` if it has none yet.
    pub head: Option<NaiveDateTime>,
    /// The block timestamp of the base block of an imported or migrated instance. `None` for an
    /// instance synced in place. The instance has no history before it.
    pub base: Option<NaiveDateTime>,
    /// The oldest version whose history is complete: `now()` minus `partition_retention()`.
    pub retention_horizon: NaiveDateTime,
}

/// The active instance of the protocol system `name`.
///
/// `None` if the protocol system has no active instance, or if no protocol system has that name.
/// Filter with the returned id as a bound value: with a custom plan, Postgres then plans and
/// locks only that instance's leaves. A generic plan still locks every leaf.
pub async fn active_instance(
    name: &str,
    conn: &mut AsyncPgConnection,
) -> Result<Option<InstanceId>, StorageError> {
    let id = protocol_system::table
        .filter(protocol_system::name.eq(name))
        .select(protocol_system::active_instance_id)
        .first::<Option<InstanceId>>(conn)
        .await
        .optional()
        .map_err(PostgresError::from)?;
    Ok(id.flatten())
}

/// The active instance of the protocol system `name`, with its head, its base block time and the
/// retention horizon, in one query.
///
/// `None` if the protocol system has no active instance, or if no protocol system has that name.
/// Filter with the returned id as a bound value, as for [`active_instance`].
pub async fn active_instance_head(
    name: &str,
    conn: &mut AsyncPgConnection,
) -> Result<Option<ActiveInstance>, StorageError> {
    let row = protocol_system::table
        .left_join(
            extraction_state::table.on(extraction_state::instance_id
                .nullable()
                .eq(protocol_system::active_instance_id)),
        )
        .left_join(
            protocol_instance::table.on(protocol_instance::id
                .nullable()
                .eq(protocol_system::active_instance_id)),
        )
        .filter(protocol_system::name.eq(name))
        .select((
            protocol_system::active_instance_id,
            extraction_state::block_ts.nullable(),
            protocol_instance::base_block_ts.nullable(),
            diesel::dsl::sql::<Timestamptz>("now() - partition_retention()"),
        ))
        .first::<(Option<InstanceId>, Option<NaiveDateTime>, Option<NaiveDateTime>, NaiveDateTime)>(
            conn,
        )
        .await
        .optional()
        .map_err(PostgresError::from)?;
    Ok(row.and_then(|(id, head, base, retention_horizon)| {
        id.map(|id| ActiveInstance { id, head, base, retention_horizon })
    }))
}

/// The active instance of every protocol system that has one.
///
/// Instances no protocol system points to are absent, even when `ready`. Empty if no protocol
/// system has an active instance.
pub async fn active_instance_ids(
    conn: &mut AsyncPgConnection,
) -> Result<Vec<InstanceId>, StorageError> {
    Ok(protocol_system::table
        .filter(protocol_system::active_instance_id.is_not_null())
        .select(protocol_system::active_instance_id.assume_not_null())
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;
    use crate::postgres::{db_fixtures, testing::instance_test_connection};

    struct Setup {
        curve_system_id: i64,
        uniswap_v4_system_id: i64,
        curve: InstanceId,
        uniswap_v4: InstanceId,
    }

    /// "vm:curve" and "uniswap_v4" each point to an instance. "vm:curve" has a second instance it
    /// does not point to, and "vm:old" has an instance but no pointer.
    async fn setup_protocols(conn: &mut AsyncPgConnection) -> Setup {
        let curve_system_id =
            db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
        let uniswap_v4_system_id =
            db_fixtures::insert_protocol_system(conn, "uniswap_v4".to_string()).await;
        let old_system_id = db_fixtures::insert_protocol_system(conn, "vm:old".to_string()).await;

        let curve = db_fixtures::insert_instance(conn, curve_system_id, "0.0.1").await;
        db_fixtures::insert_instance(conn, curve_system_id, "0.0.2").await;
        let uniswap_v4 = db_fixtures::insert_instance(conn, uniswap_v4_system_id, "0.0.1").await;
        db_fixtures::insert_instance(conn, old_system_id, "0.0.1").await;

        db_fixtures::set_active_instance(conn, curve_system_id, Some(curve)).await;
        db_fixtures::set_active_instance(conn, uniswap_v4_system_id, Some(uniswap_v4)).await;
        Setup { curve_system_id, uniswap_v4_system_id, curve, uniswap_v4 }
    }

    #[tokio::test]
    async fn test_active_instance_returns_the_pointer() {
        let mut conn = instance_test_connection().await;
        let protocol_system_id =
            db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let id = db_fixtures::insert_instance(&mut conn, protocol_system_id, "0.0.1").await;
        db_fixtures::set_active_instance(&mut conn, protocol_system_id, Some(id)).await;

        assert_eq!(
            active_instance("vm:curve", &mut conn)
                .await
                .unwrap(),
            Some(id)
        );
    }

    #[tokio::test]
    async fn test_active_instance_is_none_without_pointer() {
        let mut conn = instance_test_connection().await;
        let protocol_system_id =
            db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        db_fixtures::insert_instance(&mut conn, protocol_system_id, "0.0.1").await;

        assert_eq!(
            active_instance("vm:curve", &mut conn)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_active_instance_is_none_for_unknown_protocol() {
        let mut conn = instance_test_connection().await;

        assert_eq!(
            active_instance("not-a-protocol", &mut conn)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_active_instance_ids_lists_only_pointed_instances() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;

        let mut ids = active_instance_ids(&mut conn)
            .await
            .unwrap();
        ids.sort();

        let mut expected = vec![setup.uniswap_v4, setup.curve];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn test_active_instance_ids_is_empty_without_pointers() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;
        db_fixtures::set_active_instance(&mut conn, setup.curve_system_id, None).await;
        db_fixtures::set_active_instance(&mut conn, setup.uniswap_v4_system_id, None).await;

        assert_eq!(
            active_instance_ids(&mut conn)
                .await
                .unwrap(),
            Vec::new()
        );
    }

    fn day(d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, d)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    #[tokio::test]
    async fn test_active_instance_head_has_the_extraction_state() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;
        db_fixtures::set_instance_head(&mut conn, setup.curve, day(2)).await;
        db_fixtures::set_instance_head(&mut conn, setup.uniswap_v4, day(3)).await;

        let found = active_instance_head("vm:curve", &mut conn)
            .await
            .unwrap();

        assert_eq!(found.map(|i| (i.id, i.head)), Some((setup.curve, Some(day(2)))));
    }

    #[tokio::test]
    async fn test_active_instance_head_without_extraction_state() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;

        let found = active_instance_head("vm:curve", &mut conn)
            .await
            .unwrap();

        assert_eq!(found.map(|i| (i.id, i.head)), Some((setup.curve, None)));
    }

    #[tokio::test]
    async fn test_active_instance_head_of_an_inactive_or_unknown_protocol() {
        let mut conn = instance_test_connection().await;
        setup_protocols(&mut conn).await;

        let inactive = active_instance_head("vm:old", &mut conn)
            .await
            .unwrap();
        let unknown = active_instance_head("not-a-protocol", &mut conn)
            .await
            .unwrap();

        assert_eq!(inactive, None);
        assert_eq!(unknown, None);
    }

    #[tokio::test]
    async fn test_active_instance_head_has_the_base_block() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;
        diesel::update(protocol_instance::table.filter(protocol_instance::id.eq(setup.curve)))
            .set(protocol_instance::base_block_ts.eq(day(1)))
            .execute(&mut conn)
            .await
            .unwrap();

        let curve = active_instance_head("vm:curve", &mut conn)
            .await
            .unwrap()
            .unwrap();
        let uniswap_v4 = active_instance_head("uniswap_v4", &mut conn)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(curve.base, Some(day(1)));
        assert_eq!(uniswap_v4.base, None);
    }

    #[tokio::test]
    async fn test_active_instance_head_has_the_retention_horizon() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;
        diesel::sql_query("UPDATE partition_retention_config SET retention = '7 days'")
            .execute(&mut conn)
            .await
            .unwrap();
        let expected = diesel::select(diesel::dsl::sql::<Timestamptz>("now() - interval '7 days'"))
            .get_result::<NaiveDateTime>(&mut conn)
            .await
            .unwrap();

        let found = active_instance_head("vm:curve", &mut conn)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(found.id, setup.curve);
        assert_eq!(found.retention_horizon, expected);
    }
}
