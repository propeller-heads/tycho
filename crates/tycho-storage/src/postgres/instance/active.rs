//! # Active-instance filters
//!
//! SQL subselects on `public.protocol_system.active_instance_id`, the pointer to the instance
//! readers see. Filter an instance table's `instance_id` with them:
//!
//! - one protocol: `instance_id.nullable().eq(active_instance_of(name))`
//! - all protocols: `instance_id.eq_any(active_instance_ids())`
//!
//! Postgres reads the pointer on every query. A protocol with no active instance yields no rows,
//! and so does an unknown protocol name: check the name first where it must exist.

use diesel::prelude::*;

use crate::postgres::schema::protocol_system;

/// The active instance of the protocol system `name`, as a scalar subselect.
///
/// NULL if the protocol system has no active instance, or if no protocol system has that name.
/// Loads as `Option<InstanceId>`.
#[diesel::dsl::auto_type]
pub fn active_instance_of<'a>(name: &'a str) -> _ {
    protocol_system::table
        .filter(protocol_system::name.eq(name))
        .select(protocol_system::active_instance_id.assume_not_null())
        .single_value()
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
    use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

    use super::*;
    use crate::postgres::{
        db_fixtures,
        instance::{schema::tycho::protocol_instance, InstanceId},
    };

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

    // A test that creates instances inserts "vm:curve" first, as `instance/tests.rs` does, so
    // concurrent tests wait on its unique name before they create leaves. Otherwise one test holds
    // a name another wants while it waits for a leaf name in `pg_class`, and Postgres aborts one
    // of them as a deadlock.
    async fn insert_curve(conn: &mut AsyncPgConnection) -> i64 {
        db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await
    }

    async fn active_of(conn: &mut AsyncPgConnection, protocol_system: &str) -> Option<InstanceId> {
        diesel::select(active_instance_of(protocol_system))
            .get_result(conn)
            .await
            .expect("selecting the active instance failed")
    }

    struct Setup {
        curve_system_id: i64,
        uniswap_v4_system_id: i64,
        curve: InstanceId,
        uniswap_v4: InstanceId,
    }

    /// "vm:curve" and "uniswap_v4" each point to an instance. "vm:curve" has a second instance it
    /// does not point to, and "vm:old" has an instance but no pointer.
    async fn setup_protocols(conn: &mut AsyncPgConnection) -> Setup {
        let curve_system_id = insert_curve(conn).await;
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
    async fn test_active_instance_of_returns_the_pointer() {
        let mut conn = setup_db().await;
        let protocol_system_id = insert_curve(&mut conn).await;
        let id = db_fixtures::insert_instance(&mut conn, protocol_system_id, "0.0.1").await;
        db_fixtures::set_active_instance(&mut conn, protocol_system_id, Some(id)).await;

        assert_eq!(active_of(&mut conn, "vm:curve").await, Some(id));
    }

    #[tokio::test]
    async fn test_active_instance_of_is_null_without_pointer() {
        let mut conn = setup_db().await;
        let protocol_system_id = insert_curve(&mut conn).await;
        db_fixtures::insert_instance(&mut conn, protocol_system_id, "0.0.1").await;

        assert_eq!(active_of(&mut conn, "vm:curve").await, None);
    }

    #[tokio::test]
    async fn test_active_instance_of_is_null_for_unknown_protocol() {
        let mut conn = setup_db().await;

        assert_eq!(active_of(&mut conn, "not-a-protocol").await, None);
    }

    #[tokio::test]
    async fn test_active_instance_ids_lists_only_pointed_instances() {
        let mut conn = setup_db().await;
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
        let mut conn = setup_db().await;
        let setup = setup_protocols(&mut conn).await;

        let active: i64 = protocol_instance::table
            .filter(protocol_instance::id.eq_any(active_instance_ids()))
            .count()
            .get_result(&mut conn)
            .await
            .expect("counting the active instances failed");
        assert_eq!(active, 2);

        let curve: i64 = protocol_instance::table
            .filter(
                protocol_instance::id
                    .nullable()
                    .eq(active_instance_of("vm:curve")),
            )
            .count()
            .get_result(&mut conn)
            .await
            .expect("counting the active vm:curve instance failed");
        assert_eq!(curve, 1);

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
