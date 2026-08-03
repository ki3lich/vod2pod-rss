use std::error::Error;
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::process::Command;
use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;

/// Guard to ensure child process is waited on when dropped
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.wait();
    }
}

impl Deref for ChildGuard {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

use actix_web::web::Bytes;
use futures::Future;
use genawaiter::sync::{Co, Gen};
use log::info;
use log::{debug, error, warn};
use reqwest::Url;
use serde::Serialize;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

use crate::configs::{conf, AudioCodec, Conf, ConfName};
use crate::provider;
use crate::provider::MediaProvider;

#[derive(Serialize)]
pub struct FfmpegParameters {
    pub seek_time: f32,
    pub url: Url,
    pub audio_codec: AudioCodec,
    pub bitrate_kbit: usize,
    pub max_rate_kbit: usize,
    pub expected_bytes_count: usize,
    pub timeout_in_seconds: usize,
}

impl FfmpegParameters {
    pub fn bitarate(&self) -> usize {
        self.bitrate_kbit * 1024
    }
}

#[derive(Debug)]
pub struct Transcoder {
    ffmpeg_command: Command,
    expected_bytes_count: usize,
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
/// caller (the server) already wraps `Transcoder::new` in its own
/// `FfmpegTimeoutSeconds` timeout, so this preflight only needs to beat ffmpeg's
/// ~127s `ETIMEDOUT`, not the whole transcode.
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

impl Transcoder {
    pub async fn new(ffmpeg_paramenters: &FfmpegParameters) -> eyre::Result<Self> {
        let provider = provider::from(&ffmpeg_paramenters.url);

        // Resolve the (possibly cached) stream URL. For YouTube this runs yt-dlp
        // and reads/writes the Redis cache; for the generic provider it is a
        // no-op clone.
        let stream_url = provider.get_stream_url(&ffmpeg_paramenters.url).await?;

        // Preflight: a CDN edge that drops SYNs makes ffmpeg hang ~127s before
        // ETIMEDOUT. Fail fast instead, and evict the cached stream URL so the
        // next request re-resolves (the upstream edge may have rotated).
        let preflight_timeout = Duration::from_secs(
            conf()
                .get(ConfName::PreflightTimeoutSeconds)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(3),
        );
        if let Err(e) = preflight_stream_reachable(&stream_url, preflight_timeout).await {
            warn!(
                "preflight reachability check failed for {stream_url}; \
                 evicting cached stream url for {} and failing fast: {e}",
                ffmpeg_paramenters.url
            );
            // Best-effort: a Redis failure must not mask the original preflight
            // error. No fast local unit-test seam exists for this eviction path
            // — it is bound to the real provider selected by `provider::from`
            // (which dispatches by URL) and to a live Redis — so it is exercised
            // only end-to-end. The pure verdict logic above (`interpret_preflight_connect`)
            // and the cache-key format (`stream_url_cache_key`) are unit-tested.
            let _ = provider
                .evict_stream_url_cache(&ffmpeg_paramenters.url)
                .await;
            return Err(e);
        }

        let ffmpeg_command = Self::get_ffmpeg_command(&FfmpegParameters {
            seek_time: ffmpeg_paramenters.seek_time,
            url: stream_url,
            audio_codec: ffmpeg_paramenters.audio_codec.to_owned(),
            bitrate_kbit: ffmpeg_paramenters.bitrate_kbit,
            max_rate_kbit: ffmpeg_paramenters.max_rate_kbit,
            expected_bytes_count: ffmpeg_paramenters.expected_bytes_count,
            timeout_in_seconds: ffmpeg_paramenters.timeout_in_seconds,
        });

        Ok(Self {
            ffmpeg_command,
            expected_bytes_count: ffmpeg_paramenters.expected_bytes_count,
        })
    }

