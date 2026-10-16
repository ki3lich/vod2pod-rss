//! Redis storage of cached Feeds: the record layout, the fresh markers and
//! the quota breaker. The serving policy and the Degradation Ladder live in
//! the Feed Serving module (`feed_serving`); this module is the storage
//! mechanics its Redis adapter delegates to, plus the client-facing ETag and
//! HTTP-date helpers the web tier uses.
//!
//! Domain terms live in CONTEXT.md; the rationale of the serving policy is in
//! docs/adr/0001-serve-stale-and-degrade-instead-of-failing.md.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use log::warn;

use crate::provider::FeedProbeState;

pub const REDIS_FEED_PREFIX: &str = "vod2pod:feed:";
pub const REDIS_FRESH_PREFIX: &str = "vod2pod:fresh:";
pub const REDIS_QUOTA_BREAKER_PREFIX: &str = "vod2pod:quota_breaker:";
/// Quota breaker key of the YouTube provider (the only one with a metered
/// quota today).
pub const YT_QUOTA_BREAKER_KEY: &str = "vod2pod:quota_breaker:youtube";
pub const REDIS_QUOTA_USAGE_PREFIX: &str = "vod2pod:quota_used:";

pub fn feed_key(canonical_id: &str) -> String {
    format!("{REDIS_FEED_PREFIX}{canonical_id}")
}

pub fn fresh_key(canonical_id: &str) -> String {
    format!("{REDIS_FRESH_PREFIX}{canonical_id}")
}

/// FNV-1a 64 bit: tiny and stable across processes and releases. Only used
/// for change detection (ETags), never for anything security related.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// ETag of a feed body: unquoted hex of the FNV-1a 64 hash.
pub fn etag_of(body: &str) -> String {
    format!("{:016x}", fnv1a64(body.as_bytes()))
}

/// RFC 7232 `If-None-Match` matching against our unquoted-hex etag. Handles
/// quoted values, weak validators (`W/`), comma separated lists and `*`.
pub fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    if_none_match.split(',').any(|candidate| {
        let candidate = candidate.trim();
        if candidate == "*" {
            return true;
        }
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate.trim_matches('"') == etag
    })
}

/// The moment YouTube's daily quota resets: midnight America/Los_Angeles.
/// Pure function so tests can exercise it.
pub fn next_quota_reset(now_utc: DateTime<Utc>) -> DateTime<Utc> {
    let pt_now = now_utc.with_timezone(&Los_Angeles);
    let next_day = pt_now.date_naive() + chrono::Duration::days(1);
    let midnight = next_day
        .and_hms_opt(0, 0, 0)
        .expect("midnight of any date exists");
    match Los_Angeles.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.with_timezone(&Utc)
        }
        // midnight is never skipped by US DST transitions; if that ever
        // changes, fall back to a conservative fixed PST offset (resets early
        // rather than late)
        chrono::LocalResult::None => {
            let approx = midnight - chrono::Duration::hours(8);
            DateTime::<Utc>::from_naive_utc_and_offset(approx, Utc)
        }
    }
}

/// Whether the upstream error is YouTube quota exhaustion. Pure function so
/// tests can exercise it.
pub fn is_quota_exceeded(err: &eyre::Error) -> bool {
    format!("{err:#}").contains("quotaExceeded")
}

/// RFC 7231 IMF-fixdate ("Sun, 21 Sep 2026 04:27:35 GMT") for a unix
/// timestamp, used in `Last-Modified` responses. Pure function so tests can
/// exercise it.
pub fn http_date(epoch_secs: i64) -> String {
    match DateTime::<Utc>::from_timestamp(epoch_secs, 0) {
        Some(dt) => dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        None => String::new(),
    }
}

