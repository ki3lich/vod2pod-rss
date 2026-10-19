//! Hermetic tests of the Degradation Ladder at the Feed Serving interface:
//! in-memory cache adapter, fixed clock, scripted provider fake — no Redis,
//! no network, no credentials. Every served rung of docs/adr/0001 is
//! asserted here.

use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use url::Url;

use super::{
    CachedFeed, FeedCacheStore, FeedServing, Freshness, FreshnessWindows, RevalidationSpawner,
    RevalidationTask, ServingPolicy,
};
use crate::provider::{FeedProbeState, GeneratedFeed, MediaProvider, QuotaExhausted};

use super::memory::InMemoryCache;
use super::Clock;

/// Clock frozen at construction; `advance_secs` moves it forward so every
/// window is deterministic.
struct FixedClock {
    now_secs: AtomicI64,
    nanos: u32,
}

impl FixedClock {
    fn at(secs: i64) -> Self {
        Self {
            now_secs: AtomicI64::new(secs),
            nanos: 0,
        }
    }

    fn with_nanos(secs: i64, nanos: u32) -> Self {
        Self {
            now_secs: AtomicI64::new(secs),
            nanos,
        }
    }

    fn advance_secs(&self, secs: i64) {
        self.now_secs.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn unix_now(&self) -> i64 {
        self.now_secs.load(Ordering::SeqCst)
    }

    fn subsec_nanos(&self) -> u32 {
        self.nanos
    }
}

/// Conversion, probe and quota-free-generation counters shared between the
/// test and its provider fake.
#[derive(Default)]
struct Counters {
    conversions: AtomicUsize,
    probes: AtomicUsize,
    quota_free_generations: AtomicUsize,
}

/// How the fake's quota-free (Degraded Feed) generation answers.
enum QuotaFreeOutcome {
    /// The provider supports no quota-free generation (the trait default):
    /// the degrade rung errors.
    Unsupported,
    /// The degrade rung produces this body.
    Body(String),
    /// The degrade rung fails with this error.
    Fail(String),
}

/// Provider fake whose conversions, probes and quota-free generations are
/// counted; conversions can be slowed down (to make coalescing observable) or
/// failed — plain or quota-exhausted — to reach the Degraded Feed rung, the
/// probe outcome is scripted, and the Quota Breaker preemption and degrade
/// rung behavior are reachable without any provider facts hardcoded here.
struct FakeProvider {
    canonical_id: Option<String>,
    counters: Arc<Counters>,
    conversion_delay: Duration,
    fail_conversions: bool,
    /// Conversion failures carrying the typed `QuotaExhausted` case, so the
    /// module classifies them as quota exhaustion by type.
    quota_exhausted_conversions: bool,
    /// Scripted probe answer: `Some(unchanged)` answers the probe, `None`
    /// reports the probe unsupported (revalidation then converts).
    probe_unchanged: Option<bool>,
    /// Redis key of the fake's Quota Breaker; `None` for a provider without
    /// metered quota.
    quota_breaker_key: Option<String>,
    /// Scripted answer of the quota-free (Degraded Feed) generation.
    quota_free_outcome: QuotaFreeOutcome,
}

impl FakeProvider {
    /// Returns the fake and the counters its conversions and probes are
    /// counted in.
    fn new(canonical_id: Option<&str>) -> (Self, Arc<Counters>) {
        let counters: Arc<Counters> = Arc::default();
        (
            Self {
                canonical_id: canonical_id.map(str::to_string),
                counters: counters.clone(),
                conversion_delay: Duration::ZERO,
                fail_conversions: false,
                quota_exhausted_conversions: false,
                probe_unchanged: None,
                quota_breaker_key: None,
                quota_free_outcome: QuotaFreeOutcome::Unsupported,
            },
            counters,
        )
    }
}

#[async_trait]
impl MediaProvider for FakeProvider {
    async fn generate_rss_feed(&self, channel_url: Url) -> eyre::Result<GeneratedFeed> {
        self.counters.conversions.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.conversion_delay).await;
        if self.quota_exhausted_conversions {
            return Err(eyre::Report::new(QuotaExhausted::new(eyre::eyre!(
                "upstream rejected the call: quota spent"
            ))));
        }
        if self.fail_conversions {
            return Err(eyre::eyre!("conversion failed"));
        }
        Ok(GeneratedFeed {
            body: format!("<rss>converted {channel_url}</rss>"),
            probe_state: None,
            quota_units: None,
        })
    }

