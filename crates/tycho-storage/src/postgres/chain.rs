use std::collections::HashMap;

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use itertools::Itertools;
use tracing::{debug_span, instrument, warn, Instrument};
use tycho_common::{
    models::{blockchain::*, BlockHash, TxHash},
    storage::{BlockIdentifier, StorageError},
    Bytes,
};

use super::{orm, schema, storage_error_from_diesel, PostgresError, PostgresGateway, MAX_TS};

impl PostgresGateway {
    #[instrument(skip_all)]
    pub async fn upsert_block(
        &self,
        blocks: &[Block],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use super::schema::block::dsl::*;
        if blocks.is_empty() {
            warn!("Upsert blocks called with empty blocks!");
            return Ok(());
        }
        let block_chain_id = self.get_chain_id(&blocks[0].chain)?;
        let new_blocks = blocks
            .iter()
            .map(|new| orm::NewBlock {
                hash: new.hash.clone(),
                parent_hash: new.parent_hash.clone(),
                chain_id: block_chain_id,
                main: true,
                number: new.number as i64,
                ts: new.ts,
            })
            .collect_vec();

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

    #[instrument(skip_all)]
    pub async fn get_block(
        &self,
        block_id: &BlockIdentifier,
        conn: &mut AsyncPgConnection,
    ) -> Result<Block, StorageError> {
        // taking a reference here is necessary, to not move block_id
        // so it can be used in the map_err closure later on. It would
        // be better if BlockIdentifier was copy though (complicates lifetimes).
        let mut orm_block = match &block_id {
            BlockIdentifier::Number((chain, number)) => {
                orm::Block::by_number(*chain, *number, conn).await
            }

            BlockIdentifier::Hash(block_hash) => orm::Block::by_hash(block_hash, conn).await,
            BlockIdentifier::Latest(chain) => orm::Block::most_recent(*chain, conn).await,
        }
        .map_err(|err| storage_error_from_diesel(err, "Block", &block_id.to_string(), None))?;
        let chain = self.get_chain(&orm_block.chain_id)?;
        Ok(Block::new(
            orm_block.number as u64,
            chain,
            std::mem::take(&mut orm_block.hash),
            std::mem::take(&mut orm_block.parent_hash),
            orm_block.ts,
        ))
    }

    #[instrument(skip_all)]
    pub async fn upsert_tx(
        &self,
        new: &[Transaction],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        use super::schema::transaction::dsl::*;
        if new.is_empty() {
            warn!("Upsert tx called with empty transactions!");
            return Ok(());
        }

        let block_hashes = new
            .iter()
            .map(|n| n.block_hash.clone())
            .collect_vec();
        let parent_blocks = async {
            schema::block::table
                .filter(schema::block::hash.eq_any(&block_hashes))
                .select((schema::block::hash, schema::block::id))
                .get_results::<(Bytes, i64)>(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "Transaction",
                        &format!(
                            "Batch: {:x} and {} more",
                            &block_hashes[0],
                            block_hashes.len() - 1
                        ),
                        Some("Block".to_owned()),
                    )
                })
        }
        .instrument(debug_span!("resolve_block_ids", count = block_hashes.len()))
        .await?
        .into_iter()
        .collect::<HashMap<_, _>>();

        let orm_txns = new
            .iter()
            .map(|new| {
                let block_h = &new.block_hash;
                let bid = *parent_blocks
                    .get(block_h)
                    .ok_or_else(|| {
                        StorageError::NoRelatedEntity(
                            "Block".to_string(),
                            "Transaction".to_string(),
                            block_h.to_string(),
                        )
                    })?;
                Ok(orm::NewTransaction {
                    hash: new.hash.clone(),
                    block_id: bid,
                    from: new.from.clone(),
                    to: new.to.clone().unwrap_or_default(),
                    index: new.index as i64,
                })
            })
            .collect::<Result<Vec<orm::NewTransaction>, StorageError>>()?;

