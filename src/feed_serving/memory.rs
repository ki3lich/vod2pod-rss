//! In-memory adapter of the Feed Serving module's cache seam, used by the
//! hermetic tests: no Redis, no network, no credentials. Expirations mirror
//! the Redis behavior (server-side TTLs there, anchored instants here), all
//! judged against the `now` the serving module passes in — the adapter holds
//! no clock of its own.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::{CachedFeed, FeedCacheStore, FreshnessWindows, LoadState};

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
    quota_usage: Arc<Mutex<i64>>,
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self {
            records: Arc::default(),
            breakers: Arc::default(),
            quota_usage: Arc::default(),
        }
    }
}

#[async_trait]
impl FeedCacheStore for InMemoryCache {
    async fn load_state(&self, canonical_key: &str, now: i64) -> LoadState {
        let mut records = self.records.lock().unwrap();
        let Some(record) = records.get(canonical_key) else {
            return LoadState::default();
        };
        if now >= record.body_expires_at {
            records.remove(canonical_key);
            return LoadState::default();
        }
        LoadState {
            feed: Some(record.feed.clone()),
            fresh_marker_ttl: record.fresh_expires_at.and_then(|expires_at| {
                let remaining = expires_at - now;
                (remaining > 0).then_some(remaining)
            }),
        }
    }

    async fn store_and_report(
        &self,
        canonical_key: &str,
        feed: &CachedFeed,
        windows: FreshnessWindows,
        now: i64,
    ) -> eyre::Result<i64> {
        let mut records = self.records.lock().unwrap();
        records.insert(
            canonical_key.to_string(),
            MemRecord {
                feed: feed.clone(),
                body_expires_at: now + windows.stale_window_secs as i64,
                fresh_expires_at: Some(now + windows.fresh_ttl_secs as i64),
            },
        );
        Ok(windows.fresh_ttl_secs as i64)
    }

    async fn bump_quota_usage(&self, units: u64, _day_key: &str) -> eyre::Result<i64> {
        let mut total = self.quota_usage.lock().unwrap();
        *total += units as i64;
        Ok(*total)
    }

    async fn renew_freshness(
        &self,
        canonical_key: &str,
        windows: FreshnessWindows,
        now: i64,
    ) -> eyre::Result<()> {
        let mut records = self.records.lock().unwrap();
        match records.get_mut(canonical_key) {
            Some(record) => {
                record.fresh_expires_at = Some(now + windows.fresh_ttl_secs as i64);
                record.body_expires_at = now + windows.stale_window_secs as i64;
                Ok(())
            }
            // the entry expired between the load and the renewal: behave like
            // Redis, which re-anchors the marker anyway (a marker without a
            // body classifies as a miss)
            None => Ok(()),
        }
    }

    async fn quota_breaker_open(&self, breaker_key: &str, now: i64) -> Option<i64> {
        let breakers = self.breakers.lock().unwrap();
        let reset = *breakers.get(breaker_key)?;
        (reset > now).then_some(reset)
    }