    async fn probe_feed_freshness(
        &self,
        _channel_url: &Url,
        _state: &FeedProbeState,
    ) -> eyre::Result<bool> {
        self.counters.probes.fetch_add(1, Ordering::SeqCst);
        match self.probe_unchanged {
            Some(unchanged) => Ok(unchanged),
            None => Err(eyre::eyre!("probe not supported by the fake provider")),
        }
    }

    async fn generate_rss_feed_quota_free(&self, _channel_url: Url) -> eyre::Result<String> {
        self.counters
            .quota_free_generations
            .fetch_add(1, Ordering::SeqCst);
        match &self.quota_free_outcome {
            QuotaFreeOutcome::Body(body) => Ok(body.clone()),
            QuotaFreeOutcome::Fail(message) => Err(eyre::eyre!("{message}")),
            QuotaFreeOutcome::Unsupported => Err(eyre::eyre!(
                "quota-free feed generation not supported by this provider"
            )),
        }
    }

    fn quota_breaker_key(&self) -> Option<String> {
        self.quota_breaker_key.clone()
    }

    fn next_quota_reset(&self, now_utc: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // the fake's own reset policy: one hour after now
        self.quota_breaker_key
            .as_ref()
            .map(|_| now_utc + chrono::Duration::hours(1))
    }

    fn quota_usage_day(&self, _now_utc: DateTime<Utc>) -> Option<String> {
        Some("fake-usage-day".to_string())
    }

    async fn canonical_feed_id(&self, _channel_url: &Url) -> Option<String> {
        self.canonical_id.clone()
    }

    async fn get_stream_url(&self, _media_url: &Url) -> eyre::Result<Url> {
        Err(eyre::eyre!("not supported by the fake provider"))
    }

    fn domain_whitelist_regexes(&self) -> Vec<regex::Regex> {
        Vec::new()
    }
}

/// Spawner fake that records every revalidation request — its Canonical Feed
/// Key included — and keeps the task, so tests can assert the request count
/// and target, and drive the revalidation inline (`run_pending`) instead of
/// racing a real runtime.
#[derive(Default)]
struct RecordingSpawner {
    requests: AtomicUsize,
    pending: Mutex<Vec<(String, RevalidationTask)>>,
}

impl RecordingSpawner {
    /// Revalidation requests recorded so far.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// The Canonical Feed Keys of the recorded requests, in request order.
    fn keys(&self) -> Vec<String> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Drive every recorded revalidation to completion, in request order.
    async fn run_pending(&self) {
        let tasks: Vec<(String, RevalidationTask)> =
            std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, task) in tasks {
            task.await;
        }
    }
}

impl RevalidationSpawner for RecordingSpawner {
    fn spawn(&self, canonical_key: &str, task: RevalidationTask) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.pending
            .lock()
            .unwrap()
            .push((canonical_key.to_string(), task));
    }
}

/// Cache adapter wrapper counting every seam call the serving module makes:
/// one call is one storage round trip (the Redis adapter pipelines each
/// widened operation into a single round trip; the in-memory adapter is one
/// map access). Pins the hot path's round-trip budget — a fresh check costs
/// one load, a conversion costs one store, and nothing re-reads what it
/// just stored.
struct CountingCache {
    inner: InMemoryCache,
    loads: AtomicUsize,
    stores: AtomicUsize,
    renewals: AtomicUsize,
}

