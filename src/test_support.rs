//! Test-only helpers shared across the modules' test suites.

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use crate::provider::{GeneratedFeed, MediaProvider};

/// A unique temp directory, removed when the guard drops — the sandbox a
/// test's staged files live in, isolated from parallel tests.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// Create a uniquely-named directory under the system temp dir. `tag`
    /// names the test concern, e.g. `staging-retry`.
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("vod2pod-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Serialize tests that mutate process env vars. Env is process-global while
/// cargo runs a binary's tests in parallel threads, so any test that
/// `set_var`s must hold this lock for its whole body.
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Pin one environment variable to a value for one test, restoring the
/// ambient value on drop — even when the test panics mid-await. Hold the
/// env lock for the pin's whole life: env is process-global while cargo
/// runs a binary's tests in parallel threads. The integration binaries
/// under tests/ keep their own copies of this shape — `pub(crate)` is
/// invisible across the tests/ crate boundary.
pub(crate) struct EnvPin {
    name: &'static str,
    prev: Option<String>,
}

impl EnvPin {
    pub(crate) fn set(name: &'static str, value: &str) -> Self {
        let prev = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, prev }
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.name, v),
            None => std::env::remove_var(self.name),
        }
    }
}

/// A stub `yt-dlp` installed on `PATH` that records the argv it was handed
/// and answers with a fixed stdout, stderr and exit code — the spawn seam,
/// where the wrapper's argv and its account of a failure are observable
/// without YouTube. Hold [`env_lock`] for the stub's whole life: it mutates
/// `PATH`.
#[cfg(unix)]
pub(crate) struct StubYtDlp {
    _dir: TempDir,
    args_file: PathBuf,
    _path: EnvPin,
}

#[cfg(unix)]
impl StubYtDlp {
    /// Install a stub that prints `stdout`, prints `stderr` on stderr and
    /// exits with `exit_code`.
    pub(crate) fn install(tag: &str, stdout: &str, stderr: &str, exit_code: i32) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new(&format!("yt-dlp-stub-{tag}"));
        let args_file = dir.path().join("argv");
        let stdout_file = dir.path().join("stdout");
        let stderr_file = dir.path().join("stderr");
        std::fs::write(&stdout_file, stdout).unwrap();
        std::fs::write(&stderr_file, stderr).unwrap();

        // every interpolated path is a generated UUID name, so single-quoting
        // it in the script is safe
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\ncat '{}' >&2\nexit {}\n",
            args_file.display(),
            stdout_file.display(),
            stderr_file.display(),
            exit_code
        );
        let program = dir.path().join("yt-dlp");
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let path = EnvPin::set("PATH", &path);

        Self {
            _dir: dir,
            args_file,
            _path: path,
        }
    }

    /// The argv the stub was invoked with. A missing record means the stub
    /// never ran, so it panics rather than passing off an empty invocation.
    pub(crate) fn recorded_args(&self) -> Vec<String> {
        std::fs::read_to_string(&self.args_file)
            .expect("the stub must record the argv it was handed")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// Provider fake whose Stream URL is the media bytes themselves — a
/// YouTube-like direct-media source resolved to a scripted origin. No
/// yt-dlp, no Redis. Its whitelist is never consulted — the transcoder
/// consumes only resolution, and the transcode route's guard dispatches the
/// provider from the media URL, not the resolver's — so it is empty.
pub(crate) struct OriginProvider(pub(crate) Url);

#[async_trait]
impl MediaProvider for OriginProvider {
    async fn generate_rss_feed(&self, _channel_url: Url) -> eyre::Result<GeneratedFeed> {
        panic!("staged e2e never converts feeds")
    }

    async fn get_stream_url(&self, _media_url: &Url) -> eyre::Result<Url> {
        Ok(self.0.clone())
    }

    fn streams_direct_media(&self) -> bool {
        true
    }

    fn domain_whitelist_regexes(&self) -> Vec<Regex> {
        Vec::new()
    }
}

/// A small real mp3 fixture generated with ffmpeg's lavfi tone generator —
/// the source a scripted origin serves and transcodes re-encode. The fixture
/// dir is removed when the test ends.
pub(crate) struct MediaFixture {
    _dir: TempDir,
    pub(crate) bytes: Vec<u8>,
}

impl MediaFixture {
    pub(crate) fn new(tag: &str, duration_secs: u32) -> Self {
        let dir = TempDir::new(&format!("e2e-{tag}"));
        let fixture = dir.path().join("source.mp3");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={duration_secs}"),
                "-acodec",
                "libmp3lame",
                "-ab",
                "64k",
                fixture.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg must be available to run this test");
        assert!(generated.success(), "failed to generate test fixture");
        let bytes = std::fs::read(&fixture).unwrap();
        Self { _dir: dir, bytes }
    }
}

/// A real 16:9 still — the shape a YouTube thumbnail has and the artwork
/// composer must turn square. Generated by the ffmpeg `make test` provisions.
pub(crate) fn still_16_9() -> Vec<u8> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1280x720",
            "-frames:v",
            "1",
            "-c:v",
            "mjpeg",
            "-f",
            "image2pipe",
            "pipe:1",
        ])
        .output()
        .expect("ffmpeg must be available to run this test");
    assert!(output.status.success(), "failed to generate the test still");
    output.stdout
}

/// A YouTube video id no earlier run can have cached: the artwork cache
/// outlives one test run, so a fixed id would make the second run serve cache
/// and skip the fetch a test asserts on.
pub(crate) fn uncached_video_id() -> String {
    format!("test{}", uuid::Uuid::new_v4().simple())
}

/// Width and height from a JPEG's SOF marker — the pixels a player would
/// decode, read independently of the encoder that wrote them.
pub(crate) fn jpeg_dimensions(jpeg: &[u8]) -> (u32, u32) {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "not a JPEG");
    let mut offset = 2;
    while offset + 9 < jpeg.len() {
        if jpeg[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = jpeg[offset + 1];
        let length = u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]) as usize;
        // every SOFn frame header carries height then width
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let height = u16::from_be_bytes([jpeg[offset + 5], jpeg[offset + 6]]) as u32;
            let width = u16::from_be_bytes([jpeg[offset + 7], jpeg[offset + 8]]) as u32;
            return (width, height);
        }
        offset += 2 + length;
    }
    panic!("no SOF marker in the JPEG");
}

/// Fast-but-real staging limits for tests outside the fetcher's own suite:
/// 1 KiB chunks, 2 in flight, tight timeouts, near-zero backoff. Tests
/// override the fields their scenario cares about with struct-update
/// syntax.
pub(crate) fn staging_test_limits() -> crate::transcoder::staging::StageLimits {
    use crate::transcoder::staging::{StageLimits, MAX_CHUNK_ATTEMPTS};
    use std::time::Duration;

    StageLimits {
        chunk_size: 1024,
        concurrency: 2,
        max_attempts: MAX_CHUNK_ATTEMPTS,
        attempt_timeout: Duration::from_secs(5),
        stage_timeout: Duration::from_secs(30),
        retry_backoff: Duration::from_millis(10),
    }
}