    async fn open_quota_breaker(
        &self,
        breaker_key: &str,
        reset_epoch_secs: i64,
        now: i64,
    ) -> eyre::Result<()> {
        // mirror the Redis adapter: no breaker into the past
        if reset_epoch_secs <= now {
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

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "k";

    const FRESH_1H_STALE_7D: FreshnessWindows = FreshnessWindows {
        fresh_ttl_secs: 3600,
        stale_window_secs: 7 * 24 * 3600,
    };

    /// Store a copy as a conversion would have, at the given time.
    async fn store_at(cache: &InMemoryCache, at: i64, body: &str, windows: FreshnessWindows) {
        let feed = CachedFeed {
            body: body.to_string(),
            generated_at: at,
            probe_state: None,
            degraded: false,
        };
        cache
            .store_and_report(KEY, &feed, windows, at)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn load_state_returns_the_stored_copy_and_the_fresh_marker_ttl() {
        let cache = InMemoryCache::new();
        store_at(&cache, 1_000, "<rss>cached</rss>", FRESH_1H_STALE_7D).await;

        let state = cache.load_state(KEY, 1_100).await;

        let feed = state.feed.expect("the stored copy is servable");
        assert_eq!(feed.body, "<rss>cached</rss>");
        assert_eq!(feed.generated_at, 1_000);
        // one call carries both halves: the copy and the marker ttl
        assert_eq!(state.fresh_marker_ttl, Some(3600 - 100));
    }

    #[tokio::test]
    async fn load_state_serves_stale_once_the_fresh_marker_dies() {
        let cache = InMemoryCache::new();
        store_at(&cache, 1_000, "<rss>cached</rss>", FRESH_1H_STALE_7D).await;

        let state = cache.load_state(KEY, 1_000 + 3600 + 1).await;

        // the marker is gone but the copy still lives (the Stale Window)
        assert_eq!(state.fresh_marker_ttl, None);
        assert!(state.feed.is_some());
    }

    #[tokio::test]
    async fn load_state_misses_once_the_stale_window_has_passed() {
        let cache = InMemoryCache::new();
        store_at(&cache, 0, "<rss>cached</rss>", FRESH_1H_STALE_7D).await;

        let state = cache.load_state(KEY, 7 * 24 * 3600).await;

        assert!(state.feed.is_none());
        assert_eq!(state.fresh_marker_ttl, None);
    }

    #[tokio::test]
    async fn store_and_report_reports_the_ttl_it_anchored() {
        let cache = InMemoryCache::new();

        let reported = cache
            .store_and_report(
                KEY,
                &CachedFeed {
                    body: "<rss>new</rss>".to_string(),
                    generated_at: 1_000,
                    probe_state: None,
                    degraded: false,
                },
                FRESH_1H_STALE_7D,
                1_000,
            )
            .await
            .unwrap();

        // the report is the fresh ttl the store anchored: no re-read round
        // trip is needed to learn it
        assert_eq!(reported, 3600);
        let state = cache.load_state(KEY, 1_000).await;
        assert_eq!(state.fresh_marker_ttl, Some(3600));
    }

    #[tokio::test]
    async fn store_and_report_replaces_the_previous_copy_and_reanchors_freshness() {
        let cache = InMemoryCache::new();
        store_at(&cache, 1_000, "<rss>old</rss>", FRESH_1H_STALE_7D).await;

        store_at(
            &cache,
            5_000,
            "<rss>new</rss>",
            FreshnessWindows {
                fresh_ttl_secs: 60,
                stale_window_secs: 7 * 24 * 3600,
            },
        )
        .await;

        let state = cache.load_state(KEY, 5_010).await;
        let feed = state.feed.expect("the new copy replaced the old one");
        assert_eq!(feed.body, "<rss>new</rss>");
        assert_eq!(feed.generated_at, 5_000);
        assert_eq!(state.fresh_marker_ttl, Some(60 - 10));
    }

    #[tokio::test]
    async fn breaker_expiry_is_judged_against_the_time_passed_in() {
        let cache = InMemoryCache::new();
        cache.open_quota_breaker(KEY, 2_000, 1_000).await.unwrap();

        // open while now is before the reset, expired after it: the adapter
        // owns no clock, the caller's now decides
        assert_eq!(cache.quota_breaker_open(KEY, 1_500).await, Some(2_000));
        assert_eq!(cache.quota_breaker_open(KEY, 2_500).await, None);
    }

    #[tokio::test]
    async fn opening_the_breaker_into_the_past_is_a_no_op() {
        let cache = InMemoryCache::new();

        cache.open_quota_breaker(KEY, 500, 1_000).await.unwrap();

        assert_eq!(cache.quota_breaker_open(KEY, 1_000).await, None);
        assert_eq!(cache.breaker_reset(KEY).await, None);
    }
}
