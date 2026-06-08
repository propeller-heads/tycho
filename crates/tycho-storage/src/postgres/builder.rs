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
}

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

    pub async fn build(self) -> Result<(CachedGateway, JoinHandle<()>), StorageError> {
        let pool = postgres::connect(&self.database_url).await?;
        postgres::ensure_chains(&self.chains, pool.clone()).await;
        postgres::ensure_protocol_systems(&self.protocol_systems, pool.clone()).await;

        let inner_gw = PostgresGateway::new(pool.clone(), self.retention_horizon).await?;
        let (tx, rx) = mpsc::channel(10);
        let chain = self
            .chains
            .first()
            .expect("No chains provided"); //TODO: handle multichain?
        let write_executor = postgres::cache::DBCacheWriteExecutor::new(
            chain.to_string(),
            *chain,
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

        let inner_gw = PostgresGateway::new(pool.clone(), self.retention_horizon).await?;
        let (tx, _) = mpsc::channel(10);

        let cached_gw = CachedGateway::new(tx, pool.clone(), inner_gw.clone());
        Ok(cached_gw)
    }

    pub async fn build_direct_gw(self) -> Result<DirectGateway, StorageError> {
        let pool = postgres::connect(&self.database_url).await?;
        postgres::ensure_chains(&self.chains, pool.clone()).await;
        postgres::ensure_protocol_systems(&self.protocol_systems, pool.clone()).await;

        let inner_gw = PostgresGateway::new(pool.clone(), self.retention_horizon).await?;

        let chain = self
            .chains
            .first()
            .expect("No chains provided"); //TODO: handle multichain?

        let direct_gw = DirectGateway::new(pool.clone(), inner_gw.clone(), *chain);
        Ok(direct_gw)
    }
}
