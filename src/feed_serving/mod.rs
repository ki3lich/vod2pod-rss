//! The Feed Serving module: the single seam for serving a Feed. It owns the
//! served rungs of the Degradation Ladder (docs/adr/0001) — Fresh hit, Stale
//! hit, Feed Conversion — plus the serving policy numbers, Canonical Feed Key
//! derivation and stale-while-revalidate coalescing. Storage sits behind the
//! [`FeedCacheStore`] adapter (Redis in production, in-memory in tests) and
//! time behind the [`Clock`] adapter (wall clock in production, fixed clock
//! in tests), so the served rungs are exercised hermetically at this
//! interface (the adapter-owned tail is driven through fakes until it moves
//! in).
//!
//! The module reads no environment configuration: the HTTP adapter reads the
//! config and injects a [`ServingPolicy`]. The ladder tail (Quota Breaker
//! preemption and the Degraded Feed rung) still lives in the HTTP adapter and
//! is injected as [`DownstreamRungs`] — ceiling: tail outside the module,
//! carve it in per issue #157 (background revalidation likewise, #156).

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use log::{debug, info, warn};
use tokio::sync::{Mutex, OwnedMutexGuard};
use url::Url;

use crate::{feed_cache::CachedFeed, provider::MediaProvider};

/// Seconds of jitter added on top of the Fresh TTL so that many feeds polled
/// by the same podcatcher do not all expire (and regenerate) at the same
/// time. The HTTP adapter injects it into the policy.
pub const FRESH_TTL_JITTER_SECS: u64 = 300;

/// Serving policy numbers, injected at construction. Defaults live in
/// docs/adr/0001; the values come from the environment read by the HTTP
/// adapter, never by this module.
#[derive(Clone, Copy, Debug)]
pub struct ServingPolicy {
    /// How long a stored Feed counts as Fresh (the fresh marker TTL).
    pub fresh_ttl_secs: u64,
    /// How long a stored Feed may keep being served after it stopped being
    /// Fresh (its storage expiry). Serving within the Stale Window is always
    /// preferred over failing.
    pub stale_window_secs: u64,
    /// Upper bound on the age a cached Feed may reach while its fresh marker
    /// still lives; past it the copy is served Stale even when the marker is
    /// alive, because probes may not renew freshness forever.
    pub max_fresh_period_secs: u64,
    /// Max-Age sent when serving a Stale copy: tell the client to come back
    /// soon, the background revalidation is likely done by then.
    pub stale_serve_max_age_secs: u64,
    /// Jitter added on top of the Fresh TTL (see [`FRESH_TTL_JITTER_SECS`]).
    pub fresh_ttl_jitter_secs: u64,
}

/// Source of time for serving decisions. The wall clock in production, a
/// fixed clock in tests so every window is deterministic.
pub trait Clock: Send + Sync {
    /// Current unix time in seconds.
    fn unix_now(&self) -> i64;
    /// Sub-second nanos of the current instant; feeds the Fresh TTL jitter so
    /// many feeds do not all expire (and regenerate) at the same moment.
    fn subsec_nanos(&self) -> u32;
}

/// Production clock: the system wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct WallClock;

impl Clock for WallClock {
    fn unix_now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn subsec_nanos(&self) -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    }
}

/// Storage of cached Feeds, behind which the byte layout hides: the module
/// owns freshness policy, the adapter owns storage mechanics. The Redis
/// adapter runs in production (current data layout), the in-memory adapter
/// in tests.
#[async_trait]
pub trait FeedCacheStore: Send + Sync {
    /// The cached Feed for a Canonical Feed Key, when one is stored and still
    /// inside the Stale Window. A storage failure behaves like a miss: the
    /// ladder keeps serving clients.
    async fn load(&self, canonical_key: &str) -> Option<CachedFeed>;

    /// Remaining seconds of the fresh marker, or `None` when it is gone (or
    /// its TTL could not be read).
    async fn fresh_marker_ttl(&self, canonical_key: &str) -> Option<i64>;

    /// Store a Feed (overwriting any previous one) and anchor its freshness:
    /// the fresh marker gets `fresh_ttl_secs`, the body lives
    /// `stale_window_secs`.
    async fn store(
        &self,
        canonical_key: &str,
        feed: &CachedFeed,
        fresh_ttl_secs: u64,
        stale_window_secs: u64,
    ) -> eyre::Result<()>;

    /// Record metered units a Feed Conversion consumed (best-effort usage
    /// estimate; a failure only loses the estimate, never the feed).
    async fn bump_quota_usage(&self, units: u64) -> eyre::Result<i64>;
}

