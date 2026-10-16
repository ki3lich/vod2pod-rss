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

/// One item yielded to the client: a chunk of transcoded audio, or the error
/// that ends the transfer.
type ChannelBytes = Result<Bytes, std::io::Error>;

/// Consecutive `Interrupted` reads tolerated on the transcoded stdout before
/// the transfer is failed. The budget is per *consecutive* streak: any
/// successful read restarts it (see [`pump_ffmpeg_stdout`]), so transient
/// interruptions spread across a long transcode never accumulate into a
/// failure — only a sustained outage exhausts it.
const MAX_CONSECUTIVE_READ_INTERRUPTS: u32 = 10;

/// How long to wait before retrying an `Interrupted` stdout read.
const READ_INTERRUPT_BACKOFF: Duration = Duration::from_secs(1);

/// Forward ffmpeg's transcoded stdout to the client channel.
///
/// Runs on its own thread (blocking reads): reads `out` in chunks and
/// `blocking_send`s each chunk to the client, enforcing `expected_bytes_count`
/// — truncating ffmpeg's overproduction, padding small undershoots with NUL
/// bytes, and failing the transfer when the input dies far short of the
/// declared length (the shortfall threshold in the body).
///
/// A read failing with `ErrorKind::Interrupted` (a signal arrived mid-syscall)
/// is retried after `interrupt_backoff`. The retry budget is consecutive:
/// `streak` counts interruptions since the last successful read and any
/// successful read resets it, so isolated transient interruptions across a
/// long transcode never exhaust it — only a sustained outage does.
fn pump_ffmpeg_stdout<R: Read>(
    mut out: R,
    mut child: ChildGuard,
    tx_stdout: Sender<ChannelBytes>,
    expected_bytes_count: usize,
    interrupt_backoff: Duration,
) {
    const BUFFER_SIZE: usize = 16384;
    let mut buff: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];
    let mut streak: u32 = 0;
    let mut sent_bytes_count: usize = 0;
    loop {
        match out.read(&mut buff) {
            Ok(read_bytes) => {
                // Any successful read restarts the interruption budget: only
                // *consecutive* interruptions may exhaust it, so isolated
                // transient interruptions across a long transcode can never
                // accumulate into a failure.
                streak = 0;

                if sent_bytes_count + read_bytes > expected_bytes_count {
                    //partial request is fulfilled we only need to send the remaining data
                    let bytes_remaining = expected_bytes_count - sent_bytes_count;
                    _ = tx_stdout
                        .blocking_send(Ok(Bytes::copy_from_slice(&buff[..bytes_remaining])));
                    info!("transcoded everything in partial request");
                    _ = child.kill();
                    _ = child.wait();
                    break;
                }

                if read_bytes == 0 {
                    // An input that dies early is indistinguishable from a
                    // finished one at the stdout level: both close the pipe,
                    // and ffmpeg exits 0 with an empty stderr. Only the byte
                    // shortfall tells them apart. Within MAX_OK_SHORTFALL_RATIO
                    // the EOF is a finished, slightly-shorter-than-declared
                    // stream (encoder overhead, duration rounding — a
                    // fraction of a percent in production) and keeps the
                    // historical padding; beyond it the upstream ended
                    // prematurely, and padding would yield a full-length 200
                    // of mostly NUL bytes that podcatchers cache and then
                    // fail to play — fail the stream instead so the client
                    // sees a broken transfer and retries later. Tail ranges
                    // mapping past the real end of a shorter-than-declared
                    // stream land on the same error path on purpose: a
                    // broken transfer beats cached zeros. See
                    // `transcode_stream_errors_when_input_ends_far_short_of_expected`
                    // for the incident that motivated the threshold.
                    const MAX_OK_SHORTFALL_RATIO: f64 = 0.1;
                    let shortfall = expected_bytes_count - sent_bytes_count;
                    if shortfall as f64 > expected_bytes_count as f64 * MAX_OK_SHORTFALL_RATIO {
                        error!(
                            "input stream ended after {sent_bytes_count} of {expected_bytes_count} expected bytes; not padding the shortfall"
                        );
                        _ = tx_stdout.blocking_send(Err(std::io::Error::other(format!(
                            "media stream ended prematurely: got {sent_bytes_count} of {expected_bytes_count} bytes"
                        ))));
                        _ = child.wait();
                        break;
                    }
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
                            _ = tx_stdout.blocking_send(Ok(Bytes::copy_from_slice(&NULL_BUFF)));
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

                let send_res =
                    tx_stdout.blocking_send(Ok(Bytes::copy_from_slice(&buff[..read_bytes])));

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
                    if streak >= MAX_CONSECUTIVE_READ_INTERRUPTS {
                        error!("read was interrupted too many times");
                        fail_transfer(&tx_stdout, &mut child, e);
                        break;
                    }
                    warn!("read was interrupted, retrying in {interrupt_backoff:?}");
                    sleep(interrupt_backoff);
                    streak += 1;
                }
                _ => {
                    // Fatal like an exhausted budget: the terminating error is
                    // out; reading on could only produce items after it.
                    fail_transfer(&tx_stdout, &mut child, e);
                    break;
                }
            },
        }
    }
}

