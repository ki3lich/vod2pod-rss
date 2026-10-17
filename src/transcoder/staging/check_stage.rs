//! The production staging check stage: the [`CheckStage`] that performs the
//! Source Stage for a resolved Stream URL and hands back the Staged Source —
//! the wiring that makes staged fetching the serving path (issue #170).
//!
//! Owns the temp-file lifecycle of a staged source: the file is written under
//! a unique name, deleted when the staging itself fails, and handed to the
//! transcoder on success — which deletes it when the transcode ends.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::{debug, info, warn};
use reqwest::Client;
use tokio::sync::Semaphore;

use crate::configs::{
    conf_u64, ConfName, DEFAULT_STAGE_CHUNK_CONCURRENCY, DEFAULT_STAGE_CHUNK_SIZE_BYTES,
    DEFAULT_STAGE_CONCURRENCY, DEFAULT_STAGE_REQUEST_TIMEOUT_SECS,
    DEFAULT_STAGE_TOTAL_TIMEOUT_SECS,
};
use crate::transcoder::stream_url::CheckStage;

use super::chunk_plan::MAX_CHUNK_ATTEMPTS;
use super::fetcher::{SourceStager, StageLimits};

/// Temp-file name prefix for staged sources. Only this process's staged
/// sources use the prefix (the deployed pod gives the process its own
/// ephemeral storage). The startup sweep removes exactly these.
pub const STAGED_FILE_PREFIX: &str = "vod2pod-stage-";

/// Pause between a chunk's fresh-connection retries. The measured edge
/// resets long reads mid-stream; a short pause is enough to get a fresh
/// connection without stretching the stage.
const STAGE_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// One shared connection pool for all staged fetches: `Client::clone` is an
/// Arc over the pool, so per-request [`StagingCheckStage`]s reuse connections.
fn shared_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(Client::new).clone()
}

/// The staging gate with room for `concurrency` stages at once — one slot
/// per staged file on disk. A configured zero (or anything that would leave
/// no slot) must never deadlock the pipeline, so the floor is one.
///
/// The constructor startup and tests share: startup builds the process-wide
/// gate with it, tests build gates of their own capacity and inject them
/// into the resolver (issue #177).
pub fn stage_gate_of(concurrency: u64) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(concurrency.max(1) as usize))
}

/// The process-wide staging gate, initialized exactly once by
/// [`init_stage_gate`] and handed out by [`stage_gate`] ever after.
static STAGE_GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Initialize the process-wide staging gate from the configured staging
/// concurrency (`STAGE_CONCURRENCY`). Runs exactly once, at startup beside
/// the Startup Sweep and before the server accepts its first request —
/// capacity has one owner and one place (issue #177). A second
/// initialization is a wiring bug and fails loudly.
pub fn init_stage_gate() {
    let concurrency = conf_u64(ConfName::StageConcurrency, DEFAULT_STAGE_CONCURRENCY);
    if STAGE_GATE.set(stage_gate_of(concurrency)).is_err() {
        panic!(
            "the staging gate is already initialized: it is initialized exactly once, at startup"
        );
    }
}

/// The process-wide staging gate: one permit per staged file on disk, its
/// capacity the staging concurrency the startup initialization configured.
///
/// A staged transcode holds its slot from before its stage creates the temp
/// file until the transcode ends and the file is deleted — so simultaneous
/// subscribers can stage at most this many sources in parallel and hold at
/// most this many staged files on disk; everyone else queues. The capacity
/// is fixed at startup: retuning takes a restart.
///
/// # Panics
/// When startup never initialized the gate: no transcode may stage before
/// [`init_stage_gate`] ran.
pub fn stage_gate() -> Arc<Semaphore> {
    STAGE_GATE.get().cloned().unwrap_or_else(|| {
        panic!(
            "the staging gate is not initialized: init_stage_gate() must run at startup, \
             before the server accepts requests"
        )
    })
}