/// The tail of the Degradation Ladder not yet owned by this module: Quota
/// Breaker preemption and the Degraded Feed rung. Injected at construction;
/// the module calls it at exactly the two ladder points where those rungs
/// apply, so the rung order of docs/adr/0001 is unchanged.
/// ceiling: adapter-owned tail — carve into this module per issue #157,
/// which also deletes this trait.
#[async_trait]
pub trait DownstreamRungs: Send + Sync {
    /// Called on a miss, after the coalescing re-read, before any metered
    /// conversion. `Some(feed)`: a downstream rung produced it (e.g. the
    /// breaker is open and a Degraded Feed applies) — serve it. `None`: no
    /// downstream rung applies, proceed with the Feed Conversion.
    async fn before_conversion(
        &self,
        source_url: &Url,
        provider: &dyn MediaProvider,
        canonical_key: &str,
    ) -> Option<ServedFeed>;

    /// Called when the Feed Conversion failed. `Some(feed)`: the degrade rung
    /// produced it — serve it. `None`: the ladder is exhausted, the request
    /// errors.
    async fn on_conversion_failed(
        &self,
        source_url: &Url,
        provider: &dyn MediaProvider,
        canonical_key: &str,
        err: eyre::Error,
    ) -> Option<ServedFeed>;
}

/// Which ladder rung produced the served Feed: the metadata the HTTP side
/// needs beside the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Cached copy still inside the Fresh TTL and the Fresh Period: served
    /// as-is, nothing upstream was contacted.
    Fresh,
    /// Cached copy past freshness but inside the Stale Window: served as-is
    /// while the caller revalidates in the background.
    Stale,
    /// Produced during this request (Feed Conversion or a downstream rung).
    FreshlyGenerated,
}

/// A Feed ready to serve: the raw body (before per-request customization,
/// which depends on the requesting host and is redone on every serve) plus
/// the response metadata the HTTP adapter needs.
#[derive(Clone, Debug)]
pub struct ServedFeed {
    pub body: String,
    /// When the body was produced (unix seconds).
    pub generated_at: i64,
    /// `true` when the body came from the quota-free rung (fewer items than a
    /// full conversion); such bodies are never renewed by a probe, they
    /// expire normally and get replaced by a full feed.
    pub degraded: bool,
    pub freshness: Freshness,
    /// Cache-Control max-age the adapter should send.
    pub max_age_secs: i64,
    /// The Canonical Feed Key this Feed is cached under.
    pub canonical_key: String,
}

/// Freshness classification of a cache entry. A copy counts as
/// [`Freshness::Fresh`] only while the fresh marker lives AND the copy is
/// younger than the Fresh Period (the bound on how long probes may keep
/// renewing it). Beyond that, but inside the Stale Window, it is served
/// stale. `None` when no usable copy exists (nothing stored, or the Stale
/// Window has passed): a Feed Conversion is needed. Pure function so tests
/// can exercise it.
pub fn classify_freshness(
    has_body: bool,
    fresh_marker: bool,
    generated_at: i64,
    now: i64,
    max_fresh_period_secs: i64,
    stale_window_secs: i64,
) -> Option<Freshness> {
    if !has_body {
        return None;
    }
    let age = now - generated_at;
    if fresh_marker && age <= max_fresh_period_secs {
        Some(Freshness::Fresh)
    } else if age < stale_window_secs {
        Some(Freshness::Stale)
    } else {
        None
    }
}

/// Server-layer normalization used as Canonical Feed Key when the provider
/// cannot give a better identity: lowercase host without `www.`/`m.`
/// prefixes, default ports elided, query pairs sorted, fragment and trailing
/// slash dropped. Two Source URLs of the same feed must normalize to the same
/// string.
pub fn canonicalize_url(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let host = host.strip_prefix("m.").unwrap_or(host);

    let port = match url.port() {
        Some(port) => format!(":{port}"),
        None => String::new(),
    };

    let path = url.path();
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };

    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    pairs.sort();
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    if query.is_empty() {
        format!("{}://{}{}{}", url.scheme(), host, port, path)
    } else {
        format!("{}://{}{}{}?{}", url.scheme(), host, port, path, query)
    }
}

/// The single cache identity for the Feed behind `source_url`: the resolved
/// provider's identity when it can determine one cheaply (without a full
/// Feed Conversion), the normalized Source URL otherwise. Two Source URLs of
/// the same feed must yield the same key so they share one cache entry,
/// freshness and quota cost.
pub async fn canonical_feed_key(source_url: &Url, provider: &dyn MediaProvider) -> String {
    provider
        .canonical_feed_id(source_url)
        .await
        .unwrap_or_else(|| canonicalize_url(source_url))
}

