//! Stream-URL resolution: the seam between the transcoder and the providers'
//! yt-dlp-backed resolution.
//!
//! The transcoder consumes [`StreamUrlResolver`]. Production wires
//! [`ProviderStreamUrlResolver`] (provider dispatch, cached yt-dlp
//! resolution, the staging gate, the check stage and the eviction-and-retry
//! loop) and [`TcpPreflight`] (the legacy reachability check). Fakes of the
//! traits drive the whole pipeline in tests — no yt-dlp, Redis or network.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::warn;
use reqwest::Url;

use crate::configs::{conf, Conf, ConfName};
use crate::provider::MediaProvider;
use crate::transcoder::staging::StagedFileGuard;
use tokio::sync::OwnedSemaphorePermit;

/// The source a resolution hands the transcoder: either the resolved Stream
/// URL for ffmpeg to read over the network (the legacy artifact), or a
/// Staged Source — the local, byte-verified file a Source Stage produced —
/// for ffmpeg to read from disk. The staged artifact is carried as a
/// [`StagedSource`]: the file and its Staging Slot owned together, so a
/// resolution or transcoder construction that fails or is dropped deletes
/// the file and releases the slot instead of stranding either for the
/// startup sweep or a queued transcode.
#[derive(Debug)]
pub enum ResolvedSource {
    StreamUrl(Url),
    StagedFile(StagedSource),
}

/// A Staged Source already holding its Staging Slot — the artifact
/// resolution hands the transcoder (issue #181).
///
/// The resolver acquires the slot before resolution begins and returns the
/// staged file owning it, so acquire-before-resolve is enforced by the type:
/// a staged artifact without a slot cannot be constructed, and the
/// transcoder — which cannot even name the gate — has no ordering to
/// remember. The slot is held from before the stage creates the file until
/// the value drops, so at most the gate's capacity of staged files exists at
/// once and transcodes beyond it queue.
///
/// Field order is load-bearing: `file` drops first, deleting the staged
/// file, and only then does the slot return to the gate — the "one slot per
/// staged file on disk" invariant holds exactly, with no window holding
/// capacity+1 files.
#[derive(Debug)]
pub struct StagedSource {
    file: StagedFileGuard,
    _slot: OwnedSemaphorePermit,
}

impl StagedSource {
    /// Pair a staged file with the slot its stage ran under. Only the
    /// resolution pipeline constructs this — the slot is acquired before
    /// resolution begins, never after.
    pub(crate) fn new(file: StagedFileGuard, slot: OwnedSemaphorePermit) -> Self {
        Self { file, _slot: slot }
    }

    /// The staged file's path — what ffmpeg's `-i` reads.
    pub(crate) fn path(&self) -> &std::path::Path {
        self.file.path()
    }

    /// Adopt an already-staged file, with a private one-slot gate backing
    /// its Staging Slot. Test-only — production acquires the slot from the
    /// injected or process-wide gate before resolution; the test fakes
    /// script a staged artifact by adopting a fixture path.
    #[cfg(test)]
    pub(crate) fn adopt(path: std::path::PathBuf) -> Self {
        Self::new(
            StagedFileGuard::adopt(path),
            Arc::new(tokio::sync::Semaphore::new(1))
                .try_acquire_owned()
                .expect("a fresh semaphore always yields its permit"),
        )
    }
}

/// Turns a media URL — the original provider URL carried inside the RSS
/// Enclosure (e.g. a YouTube watch URL) — into the source ffmpeg reads.
///
/// This is the seam the transcoder consumes. Implementations own the whole
/// resolution pipeline — provider dispatch, cached yt-dlp resolution, the
/// check stage behind the staging gate and the eviction-and-retry loop — so
/// the transcoder can be built and tested without yt-dlp or network.
#[async_trait]
pub trait StreamUrlResolver: Send + Sync {
    /// Resolve the source for `media_url` — a Stream URL, or a staged local
    /// file already holding its Staging Slot — failing fast when the resolved
    /// source turns out to be unreachable or unstaged (the cached resolution
    /// is then evicted so the next request re-resolves). A staged hand-back
    /// always carries its slot: whoever holds the value holds the slot until
    /// the transcode ends (issue #181).
    async fn resolve(&self, media_url: &Url) -> eyre::Result<ResolvedSource>;
}