/// Surface `e` to the client as the transfer's terminating error, then stop
/// ffmpeg and the pump: the transfer is over, and reading on could only
/// produce items after the error the client already saw.
fn fail_transfer(tx_stdout: &Sender<ChannelBytes>, child: &mut ChildGuard, e: std::io::Error) {
    if let Err(err) = tx_stdout.blocking_send(Err(e)) {
        error!("unexpected error occurred:");
        error!("{}", err);
    }
    _ = child.kill();
    _ = child.wait();
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
            // is unit-tested.
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
            let out = child.stdout.take().expect("failed to open stdout");

            let channel_size: usize = 100;
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
                pump_ffmpeg_stdout(
                    out,
                    child,
                    tx_stdout,
                    expected_bytes_count,
                    READ_INTERRUPT_BACKOFF,
                )
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

    #[test]
    fn transcode_stream_errors_when_input_ends_far_short_of_expected() {
        // Real incident (2026-10-13, Kwadrans z Kangą #327): a flaky googlevideo
        // edge delivered ~2s of a 50-minute episode and closed the connection
        // cleanly. ffmpeg exited 0 with an empty stderr — indistinguishable from
        // a finished input at the stdout level — and the transcoder dressed the
        // result up as a full-length 200 by padding the shortfall with NUL
        // bytes. Apple Podcasts cached that "valid" file, failed to decode the
        // padding after the real audio, skipped the episode and marked it
        // played. An input that ends this far short of the declared length must
        // surface as an error, so the client sees a broken transfer and retries
        // later instead of caching garbage.
        //
        // Deterministic: a local 2s fixture "ends" in well under a second while
        // the declared length claims ~15x more audio. No network involved (the
        // `file` protocol is in ffmpeg's whitelist).
        let fixture_dir = std::env::temp_dir().join(format!("vod2pod-test-{}", std::process::id()));
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let fixture = fixture_dir.join("short.mp3");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-acodec",
                "libmp3lame",
                "-ab",
                "32k",
                fixture.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg must be available to run this test");
        assert!(generated.success(), "failed to generate test fixture");

        let real_len = std::fs::metadata(&fixture).unwrap().len() as usize;
        let url = Url::from_file_path(&fixture).unwrap();
        let expected_bytes_count = real_len * 15;
        let params = FfmpegParameters {
            seek_time: 0.0,
            url,
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 32,
            max_rate_kbit: 960,
            expected_bytes_count,
            timeout_in_seconds: 600,
        };
        let transcoder = Transcoder {
            ffmpeg_command: Transcoder::get_ffmpeg_command(&params),
            expected_bytes_count,
        };

        use futures::StreamExt;
        let mut stream = transcoder.get_transcode_stream();
        let mut ok_bytes = 0usize;
        let mut stream_err = None;
        futures::executor::block_on(async {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => ok_bytes += bytes.len(),
                    Err(e) => {
                        stream_err = Some(e);
                        break;
                    }
                }
            }
        });
        // Clean up before any assertion so a failing run leaves no /tmp litter.
        std::fs::remove_dir_all(&fixture_dir).ok();

        let err = stream_err.unwrap_or_else(|| {
            panic!(
                "transcode stream reported success after only {ok_bytes} of \
                 {expected_bytes_count} expected bytes — early input death was \
                 padded over instead of surfacing as an error"
            )
        });
        info!("stream failed as expected after {ok_bytes} bytes: {err}");
        assert!(ok_bytes < expected_bytes_count);
    }

    #[test]
    fn transcode_stream_still_pads_small_undershoot() {
        // The other side of the threshold: a stream that ends within legitimate
        // encoder undershoot of the declared length is a finished, slightly
        // short stream — not a dead one — and must keep the historical padding
        // behavior, completing the transfer with the full declared length.
        let fixture_dir = std::env::temp_dir().join(format!("vod2pod-test-{}", std::process::id()));
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let fixture = fixture_dir.join("short.mp3");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-acodec",
                "libmp3lame",
                "-ab",
                "32k",
                fixture.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg must be available to run this test");
        assert!(generated.success(), "failed to generate test fixture");

        let real_len = std::fs::metadata(&fixture).unwrap().len() as usize;
        let url = Url::from_file_path(&fixture).unwrap();
        // Re-encoding the same audio at the same bitrate lands within a few
        // percent of the fixture size — well inside the 10% threshold either
        // way (a slightly larger output takes the partial-request truncation
        // branch, a slightly smaller one the padding branch; both complete).
        let expected_bytes_count = real_len;
        let params = FfmpegParameters {
            seek_time: 0.0,
            url,
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 32,
            max_rate_kbit: 960,
            expected_bytes_count,
            timeout_in_seconds: 600,
        };
        let transcoder = Transcoder {
            ffmpeg_command: Transcoder::get_ffmpeg_command(&params),
            expected_bytes_count,
        };

        use futures::StreamExt;
        let mut stream = transcoder.get_transcode_stream();
        let mut ok_bytes = 0usize;
        let mut stream_err = None;
        futures::executor::block_on(async {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(bytes) => ok_bytes += bytes.len(),
                    Err(e) => {
                        stream_err = Some(e);
                        break;
                    }
                }
            }
        });
        std::fs::remove_dir_all(&fixture_dir).ok();

        assert!(
            stream_err.is_none(),
            "small undershoot was surfaced as an error: {stream_err:?}"
        );
        assert_eq!(ok_bytes, expected_bytes_count);
    }

    /// A byte source whose read outcomes are scripted in advance, so the
    /// stdout pump can be driven through interruption/success sequences
    /// deterministically — no signals, no pipes, no real ffmpeg.
    struct ScriptedSource {
        steps: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl Read for ScriptedSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                Some(Ok(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                // Script exhausted: the source is done.
                None => Ok(0),
            }
        }
    }

    fn interrupted() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "synthetic read interruption",
        )
    }

    #[test]
    fn interruptions_interleaved_with_reads_do_not_exhaust_retry_budget() {
        // The retry budget is consecutive, not cumulative: any successful read
        // restarts it, so a long transcode survives arbitrarily many isolated
        // interruptions. A counter that never resets treats each interruption
        // as draining one shared lifetime budget, and enough of them over a
        // long transcode kill an otherwise healthy stream.
        const CHUNK_LEN: usize = 100;
        // Far more interruptions than the budget tolerates in a row.
        const INTERLEAVED_INTERRUPTIONS: usize = 15;
        let mut steps = std::collections::VecDeque::new();
        for _ in 0..INTERLEAVED_INTERRUPTIONS {
            steps.push_back(Err(interrupted()));
            steps.push_back(Ok(vec![0xAA; CHUNK_LEN]));
        }
        steps.push_back(Ok(Vec::new())); // clean EOF
        let expected_bytes_count = INTERLEAVED_INTERRUPTIONS * CHUNK_LEN;

        let (tx, mut rx) = channel(100);
        let child = ChildGuard(Command::new("true").spawn().unwrap());
        std::thread::spawn(move || {
            pump_ffmpeg_stdout(
                ScriptedSource { steps },
                child,
                tx,
                expected_bytes_count,
                Duration::ZERO,
            );
        });

        // Mirror the real consumer: stop at the first error — that is where
        // a budget exhaustion would end the transfer.
        let mut received = 0usize;
        let mut stream_err = None;
        while let Some(item) = rx.blocking_recv() {
            match item {
                Ok(bytes) => received += bytes.len(),
                Err(e) => {
                    stream_err = Some(e);
                    break;
                }
            }
        }

        assert!(
            stream_err.is_none(),
            "stream failed under interruptions interleaved with successful reads: {stream_err:?}"
        );
        assert_eq!(
            received, expected_bytes_count,
            "stream was cut short by interruptions"
        );
    }

    #[test]
    fn consecutive_interruptions_beyond_retry_budget_fail_the_stream() {
        // The other half of the contract: a sustained outage — interruptions
        // back to back, never a successful read in between — must still fail
        // the transfer instead of retrying forever. The reset-on-success
        // repair must not turn the budget into a no-op.
        // One interruption beyond the tolerated streak exhausts the budget.
        let steps: std::collections::VecDeque<_> = (0..MAX_CONSECUTIVE_READ_INTERRUPTS + 1)
            .map(|_| Err(interrupted()))
            .collect();

        let (tx, mut rx) = channel(100);
        let child = ChildGuard(Command::new("true").spawn().unwrap());
        std::thread::spawn(move || {
            pump_ffmpeg_stdout(ScriptedSource { steps }, child, tx, 1000, Duration::ZERO);
        });

        match rx.blocking_recv().expect("stream ended without any item") {
            Ok(_) => panic!("stream delivered data through a sustained interruption outage"),
            Err(e) => info!("stream failed as expected: {e}"),
        }
        assert!(
            rx.blocking_recv().is_none(),
            "pump kept producing after failing the transfer"
        );
    }
}
