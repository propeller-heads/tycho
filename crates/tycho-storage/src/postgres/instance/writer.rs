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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::Duration;
    use diesel_async::AsyncConnection;
    use tycho_common::models::{
        blockchain::{Block, Transaction},
        Chain,
    };

    use super::{super::orm::ExtractionState as StoredState, *};
    use crate::postgres::{db_fixtures, schema};

    struct Setup {
        conn: AsyncPgConnection,
        gateway: PostgresGateway,
        instance: InstanceId,
    }

    async fn setup() -> Setup {
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for DB tests");
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .expect("connecting to the test database failed");
        conn.begin_test_transaction()
            .await
            .expect("starting the test transaction failed");
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
        db_fixtures::insert_token(
            &mut conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let protocol_system_id =
            db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, protocol_system_id, "0.0.2").await;
        let gateway = PostgresGateway::from_connection(&mut conn).await;
        Setup { conn, gateway, instance }
    }

    fn block(number: u64) -> Block {
        let ts = db_fixtures::yesterday_one_am() +
            Duration::seconds(12 * i64::try_from(number).unwrap());
        Block::new(
            number,
            Chain::Ethereum,
            Bytes::from(number.to_be_bytes().to_vec()),
            Bytes::from((number - 1).to_be_bytes().to_vec()),
            ts,
        )
    }

    fn tx(block: &Block, index: u64) -> Transaction {
        Transaction::new(
            Bytes::from(vec![u8::try_from(index).unwrap(); 32]),
            block.hash.clone(),
            Bytes::from_str("0x01").unwrap(),
            None,
            index,
        )
    }

    fn state(block: &Block, cursor: &str) -> ExtractionState {
        ExtractionState::new(
            "vm:curve".to_string(),
            Chain::Ethereum,
            Some(serde_json::json!({"key": cursor})),
            cursor.as_bytes(),
            block.hash.clone(),
        )
    }

    async fn write_batch(setup: &mut Setup, ops: &[WriteOp]) {
        let instance = setup.instance;
        write_batch_as(setup, instance, ops).await;
    }

    async fn write_batch_as(setup: &mut Setup, instance: InstanceId, ops: &[WriteOp]) {
        let writer = InstanceWriter::new(instance, &setup.gateway, ops).expect("origins resolve");
        for op in ops {
            writer
                .write_op(op, &mut setup.conn)
                .await
                .unwrap_or_else(|e| panic!("{} failed: {:?}", op.variant_name(), e.0));
        }
    }

    async fn stored_state(conn: &mut AsyncPgConnection) -> Vec<StoredState> {
        extraction_state::table
            .select(StoredState::as_select())
            .order_by(extraction_state::instance_id)
            .load(conn)
            .await
            .expect("reading extraction state failed")
    }

    async fn transaction_count(conn: &mut AsyncPgConnection) -> i64 {
        schema::transaction::table
            .count()
            .get_result(conn)
            .await
            .expect("counting transactions failed")
    }

    #[tokio::test]
    async fn test_writes_block_and_state_without_transactions() {
        let mut setup = setup().await;
        let b1 = block(1);
        let ops = vec![
            WriteOp::UpsertBlock(vec![b1.clone()]),
            WriteOp::UpsertTx(vec![tx(&b1, 0), tx(&b1, 1)]),
            WriteOp::SaveExtractionState(state(&b1, "cursor@1")),
        ];

        write_batch(&mut setup, &ops).await;

        let stored_block = orm::Block::by_hash(&b1.hash, &mut setup.conn)
            .await
            .expect("block stored");
        assert_eq!((stored_block.number, stored_block.ts), (1, b1.ts));
        assert_eq!(transaction_count(&mut setup.conn).await, 0);
        let rows = stored_state(&mut setup.conn).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.instance_id, setup.instance);
        assert_eq!(row.cursor, Some(Bytes::from("cursor@1".as_bytes().to_vec())));
        assert_eq!((row.block_number, &row.block_hash, row.block_ts), (1, &b1.hash, b1.ts));
        assert_eq!(row.attributes, Some(serde_json::json!({"key": "cursor@1"})));
    }

    #[tokio::test]
    async fn test_next_batch_replaces_state() {
        let mut setup = setup().await;
        let (b1, b2) = (block(1), block(2));
        let first = vec![
            WriteOp::UpsertBlock(vec![b1.clone()]),
            WriteOp::SaveExtractionState(state(&b1, "cursor@1")),
        ];
        let second = vec![
            WriteOp::UpsertBlock(vec![b2.clone()]),
            WriteOp::SaveExtractionState(state(&b2, "cursor@2")),
        ];

        write_batch(&mut setup, &first).await;
        write_batch(&mut setup, &second).await;

        let rows = stored_state(&mut setup.conn).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cursor, Some(Bytes::from("cursor@2".as_bytes().to_vec())));
        assert_eq!((rows[0].block_number, rows[0].block_ts), (2, b2.ts));
    }

    #[tokio::test]
    async fn test_state_takes_stored_block_outside_batch() {
        let mut setup = setup().await;
        let b1 = block(1);
        write_batch(&mut setup, &[WriteOp::UpsertBlock(vec![b1.clone()])]).await;

        write_batch(&mut setup, &[WriteOp::SaveExtractionState(state(&b1, "cursor@1"))]).await;

        let rows = stored_state(&mut setup.conn).await;
        assert_eq!((rows[0].block_number, rows[0].block_ts), (1, b1.ts));
    }

    #[tokio::test]
    async fn test_state_of_unknown_block_fails() {
        let mut setup = setup().await;
        let op = WriteOp::SaveExtractionState(state(&block(7), "cursor@7"));
        let writer = InstanceWriter::new(setup.instance, &setup.gateway, slice_of(&op)).unwrap();

        let res = writer
            .write_op(&op, &mut setup.conn)
            .await;

        assert!(
            matches!(res, Err(PostgresError(StorageError::NoRelatedEntity(_, _, _)))),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn test_each_instance_keeps_its_own_state() {
        let mut setup = setup().await;
        let other_system =
            db_fixtures::insert_protocol_system(&mut setup.conn, "uniswap_v4".to_string()).await;
        let other = db_fixtures::insert_instance(&mut setup.conn, other_system, "3").await;
        let (b1, b2) = (block(1), block(2));

        write_batch(
            &mut setup,
            &[
                WriteOp::UpsertBlock(vec![b1.clone(), b2.clone()]),
                WriteOp::SaveExtractionState(state(&b2, "curve@2")),
            ],
        )
        .await;
        write_batch_as(&mut setup, other, &[WriteOp::SaveExtractionState(state(&b1, "v4@1"))])
            .await;

        let rows = stored_state(&mut setup.conn).await;
        let by_instance: Vec<(InstanceId, i64)> = rows
            .iter()
            .map(|r| (r.instance_id, r.block_number))
            .collect();
        assert_eq!(by_instance, vec![(setup.instance, 2), (other, 1)]);
    }

    #[tokio::test]
    async fn test_unsupported_op_fails() {
        let mut setup = setup().await;
        let op = WriteOp::InsertTokens(vec![]);
        let writer = InstanceWriter::new(setup.instance, &setup.gateway, slice_of(&op)).unwrap();

        let res = writer
            .write_op(&op, &mut setup.conn)
            .await;

        assert!(matches!(res, Err(PostgresError(StorageError::Unsupported(_)))), "{res:?}");
    }

    fn slice_of(op: &WriteOp) -> &[WriteOp] {
        std::slice::from_ref(op)
    }
}
