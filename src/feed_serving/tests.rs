//! Hermetic tests of the Degradation Ladder at the Feed Serving interface:
//! in-memory cache adapter, fixed clock, scripted provider fake — no Redis,
//! no network, no credentials. Every served rung of docs/adr/0001 is
//! asserted here.

use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use url::Url;

use super::{FeedCacheStore, FeedServing, Freshness, ServingPolicy};
use crate::feed_cache::CachedFeed;
use crate::provider::{GeneratedFeed, MediaProvider};

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

/// Provider fake whose conversions are counted and can be slowed down (to
/// make coalescing observable) or failed (to reach the ladder tail).
struct FakeProvider {
    canonical_id: Option<String>,
    conversions: Arc<AtomicUsize>,
    conversion_delay: Duration,
    fail_conversions: bool,
}

impl FakeProvider {
    /// Returns the fake and the counter its conversions are counted in.
    fn new(canonical_id: Option<&str>) -> (Self, Arc<AtomicUsize>) {
        let conversions = Arc::new(AtomicUsize::new(0));
        (
            Self {
                canonical_id: canonical_id.map(str::to_string),
                conversions: conversions.clone(),
                conversion_delay: Duration::ZERO,
                fail_conversions: false,
            },
            conversions,
        )
    }
}

#[async_trait]
impl MediaProvider for FakeProvider {
    async fn generate_rss_feed(&self, channel_url: Url) -> eyre::Result<GeneratedFeed> {
        self.conversions.fetch_add(1, Ordering::SeqCst);
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

/// Downstream rungs that never apply: the module converts on every miss.
/// Counts how often the module consulted the ladder tail.
#[derive(Default)]
struct FakeDownstream {
    consulted: AtomicUsize,
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
    let generated_at = clock.unix_now() - 10;
    cache
        .store(
            key,
            &CachedFeed {
                body: body.to_string(),
                generated_at,
                probe_state: None,
                degraded: false,
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
    let cache = InMemoryCache::new(clock.clone());
    let (provider, conversions) = FakeProvider::new(Some("k"));
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Box::new(FakeDownstream::default()),
    );
    seed(&cache, "k", &clock, "<rss>cached</rss>", 3600).await;
    clock.advance_secs(10);

    let served = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();

    assert_eq!(served.freshness, Freshness::Fresh);
    assert_eq!(served.body, "<rss>cached</rss>");
    assert_eq!(served.generated_at, 980);
    assert!(!served.degraded);
    // max-age is the remaining fresh-marker ttl
    assert_eq!(served.max_age_secs, 3600 - 10);
    assert_eq!(served.canonical_key, "k");
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stale_hit_serves_within_stale_window_honoring_stale_serve_max_age() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    let (provider, conversions) = FakeProvider::new(Some("k"));
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Box::new(FakeDownstream::default()),
    );
    seed(&cache, "k", &clock, "<rss>cached</rss>", 3600).await;
    // the fresh marker (1h) is gone, the copy (7d window) is not
    clock.advance_secs(3600 + 1);

    let served = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();

    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.body, "<rss>cached</rss>");
    // stale copies tell the client to come back soon
    assert_eq!(served.max_age_secs, 60);
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fresh_period_expiry_serves_stale_even_with_live_marker() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    let (provider, conversions) = FakeProvider::new(Some("k"));
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Box::new(FakeDownstream::default()),
    );
    // marker outlives the fresh period on purpose
    seed(&cache, "k", &clock, "<rss>cached</rss>", 100_000).await;
    // copy is one second past the fresh period, marker still alive
    clock.advance_secs(24 * 3600 + 1);

    let served = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();