/// The resolution pipeline's check stage, consulted with a resolved stream
/// URL.
///
/// An `Err` marks the stage failed and fails the resolution fast instead of
/// letting ffmpeg hang on a dead source. What "failed" means is the
/// implementation's call: the production [`TcpPreflight`] fails only on
/// connect timeouts — DNS errors and connection-refused are left for ffmpeg,
/// which fails on those quickly.
///
/// Two kinds of check stage slot into the same seam:
///
/// - the legacy reachability preflight ([`TcpPreflight`]) verifies the
///   source is reachable and hands back `None` — the Stream URL stands and
///   ffmpeg reads the network itself;
/// - the staging stage ([`StagingCheckStage`], the production default)
///   fetches the source to a local Staged Source and hands back its guard —
///   the artifact whose drop deletes the file, held by the resolution until
///   the transcoder adopts it. ffmpeg reads the file, with no network I/O.
///   A staging failure is treated exactly like a preflight failure: cache
///   eviction, re-resolution, retry.
#[async_trait]
pub trait CheckStage: Send + Sync {
    async fn check(&self, stream_url: &Url) -> eyre::Result<Option<StagedFileGuard>>;
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
impl CheckStage for TcpPreflight {
    async fn check(&self, stream_url: &Url) -> eyre::Result<Option<StagedFileGuard>> {
        preflight_stream_reachable(stream_url, self.connect_timeout).await?;
        // The preflight never stages: a pass leaves the Stream URL standing,
        // for ffmpeg to read over the network.
        Ok(None)
    }
}

/// Production stream-URL resolution for one media URL.
///
/// Holds the provider the media URL dispatches to (for YouTube: yt-dlp
/// resolution cached in Redis), the check stages behind the staging gate,
/// and owns the eviction-and-retry loop: when the consulted check stage
/// fails, the provider's cached resolution is evicted — the upstream CDN
/// edge may have rotated — so the next request re-resolves, and the request
/// itself fails fast.
pub struct ProviderStreamUrlResolver {
    provider: Arc<dyn MediaProvider>,
    /// The legacy check stage: the TCP reachability preflight.
    preflight: Arc<dyn CheckStage>,
    /// The staging check stage. Production wires [`StagingCheckStage`] here;
    /// the `None` fallback to the preflight remains as a safety net.
    stager: Option<Arc<dyn CheckStage>>,
    /// The staging kill switch (`STAGE_SOURCES`): off, every request takes
    /// the exact legacy path.
    staging_enabled: bool,
    /// The staging gate the staged path holds its slot from, injected to
    /// control its capacity (issue #177). `None` — production — is the
    /// process-wide gate, which startup initialized exactly once from the
    /// configured staging concurrency; it is consulted only when this
    /// resolver will stage.
    stage_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl ProviderStreamUrlResolver {
    /// Production pipeline for `media_url`: the provider the URL dispatches
    /// to, the TCP preflight with its configured timeout, and the staging
    /// check stage behind the kill switch (issue #170 — staged fetching is
    /// the production path; `STAGE_SOURCES=false` rolls back to legacy).
    pub fn for_media_url(media_url: &Url) -> Self {
        Self {
            provider: Arc::new(crate::provider::from(media_url)),
            preflight: Arc::new(TcpPreflight::with_configured_timeout()),
            stager: Some(Arc::new(super::staging::StagingCheckStage::production())),
            staging_enabled: staging_enabled_from_conf(),
            // Production capacity is the process-wide gate's; a slot is
            // acquired from it in `resolve` (via [`Self::gate`]) exactly when
            // [`Self::staging_stage`] picks the staging stage.
            stage_gate: None,
        }
    }

    /// Injection point for tests: any provider fake, any check stages, any
    /// kill-switch state, any staging gate capacity.
    #[cfg(test)]
    pub fn new(
        provider: Arc<dyn MediaProvider>,
        preflight: Arc<dyn CheckStage>,
        stager: Option<Arc<dyn CheckStage>>,
        staging_enabled: bool,
        stage_gate: Option<Arc<tokio::sync::Semaphore>>,
    ) -> Self {
        Self {
            provider,
            preflight,
            stager,
            staging_enabled,
            stage_gate,
        }
    }

    /// The one staging decision: which check stage this resolution consults.
    /// `Some` is the staging stage, and its slot is acquired before
    /// resolution begins; `None` is the legacy preflight, slotless. Staging
    /// applies when the kill switch is open *and* the provider's Stream URL
    /// is a direct media source — a manifest-based source (PeerTube
    /// playlists) can never be routed into staging, whatever the switch
    /// says. The gate and the check stage's required agreement is this
    /// single decision — a staged-but-slotless or slotted-but-preflight
    /// resolution is not representable, because both flow from the same
    /// `Option` (issue #181).
    fn staging_stage(&self) -> Option<&dyn CheckStage> {
        if !(self.staging_enabled && self.provider.streams_direct_media()) {
            return None;
        }
        match &self.stager {
            Some(stager) => Some(stager.as_ref()),
            None => {
                warn!(
                    "source staging is enabled (STAGE_SOURCES) but no staging stage is \
                     wired; taking the legacy direct-url path"
                );
                None
            }
        }
    }

    /// The gate this resolution's slot comes from: the injected one when a
    /// test handed one over, the process-wide gate otherwise.
    fn gate(&self) -> Arc<tokio::sync::Semaphore> {
        self.stage_gate
            .clone()
            .unwrap_or_else(super::staging::stage_gate)
    }
}

/// The kill switch as configured: `STAGE_SOURCES`, defaulting to on — staged
/// fetching is the production path (issue #170). Parsed opt-out: only
/// "false" (case-insensitive) returns to the legacy path, so the rollback
/// value a struggling operator types always works, and a typo can never
/// silently disable staging.
fn staging_enabled_from_conf() -> bool {
    staging_enabled_from(&conf().get(ConfName::StageSources).unwrap_or_default())
}

/// The kill-switch verdict for one raw `STAGE_SOURCES` value. Only the
/// exact rollback spellings — "false", "0", "off", "no", case-insensitive
/// and trimmed — disable staging; anything else, above all a typo of one
/// of them, keeps staging on. Staging is the production path, so the safe
/// reading of an unrecognized value is "not a rollback".
fn staging_enabled_from(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "off" | "no"
    )
}

#[async_trait]
impl StreamUrlResolver for ProviderStreamUrlResolver {
    async fn resolve(&self, media_url: &Url) -> eyre::Result<ResolvedSource> {
        // The one staging decision, made once: the slot is acquired before
        // resolution begins (issue #172's ordering, now structural — issue
        // #181), and the same decision picks the stage. On any failure the
        // slot drops with it; a successful stage returns owning it.
        let staging = self.staging_stage();
        let slot = match staging {
            Some(_) => Some(
                self.gate()
                    .acquire_owned()
                    .await
                    .expect("the staging gate is never closed"),
            ),
            None => None,
        };
        let stream_url = self.provider.get_stream_url(media_url).await?;

        // Check stage: the staging stage when a slot was taken, the legacy
        // preflight otherwise. A failure — a SYN-dropping edge on the
        // preflight, a failed stage on the stager — means ffmpeg would hang
        // or read a broken source: fail fast, and evict the cached stream
        // URL so the next request re-resolves (the upstream edge may have
        // rotated).
        let consulted = staging.unwrap_or(self.preflight.as_ref());
        match consulted.check(&stream_url).await {
            Ok(None) => Ok(ResolvedSource::StreamUrl(stream_url)),
            Ok(Some(staged)) => {
                let slot = slot.expect("the staging stage runs exactly when a slot was taken");
                Ok(ResolvedSource::StagedFile(StagedSource::new(staged, slot)))
            }
            Err(e) => {
                warn!(
                    "check stage failed for {stream_url}; \
                     evicting cached stream url for {media_url} and failing fast: {e}"
                );
                // Best-effort: a Redis failure must not mask the original
                // check-stage error.
                let _ = self.provider.evict_stream_url_cache(media_url).await;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::provider::GeneratedFeed;
    use std::path::PathBuf;
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
    /// `direct_media` is the provider's staging declaration: YouTube-like
    /// (googlevideo bytes) when true, manifest-based (PeerTube playlists)
    /// when false.
    struct ScriptedProvider {
        resolutions: Mutex<Vec<eyre::Result<Url>>>,
        direct_media: bool,
        log: Arc<ProviderLog>,
    }

    impl ScriptedProvider {
        /// A direct-media (YouTube-like) provider fake.
        fn new(resolutions: Vec<eyre::Result<Url>>) -> (Self, Arc<ProviderLog>) {
            Self::with_direct_media(resolutions, true)
        }

        /// A manifest-based (PeerTube-like) provider fake: its resolved URL
        /// is a playlist, never stageable bytes.
        fn manifest(resolutions: Vec<eyre::Result<Url>>) -> (Self, Arc<ProviderLog>) {
            Self::with_direct_media(resolutions, false)
        }

        fn with_direct_media(
            resolutions: Vec<eyre::Result<Url>>,
            direct_media: bool,
        ) -> (Self, Arc<ProviderLog>) {
            let log: Arc<ProviderLog> = Arc::default();
            (
                Self {
                    resolutions: Mutex::new(resolutions),
                    direct_media,
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

        fn streams_direct_media(&self) -> bool {
            self.direct_media
        }

        fn domain_whitelist_regexes(&self) -> Vec<regex::Regex> {
            Vec::new()
        }
    }

    /// Check-stage fake scripting `check` answers in order and recording the
    /// URLs it was consulted on — the TCP preflight and the staging stage,
    /// both without network. An empty script panics on consultation, so a
    /// test fails loudly when a stage runs that should have been skipped.
    struct ScriptedCheck {
        outcomes: Mutex<Vec<eyre::Result<Option<StagedFileGuard>>>>,
        checked: Mutex<Vec<Url>>,
    }

    impl ScriptedCheck {
        fn new(outcomes: Vec<eyre::Result<Option<StagedFileGuard>>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                checked: Mutex::new(Vec::new()),
            }
        }

        /// The URLs this stage was consulted on, in order.
        async fn checked(&self) -> Vec<Url> {
            self.checked.lock().await.clone()
        }
    }

    #[async_trait]
    impl CheckStage for ScriptedCheck {
        async fn check(&self, stream_url: &Url) -> eyre::Result<Option<StagedFileGuard>> {
            self.checked.lock().await.push(stream_url.clone());
            let mut outcomes = self.outcomes.lock().await;
            if outcomes.is_empty() {
                panic!(
                    "no scripted check outcome left — the check stage ran more times than scripted"
                );
            }
            outcomes.remove(0)
        }
    }

    /// A staged-file path value for scripting the staging stage's answer.
    fn staged_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    /// Assert a resolution handed back the expected Stream URL. The staged
    /// variant carries an owning guard, so the enum has no `PartialEq` —
    /// the Stream URL comparison is spelled out here instead.
    fn assert_stream_url(source: ResolvedSource, expected: &Url) {
        match source {
            ResolvedSource::StreamUrl(url) => assert_eq!(url, *expected),
            other => panic!("expected the Stream URL {expected}, got {other:?}"),
        }
    }

    #[test]
    fn kill_switch_reads_the_rollback_spellings_and_keeps_a_typo_on() {
        // The operator's rollback (user story 7): the common spellings of
        // "off" all disable staging — but a typo of one of them must keep
        // staging on, never silently flip the meaning.
        for off in [
            "false", "FALSE", "False", "0", "off", "OFF", "no", "NO", " false ",
        ] {
            assert!(!staging_enabled_from(off), "\"{off}\" must disable staging");
        }
        for on in [
            "true", "TRUE", "1", "yes", "on", "", "flase", "offf", "nope",
        ] {
            assert!(staging_enabled_from(on), "\"{on}\" must keep staging on");
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
        let preflight = Arc::new(ScriptedCheck::new(vec![
            Err(eyre::eyre!(
                "media source unreachable: timed out connecting to stream url"
            )),
            Ok(None),
        ]));
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            None,
            false,
            None,
        );

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
        assert_stream_url(second, &fresh);
        assert_eq!(log.resolution_calls.load(Ordering::SeqCst), 2);
        // The preflights ran against the resolved URLs — the dead cached one,
        // then the fresh one — never against the media URL being resolved.
        assert_eq!(preflight.checked().await, vec![cached, fresh]);
    }

    #[tokio::test]
    async fn resolution_failure_propagates_without_evicting() {
        // A resolution failure (yt-dlp finding nothing, say) propagates as
        // the stream error it produces today, and must NOT evict: nothing was
        // resolved, so there is no cached URL whose rotation could help.
        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let (provider, log) =
            ScriptedProvider::new(vec![Err(eyre::eyre!("unable to extract stream url"))]);
        let preflight = Arc::new(ScriptedCheck::new(Vec::new()));
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            None,
            false,
            None,
        );

        let err = resolver
            .resolve(&media)
            .await
            .expect_err("the scripted resolution failure must propagate");
        assert!(err.to_string().contains("unable to extract stream url"));
        assert_eq!(log.evictions.load(Ordering::SeqCst), 0);
        assert_eq!(log.resolution_calls.load(Ordering::SeqCst), 1);
        assert!(
            preflight.checked().await.is_empty(),
            "an unresolved URL never reaches the preflight"
        );
    }

    #[tokio::test]
    async fn kill_switch_off_takes_the_legacy_path_even_for_direct_media() {
        // The default deployment state: whatever the provider declares, with
        // the kill switch off every request takes the exact legacy path —
        // the preflight runs against the resolved Stream URL and no staged
        // artifact ever exists. The staging stage is scripted empty, so the
        // test panics if the pipeline even consults it — no staged request
        // is attempted, whatever "attempted" would cost.
        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let resolved = Url::parse("https://edge.googlevideo.com/videoplayback?x=1").unwrap();
        let (provider, log) = ScriptedProvider::new(vec![Ok(resolved.clone())]);
        let preflight = Arc::new(ScriptedCheck::new(vec![Ok(None)]));
        let stager = Arc::new(ScriptedCheck::new(Vec::new()));
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            Some(stager.clone()),
            false,
            None,
        );

        let source = resolver
            .resolve(&media)
            .await
            .expect("the legacy path must resolve");
        assert_stream_url(source, &resolved);
        assert_eq!(log.resolution_calls.load(Ordering::SeqCst), 1);
        assert_eq!(log.evictions.load(Ordering::SeqCst), 0);
        assert_eq!(preflight.checked().await, vec![resolved]);
        assert!(
            stager.checked().await.is_empty(),
            "the staging stage must not be consulted while the kill switch is off"
        );
    }

    #[tokio::test]
    async fn manifest_provider_never_routes_to_staging_regardless_of_switch() {
        // A manifest-based provider (PeerTube) resolves to a playlist, not
        // bytes — the pipeline must keep it on the legacy path even with the
        // kill switch on. The staging stage is scripted empty, so the test
        // panics if the pipeline even consults it.
        let media = Url::parse("https://framatube.org/videos/watch/abc123").unwrap();
        let resolved = Url::parse("https://framatube.org/static/hls/abc.m3u8").unwrap();
        let (provider, log) = ScriptedProvider::manifest(vec![Ok(resolved.clone())]);
        let preflight = Arc::new(ScriptedCheck::new(vec![Ok(None)]));
        let stager = Arc::new(ScriptedCheck::new(Vec::new()));
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            Some(stager.clone()),
            true,
            None,
        );

        let source = resolver
            .resolve(&media)
            .await
            .expect("a manifest provider resolves through the legacy path");
        assert_stream_url(source, &resolved);
        assert_eq!(log.evictions.load(Ordering::SeqCst), 0);
        assert_eq!(preflight.checked().await, vec![resolved]);
        assert!(
            stager.checked().await.is_empty(),
            "a manifest-based source can never be routed into staging"
        );
    }

    #[tokio::test]
    async fn the_staged_handback_carries_its_slot_until_the_source_dies() {
        // The slotted hand-back (issue #181): the resolver acquires the slot
        // before resolution begins and returns the Staged Source owning it —
        // on a capacity-1 gate the handback holds the only slot, a second
        // resolution queues while the source lives, and dropping the source
        // frees the slot for it. Slot and file travel with the value; the
        // transcoder has no ordering to remember.
        use crate::transcoder::staging::stage_gate_of;

        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let resolved = Url::parse("https://edge.googlevideo.com/videoplayback?x=1").unwrap();
        let staged = staged_path("vod2pod-test-staged-slotted.source");
        // Both resolutions resolve: the first stages and holds the slot, the
        // second runs queued on it.
        let (provider, _) = ScriptedProvider::new(vec![Ok(resolved.clone()), Ok(resolved.clone())]);
        let gate = stage_gate_of(1);
        let stager = Arc::new(ScriptedCheck::new(vec![
            Ok(Some(StagedFileGuard::adopt(staged.clone()))),
            Ok(Some(StagedFileGuard::adopt(staged.clone()))),
        ]));
        let resolver = Arc::new(ProviderStreamUrlResolver::new(
            Arc::new(provider),
            Arc::new(ScriptedCheck::new(Vec::new())),
            Some(stager),
            true,
            Some(gate.clone()),
        ));

        let first = resolver
            .resolve(&media)
            .await
            .expect("the first resolution stages and holds the gate's only slot");
        assert_eq!(
            gate.available_permits(),
            0,
            "the handed-back source must hold the gate's only slot"
        );

        // While the first source lives, a second resolution queues on the
        // empty gate: the staging stage is never consulted, no slot is free.
        let second = {
            let resolver = resolver.clone();
            let media = media.clone();
            tokio::spawn(async move { resolver.resolve(&media).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "the second resolution must queue while the first source holds its slot"
        );

        // Dropping the source releases its slot; the queued resolution
        // proceeds and stages in its turn.
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("the queued resolution must proceed once the slot is released")
            .expect("the queued resolution task must not panic");
        assert!(matches!(second, Ok(ResolvedSource::StagedFile(_))));
        drop(second);
        assert_eq!(
            gate.available_permits(),
            1,
            "every slot dies with the source carrying it"
        );
    }

    #[tokio::test]
    async fn legacy_shapes_resolve_slotless_even_with_a_gate_injected() {
        // The slot is bound to staging and nothing else (issue #181): with
        // the kill switch off, or a manifest-based provider, resolution
        // never touches the gate — even when one is injected — so two
        // resolutions on a capacity-1 gate never queue.
        use crate::transcoder::staging::stage_gate_of;

        // Kill switch off: the exact legacy path, slotless.
        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let resolved = Url::parse("https://edge.googlevideo.com/videoplayback?x=1").unwrap();
        let (provider, _) = ScriptedProvider::new(vec![Ok(resolved.clone())]);
        let gate = stage_gate_of(1);
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            Arc::new(ScriptedCheck::new(vec![Ok(None)])),
            Some(Arc::new(ScriptedCheck::new(Vec::new()))),
            false,
            Some(gate.clone()),
        );
        resolver
            .resolve(&media)
            .await
            .expect("the legacy path resolves");
        assert_eq!(
            gate.available_permits(),
            1,
            "the kill-switch path must stay slotless"
        );

        // Manifest-based provider: never staged, whatever the switch says.
        let media = Url::parse("https://framatube.org/videos/watch/abc123").unwrap();
        let playlist = Url::parse("https://framatube.org/static/hls/abc.m3u8").unwrap();
        let (provider, _) = ScriptedProvider::manifest(vec![Ok(playlist.clone())]);
        let gate = stage_gate_of(1);
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            Arc::new(ScriptedCheck::new(vec![Ok(None)])),
            Some(Arc::new(ScriptedCheck::new(Vec::new()))),
            true,
            Some(gate.clone()),
        );
        resolver
            .resolve(&media)
            .await
            .expect("a manifest provider resolves through the legacy path");
        assert_eq!(
            gate.available_permits(),
            1,
            "a manifest-based source can never take a slot"
        );
    }

    #[tokio::test]
    async fn staged_check_stage_success_hands_over_the_staged_file() {
        // Gate fully open (switch on, direct-media provider): the staging
        // stage replaces the preflight as the consulted check stage, and its
        // staged artifact — not the Stream URL — is what the pipeline hands
        // the transcoder. The preflight is scripted empty, so the test panics
        // if it runs: on a staged path ffmpeg does no network I/O.
        use crate::transcoder::staging::stage_gate_of;

        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let resolved = Url::parse("https://edge.googlevideo.com/videoplayback?x=1").unwrap();
        let staged = staged_path("vod2pod-test-staged-success.source");
        let (provider, _log) = ScriptedProvider::new(vec![Ok(resolved.clone())]);
        let preflight = Arc::new(ScriptedCheck::new(Vec::new()));
        let stager = Arc::new(ScriptedCheck::new(vec![Ok(Some(StagedFileGuard::adopt(
            staged.clone(),
        )))]));
        // The staged path resolves holding a slot (issue #181): the test
        // injects a gate of its own (issue #177) — the process-wide gate is
        // startup's, and tests never run it.
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            Some(stager.clone()),
            true,
            Some(stage_gate_of(4)),
        );

        let source = resolver
            .resolve(&media)
            .await
            .expect("a successful stage hands over its artifact");
        match source {
            ResolvedSource::StagedFile(guard) => assert_eq!(guard.path(), staged),
            other => panic!("the staged stage's artifact must be handed over: {other:?}"),
        }
        assert_eq!(stager.checked().await, vec![resolved]);
        assert!(
            preflight.checked().await.is_empty(),
            "the staged path must not run the TCP preflight"
        );
    }

    #[tokio::test]
    async fn staging_failure_evicts_and_reresolves_like_a_preflight_failure() {
        // The evict-and-retry loop must treat a staging failure exactly like
        // a preflight failure: the request fails fast, the cached resolution
        // is evicted, and the next request re-resolves — the fresh URL then
        // goes through the staging gate again.
        use crate::transcoder::staging::stage_gate_of;

        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let cached = Url::parse("https://dead.edge.googlevideo.com/videoplayback?x=1").unwrap();
        let fresh = Url::parse("https://live.edge.googlevideo.com/videoplayback?x=1").unwrap();
        let (provider, log) = ScriptedProvider::new(vec![Ok(cached.clone()), Ok(fresh.clone())]);
        let preflight = Arc::new(ScriptedCheck::new(Vec::new()));
        let staged_retry = staged_path("vod2pod-test-staged-retry.source");
        let stager = Arc::new(ScriptedCheck::new(vec![
            Err(eyre::eyre!("source stage failed: origin reset mid-chunk")),
            Ok(Some(StagedFileGuard::adopt(staged_retry.clone()))),
        ]));
        // The staged path resolves holding a slot (issue #181): a test gate
        // is injected (issue #177); the failed first attempt drops its slot
        // with its error, the second hands back owning one.
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            Some(stager.clone()),
            true,
            Some(stage_gate_of(4)),
        );

        let first = resolver.resolve(&media).await;
        assert!(
            first.is_err(),
            "a staging failure must fail the request fast, got {first:?}"
        );
        assert_eq!(log.evictions.load(Ordering::SeqCst), 1);
        assert_eq!(*log.evicted_sources.lock().await, vec![media.clone()]);

        let second = resolver
            .resolve(&media)
            .await
            .expect("after eviction the next request re-resolves and re-stages");
        match second {
            ResolvedSource::StagedFile(guard) => assert_eq!(guard.path(), staged_retry),
            other => panic!("the re-resolution must re-stage: {other:?}"),
        }
        assert_eq!(log.resolution_calls.load(Ordering::SeqCst), 2);
        // Both staging attempts ran against the URLs the provider resolved —
        // the dead cached one, then the fresh one — never against the media
        // URL being resolved.
        assert_eq!(stager.checked().await, vec![cached, fresh]);
        assert!(preflight.checked().await.is_empty());
    }

