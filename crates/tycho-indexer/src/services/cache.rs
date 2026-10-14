use std::{
    error::Error,
    fmt::Debug,
    hash::Hash,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use deepsize::DeepSizeOf;
use futures03::Future;
use metrics::{counter, gauge};
use mini_moka::sync::Cache;
use tokio::sync::MutexGuard;
use tracing::{debug, instrument, trace, Level};

/// A wrapper around the cached value that also tracks its size.
/// The value is wrapped in a Mutex to allow for in-flight requests to be handled correctly.
/// The size is stored in an AtomicU32 to allow for concurrent read/write access.
struct ValueWithSize<V> {
    value: tokio::sync::Mutex<Option<Arc<V>>>,
    size: AtomicU32,
}

impl<V: DeepSizeOf + Send + Sync> ValueWithSize<V> {
    /// Returns the previous snapshot size of the cached value.
    /// Requires to be set manually when the value is updated such that the function is
    /// non-blocking.
    fn size(&self) -> u32 {
        self.size.load(Ordering::Relaxed)
    }

    /// Creates a placeholder ValueWithSize with no value and size 0.
    /// This is used to indicate that a request is in-flight.
    async fn placeholder() -> ValueWithSize<V> {
        Self { value: tokio::sync::Mutex::new(None), size: AtomicU32::new(0) }
    }

    /// Updates the cached value and its size.
    /// This requires a lock on the value to ensure that no other thread is reading or writing
    /// the value at the same time.
    fn update(&self, new_value: Arc<V>, mut value_update_guard: MutexGuard<Option<Arc<V>>>) {
        self.size
            .store(new_value.deep_size_of() as u32, Ordering::Relaxed);
        *value_update_guard = Some(new_value);
    }
}

/// The response caches of the RPC handler, one per endpoint. Set the ones to turn off per
/// deployment with `DISABLED_RPC_CACHES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum RpcCacheKind {
    /// `/contract_state` responses.
    ContractStorage,
    /// `/protocol_state` responses.
    ProtocolState,
    /// `/protocol_components` responses.
    ProtocolComponents,
    /// `/traced_entry_points` responses.
    TracedEntryPoints,
}

pub struct RpcCache<R, V> {
    name: String,
    cache: Cache<R, Arc<ValueWithSize<V>>>,
    /// When `false`, every `get` runs the fallback and stores nothing.
    enabled: bool,
}

