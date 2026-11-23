//! Tokens: shared by all instances and found by address.
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::token::Token, storage::StorageError};

use crate::postgres::{
    instance::{gateway::PostgresGatewayNew, write::ROWS_PER_INSERT},
    orm::NewToken,
    schema::token,
    storage_error_from_diesel,
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
        for chunk in rows.chunks(ROWS_PER_INSERT) {
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