impl CountingCache {
    fn new() -> Self {
        Self {
            inner: InMemoryCache::new(),
            loads: AtomicUsize::new(0),
            stores: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl FeedCacheStore for CountingCache {
    async fn load_state(&self, canonical_key: &str, now: i64) -> super::LoadState {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load_state(canonical_key, now).await
    }

    async fn store_and_report(
        &self,
        canonical_key: &str,
        feed: &CachedFeed,
        windows: FreshnessWindows,
        now: i64,
    ) -> eyre::Result<i64> {
        self.stores.fetch_add(1, Ordering::SeqCst);
        self.inner
            .store_and_report(canonical_key, feed, windows, now)
            .await
    }

    async fn bump_quota_usage(&self, units: u64, day_key: &str) -> eyre::Result<i64> {
        self.inner.bump_quota_usage(units, day_key).await
    }

    async fn renew_freshness(
        &self,
        canonical_key: &str,
        windows: FreshnessWindows,
        now: i64,
    ) -> eyre::Result<()> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        self.inner
            .renew_freshness(canonical_key, windows, now)
            .await
    }

    async fn quota_breaker_open(&self, breaker_key: &str, now: i64) -> Option<i64> {
        self.inner.quota_breaker_open(breaker_key, now).await
    }

    async fn open_quota_breaker(
        &self,
        breaker_key: &str,
        reset_epoch_secs: i64,
        now: i64,
    ) -> eyre::Result<()> {
        self.inner
            .open_quota_breaker(breaker_key, reset_epoch_secs, now)
            .await
    }
}

/// Redis key shared by the provider fake and the breaker manipulations in
/// the tests below: both sides must name the same breaker for the preemption
/// and opening behavior to engage.
const FAKE_BREAKER_KEY: &str = "vod2pod:quota_breaker:fake";

fn test_policy() -> ServingPolicy {
    ServingPolicy {
        fresh_ttl_secs: 3600,
        stale_window_secs: 7 * 24 * 3600,
        max_fresh_period_secs: 24 * 3600,
        stale_serve_max_age_secs: 60,
        degraded_fresh_ttl_secs: 300,
        fresh_ttl_jitter_secs: 0,
    }
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

/// Seed the cache through the adapter itself, as a conversion one fresh
/// period ago would have.
async fn seed(
    cache: &InMemoryCache,
    key: &str,
    clock: &FixedClock,
    body: &str,
    fresh_ttl_secs: u64,
) {
    seed_with(cache, key, clock, body, fresh_ttl_secs, None, false).await;
}

/// Seed with an explicit probe state and degraded flag, as a conversion (or
/// the degraded rung) would have stored it.
async fn seed_with(
    cache: &InMemoryCache,
    key: &str,
    clock: &FixedClock,
    body: &str,
    fresh_ttl_secs: u64,
    probe_state: Option<FeedProbeState>,
    degraded: bool,
) {
    let generated_at = clock.unix_now() - 10;
    cache
        .store_and_report(
            key,
            &CachedFeed {
                body: body.to_string(),
                generated_at,
                probe_state,
                degraded,
            },
            FreshnessWindows {
                fresh_ttl_secs,
                stale_window_secs: 7 * 24 * 3600,
            },
            clock.unix_now(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn fresh_hit_serves_cached_body_with_freshness_metadata() {
    // the conversion happened at t=990; the poll arrives 10s later
    let clock = Arc::new(FixedClock::at(990));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );
    seed(&cache, "k", &clock, "<rss>cached</rss>", 3600).await;
    clock.advance_secs(10);

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    assert_eq!(served.freshness, Freshness::Fresh);
    assert_eq!(served.body, "<rss>cached</rss>");
    assert_eq!(served.generated_at, 980);
    assert!(!served.degraded);
    // max-age is the remaining fresh-marker ttl
    assert_eq!(served.max_age_secs, 3600 - 10);
    assert_eq!(served.canonical_key, "k");
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stale_hit_serves_within_stale_window_honoring_stale_serve_max_age() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );
    seed(&cache, "k", &clock, "<rss>cached</rss>", 3600).await;
    // the fresh marker (1h) is gone, the copy (7d window) is not
    clock.advance_secs(3600 + 1);

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.body, "<rss>cached</rss>");
    // stale copies tell the client to come back soon
    assert_eq!(served.max_age_secs, 60);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fresh_period_expiry_serves_stale_even_with_live_marker() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );
    // marker outlives the fresh period on purpose
    seed(&cache, "k", &clock, "<rss>cached</rss>", 100_000).await;
    // copy is one second past the fresh period, marker still alive
    clock.advance_secs(24 * 3600 + 1);

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // probes may not renew freshness forever: past the fresh period the copy
    // is stale even with a live marker
    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.max_age_secs, 60);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn miss_converts_stores_and_serves() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    assert_eq!(served.freshness, Freshness::FreshlyGenerated);
    assert_eq!(served.body, "<rss>converted https://example.com/feed</rss>");
    assert_eq!(served.generated_at, 1_000);
    // the fresh marker was just set, so its remaining ttl is the full one
    assert_eq!(served.max_age_secs, 3600);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);

    // the conversion result went through the cache adapter
    let stored = cache
        .load_state("k", clock.unix_now())
        .await
        .feed
        .expect("conversion must be stored");
    assert_eq!(stored.body, served.body);
    assert_eq!(stored.generated_at, 1_000);
    assert!(!stored.degraded);

    // a second poll now hits the fresh copy instead of converting again
    let again = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(again.freshness, Freshness::Fresh);
    assert_eq!(again.body, served.body);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn injected_policy_governs_the_windows() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let policy = ServingPolicy {
        fresh_ttl_secs: 50,
        stale_window_secs: 100,
        max_fresh_period_secs: 20,
        stale_serve_max_age_secs: 7,
        degraded_fresh_ttl_secs: 123,
        fresh_ttl_jitter_secs: 0,
    };
    let serving = FeedServing::new(policy, clock.clone(), Arc::new(RecordingSpawner::default()));

