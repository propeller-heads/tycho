//! Tokens: shared by all instances and found by address.
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::token::Token, storage::StorageError};

use crate::postgres::{
    instance::gateway::PostgresGatewayNew, orm::NewToken, schema::token, storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Stores the tokens that are not stored yet. A stored token with the same address keeps its
    /// row: the first writer of a token wins.
    pub(crate) async fn insert_tokens(
        &self,
        tokens: &[Token],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let rows = tokens
            .iter()
            .map(NewToken::from_token)
            .collect::<Result<Vec<_>, _>>()?;
        for chunk in rows.chunks(NewToken::MAX_BATCH_SIZE) {
            diesel::insert_into(token::table)
                .values(chunk)
                .on_conflict(token::address)
                .do_nothing()
                .execute(conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Token", "batch", None))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use crate::postgres::{
        instance::gateway::PostgresGatewayNew, orm, schema::token,
        testing::instance_test_connection,
    };

    async fn setup() -> (AsyncPgConnection, PostgresGatewayNew) {
        let mut conn = instance_test_connection().await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        (conn, gateway)
    }

    fn address() -> Bytes {
        Bytes::from("0x00000000000000000000000000000000000000a1")
    }

    fn new_token(symbol: &str) -> Token {
        Token::new(&address(), symbol, 18, 10, &[Some(30_000)], Chain::Ethereum, 100)
    }

    async fn stored(conn: &mut AsyncPgConnection) -> Vec<orm::Token> {
        token::table
            .filter(token::address.eq(address()))
            .select(orm::Token::as_select())
            .load(conn)
            .await
            .expect("reading the tokens failed")
    }

    #[tokio::test]
    async fn test_insert_stores_token_by_address() {
        let (mut conn, gateway) = setup().await;

        gateway
            .insert_tokens(&[new_token("A")], &mut conn)
            .await
            .unwrap();

        let stored = stored(&mut conn).await;
        assert_eq!(stored.len(), 1);
        let row = &stored[0];
        assert_eq!(
            (&row.address, row.symbol.as_str(), row.decimals, row.tax, &row.gas, row.quality),
            (&address(), "A", 18, 10, &vec![Some(30_000)], 100)
        );
    }

    #[tokio::test]
    async fn test_insert_keeps_the_stored_token() {
        let (mut conn, gateway) = setup().await;
        gateway
            .insert_tokens(&[new_token("OLD")], &mut conn)
            .await
            .unwrap();

        gateway
            .insert_tokens(&[new_token("NEW")], &mut conn)
            .await
            .unwrap();

        let stored = stored(&mut conn).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].symbol, "OLD");
    }

    #[tokio::test]
    async fn test_insert_takes_one_address_twice() {
        let (mut conn, gateway) = setup().await;

        gateway
            .insert_tokens(&[new_token("A"), new_token("B")], &mut conn)
            .await
            .unwrap();

        let stored = stored(&mut conn).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].symbol, "A");
    }

    #[tokio::test]
    async fn test_insert_cuts_a_long_symbol_at_a_char_boundary() {
        let (mut conn, gateway) = setup().await;
        let symbol = format!("{}éb", "a".repeat(254));

        gateway
            .insert_tokens(&[new_token(&symbol)], &mut conn)
            .await
            .unwrap();

        assert_eq!(stored(&mut conn).await[0].symbol, "a".repeat(254));
    }
}
