//! Write path into the tables of one instance.
use diesel::{prelude::*, upsert::excluded};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::ExtractionState, storage::StorageError, Bytes};

use super::{
    origins::{BatchOrigins, BlockOrigin},
    orm::NewExtractionState,
    schema::tycho::extraction_state,
    InstanceId,
};
use crate::postgres::{
    cache::WriteOp, orm, storage_error_from_diesel, PostgresError, PostgresGateway,
};

/// Writes the ops of one batch into the tables of one instance.
///
/// Shared rows, such as blocks, go to the `public` tables. Transactions are not stored: instance
/// rows carry the block and index of the transaction that made them, taken from the batch. An op
/// this writer can't put into instance tables fails with [`StorageError::Unsupported`], so no
/// change is dropped without notice.
pub struct InstanceWriter<'a> {
    instance: InstanceId,
    gateway: &'a PostgresGateway,
    origins: BatchOrigins,
}

impl<'a> InstanceWriter<'a> {
    /// Prepares a writer for the batch `ops` of `instance`.
    ///
    /// Fails when the batch's transactions don't resolve to its blocks, see
    /// [`BatchOrigins::from_ops`].
    pub fn new(
        instance: InstanceId,
        gateway: &'a PostgresGateway,
        ops: &[WriteOp],
    ) -> Result<Self, StorageError> {
        Ok(Self { instance, gateway, origins: BatchOrigins::from_ops(ops)? })
    }

    /// Executes one op of the batch on `conn`.
    pub async fn write_op(
        &self,
        op: &WriteOp,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), PostgresError> {
        match op {
            WriteOp::UpsertBlock(blocks) => {
                self.gateway
                    .upsert_block(blocks, conn)
                    .await?
            }
            // Read into the origins when the writer was made; there is no table to write.
            WriteOp::UpsertTx(_) => {}
            WriteOp::SaveExtractionState(state) => {
                self.save_extraction_state(state, conn)
                    .await?
            }
            WriteOp::InsertContract(_) |
            WriteOp::UpdateContracts(_) |
            WriteOp::InsertAccountBalances(_) |
            WriteOp::InsertProtocolComponents(_) |
            WriteOp::InsertTokens(_) |
            WriteOp::UpdateTokens(_) |
            WriteOp::InsertComponentBalances(_) |
            WriteOp::UpsertProtocolState(_) |
            WriteOp::InsertEntryPoints(_) |
            WriteOp::InsertEntryPointTracingParams(_) |
            WriteOp::UpsertTracedEntryPoints(_) => {
                return Err(StorageError::Unsupported(format!(
                    "{} for instance {}",
                    op.variant_name(),
                    self.instance
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Saves the cursor and the last committed block of the instance, replacing the previous
    /// ones.
    async fn save_extraction_state(
        &self,
        state: &ExtractionState,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), PostgresError> {
        let block = match self.origins.block(&state.block_hash) {
            Some(block) => block,
            None => {
                let stored = orm::Block::by_hash(&state.block_hash, conn)
                    .await
                    .map_err(|err| {
                        storage_error_from_diesel(
                            err,
                            "Block",
                            &state.block_hash.to_string(),
                            Some("ExtractionState".to_owned()),
                        )
                    })?;
                BlockOrigin { number: stored.number, ts: stored.ts }
            }
        };
        let row = NewExtractionState {
            instance_id: self.instance,
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
                storage_error_from_diesel(err, "ExtractionState", &self.instance.to_string(), None)
            })?;
        Ok(())
    }
}
