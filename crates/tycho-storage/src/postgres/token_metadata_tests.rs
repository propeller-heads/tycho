//! Focused SQL integration test. Uses its own schema inside the `DATABASE_URL` test database;
//! no production fixtures or optional PostgreSQL extensions are needed.

use diesel_async::{scoped_futures::ScopedFutureExt, AsyncConnection, SimpleAsyncConnection};
use tycho_common::{models::token::Token, Bytes};

use super::*;

// Runs in the `serial_db` group like the other PostgreSQL tests, but inside its own schema so
// it neither depends on nor disturbs the migrated fixtures those tests use.
#[tokio::test]
async fn test_metadata_recovery_is_durable_atomic_and_does_not_downgrade_ready_rows_serial_db() {
    let url = std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
    let mut conn = AsyncPgConnection::establish(&url)
        .await
        .unwrap();
    let schema_name = format!("metadata_recovery_{}", std::process::id());
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema_name} CASCADE;
        CREATE SCHEMA {schema_name}; SET search_path TO {schema_name};
        CREATE TABLE account (
            id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, address BYTEA NOT NULL,
            chain_id BIGINT NOT NULL, creation_tx BIGINT, created_at TIMESTAMP, deleted_at TIMESTAMP,
            deletion_tx BIGINT, inserted_ts TIMESTAMP NOT NULL DEFAULT now(),
            modified_ts TIMESTAMP NOT NULL DEFAULT now(), UNIQUE(address, chain_id));
        CREATE TABLE token (
            id BIGSERIAL PRIMARY KEY, account_id BIGINT NOT NULL UNIQUE REFERENCES account(id),
            symbol TEXT NOT NULL, decimals INTEGER NOT NULL, tax BIGINT NOT NULL,
            gas BIGINT[] NOT NULL, quality INTEGER NOT NULL,
            inserted_ts TIMESTAMP NOT NULL DEFAULT now(), modified_ts TIMESTAMP NOT NULL DEFAULT now());
        INSERT INTO account (title, address, chain_id) VALUES ('legacy', decode('00', 'hex'), 1);
        INSERT INTO token (account_id, symbol, decimals, tax, gas, quality) VALUES (1, 'LEGACY', 18, 0, '{{}}', 100);
    ")).await.unwrap();
    conn.batch_execute(include_str!("../../migrations/2026-09-07_token_metadata_readiness/up.sql"))
        .await
        .unwrap();
    let gateway = PostgresGateway::with_cache(
        Arc::new(ChainEnumCache::from_tuples(vec![(1, "ethereum".into()), (2, "polygon".into())])),
        Arc::new(NativeTokenEnumCache::from_tuples(vec![])),
        Arc::new(ProtocolSystemEnumCache::from_tuples(vec![])),
        None,
        chrono::DateTime::UNIX_EPOCH.naive_utc(),
    );
    let legacy = gateway
        .ready_token_metadata(Chain::Ethereum, &[Bytes::from("0x00")], &mut conn)
        .await
        .unwrap();
    assert_eq!(legacy[0].symbol, "LEGACY");
    let pending: Vec<_> = (1u8..=3)
        .map(|id| Token::pending(&Bytes::from(vec![id]), Chain::Ethereum))
        .collect();
    gateway
        .add_tokens(&pending, &mut conn)
        .await
        .unwrap();
    assert!(gateway
        .update_tokens(&pending, &mut conn)
        .await
        .is_err());
    let premature = Token::new(&pending[1].address, "PREMATURE", 18, 0, &[], Chain::Ethereum, 100);
    gateway
        .update_tokens(&[premature], &mut conn)
        .await
        .unwrap();
    let stats = gateway
        .pending_token_metadata_stats(Chain::Ethereum, &mut conn)
        .await
        .unwrap();
    assert_eq!(stats.0, 3);
    assert!(stats.1.is_some());
    gateway
        .add_tokens(&[Token::pending(&pending[0].address, Chain::Polygon)], &mut conn)
        .await
        .unwrap();
    let first_page = gateway
        .pending_token_metadata(Chain::Ethereum, 0, 1, &mut conn)
        .await
        .unwrap();
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].1.address, pending[0].address);
    let ready =
        Token::new(&pending[0].address, "REPAIRED", 6, 25, &[Some(30_000)], Chain::Ethereum, 50);
    let tokens = vec![ready.clone()];
    let rollback = conn
        .transaction::<(), diesel::result::Error, _>(|conn| {
            async {
                gateway
                    .complete_token_metadata(&tokens, conn)
                    .await
                    .unwrap();
                Err(diesel::result::Error::RollbackTransaction)
            }
            .scope_boxed()
        })
        .await;
    assert!(rollback.is_err());
    assert!(gateway
        .ready_token_metadata(Chain::Ethereum, std::slice::from_ref(&ready.address), &mut conn)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        gateway
            .pending_token_metadata(Chain::Ethereum, 0, 32, &mut conn)
            .await
            .unwrap()
            .len(),
        3
    );
    // The id cursor resumes strictly after the last row of the previous page.
    let next_page = gateway
        .pending_token_metadata(Chain::Ethereum, first_page[0].0, 32, &mut conn)
        .await
        .unwrap();
    assert_eq!(
        next_page
            .iter()
            .map(|row| row.1.address.clone())
            .collect::<Vec<_>>(),
        vec![pending[1].address.clone(), pending[2].address.clone()]
    );

    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        async {
            Ok(gateway
                .complete_token_metadata(&tokens, conn)
                .await
                .unwrap())
        }
        .scope_boxed()
    })
    .await
    .unwrap();
    // Both stale creation writes and a competing recovery result must preserve the committed row.
    gateway
        .add_tokens(&pending, &mut conn)
        .await
        .unwrap();
    let competing = Token::new(&ready.address, "STALE", 18, 0, &[], Chain::Ethereum, 100);
    assert!(gateway
        .complete_token_metadata(&[competing], &mut conn)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        gateway
            .pending_token_metadata(Chain::Polygon, 0, 32, &mut conn)
            .await
            .unwrap()
            .len(),
        1
    );
    drop(conn);

    // A new connection sees the repair and the unfinished queue, without either in-memory cache.
    let mut conn = AsyncPgConnection::establish(&url)
        .await
        .unwrap();
    conn.batch_execute(&format!("SET search_path TO {schema_name}"))
        .await
        .unwrap();
    let restored = gateway
        .ready_token_metadata(Chain::Ethereum, &[ready.address], &mut conn)
        .await
        .unwrap();
    assert_eq!(
        (restored[0].symbol.as_str(), restored[0].decimals, restored[0].tax, restored[0].quality),
        ("REPAIRED", 6, 25, 50)
    );
    assert_eq!(restored[0].gas, vec![Some(30_000)]);
    assert_eq!(
        gateway
            .pending_token_metadata(Chain::Ethereum, 0, 32, &mut conn)
            .await
            .unwrap()
            .len(),
        2
    );
    conn.batch_execute(include_str!(
        "../../migrations/2026-09-07_token_metadata_readiness/down.sql"
    ))
    .await
    .unwrap();
    conn.batch_execute(&format!("DROP SCHEMA {schema_name} CASCADE"))
        .await
        .unwrap();
}
