use std::sync::Arc;

use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use tycho_common::{models::Chain, storage::StorageError};

use crate::postgres::{ChainEnumCache, FromConnection, ProtocolSystemEnumCache};

/// Inner gateway on the `tycho` instance tables. It runs the SQL for the public gateways and reads
/// each protocol's active instance.
///
/// A working name while both inner gateways exist: the cut-over renames it to `PostgresGateway`
/// and deletes the gateway on the old tables.
pub struct PostgresGatewayNew {
    pub(super) chain_id_cache: Arc<ChainEnumCache>,
    pub(super) protocol_system_id_cache: Arc<ProtocolSystemEnumCache>,
}

impl PostgresGatewayNew {
    /// Loads the chain and protocol system names from the database.
    ///
    /// A name added to the database later is unknown to this gateway.
    pub async fn new(pool: Pool<AsyncPgConnection>) -> Result<Self, StorageError> {
        let chain_id_cache = ChainEnumCache::from_pool(pool.clone()).await?;
        let protocol_system_id_cache = ProtocolSystemEnumCache::from_pool(pool).await?;
        Ok(Self {
            chain_id_cache: Arc::new(chain_id_cache),
            protocol_system_id_cache: Arc::new(protocol_system_id_cache),
        })
    }

    /// The database id of `chain`, or `NotFound` for a chain the database does not know.
    pub(super) fn get_chain_id(&self, chain: &Chain) -> Result<i64, StorageError> {
        self.chain_id_cache.try_get_id(chain)
    }

    /// The database id of the protocol system `name`, or `NotFound` for an unknown name.
    pub(super) fn get_protocol_system_id(&self, name: &str) -> Result<i64, StorageError> {
        self.protocol_system_id_cache
            .try_get_id(&name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::{db_fixtures, testing::run_against_db};

    #[tokio::test]
    async fn test_serial_db_new_gateway_resolves_known_names() {
        run_against_db(|pool| async move {
            {
                let mut conn = pool
                    .get()
                    .await
                    .expect("getting a connection failed");
                db_fixtures::insert_chain(&mut conn, "ethereum").await;
                db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
            }

            let gateway = PostgresGatewayNew::new(pool)
                .await
                .expect("creating the gateway failed");

            assert!(gateway
                .get_chain_id(&Chain::Ethereum)
                .is_ok());
            assert!(matches!(gateway.get_chain_id(&Chain::Base), Err(StorageError::NotFound(..))));
            assert!(gateway
                .get_protocol_system_id("vm:curve")
                .is_ok());
            assert!(matches!(
                gateway.get_protocol_system_id("unknown"),
                Err(StorageError::NotFound(..))
            ));
        })
        .await;
    }
}
