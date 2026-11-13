use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use chrono::{Duration, Local, NaiveDateTime};
use deepsize::DeepSizeOf;
use tokio::sync::RwLock;
use tracing::{debug, info, info_span, instrument, Instrument};
use tycho_common::{
    models::{
        protocol::{ProtocolComponent, QualityRange},
        token::Token,
        Address, Chain, ComponentId,
    },
    storage::{ProtocolGateway, StorageError},
    Bytes,
};

#[async_trait]
pub trait ProtocolDataCache: Send + Sync {
    async fn get_token_prices<'a>(
        &'a self,
        addresses: &'a [Bytes],
    ) -> Result<Vec<Option<f64>>, StorageError>;

    async fn get_tokens<'a>(
        &'a self,
        addresses: &'a [Bytes],
    ) -> Result<Vec<Option<Token>>, StorageError>;

    async fn has_token(&self, addresses: &[Address]) -> Vec<bool>;

    async fn add_tokens<T: IntoIterator<Item = Token> + Send + Sync>(
        &self,
        tokens: T,
    ) -> Result<(), StorageError>;

    async fn get_protocol_components<'a>(
        &'a self,
        system: &'a str,
        component_ids: &'a [ComponentId],
    ) -> Result<HashMap<ComponentId, ProtocolComponent>, StorageError>;

    async fn add_components<T: IntoIterator<Item = ProtocolComponent> + Send + Sync>(
        &self,
        components: T,
    ) -> Result<(), StorageError>;
}

type ProtocolComponentStore = HashMap<String, HashMap<ComponentId, ProtocolComponent>>;

#[derive(Clone)]
pub struct ProtocolMemoryCache {
    chain: Chain,
    tokens: Arc<RwLock<HashMap<Bytes, Token>>>,
    token_prices: Arc<RwLock<TokenPrices>>,
    components: Arc<RwLock<ProtocolComponentStore>>,
    max_price_age: chrono::Duration,
    gateway: Arc<dyn ProtocolGateway + Send + Sync>,
    size_metric: bool,
}

#[derive(Default)]
struct TokenPrices {
    prices: HashMap<Bytes, f64>,
    last_price_update: NaiveDateTime,
}

impl ProtocolMemoryCache {
    pub fn new(
        chain: Chain,
        max_price_age: chrono::Duration,
        gateway: Arc<dyn ProtocolGateway + Send + Sync>,
    ) -> Self {
        Self {
            chain,
            max_price_age,
            gateway,
            tokens: Arc::new(RwLock::new(HashMap::new())),
            components: Arc::new(RwLock::new(HashMap::new())),
            token_prices: Arc::new(RwLock::new(TokenPrices::default())),
            size_metric: true,
        }
    }

    /// Sets whether extractors report the size of this cache (on by default). See
    /// [`ProtocolMemoryCache::size_of`].
    pub fn with_size_metric(mut self, enabled: bool) -> Self {
        self.size_metric = enabled;
        self
    }

    pub fn size_metric_enabled(&self) -> bool {
        self.size_metric
    }

    #[instrument(skip_all)]
    pub async fn populate(&self) -> Result<(), StorageError> {
        let mut n_tokens = 0;
        let mut n_components = 0;
        {
            let mut cached_tokens = self.tokens.write().await;
            self.gateway
                .get_tokens(self.chain, None, QualityRange::None(), None, None)
                .await?
                .entity
                .into_iter()
                .for_each(|t| {
                    n_tokens += 1;
                    cached_tokens.insert(t.address.clone(), t);
                });
        }
        {
            let mut cached_components = self.components.write().await;
            self.gateway
                .get_protocol_components(&self.chain, None, None, None, None)
                .await?
                .entity
                .into_iter()
                .for_each(|pc| {
                    n_components += 1;
                    cached_components
                        .entry(pc.protocol_system.clone())
                        .or_default()
                        .insert(pc.id.clone(), pc);
                });
        }
        let n_prices = self.update_prices_cache().await?;
        info!(?n_tokens, ?n_components, ?n_prices, "ProtocolCachePopulated");
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_prices_cache(&self) -> Result<usize, StorageError> {
        let mut token_prices = self.token_prices.write().await;
        token_prices.prices = self
            .gateway
            .get_token_prices(&self.chain)
            .await?;
        let n_fetched = token_prices.prices.len();
        debug!(last_price_update = ?token_prices.last_price_update, ?n_fetched, resource = "prices", "CacheMiss");
        token_prices.last_price_update = Local::now().naive_utc();
        Ok(n_fetched)
    }

    /// Returns the approximate size of the cache in bytes.
    ///
    /// Walks every cached entry under its read lock, one part at a time.
    #[instrument(skip_all)]
    pub async fn size_of(&self) -> usize {
        let tokens = async { self.tokens.read().await.deep_size_of() }
            .instrument(info_span!("size_of_tokens"))
            .await;
        let prices = async {
            self.token_prices
                .read()
                .await
                .prices
                .deep_size_of()
        }
        .instrument(info_span!("size_of_prices"))
        .await;
        let components = async {
            self.components
                .read()
                .await
                .deep_size_of()
        }
        .instrument(info_span!("size_of_components"))
        .await;
        self.chain.deep_size_of() +
            tokens +
            prices +
            size_of::<NaiveDateTime>() +
            components +
            size_of::<Duration>() +
            size_of_val(&self.gateway)
    }
}

#[async_trait]
impl ProtocolDataCache for ProtocolMemoryCache {
    async fn get_token_prices<'a>(
        &'a self,
        addresses: &'a [Bytes],
    ) -> Result<Vec<Option<f64>>, StorageError> {
        let last_update = self
            .token_prices
            .read()
            .await
            .last_price_update;

        let now = Local::now().naive_utc();
        if now.signed_duration_since(last_update) > self.max_price_age {
            self.update_prices_cache().await?;
        }
        let mut res = Vec::with_capacity(addresses.len());
        let inner = self.token_prices.read().await;
        for addr in addresses.iter() {
            res.push(inner.prices.get(addr).copied());
        }
        Ok(res)
    }

