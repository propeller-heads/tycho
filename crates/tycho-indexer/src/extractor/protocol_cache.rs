use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use chrono::{Duration, Local, NaiveDateTime};
use deepsize::DeepSizeOf;
use tokio::sync::RwLock;
use tracing::{debug, info, instrument};
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
}

#[derive(Default)]
struct TokenPrices {
    prices: HashMap<Bytes, f64>,
    last_price_update: NaiveDateTime,
}

/// A `Pending` identity must never replace a `Ready` entry. Blocks buffered before a recovery
/// write committed still carry the pending placeholder, and a database read started before that
/// commit can return it too; in both cases the cached entry is the newer truth.
fn keeps_ready_entry(existing: Option<&Token>, incoming: &Token) -> bool {
    !incoming.metadata_status.is_ready() &&
        existing.is_some_and(|old| old.metadata_status.is_ready())
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
        }
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
    pub async fn size_of(&self) -> usize {
        self.chain.deep_size_of() +
            self.tokens.read().await.deep_size_of() +
            self.token_prices
                .read()
                .await
                .prices
                .deep_size_of() +
            size_of::<NaiveDateTime>() +
            self.components
                .read()
                .await
                .deep_size_of() +
            size_of::<Duration>() +
            size_of_val(&self.gateway)
    }

    /// Best effort: the cache can hold pending identities from blocks that were reverted and
    /// never re-included, so some of these addresses may have no database row at all.
    pub async fn pending_token_addresses(&self) -> Vec<Bytes> {
        self.tokens
            .read()
            .await
            .values()
            .filter(|token| !token.metadata_status.is_ready())
            .map(|token| token.address.clone())
            .collect()
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
                    if keeps_ready_entry(cached_tokens.get(&t.address), &t) {
                        return;
                    }
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

    async fn has_token(&self, addresses: &[Address]) -> Vec<bool> {
        let guard = self.tokens.read().await;
        addresses
            .iter()
            .map(|address| guard.contains_key(address))
            .collect()
    }

    async fn add_tokens<T: IntoIterator<Item = Token> + Send + Sync>(
        &self,
        tokens: T,
    ) -> Result<(), StorageError> {
        let mut guard = self.tokens.write().await;
        for token in tokens {
            if keeps_ready_entry(guard.get(&token.address), &token) {
                continue;
            }
            guard.insert(token.address.clone(), token);
        }
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

    async fn add_components<T: IntoIterator<Item = ProtocolComponent> + Send + Sync>(
        &self,
        components: T,
    ) -> Result<(), StorageError> {
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
    async fn recovered_metadata_survives_a_stale_pending_block() {
        let cache = ProtocolMemoryCache::new(
            Chain::Ethereum,
            Duration::seconds(60),
            Arc::new(MockGateway::new()),
        );
        let address = Bytes::from("0x01");
        let pending = Token::pending(&address, Chain::Ethereum);
        cache
            .add_tokens([pending.clone()])
            .await
            .unwrap();
        assert_eq!(cache.pending_token_addresses().await, vec![address.clone()]);
        let ready = Token::new(&address, "RECOVERED", 6, 25, &[Some(42_000)], Chain::Ethereum, 50);
        cache.add_tokens([ready]).await.unwrap();
        cache
            .add_tokens([pending])
            .await
            .unwrap();
        let result = cache
            .get_tokens(std::slice::from_ref(&address))
            .await
            .unwrap()
            .remove(0)
            .unwrap();
        assert!(result.metadata_status.is_ready());
        assert_eq!(
            (result.symbol.as_str(), result.decimals, result.tax, result.quality),
            ("RECOVERED", 6, 25, 50)
        );
        assert_eq!(result.gas, vec![Some(42_000)]);
        assert!(cache
            .pending_token_addresses()
            .await
            .is_empty());

        // Ready-to-ready updates still apply.
        cache
            .add_tokens([Token::new(&address, "V2", 6, 25, &[], Chain::Ethereum, 10)])
            .await
            .unwrap();
        let updated = cache
            .get_tokens(&[address])
            .await
            .unwrap()
            .remove(0)
            .unwrap();
        assert_eq!((updated.symbol.as_str(), updated.quality), ("V2", 10));
    }

    #[tokio::test]
    async fn db_fill_never_downgrades_a_ready_entry() {
        let ready_address = Bytes::from("0x01");
        let missing_address = Bytes::from("0x02");
        // A database read started before the recovery write committed still returns the
        // pending placeholder for the ready token alongside the missing one.
        let stale = Token::pending(&ready_address, Chain::Ethereum);
        let missing = Token::new(&missing_address, "B", 18, 0, &[None], Chain::Ethereum, 100);
        let mut gateway = MockGateway::new();
        gateway
            .expect_get_tokens()
            .times(1)
            .return_once(move |_, _, _, _, _| {
                Box::pin(
                    async move { Ok(WithTotal { entity: vec![stale, missing], total: Some(2) }) },
                )
            });
        let cache =
            ProtocolMemoryCache::new(Chain::Ethereum, Duration::seconds(60), Arc::new(gateway));
        cache
            .add_tokens([Token::new(
                &ready_address,
                "RECOVERED",
                6,
                25,
                &[Some(42_000)],
                Chain::Ethereum,
                50,
            )])
            .await
            .unwrap();

        let result = cache
            .get_tokens(&[ready_address, missing_address])
            .await
            .unwrap();

        let ready = result[0].as_ref().unwrap();
        assert!(ready.metadata_status.is_ready());
        assert_eq!((ready.symbol.as_str(), ready.decimals, ready.quality), ("RECOVERED", 6, 50));
        let filled = result[1].as_ref().unwrap();
        assert!(filled.metadata_status.is_ready());
        assert_eq!(filled.symbol, "B");
    }

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
