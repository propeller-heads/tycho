//! The cursor and last committed block of each instance.
use diesel::{prelude::*, upsert::excluded};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::ExtractionState, storage::StorageError, Bytes};

use super::block::block_origin;
use crate::postgres::{
    instance::{
        gateway::PostgresGatewayNew, origins::BatchOrigins, orm::NewExtractionState,
        schema::tycho::extraction_state, InstanceId,
    },
    storage_error_from_diesel,
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
        let block = block_origin(origins, &state.block_hash, "ExtractionState", conn).await?;
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

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use tycho_common::models::{blockchain::Block, Chain};

    use super::*;
    use crate::postgres::{
        cache::WriteOp, db_fixtures, instance::orm::ExtractionState as StoredState,
        testing::instance_test_connection,
    };

    struct Setup {
        conn: AsyncPgConnection,
        gateway: PostgresGatewayNew,
        instance: InstanceId,
    }

    async fn setup() -> Setup {
        let mut conn = instance_test_connection().await;
        db_fixtures::insert_chain(&mut conn, "ethereum").await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
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

    fn state(block: &Block, cursor: &str) -> ExtractionState {
        ExtractionState::new(
            "vm:curve".to_string(),
            Chain::Ethereum,
            Some(serde_json::json!({"key": cursor})),
            cursor.as_bytes(),
            block.hash.clone(),
        )
    }

    fn origins_of(blocks: &[Block]) -> BatchOrigins {
        BatchOrigins::from_ops(&[WriteOp::UpsertBlock(blocks.to_vec())]).expect("origins resolve")
    }

    async fn stored_state(conn: &mut AsyncPgConnection) -> Vec<StoredState> {
        extraction_state::table
            .select(StoredState::as_select())
            .order_by(extraction_state::instance_id)
            .load(conn)
            .await
            .expect("reading extraction state failed")
    }

    #[tokio::test]
    async fn test_saves_state_with_block_from_batch() {
        let mut setup = setup().await;
        let b1 = block(1);

        setup
            .gateway
            .save_state(
                setup.instance,
                &origins_of(std::slice::from_ref(&b1)),
                &state(&b1, "c1"),
                &mut setup.conn,
            )
            .await
            .expect("saving the state failed");

        let rows = stored_state(&mut setup.conn).await;
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.instance_id, setup.instance);
        assert_eq!(row.cursor, Some(Bytes::from("c1".as_bytes().to_vec())));
        assert_eq!((row.block_number, &row.block_hash, row.block_ts), (1, &b1.hash, b1.ts));
        assert_eq!(row.attributes, Some(serde_json::json!({"key": "c1"})));
    }

    #[tokio::test]
    async fn test_next_save_replaces_state() {
        let mut setup = setup().await;
        let (b1, b2) = (block(1), block(2));
        let origins = origins_of(&[b1.clone(), b2.clone()]);

        for (b, cursor) in [(&b1, "c1"), (&b2, "c2")] {
            setup
                .gateway
                .save_state(setup.instance, &origins, &state(b, cursor), &mut setup.conn)
                .await
                .expect("saving the state failed");
        }

        let rows = stored_state(&mut setup.conn).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cursor, Some(Bytes::from("c2".as_bytes().to_vec())));
        assert_eq!(
            (rows[0].block_number, &rows[0].block_hash, rows[0].block_ts),
            (2, &b2.hash, b2.ts)
        );
        assert_eq!(rows[0].attributes, Some(serde_json::json!({"key": "c2"})));
    }

    #[tokio::test]
    async fn test_state_takes_stored_block_outside_batch() {
        let mut setup = setup().await;
        let b1 = block(1);
        setup
            .gateway
            .upsert_block(std::slice::from_ref(&b1), &mut setup.conn)
            .await
            .expect("storing the block failed");

        setup
            .gateway
            .save_state(
                setup.instance,
                &BatchOrigins::default(),
                &state(&b1, "c1"),
                &mut setup.conn,
            )
            .await
            .expect("saving the state failed");

        let rows = stored_state(&mut setup.conn).await;
        assert_eq!((rows[0].block_number, rows[0].block_ts), (1, b1.ts));
    }

    #[tokio::test]
    async fn test_state_of_unknown_block_fails() {
        let mut setup = setup().await;
        let b7 = block(7);

        let res = setup
            .gateway
            .save_state(
                setup.instance,
                &BatchOrigins::default(),
                &state(&b7, "c7"),
                &mut setup.conn,
            )
            .await;

        assert!(matches!(res, Err(StorageError::NoRelatedEntity(_, _, _))), "{res:?}");
    }

    #[tokio::test]
    async fn test_each_instance_keeps_its_own_state() {
        let mut setup = setup().await;
        let system =
            db_fixtures::insert_protocol_system(&mut setup.conn, "uniswap_v4".to_string()).await;
        let other = db_fixtures::insert_instance(&mut setup.conn, system, "3").await;
        let (b1, b2) = (block(1), block(2));
        let origins = origins_of(&[b1.clone(), b2.clone()]);

        for (instance, b) in [(setup.instance, &b2), (other, &b1)] {
            setup
                .gateway
                .save_state(instance, &origins, &state(b, "c"), &mut setup.conn)
                .await
                .expect("saving the state failed");
        }

        let rows = stored_state(&mut setup.conn).await;
        let by_instance: Vec<(InstanceId, i64)> = rows
            .iter()
            .map(|r| (r.instance_id, r.block_number))
            .collect();
        assert_eq!(by_instance, vec![(setup.instance, 2), (other, 1)]);
    }
}
