//! Test-only helpers shared across the modules' test suites.

use std::path::{Path, PathBuf};

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

/// Pin `REDIS_ADDRESS`/`REDIS_PORT` to a dead port for one test, restoring
/// the ambient values on drop — even when the test panics mid-await. The
/// integration binaries keep their own copy of this shape: `pub(crate)` is
/// invisible across the tests/ crate boundary.
pub(crate) struct DeadRedisEnvPin {
    prev: (Option<String>, Option<String>),
}

impl DeadRedisEnvPin {
    pub(crate) fn new() -> Self {
        let prev = (
            std::env::var("REDIS_ADDRESS").ok(),
            std::env::var("REDIS_PORT").ok(),
        );
        std::env::set_var("REDIS_ADDRESS", "127.0.0.1");
        // Port 1 hosts no Redis in any environment.
        std::env::set_var("REDIS_PORT", "1");
        Self { prev }
    }
}

impl Drop for DeadRedisEnvPin {
    fn drop(&mut self) {
        match self.prev.0.take() {
            Some(v) => std::env::set_var("REDIS_ADDRESS", v),
            None => std::env::remove_var("REDIS_ADDRESS"),
        }
        match self.prev.1.take() {
            Some(v) => std::env::set_var("REDIS_PORT", v),
            None => std::env::remove_var("REDIS_PORT"),
        }
    }
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
