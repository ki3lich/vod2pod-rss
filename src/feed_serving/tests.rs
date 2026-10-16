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
use url::Url;

use super::{
    FeedCacheStore, FeedServing, Freshness, RevalidationSpawner, RevalidationTask, ServingPolicy,
};
use crate::feed_cache::CachedFeed;
use crate::provider::{FeedProbeState, GeneratedFeed, MediaProvider};

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

/// Conversion and probe counters shared between the test and its provider
/// fake.
#[derive(Default)]
struct Counters {
    conversions: AtomicUsize,
    probes: AtomicUsize,
}

/// Provider fake whose conversions and probes are counted; conversions can
/// be slowed down (to make coalescing observable) or failed (to reach the
/// ladder tail), and the probe outcome is scripted.
struct FakeProvider {
    canonical_id: Option<String>,
    counters: Arc<Counters>,
    conversion_delay: Duration,
    fail_conversions: bool,
    /// Scripted probe answer: `Some(unchanged)` answers the probe, `None`
    /// reports the probe unsupported (revalidation then converts).
    probe_unchanged: Option<bool>,
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
                probe_unchanged: None,
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

/// Downstream rungs that never apply: the module converts on every miss.
/// Counts how often the module consulted the ladder tail, can be scripted to
/// preempt metered revalidation work, and records revalidation failures.
#[derive(Default)]
struct FakeDownstream {
    consulted: AtomicUsize,
    preempts: bool,
    failed_revalidations: AtomicUsize,
}

#[async_trait]
impl super::DownstreamRungs for FakeDownstream {
    async fn before_conversion(
        &self,
        _source_url: &Url,
        _provider: &dyn MediaProvider,
        _canonical_key: &str,
    ) -> Option<super::ServedFeed> {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        None
    }

    async fn on_conversion_failed(
        &self,
        _source_url: &Url,
        _provider: &dyn MediaProvider,
        _canonical_key: &str,
        _err: eyre::Error,
    ) -> Option<super::ServedFeed> {
        self.consulted.fetch_add(1, Ordering::SeqCst);
        None
    }

    async fn blocks_metered_revalidation(&self, _provider: &dyn MediaProvider) -> bool {
        self.preempts
    }

    async fn on_revalidation_failed(
        &self,
        _source_url: &Url,
        _provider: &dyn MediaProvider,
        _canonical_key: &str,
        _err: &eyre::Error,
    ) {
        self.failed_revalidations.fetch_add(1, Ordering::SeqCst);
    }
}

fn test_policy() -> ServingPolicy {
    ServingPolicy {
        fresh_ttl_secs: 3600,
        stale_window_secs: 7 * 24 * 3600,
        max_fresh_period_secs: 24 * 3600,
        stale_serve_max_age_secs: 60,
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
        .store(
            key,
            &CachedFeed {
                body: body.to_string(),
                generated_at,
                probe_state,
                degraded,
            },
            fresh_ttl_secs,
            7 * 24 * 3600,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn fresh_hit_serves_cached_body_with_freshness_metadata() {
    // the conversion happened at t=990; the poll arrives 10s later
    let clock = Arc::new(FixedClock::at(990));
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let downstream = FakeDownstream::default();
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(downstream),
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
    let stored = cache.load("k").await.expect("conversion must be stored");
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let policy = ServingPolicy {
        fresh_ttl_secs: 50,
        stale_window_secs: 100,
        max_fresh_period_secs: 20,
        stale_serve_max_age_secs: 7,
        fresh_ttl_jitter_secs: 0,
    };
    let serving = FeedServing::new(
        policy,
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
    );

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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, _) = FakeProvider::new(Some("k"));
    let provider = Arc::new(provider);
    let policy = ServingPolicy {
        fresh_ttl_secs: 100,
        stale_window_secs: 100_000,
        max_fresh_period_secs: 100_000,
        stale_serve_max_age_secs: 60,
        fresh_ttl_jitter_secs: 100,
    };
    let serving = FeedServing::new(
        policy,
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
    );

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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("yt:playlist:PLabc"));
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    // provider cannot determine an identity: the module falls back to
    // Source URL normalization
    let (provider, counters) = FakeProvider::new(None);
    let provider = Arc::new(provider);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("coalesce"));
    provider.conversion_delay = Duration::from_millis(300);
    let provider = Arc::new(provider);
    let serving = Arc::new(FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(FakeDownstream::default()),
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

// --- stale-while-revalidate: the module schedules and runs revalidation ---

#[tokio::test]
async fn stale_hit_records_one_revalidation_request_per_hit_and_fresh_none() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (provider, counters) = FakeProvider::new(Some("req-per-hit"));
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("probe-renews"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
    assert_eq!(cache.fresh_marker_ttl("probe-renews").await, Some(3600));

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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("probe-changed"));
    provider.probe_unchanged = Some(false);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    // probe_unchanged = None: the fake reports the probe unsupported (Err)
    let (provider, counters) = FakeProvider::new(Some("probe-failure"));
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("past-period"));
    // the probe would answer "unchanged": if it ran, the copy would be
    // renewed instead of converted
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("degraded"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("coalesce-reval"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        Arc::new(FakeDownstream::default()),
    );
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
async fn revalidation_is_skipped_while_the_tail_preempts_metered_work() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("tail-preempts"));
    provider.probe_unchanged = Some(true);
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let downstream = Arc::new(FakeDownstream {
        preempts: true,
        ..Default::default()
    });
    let serving = FeedServing::new(test_policy(), clock.clone(), spawner.clone(), downstream);
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
async fn revalidation_failure_keeps_the_stale_body_and_reaches_the_tail() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("reval-failure"));
    provider.fail_conversions = true;
    let provider = Arc::new(provider);
    let spawner = Arc::new(RecordingSpawner::default());
    let downstream = Arc::new(FakeDownstream::default());
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        spawner.clone(),
        downstream.clone(),
    );
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

    // the conversion failure reached the ladder tail...
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert_eq!(downstream.failed_revalidations.load(Ordering::SeqCst), 1);
    // ...and the stale body was kept on purpose: clients keep being served
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

// --- pure-function pins (moved with their functions from feed_cache) ---

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
async fn exhausted_ladder_errors_when_downstream_cannot_produce() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = Arc::new(InMemoryCache::new(clock.clone()));
    let (mut provider, counters) = FakeProvider::new(Some("k"));
    provider.fail_conversions = true;
    let provider = Arc::new(provider);
    let downstream = FakeDownstream::default();
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Arc::new(RecordingSpawner::default()),
        Arc::new(downstream),
    );

    let result = serving
        .serve(
            cache.clone(),
            &url("https://example.com/feed"),
            provider.clone(),
        )
        .await;

    // nothing cached, conversion failed, downstream rungs produced nothing:
    // the request errors
    assert!(result.is_err());
    assert_eq!(counters.conversions.load(Ordering::SeqCst), 1);
    assert!(cache.load("k").await.is_none());
}