/// The production check stage: performs the Source Stage for the resolved
/// Stream URL — parallel bounded ranged GETs into a uniquely-named temp file —
/// and hands back the Staged Source in place of the URL.
///
/// A staging failure (dead origin after its retries, a total-stage timeout)
/// fails the resolution, which evicts the cached resolution and fails the
/// request fast — exactly like a preflight failure. The temp file never
/// outlives a failure.
pub struct StagingCheckStage {
    client: Client,
    limits: StageLimits,
    temp_dir: PathBuf,
}

impl StagingCheckStage {
    /// The production stage: the configured staging knobs (the measured
    /// limits when unset, issue #172) and the system temp dir.
    pub fn production() -> Self {
        Self::new(
            shared_client(),
            StageLimits {
                chunk_size: conf_u64(
                    ConfName::StageChunkSizeBytes,
                    DEFAULT_STAGE_CHUNK_SIZE_BYTES,
                ),
                // The chunk window over the plan: the measured
                // full-aggregate speed (issue #166), configurable so an
                // operator can tune it for their network. A configured zero
                // must never stall the plan window — the floor is one, as
                // for the staging gate.
                concurrency: conf_u64(
                    ConfName::StageChunkConcurrency,
                    DEFAULT_STAGE_CHUNK_CONCURRENCY,
                )
                .max(1) as usize,
                max_attempts: MAX_CHUNK_ATTEMPTS,
                attempt_timeout: Duration::from_secs(conf_u64(
                    ConfName::StageRequestTimeoutSeconds,
                    DEFAULT_STAGE_REQUEST_TIMEOUT_SECS,
                )),
                stage_timeout: Duration::from_secs(conf_u64(
                    ConfName::StageTotalTimeoutSeconds,
                    DEFAULT_STAGE_TOTAL_TIMEOUT_SECS,
                )),
                retry_backoff: STAGE_RETRY_BACKOFF,
            },
            std::env::temp_dir(),
        )
    }

    /// A stage with explicit parts — the injection point tests use to shrink
    /// limits and isolate the temp dir.
    pub(crate) fn new(client: Client, limits: StageLimits, temp_dir: PathBuf) -> Self {
        Self {
            client,
            limits,
            temp_dir,
        }
    }
}

#[async_trait]
impl CheckStage for StagingCheckStage {
    async fn check(&self, stream_url: &reqwest::Url) -> eyre::Result<Option<StagedFileGuard>> {
        let guard = StagedFileGuard::reserve_in(&self.temp_dir);
        let stager = SourceStager::new(self.client.clone(), self.limits);
        let started = Instant::now();
        let report = match tokio::time::timeout(
            self.limits.stage_timeout,
            stager.stage_to(stream_url, guard.path()),
        )
        .await
        {
            Ok(Ok(report)) => report,
            Ok(Err(e)) => {
                warn!("source stage failed for {stream_url}; the staged file is discarded: {e}");
                return Err(e);
            }
            Err(_elapsed) => {
                let message = format!(
                    "source stage timed out after {:?} (the total stage deadline)",
                    self.limits.stage_timeout
                );
                warn!("source stage timed out for {stream_url}; the staged file is discarded");
                return Err(eyre::eyre!(message));
            }
        };
        let elapsed = started.elapsed();
        let secs = elapsed.as_secs_f64().max(0.001);
        info!(
            "staged source for {stream_url}: {} bytes in {:.1?} ({:.1} MB/s, {} chunk \
             retries) at {}",
            report.bytes,
            elapsed,
            report.bytes as f64 / (1024.0 * 1024.0) / secs,
            report.chunk_retries,
            guard.path().display()
        );
        // Handover: the guard itself is the artifact. It crosses the
        // resolution seam to the transcoder, which holds it through the
        // transcode — its drop deletes the file. There is no unowned
        // window: a resolution or construction that fails, or a dropped
        // future, drops the guard with it and deletes the file.
        Ok(Some(guard))
    }
}

/// Owns one staged-source temp file and deletes it when dropped.
///
/// The staging stage reserves the file and stages into it; on success the
/// guard itself crosses the resolution seam to the transcoder, which holds
/// it for the transcode's whole life. The file is therefore deleted exactly
/// when the transcode ends (completed, failed, or the client gone) — and on
/// any failure or dropped construction the guard still holds the file, so
/// deleting it is the drop.
#[derive(Debug)]
pub struct StagedFileGuard {
    path: Option<PathBuf>,
}

