//! In-memory cache with TTL expiry, LRU eviction, and in-flight request
//! deduplication.
//!
//! `get_or_fetch` shares one fetch between concurrent misses, and a generation
//! counter prevents a `clear()` issued mid-flight from being repopulated with
//! stale data. Keys must never contain credentials (endpoint + sorted params).

use crate::api::ApiError;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

/// Default TTL: 5 minutes.
pub const DEFAULT_TTL_MS: u64 = 5 * 60 * 1000;
/// Default capacity before LRU eviction.
pub const DEFAULT_MAX_ENTRIES: usize = 100;

type Flight = Arc<OnceCell<Result<Value, ApiError>>>;

struct Entry {
    key: String,
    value: Value,
    expiry: Instant,
}

#[derive(Default)]
struct Inner {
    /// Least-recently-used first.
    entries: Vec<Entry>,
    inflight: HashMap<String, (Flight, u64)>,
    generation: u64,
}

/// LRU + TTL cache holding JSON values.
pub struct MemoryCache {
    inner: Mutex<Inner>,
    default_ttl: Duration,
    max_entries: usize,
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL_MS, DEFAULT_MAX_ENTRIES)
    }
}

impl MemoryCache {
    /// Create a cache with a default TTL and capacity.
    pub fn new(default_ttl_ms: u64, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            default_ttl: Duration::from_millis(default_ttl_ms),
            max_entries,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Number of retained entries.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cached value for `key` (refreshing its LRU position), if unexpired.
    pub fn get(&self, key: &str) -> Option<Value> {
        Self::get_locked(&mut self.lock(), key)
    }

    fn get_locked(inner: &mut Inner, key: &str) -> Option<Value> {
        let index = inner.entries.iter().position(|e| e.key == key)?;
        let entry = inner.entries.remove(index);
        if Instant::now() >= entry.expiry {
            return None;
        }
        let value = entry.value.clone();
        inner.entries.push(entry);
        Some(value)
    }

    /// Store a value, evicting the least-recently-used entry when full.
    pub fn set(&self, key: &str, value: Value, ttl_ms: Option<u64>) {
        let ttl = ttl_ms.map_or(self.default_ttl, Duration::from_millis);
        let mut inner = self.lock();
        self.set_locked(&mut inner, key, value, ttl);
    }

    fn set_locked(&self, inner: &mut Inner, key: &str, value: Value, ttl: Duration) {
        inner.entries.retain(|e| e.key != key);
        inner.entries.push(Entry {
            key: key.to_string(),
            value,
            expiry: Instant::now() + ttl,
        });
        while inner.entries.len() > self.max_entries {
            inner.entries.remove(0);
        }
    }

    /// Remove one entry.
    pub fn delete(&self, key: &str) -> bool {
        let mut inner = self.lock();
        let before = inner.entries.len();
        inner.entries.retain(|e| e.key != key);
        before != inner.entries.len()
    }

    /// Drop everything and invalidate in-flight fetches.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.entries.clear();
        inner.inflight.clear();
        inner.generation += 1;
    }

    /// Return the cached value or run `fetcher`, sharing it between concurrent misses.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: &str,
        ttl_ms: u64,
        fetcher: F,
    ) -> Result<Value, ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, ApiError>>,
    {
        let (flight, generation) = {
            let mut inner = self.lock();
            if let Some(value) = Self::get_locked(&mut inner, key) {
                return Ok(value);
            }
            let generation = inner.generation;
            inner
                .inflight
                .entry(key.to_string())
                .or_insert_with(|| (Arc::new(OnceCell::new()), generation))
                .clone()
        };
        let result = flight.get_or_init(fetcher).await.clone();
        let mut inner = self.lock();
        let owned = inner
            .inflight
            .get(key)
            .is_some_and(|(current, _)| Arc::ptr_eq(current, &flight));
        if owned {
            inner.inflight.remove(key);
            if let Ok(value) = &result
                && inner.generation == generation
            {
                self.set_locked(
                    &mut inner,
                    key,
                    value.clone(),
                    Duration::from_millis(ttl_ms),
                );
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn evicts_least_recently_used() {
        let cache = MemoryCache::new(60_000, 2);
        cache.set("a", json!(1), None);
        cache.set("b", json!(2), None);
        assert_eq!(cache.get("a"), Some(json!(1)));
        cache.set("c", json!(3), None);
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a"), Some(json!(1)));
        assert!(cache.delete("a"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn expires_entries() {
        let cache = MemoryCache::default();
        cache.set("a", json!(1), Some(0));
        assert_eq!(cache.get("a"), None);
    }

    #[tokio::test]
    async fn deduplicates_concurrent_fetches() {
        let cache = Arc::new(MemoryCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |cache: Arc<MemoryCache>, calls: Arc<AtomicUsize>| async move {
            cache
                .get_or_fetch("k", 60_000, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(json!("v"))
                })
                .await
        };
        let (a, b) = tokio::join!(
            fetch(cache.clone(), calls.clone()),
            fetch(cache.clone(), calls.clone())
        );
        assert_eq!(a.unwrap(), json!("v"));
        assert_eq!(b.unwrap(), json!("v"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get("k"), Some(json!("v")));
    }

    #[tokio::test]
    async fn clear_prevents_stale_repopulation_and_errors_are_not_cached() {
        let cache = Arc::new(MemoryCache::default());
        let inner = cache.clone();
        let result = cache
            .get_or_fetch("k", 60_000, || async move {
                inner.clear();
                Ok(json!("stale"))
            })
            .await;
        assert_eq!(result.unwrap(), json!("stale"));
        assert_eq!(cache.get("k"), None);
        let failed = cache
            .get_or_fetch("e", 60_000, || async { Err(ApiError::Network("x".into())) })
            .await;
        assert!(failed.is_err());
        let retried = cache
            .get_or_fetch("e", 60_000, || async { Ok(json!(1)) })
            .await;
        assert_eq!(retried.unwrap(), json!(1));
    }
}