    let generated = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(generated.freshness, Freshness::FreshlyGenerated);

    // fresh ttl comes from the injected policy
    let fresh = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(fresh.freshness, Freshness::Fresh);
    assert_eq!(fresh.max_age_secs, 50);

    // past the fresh ttl but inside the (short) stale window: stale with the
    // injected stale-serve max-age
    clock.advance_secs(60);
    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);
    assert_eq!(stale.max_age_secs, 7);

    // past the stale window: miss, a new conversion happens
    clock.advance_secs(50);
    let regenerated = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(regenerated.freshness, Freshness::FreshlyGenerated);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn injected_fresh_ttl_jitter_spreads_expiry() {
    let clock = Arc::new(FixedClock::with_nanos(1_000, 250));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, _) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let policy = ServingPolicy {
        fresh_ttl_secs: 100,
        stale_window_secs: 100_000,
        max_fresh_period_secs: 100_000,
        stale_serve_max_age_secs: 60,
        degraded_fresh_ttl_secs: 300,
        fresh_ttl_jitter_secs: 100,
    };
    let serving = FeedServing::new(policy, clock.clone(), Arc::new(RecordingSpawner::default()));

    serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // 100 + (250 nanos % 100 jitter) = 150
    let fresh = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(fresh.max_age_secs, 150);
}

#[tokio::test]
async fn provider_canonical_key_shares_entry_freshness_and_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("yt:playlist:PLabc"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let first = serving
        .serve(
            cache.clone(),
            &url("https://a.example/one"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(first.freshness, Freshness::FreshlyGenerated);

    // a different Source URL, same provider identity: one cache entry, one
    // conversion, freshness shared
    let second = serving
        .serve(
            cache.clone(),
            &url("https://b.example/two"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(second.freshness, Freshness::Fresh);
    assert_eq!(second.body, first.body);
    assert_eq!(second.canonical_key, first.canonical_key);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn normalized_url_canonical_key_shares_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    // provider cannot determine an identity: the module falls back to
    // Source URL normalization
    let (provider, counters) = FakeProvider::new(None);
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let first = serving
        .serve(
            cache.clone(),
            &url("https://youtube.com/playlist?list=PLa"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(first.freshness, Freshness::FreshlyGenerated);

    // a host-spelling variant of the same Source URL normalizes to the same
    // key
    let second = serving
        .serve(
            cache.clone(),
            &url("https://WWW.YOUTUBE.com/playlist?list=PLa"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(second.freshness, Freshness::Fresh);
    assert_eq!(second.body, first.body);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_misses_coalesce_into_one_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("coalesce"));
    provider.conversion_delay = Duration::from_millis(300);
    let provider = Arc::new(provider);
    let serving = Arc::new(FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    ));

    // two concurrent polls for the same feed: the second caller waits for
    // the in-flight regeneration and re-reads the cache instead of
    // converting again
    let url_a = url("https://example.com/one");
    let url_b = url("https://example.com/two");
    let (a, b) = tokio::join!(
        serving.serve(cache.clone(), &url_a, provider.clone()),
        serving.serve(cache.clone(), &url_b, provider.clone()),
    );

    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.body, b.body);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
}

// --- the hot path's round-trip budget: one seam call per storage round trip ---

#[tokio::test]
async fn a_fresh_serve_costs_exactly_one_load_round_trip() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(CountingCache::new());
    let (provider, counters) = FakeProvider::new(Some("rt-fresh"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );
    seed(&cache.inner, "rt-fresh", &clock, "<rss>cached</rss>", 3600).await;
    clock.advance_secs(10);

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.freshness, Freshness::Fresh);

    // one cache check, one round trip: the load-state call carried the copy
    // and the fresh marker together, nothing stored or re-read
    assert_eq!(cache.loads.load(Ordering::SeqCst), 1);
    assert_eq!(cache.stores.load(Ordering::SeqCst), 0);
    assert_eq!(cache.renewals.load(Ordering::SeqCst), 0);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_cold_conversion_costs_two_loads_and_one_store() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(CountingCache::new());
    let (provider, counters) = FakeProvider::new(Some("rt-miss"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.freshness, Freshness::FreshlyGenerated);

    // the miss check, the re-check after the regeneration slot, and the
    // store-and-report that anchored the result: nothing re-read what it
    // just stored
    assert_eq!(cache.loads.load(Ordering::SeqCst), 2);
    assert_eq!(cache.stores.load(Ordering::SeqCst), 1);
    assert_eq!(cache.renewals.load(Ordering::SeqCst), 0);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
}

// --- stale-while-revalidate: the module schedules and runs revalidation ---

#[tokio::test]
async fn stale_hit_records_one_revalidation_request_per_hit_and_fresh_none() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (provider, counters) = FakeProvider::new(Some("req-per-hit"));
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed(&cache, "req-per-hit", &clock, "<rss>cached</rss>", 3600).await;

    // a fresh hit schedules nothing
    let fresh = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(fresh.freshness, Freshness::Fresh);
    assert_eq!(spawner.requests(), 0);

    // every stale hit schedules exactly one revalidation request
    clock.advance_secs(3600 + 1);
    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);
    assert_eq!(spawner.requests(), 1);

    let again = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(again.freshness, Freshness::Stale);
    // both requests target this feed's Canonical Feed Key
    assert_eq!(
        spawner.keys(),
        vec!["req-per-hit".to_string(), "req-per-hit".to_string()]
    );
    // serving from cache never converts
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn revalidation_probes_and_renews_freshness_without_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("probe-renews"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    // the fresh marker (10s) dies before the body does: the poll serves
    // stale, the copy is still young enough to be probeable
    seed_with(
        &cache,
        "probe-renews",
        &clock,
        "<rss>cached</rss>",
        10,
        Some(FeedProbeState::default()),
        false,
    )
    .await;
    clock.advance_secs(20);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);
    assert_eq!(spawner.requests(), 1);

    spawner.run_pending().await;

    // the probe answered "unchanged": freshness renewed, no Feed Conversion,
    // the probe's ~1 quota unit recorded
    assert_eq!(counters.probes.load(Ordering::SeqCst), 1);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
    assert_eq!(cache.total_quota_usage().await, 1);
    assert_eq!(
        cache
            .load_state("probe-renews", clock.unix_now())
            .await
            .fresh_marker_ttl,
        Some(3600)
    );

    // the next poll is served fresh again
    let renewed = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(renewed.freshness, Freshness::Fresh);
}

#[tokio::test]
async fn probe_changed_falls_through_to_full_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("probe-changed"));
    provider.probe_unchanged = Some(false);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed_with(
        &cache,
        "probe-changed",
        &clock,
        "<rss>cached</rss>",
        10,
        Some(FeedProbeState::default()),
        false,
    )
    .await;
    clock.advance_secs(20);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    assert_eq!(counters.probes.load(Ordering::SeqCst), 1);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);

    // the conversion result replaced the stale copy and is served fresh
    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.freshness, Freshness::Fresh);
    assert_eq!(served.body, "<rss>converted https://example.com/feed</rss>");
}

