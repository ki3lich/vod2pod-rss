//! Source staging: turning a resolved Stream URL into a local, byte-verified
//! source file via parallel bounded ranged GETs, so ffmpeg reads a file
//! instead of crawling a throttled CDN edge.
//!
//! Three pieces:
//!
//! - [`chunk_plan`] is the pure brain: the ordered chunk plan, the
//!   concurrency window over it, and the retry-vs-fail verdict for every
//!   chunk outcome. No I/O — exhaustively testable (prior art: the stdout
//!   pump's decision functions).
//! - [`fetcher`] is the executor: one HTTP attempt per observation, per-chunk
//!   timeout and fresh-connection retries, total size learned from the first
//!   chunk's Content-Range (that request doubling as the reachability
//!   probe), and the assembled file verified against the origin total.
//!   Everything is proven against a scripted test origin.
//! - [`check_stage`] is the production wiring (issue #170): the check stage
//!   the resolution pipeline consults, and the temp-file lifecycle of a
//!   staged source.

mod chunk_plan;
mod fetcher;

pub mod check_stage;

// Re-exports: the seam's public surface — everything outside this module
// consumes the staging engine through the root, never by walking into a
// submodule.
pub use check_stage::{stage_gate, sweep_stale_staged_sources, StagedFileGuard, StagingCheckStage};

// Test-only knobs, shared across the modules' test suites.
#[cfg(test)]
pub use check_stage::STAGED_FILE_PREFIX;
#[cfg(test)]
pub use chunk_plan::MAX_CHUNK_ATTEMPTS;
#[cfg(test)]
pub use fetcher::StageLimits;

#[cfg(test)]
pub(crate) mod test_origin;