impl StagedFileGuard {
    /// Reserve a uniquely-named staged-file path under `temp_dir`.
    pub(crate) fn reserve_in(temp_dir: &Path) -> Self {
        Self {
            path: Some(temp_dir.join(format!("{STAGED_FILE_PREFIX}{}", uuid::Uuid::new_v4()))),
        }
    }

    /// Adopt an already-staged file: from here on deleting it is this
    /// guard's job. Test-only — production stages reserve their own guard;
    /// the test fakes script a staged artifact by adopting a fixture path.
    #[cfg(test)]
    pub fn adopt(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// The reserved path — the staged fetch writes here, and the
    /// resolution seam carries the guard to the transcoder.
    pub(crate) fn path(&self) -> &Path {
        self.path
            .as_ref()
            .expect("path is only taken when the guard drops")
    }
}

impl Drop for StagedFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            match std::fs::remove_file(&path) {
                Ok(()) => debug!("removed staged source {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!("failed to remove staged source {}: {e}", path.display())
                }
            }
        }
    }
}

/// Remove the staged-source temp files a crashed predecessor left behind,
/// returning how many were removed.
///
/// Only files under [`STAGED_FILE_PREFIX`] are touched — the temp dir is
/// shared with whatever else runs on the machine. Like the prefix itself,
/// the sweep assumes this process's staged sources are its alone (the
/// deployed pod gives the process its own ephemeral storage): a second
/// instance sharing the temp dir would see the other's live files swept.
/// The sweep runs exactly once, at process startup before the server
/// accepts requests: at that point no staged file of this process exists,
/// so nothing in flight can be swept, and the guard-owned deletion covers
/// every file created after it.
pub fn sweep_stale_staged_sources_in(dir: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGED_FILE_PREFIX)
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            // A file vanishing between the listing and the unlink is fine —
            // its owner deleted it first. Anything else is shouted about but
            // never fatal: sweeping is best-effort hygiene, not a startup
            // precondition.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "startup sweep could not remove {}: {e}",
                entry.path().display()
            ),
        }
    }
    Ok(removed)
}