#[tokio::test]
async fn probe_failure_falls_through_to_full_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    // probe_unchanged = None: the fake reports the probe unsupported (Err)
    let (provider, counters) = FakeProvider::new(Some("probe-failure"));
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed_with(
        &cache,
        "probe-failure",
        &clock,
        "<rss>cached</rss>",
        10,
        Some(FeedProbeState::default()),
        false,
    )
    .await;
    clock.advance_secs(20);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    assert_eq!(counters.probes.load(Ordering::SeqCst), 1);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.freshness, Freshness::Fresh);
    assert_eq!(served.body, "<rss>converted https://example.com/feed</rss>");
}

#[tokio::test]
async fn feed_past_fresh_period_is_never_probed_and_converts_fully() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("past-period"));
    // the probe would answer "unchanged": if it ran, the copy would be
    // renewed instead of converted
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    // marker outlives the fresh period on purpose; the copy is one second
    // past it
    seed_with(
        &cache,
        "past-period",
        &clock,
        "<rss>cached</rss>",
        100_000,
        Some(FeedProbeState::default()),
        false,
    )
    .await;
    clock.advance_secs(24 * 3600 + 1);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    // past the Fresh Period probes may not renew: full conversion
    assert_eq!(counters.probes.load(Ordering::SeqCst), 0);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn degraded_feed_is_never_probed_and_converts_fully() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("degraded"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    // degraded copies must be replaced by a real conversion as soon as
    // possible, never renewed by a probe
    seed_with(
        &cache,
        "degraded",
        &clock,
        "<rss>degraded</rss>",
        10,
        Some(FeedProbeState::default()),
        true,
    )
    .await;
    clock.advance_secs(20);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    assert_eq!(counters.probes.load(Ordering::SeqCst), 0);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.body, "<rss>converted https://example.com/feed</rss>");
    assert!(!served.degraded);
}

