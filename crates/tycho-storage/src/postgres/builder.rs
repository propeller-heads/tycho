use chrono::NaiveDateTime;
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
        let chain = self.single_chain()?;
        let pool = postgres::connect(&self.database_url).await?;
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

        let cached_gw = CachedGateway::new(tx, pool.clone(), inner_gw.clone());
        Ok((cached_gw, handle))
    }

    pub async fn build_gw(self) -> Result<CachedGateway, StorageError> {
        let pool = postgres::connect(&self.database_url).await?;

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
        let pool = postgres::connect(&self.database_url).await?;
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
