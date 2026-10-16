//! Stream-URL resolution: the seam between the transcoder and the providers'
//! yt-dlp-backed resolution.
//!
//! The transcoder consumes [`StreamUrlResolver`]. Production wires
//! [`ProviderStreamUrlResolver`] (provider dispatch, cached yt-dlp
//! resolution, the eviction-and-retry loop) and [`TcpPreflight`] (the real
//! reachability connect). Fakes of the two traits drive the whole pipeline
//! in tests — no yt-dlp, Redis or network.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::warn;
use reqwest::Url;

use crate::configs::{conf, Conf, ConfName};
use crate::provider::MediaProvider;

/// Turns a media URL — the original provider URL carried inside the RSS
/// Enclosure (e.g. a YouTube watch URL) — into the stream URL ffmpeg reads.
///
/// This is the seam the transcoder consumes. Implementations own the whole
/// resolution pipeline — provider dispatch, cached yt-dlp resolution, the
/// preflight reachability check and the eviction-and-retry loop — so the
/// transcoder can be built and tested without yt-dlp or network.
#[async_trait]
pub trait StreamUrlResolver: Send + Sync {
    /// Resolve the stream URL for `media_url`, failing fast when the
    /// resolved source turns out to be unreachable (the cached resolution is
    /// then evicted so the next request re-resolves).
    async fn resolve(&self, media_url: &Url) -> eyre::Result<Url>;
}

/// Preflight reachability check for a resolved stream URL.
///
/// An `Err` marks the source unreachable and fails the resolution fast
/// instead of letting ffmpeg hang. What "unreachable" means is the
/// implementation's call: the production [`TcpPreflight`] fails only on
/// connect timeouts — DNS errors and connection-refused are left for ffmpeg,
/// which fails on those quickly.
#[async_trait]
pub trait StreamReachability: Send + Sync {
    async fn check(&self, stream_url: &Url) -> eyre::Result<()>;
}

/// Decide a preflight verdict from a `tokio::time::timeout(TcpStream::connect(...))`
/// outcome.
///
/// Only a *timeout* is treated as a failure. It is the exact symptom of a CDN
/// edge node that resolves in DNS but silently drops TCP SYNs: ffmpeg would
/// otherwise burn the kernel's ~127s SYN-retry budget before returning
/// `ETIMEDOUT`. DNS errors and connection-refused are left for ffmpeg to
/// handle, because ffmpeg fails on those quickly — there is no long hang to
/// avoid, and failing the preflight on them would only add false negatives.
fn interpret_preflight_connect(
    outcome: Result<Result<(), std::io::Error>, tokio::time::error::Elapsed>,
) -> eyre::Result<()> {
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_fast_failure)) => Ok(()),
        Err(_timed_out) => Err(eyre::eyre!(
            "media source unreachable: timed out connecting to stream url"
        )),
    }
}

/// Open a TCP connection to the resolved stream URL's `host:port` with a short
/// timeout, returning the verdict via [`interpret_preflight_connect`].
///
/// `connect_timeout` is the maximum time to wait for the TCP handshake. The
/// caller (the server) already wraps transcoder construction in its own
/// `FfmpegTimeoutSeconds` timeout, so this preflight only needs to beat
/// ffmpeg's ~127s `ETIMEDOUT`, not the whole transcode.
async fn preflight_stream_reachable(url: &Url, connect_timeout: Duration) -> eyre::Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| eyre::eyre!("stream url has no host: {url}"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| eyre::eyre!("stream url has no known port: {url}"))?;

    let outcome = tokio::time::timeout(connect_timeout, async {
        tokio::net::TcpStream::connect((host, port))
            .await
            .map(|_| ())
    })
    .await;
    interpret_preflight_connect(outcome)
}

/// Production reachability check: a TCP connect to the stream URL's
/// `host:port` with a short timeout.
pub struct TcpPreflight {
    connect_timeout: Duration,
}

impl TcpPreflight {
    /// Preflight with the timeout configured via `PreflightTimeoutSeconds`
    /// (3 seconds when unset).
    pub fn with_configured_timeout() -> Self {
        Self {
            connect_timeout: Duration::from_secs(
                conf()
                    .get(ConfName::PreflightTimeoutSeconds)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(3),
            ),
        }
    }

    /// Preflight with an explicit timeout.
    #[cfg(test)]
    pub fn with_timeout(connect_timeout: Duration) -> Self {
        Self { connect_timeout }
    }
}

#[async_trait]
impl StreamReachability for TcpPreflight {
    async fn check(&self, stream_url: &Url) -> eyre::Result<()> {
        preflight_stream_reachable(stream_url, self.connect_timeout).await
    }
}

