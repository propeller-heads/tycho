//! Blocks, which stay in the shared `public.block` table.
use diesel::OptionalExtension;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tracing::{debug_span, warn, Instrument};
use tycho_common::{
    models::{blockchain::Block, BlockHash},
    storage::StorageError,
};

use crate::postgres::{
    convert_int,
    instance::{
        gateway::PostgresGatewayNew,
        origins::{BatchOrigins, BlockOrigin},
    },
    orm, storage_error_from_diesel, ChainEnumCache,
};

impl PostgresGatewayNew {
    /// Runs [`upsert_blocks`] with this gateway's chain cache.
    pub(crate) async fn upsert_block(
        &self,
        blocks: &[Block],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        upsert_blocks(&self.chain_id_cache, blocks, conn).await
    }
}

/// Returns the number and timestamp of the block with `hash`: from `origins`, or from the shared
/// block table when the batch doesn't carry the block.
///
/// Fails with [`StorageError::NoRelatedEntity`] naming `related` when neither has it.
pub(super) async fn block_origin(
    origins: &BatchOrigins,
    hash: &BlockHash,
    related: &str,
    conn: &mut AsyncPgConnection,
) -> Result<BlockOrigin, StorageError> {
    if let Some(block) = origins.block(hash) {
        return Ok(block);
    }
    let stored = orm::Block::by_hash(hash, conn)
        .await
        .optional()
        .map_err(|err| storage_error_from_diesel(err, "Block", &hash.to_string(), None))?
        .ok_or_else(|| {
            StorageError::NoRelatedEntity("Block".to_owned(), related.to_owned(), hash.to_string())
        })?;
    Ok(BlockOrigin { number: stored.number, ts: stored.ts })
}

/// Inserts `blocks` into the shared block table, all on the chain of the first block. A block whose
/// hash is already stored is left as it is.
///
/// Fails with [`StorageError::NotFound`] when the chain is unknown, and with
/// [`StorageError::DecodeError`] when a block number does not fit its column.
pub async fn upsert_blocks(
    chain_ids: &ChainEnumCache,
    blocks: &[Block],
    conn: &mut AsyncPgConnection,
) -> Result<(), StorageError> {
    use crate::postgres::schema::block::dsl::*;
    if blocks.is_empty() {
        warn!("Upsert blocks called with empty blocks!");
        return Ok(());
    }
    let block_chain_id = chain_ids.try_get_id(&blocks[0].chain)?;
    let new_blocks = blocks
        .iter()
        .map(|new| {
            Ok(orm::NewBlock {
                hash: new.hash.clone(),
                parent_hash: new.parent_hash.clone(),
                chain_id: block_chain_id,
                main: true,
                number: convert_int(new.number, "block number")?,
                ts: new.ts,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;

    // Insert in batches to avoid exceeding PostgreSQL parameter limit
    // assumes that block with the same hash will not appear with different values
    let block_count = new_blocks.len();
    async {
        for chunk in new_blocks.chunks(orm::NewBlock::MAX_BATCH_SIZE) {
            diesel::insert_into(block)
                .values(chunk)
                .on_conflict_do_nothing()
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "Block",
                        &format!("Batch: {} and {} more", chunk[0].hash, chunk.len() - 1),
                        None,
                    )
                })?;
        }
        Ok::<(), StorageError>(())
    }
    .instrument(debug_span!("insert_blocks", count = block_count))
    .await?;
    Ok(())
}