/// Whether an `If-Modified-Since` HTTP-date is at or after `epoch_secs`
/// (meaning: the client already has this version, a 304 is appropriate).
/// Unparseable dates are treated as "client has nothing" (serve 200).
/// Pure function so tests can exercise it.
pub fn http_date_at_or_after(if_modified_since: &str, epoch_secs: i64) -> bool {
    // RFC 7231 IMF-fixdate always ends in a literal " GMT", which is UTC
    match chrono::NaiveDateTime::parse_from_str(if_modified_since, "%a, %d %b %Y %H:%M:%S GMT") {
        Ok(parsed) => parsed.and_utc().timestamp() >= epoch_secs,
        Err(_) => false,
    }
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

/// Add `units` to the daily quota usage estimate (keyed by pacific-time date,
/// like YouTube's reset) and return the running total. Best effort: a redis
/// failure only loses the estimate, never the feed.
pub async fn bump_quota_usage(
    con: &mut redis::aio::MultiplexedConnection,
    units: u64,
) -> eyre::Result<i64> {
    let day = Utc::now().with_timezone(&Los_Angeles).format("%F");
    let key = format!("{REDIS_QUOTA_USAGE_PREFIX}{day}");
    let total: i64 = redis::cmd("INCRBY")
        .arg(&key)
        .arg(units as i64)
        .query_async(con)
        .await?;
    // keep the counter around for 48h so the running total survives a bit of
    // clock skew around midnight pacific
    redis::cmd("EXPIRE")
        .arg(&key)
        .arg(48 * 3600)
        .query_async::<()>(con)
        .await?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    #[test]
    fn test_etag_of_is_deterministic_and_change_sensitive() {
        let a = etag_of("<rss>feed v1</rss>");
        assert_eq!(a, etag_of("<rss>feed v1</rss>"));
        assert_ne!(a, etag_of("<rss>feed v2</rss>"));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn test_etag_matches_rfc7232_forms() {
        let etag = etag_of("body");
        assert!(etag_matches(&format!("\"{etag}\""), &etag));
        assert!(etag_matches(&etag, &etag));
        assert!(etag_matches(&format!("W/\"{etag}\""), &etag));
        assert!(etag_matches(&format!("\"other\", \"{etag}\""), &etag));
        assert!(etag_matches(&format!("\"other\" ,\"{etag}\" "), &etag));
        assert!(etag_matches("*", &etag));
        assert!(!etag_matches("\"other\"", &etag));
        assert!(!etag_matches("", &etag));
    }

    #[test]
    fn test_next_quota_reset_is_next_pacific_midnight() {
        // 2026-09-21T04:27Z is 2026-09-20 21:27 PDT (UTC-7): reset is
        // 2026-09-21 00:00 PDT == 07:00Z
        let now = DateTime::parse_from_rfc3339("2026-09-21T04:27:35Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-09-21T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(next_quota_reset(now), expected);
    }

    #[test]
    fn test_next_quota_reset_during_pst_winter() {
        // 2026-01-15T08:00Z is exactly midnight PST (UTC-8): reset is the
        // NEXT midnight, 2026-01-16 08:00Z
        let now = DateTime::parse_from_rfc3339("2026-01-15T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-01-16T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(next_quota_reset(now), expected);
    }

    #[test]
    fn test_next_quota_reset_day_before_dst_start() {
        // DST starts 2026-03-08 at 02:00; midnight of 2026-03-08 is still PST
        let now = DateTime::parse_from_rfc3339("2026-03-07T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-03-08T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(next_quota_reset(now), expected);
    }

    #[test]
    fn test_is_quota_exceeded_matches_real_error_shape() {
        let err = eyre::eyre!(
            "Bad Request: {{\"error\":{{\"code\":403,\"errors\":[{{\"domain\":\"youtube.quota\",\"reason\":\"quotaExceeded\"}}]}}}}"
        );
        assert!(is_quota_exceeded(&err));

        let wrapped = eyre::eyre!("Bad Request: quotaExceeded").wrap_err("could not generate feed");
        assert!(is_quota_exceeded(&wrapped));

        let other = eyre::eyre!("playlist not found");
        assert!(!is_quota_exceeded(&other));
    }

    #[test]
    fn test_http_date_roundtrip_and_comparison() {
        let date = http_date(1_789_964_855);
        assert_eq!(date, "Mon, 21 Sep 2026 04:27:35 GMT");
        // a client whose copy is as new as ours gets a 304
        assert!(http_date_at_or_after(&date, 1_789_964_855));
        // a client with an older copy must get a 200
        assert!(!http_date_at_or_after(&date, 1_789_964_856));
        assert!(http_date_at_or_after(&date, 1_789_964_854));
    }

    #[test]
    fn test_http_date_at_or_after_rejects_garbage() {
        assert!(!http_date_at_or_after("not a date", 0));
        assert!(!http_date_at_or_after("", 0));
    }
}