    #[tokio::test]
    async fn real_staging_failure_of_a_dead_origin_evicts_and_reresolves() {
        // The evict-and-retry loop with the REAL staging engine (issue #170):
        // the cached resolution points at an origin answering 5xx on every
        // attempt, so the stage fails its probe budget; the cached URL is
        // evicted and the next request re-resolves to a live origin, whose
        // staged file comes back byte-identical. The preflight is scripted
        // empty — on a staging path it must never run.
        use crate::test_support::staging_test_limits;
        use crate::transcoder::staging::stage_gate_of;
        use crate::transcoder::staging::test_origin::{FaultPolicy, OriginScript, ScriptedOrigin};
        use crate::transcoder::staging::{StageLimits, StagingCheckStage};
        use std::time::Duration;

        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let dead = ScriptedOrigin::start(
            1024,
            OriginScript {
                server_error: FaultPolicy::Always,
                ..OriginScript::default()
            },
        );
        let live = ScriptedOrigin::start(3 * 1024, OriginScript::default());
        let (provider, log) = ScriptedProvider::new(vec![Ok(dead.url()), Ok(live.url())]);
        let preflight = Arc::new(ScriptedCheck::new(Vec::new()));
        let temp_dir = crate::test_support::TempDir::new("staging-retry");
        let stager = Arc::new(StagingCheckStage::new(
            reqwest::Client::new(),
            StageLimits {
                stage_timeout: Duration::from_secs(60),
                ..staging_test_limits()
            },
            temp_dir.path().to_path_buf(),
        ));
        // The real staging engine resolves slot-carrying now (issue #181):
        // a test gate is injected (issue #177).
        let resolver = ProviderStreamUrlResolver::new(
            Arc::new(provider),
            preflight.clone(),
            Some(stager.clone()),
            true,
            Some(stage_gate_of(4)),
        );

        let first = resolver.resolve(&media).await;
        assert!(
            first.is_err(),
            "a dead origin must fail the stage and the request, got {first:?}"
        );
        assert_eq!(log.evictions.load(Ordering::SeqCst), 1);
        assert_eq!(*log.evicted_sources.lock().await, vec![media.clone()]);
        assert!(
            std::fs::read_dir(temp_dir.path()).unwrap().count() == 0,
            "the failed stage must leave no temp file"
        );

        let second = resolver
            .resolve(&media)
            .await
            .expect("after eviction the next request re-resolves and re-stages");
        let staged = match second {
            ResolvedSource::StagedFile(guard) => guard,
            other => panic!("the live origin must stage, got {other:?}"),
        };
        assert_eq!(
            std::fs::read(staged.path()).unwrap(),
            live.content(),
            "the staged file must be byte-identical to the origin"
        );
        assert!(preflight.checked().await.is_empty());
        // The handed-over file dies with the guard carrying it.
        drop(staged);
    }