/// Production stream-URL resolution for one media URL.
///
/// Holds the provider the media URL dispatches to (for YouTube: yt-dlp
/// resolution cached in Redis) and the reachability check, and owns the
/// eviction-and-retry loop: when the preflight finds the resolved source
/// unreachable, the provider's cached resolution is evicted — the upstream
/// CDN edge may have rotated — so the next request re-resolves, and the
/// request itself fails fast.
pub struct ProviderStreamUrlResolver {
    provider: Arc<dyn MediaProvider>,
    reachability: Arc<dyn StreamReachability>,
}

impl ProviderStreamUrlResolver {
    /// Production pipeline for `media_url`: the provider the URL dispatches
    /// to, plus the TCP preflight with its configured timeout.
    pub fn for_media_url(media_url: &Url) -> Self {
        Self {
            provider: Arc::new(crate::provider::from(media_url)),
            reachability: Arc::new(TcpPreflight::with_configured_timeout()),
        }
    }

    /// Injection point for tests: any provider fake with any reachability
    /// fake.
    #[cfg(test)]
    pub fn new(
        provider: Arc<dyn MediaProvider>,
        reachability: Arc<dyn StreamReachability>,
    ) -> Self {
        Self {
            provider,
            reachability,
        }
    }
}

#[async_trait]
impl StreamUrlResolver for ProviderStreamUrlResolver {
    async fn resolve(&self, media_url: &Url) -> eyre::Result<Url> {
        let stream_url = self.provider.get_stream_url(media_url).await?;

        // Preflight: a CDN edge that drops SYNs makes ffmpeg hang ~127s before
        // ETIMEDOUT. Fail fast instead, and evict the cached stream URL so the
        // next request re-resolves (the upstream edge may have rotated).
        if let Err(e) = self.reachability.check(&stream_url).await {
            warn!(
                "preflight reachability check failed for {stream_url}; \
                 evicting cached stream url for {media_url} and failing fast: {e}"
            );
            // Best-effort: a Redis failure must not mask the original
            // preflight error.
            let _ = self.provider.evict_stream_url_cache(media_url).await;
            return Err(e);
        }

        Ok(stream_url)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::provider::GeneratedFeed;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    /// Resolution call count and evictions recorded by [`ScriptedProvider`].
    #[derive(Default)]
    struct ProviderLog {
        resolution_calls: AtomicUsize,
        evictions: AtomicUsize,
        evicted_sources: Mutex<Vec<Url>>,
    }

    /// Provider fake scripting `get_stream_url` answers in order and
    /// recording evictions — the cached yt-dlp rung without yt-dlp or Redis.
    struct ScriptedProvider {
        resolutions: Mutex<Vec<eyre::Result<Url>>>,
        log: Arc<ProviderLog>,
    }

    impl ScriptedProvider {
        fn new(resolutions: Vec<eyre::Result<Url>>) -> (Self, Arc<ProviderLog>) {
            let log: Arc<ProviderLog> = Arc::default();
            (
                Self {
                    resolutions: Mutex::new(resolutions),
                    log: log.clone(),
                },
                log,
            )
        }
    }

    #[async_trait]
    impl MediaProvider for ScriptedProvider {
        async fn generate_rss_feed(&self, _channel_url: Url) -> eyre::Result<GeneratedFeed> {
            panic!("stream-URL resolution never converts feeds")
        }

        async fn get_stream_url(&self, _media_url: &Url) -> eyre::Result<Url> {
            self.log.resolution_calls.fetch_add(1, Ordering::SeqCst);
            let mut resolutions = self.resolutions.lock().await;
            if resolutions.is_empty() {
                panic!(
                    "no scripted resolution left — the test drove resolution more times than scripted"
                );
            }
            resolutions.remove(0)
        }

        async fn evict_stream_url_cache(&self, media_url: &Url) -> eyre::Result<()> {
            self.log.evictions.fetch_add(1, Ordering::SeqCst);
            self.log
                .evicted_sources
                .lock()
                .await
                .push(media_url.clone());
            Ok(())
        }

        fn domain_whitelist_regexes(&self) -> Vec<regex::Regex> {
            Vec::new()
        }
    }

    /// Reachability fake scripting `check` answers in order and recording
    /// the URLs it was consulted on — the TCP preflight without network.
    struct ScriptedReachability {
        outcomes: Mutex<Vec<eyre::Result<()>>>,
        checked: Mutex<Vec<Url>>,
    }

    impl ScriptedReachability {
        fn new(outcomes: Vec<eyre::Result<()>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                checked: Mutex::new(Vec::new()),
            }
        }

        /// The URLs the preflight was consulted on, in order.
        async fn checked(&self) -> Vec<Url> {
            self.checked.lock().await.clone()
        }
    }