/// One regeneration slot per Canonical Feed Key, process-local. Concurrent
/// requests for the same feed share one upstream pass instead of stampeding
/// the provider API.
static REGENERATION_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Above this many tracked feeds the idle locks are dropped: they only exist
/// to coalesce in-flight conversions, and live holders keep their Arc alive
/// through the coalescing window. Worst case after a clear is a duplicated
/// upstream pass per feed, which the cache absorbs.
const MAX_TRACKED_REGENERATION_LOCKS: usize = 4096;

async fn regeneration_lock(canonical_key: &str) -> Arc<Mutex<()>> {
    let mut locks = REGENERATION_LOCKS.lock().await;
    if locks.len() > MAX_TRACKED_REGENERATION_LOCKS {
        locks.clear();
    }
    locks.entry(canonical_key.to_string()).or_default().clone()
}

/// Wait for any in-flight regeneration of this feed, then hold its slot.
/// Callers MUST re-read the cache after acquiring: the previous holder may
/// have refreshed the entry in the meantime.
pub async fn begin_regeneration(canonical_key: &str) -> OwnedMutexGuard<()> {
    regeneration_lock(canonical_key).await.lock_owned().await
}

/// Non-blocking variant for background revalidation: `None` means another
/// regeneration is already running for this feed, so there is nothing to do.
pub async fn try_begin_regeneration(canonical_key: &str) -> Option<OwnedMutexGuard<()>> {
    regeneration_lock(canonical_key).await.try_lock_owned().ok()
}

/// The Feed Serving module. Constructed with an injected policy, clock and
/// downstream rungs; the cache adapter is supplied per serve call, so the
/// HTTP adapter keeps its connection handling.
pub struct FeedServing {
    policy: ServingPolicy,
    clock: Arc<dyn Clock>,
    downstream: Box<dyn DownstreamRungs>,
}

impl FeedServing {
    pub fn new(
        policy: ServingPolicy,
        clock: Arc<dyn Clock>,
        downstream: Box<dyn DownstreamRungs>,
    ) -> Self {
        Self {
            policy,
            clock,
            downstream,
        }
    }

    /// Run the Degradation Ladder for a Source URL and return the Feed to
    /// serve (docs/adr/0001): a Fresh copy is served as-is; a Stale copy is
    /// served within the Stale Window; a miss performs a coalesced Feed
    /// Conversion, stores the result and serves it. The remaining rungs
    /// (Quota Breaker preemption, Degraded Feed) come from the injected
    /// downstream rungs until they are carved into this module. An error
    /// means the ladder is exhausted: the adapter answers the client with an
    /// error.
    pub async fn serve(
        &self,
        cache: &dyn FeedCacheStore,
        source_url: &Url,
        provider: &dyn MediaProvider,
    ) -> eyre::Result<ServedFeed> {
        let canonical_key = canonical_feed_key(source_url, provider).await;
        debug!("canonical feed key for {source_url}: {canonical_key}");

        // rung 0/1: a fresh copy is served without contacting the provider at
        // all; a stale copy is served while the feed is revalidated in the
        // background (stale-while-revalidate)
        if let Some(served) = self.serve_from_cache(cache, &canonical_key).await {
            return Ok(served);
        }

        // nothing usable cached: requests for the same feed are serialized so
        // a poll burst shares one upstream pass; the cache is re-checked after
        // waiting because the previous slot holder may have refreshed it
        // already.
        let _regen_slot = begin_regeneration(&canonical_key).await;
        if let Some(served) = self.serve_from_cache(cache, &canonical_key).await {
            info!(
                "re-checked cache after waiting for a concurrent regeneration of {canonical_key}"
            );
            return Ok(served);
        }

        // the ladder tail (Quota Breaker preemption) gets the first shot at a
        // miss, before any metered call
        if let Some(served) = self
            .downstream
            .before_conversion(source_url, provider, &canonical_key)
            .await
        {
            return Ok(served);
        }

        // rung 2: Feed Conversion
        match self
            .convert_and_store(cache, &canonical_key, source_url, provider)
            .await
        {
            Ok(served) => Ok(served),
            Err(e) => {
                // the remaining rungs apply on any conversion failure (quota
                // exhaustion included); the fallback itself failing surfaces
                // the error to the client
                self.downstream
                    .on_conversion_failed(source_url, provider, &canonical_key, e)
                    .await
                    .ok_or_else(|| eyre::eyre!("degradation ladder exhausted for {source_url}"))
            }
        }
    }