#[tokio::test]
async fn revalidation_coalesces_when_one_is_already_in_flight() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("coalesce-reval"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed_with(
        &cache,
        "coalesce-reval",
        &clock,
        "<rss>cached</rss>",
        10,
        Some(FeedProbeState::default()),
        false,
    )
    .await;
    clock.advance_secs(20);

    // another regeneration of this feed already holds the slot: the
    // recorded revalidation finds it in flight and skips entirely
    let _slot = super::begin_regeneration("coalesce-reval").await;

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);
    assert_eq!(spawner.requests(), 1);

    spawner.run_pending().await;

    assert_eq!(counters.probes.load(Ordering::SeqCst), 0);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn revalidation_is_skipped_while_the_quota_breaker_is_open() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("tail-preempts"));
    provider.probe_unchanged = Some(true);
    provider.quota_breaker_key = Some(FAKE_BREAKER_KEY.to_string());
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed_with(
        &cache,
        "tail-preempts",
        &clock,
        "<rss>cached</rss>",
        10,
        Some(FeedProbeState::default()),
        false,
    )
    .await;
    // the provider's quota is known to be exhausted until far past the test's
    // clock
    cache
        .open_quota_breaker(FAKE_BREAKER_KEY, 10_000, clock.unix_now())
        .await
        .unwrap();
    clock.advance_secs(20);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    // the Quota Breaker is open: no metered work, the stale body keeps
    // serving untouched
    assert_eq!(counters.probes.load(Ordering::SeqCst), 0);
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.body, "<rss>cached</rss>");
}

#[tokio::test]
async fn revalidation_failure_keeps_the_stale_body_and_does_not_degrade() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("reval-failure"));
    provider.fail_conversions = true;
    provider.quota_free_outcome = QuotaFreeOutcome::Body("<atom>degraded</atom>".to_string());
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed(&cache, "reval-failure", &clock, "<rss>cached</rss>", 3600).await;
    clock.advance_secs(3600 + 1);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    // the revalidation converted and failed; the stale body was kept on
    // purpose, and no Degraded Feed was produced: the background has no
    // client to serve, the stale body keeps serving clients
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 0);
    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.body, "<rss>cached</rss>");
}

#[tokio::test]
async fn revalidation_quota_exhaustion_opens_the_breaker() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("reval-quota"));
    provider.quota_exhausted_conversions = true;
    provider.quota_breaker_key = Some(FAKE_BREAKER_KEY.to_string());
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone());
    seed(&cache, "reval-quota", &clock, "<rss>cached</rss>", 3600).await;
    clock.advance_secs(3600 + 1);

    let stale = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);

    spawner.run_pending().await;

    // the quota exhaustion opened the breaker, holding until the provider's
    // own reset answer (the fake's policy: one hour after the fixed clock's
    // now)
    let now = DateTime::<Utc>::from_timestamp(clock.unix_now(), 0).unwrap();
    assert_eq!(
        cache.breaker_reset(FAKE_BREAKER_KEY).await,
        Some((now + chrono::Duration::hours(1)).timestamp())
    );
    // the background produced no Degraded Feed: there is no client to serve
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 0);
    // the stale body was kept on purpose: clients keep being served
    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.body, "<rss>cached</rss>");
}

// --- pure-function pins ---

#[test]
fn test_canonicalize_url_merges_host_spelling_variants() {
    let expected = super::canonicalize_url(&url("https://youtube.com/playlist?list=PLabc"));
    assert_eq!(
        expected,
        super::canonicalize_url(&url("https://www.youtube.com/playlist?list=PLabc"))
    );
    assert_eq!(
        expected,
        super::canonicalize_url(&url("https://m.youtube.com/playlist?list=PLabc"))
    );
    assert_eq!(
        expected,
        super::canonicalize_url(&url("https://WWW.YOUTUBE.COM/playlist?list=PLabc"))
    );
    assert_eq!(
        expected,
        super::canonicalize_url(&url("https://youtube.com/playlist?list=PLabc#fragment"))
    );
    assert_eq!(
        expected,
        super::canonicalize_url(&url("https://youtube.com:443/playlist?list=PLabc"))
    );
}

#[test]
fn test_canonicalize_url_sorts_query_pairs() {
    assert_eq!(
        super::canonicalize_url(&url("https://youtube.com/playlist?list=PLabc&foo=1")),
        super::canonicalize_url(&url("https://youtube.com/playlist?foo=1&list=PLabc"))
    );
}

#[test]
fn test_canonicalize_url_keeps_distinct_feeds_distinct() {
    let a = super::canonicalize_url(&url("https://youtube.com/playlist?list=PLabc"));
    let b = super::canonicalize_url(&url("https://youtube.com/playlist?list=PLxyz"));
    assert_ne!(a, b);
    // http vs https stay distinct
    assert_ne!(
        super::canonicalize_url(&url("http://youtube.com/playlist?list=PLabc")),
        a
    );
}