        // Insert in batches to avoid exceeding PostgreSQL parameter limit
        // assumes that tx with the same hash will not appear with different values
        let total_txns = orm_txns.len();
        async {
            for chunk in orm_txns.chunks(orm::NewTransaction::MAX_BATCH_SIZE) {
                diesel::insert_into(transaction)
                    .values(chunk)
                    .on_conflict_do_nothing()
                    .execute(conn)
                    .await
                    .map_err(|err| {
                        storage_error_from_diesel(
                            err,
                            "Transaction",
                            &format!("Batch {:x} and {} more", &chunk[0].hash, chunk.len() - 1),
                            None,
                        )
                    })?;
            }
            Ok::<(), StorageError>(())
        }
        .instrument(debug_span!("insert_transactions", count = total_txns))
        .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn get_tx(
        &self,
        hash: &TxHash,
        conn: &mut AsyncPgConnection,
    ) -> Result<Transaction, StorageError> {
        schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::hash.eq(&hash))
            .select((orm::Transaction::as_select(), schema::block::hash))
            .first::<(orm::Transaction, BlockHash)>(conn)
            .await
            .map(|(mut orm_tx, block_hash)| {
                Ok(Transaction {
                    hash: std::mem::take(&mut orm_tx.hash),
                    block_hash,
                    from: std::mem::take(&mut orm_tx.from),
                    to: Some(std::mem::take(&mut orm_tx.to)),
                    index: orm_tx.index as u64,
                })
            })
            .map_err(|err| {
                storage_error_from_diesel(err, "Transaction", &hex::encode(hash), None)
            })?
    }

    pub async fn revert_state(
        &self,
        to: &BlockIdentifier,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        // To revert all changes of a chain, we need to delete & modify entries
        // from a big number of tables. Reverting state, signifies deleting
        // history. We will not keep any branches in the db only the main branch
        // will be kept.
        let block = orm::Block::by_id(to, conn)
            .await
            .map_err(PostgresError::from)?;

        // All entities and version updates are connected to the block via a
        // cascade delete, this ensures that the state is reverted by simply
        // deleting the correct blocks, which then triggers cascading deletes on
        // child entries. All blocks after the `to` block are deleted - the `to`
        // block and its connected data persists.
        diesel::delete(
            schema::block::table
                .filter(schema::block::number.gt(block.number))
                .filter(schema::block::chain_id.eq(block.chain_id)),
        )
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        // Any versioned table's rows, which have `valid_to` set to "> block.ts"
        // need, to be updated to be valid again (thus, valid_to = NULL).
        diesel::update(
            schema::contract_storage::table.filter(schema::contract_storage::valid_to.gt(block.ts)),
        )
        .set(schema::contract_storage::valid_to.eq(MAX_TS))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        diesel::update(
            schema::account_balance::table.filter(schema::account_balance::valid_to.gt(block.ts)),
        )
        .set(schema::account_balance::valid_to.eq(MAX_TS))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        diesel::update(
            schema::contract_code::table.filter(schema::contract_code::valid_to.gt(block.ts)),
        )
        .set(schema::contract_code::valid_to.eq(MAX_TS))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        diesel::update(
            schema::protocol_state::table.filter(schema::protocol_state::valid_to.gt(block.ts)),
        )
        .set(schema::protocol_state::valid_to.eq(MAX_TS))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        // Any versioned table's rows, which have `deleted_at` set to "> block.ts"
        // need, to be updated to be valid again (thus, deleted_at = NULL).
        diesel::update(schema::account::table.filter(schema::account::deleted_at.gt(block.ts)))
            .set(schema::account::deleted_at.eq(MAX_TS))
            .execute(conn)
            .await
            .map_err(PostgresError::from)?;

        diesel::update(
            schema::protocol_component::table
                .filter(schema::protocol_component::deleted_at.gt(block.ts)),
        )
        .set(schema::protocol_component::deleted_at.eq(MAX_TS))
        .execute(conn)
        .await
        .map_err(PostgresError::from)?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{slice, str::FromStr, time::Duration};

    use diesel_async::AsyncConnection;
    use tycho_common::models::Chain;

    use super::*;
    use crate::postgres::db_fixtures::{self, yesterday_half_past_midnight, yesterday_midnight};

    type EVMGateway = PostgresGateway;

    async fn setup_db() -> AsyncPgConnection {
        let db_url = std::env::var("DATABASE_URL").unwrap();
        let mut conn = AsyncPgConnection::establish(&db_url)
            .await
            .unwrap();
        conn.begin_test_transaction()
            .await
            .unwrap();
        conn
    }

    async fn setup_data(conn: &mut AsyncPgConnection) {
        let chain_id: i64 = db_fixtures::insert_chain(conn, "ethereum").await;
        db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let block_ids = db_fixtures::insert_blocks(conn, chain_id).await;
        db_fixtures::insert_txns(
            conn,
            &[(
                block_ids[0],
                1i64,
                "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
            )],
        )
        .await;
    }

    fn block(hash: &str) -> Block {
        Block::new(
            2,
            Chain::Ethereum,
            Bytes::from(hash),
            Bytes::from("0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6"),
            yesterday_half_past_midnight(),
        )
    }

    #[tokio::test]
    async fn test_get_block_latest() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = block("0xb495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9");
        let block_id = BlockIdentifier::Latest(Chain::Ethereum);

        let block = gw
            .get_block(&block_id, &mut conn)
            .await
            .unwrap();

        assert_eq!(block, exp);
    }

    #[tokio::test]
    async fn test_get_block() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = block("0xb495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9");
        let block_id = BlockIdentifier::Number((Chain::Ethereum, 2));

        let block = gw
            .get_block(&block_id, &mut conn)
            .await
            .unwrap();

        assert_eq!(block, exp);
    }

    #[tokio::test]
    async fn test_add_block() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let block = block("0xbadbabe000000000000000000000000000000000000000000000000000000000");

        gw.upsert_block(slice::from_ref(&block), &mut conn)
            .await
            .unwrap();
        let retrieved_block = gw
            .get_block(&BlockIdentifier::Hash(block.hash.clone()), &mut conn)
            .await
            .unwrap();

        assert_eq!(retrieved_block, block);
    }

    #[tokio::test]
    async fn test_upsert_block() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let block = Block::new(
            1,
            Chain::Ethereum,
            Bytes::from("0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6"),
            Bytes::from("0xd4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"),
            yesterday_midnight(),
        );

        gw.upsert_block(slice::from_ref(&block), &mut conn)
            .await
            .unwrap();
        let retrieved_block = gw
            .get_block(&BlockIdentifier::Hash(block.hash.clone()), &mut conn)
            .await
            .unwrap();

        assert_eq!(retrieved_block, block);
    }

    fn transaction(hash: &str) -> Transaction {
        Transaction {
            hash: Bytes::from(hash),
            block_hash: Bytes::from(
                "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
            ),
            from: Bytes::from("0x4648451b5f87ff8f0f7d622bd40574bb97e25980"),
            to: Some(Bytes::from("0x6b175474e89094c44da98b954eedeac495271d0f")),
            index: 1,
        }
    }

    #[tokio::test]
    async fn test_get_tx() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let exp = transaction("0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945");

        let tx = gw
            .get_tx(&exp.hash.clone(), &mut conn)
            .await
            .unwrap();

        assert_eq!(tx, exp);
    }

    #[tokio::test]
    async fn test_add_tx() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let mut tx =
            transaction("0xbadbabe000000000000000000000000000000000000000000000000000000000");
        tx.block_hash =
            Bytes::from("0xb495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9");

        gw.upsert_tx(&[tx.clone()], &mut conn)
            .await
            .unwrap();
        let retrieved_tx = gw
            .get_tx(&tx.hash.clone(), &mut conn)
            .await
            .unwrap();

        assert_eq!(tx, retrieved_tx);
    }

    #[tokio::test]
    async fn test_upsert_tx() {
        let mut conn = setup_db().await;
        setup_data(&mut conn).await;
        let gw = EVMGateway::from_connection(&mut conn).await;
        let tx = Transaction {
            hash: Bytes::from("0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945"),
            block_hash: Bytes::from(
                "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
            ),
            from: Bytes::from("0x4648451b5F87FF8F0F7D622bD40574bb97E25980"),
            to: Some(Bytes::from("0x6B175474E89094C44Da98b954EedeAC495271d0F")),
            index: 1,
        };

        gw.upsert_tx(slice::from_ref(&tx), &mut conn)
            .await
            .unwrap();
        let retrieved_tx = gw
            .get_tx(&tx.hash.clone(), &mut conn)
            .await
            .unwrap();

        assert_eq!(tx, retrieved_tx);
    }

    async fn setup_revert_data(conn: &mut AsyncPgConnection) {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let blk = db_fixtures::insert_blocks(conn, chain_id).await;
        let ts = chrono::Local::now().naive_utc();
        let txn = db_fixtures::insert_txns(
            conn,
            &[
                (
                    // deploy c0
                    blk[0],
                    1i64,
                    "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945",
                ),
                (
                    // change c0 state, deploy c2
                    blk[0],
                    2i64,
                    "0x794f7df7a3fe973f1583fbb92536f9a8def3a89902439289315326c04068de54",
                ),
                // ----- Block 01 LAST
                (
                    // deploy c1, delete c2
                    blk[1],
                    1i64,
                    "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7",
                ),
                (
                    // change c0 and c1 state
                    blk[1],
                    2i64,
                    "0x50449de1973d86f21bfafa7c72011854a7e33a226709dc3e2e4edcca34188388",
                ),
                // ----- Block 02 LAST
            ],
        )
        .await;
        let (_, native_token) = db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;

        // Account C0
        let c0 = db_fixtures::insert_account(
            conn,
            "6B175474E89094C44Da98b954EedeAC495271d0F",
            "account0",
            chain_id,
            Some(txn[0]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 0, native_token, txn[0], Some(&ts), c0).await;
        db_fixtures::insert_contract_code(conn, c0, txn[0], Bytes::from_str("C0C0C0").unwrap())
            .await;
        db_fixtures::insert_account_balance(
            conn,
            100,
            native_token,
            txn[1],
            Some(&(ts + Duration::from_secs(3600))),
            c0,
        )
        .await;
        // Slot 2 is never modified again
        db_fixtures::insert_slots(conn, c0, txn[1], &ts, None, &[(2, 1, None)]).await;
        // First version for slots 0 and 1.
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[1],
            &ts,
            Some(&(ts + Duration::from_secs(3600))),
            &[(0, 1, None), (1, 5, None)],
        )
        .await;
        db_fixtures::insert_account_balance(conn, 101, native_token, txn[3], None, c0).await;
        // Second and final version for 0 and 1, new slots 5 and 6
        db_fixtures::insert_slots(
            conn,
            c0,
            txn[3],
            &(ts + Duration::from_secs(3600)),
            None,
            &[(0, 2, Some(1)), (1, 3, Some(5)), (5, 25, None), (6, 30, None)],
        )
        .await;

        // Account C1
        let c1 = db_fixtures::insert_account(
            conn,
            "73BcE791c239c8010Cd3C857d96580037CCdd0EE",
            "c1",
            chain_id,
            Some(txn[2]),
        )
        .await;
        db_fixtures::insert_account_balance(conn, 50, native_token, txn[2], None, c1).await;
        db_fixtures::insert_contract_code(conn, c1, txn[2], Bytes::from_str("C1C1C1").unwrap())
            .await;
        db_fixtures::insert_slots(
            conn,
            c1,
            txn[3],
            &(ts + Duration::from_secs(3600)),
            None,
            &[(0, 128, None), (1, 255, None)],
        )
        .await;
    }

    #[tokio::test]
    async fn test_revert() {
        let mut conn = setup_db().await;
        setup_revert_data(&mut conn).await;

        // set up contracts data
        let block1_hash =
            Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                .unwrap();
        let c0_address =
            Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").expect("c0 address valid");
        let exp_slots: HashMap<Bytes, Bytes> = vec![
            (Bytes::from(0_u8).lpad(32, 0), Bytes::from(1_u8).lpad(32, 0)),
            (Bytes::from(1_u8).lpad(32, 0), Bytes::from(5_u8).lpad(32, 0)),
            (Bytes::from(2_u8).lpad(32, 0), Bytes::from(1_u8).lpad(32, 0)),
        ]
        .into_iter()
        .collect();

        let gw = EVMGateway::from_connection(&mut conn).await;

        gw.revert_state(&BlockIdentifier::Hash(block1_hash), &mut conn)
            .await
            .unwrap();

        let slots: HashMap<Bytes, Bytes> = schema::contract_storage::table
            .inner_join(schema::account::table)
            .filter(schema::account::address.eq(c0_address))
            .select((schema::contract_storage::slot, schema::contract_storage::value))
            .get_results::<(Bytes, Option<Bytes>)>(&mut conn)
            .await
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().unwrap_or(Bytes::zero(32))))
            .collect();
        assert_eq!(slots, exp_slots);

        let c1 = schema::account::table
            .filter(schema::account::title.eq("c1"))
            .select(schema::account::id)
            .get_results::<i64>(&mut conn)
            .await
            .unwrap();
        assert_eq!(c1.len(), 0);
    }
}
