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