#[test]
fn test_canonicalize_url_handles_no_query_and_trailing_slash() {
    assert_eq!(
        super::canonicalize_url(&url("https://example.com/feed/")),
        "https://example.com/feed"
    );
    assert_eq!(
        super::canonicalize_url(&url("https://example.com")),
        "https://example.com/"
    );
}

#[test]
fn test_canonicalize_url_keeps_non_default_ports() {
    assert_eq!(
        super::canonicalize_url(&url("https://example.com:8443/feed")),
        "https://example.com:8443/feed"
    );
}

#[test]
fn test_classify_freshness_fresh_within_period() {
    assert_eq!(
        super::classify_freshness(true, true, 1_000, 1_500, 600, 7 * 24 * 3600),
        Some(Freshness::Fresh)
    );
}

#[test]
fn test_classify_freshness_stale_when_marker_expired_but_in_window() {
    assert_eq!(
        super::classify_freshness(true, false, 1_000, 2_000, 600, 7 * 24 * 3600),
        Some(Freshness::Stale)
    );
}

#[test]
fn test_classify_freshness_stale_when_fresh_period_exceeded() {
    // fresh marker alive but the copy is older than the fresh period:
    // probes may not renew forever, force revalidation
    assert_eq!(
        super::classify_freshness(true, true, 1_000, 1_000 + 600, 599, 7 * 24 * 3600),
        Some(Freshness::Stale)
    );
}

#[test]
fn test_classify_freshness_miss_beyond_stale_window() {
    assert_eq!(
        super::classify_freshness(
            true,
            false,
            1_000,
            1_000 + 7 * 24 * 3600,
            600,
            7 * 24 * 3600
        ),
        None
    );
}

#[test]
fn test_classify_freshness_miss_without_body() {
    assert_eq!(
        super::classify_freshness(false, true, 1_000, 1_100, 600, 7 * 24 * 3600),
        None
    );
}

#[tokio::test]
async fn exhausted_ladder_errors_when_degraded_feed_generation_fails() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("k"));
    provider.fail_conversions = true;
    // the Degraded Feed rung cannot produce anything either
    provider.quota_free_outcome = QuotaFreeOutcome::Fail("quota-free generation failed".into());
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let result = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await;

    // nothing cached, the conversion failed, the Degraded Feed rung failed:
    // the request errors — subscribers never receive a silent empty feed
    assert!(result.is_err());
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 1);
    assert!(cache.load_state("k", clock.unix_now()).await.feed.is_none());
}

#[tokio::test]
async fn open_breaker_skips_metered_conversion_and_serves_the_degraded_feed() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("k"));
    provider.quota_breaker_key = Some(FAKE_BREAKER_KEY.to_string());
    provider.quota_free_outcome = QuotaFreeOutcome::Body("<atom>degraded</atom>".to_string());
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );
    // the provider's quota is known to be exhausted until far past the test's
    // clock
    cache
        .open_quota_breaker(FAKE_BREAKER_KEY, 10_000, clock.unix_now())
        .await
        .unwrap();

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // the metered conversion was preempted: the Degraded Feed rung applied
    // right away
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 0);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 1);
    assert_eq!(served.freshness, Freshness::FreshlyGenerated);
    assert!(served.degraded);
    assert_eq!(served.body, "<atom>degraded</atom>");
    // the Degraded Feed is cached with its own short fresh ttl
    assert_eq!(served.max_age_secs, 300);
    let cached = cache.load_state("k", clock.unix_now()).await.feed.unwrap();
    assert!(cached.degraded);
    assert_eq!(
        cache
            .load_state("k", clock.unix_now())
            .await
            .fresh_marker_ttl,
        Some(300)
    );

    // a later poll is served by that cached copy, fresh, no new generation
    let again = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();
    assert_eq!(again.freshness, Freshness::Fresh);
    assert!(again.degraded);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quota_exhausted_conversion_opens_the_breaker_and_degrades() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("k"));
    provider.quota_exhausted_conversions = true;
    provider.quota_breaker_key = Some(FAKE_BREAKER_KEY.to_string());
    provider.quota_free_outcome = QuotaFreeOutcome::Body("<atom>degraded</atom>".to_string());
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // the conversion failed with quota exhaustion: the Degraded Feed rung
    // applied
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 1);
    assert!(served.degraded);
    assert_eq!(served.freshness, Freshness::FreshlyGenerated);
    // the breaker opened for the provider, holding until its own reset
    // answer (the fake's policy: one hour after now)
    let now = DateTime::<Utc>::from_timestamp(clock.unix_now(), 0).unwrap();
    assert_eq!(
        cache.breaker_reset(FAKE_BREAKER_KEY).await,
        Some((now + chrono::Duration::hours(1)).timestamp())
    );
}

