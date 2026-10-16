//! The Feed Serving module: the single seam for serving a Feed. It owns the
//! whole Degradation Ladder (docs/adr/0001) — Fresh hit, Stale hit
//! (+ background revalidation), Feed Conversion, Quota Breaker preemption,
//! Degraded Feed, error — plus the serving policy numbers, Canonical Feed Key
//! derivation and stale-while-revalidate coalescing. Storage sits behind the
//! [`FeedCacheStore`] adapter (Redis in production, in-memory in tests), time
//! behind the [`Clock`] adapter (wall clock in production, fixed clock in
//! tests) and background scheduling behind the [`RevalidationSpawner`]
//! adapter (actix runtime in production, recording spawner in tests), so
//! every rung is exercised hermetically at this interface.
//!
//! The module reads no environment configuration: the HTTP adapter reads the
//! config and injects a [`ServingPolicy`]. Quota exhaustion is still detected
//! by the inherited string match (the reset rule alongside it) — both are
//! retyped across the provider seam by the next ticket.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use tokio::sync::{Mutex, OwnedMutexGuard};
use url::Url;

use crate::{
    feed_cache::{is_quota_exceeded, next_quota_reset, CachedFeed},
    provider::MediaProvider,
};

/// Seconds of jitter added on top of the Fresh TTL so that many feeds polled
/// by the same podcatcher do not all expire (and regenerate) at the same
/// time. The HTTP adapter injects it into the policy.
pub const FRESH_TTL_JITTER_SECS: u64 = 300;

/// Fresh TTL given to a Degraded Feed: short on purpose, so the next
/// revalidation replaces it with a full feed as soon as possible (e.g. right
/// after the quota reset) instead of serving the limited feed for a full
/// hour. The HTTP adapter injects it into the policy.
pub const DEGRADED_FRESH_TTL_SECS: u64 = 300;

/// Serving policy numbers, injected at construction. Defaults live in
/// docs/adr/0001; the values come from the environment (or from this
/// module's fixed constants, e.g. the degraded Fresh TTL) read by the HTTP
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
    /// Fresh TTL given to a Degraded Feed: short on purpose (see
    /// [`DEGRADED_FRESH_TTL_SECS`] for why).
    pub degraded_fresh_ttl_secs: u64,
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

/// A background revalidation task handed to the spawner adapter.
pub type RevalidationTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Schedules the module's background revalidation outside the serving
/// request. The actix runtime in production; a recording spawner in tests, so
/// revalidation requests — the Canonical Feed Key they target included — and
/// their effects are observable hermetically.
pub trait RevalidationSpawner: Send + Sync {
    /// Schedule the revalidation of the Feed cached under `canonical_key`.
    fn spawn(&self, canonical_key: &str, task: RevalidationTask);
}

/// Production spawner: the actix runtime the server runs on.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActixSpawner;

impl RevalidationSpawner for ActixSpawner {
    fn spawn(&self, _canonical_key: &str, task: RevalidationTask) {
        actix_rt::spawn(task);
    }
}

/// Storage behind the serving ladder: cached Feeds and the shared Quota
/// Breaker flag. The module owns freshness and quota policy, the adapter owns
/// storage mechanics. The Redis adapter runs in production (current data
/// layout), the in-memory adapter in tests.
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

    /// Re-anchor freshness after a probe answered "unchanged": the fresh
    /// marker gets `fresh_ttl_secs` (which must not outlive the Fresh
    /// Period), the body TTL is re-anchored to the Stale Window. The
    /// generation timestamp is deliberately not touched, so the Fresh Period
    /// still forces a full conversion eventually.
    async fn renew_freshness(
        &self,
        canonical_key: &str,
        fresh_ttl_secs: u64,
        stale_window_secs: u64,
    ) -> eyre::Result<()>;

    /// The Quota Breaker identified by `breaker_key` while it is open —
    /// metered calls to the provider owning the key must not be made —
    /// carrying the unix time it holds until. `None` when it is closed (or
    /// its state could not be read: a closed breaker only ever costs quota).
    async fn quota_breaker_open(&self, breaker_key: &str) -> Option<i64>;

    /// Open the Quota Breaker identified by `breaker_key` until
    /// `reset_epoch_secs`, so no request pays the failing-call cost until
    /// then. Persistence is best-effort: a failure only delays the
    /// preemption, the ladder keeps serving clients either way.
    async fn open_quota_breaker(
        &self,
        breaker_key: &str,
        reset_epoch_secs: i64,
    ) -> eyre::Result<()>;
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
async fn begin_regeneration(canonical_key: &str) -> OwnedMutexGuard<()> {
    regeneration_lock(canonical_key).await.lock_owned().await
}