    #[instrument(skip_all, fields(n_addresses=addresses.len()))]
    async fn get_tokens<'a>(
        &'a self,
        addresses: &'a [Bytes],
    ) -> Result<Vec<Option<Token>>, StorageError> {
        let missing = {
            let cached_tokens = self.tokens.read().await;
            addresses
                .iter()
                .filter(|&a| !cached_tokens.contains_key(a))
                .collect::<Vec<_>>()
        };
        if !missing.is_empty() {
            let mut cached_tokens = self.tokens.write().await;
            let mut n_fetched = 0;
            self.gateway
                .get_tokens(self.chain, Some(&missing), QualityRange::None(), None, None)
                .await?
                .entity
                .into_iter()
                .for_each(|t| {
                    n_fetched += 1;
                    cached_tokens.insert(t.address.clone(), t);
                });
            debug!(n_missing = missing.len(), n_fetched, resource = "token", "CacheMiss");
        }
        let cached_tokens = self.tokens.read().await;
        Ok(addresses
            .iter()
            .map(|addr| cached_tokens.get(addr).cloned())
            .collect::<Vec<_>>())
    }

    #[instrument(skip_all, fields(n_addresses = addresses.len()))]
    async fn has_token(&self, addresses: &[Address]) -> Vec<bool> {
        let guard = self.tokens.read().await;
        addresses
            .iter()
            .map(|address| guard.contains_key(address))
            .collect()
    }

    #[instrument(skip_all)]
    async fn add_tokens<T: IntoIterator<Item = Token> + Send + Sync>(
        &self,
        tokens: T,
    ) -> Result<(), StorageError> {
        let tokens = tokens
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .collect::<Vec<_>>();
        // Most blocks bring no new token. Taking the write lock anyway would queue this
        // extractor behind any reader, and every later reader behind it.
        if tokens.is_empty() {
            return Ok(());
        }
        self.tokens.write().await.extend(tokens);
        Ok(())
    }

    #[instrument(skip_all, fields(n_component_ids=component_ids.len()))]
    async fn get_protocol_components<'a>(
        &'a self,
        system: &'a str,
        component_ids: &'a [ComponentId],
    ) -> Result<HashMap<ComponentId, ProtocolComponent>, StorageError> {
        let empty = HashMap::new();
        let missing = {
            let guard = self.components.read().await;
            let cached_components = guard.get(system).unwrap_or(&empty);
            component_ids
                .iter()
                .filter(|&id| !cached_components.contains_key(id))
                .collect::<Vec<_>>()
        };
        if !missing.is_empty() {
            let mut guard = self.components.write().await;
            let cached_components = guard
                .entry(system.to_string())
                .or_default();
            let mut n_fetched = 0;
            self.gateway
                .get_protocol_components(
                    &self.chain,
                    Some(system.to_string()),
                    Some(
                        &missing
                            .iter()
                            .map(|id| id.as_str())
                            .collect::<Vec<_>>(),
                    ),
                    None,
                    None,
                )
                .await?
                .entity
                .into_iter()
                .for_each(|c| {
                    n_fetched += 1;
                    cached_components.insert(c.id.clone(), c);
                });
            debug!(n_missing = missing.len(), n_fetched, resource = "component", "CacheMiss");
        }
        let guard = self.components.read().await;
        let cached_components = guard.get(system).unwrap_or(&empty);
        Ok(component_ids
            .iter()
            .filter_map(|id| cached_components.get(id).cloned())
            .map(|c| (c.id.clone(), c))
            .collect())
    }

    #[instrument(skip_all)]
    async fn add_components<T: IntoIterator<Item = ProtocolComponent> + Send + Sync>(
        &self,
        components: T,
    ) -> Result<(), StorageError> {
        let components = components
            .into_iter()
            .collect::<Vec<_>>();
        // See `add_tokens`: skip the write lock when there is nothing to add.
        if components.is_empty() {
            return Ok(());
        }
        let mut guard = self.components.write().await;
        components.into_iter().for_each(|pc| {
            let components = guard
                .entry(pc.protocol_system.clone())
                .or_default();
            components.insert(pc.id.clone(), pc);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use mockall::predicate::*;
    use tycho_common::{models::ChangeType, storage::WithTotal};

    use super::*;
    use crate::testing::MockGateway;

    #[tokio::test]
    async fn test_get_token_prices() {
        let chain = Chain::Ethereum;
        let max_price_age = Duration::seconds(60);
        let mut gateway = MockGateway::new();

        let token_prices = prices();
        gateway
            .expect_get_token_prices()
            .with(eq(chain))
            .times(1)
            .return_once(move |_| Box::pin(async move { Ok(token_prices.clone()) }));

        let cache = ProtocolMemoryCache::new(chain, max_price_age, Arc::new(gateway));

        let addresses = &[Bytes::from("0x01"), Bytes::from("0x02")];
        let prices = cache
            .get_token_prices(addresses)
            .await
            .unwrap();

        assert_eq!(prices, vec![Some(1.0), Some(2.0)]);
    }

    fn prices() -> HashMap<Bytes, f64> {
        HashMap::from([(Bytes::from("0x01"), 1.0), (Bytes::from("0x02"), 2.0)])
    }

    #[tokio::test]
    async fn test_get_tokens() {
        let chain = Chain::Ethereum;
        let max_price_age = Duration::seconds(60);
        let mut gateway = MockGateway::new();

        let tokens = tokens();
        let ret_tokens = tokens.clone();
        gateway
            .expect_get_tokens()
            .return_once(|_, _, _, _, _| {
                Box::pin(async move { Ok(WithTotal { entity: ret_tokens, total: Some(2) }) })
            });
        let cache = ProtocolMemoryCache::new(chain, max_price_age, Arc::new(gateway));

        let addresses = tokens
            .iter()
            .map(|t| t.address.clone())
            .collect::<Vec<_>>();
        let cached_tokens = cache
            .get_tokens(&addresses)
            .await
            .unwrap();

        assert_eq!(cached_tokens.len(), 2);
        assert_eq!(
            cached_tokens[0]
                .as_ref()
                .unwrap()
                .address,
            Bytes::from("0x01")
        );
        assert_eq!(
            cached_tokens[1]
                .as_ref()
                .unwrap()
                .address,
            Bytes::from("0x02")
        );
    }

    fn tokens() -> Vec<Token> {
        vec![
            Token::new(&Bytes::from("0x01"), "T1", 18, 0, &[None], Chain::Ethereum, 100),
            Token::new(&Bytes::from("0x02"), "T2", 18, 0, &[None], Chain::Ethereum, 100),
        ]
    }

    fn components() -> Vec<ProtocolComponent> {
        vec![
            ProtocolComponent::new(
                "component1",
                "sys1",
                "pool1",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::default(),
                NaiveDateTime::default(),
            ),
            ProtocolComponent::new(
                "component2",
                "sys1",
                "pool1",
                Chain::Ethereum,
                Vec::new(),
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::default(),
                NaiveDateTime::default(),
            ),
        ]
    }

    #[tokio::test]
    async fn test_get_protocol_components() {
        let chain = Chain::Ethereum;
        let max_price_age = Duration::seconds(60);
        let mut gateway = MockGateway::new();

        let components = components();
        let ret_components = components.clone();
        gateway
            .expect_get_protocol_components()
            .return_once(move |_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: ret_components, total: Some(10) }) })
            });

        let cache = ProtocolMemoryCache::new(chain, max_price_age, Arc::new(gateway));
        let component_ids = components
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>();
        let cached_components = cache
            .get_protocol_components("sys1", &component_ids)
            .await
            .unwrap();

        assert_eq!(cached_components.len(), 2);
        assert!(cached_components.contains_key("component1"));
        assert!(cached_components.contains_key("component2"));
    }

    #[tokio::test]
    async fn test_populate() {
        let chain = Chain::Ethereum;
        let max_price_age = Duration::seconds(60);
        let mut gateway = MockGateway::new();
        gateway
            .expect_get_tokens()
            .return_once(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: tokens(), total: Some(2) }) })
            });
        gateway
            .expect_get_protocol_components()
            .return_once(|_, _, _, _, _| {
                Box::pin(async { Ok(WithTotal { entity: components(), total: Some(10) }) })
            });
        gateway
            .expect_get_token_prices()
            .with(eq(chain))
            .times(1)
            .return_once(|_| Box::pin(async { Ok(prices()) }));
        let exp_tokens = tokens()
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .collect();
        let exp_prices = prices();
        let mut exp_components: ProtocolComponentStore = HashMap::new();
        components().into_iter().for_each(|pc| {
            exp_components
                .entry(pc.protocol_system.clone())
                .or_default()
                .insert(pc.id.clone(), pc);
        });
        let cache = ProtocolMemoryCache::new(chain, max_price_age, Arc::new(gateway));

        cache.populate().await.unwrap();

        let cached_tokens = cache.tokens.read().await.clone();
        assert_eq!(cached_tokens, exp_tokens);
        let cached_prices = cache
            .token_prices
            .read()
            .await
            .prices
            .clone();
        assert_eq!(cached_prices, exp_prices);
        let cached_components = cache.components.read().await.clone();
        assert_eq!(cached_components, exp_components);
    }
}