impl<R, V> RpcCache<R, V>
where
    R: Clone + Hash + Eq + Send + Sync + Debug + DeepSizeOf + 'static,
    V: Send + Sync + DeepSizeOf + 'static,
{
    pub fn new(name: &str, capacity: u64, ttl: u64) -> Self {
        let weigher = Box::new(|key: &R, value_with_size: &Arc<ValueWithSize<V>>| {
            let key_weight = key.deep_size_of() as u32;
            value_with_size
                .size()
                .saturating_add(key_weight)
        });

        let cache = Cache::builder()
            .max_capacity(capacity)
            .time_to_live(std::time::Duration::from_secs(ttl))
            .weigher(weigher)
            .build();

        Self { name: name.to_string(), cache, enabled: true }
    }

    /// Turns the cache off: every `get` runs the fallback and stores nothing.
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    #[instrument(
        name = "rpc.cache.get",
        level = Level::TRACE,
        fields(miss, should_cache, size, resource = self.name),
        skip(self, fallback))
    ]
    pub async fn get<
        'a,
        E: Error,
        Fut: Future<Output = Result<(V, bool), E>> + Send + 'a,
        F: Fn(R) -> Fut + Send + Sync,
    >(
        &'a self,
        request: R,
        fallback: F,
    ) -> Result<Arc<V>, E> {
        if !self.enabled {
            trace!("CacheDisabled");
            let (response, _) = (fallback)(request).await?;
            return Ok(Arc::new(response));
        }
        tracing::Span::current().record("size", self.cache.entry_count());
        // Check the cache for a cached response
        if let Some(inflight_val) = self.cache.get(&request) {
            // the value is present or is being written to
            // If the values is None, we likely hit a should_cache = false entry while in-flight,
            //  so we simply behave as if it was a cache miss.
            if let Some(res) = inflight_val.value.lock().await.clone() {
                tracing::Span::current().record("miss", false);
                trace!("CacheHit");
                counter!("rpc_cache_hits", "cache" => self.name.clone()).increment(1);

                return Ok(res);
            }
        }

        tracing::Span::current().record("miss", true);
        trace!("CacheMiss");
        counter!("rpc_cache_misses", "cache" => self.name.clone()).increment(1);

        // Log weighted size on cache miss
        let cache_weighted_size = self.cache.weighted_size();
        tracing::Span::current().record("weighted_size", cache_weighted_size);
        debug!(cache_weighted_size, name = self.name, "CacheWeightedSize");
        gauge!("rpc_cache_weighted_size", "cache" => self.name.clone())
            .set(cache_weighted_size as f64);
        gauge!("rpc_cache_entry_count", "cache" => self.name.clone())
            .set(self.cache.entry_count() as f64);

        let value = Arc::new(ValueWithSize::placeholder().await);
        let value_update_guard = value.value.lock().await;

        // We insert a None value here to indicate that this request is in flight.
        //  In some cases this None value may leak to the reading part above, e.g.
        //  if the fallback failed or if the value is not safe to be cached.
        self.cache
            .insert(request.clone(), value.clone());
        trace!("SavedPlaceholder");

        let (response, should_cache) = (fallback)(request.clone())
            .await
            .inspect_err(|_| {
                // invalidate the cache if the fallback errors
                self.cache.invalidate(&request);
                trace!("FallbackFailure");
            })?;
        let arc_response = Arc::new(response);

        // PERF: unnecessary lock if we don't cache the value, could be improved
        //  if `should_cache` value can be determined beforehand.
        if should_cache {
            tracing::Span::current().record("should_cache", true);
            // Update the placeholder placed in the cache with the actual value
            value.update(arc_response.clone(), value_update_guard);
            // Re-insert to update the weight in the cache
            self.cache
                .insert(request.clone(), value.clone());
            trace!("UpdatedPlaceholder")
        } else {
            drop(value_update_guard);
            self.cache.invalidate(&request);
        }
        Ok(arc_response)
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use deepsize::DeepSizeOf;
    use futures03::future::try_join_all;
    use mini_moka::sync::ConcurrentCacheExt;
    use tokio::sync::Mutex;

    use crate::services::{cache::RpcCache, rpc::RpcError};

    #[test_log::test(tokio::test)]
    async fn test_sequential_access() {
        let access_counter = Arc::new(Mutex::new(0));
        let cache = RpcCache::<String, i32>::new("test", 100, 3600);

        cache
            .get("k0".to_string(), |_| async { increment_counter(access_counter.clone()).await })
            .await
            .unwrap();

        cache
            .get("k0".to_string(), |_| async { increment_counter(access_counter.clone()).await })
            .await
            .unwrap();

        let v = *access_counter.lock().await;
        assert_eq!(v, 1);
    }

    async fn increment_counter(access_counter: Arc<Mutex<i32>>) -> Result<(i32, bool), RpcError> {
        let mut guard = access_counter.lock().await;
        *guard += 1;
        Ok((1, true))
    }

    #[test_log::test(tokio::test)]
    async fn test_disabled_cache_runs_fallback_every_time() {
        let access_counter = Arc::new(Mutex::new(0));
        let mut cache = RpcCache::<String, i32>::new("test", 100, 3600);
        cache.disable();

        for _ in 0..2 {
            cache
                .get("k0".to_string(), |_| async {
                    increment_counter(access_counter.clone()).await
                })
                .await
                .unwrap();
        }

        assert_eq!(*access_counter.lock().await, 2);
        assert_eq!(cache.cache.entry_count(), 0);
    }

    #[test_log::test(tokio::test)]
    async fn test_parallel_access() {
        let access_counter = Arc::new(Mutex::new(0));
        let cache = RpcCache::<String, i32>::new("test", 100, 3600);
        let tasks: Vec<_> = (0..10)
            .map(|_| {
                cache.get("k0".to_string(), |_| async {
                    increment_counter(access_counter.clone()).await
                })
            })
            .collect();

        try_join_all(tasks)
            .await
            .expect("a task failed");

        let v = *access_counter.lock().await;
        assert_eq!(v, 1);
    }

    async fn increment_counter_unsafe_value(
        access_counter: Arc<Mutex<i32>>,
    ) -> Result<(i32, bool), RpcError> {
        let mut guard = access_counter.lock().await;
        *guard += 1;
        Ok((1, false))
    }

    #[test_log::test(tokio::test)]
    async fn test_parallel_access_unsafe_cache() {
        let access_counter = Arc::new(Mutex::new(0));
        let cache = RpcCache::<usize, i32>::new("test", 100, 3600);
        let tasks: Vec<_> = (0..10)
            .map(|i| {
                cache.get(i % 2, |_| async {
                    increment_counter_unsafe_value(access_counter.clone()).await
                })
            })
            .collect();

        try_join_all(tasks)
            .await
            .expect("a task failed");

        let v = *access_counter.lock().await;
        assert_eq!(v, 10);
    }

    #[test_log::test(tokio::test)]
    async fn test_cache_evicts_when_over_capacity() {
        const CAPACITY: u64 = 1000;
        let cache = RpcCache::<u8, Vec<u8>>::new("eviction", CAPACITY, 3600);

        let mut keys = Vec::new();
        let mut total_expected_weight = 0usize;

        for i in 0..5 {
            let key = i;
            let value = vec![i; 256];
            total_expected_weight += key.deep_size_of() + value.deep_size_of();
            let value_for_fallback = value.clone();
            cache
                .get(key, move |_| {
                    let value_for_fallback = value_for_fallback.clone();
                    async move {
                        // simulate some latency in the fallback
                        tokio::time::sleep(core::time::Duration::from_millis(10)).await;

                        Ok::<(Vec<u8>, bool), RpcError>((value_for_fallback, true))
                    }
                })
                .await
                .unwrap();

            keys.push(key);
        }

        cache.cache.sync();

        assert!(
            total_expected_weight as u64 > CAPACITY,
            "test precondition failed: expected inserted weight {total_expected_weight} to exceed capacity {CAPACITY}"
        );

        assert_eq!(
            cache.cache.entry_count(),
            3,
            "cache should evict entries once weight exceeds capacity"
        );

        let evicted_count = keys
            .iter()
            .filter(|k| cache.cache.get(*k).is_none())
            .count();
        assert_eq!(
            evicted_count, 2,
            "expected at least one key to be evicted after exceeding capacity"
        );

        assert!(
            cache.cache.weighted_size() <= CAPACITY,
            "cache weighted size {} should be <= capacity {} after eviction",
            cache.cache.weighted_size(),
            CAPACITY
        );
    }
}