    /// Read the cache and classify the entry. `Some` = a usable copy: serve
    /// it. `None` = miss, a Feed Conversion is needed.
    async fn serve_from_cache(
        &self,
        cache: &dyn FeedCacheStore,
        canonical_key: &str,
    ) -> Option<ServedFeed> {
        let entry = cache.load(canonical_key).await;
        let fresh_marker_ttl = cache.fresh_marker_ttl(canonical_key).await;
        let freshness = classify_freshness(
            entry.is_some(),
            fresh_marker_ttl.is_some(),
            entry.as_ref().map(|entry| entry.generated_at).unwrap_or(0),
            self.clock.unix_now(),
            self.policy.max_fresh_period_secs as i64,
            self.policy.stale_window_secs as i64,
        )?;
        // classification only returns a hit when the entry exists
        let CachedFeed {
            body,
            generated_at,
            probe_state: _,
            degraded,
        } = entry?;
        Some(ServedFeed {
            body,
            generated_at,
            degraded,
            max_age_secs: match freshness {
                Freshness::Fresh => fresh_marker_ttl.unwrap_or(self.policy.fresh_ttl_secs as i64),
                Freshness::Stale => self.policy.stale_serve_max_age_secs as i64,
                // classify_freshness never produces this for a cached copy;
                // treat it as a miss rather than panic in the serving path
                Freshness::FreshlyGenerated => return None,
            },
            freshness,
            canonical_key: canonical_key.to_string(),
        })
    }

    /// Rung 2: full Feed Conversion, then store the raw body together with the
    /// Freshness Probe state it was generated from. The stored copy is
    /// re-read so the response metadata reflects what was just cached; when
    /// the cache is unavailable the generated body is served anyway (losing
    /// the cache entry only costs quota).
    ///
    /// Public so background revalidation can invoke the conversion rung
    /// without running the whole ladder — ceiling: revalidation lives in the
    /// HTTP adapter until it joins this module (issue #156).
    pub async fn convert_and_store(
        &self,
        cache: &dyn FeedCacheStore,
        canonical_key: &str,
        source_url: &Url,
        provider: &dyn MediaProvider,
    ) -> eyre::Result<ServedFeed> {
        let start = Instant::now();
        let generated = provider.generate_rss_feed(source_url.clone()).await?;

        let entry = CachedFeed {
            body: generated.body,
            generated_at: self.clock.unix_now(),
            probe_state: generated.probe_state,
            degraded: false,
        };
        if let Err(e) = cache
            .store(
                canonical_key,
                &entry,
                self.jittered_fresh_ttl(),
                self.policy.stale_window_secs,
            )
            .await
        {
            warn!("could not store feed {canonical_key} in cache: {e}");
        }

        if let Some(units) = generated.quota_units {
            match cache.bump_quota_usage(units).await {
                Ok(total) => info!("quota: ~{total} units consumed today (estimate)"),
                Err(e) => warn!("could not record quota usage estimate: {e}"),
            }
        }
        debug!(
            "feed conversion for {canonical_key} took {} seconds",
            start.elapsed().as_secs_f32()
        );

        // prefer the cache read so the fresh marker (and its max-age)
        // reflects what was just stored; fall back to the generated body
        // when the cache read fails
        let fresh_marker_ttl = cache.fresh_marker_ttl(canonical_key).await;
        let (body, generated_at) = match cache.load(canonical_key).await {
            Some(cached) => (cached.body, cached.generated_at),
            None => (entry.body, entry.generated_at),
        };
        Ok(ServedFeed {
            body,
            generated_at,
            degraded: false,
            freshness: Freshness::FreshlyGenerated,
            max_age_secs: fresh_marker_ttl.unwrap_or(self.policy.fresh_ttl_secs as i64),
            canonical_key: canonical_key.to_string(),
        })
    }

    /// Fresh TTL with the shared-no-expiry-moment jitter applied.
    fn jittered_fresh_ttl(&self) -> u64 {
        let base = self.policy.fresh_ttl_secs;
        if self.policy.fresh_ttl_jitter_secs == 0 {
            return base;
        }
        base + u64::from(self.clock.subsec_nanos()) % self.policy.fresh_ttl_jitter_secs
    }
}

#[cfg(test)]
mod memory;
mod redis_cache;

pub use redis_cache::RedisFeedCache;

#[cfg(test)]
mod tests;