/// Non-blocking variant for the module's background revalidation: `None`
/// means another regeneration is already running for this feed, so there is
/// nothing to do.
async fn try_begin_regeneration(canonical_key: &str) -> Option<OwnedMutexGuard<()>> {
    regeneration_lock(canonical_key).await.try_lock_owned().ok()
}

/// The Feed Serving module. Constructed with an injected policy, clock and
/// spawner; the cache adapter is supplied per serve call, so the HTTP adapter
/// keeps its connection handling. Cloning is cheap: the adapters are shared
/// through `Arc`s, which is how the background revalidation future owns the
/// module logic it runs.
#[derive(Clone)]
pub struct FeedServing {
    policy: ServingPolicy,
    clock: Arc<dyn Clock>,
    /// Schedules the module's background revalidation outside the serving
    /// request: the actix runtime in production, a recording spawner in
    /// tests.
    spawner: Arc<dyn RevalidationSpawner>,
}

impl FeedServing {
    pub fn new(
        policy: ServingPolicy,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn RevalidationSpawner>,
    ) -> Self {
        Self {
            policy,
            clock,
            spawner,
        }
    }

    /// Run the Degradation Ladder for a Source URL and return the Feed to
    /// serve (docs/adr/0001): a Fresh copy is served as-is; a Stale copy is
    /// served within the Stale Window while its background revalidation is
    /// scheduled through the spawner adapter (stale-while-revalidate); a miss
    /// first consults the Quota Breaker — while it is open no metered call is
    /// attempted at all and the Degraded Feed rung applies right away — then
    /// performs a coalesced Feed Conversion, stores the result and serves it;
    /// a failed conversion opens the breaker on quota exhaustion and falls
    /// through to the Degraded Feed rung. An error means the ladder is
    /// exhausted: the adapter answers the client with an error.
    ///
    /// The cache and provider are `Arc`s because the scheduled revalidation
    /// keeps working with them after this request returns.
    pub async fn serve(
        &self,
        cache: Arc<dyn FeedCacheStore>,
        source_url: &Url,
        provider: Arc<dyn MediaProvider>,
    ) -> eyre::Result<ServedFeed> {
        let canonical_key = canonical_feed_key(source_url, provider.as_ref()).await;
        debug!("canonical feed key for {source_url}: {canonical_key}");

        // rung 0/1: a fresh copy is served without contacting the provider at
        // all; a stale copy is served while the feed is revalidated in the
        // background (stale-while-revalidate)
        if let Some(served) = self.serve_from_cache(&*cache, &canonical_key).await {
            self.schedule_revalidation(
                served.freshness,
                &cache,
                &canonical_key,
                source_url,
                &provider,
            );
            return Ok(served);
        }

        // nothing usable cached: requests for the same feed are serialized so
        // a poll burst shares one upstream pass; the cache is re-checked after
        // waiting because the previous slot holder may have refreshed it
        // already.
        let _regen_slot = begin_regeneration(&canonical_key).await;
        if let Some(served) = self.serve_from_cache(&*cache, &canonical_key).await {
            info!(
                "re-checked cache after waiting for a concurrent regeneration of {canonical_key}"
            );
            self.schedule_revalidation(
                served.freshness,
                &cache,
                &canonical_key,
                source_url,
                &provider,
            );
            return Ok(served);
        }

        // the Quota Breaker preempts when THIS provider's quota is known to
        // be exhausted: metered calls are skipped entirely and the ladder
        // degrades right away
        if let Some(reset) = self.breaker_open(&*cache, provider.as_ref()).await {
            info!(
                "quota breaker open until {}, degrading to quota-free generation for {source_url}",
                DateTime::<Utc>::from_timestamp(reset, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            );
            return self
                .degrade(&*cache, provider.as_ref(), source_url, &canonical_key)
                .await;
        }

        // rung 2: Feed Conversion
        match self
            .convert_and_store(&*cache, &canonical_key, source_url, provider.as_ref())
            .await
        {
            Ok(served) => Ok(served),
            Err(e) => {
                // the Degraded Feed rung applies on any conversion failure —
                // quota exhaustion included, opening the breaker so no further
                // request pays the failing-call cost; the fallback itself
                // failing is the ladder's end: the request errors rather than
                // serving a silent empty feed
                if self
                    .open_breaker_if_quota_exhausted(&*cache, provider.as_ref(), &e)
                    .await
                {
                    error!("provider quota exhausted for {source_url}:\n{e}");
                } else {
                    error!("could not generate rss feed for {source_url}:\n{e}");
                }
                self.degrade(&*cache, provider.as_ref(), source_url, &canonical_key)
                    .await
            }
        }
    }

    /// A Stale hit serves immediately and schedules its background
    /// revalidation through the spawner adapter: exactly one revalidation
    /// request per stale serve (coalescing happens inside the revalidation
    /// itself). The revalidation future owns a cheap clone of this module, so
    /// it runs the same ladder logic after the request is gone.
    fn schedule_revalidation(
        &self,
        freshness: Freshness,
        cache: &Arc<dyn FeedCacheStore>,
        canonical_key: &str,
        source_url: &Url,
        provider: &Arc<dyn MediaProvider>,
    ) {
        if freshness != Freshness::Stale {
            return;
        }
        info!("serving stale feed {canonical_key}, scheduling background revalidation");
        self.spawner.spawn(
            canonical_key,
            Box::pin(self.clone().revalidate(
                cache.clone(),
                canonical_key.to_string(),
                source_url.clone(),
                provider.clone(),
            )),
        );
    }

    /// The background half of stale-while-revalidate (the ladder's
    /// revalidation rung): probe the cached copy while it is still probeable
    /// — "unchanged" renews freshness without a Feed Conversion — and re-run
    /// the module's own Feed Conversion otherwise (probe "changed", probe
    /// failure, Fresh Period exhausted, Degraded Feed). Failures keep the
    /// stale body cached so clients keep being served. Runs through the
    /// spawner adapter, outside any request.
    async fn revalidate(
        self,
        cache: Arc<dyn FeedCacheStore>,
        canonical_key: String,
        source_url: Url,
        provider: Arc<dyn MediaProvider>,
    ) {
        // coalescing: another revalidation or conversion of this feed is in
        // flight, there is nothing to do
        let Some(_regen_slot) = try_begin_regeneration(&canonical_key).await else {
            debug!("revalidation of {canonical_key} already in flight, skipping");
            return;
        };

        // metered work (probe and conversion alike) is preempted while the
        // Quota Breaker is open: the stale body keeps serving untouched
        if self
            .breaker_open(&*cache, provider.as_ref())
            .await
            .is_some()
        {
            info!("quota breaker open, skipping background revalidation of {canonical_key}");
            return;
        }

        let Some(cached) = cache.load(&canonical_key).await else {
            return;
        };

        let age = self.clock.unix_now() - cached.generated_at;
        // probes may only renew a feed while it is within the Fresh Period;
        // degraded feeds are never probed, they must be replaced by a real
        // conversion as soon as possible
        let probeable = age < self.policy.max_fresh_period_secs as i64 && !cached.degraded;

        if probeable {
            if let Some(state) = cached.probe_state.clone() {
                match provider.probe_feed_freshness(&source_url, &state).await {
                    Ok(true) => {
                        info!(
                            "feed {canonical_key} unchanged upstream, renewing freshness after probe"
                        );
                        if let Err(e) = cache.bump_quota_usage(1).await {
                            warn!("could not record quota usage estimate: {e}");
                        }
                        // the fresh marker must not outlive the Fresh Period
                        let remaining_fresh = (self.policy.max_fresh_period_secs as i64 - age)
                            .min(self.policy.fresh_ttl_secs as i64);
                        if let Err(e) = cache
                            .renew_freshness(
                                &canonical_key,
                                remaining_fresh.max(1) as u64,
                                self.policy.stale_window_secs,
                            )
                            .await
                        {
                            warn!("could not renew freshness of {canonical_key}: {e}");
                        }
                        return;
                    }
                    Ok(false) => info!("feed {canonical_key} changed upstream, regenerating"),
                    Err(e) => {
                        warn!("freshness probe failed for {canonical_key} ({e}), regenerating")
                    }
                }
            }
        } else if age >= self.policy.max_fresh_period_secs as i64 {
            info!("feed {canonical_key} reached the fresh period limit, regenerating fully");
        }

        if let Err(e) = self
            .convert_and_store(&*cache, &canonical_key, &source_url, provider.as_ref())
            .await
        {
            // the stale body is kept on purpose, it keeps serving clients;
            // a quota exhaustion additionally opens the breaker, so the
            // instance stops attempting metered calls until the provider's
            // reset
            if self
                .open_breaker_if_quota_exhausted(&*cache, provider.as_ref(), &e)
                .await
            {
                warn!("provider quota exhausted during background revalidation of {canonical_key}");
            } else {
                error!("background revalidation of {canonical_key} failed:\n{e}");
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
    /// The single implementation of the conversion rung: both the serving
    /// path and background revalidation go through it.
    async fn convert_and_store(
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

    /// The provider's Quota Breaker state, read through the cache adapter:
    /// `Some(reset-at)` while metered calls to this provider must not be
    /// made. Providers without a breaker key have no metered quota; their
    /// conversions are never preempted.
    async fn breaker_open(
        &self,
        cache: &dyn FeedCacheStore,
        provider: &dyn MediaProvider,
    ) -> Option<i64> {
        match provider.quota_breaker_key().as_deref() {
            Some(key) => cache.quota_breaker_open(key).await,
            None => None,
        }
    }

    /// Open the provider's Quota Breaker (until its next reset — midnight
    /// pacific for YouTube) when the error means the quota is exhausted, so
    /// no request pays the failing-call cost until then. Returns whether the
    /// error was classified as quota exhaustion (the inherited upstream
    /// string match, retyped by the next ticket).
    async fn open_breaker_if_quota_exhausted(
        &self,
        cache: &dyn FeedCacheStore,
        provider: &dyn MediaProvider,
        err: &eyre::Error,
    ) -> bool {
        if !is_quota_exceeded(err) {
            return false;
        }
        if let Some(key) = provider.quota_breaker_key() {
            let reset =
                DateTime::<Utc>::from_timestamp(self.clock.unix_now(), 0).map(next_quota_reset);
            match reset {
                Some(reset) => match cache.open_quota_breaker(&key, reset.timestamp()).await {
                    Ok(()) => warn!("quota breaker {key} open until {}", reset.to_rfc3339()),
                    Err(e) => warn!("could not persist quota breaker: {e}"),
                },
                None => {
                    warn!("could not determine the next quota reset for {key}, breaker not opened")
                }
            }
        }
        true
    }

    /// The Degraded Feed rung: produce a feed without consuming metered
    /// provider quota (for YouTube: the provider's public atom feed) and
    /// store it with its own short Fresh TTL, so the next revalidation
    /// replaces it with a full feed as soon as possible (e.g. right after the
    /// quota reset). An error here is the ladder's end: the request errors
    /// rather than serving a silent empty feed.
    async fn degrade(
        &self,
        cache: &dyn FeedCacheStore,
        provider: &dyn MediaProvider,
        source_url: &Url,
        canonical_key: &str,
    ) -> eyre::Result<ServedFeed> {
        warn!("degrading to quota-free feed generation for {source_url}");
        let body = provider
            .generate_rss_feed_quota_free(source_url.clone())
            .await?;
        let entry = CachedFeed {
            body,
            generated_at: self.clock.unix_now(),
            // the Degraded Feed is never probed; it is replaced by a full
            // conversion as soon as possible
            probe_state: None,
            degraded: true,
        };
        if let Err(e) = cache
            .store(
                canonical_key,
                &entry,
                self.policy.degraded_fresh_ttl_secs,
                self.policy.stale_window_secs,
            )
            .await
        {
            warn!("could not store degraded feed {canonical_key} in cache: {e}");
        }
        Ok(ServedFeed {
            body: entry.body,
            generated_at: entry.generated_at,
            degraded: true,
            freshness: Freshness::FreshlyGenerated,
            max_age_secs: self.policy.degraded_fresh_ttl_secs as i64,
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