    #[async_trait]
    impl StreamReachability for ScriptedReachability {
        async fn check(&self, stream_url: &Url) -> eyre::Result<()> {
            self.checked.lock().await.push(stream_url.clone());
            let mut outcomes = self.outcomes.lock().await;
            if outcomes.is_empty() {
                panic!(
                    "no scripted reachability outcome left — the preflight ran more times than scripted"
                );
            }
            outcomes.remove(0)
        }
    }

    #[tokio::test]
    async fn unreachable_stream_url_evicts_cache_and_next_request_reresolves() {
        // The eviction-and-retry loop, driven end to end on fakes: the cached
        // resolution points at an unreachable source (the preflight's connect
        // times out), so the request fails fast and the cache is evicted; the
        // next request re-resolves — the upstream edge rotated — and its
        // preflight runs against the fresh URL, which passes.
        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let cached = Url::parse("https://dead.edge.googlevideo.com/videoplayback?x=1").unwrap();
        let fresh = Url::parse("https://live.edge.googlevideo.com/videoplayback?x=1").unwrap();
        let (provider, log) = ScriptedProvider::new(vec![Ok(cached.clone()), Ok(fresh.clone())]);
        let reachability = Arc::new(ScriptedReachability::new(vec![
            Err(eyre::eyre!(
                "media source unreachable: timed out connecting to stream url"
            )),
            Ok(()),
        ]));
        let resolver = ProviderStreamUrlResolver::new(Arc::new(provider), reachability.clone());

        let first = resolver.resolve(&media).await;
        assert!(
            first.is_err(),
            "an unreachable source must fail fast, got {first:?}"
        );
        assert_eq!(log.evictions.load(Ordering::SeqCst), 1);
        assert_eq!(*log.evicted_sources.lock().await, vec![media.clone()]);

        let second = resolver
            .resolve(&media)
            .await
            .expect("after eviction the next request re-resolves and must succeed");
        assert_eq!(second, fresh);
        assert_eq!(log.resolution_calls.load(Ordering::SeqCst), 2);
        // The preflights ran against the resolved URLs — the dead cached one,
        // then the fresh one — never against the media URL being resolved.
        assert_eq!(reachability.checked().await, vec![cached, fresh]);
    }

    #[tokio::test]
    async fn resolution_failure_propagates_without_evicting() {
        // A resolution failure (yt-dlp finding nothing, say) propagates as
        // the stream error it produces today, and must NOT evict: nothing was
        // resolved, so there is no cached URL whose rotation could help.
        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let (provider, log) =
            ScriptedProvider::new(vec![Err(eyre::eyre!("unable to extract stream url"))]);
        let reachability = Arc::new(ScriptedReachability::new(Vec::new()));
        let resolver = ProviderStreamUrlResolver::new(Arc::new(provider), reachability.clone());

        let err = resolver
            .resolve(&media)
            .await
            .expect_err("the scripted resolution failure must propagate");
        assert!(err.to_string().contains("unable to extract stream url"));
        assert_eq!(log.evictions.load(Ordering::SeqCst), 0);
        assert_eq!(log.resolution_calls.load(Ordering::SeqCst), 1);
        assert!(
            reachability.checked().await.is_empty(),
            "an unresolved URL never reaches the preflight"
        );
    }

    #[test]
    fn preflight_connect_connected_is_ok() {
        assert!(interpret_preflight_connect(Ok(Ok(()))).is_ok());
    }

    #[test]
    fn preflight_connect_fast_failure_is_ok() {
        // DNS errors and connection-refused are fast: leave them to ffmpeg (no
        // long hang to avoid) and let the transcode proceed.
        let fast = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        assert!(interpret_preflight_connect(Ok(Err(fast))).is_ok());
    }

    #[tokio::test]
    async fn preflight_connect_timeout_is_err() {
        // A SYN-dropping endpoint never completes the handshake. A connect that
        // never resolves, timed out, reproduces the exact outcome ffmpeg would
        // otherwise hang ~127s on. Deterministic: no real blackhole required.
        let never = std::future::pending::<std::io::Result<()>>();
        let outcome = tokio::time::timeout(Duration::from_millis(20), never).await;
        assert!(interpret_preflight_connect(outcome).is_err());
    }

    #[tokio::test]
    async fn preflight_reachable_local_listener_is_ok() {
        // A listening port completes the TCP handshake via the kernel backlog,
        // even without a user-space accept().
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x.mp3")).unwrap();
        let preflight = TcpPreflight::with_timeout(Duration::from_secs(2));
        assert!(preflight.check(&url).await.is_ok());
        drop(listener);
    }

    #[tokio::test]
    async fn preflight_refused_local_port_is_ok() {
        // A closed local port refuses instantly — fast — so ffmpeg handles it
        // quickly; the preflight must NOT turn it into a failure.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // free the port -> subsequent connect is refused
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x.mp3")).unwrap();
        let preflight = TcpPreflight::with_timeout(Duration::from_secs(2));
        assert!(preflight.check(&url).await.is_ok());
    }
}
