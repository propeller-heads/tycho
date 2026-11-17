use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{
        protocol::{ComponentPause, PauseRecord, UnpauseRecord},
        Chain, ComponentId,
    },
    storage::StorageError,
};

use super::{orm, schema, storage_error_from_diesel, PostgresGateway};

impl PostgresGateway {
    /// Resolves a protocol system id from the cache, or from the table if another process added
    /// the system after the cache was loaded.
    async fn load_protocol_system_id(
        &self,
        system: &str,
        conn: &mut AsyncPgConnection,
    ) -> Result<i64, StorageError> {
        use schema::protocol_system::dsl as ps;

        if let Ok(id) = self.get_protocol_system_id(&system.to_string()) {
            return Ok(id);
        }
        ps::protocol_system
            .filter(ps::name.eq(system))
            .select(ps::id)
            .first(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "ProtocolSystem", system, None).into())
    }

    /// Inserts one open pause row per record, skipping components that already have one.
    ///
    /// Returns the component ids of the inserted rows.
    pub async fn pause_components(
        &self,
        chain: &Chain,
        system: &str,
        pauses: &[PauseRecord],
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentId>, StorageError> {
        use schema::component_pause::dsl as cp;

        let chain_id = self.get_chain_id(chain)?;
        let system_id = self
            .load_protocol_system_id(system, conn)
            .await?;
        if pauses.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<_> = pauses
            .iter()
            .map(|pause| orm::NewComponentPause {
                chain_id,
                protocol_system_id: system_id,
                component_id: &pause.component_id,
                reason: pause.reason.as_deref(),
                actor: pause.actor.as_deref(),
                evidence_url: pause.evidence_url.as_deref(),
            })
            .collect();

        let mut paused = Vec::with_capacity(rows.len());
        for chunk in rows.chunks(orm::NewComponentPause::MAX_BATCH_SIZE) {
            let mut chunk_paused: Vec<ComponentId> = diesel::insert_into(cp::component_pause)
                .values(chunk)
                .on_conflict((cp::chain_id, cp::component_id))
                .filter_target(cp::unpaused_at.is_null())
                .do_nothing()
                .returning(cp::component_id)
                .get_results(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "ComponentPause", system, None))?;
            paused.append(&mut chunk_paused);
        }
        Ok(paused)
    }

    /// Closes the open pause rows of the given components.
    ///
    /// Returns the component ids of the closed rows.
    pub async fn unpause_components(
        &self,
        chain: &Chain,
        component_ids: &[&str],
        unpause: &UnpauseRecord,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentId>, StorageError> {
        use schema::component_pause::dsl as cp;

        let chain_id = self.get_chain_id(chain)?;
        if component_ids.is_empty() {
            return Ok(Vec::new());
        }
        diesel::update(cp::component_pause)
            .filter(cp::chain_id.eq(chain_id))
            .filter(cp::component_id.eq_any(component_ids))
            .filter(cp::unpaused_at.is_null())
            .set((
                cp::unpaused_at.eq(diesel::dsl::now),
                cp::unpause_actor.eq(unpause.actor.as_deref()),
                cp::unpause_reason.eq(unpause.reason.as_deref()),
            ))
            .returning(cp::component_id)
            .get_results(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ComponentPause", &chain.to_string(), None).into()
            })
    }

    /// Loads the open pause rows of a chain, ordered by protocol system and component id.
    pub async fn get_component_pauses(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Vec<ComponentPause>, StorageError> {
        use schema::{component_pause::dsl as cp, protocol_system::dsl as ps};

        let chain_id = self.get_chain_id(chain)?;
        let rows: Vec<orm::OpenComponentPause> = cp::component_pause
            .inner_join(ps::protocol_system)
            .filter(cp::chain_id.eq(chain_id))
            .filter(cp::unpaused_at.is_null())
            .order_by((ps::name, cp::component_id))
            .select((
                ps::name,
                cp::component_id,
                cp::reason,
                cp::actor,
                cp::evidence_url,
                cp::paused_at,
            ))
            .load(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ComponentPause", &chain.to_string(), None)
            })?;

        Ok(rows
            .into_iter()
            .map(|row| ComponentPause {
                protocol_system: row.protocol_system,
                record: PauseRecord {
                    component_id: row.component_id,
                    reason: row.reason,
                    actor: row.actor,
                    evidence_url: row.evidence_url,
                },
                paused_since: row.paused_at,
            })
            .collect())
    }
}

