//! In-memory adapter of the Feed Serving module's cache seam, used by the
//! hermetic tests: no Redis, no network, no credentials. Expirations mirror
//! the Redis behavior through the injected clock.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::{Clock, FeedCacheStore};
use crate::feed_cache::CachedFeed;

#[derive(Clone)]
struct MemRecord {
    feed: CachedFeed,
    /// Unix seconds after which the body may no longer be served (the Stale
    /// Window anchored at store time).
    body_expires_at: i64,
    /// Unix seconds after which the fresh marker expires; `None` when no
    /// marker is set.
    fresh_expires_at: Option<i64>,
}

/// Cache adapter storing entries in a process-local map.
#[derive(Clone)]
pub struct InMemoryCache {
    records: Arc<Mutex<HashMap<String, MemRecord>>>,
    /// Quota Breaker flags, keyed by the provider-owned breaker key and
    /// carrying the unix time each holds until.
    breakers: Arc<Mutex<HashMap<String, i64>>>,
    clock: Arc<dyn Clock>,
    quota_usage: Arc<Mutex<i64>>,
}

impl InMemoryCache {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            records: Arc::default(),
            breakers: Arc::default(),
            clock,
            quota_usage: Arc::default(),
        }
    }
}

#[async_trait]
impl FeedCacheStore for InMemoryCache {
    async fn load(&self, canonical_key: &str) -> Option<CachedFeed> {
        let mut records = self.records.lock().unwrap();
        let now = self.clock.unix_now();
        let record = records.get(canonical_key)?;
        if now >= record.body_expires_at {
            records.remove(canonical_key);
            return None;
        }
        Some(record.feed.clone())
    }

    async fn fresh_marker_ttl(&self, canonical_key: &str) -> Option<i64> {
        let records = self.records.lock().unwrap();
        let now = self.clock.unix_now();
        let fresh_expires_at = records.get(canonical_key)?.fresh_expires_at?;
        let remaining = fresh_expires_at - now;
        (remaining > 0).then_some(remaining)
    }

    async fn store(
        &self,
        canonical_key: &str,
        feed: &CachedFeed,
        fresh_ttl_secs: u64,
        stale_window_secs: u64,
    ) -> eyre::Result<()> {
        let now = self.clock.unix_now();
        let mut records = self.records.lock().unwrap();
        records.insert(
            canonical_key.to_string(),
            MemRecord {
                feed: feed.clone(),
                body_expires_at: now + stale_window_secs as i64,
                fresh_expires_at: Some(now + fresh_ttl_secs as i64),
            },
        );
        Ok(())
    }

    async fn bump_quota_usage(&self, units: u64) -> eyre::Result<i64> {
        let mut total = self.quota_usage.lock().unwrap();
        *total += units as i64;
        Ok(*total)
    }

    async fn renew_freshness(
        &self,
        canonical_key: &str,
        fresh_ttl_secs: u64,
        stale_window_secs: u64,
    ) -> eyre::Result<()> {
        let now = self.clock.unix_now();
        let mut records = self.records.lock().unwrap();
        match records.get_mut(canonical_key) {
            Some(record) => {
                record.fresh_expires_at = Some(now + fresh_ttl_secs as i64);
                record.body_expires_at = now + stale_window_secs as i64;
                Ok(())
            }
            // the entry expired between the load and the renewal: behave like
            // Redis, which re-anchors the marker anyway (a marker without a
            // body classifies as a miss)
            None => Ok(()),
        }
    }

    async fn quota_breaker_open(&self, breaker_key: &str) -> Option<i64> {
        let breakers = self.breakers.lock().unwrap();
        let reset = *breakers.get(breaker_key)?;
        (reset > self.clock.unix_now()).then_some(reset)
    }

    async fn open_quota_breaker(
        &self,
        breaker_key: &str,
        reset_epoch_secs: i64,
    ) -> eyre::Result<()> {
        // mirror the Redis adapter: no breaker into the past
        if reset_epoch_secs <= self.clock.unix_now() {
            return Ok(());
        }
        self.breakers
            .lock()
            .unwrap()
            .insert(breaker_key.to_string(), reset_epoch_secs);
        Ok(())
    }
}

impl InMemoryCache {
    /// Total quota usage recorded so far; lets tests observe the probe's
    /// ~1-unit cost.
    #[cfg(test)]
    pub async fn total_quota_usage(&self) -> i64 {
        *self.quota_usage.lock().unwrap()
    }

    /// The unix time the breaker identified by `breaker_key` holds until,
    /// regardless of the clock; lets tests assert what opening a breaker
    /// recorded.
    #[cfg(test)]
    pub async fn breaker_reset(&self, breaker_key: &str) -> Option<i64> {
        self.breakers.lock().unwrap().get(breaker_key).copied()
    }
}
