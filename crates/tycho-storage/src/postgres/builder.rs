use chrono::NaiveDateTime;
use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use tokio::{sync::mpsc, task::JoinHandle};
use tycho_common::{models::Chain, storage::StorageError};

use crate::{
    postgres,
    postgres::{cache::CachedGateway, direct::DirectGateway, PostgresGateway},
};

#[derive(Default)]
pub struct GatewayBuilder {
    database_url: String,
    protocol_systems: Vec<String>,
    retention_horizon: NaiveDateTime,
    chains: Vec<Chain>,
    token_cache: bool,
    pool_size: Option<usize>,
    rpc_pool_size: Option<usize>,
}

/// The gateways of an indexer that extracts and serves requests in one process.
pub struct IndexerGateways {
    /// For extractors. Reads and the database writer share its pool and nothing else does.
    pub extraction: CachedGateway,
    /// For request handling and background reads. Has its own pool.
    pub rpc: CachedGateway,
    /// The database writer task.
    pub writer: JoinHandle<()>,
}

/// How often the token cache polls for token rows modified by other processes.
const TOKEN_CACHE_REFRESH_PERIOD: std::time::Duration = std::time::Duration::from_secs(60);

impl GatewayBuilder {
    pub fn new(database_url: &str) -> Self {
        Self { database_url: database_url.to_string(), ..Default::default() }
    }

    pub fn set_chains(mut self, chains: &[Chain]) -> Self {
        self.chains = chains.to_vec();
        self
    }

    pub fn set_protocol_systems(mut self, protocol_systems: &[String]) -> Self {
        self.protocol_systems = protocol_systems.to_vec();
        self
    }

    pub fn set_retention_horizon(mut self, horizon: NaiveDateTime) -> Self {
        self.retention_horizon = horizon;
        self
    }

    /// Caps the connections of the pool that `build`, `build_gw` and `build_direct_gw` create,
    /// and of the extraction pool of `build_with_rpc_gateway`. Without it, the pool opens up to
    /// twice the number of CPUs.
    pub fn set_pool_size(mut self, size: usize) -> Self {
        self.pool_size = Some(size);
        self
    }

    /// Caps the connections of the request pool of `build_with_rpc_gateway`. Without it, the
    /// pool opens up to twice the number of CPUs.
    pub fn set_rpc_pool_size(mut self, size: usize) -> Self {
        self.rpc_pool_size = Some(size);
        self
    }

    /// Serves `get_tokens` from an in-memory copy of the token tables instead of SQL.
    /// Costs a full token load at startup plus a periodic refresh query; intended for
    /// the long-running `index` and `rpc` services.
    pub fn enable_token_cache(mut self) -> Self {
        self.token_cache = true;
        self
    }

    // TODO: remove once all interfaces are refactored to be single-chain targeted.
    fn single_chain(&self) -> Result<Chain, StorageError> {
        match self.chains.as_slice() {
            [chain] => Ok(*chain),
            [] => Err(StorageError::Unexpected("No chain provided".to_string())),
            _ => Err(StorageError::Unexpected(format!(
                "Expected exactly one chain, got {}: {:?}",
                self.chains.len(),
                self.chains
            ))),
        }
    }

    pub async fn build(self) -> Result<(CachedGateway, JoinHandle<()>), StorageError> {
        let pool = postgres::connect(&self.database_url, self.pool_size).await?;
        self.build_cached(pool.clone(), pool)
            .await
    }

    /// Builds an extraction gateway and a request gateway with separate connection pools.
    ///
    /// Extractors read through the extraction pool and the database writer commits through it.
    /// Request handling and the token cache refresh use the request pool. A burst of requests
    /// then waits only for request connections and never delays block processing.
    pub async fn build_with_rpc_gateway(self) -> Result<IndexerGateways, StorageError> {
        let extraction_pool = postgres::connect(&self.database_url, self.pool_size).await?;
        let rpc_pool = postgres::new_pool(&self.database_url, self.rpc_pool_size)?;
        let (extraction, writer) = self
            .build_cached(extraction_pool, rpc_pool.clone())
            .await?;
        let rpc = extraction.with_pool(rpc_pool);
        Ok(IndexerGateways { extraction, rpc, writer })
    }