/// The production startup sweep: the system temp dir this process stages
/// into, logged, best-effort.
pub fn sweep_stale_staged_sources() {
    match sweep_stale_staged_sources_in(&std::env::temp_dir()) {
        Ok(0) => {}
        Ok(n) => info!("startup sweep removed {n} staged source(s) left by a previous process"),
        Err(e) => warn!("startup sweep of staged sources failed: {e}"),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::transcoder::staging::test_origin::{OriginScript, ScriptedOrigin};
    use std::io::Write as _;

    const KIB: u64 = 1024;

    /// Tight, fast limits for check-stage tests, staged into a fresh
    /// isolated temp dir.
    struct TestStage {
        stage: StagingCheckStage,
        temp_dir: crate::test_support::TempDir,
    }

    impl TestStage {
        fn new(script: OriginScript, content_len: usize) -> (Self, ScriptedOrigin) {
            let origin = ScriptedOrigin::start(content_len, script);
            let limits = crate::test_support::staging_test_limits();
            let temp_dir = crate::test_support::TempDir::new("check-stage-test");
            (
                Self {
                    stage: StagingCheckStage::new(
                        Client::new(),
                        limits,
                        temp_dir.path().to_path_buf(),
                    ),
                    temp_dir,
                },
                origin,
            )
        }

        fn staged_files(&self) -> Vec<PathBuf> {
            std::fs::read_dir(self.temp_dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .collect()
        }
    }

    #[tokio::test]
    async fn stages_the_source_and_hands_back_a_staged_file() {
        // The production stage against the scripted origin: check() hands
        // back the staged file, byte-identical to the origin's content, and
        // the temp dir holds exactly that one file.
        let (test, origin) = TestStage::new(OriginScript::default(), 5 * KIB as usize);

        let staged = test
            .stage
            .check(&origin.url())
            .await
            .expect("staging a ranged origin must succeed")
            .expect("the staging stage hands back a file");
        assert_eq!(std::fs::read(staged.path()).unwrap(), origin.content());
        assert_eq!(
            test.staged_files(),
            vec![staged.path().to_path_buf()],
            "exactly the handed-back file remains"
        );

        // The transcoder holds this guard through the transcode; its drop
        // closes the lifecycle.
        drop(staged);
        assert!(test.staged_files().is_empty());
    }

    #[tokio::test]
    async fn a_failed_stage_leaves_no_temp_file() {
        // A dead origin (5xx on every attempt) fails the stage fast — and
        // the temp file the stage had already created is deleted with it.
        let (test, origin) = TestStage::new(
            OriginScript {
                server_error: crate::transcoder::staging::test_origin::FaultPolicy::Always,
                ..OriginScript::default()
            },
            5 * KIB as usize,
        );

        let err = test
            .stage
            .check(&origin.url())
            .await
            .expect_err("a permanently failing origin must fail the stage");
        assert!(
            err.to_string().contains("503"),
            "the error names the origin's answer: {err}"
        );
        assert!(
            test.staged_files().is_empty(),
            "a failed stage must not leave its temp file behind"
        );
    }

    #[tokio::test]
    async fn a_range_ignoring_origin_stages_via_the_fallback_and_leaves_no_strays() {
        // Rung 2 end-to-end at the serving seam: the check stage hands back
        // one byte-identical file staged via the open-ended fallback, and
        // the temp dir holds exactly that file until the transcoder's guard
        // deletes it.
        let (test, origin) = TestStage::new(
            OriginScript {
                ignore_range: true,
                ..OriginScript::default()
            },
            5 * KIB as usize,
        );

        let staged = test
            .stage
            .check(&origin.url())
            .await
            .expect("a range-ignoring origin must stage via the open-ended fallback")
            .expect("the staging stage hands back a file");
        assert_eq!(std::fs::read(staged.path()).unwrap(), origin.content());
        assert_eq!(
            test.staged_files(),
            vec![staged.path().to_path_buf()],
            "exactly the handed-back file remains"
        );
        drop(staged);
        assert!(test.staged_files().is_empty());
    }

    #[tokio::test]
    async fn an_origin_dropping_bodies_fails_fast_without_leaking_the_temp_file() {
        // The measured edge resets long reads: when every attempt drops
        // mid-body, the stage fails inside its per-request budgets — never
        // an unbounded hang — and the partial file is discarded with it.
        // The drop fires on the ranged probe (the first body-serving
        // request), so this is the ranged path's fast failure — the error
        // the resolution pipeline surfaces to the podcatcher as a clean
        // transcode error.
        let (test, origin) = TestStage::new(
            OriginScript {
                drop_mid_body: crate::transcoder::staging::test_origin::FaultPolicy::Always,
                ..OriginScript::default()
            },
            5 * KIB as usize,
        );

        let started = std::time::Instant::now();
        let err = test
            .stage
            .check(&origin.url())
            .await
            .expect_err("a drop on every attempt must fail the stage");
        assert!(
            err.to_string().contains("3 of 3"),
            "the error names the exhausted attempt budget: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the stage must fail within its attempt budgets, took {:?}",
            started.elapsed()
        );
        assert!(
            test.staged_files().is_empty(),
            "a failed stage must not leave its temp file behind"
        );
    }

    #[tokio::test]
    async fn a_lying_content_range_fails_the_stage_without_leaking_the_temp_file() {
        // A Content-Range total larger than the body actually served: the
        // plan extends past the real content and the stage fails naming the
        // contradiction — never producing a padded file, never leaving the
        // partial one behind.
        let (test, origin) = TestStage::new(
            OriginScript {
                lie_total: Some(50 * KIB),
                ..OriginScript::default()
            },
            5 * KIB as usize,
        );

        let err = test
            .stage
            .check(&origin.url())
            .await
            .expect_err("a lying total must fail the stage");
        assert!(
            err.to_string().contains("416"),
            "the error names the contradiction: {err}"
        );
        assert!(
            test.staged_files().is_empty(),
            "a failed stage must not leave its temp file behind"
        );
    }

    #[tokio::test]
    async fn a_timed_out_stage_leaves_no_temp_file() {
        // The total-stage timeout bounds staging: when it fires, the partial
        // file is discarded with the attempt.
        let (mut test, origin) = TestStage::new(OriginScript::default(), 5 * KIB as usize);
        test.stage.limits.stage_timeout = Duration::from_nanos(1);

        let err = test
            .stage
            .check(&origin.url())
            .await
            .expect_err("a nanosecond budget must time the stage out");
        assert!(
            err.to_string().contains("timed out"),
            "the error names the timeout: {err}"
        );
        assert!(
            test.staged_files().is_empty(),
            "a timed-out stage must not leave its temp file behind"
        );
    }

    #[test]
    fn guard_drop_deletes_the_staged_file() {
        // The guard owns the file: dropping it deletes, whatever state the
        // file is in (created or not).
        let dir = crate::test_support::TempDir::new("guard-test");

        let guard = StagedFileGuard::reserve_in(dir.path());
        let path = guard.path().to_path_buf();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        drop(guard);
        assert!(!path.exists(), "a dropped guard deletes its file");

        // A reserved-but-never-created path drops silently.
        drop(StagedFileGuard::reserve_in(dir.path()));
    }

    /// The opt-in live probe against real googlevideo (issue #166/#170):
    /// resolves a real video through the production pipeline (yt-dlp + the
    /// staging check stage) and reports the stage's size and rate, mirroring
    /// the investigation loop that measured the throttle regime.
    ///
    /// Needs the `yt-dlp` binary, a reachable Redis (the resolution cache —
    /// `make start-deps` brings one up), network access, and disk for one
    /// source file. Run in one command:
    ///
    /// `cargo test live_probe -- --ignored --nocapture`
    ///
    /// Point it at another video with `VOD2POD_LIVE_PROBE_URL`.
    #[tokio::test]
    #[ignore = "live probe: needs yt-dlp, Redis and network; run with `cargo test live_probe -- --ignored --nocapture`"]
    async fn live_probe_resolves_and_stages_a_real_source() {
        use crate::transcoder::stream_url::{
            ProviderStreamUrlResolver, ResolvedSource, StreamUrlResolver,
        };

        let media_url = std::env::var("VOD2POD_LIVE_PROBE_URL")
            .unwrap_or_else(|_| "https://www.youtube.com/watch?v=UMO52N2vfk0".to_string());
        let media = reqwest::Url::parse(&media_url).expect("VOD2POD_LIVE_PROBE_URL must be a URL");

        println!("resolving {media} through the production pipeline");
        let resolver = ProviderStreamUrlResolver::for_media_url(&media);
        let started = Instant::now();
        let staged = match resolver.resolve(&media).await {
            Ok(ResolvedSource::StagedFile(guard)) => guard,
            Ok(source) => {
                println!("resolved to a legacy artifact (staging skipped): {source:?}");
                return;
            }
            Err(e) => panic!(
                "resolution failed (is yt-dlp installed? is Redis up? is there network?): {e}"
            ),
        };
        let elapsed = started.elapsed();
        let size = std::fs::metadata(staged.path())
            .expect("the staged file must exist")
            .len();
        let secs = elapsed.as_secs_f64().max(0.001);
        println!(
            "staged {size} bytes in {elapsed:.1?} ({:.1} MB/s, bounded ranged GETs) from {media}",
            size as f64 / (1024.0 * 1024.0) / secs
        );
        // The probe is done with the file the moment it has reported.
        drop(staged);
    }

    #[test]
    fn startup_sweep_removes_stale_staged_files_and_keeps_everything_else() {
        // A crashed predecessor leaves its staged temp files behind; the
        // startup sweep removes exactly those — files under the staged
        // prefix — and nothing else living in the temp dir. Files created
        // after the sweep — live staged sources of this process's in-flight
        // transcodes — are never touched: the sweep runs once, before the
        // server accepts its first request, and never revisits the dir.
        let dir = crate::test_support::TempDir::new("sweep");
        let stale = dir
            .path()
            .join(format!("{STAGED_FILE_PREFIX}crashed-predecessor"));
        std::fs::write(&stale, b"left by a crashed process").unwrap();
        let unrelated = dir.path().join("some-other-tools-file.tmp");
        std::fs::write(&unrelated, b"not ours").unwrap();

        let removed = sweep_stale_staged_sources_in(dir.path()).expect("the sweep must scan");
        assert_eq!(
            removed, 1,
            "exactly the planted stale staged file is removed"
        );
        assert!(!stale.exists());
        assert!(unrelated.exists(), "files outside the staged prefix stay");

        // This process's own staged sources appear after the sweep and stay.
        let live = StagedFileGuard::reserve_in(dir.path());
        std::fs::write(live.path(), b"in-flight transcode's staged source").unwrap();
        assert!(live.path().exists(), "the live staged file is not swept");
    }

    #[test]
    fn staging_knobs_default_to_the_measured_limits_and_are_configurable() {
        // Env is process-global and tests run in parallel: hold the lock
        // for the whole body.
        let _env = crate::test_support::env_lock();

        // Unset: the served path runs with the measured staging limits
        // (issue #166) — 8 MiB chunks the edge serves at full speed, a 60 s
        // per-request deadline, a 240 s total stage deadline, and the
        // measured 4 chunks in flight per stage.
        let stage = StagingCheckStage::production();
        assert_eq!(stage.limits.chunk_size, 8 * 1024 * 1024);
        assert_eq!(stage.limits.attempt_timeout, Duration::from_secs(60));
        assert_eq!(stage.limits.stage_timeout, Duration::from_secs(240));
        assert_eq!(stage.limits.concurrency, 4);

        // Set: an operator tunes for their network without code changes.
        // STAGE_CONCURRENCY is deliberately not exercised here — the env is
        // process-global, and the gate reads it exactly once, in its startup
        // initialization (issue #177); its capacity floor is pinned by the
        // gate capacity test, queueing by the injected-gate load test.
        std::env::set_var("STAGE_CHUNK_SIZE_BYTES", "1048576");
        std::env::set_var("STAGE_TOTAL_TIMEOUT_SECONDS", "30");
        std::env::set_var("STAGE_REQUEST_TIMEOUT_SECONDS", "5");
        std::env::set_var("STAGE_CHUNK_CONCURRENCY", "2");
        let tuned = StagingCheckStage::production();
        assert_eq!(tuned.limits.chunk_size, 1024 * 1024);
        assert_eq!(tuned.limits.stage_timeout, Duration::from_secs(30));
        assert_eq!(tuned.limits.attempt_timeout, Duration::from_secs(5));
        assert_eq!(tuned.limits.concurrency, 2);

        // A zero must never stall the chunk window: the floor is one, as
        // for the staging gate.
        std::env::set_var("STAGE_CHUNK_CONCURRENCY", "0");
        assert_eq!(StagingCheckStage::production().limits.concurrency, 1);

        // Leave the process env as it was found.
        for var in [
            "STAGE_CHUNK_SIZE_BYTES",
            "STAGE_TOTAL_TIMEOUT_SECONDS",
            "STAGE_REQUEST_TIMEOUT_SECONDS",
            "STAGE_CHUNK_CONCURRENCY",
        ] {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn stage_gate_capacity_follows_the_requested_concurrency_clamped_to_one() {
        // The staging slot count is the requested staging concurrency —
        // one slot per staged file on disk. A zero (or any way to end up
        // with none) must never deadlock the pipeline: the floor is one
        // slot. The constructor is shared: startup builds the process-wide
        // gate with it, tests build gates of their own capacity to inject.
        assert_eq!(stage_gate_of(4).available_permits(), 4);
        assert_eq!(stage_gate_of(9).available_permits(), 9);
        assert_eq!(stage_gate_of(0).available_permits(), 1);
    }

    #[test]
    #[should_panic(expected = "already initialized")]
    fn a_second_gate_initialization_fails_loudly() {
        // The gate is initialized exactly once, at startup (issue #177):
        // capacity has one owner and one place, and a second initialization
        // is a wiring bug that must fail loudly. Whether or not another
        // test got here first, this test's own second initialization must
        // panic.
        let _ = std::panic::catch_unwind(init_stage_gate);
        init_stage_gate();
    }
}