    #[tokio::test]
    async fn staging_enabled_without_a_wired_stager_falls_back_to_preflight() {
        // The safety net behind the gate: with the kill switch on and a
        // direct-media provider but no staging stage wired, the request
        // still completes on the legacy path instead of erroring.
        let media = Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap();
        let resolved = Url::parse("https://edge.googlevideo.com/videoplayback?x=1").unwrap();
        let (provider, log) = ScriptedProvider::new(vec![Ok(resolved.clone())]);
        let preflight = Arc::new(ScriptedCheck::new(vec![Ok(None)]));
        let resolver =
            ProviderStreamUrlResolver::new(Arc::new(provider), preflight.clone(), None, true, None);

        let source = resolver
            .resolve(&media)
            .await
            .expect("the fallback keeps the request working");
        assert_stream_url(source, &resolved);
        assert_eq!(log.evictions.load(Ordering::SeqCst), 0);
        assert_eq!(preflight.checked().await, vec![resolved]);
    }

    #[test]
    fn kill_switch_defaults_on_and_only_false_opts_out() {
        // Staged fetching is the production path: unset and mistyped values
        // keep staging on; the one rollback value is "false", any casing.
        assert!(staging_enabled_from(""));
        assert!(staging_enabled_from("True"));
        assert!(staging_enabled_from("true"));
        assert!(staging_enabled_from("TRUE"));
        assert!(staging_enabled_from("1"));
        assert!(staging_enabled_from("on"));
        assert!(staging_enabled_from("typo"));
        assert!(!staging_enabled_from("false"));
        assert!(!staging_enabled_from("False"));
        assert!(!staging_enabled_from("FALSE"));
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