#[tokio::test]
async fn non_quota_conversion_failure_still_attempts_the_degraded_rung() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("k"));
    provider.fail_conversions = true;
    provider.quota_breaker_key = Some(FAKE_BREAKER_KEY.to_string());
    provider.quota_free_outcome = QuotaFreeOutcome::Body("<atom>degraded</atom>".to_string());
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // a rung is skipped only when it cannot produce a result: the degrade
    // rung applies on any conversion failure, quota-related or not
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 1);
    assert!(served.degraded);
    // a failure that is not quota-related never opens the breaker
    assert_eq!(cache.breaker_reset(FAKE_BREAKER_KEY).await, None);
}

#[tokio::test]
async fn quota_exhaustion_without_a_breaker_key_degrades_without_opening_a_breaker() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, counters) = FakeProvider::new(Some("k"));
    provider.quota_exhausted_conversions = true;
    // no breaker key: the provider has no metered quota to break
    provider.quota_free_outcome = QuotaFreeOutcome::Body("<atom>degraded</atom>".to_string());
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // the quota-style failure still falls through to the Degraded Feed rung
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quota_free_generations.load(Ordering::SeqCst), 1);
    assert!(served.degraded);
}

#[tokio::test]
async fn degraded_feed_gets_the_injected_short_fresh_ttl() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let (mut provider, _counters) = FakeProvider::new(Some("k"));
    provider.quota_breaker_key = Some(FAKE_BREAKER_KEY.to_string());
    provider.quota_free_outcome = QuotaFreeOutcome::Body("<atom>degraded</atom>".to_string());
    let provider = Arc::new(provider);
    let policy = ServingPolicy {
        degraded_fresh_ttl_secs: 123,
        ..test_policy()
    };
    let serving = FeedServing::new(policy, clock.clone(), Arc::new(RecordingSpawner::default()));
    cache
        .open_quota_breaker(FAKE_BREAKER_KEY, 10_000, clock.unix_now())
        .await
        .unwrap();

    let served = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await
        .unwrap();

    // the degraded fresh ttl comes from the injected policy, not a constant
    assert_eq!(served.max_age_secs, 123);
    assert_eq!(
        cache
            .load_state("k", clock.unix_now())
            .await
            .fresh_marker_ttl,
        Some(123)
    );
}

/// The fake-provider breaker tests above pin the ladder policy but bypass
/// the dispatched enum; this pin drives the same policy through the real
/// dispatched provider — `provider::from` over a provider url, exactly as
/// the server constructs it — which must answer the quota facts the
/// breaker consumes. A provider method the dispatcher macro does not
/// forward answers its no-quota default, and the breaker would never open
/// (issue #176).
#[tokio::test]
async fn quota_exhausted_conversion_through_the_dispatched_provider_opens_the_breaker() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let provider = Arc::new(crate::provider::from(&url(
        "https://www.youtube.com/playlist?list=abc",
    )));
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );
    let err = eyre::Report::new(QuotaExhausted::new(eyre::eyre!(
        "upstream rejected the call: quota spent"
    )));

    let classified = serving
        .open_breaker_if_quota_exhausted(cache.as_ref(), provider.as_ref(), &err)
        .await;

    assert!(classified);
    // the breaker holds until the reset time the provider itself answers
    // through the enum — a fall-through to the no-quota default would leave
    // it closed
    let breaker_key = provider.quota_breaker_key().unwrap();
    let now = DateTime::<Utc>::from_timestamp(clock.unix_now(), 0).unwrap();
    let reset = provider.next_quota_reset(now).unwrap();
    assert!(reset > now, "the provider answers a future reset");
    assert_eq!(
        cache.breaker_reset(&breaker_key).await,
        Some(reset.timestamp())
    );
}

/// Usage day facts reach the usage recorder through the dispatched
/// provider: the units land only because the provider answers a day. A
/// method the dispatcher macro does not forward answers the no-quota
/// default, and nothing is recorded (issue #176).
#[tokio::test]
async fn quota_usage_reaches_the_recorder_through_the_dispatched_provider() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new());
    let provider = Arc::new(crate::provider::from(&url(
        "https://www.youtube.com/playlist?list=abc",
    )));
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
    );

    serving
        .record_quota_usage(cache.as_ref(), provider.as_ref(), 7)
        .await;

    // the units land only when the enum forwards the usage day; the
    // no-quota default records nothing
    assert_eq!(cache.total_quota_usage().await, 7);
}