    /// Builds a gateway and its database writer on `pool`. The token cache refresh, if
    /// enabled, runs on `refresh_pool`.
    async fn build_cached(
        self,
        pool: Pool<AsyncPgConnection>,
        refresh_pool: Pool<AsyncPgConnection>,
    ) -> Result<(CachedGateway, JoinHandle<()>), StorageError> {
        let chain = self.single_chain()?;
        let mut conn = pool
            .get()
            .await
            .map_err(|e| StorageError::Unexpected(e.to_string()))?;
        postgres::ensure_chain(chain, &mut conn).await?;
        postgres::ensure_protocol_systems(&self.protocol_systems, &mut conn).await;
        drop(conn);

        let inner_gw = PostgresGateway::new(
            pool.clone(),
            self.retention_horizon,
            self.token_cache
                .then_some(self.chains.as_slice()),
        )
        .await?;
        if let Some(token_cache) = &inner_gw.token_cache {
            token_cache.spawn_refresh_task(refresh_pool, TOKEN_CACHE_REFRESH_PERIOD);
        }
        let (tx, rx) = mpsc::channel(10);
        let write_executor = postgres::cache::DBCacheWriteExecutor::new(
            chain.to_string(),
            chain,
            pool.clone(),
            inner_gw.clone(),
            rx,
        )
        .await;
        let handle = write_executor.run();

        let cached_gw = CachedGateway::new(tx, pool, inner_gw);
        Ok((cached_gw, handle))
    }

    pub async fn build_gw(self) -> Result<CachedGateway, StorageError> {
        let pool = postgres::connect(&self.database_url, self.pool_size).await?;

        let inner_gw = PostgresGateway::new(
            pool.clone(),
            self.retention_horizon,
            self.token_cache
                .then_some(self.chains.as_slice()),
        )
        .await?;
        if let Some(token_cache) = &inner_gw.token_cache {
            token_cache.spawn_refresh_task(pool.clone(), TOKEN_CACHE_REFRESH_PERIOD);
        }
        let (tx, _) = mpsc::channel(10);

        let cached_gw = CachedGateway::new(tx, pool.clone(), inner_gw.clone());
        Ok(cached_gw)
    }

    pub async fn build_direct_gw(self) -> Result<DirectGateway, StorageError> {
        let chain = self.single_chain()?;
        let pool = postgres::connect(&self.database_url, self.pool_size).await?;
        let mut conn = pool
            .get()
            .await
            .map_err(|e| StorageError::Unexpected(e.to_string()))?;
        postgres::ensure_chain(chain, &mut conn).await?;
        postgres::ensure_protocol_systems(&self.protocol_systems, &mut conn).await;
        drop(conn);

        let inner_gw = PostgresGateway::new(
            pool.clone(),
            self.retention_horizon,
            self.token_cache
                .then_some(self.chains.as_slice()),
        )
        .await?;
        if let Some(token_cache) = &inner_gw.token_cache {
            token_cache.spawn_refresh_task(pool.clone(), TOKEN_CACHE_REFRESH_PERIOD);
        }

        let direct_gw = DirectGateway::new(pool.clone(), inner_gw.clone(), chain);
        Ok(direct_gw)
    }
}

#[cfg(test)]
mod test_serial_db {
    use std::time::Duration;

    use super::*;
    use crate::postgres::testing::run_against_db;

    fn database_url() -> String {
        std::env::var("DATABASE_URL").expect("Database URL must be set for testing")
    }

    /// Takes a connection from `pool`, failing instead of waiting when its users hold them all.
    async fn connect(pool: &Pool<AsyncPgConnection>) -> impl Drop {
        tokio::time::timeout(Duration::from_secs(5), pool.get())
            .await
            .expect("waited for a connection held by the other pool's users")
            .expect("pool should connect")
    }

    #[tokio::test]
    async fn build_with_rpc_gateway_keeps_request_connections_apart_serial_db() {
        run_against_db(|_| async move {
            let gateways = GatewayBuilder::new(&database_url())
                .set_chains(&[Chain::Ethereum])
                .set_pool_size(2)
                .set_rpc_pool_size(3)
                .build_with_rpc_gateway()
                .await
                .expect("gateways should build");
            let extraction = gateways.extraction.pool();
            let rpc = gateways.rpc.pool();
            assert_eq!(extraction.status().max_size, 2);
            assert_eq!(rpc.status().max_size, 3);

            // Requests hold every request connection; extraction still connects at once.
            let mut held = Vec::new();
            for _ in 0..3 {
                held.push(connect(rpc).await);
            }
            let extraction_conn = connect(extraction).await;

            assert_eq!(rpc.status().available, 0);
            drop((extraction_conn, held));
        })
        .await;
    }

    #[tokio::test]
    async fn build_uses_one_pool_of_the_set_size_serial_db() {
        run_against_db(|_| async move {
            let (gateway, _writer) = GatewayBuilder::new(&database_url())
                .set_chains(&[Chain::Ethereum])
                .set_pool_size(3)
                .build()
                .await
                .expect("gateway should build");

            assert_eq!(gateway.pool().status().max_size, 3);
        })
        .await;
    }
}