#[cfg(test)]
mod test_serial_db {
    use chrono::NaiveDateTime;
    use diesel_async::AsyncConnection;
    use tycho_common::models::{FinancialType, ImplementationType};

    use super::*;
    use crate::postgres::db_fixtures;

    const SYSTEM: &str = "ambient";
    const OTHER_SYSTEM: &str = "zigzag";

    /// Indexed component of `SYSTEM` on Ethereum, created by `setup_pause_gateway`.
    const INDEXED: &str = "state1";

    async fn setup_db() -> AsyncPgConnection {
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .expect("failed to connect to the database");
        conn.begin_test_transaction()
            .await
            .expect("failed to begin a test transaction");
        conn
    }

    /// Sets up Ethereum and Starknet, both protocol systems and the `INDEXED` component.
    async fn setup_pause_gateway(conn: &mut AsyncPgConnection) -> PostgresGateway {
        let mut chain_ids = Vec::new();
        // The gateway loads each chain's native token when it is built.
        for chain in ["ethereum", "starknet"] {
            let chain_id = db_fixtures::insert_chain(conn, chain).await;
            db_fixtures::insert_token(
                conn,
                chain_id,
                "0000000000000000000000000000000000000000",
                "ETH",
                18,
                Some(100),
            )
            .await;
            chain_ids.push(chain_id);
        }
        let blocks = db_fixtures::insert_blocks(conn, chain_ids[0]).await;
        let tx_hash = "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945";
        let txns = db_fixtures::insert_txns(conn, &[(blocks[0], 1, tx_hash)]).await;
        let system_id = db_fixtures::insert_protocol_system(conn, SYSTEM.to_owned()).await;
        db_fixtures::insert_protocol_system(conn, OTHER_SYSTEM.to_owned()).await;
        let type_id = db_fixtures::insert_protocol_type(
            conn,
            "Pool",
            Some(FinancialType::Swap),
            None,
            Some(ImplementationType::Custom),
        )
        .await;
        db_fixtures::insert_protocol_component(
            conn,
            INDEXED,
            chain_ids[0],
            system_id,
            type_id,
            txns[0],
            None,
            None,
        )
        .await;
        PostgresGateway::from_connection(conn).await
    }

    /// Inserts an open pause of `SYSTEM` without going through `pause_components`.
    async fn insert_open_pause(
        gw: &PostgresGateway,
        conn: &mut AsyncPgConnection,
        chain: &Chain,
        component_id: &str,
    ) {
        let row = orm::NewComponentPause {
            chain_id: gw
                .get_chain_id(chain)
                .expect("chain is set up"),
            protocol_system_id: gw
                .get_protocol_system_id(&SYSTEM.to_owned())
                .expect("protocol system is set up"),
            component_id,
            reason: None,
            actor: None,
            evidence_url: None,
        };
        diesel::insert_into(schema::component_pause::table)
            .values(&row)
            .execute(conn)
            .await
            .expect("failed to insert an open pause");
    }

    fn pause(component_id: &str, reason: &str) -> PauseRecord {
        PauseRecord {
            component_id: component_id.to_owned(),
            reason: Some(reason.to_owned()),
            actor: Some("watcher".to_owned()),
            evidence_url: Some("https://example.com/evidence".to_owned()),
        }
    }

    fn unpause() -> UnpauseRecord {
        UnpauseRecord {
            actor: Some("operator".to_owned()),
            reason: Some("pool is healthy".to_owned()),
        }
    }

    /// The `unpaused_at` of every row stored for a component, open or closed, oldest first.
    async fn stored_rows(conn: &mut AsyncPgConnection, id: &str) -> Vec<Option<NaiveDateTime>> {
        use schema::component_pause::dsl as cp;
        cp::component_pause
            .filter(cp::component_id.eq(id))
            .order_by(cp::id)
            .select(cp::unpaused_at)
            .load(conn)
            .await
            .expect("failed to read component_pause rows")
    }

