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
/// with [`YtDlpError::Timeout`]. The exit status is not inspected: callers
/// parse `stdout` and decide for themselves, as they always have.
pub(super) async fn run(args: &[&str]) -> Result<std::process::Output, YtDlpError> {
    let mut command = Command::new("yt-dlp");
    command.kill_on_drop(true).args(args);

    match tokio::time::timeout(TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(YtDlpError::Spawn(e)),
        Err(_) => Err(YtDlpError::Timeout),
    }
}

/// Resolves the audio stream URL for `url` via
/// `yt-dlp -f bestaudio --get-url`, plus provider-specific `extra_args`.
pub(super) async fn stream_url(url: &Url, extra_args: &[String]) -> eyre::Result<Url> {
    debug!("getting stream_url via yt-dlp for video: {}", url);

    let mut args: Vec<&str> = vec!["-f", "bestaudio", "--get-url", url.as_str()];
    args.extend(extra_args.iter().map(String::as_str));

    let output = run(&args).await.map_err(eyre::Error::new)?;
    let raw_url = std::str::from_utf8(&output.stdout).unwrap_or_default();
    match Url::from_str(raw_url) {
        Ok(url) => Ok(url),
        Err(e) => {
            warn!(
                "error while parsing stream url using yt-dlp:\nerror: {}\nyt-dlp stdout: {}\nyt-dlp stderr: {}",
                e,
                raw_url,
                std::str::from_utf8(&output.stderr).unwrap_or_default()
            );
            Err(eyre::eyre!(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
