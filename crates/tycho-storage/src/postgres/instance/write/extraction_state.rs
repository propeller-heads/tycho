//! The cursor and last committed block of each instance.
use diesel::{prelude::*, upsert::excluded};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::ExtractionState, storage::StorageError, Bytes};

use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew,
        origins::{BatchOrigins, BlockOrigin},
        orm::NewExtractionState,
        schema::tycho::extraction_state,
        InstanceId,
    },
    orm, storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Saves the cursor and the last committed block of `instance`, replacing the previous ones.
    ///
    /// `instance` alone selects the row: the name and chain of `state` are not stored or checked.
    ///
    /// The block number and timestamp come from `origins`, or from the shared block table when the
    /// batch doesn't carry the block. Fails with [`StorageError::NoRelatedEntity`] when neither
    /// has it.
    pub(crate) async fn save_state(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        state: &ExtractionState,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let block = match origins.block(&state.block_hash) {
            Some(block) => block,
            None => {
                let stored = orm::Block::by_hash(&state.block_hash, conn)
                    .await
                    .optional()
                    .map_err(|err| {
                        storage_error_from_diesel(err, "Block", &state.block_hash.to_string(), None)
                    })?
                    .ok_or_else(|| {
                        StorageError::NoRelatedEntity(
                            "Block".to_owned(),
                            "ExtractionState".to_owned(),
                            state.block_hash.to_string(),
                        )
                    })?;
                BlockOrigin { number: stored.number, ts: stored.ts }
            }
        };
        let row = NewExtractionState {
            instance_id: instance,
            cursor: Some(Bytes::from(state.cursor.clone())),
            block_number: block.number,
            block_hash: state.block_hash.clone(),
            block_ts: block.ts,
            attributes: Some(state.attributes.clone()),
        };
        diesel::insert_into(extraction_state::table)
            .values(&row)
            .on_conflict(extraction_state::instance_id)
            .do_update()
            .set((
                extraction_state::cursor.eq(excluded(extraction_state::cursor)),
                extraction_state::block_number.eq(excluded(extraction_state::block_number)),
                extraction_state::block_hash.eq(excluded(extraction_state::block_hash)),
                extraction_state::block_ts.eq(excluded(extraction_state::block_ts)),
                extraction_state::attributes.eq(excluded(extraction_state::attributes)),
            ))
            .execute(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ExtractionState", &instance.to_string(), None)
            })?;
        Ok(())
    }
}

