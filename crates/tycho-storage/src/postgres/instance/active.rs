//! # Active-instance filters
//!
//! Readers see the instance that `public.protocol_system.active_instance_id` points to.
//!
//! - one protocol: look up the instance once per read with [`active_instance`] and filter
//!   `instance_id.eq(id)`. Postgres prunes a subselect only during execution, so with one it would
//!   plan and lock every leaf of each table.
//! - all protocols: `instance_id.eq_any(active_instance_ids())`.
//!
//! A protocol with no active instance yields no rows, and so does an unknown protocol name:
//! check the name first where it must exist.

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::postgres::{instance::InstanceId, schema::protocol_system, PostgresError, StorageError};

/// The active instance of the protocol system `name`.
///
/// `None` if the protocol system has no active instance, or if no protocol system has that name.
/// Filter with the returned id as a bound value: Postgres then plans and locks only that
/// instance's leaves.
pub async fn active_instance(
    conn: &mut AsyncPgConnection,
    name: &str,
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

/// The active instance of every protocol system that has one, as a subselect.
///
/// Instances no protocol system points to are absent, even when `ready`. Empty if no protocol
/// system has an active instance. Loads as `Vec<InstanceId>`.
#[diesel::dsl::auto_type]
pub fn active_instance_ids() -> _ {
    protocol_system::table
        .filter(protocol_system::active_instance_id.is_not_null())
        .select(protocol_system::active_instance_id.assume_not_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::{
        db_fixtures, instance::schema::tycho::protocol_instance, testing::instance_test_connection,
    };

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
            active_instance(&mut conn, "vm:curve")
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
            active_instance(&mut conn, "vm:curve")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_active_instance_is_none_for_unknown_protocol() {
        let mut conn = instance_test_connection().await;

        assert_eq!(
            active_instance(&mut conn, "not-a-protocol")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_active_instance_ids_lists_only_pointed_instances() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;

        let mut ids: Vec<InstanceId> = active_instance_ids()
            .load(&mut conn)
            .await
            .expect("loading the active instances failed");
        ids.sort();

        let mut expected = vec![setup.uniswap_v4, setup.curve];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn test_active_instance_filters_compose_on_instance_tables() {
        let mut conn = instance_test_connection().await;
        let setup = setup_protocols(&mut conn).await;

        let active: i64 = protocol_instance::table
            .filter(protocol_instance::id.eq_any(active_instance_ids()))
            .count()
            .get_result(&mut conn)
            .await
            .expect("counting the active instances failed");
        assert_eq!(active, 2);

        db_fixtures::set_active_instance(&mut conn, setup.curve_system_id, None).await;
        db_fixtures::set_active_instance(&mut conn, setup.uniswap_v4_system_id, None).await;
        let active: i64 = protocol_instance::table
            .filter(protocol_instance::id.eq_any(active_instance_ids()))
            .count()
            .get_result(&mut conn)
            .await
            .expect("counting the active instances failed");
        assert_eq!(active, 0);
    }
}
