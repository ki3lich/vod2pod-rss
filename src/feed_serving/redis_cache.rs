//! Redis adapter of the Feed Serving module's cache seam. It owns the whole
//! storage mechanics: the production data layout — and therefore every cache
//! written by earlier releases — is preserved byte for byte, so an upgrade
//! neither invalidates existing caches nor changes serving. Each widened
//! operation is pipelined into a single storage round trip; TTLs are
//! anchored by the Redis server itself, and every time-dependent judgment
//! (Quota Breaker expiry) uses the `now` the serving module passes in.

use std::collections::HashMap;

use async_trait::async_trait;
use log::warn;
use redis::aio::MultiplexedConnection;

use super::{CachedFeed, FeedCacheStore, FreshnessWindows, LoadState};

const REDIS_FEED_PREFIX: &str = "vod2pod:feed:";
const REDIS_FRESH_PREFIX: &str = "vod2pod:fresh:";
const REDIS_QUOTA_USAGE_PREFIX: &str = "vod2pod:quota_used:";

fn feed_key(canonical_id: &str) -> String {
    format!("{REDIS_FEED_PREFIX}{canonical_id}")
}

fn fresh_key(canonical_id: &str) -> String {
    format!("{REDIS_FRESH_PREFIX}{canonical_id}")
}

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

/// Parse the stored feed hash. The body is the only mandatory field; missing
/// metadata parses to the same defaults the layout has always served.
fn parse_cached_feed(map: &HashMap<String, String>) -> Option<CachedFeed> {
    let body = map.get("body")?;
    Some(CachedFeed {
        generated_at: map
            .get("generated_at")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        probe_state: map
            .get("probe_state")
            .filter(|json| !json.is_empty())
            .and_then(|json| serde_json::from_str(json).ok()),
        degraded: map.get("degraded").map(|v| v == "1").unwrap_or(false),
        body: body.clone(),
    })
}

#[async_trait]
impl FeedCacheStore for RedisFeedCache {
    async fn load_state(&self, canonical_key: &str, _now: i64) -> LoadState {
        let mut con = self.con.clone();
        // one round trip: the stored feed hash and the fresh marker ttl
        let state: Result<(HashMap<String, String>, i64), _> = redis::pipe()
            .cmd("HGETALL")
            .arg(feed_key(canonical_key))
            .cmd("TTL")
            .arg(fresh_key(canonical_key))
            .query_async(&mut con)
            .await;
        match state {
            Ok((map, ttl)) => LoadState {
                feed: parse_cached_feed(&map),
                fresh_marker_ttl: (ttl > 0).then_some(ttl),
            },
            Err(e) => {
                warn!("could not load cache state for {canonical_key}: {e}");
                LoadState::default()
            }
        }
    }

    async fn store_and_report(
        &self,
        canonical_key: &str,
        feed: &CachedFeed,
        windows: FreshnessWindows,
        _now: i64,
    ) -> eyre::Result<i64> {
        let mut con = self.con.clone();
        let probe_json = feed
            .probe_state
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_default();

        // one round trip: the feed hash, its Stale Window, and the fresh
        // marker with its Fresh TTL
        redis::pipe()
            .cmd("HSET")
            .arg(feed_key(canonical_key))
            .arg("body")
            .arg(&feed.body)
            .arg("generated_at")
            .arg(feed.generated_at)
            .arg("probe_state")
            .arg(probe_json)
            .arg("degraded")
            .arg(if feed.degraded { "1" } else { "0" })
            .cmd("EXPIRE")
            .arg(feed_key(canonical_key))
            .arg(windows.stale_window_secs)
            .cmd("SET")
            .arg(fresh_key(canonical_key))
            .arg(1)
            .arg("EX")
            .arg(windows.fresh_ttl_secs)
            .query_async::<()>(&mut con)
            .await?;

        // the marker was just anchored with exactly the Fresh TTL: that is
        // the report, no re-read round trip needed
        Ok(windows.fresh_ttl_secs as i64)
    }

    async fn bump_quota_usage(&self, units: u64, day_key: &str) -> eyre::Result<i64> {
        let mut con = self.con.clone();
        let key = format!("{REDIS_QUOTA_USAGE_PREFIX}{day_key}");
        // one round trip: the increment and the 48h retention that keeps the
        // running total alive across a bit of clock skew around the
        // usage-day boundary
        let (total,): (i64,) = redis::pipe()
            .cmd("INCRBY")
            .arg(&key)
            .arg(units as i64)
            .cmd("EXPIRE")
            .arg(&key)
            .arg(48 * 3600)
            .query_async(&mut con)
            .await?;
        Ok(total)
    }

    async fn renew_freshness(
        &self,
        canonical_key: &str,
        windows: FreshnessWindows,
        _now: i64,
    ) -> eyre::Result<()> {
        let mut con = self.con.clone();
        // one round trip: the marker gets a new full TTL, the body TTL is
        // re-anchored to the Stale Window
        redis::pipe()
            .cmd("SET")
            .arg(fresh_key(canonical_key))
            .arg(1)
            .arg("EX")
            .arg(windows.fresh_ttl_secs)
            .cmd("EXPIRE")
            .arg(feed_key(canonical_key))
            .arg(windows.stale_window_secs)
            .query_async::<()>(&mut con)
            .await?;
        Ok(())
    }

    async fn quota_breaker_open(&self, breaker_key: &str, now: i64) -> Option<i64> {
        let mut con = self.con.clone();
        let reset: Option<i64> = redis::cmd("GET")
            .arg(breaker_key)
            .query_async(&mut con)
            .await
            .unwrap_or_default();
        reset.filter(|reset| *reset > now)
    }

    async fn open_quota_breaker(
        &self,
        breaker_key: &str,
        reset_epoch_secs: i64,
        now: i64,
    ) -> eyre::Result<()> {
        let mut con = self.con.clone();
        if reset_epoch_secs <= now {
            return Ok(());
        }
        redis::cmd("SET")
            .arg(breaker_key)
            .arg(reset_epoch_secs)
            .arg("EX")
            .arg(reset_epoch_secs - now)
            .query_async::<()>(&mut con)
            .await?;
        Ok(())
    }
}