    fn get_ffmpeg_command(ffmpeg_paramenters: &FfmpegParameters) -> Command {
        debug!("generating ffmpeg command");
        let mut command = Command::new("ffmpeg");
        let command_ref = &mut command;

        command_ref
            .args(["-ss", ffmpeg_paramenters.seek_time.to_string().as_str()])
            .args([
                "-protocol_whitelist",
                "file,http,https,tcp,tls",
                "-i",
                ffmpeg_paramenters.url.as_str(),
            ])
            .args([
                "-acodec",
                ffmpeg_paramenters.audio_codec.get_ffmpeg_codec_str(),
            ])
            .args(["-threads", "0"])
            .args([
                "-ab",
                format!("{}k", ffmpeg_paramenters.bitrate_kbit).as_str(),
            ])
            .args(["-f", ffmpeg_paramenters.audio_codec.get_extension_str()])
            .args([
                "-bufsize",
                (ffmpeg_paramenters.bitrate_kbit * 30).to_string().as_str(),
            ])
            .args([
                "-maxrate",
                format!("{}k", ffmpeg_paramenters.max_rate_kbit).as_str(),
            ])
            .args([
                "-timeout",
                ffmpeg_paramenters.timeout_in_seconds.to_string().as_str(),
            ])
            .args(["-hide_banner"])
            .args(["-loglevel", "error"])
            .arg("-");
        let args: Vec<String> = command_ref
            .get_args()
            .map(|x| x.to_string_lossy().to_string())
            .collect();
        info!(
            "generated ffmpeg command:\n{} {}",
            command_ref.get_program().to_string_lossy(),
            args.join(" ")
        );
        command
    }

    pub fn get_transcode_stream(
        self,
    ) -> Gen<Result<Bytes, impl Error>, (), impl Future<Output = ()>> {
        async fn generetor_coroutine(
            mut command: Command,
            expected_bytes_count: usize,
            co: Co<Result<Bytes, std::io::Error>>,
        ) {
            let mut child = ChildGuard(
                command
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("failed to run commnad"),
            );

            let mut err = child.stderr.take().expect("failed to open stderr");
            let mut out = child.stdout.take().expect("failed to open stdout");

            let channel_size: usize = 100;
            type ChannelBytes = Result<Bytes, std::io::Error>;
            let (tx, mut rx): (Sender<ChannelBytes>, Receiver<ChannelBytes>) =
                channel(channel_size);

            let tx_stdout = tx.clone();
            let tx_stderr = tx;

            //stderr thread
            std::thread::spawn(move || loop {
                let mut buf = String::new();
                match err.read_to_string(&mut buf) {
                    Ok(0) => {
                        debug!("ffmpeg stderr closed");
                        break;
                    }
                    Ok(_) => {
                        error!("{}", buf);
                        let _ = tx_stderr.blocking_send(Err(std::io::Error::other(buf)));
                    }
                    Err(e) => {
                        error!("failed to read from stderr: {}", e);
                        let _ = tx_stderr.blocking_send(Err(std::io::Error::other(e)));
                        break;
                    }
                }
            });

            //stdout thread
            std::thread::spawn(move || {
                const BUFFER_SIZE: usize = 16384;
                let mut buff: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];
                let mut tries = 0;
                let mut sent_bytes_count: usize = 0;
                loop {
                    match out.read(&mut buff) {
                        Ok(read_bytes) => {
                            if sent_bytes_count + read_bytes > expected_bytes_count {
                                //partial request is fulfilled we only need to send the remaining data
                                let bytes_remaining = expected_bytes_count - sent_bytes_count;
                                _ = tx_stdout.blocking_send(Ok(Bytes::copy_from_slice(
                                    &buff[..bytes_remaining],
                                )));
                                info!("transcoded everything in partial request");
                                _ = child.kill();
                                _ = child.wait();
                                break;
                            }

                            if read_bytes == 0 {
                                info!("transcoded everything");
                                //pad end of stream with 00000000 bytes if client expects more data to be sent
                                const NULL_BUFF: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];
                                debug!(
                                    "sending {} bytes of padding",
                                    expected_bytes_count - sent_bytes_count
                                );
                                while sent_bytes_count < expected_bytes_count {
                                    let padding_bytes = expected_bytes_count - sent_bytes_count;
                                    if padding_bytes >= BUFFER_SIZE {
                                        _ = tx_stdout
                                            .blocking_send(Ok(Bytes::copy_from_slice(&NULL_BUFF)));
                                        sent_bytes_count += BUFFER_SIZE;
                                    } else {
                                        _ = tx_stdout.blocking_send(Ok(Bytes::copy_from_slice(
                                            &NULL_BUFF[..padding_bytes],
                                        )));
                                        sent_bytes_count += padding_bytes;
                                    }
                                }
                                _ = child.wait();
                                break;
                            }

                            let send_res = tx_stdout
                                .blocking_send(Ok(Bytes::copy_from_slice(&buff[..read_bytes])));

                            if let Err(e) = send_res {
                                debug!("{}", e);
                                info!("connection to client dropped, stopping transcode");
                                _ = child.kill();
                                _ = child.wait();
                                break;
                            };
                            sent_bytes_count += read_bytes;
                        }
                        Err(e) => match e.kind() {
                            std::io::ErrorKind::Interrupted => {
                                if tries > 10 {
                                    error!("read was interrupted too many times");
                                    if let Err(err) = tx_stdout.blocking_send(Err(e)) {
                                        error!("unexpected error occured:");
                                        error!("{}", err);
                                        _ = child.kill();
                                        _ = child.wait();
                                        break;
                                    };
                                }
                                warn!("read was interrupted, retrying in 1sec");
                                sleep(Duration::from_secs(1));
                                tries += 1;
                            }
                            _ => {
                                if let Err(err) = tx_stdout.blocking_send(Err(e)) {
                                    error!("unexpected error occured:");
                                    error!("{}", err);
                                    _ = child.kill();
                                    _ = child.wait();
                                    break;
                                };
                            }
                        },
                    };
                }
            });

