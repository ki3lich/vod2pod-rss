//! Redis storage of cached Feeds: the record layout, the fresh markers, the
//! Quota Breaker records and the usage estimate buckets. Generic storage
//! mechanics only: no provider names, no provider error strings, no reset
//! timezones — those facts live behind the provider seam. The serving policy
//! and the Degradation Ladder live in the Feed Serving module
//! (`feed_serving`); this module is the storage mechanics its Redis adapter
//! delegates to.
//!
//! Domain terms live in CONTEXT.md; the rationale of the serving policy is in
//! docs/adr/0001-serve-stale-and-degrade-instead-of-failing.md.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use log::warn;

use crate::provider::FeedProbeState;

pub const REDIS_FEED_PREFIX: &str = "vod2pod:feed:";
pub const REDIS_FRESH_PREFIX: &str = "vod2pod:fresh:";
pub const REDIS_QUOTA_BREAKER_PREFIX: &str = "vod2pod:quota_breaker:";
pub const REDIS_QUOTA_USAGE_PREFIX: &str = "vod2pod:quota_used:";

pub fn feed_key(canonical_id: &str) -> String {
    format!("{REDIS_FEED_PREFIX}{canonical_id}")
}

pub fn fresh_key(canonical_id: &str) -> String {
    format!("{REDIS_FRESH_PREFIX}{canonical_id}")
}

/// Current unix time in seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A cached feed: the raw body plus everything needed to revalidate it
/// cheaply. The body is stored BEFORE per-request customization (transcoding
/// url injection): that step depends on the requesting host, so it is redone
/// on every serve and the response ETag is computed from the injected body.
#[derive(Clone, Debug)]
pub struct CachedFeed {
    pub body: String,
    /// When the body was produced (unix seconds). Drives the Fresh Period
    /// and the Stale Window.
    pub generated_at: i64,
    pub probe_state: Option<FeedProbeState>,
    /// `true` when the body came from the quota-free Degradation Ladder rung
    /// (fewer items than a full conversion); such bodies are never renewed by
    /// a probe, so they expire normally and get replaced by a full feed.
    pub degraded: bool,
}

/// Read the cached feed for a Canonical Feed Key. A redis failure is reported
/// and behaves like a cache miss: the degradation ladder still serves
/// clients.
pub async fn load(
    con: &mut redis::aio::MultiplexedConnection,
    canonical_id: &str,
) -> Option<CachedFeed> {
    let map: HashMap<String, String> = match redis::cmd("HGETALL")
        .arg(feed_key(canonical_id))
        .query_async(con)
        .await
    {
        Ok(map) => map,
        Err(e) => {
            warn!("could not read cached feed {canonical_id}: {e}");
            return None;
        }
    };

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

/// Store a feed (overwriting any previous one) and mark it fresh.
pub async fn store(
    con: &mut redis::aio::MultiplexedConnection,
    canonical_id: &str,
    feed: &CachedFeed,
    fresh_ttl_secs: u64,
    stale_window_secs: u64,
) -> eyre::Result<()> {
    let probe_json = feed
        .probe_state
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_default();

    redis::cmd("HSET")
        .arg(feed_key(canonical_id))
        .arg("body")
        .arg(&feed.body)
        .arg("generated_at")
        .arg(feed.generated_at)
        .arg("probe_state")
        .arg(probe_json)
        .arg("degraded")
        .arg(if feed.degraded { "1" } else { "0" })
        .query_async::<()>(con)
        .await?;
    redis::cmd("EXPIRE")
        .arg(feed_key(canonical_id))
        .arg(stale_window_secs)
        .query_async::<()>(con)
        .await?;

    set_fresh_marker(con, canonical_id, fresh_ttl_secs).await
}

pub async fn set_fresh_marker(
    con: &mut redis::aio::MultiplexedConnection,
    canonical_id: &str,
    fresh_ttl_secs: u64,
) -> eyre::Result<()> {
    redis::cmd("SET")
        .arg(fresh_key(canonical_id))
        .arg(1)
        .arg("EX")
        .arg(fresh_ttl_secs)
        .query_async::<()>(con)
        .await?;
    Ok(())
}

/// Remaining seconds of the fresh marker, or `None` when it is gone (or its
/// TTL could not be read).
pub async fn fresh_marker_ttl(
    con: &mut redis::aio::MultiplexedConnection,
    canonical_id: &str,
) -> Option<i64> {
    let ttl: i64 = match redis::cmd("TTL")
        .arg(fresh_key(canonical_id))
        .query_async(con)
        .await
    {
        Ok(ttl) => ttl,
        Err(e) => {
            warn!("could not read fresh marker ttl for {canonical_id}: {e}");
            return None;
        }
    };
    (ttl > 0).then_some(ttl)
}

/// Renew freshness after a probe answered "unchanged": fresh marker gets a
/// new full TTL, the body TTL is re-anchored to the Stale Window. The
/// generation timestamp is deliberately not touched, so the Fresh Period
/// still forces a full conversion eventually.
pub async fn renew_freshness(
    con: &mut redis::aio::MultiplexedConnection,
    canonical_id: &str,
    fresh_ttl_secs: u64,
    stale_window_secs: u64,
) -> eyre::Result<()> {
    set_fresh_marker(con, canonical_id, fresh_ttl_secs).await?;
    redis::cmd("EXPIRE")
        .arg(feed_key(canonical_id))
        .arg(stale_window_secs)
        .query_async::<()>(con)
        .await?;
    Ok(())
}

/// Reset epoch (unix seconds) the quota breaker identified by `breaker_key`
/// is open until, if it is open. Each provider with a metered quota has its
/// own breaker.
pub async fn quota_breaker_open(
    con: &mut redis::aio::MultiplexedConnection,
    breaker_key: &str,
) -> Option<i64> {
    let reset: Option<i64> = redis::cmd("GET")
        .arg(breaker_key)
        .query_async(con)
        .await
        .unwrap_or_default();
    reset.filter(|reset| *reset > unix_now())
}

/// Open the quota breaker identified by `breaker_key` until
/// `reset_epoch_secs`. No-op when the reset is in the past.
pub async fn open_quota_breaker(
    con: &mut redis::aio::MultiplexedConnection,
    breaker_key: &str,
    reset_epoch_secs: i64,
) -> eyre::Result<()> {
    let now = unix_now();
    if reset_epoch_secs <= now {
        return Ok(());
    }
    redis::cmd("SET")
        .arg(breaker_key)
        .arg(reset_epoch_secs)
        .arg("EX")
        .arg(reset_epoch_secs - now)
        .query_async::<()>(con)
        .await?;
    Ok(())
}

/// Add `units` to the quota usage estimate bucket `day_key` (the
/// provider-owned day this usage is counted on) and return the running
/// total. Best effort: a redis failure only loses the estimate, never the
/// feed.
pub async fn bump_quota_usage(
    con: &mut redis::aio::MultiplexedConnection,
    day_key: &str,
    units: u64,
) -> eyre::Result<i64> {
    let key = format!("{REDIS_QUOTA_USAGE_PREFIX}{day_key}");
    let total: i64 = redis::cmd("INCRBY")
        .arg(&key)
        .arg(units as i64)
        .query_async(con)
        .await?;
    // keep the counter around for 48h so the running total survives a bit of
    // clock skew around the usage-day boundary
    redis::cmd("EXPIRE")
        .arg(&key)
        .arg(48 * 3600)
        .query_async::<()>(con)
        .await?;
    Ok(total)
}
