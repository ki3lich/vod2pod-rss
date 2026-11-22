//! The yt-dlp invocation wrapper shared by every provider: one place spawns
//! the process, enforces the timeout, and parses the output, so provider
//! implementations cannot drift apart.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use log::{debug, warn};
use reqwest::Url;
use tokio::process::Command;

/// How long a yt-dlp invocation may run before it is considered hung.
pub(super) const TIMEOUT: Duration = Duration::from_secs(120);

/// Why a yt-dlp invocation failed before producing output.
#[derive(Debug)]
pub(super) enum YtDlpError {
    /// yt-dlp did not finish within [`TIMEOUT`].
    Timeout,
    /// yt-dlp could not be spawned.
    Spawn(std::io::Error),
}

impl fmt::Display for YtDlpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            YtDlpError::Timeout => write!(f, "yt-dlp timed out after {}s", TIMEOUT.as_secs()),
            YtDlpError::Spawn(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for YtDlpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            YtDlpError::Timeout => None,
            YtDlpError::Spawn(e) => Some(e),
        }
    }
}

/// Runs `yt-dlp` with `args` and returns its raw output.
///
/// The child is killed if the future is dropped; a hung invocation fails
/// with [`YtDlpError::Timeout`]. The exit status is not inspected here:
/// callers parse `stdout` and read the status alongside it.
pub(super) async fn run(args: &[&str]) -> Result<std::process::Output, YtDlpError> {
    let mut command = Command::new("yt-dlp");
    command.kill_on_drop(true).args(args);

    match tokio::time::timeout(TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(YtDlpError::Spawn(e)),
        Err(_) => Err(YtDlpError::Timeout),
    }
}

/// The format selector every resolved stream URL is asked for.
///
/// `bestaudio` alone fails the whole transcode when a client advertises no
/// URL-bearing audio-only format: YouTube's SABR-only streaming experiment
/// (yt-dlp issue 12482) leaves the muxed formats valid while their separate
/// audio tracks have no URL, and yt-dlp then answers "Requested format is
/// not available" for anything audio-only (seen 2026-11-22, video
/// GCGp6oa4fj8). The `/best` fallback picks the best combined format, so a
/// muxed source still transcodes — and `best` is a single format, never a
/// merge, so the answer stays one URL.
const FORMAT_SELECTOR: &str = "bestaudio/best";

/// Resolves the audio stream URL for `url` via
/// `yt-dlp -f <FORMAT_SELECTOR> --get-url`, plus provider-specific `extra_args`.
pub(super) async fn stream_url(url: &Url, extra_args: &[String]) -> eyre::Result<Url> {
    debug!("getting stream_url via yt-dlp for video: {}", url);

    let mut args: Vec<&str> = vec!["-f", FORMAT_SELECTOR, "--get-url", url.as_str()];
    args.extend(extra_args.iter().map(String::as_str));

    let output = run(&args).await.map_err(eyre::Error::new)?;
    parse_stream_url(&output.stdout, &output.stderr, output.status)
}

/// Reads the stream URL out of one yt-dlp invocation's output, or reports
/// what yt-dlp said instead.
///
/// A failing yt-dlp exits non-zero with empty stdout and puts its diagnosis
/// on stderr, so the error carrying that stderr is the only readable account
/// of the failure the operator and the podcatcher's 503 body get.
fn parse_stream_url(
    stdout: &[u8],
    stderr: &[u8],
    status: std::process::ExitStatus,
) -> eyre::Result<Url> {
    let raw_url = std::str::from_utf8(stdout).unwrap_or_default();
    let stderr = std::str::from_utf8(stderr).unwrap_or_default().trim();

    match Url::from_str(raw_url) {
        Ok(url) => Ok(url),
        Err(e) => {
            warn!(
                "error while parsing stream url using yt-dlp:\nerror: {}\nyt-dlp stdout: {}\nyt-dlp stderr: {}",
                e, raw_url, stderr
            );
            Err(eyre::eyre!(
                "yt-dlp gave no stream url ({status}): {e}\nyt-dlp stderr: {stderr}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 503 body's account of a failed resolution is yt-dlp's, not the
    /// url parser's complaint about an empty stdout.
    #[cfg(unix)]
    #[test]
    fn failed_invocation_error_carries_yt_dlp_stderr() {
        let stderr = b"WARNING: [youtube] GCGp6oa4fj8: Some android client https formats have been skipped as they are missing a URL.\nERROR: [youtube] GCGp6oa4fj8: Requested format is not available. Use --list-formats for a list of available formats\n";

        let error = parse_stream_url(b"", stderr, exit_status(1))
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("Requested format is not available"),
            "the error must carry yt-dlp's own diagnosis, got: {error}"
        );
        assert!(
            error.contains("exit status: 1"),
            "the error must name how yt-dlp ended, got: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn printed_url_is_the_resolved_stream_url() {
        let stdout = b"https://rr2---sn-x0gvoxu2a-f5fl.googlevideo.com/videoplayback?itag=251\n";

        let url = parse_stream_url(stdout, b"", exit_status(0)).unwrap();

        assert_eq!(
            url.host_str(),
            Some("rr2---sn-x0gvoxu2a-f5fl.googlevideo.com")
        );
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    /// The fallback is load-bearing: `bestaudio` alone leaves a SABR-only
    /// video unservable, so the selector only changes deliberately.
    #[test]
    fn format_selector_keeps_the_muxed_fallback() {
        assert_eq!(FORMAT_SELECTOR, "bestaudio/best");
    }

    /// The timeout message is part of the observable behavior of every
    /// former inline wrapper: a changed budget must change this string
    /// deliberately, not silently.
    #[test]
    fn timeout_error_names_the_budget() {
        assert_eq!(
            YtDlpError::Timeout.to_string(),
            "yt-dlp timed out after 120s"
        );
    }
}