    // probes may not renew freshness forever: past the fresh period the copy
    // is stale even with a live marker
    assert_eq!(served.freshness, Freshness::Stale);
    assert_eq!(served.max_age_secs, 60);
    assert_eq!(conversions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn miss_converts_stores_and_serves() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    let (provider, conversions) = FakeProvider::new(Some("k"));
    let downstream = FakeDownstream::default();
    let serving = FeedServing::new(test_policy(), clock.clone(), Box::new(downstream));

    let served = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();

    assert_eq!(served.freshness, Freshness::FreshlyGenerated);
    assert_eq!(served.body, "<rss>converted https://example.com/feed</rss>");
    assert_eq!(served.generated_at, 1_000);
    // the fresh marker was just set, so its remaining ttl is the full one
    assert_eq!(served.max_age_secs, 3600);
    assert_eq!(conversions.load(Ordering::SeqCst), 1);

    // the conversion result went through the cache adapter
    let stored = cache.load("k").await.expect("conversion must be stored");
    assert_eq!(stored.body, served.body);
    assert_eq!(stored.generated_at, 1_000);
    assert!(!stored.degraded);

    // a second poll now hits the fresh copy instead of converting again
    let again = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();
    assert_eq!(again.freshness, Freshness::Fresh);
    assert_eq!(again.body, served.body);
    assert_eq!(conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn injected_policy_governs_the_windows() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    let (provider, conversions) = FakeProvider::new(Some("k"));
    let policy = ServingPolicy {
        fresh_ttl_secs: 50,
        stale_window_secs: 100,
        max_fresh_period_secs: 20,
        stale_serve_max_age_secs: 7,
        fresh_ttl_jitter_secs: 0,
    };
    let serving = FeedServing::new(policy, clock.clone(), Box::new(FakeDownstream::default()));

    let generated = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();
    assert_eq!(generated.freshness, Freshness::FreshlyGenerated);

    // fresh ttl comes from the injected policy
    let fresh = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();
    assert_eq!(fresh.freshness, Freshness::Fresh);
    assert_eq!(fresh.max_age_secs, 50);

    // past the fresh ttl but inside the (short) stale window: stale with the
    // injected stale-serve max-age
    clock.advance_secs(60);
    let stale = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();
    assert_eq!(stale.freshness, Freshness::Stale);
    assert_eq!(stale.max_age_secs, 7);

    // past the stale window: miss, a new conversion happens
    clock.advance_secs(50);
    let regenerated = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();
    assert_eq!(regenerated.freshness, Freshness::FreshlyGenerated);
    assert_eq!(conversions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn injected_fresh_ttl_jitter_spreads_expiry() {
    let clock = Arc::new(FixedClock::with_nanos(1_000, 250));
    let cache = InMemoryCache::new(clock.clone());
    let (provider, _) = FakeProvider::new(Some("k"));
    let policy = ServingPolicy {
        fresh_ttl_secs: 100,
        stale_window_secs: 100_000,
        max_fresh_period_secs: 100_000,
        stale_serve_max_age_secs: 60,
        fresh_ttl_jitter_secs: 100,
    };
    let serving = FeedServing::new(policy, clock.clone(), Box::new(FakeDownstream::default()));

    serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();

    // 100 + (250 nanos % 100 jitter) = 150
    let fresh = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await
        .unwrap();
    assert_eq!(fresh.max_age_secs, 150);
}

#[tokio::test]
async fn provider_canonical_key_shares_entry_freshness_and_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    let (provider, conversions) = FakeProvider::new(Some("yt:playlist:PLabc"));
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Box::new(FakeDownstream::default()),
    );

    let first = serving
        .serve(&cache, &url("https://a.example/one"), &provider)
        .await
        .unwrap();
    assert_eq!(first.freshness, Freshness::FreshlyGenerated);

    // a different Source URL, same provider identity: one cache entry, one
    // conversion, freshness shared
    let second = serving
        .serve(&cache, &url("https://b.example/two"), &provider)
        .await
        .unwrap();
    assert_eq!(second.freshness, Freshness::Fresh);
    assert_eq!(second.body, first.body);
    assert_eq!(second.canonical_key, first.canonical_key);
    assert_eq!(conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn normalized_url_canonical_key_shares_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    // provider cannot determine an identity: the module falls back to
    // Source URL normalization
    let (provider, conversions) = FakeProvider::new(None);
    let serving = FeedServing::new(
        test_policy(),
        clock.clone(),
        Box::new(FakeDownstream::default()),
    );

    let first = serving
        .serve(
            &cache,
            &url("https://youtube.com/playlist?list=PLa"),
            &provider,
        )
        .await
        .unwrap();
    assert_eq!(first.freshness, Freshness::FreshlyGenerated);

    // a host-spelling variant of the same Source URL normalizes to the same
    // key
    let second = serving
        .serve(
            &cache,
            &url("https://WWW.YOUTUBE.com/playlist?list=PLa"),
            &provider,
        )
        .await
        .unwrap();
    assert_eq!(second.freshness, Freshness::Fresh);
    assert_eq!(second.body, first.body);
    assert_eq!(conversions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_misses_coalesce_into_one_conversion() {
    let clock = Arc::new(FixedClock::at(1_000));
    let cache = InMemoryCache::new(clock.clone());
    let (mut provider, conversions) = FakeProvider::new(Some("coalesce"));
    provider.conversion_delay = Duration::from_millis(300);
    let serving = Arc::new(FeedServing::new(
        test_policy(),
        clock.clone(),
        Box::new(FakeDownstream::default()),
    ));

    // two concurrent polls for the same feed: the second caller waits for
    // the in-flight regeneration and re-reads the cache instead of
    // converting again
    let url_a = url("https://example.com/one");
    let url_b = url("https://example.com/two");
    let (a, b) = tokio::join!(
        serving.serve(&cache, &url_a, &provider),
        serving.serve(&cache, &url_b, &provider),
    );

    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.body, b.body);
    assert_eq!(conversions.load(Ordering::SeqCst), 1);
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
    let cache = InMemoryCache::new(clock.clone());
    let (mut provider, conversions) = FakeProvider::new(Some("k"));
    provider.fail_conversions = true;
    let downstream = FakeDownstream::default();
    let serving = FeedServing::new(test_policy(), clock.clone(), Box::new(downstream));

    let result = serving
        .serve(&cache, &url("https://example.com/feed"), &provider)
        .await;

    // nothing cached, conversion failed, downstream rungs produced nothing:
    // the request errors
    assert!(result.is_err());
    assert_eq!(conversions.load(Ordering::SeqCst), 1);
    assert!(cache.load("k").await.is_none());
}