    fn paused_ids(pauses: &[ComponentPause]) -> Vec<&str> {
        pauses
            .iter()
            .map(|p| p.record.component_id.as_str())
            .collect()
    }

    #[tokio::test]
    async fn test_pause_components_twice() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;

        let mut first = gw
            .pause_components(
                &Chain::Ethereum,
                SYSTEM,
                &[pause(INDEXED, "first"), pause("pool_b", "first")],
                &mut conn,
            )
            .await
            .expect("first pause failed");
        let second = gw
            .pause_components(
                &Chain::Ethereum,
                SYSTEM,
                &[pause(INDEXED, "second"), pause("pool_c", "second")],
                &mut conn,
            )
            .await
            .expect("second pause failed");

        first.sort();
        assert_eq!(first, vec!["pool_b", INDEXED]);
        assert_eq!(second, vec!["pool_c"]);
        let paused = gw
            .get_component_pauses(&Chain::Ethereum, &mut conn)
            .await
            .expect("reading pauses failed");
        assert_eq!(paused_ids(&paused), vec!["pool_b", "pool_c", INDEXED]);
        assert_eq!(paused[2].protocol_system, SYSTEM);
        assert_eq!(paused[2].record, pause(INDEXED, "first"));
        assert_eq!(stored_rows(&mut conn, INDEXED).await, vec![None]);
    }

    #[tokio::test]
    async fn test_pause_components_duplicate_ids() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;

        let paused = gw
            .pause_components(
                &Chain::Ethereum,
                SYSTEM,
                &[pause("pool_a", "first"), pause("pool_a", "second")],
                &mut conn,
            )
            .await
            .expect("pause failed");

        assert_eq!(paused, vec!["pool_a"]);
        assert_eq!(stored_rows(&mut conn, "pool_a").await, vec![None]);
    }

    #[tokio::test]
    async fn test_pause_components_over_parameter_limit() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        // More pauses than one statement can bind, so only a batched insert succeeds.
        let count = usize::from(u16::MAX) / orm::NewComponentPause::FIELD_COUNT + 1;
        let pauses: Vec<_> = (0..count)
            .map(|i| pause(&format!("pool_{i}"), "r"))
            .collect();

        let paused = gw
            .pause_components(&Chain::Ethereum, SYSTEM, &pauses, &mut conn)
            .await
            .expect("pause failed");

        assert_eq!(paused.len(), count);
    }

    #[tokio::test]
    async fn test_pause_components_other_system() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        gw.pause_components(&Chain::Ethereum, SYSTEM, &[pause("pool_a", "r")], &mut conn)
            .await
            .expect("first pause failed");

        let second = gw
            .pause_components(&Chain::Ethereum, OTHER_SYSTEM, &[pause("pool_a", "r")], &mut conn)
            .await
            .expect("second pause failed");

        assert!(second.is_empty());
        assert_eq!(stored_rows(&mut conn, "pool_a").await, vec![None]);
    }

    #[tokio::test]
    async fn test_unpause_components_other_chain() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        for chain in [Chain::Ethereum, Chain::Starknet] {
            insert_open_pause(&gw, &mut conn, &chain, "pool_a").await;
        }

        gw.unpause_components(&Chain::Ethereum, &["pool_a"], &unpause(), &mut conn)
            .await
            .expect("unpause failed");

        let paused = gw
            .get_component_pauses(&Chain::Starknet, &mut conn)
            .await
            .expect("reading pauses failed");
        assert_eq!(paused_ids(&paused), vec!["pool_a"]);
    }

    #[tokio::test]
    async fn test_unpause_components_twice() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        for component_id in ["pool_a", "pool_b"] {
            insert_open_pause(&gw, &mut conn, &Chain::Ethereum, component_id).await;
        }

        let first = gw
            .unpause_components(
                &Chain::Ethereum,
                &["pool_a", "never_paused"],
                &unpause(),
                &mut conn,
            )
            .await
            .expect("first unpause failed");
        let second = gw
            .unpause_components(&Chain::Ethereum, &["pool_a"], &unpause(), &mut conn)
            .await
            .expect("second unpause failed");

        assert_eq!(first, vec!["pool_a"]);
        assert!(second.is_empty());
        let paused = gw
            .get_component_pauses(&Chain::Ethereum, &mut conn)
            .await
            .expect("reading pauses failed");
        assert_eq!(paused_ids(&paused), vec!["pool_b"]);
        let rows = stored_rows(&mut conn, "pool_a").await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_some(), "the unpaused row is kept as history");
    }

    #[tokio::test]
    async fn test_unpause_components_audit() {
        use schema::component_pause::dsl as cp;
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        insert_open_pause(&gw, &mut conn, &Chain::Ethereum, "pool_a").await;

        gw.unpause_components(&Chain::Ethereum, &["pool_a"], &unpause(), &mut conn)
            .await
            .expect("unpause failed");

        let audit: (Option<String>, Option<String>) = cp::component_pause
            .filter(cp::component_id.eq("pool_a"))
            .select((cp::unpause_actor, cp::unpause_reason))
            .first(&mut conn)
            .await
            .expect("failed to read the closed pause");
        let expected = unpause();
        assert_eq!(audit, (expected.actor, expected.reason));
    }

    #[tokio::test]
    async fn test_pause_after_unpause() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        let pauses = [pause("pool_a", "r")];
        gw.pause_components(&Chain::Ethereum, SYSTEM, &pauses, &mut conn)
            .await
            .expect("pause failed");
        gw.unpause_components(&Chain::Ethereum, &["pool_a"], &unpause(), &mut conn)
            .await
            .expect("unpause failed");

        let repaused = gw
            .pause_components(&Chain::Ethereum, SYSTEM, &pauses, &mut conn)
            .await
            .expect("second pause failed");

        assert_eq!(repaused, vec!["pool_a"]);
        let rows = stored_rows(&mut conn, "pool_a").await;
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_some());
        assert!(rows[1].is_none());
    }

    #[tokio::test]
    async fn test_pause_components_not_indexed() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;

        let paused = gw
            .pause_components(&Chain::Ethereum, SYSTEM, &[pause("not_indexed", "r")], &mut conn)
            .await
            .expect("pause failed");

        assert_eq!(paused, vec!["not_indexed"]);
        let stored = gw
            .get_component_pauses(&Chain::Ethereum, &mut conn)
            .await
            .expect("reading pauses failed");
        assert_eq!(paused_ids(&stored), vec!["not_indexed"]);
    }

    #[tokio::test]
    async fn test_get_component_pauses_other_chain() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        gw.pause_components(&Chain::Starknet, SYSTEM, &[pause("pool_a", "r")], &mut conn)
            .await
            .expect("pause failed");

        let paused = gw
            .get_component_pauses(&Chain::Ethereum, &mut conn)
            .await
            .expect("reading pauses failed");

        assert!(paused.is_empty());
    }

    #[tokio::test]
    async fn test_pause_components_system_added_after_startup() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        db_fixtures::insert_protocol_system(&mut conn, "new_system".to_owned()).await;

        let paused = gw
            .pause_components(&Chain::Ethereum, "new_system", &[pause("pool_a", "r")], &mut conn)
            .await
            .expect("pause failed");

        assert_eq!(paused, vec!["pool_a"]);
    }

    #[tokio::test]
    async fn test_pause_components_empty() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;

        let paused = gw
            .pause_components(&Chain::Ethereum, SYSTEM, &[], &mut conn)
            .await
            .expect("pause failed");

        assert!(paused.is_empty());
    }

    #[tokio::test]
    async fn test_unpause_components_empty() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;
        insert_open_pause(&gw, &mut conn, &Chain::Ethereum, "pool_a").await;

        let unpaused = gw
            .unpause_components(&Chain::Ethereum, &[], &unpause(), &mut conn)
            .await
            .expect("unpause failed");

        assert!(unpaused.is_empty());
        assert_eq!(stored_rows(&mut conn, "pool_a").await, vec![None]);
    }

    #[tokio::test]
    async fn test_pause_components_unknown_system() {
        let mut conn = setup_db().await;
        let gw = setup_pause_gateway(&mut conn).await;

        let res = gw
            .pause_components(&Chain::Ethereum, "unknown", &[pause("pool_a", "r")], &mut conn)
            .await;

        assert!(matches!(res, Err(StorageError::NotFound(..))), "got {res:?}");
    }
}
