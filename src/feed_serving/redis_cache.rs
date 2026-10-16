//! Redis adapter of the Feed Serving module's cache seam. It delegates to the
//! feed_cache storage functions so the production data layout — and therefore
//! every cache written by earlier releases — is preserved byte for byte.

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;

use super::FeedCacheStore;
use crate::feed_cache::CachedFeed;

/// Cache adapter backed by Redis. Holds a handle on one multiplexed
/// connection; clones of it share the same connection, so serving keeps the
/// one-connection-per-request pattern.
#[derive(Clone)]
pub struct RedisFeedCache {
    con: MultiplexedConnection,
}

impl RedisFeedCache {
    pub fn new(con: MultiplexedConnection) -> Self {
        Self { con }
    }
}

#[async_trait]
impl FeedCacheStore for RedisFeedCache {
    async fn load(&self, canonical_key: &str) -> Option<CachedFeed> {
        let mut con = self.con.clone();
        crate::feed_cache::load(&mut con, canonical_key).await
    }

    async fn fresh_marker_ttl(&self, canonical_key: &str) -> Option<i64> {
        let mut con = self.con.clone();
        crate::feed_cache::fresh_marker_ttl(&mut con, canonical_key).await
    }

    async fn store(
        &self,
        canonical_key: &str,
        feed: &CachedFeed,
        fresh_ttl_secs: u64,
        stale_window_secs: u64,
    ) -> eyre::Result<()> {
        let mut con = self.con.clone();
        crate::feed_cache::store(
            &mut con,
            canonical_key,
            feed,
            fresh_ttl_secs,
            stale_window_secs,
        )
        .await
    }

    async fn bump_quota_usage(&self, units: u64, day_key: &str) -> eyre::Result<i64> {
        let mut con = self.con.clone();
        crate::feed_cache::bump_quota_usage(&mut con, day_key, units).await
    }

    async fn renew_freshness(
        &self,
        canonical_key: &str,
        fresh_ttl_secs: u64,
        stale_window_secs: u64,
    ) -> eyre::Result<()> {
        let mut con = self.con.clone();
        crate::feed_cache::renew_freshness(
            &mut con,
            canonical_key,
            fresh_ttl_secs,
            stale_window_secs,
        )
        .await
    }

    async fn quota_breaker_open(&self, breaker_key: &str) -> Option<i64> {
        let mut con = self.con.clone();
        crate::feed_cache::quota_breaker_open(&mut con, breaker_key).await
    }

    async fn open_quota_breaker(
        &self,
        breaker_key: &str,
        reset_epoch_secs: i64,
    ) -> eyre::Result<()> {
        let mut con = self.con.clone();
        crate::feed_cache::open_quota_breaker(&mut con, breaker_key, reset_epoch_secs).await
    }
}