            info!("streaming to client");
            while let Some(x) = rx.recv().await {
                match x {
                    Ok(bytes) => co.yield_(Ok(bytes)).await,
                    Err(e) => {
                        rx.close();
                        co.yield_(Err(e)).await
                    }
                }
            }
        }
        Gen::new(|co| generetor_coroutine(self.ffmpeg_command, self.expected_bytes_count, co))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use log::info;

    #[test]
    fn check_ffmpeg_command() {
        // Exercises pure command construction directly. `Transcoder::new` now
        // also runs a reachability preflight (network), so driving it here would
        // couple this command-shape test to DNS/TCP behavior. `url.mp3` is not
        // expected to resolve; only the produced argv is asserted.
        let stream_url = Url::parse("http://url.mp3").unwrap();
        let params = FfmpegParameters {
            seek_time: 30.0,
            url: stream_url,
            max_rate_kbit: 64,
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 3,
            expected_bytes_count: 999,
            timeout_in_seconds: 600,
        };

        let command = Transcoder::get_ffmpeg_command(&params);
        let ppath = command.get_program();
        if let Some(x) = ppath.to_str() {
            info!("{} ", x);
            assert_eq!(x, "ffmpeg");
        }
        let mut args = command.get_args();

        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("-ss") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-ss {}", value);
                    assert_eq!(value, params.seek_time.to_string().as_str());
                }
                Some("-protocol_whitelist") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-protocol_whitelist {}", value);
                    assert_eq!(value, "file,http,https,tcp,tls");
                }
                Some("-i") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-i {}", value);
                    assert_eq!(value, params.url.as_str());
                }
                Some("-acodec") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-acodec {}", value);
                    assert_eq!(value, params.audio_codec.get_ffmpeg_codec_str());
                }
                Some("-ab") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-ab {}", value);
                }
                Some("-f") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-f {}", value);
                    assert_eq!(value, "mp3");
                }
                Some("-bufsize") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-bufsize {}", value);
                }
                Some("-maxrate") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-maxrate {}", value);
                }
                Some("-") => {
                    info!("-");
                }
                Some("-timeout") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-timeout {}", value);
                    assert_eq!(value, "600");
                }
                Some("-hide_banner") => {
                    info!("-hide_banner");
                }
                Some("-loglevel") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-loglevel {}", value);
                }
                Some("-threads") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-threads {}", value);
                }
                Some(x) => panic!("ffmpeg run with uknown option: {x}"),
                None => panic!("ffmpeg run with no options"),
            }
        }
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
        assert!(preflight_stream_reachable(&url, Duration::from_secs(2))
            .await
            .is_ok());
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
        assert!(preflight_stream_reachable(&url, Duration::from_secs(2))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn transcoder_new_passes_preflight_for_reachable() {
        // End-to-end through resolution + preflight. Uses the generic provider,
        // which clones the URL (no yt-dlp, no Redis), so this stays local.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x.mp3")).unwrap();
        let params = FfmpegParameters {
            seek_time: 0.0,
            url,
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 128,
            max_rate_kbit: 3840,
            expected_bytes_count: 100,
            timeout_in_seconds: 300,
        };
        let transcoder = Transcoder::new(&params).await;
        assert!(
            transcoder.is_ok(),
            "expected preflight to pass for a listening local port, got {transcoder:?}"
        );
        drop(listener);
    }
}
