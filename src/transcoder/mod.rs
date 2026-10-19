// pub(crate): the crate-root test_support shares the staging test knobs.
pub(crate) mod staging;
mod stdout_pump;
mod stream_url;

pub use self::staging::{init_stage_gate, sweep_stale_staged_sources};
use self::stdout_pump::{spawn_pump_threads, ChannelBytes};
use self::stream_url::{
    ProviderStreamUrlResolver, ResolvedSource, StagedSource, StreamUrlResolver,
};
use std::error::Error;
use std::ops::{Deref, DerefMut};
use std::process::Command;
use std::process::Stdio;

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
use log::{debug, info};
use reqwest::Url;
use serde::Serialize;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

use crate::configs::AudioCodec;

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
    /// A handed-over Staged Source: the transcode reads this local file
    /// instead of the network, and the value carries its own Staging Slot
    /// (issue #181) — the resolver acquired it before resolution began, so
    /// the ordering is the type's, not a caller's. Owned here so the file
    /// is deleted and the slot released exactly when the transcode ends —
    /// completed, failed, or the client gone (the value moves into the
    /// transcode stream).
    staged_source: Option<StagedSource>,
}

impl Transcoder {
    /// Production entry point: wires the real resolution pipeline behind the
    /// [`StreamUrlResolver`] seam — the provider the media URL dispatches to
    /// (cached yt-dlp resolution on the YouTube rung) plus the TCP preflight
    /// with its configured timeout — then builds the transcoder from what it
    /// resolves.
    pub async fn new(ffmpeg_paramenters: &FfmpegParameters) -> eyre::Result<Self> {
        let resolver = ProviderStreamUrlResolver::for_media_url(&ffmpeg_paramenters.url);
        Self::with_resolver(ffmpeg_paramenters, &resolver).await
    }

    /// Builds the transcoder from the stream URL resolved behind the
    /// [`StreamUrlResolver`] seam — the injection point tests use to drive
    /// resolution with fakes.
    async fn with_resolver(
        ffmpeg_paramenters: &FfmpegParameters,
        resolver: &dyn StreamUrlResolver,
    ) -> eyre::Result<Self> {
        // The seam carries either artifact: a Stream URL (ffmpeg reads the
        // network itself) or a Staged Source (ffmpeg reads the local file —
        // the `file` protocol is in the command's whitelist below, so no
        // network I/O happens on a staged path). The staged artifact arrives
        // already holding its Staging Slot (issue #181): held here, both the
        // file's deletion and the slot's release die with the transcode —
        // or with any construction failure below, leaving no stranded file
        // and no held slot.
        let (input_url, staged_source) = match resolver.resolve(&ffmpeg_paramenters.url).await? {
            ResolvedSource::StreamUrl(stream_url) => (stream_url, None),
            ResolvedSource::StagedFile(staged) => {
                let input_url = Url::from_file_path(staged.path()).map_err(|_| {
                    eyre::eyre!(
                        "staged source path is not absolute: {}",
                        staged.path().display()
                    )
                })?;
                (input_url, Some(staged))
            }
        };

        let ffmpeg_command = Self::get_ffmpeg_command(&FfmpegParameters {
            seek_time: ffmpeg_paramenters.seek_time,
            url: input_url,
            audio_codec: ffmpeg_paramenters.audio_codec.to_owned(),
            bitrate_kbit: ffmpeg_paramenters.bitrate_kbit,
            max_rate_kbit: ffmpeg_paramenters.max_rate_kbit,
            expected_bytes_count: ffmpeg_paramenters.expected_bytes_count,
            timeout_in_seconds: ffmpeg_paramenters.timeout_in_seconds,
        });

        Ok(Self {
            ffmpeg_command,
            expected_bytes_count: ffmpeg_paramenters.expected_bytes_count,
            staged_source,
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
            // Held for the life of the transcode: dropped when the stream is
            // consumed to its end (completed or failed) or dropped early
            // (client gone) — either way the staged file is deleted with the
            // transcode and its Staging Slot returns to the gate, the next
            // queued transcode then gets its turn (issues #172, #181). Safe
            // against a still-running ffmpeg: it keeps its open descriptor
            // of the unlinked file.
            // The underscore is the name only: the value's Drop is its use.
            _staged: Option<StagedSource>,
            co: Co<Result<Bytes, std::io::Error>>,
        ) {
            let mut child = ChildGuard(
                command
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("failed to run commnad"),
            );

            let stderr = child.stderr.take().expect("failed to open stderr");
            let stdout = child.stdout.take().expect("failed to open stdout");

            let channel_size: usize = 100;
            let (tx, mut rx): (Sender<ChannelBytes>, Receiver<ChannelBytes>) =
                channel(channel_size);

            // Both pump threads are thin executors of the decision functions
            // in `stdout_pump`.
            spawn_pump_threads(child, stdout, stderr, tx, expected_bytes_count);

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
        let Self {
            ffmpeg_command,
            expected_bytes_count,
            staged_source,
        } = self;
        Gen::new(|co| generetor_coroutine(ffmpeg_command, expected_bytes_count, staged_source, co))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_trait::async_trait;
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

    /// The fixed source a [`FixedResolver`] hands back: a Stream URL as-is,
    /// or a staged fixture stored as its path and adopted into a fresh
    /// guard per call — the guard is the artifact's owner, the path the
    /// fixture. The e2e tests hold the staging stage's original guard alive
    /// alongside the adopted one, so two guards own the same path: whichever
    /// drops first deletes the file and the other's drop is the tolerated
    /// no-op — the transcode ends before the test does, so the adopted
    /// guard deletes and the staging guard's drop is the no-op.
    enum FixedSource {
        Url(Url),
        Staged(std::path::PathBuf),
    }

    /// Resolver fake handing back one fixed resolved source.
    struct FixedResolver(FixedSource);

    #[async_trait]
    impl StreamUrlResolver for FixedResolver {
        async fn resolve(&self, _media_url: &Url) -> eyre::Result<ResolvedSource> {
            Ok(match &self.0 {
                FixedSource::Url(url) => ResolvedSource::StreamUrl(url.clone()),
                FixedSource::Staged(path) => {
                    ResolvedSource::StagedFile(StagedSource::adopt(path.clone()))
                }
            })
        }
    }

    #[tokio::test]
    async fn with_resolver_builds_command_from_resolved_stream_url() {
        // The transcoder consumes the seam: ffmpeg's `-i` is the *resolved*
        // stream URL the resolver handed over, not the media URL being
        // transcoded.
        let resolved = Url::parse("https://resolved.example.com/audio.mp3").unwrap();
        let params = FfmpegParameters {
            seek_time: 0.0,
            url: Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap(),
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 128,
            max_rate_kbit: 3840,
            expected_bytes_count: 100,
            timeout_in_seconds: 300,
        };

        let transcoder =
            Transcoder::with_resolver(&params, &FixedResolver(FixedSource::Url(resolved.clone())))
                .await
                .expect("resolution via the fake must succeed");

        let args: Vec<String> = transcoder
            .ffmpeg_command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let input = args
            .iter()
            .position(|a| a == "-i")
            .expect("ffmpeg command carries an input");
        assert_eq!(args[input + 1], resolved.as_str());
    }

    #[tokio::test]
    async fn with_resolver_builds_command_from_staged_file() {
        // The staged half of the seam: when the check stage hands back a
        // Staged Source, ffmpeg's `-i` is the staged file itself — read via
        // the command's whitelisted `file` protocol — not a network URL. No
        // network: the staged artifact is a real local file and nothing is
        // fetched.
        let fixture_dir =
            std::env::temp_dir().join(format!("vod2pod-test-staged-input-{}", std::process::id()));
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let staged = fixture_dir.join("staged-source.bin");
        std::fs::write(&staged, b"staged source bytes").unwrap();

        let params = FfmpegParameters {
            seek_time: 0.0,
            url: Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap(),
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 128,
            max_rate_kbit: 3840,
            expected_bytes_count: 100,
            timeout_in_seconds: 300,
        };

        let transcoder =
            Transcoder::with_resolver(&params, &FixedResolver(FixedSource::Staged(staged.clone())))
                .await
                .expect("a staged source must build the transcoder");
        std::fs::remove_dir_all(&fixture_dir).ok();

        let args: Vec<String> = transcoder
            .ffmpeg_command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let input = args
            .iter()
            .position(|a| a == "-i")
            .expect("ffmpeg command carries an input");
        assert_eq!(
            args[input + 1],
            Url::from_file_path(&staged).unwrap().as_str(),
            "ffmpeg's input must be the staged file, not a network URL"
        );
    }

    #[tokio::test]
    async fn a_failed_construction_deletes_the_staged_file_instead_of_stranding_it() {
        // User story 9: a staged file never outlives its owner. The staged
        // artifact crosses the seam as its guard, so a construction failure
        // — here a staged path that cannot become a file URL — drops the
        // guard and deletes the file.
        //
        // `Url::from_file_path` only refuses non-absolute paths, and the
        // temp dirs are absolute — so the fixture must be a relative path.
        // Cargo runs tests at the package root; the file is created there
        // under a unique name and removed by the guard's drop (the final
        // cleanup is a best-effort backstop for the failure case).
        let staged =
            std::path::PathBuf::from(format!("vod2pod-test-orphan-{}.bin", uuid::Uuid::new_v4()));
        std::fs::write(&staged, b"staged source bytes").unwrap();

        let params = FfmpegParameters {
            seek_time: 0.0,
            url: Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap(),
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 128,
            max_rate_kbit: 3840,
            expected_bytes_count: 100,
            timeout_in_seconds: 300,
        };
        let err =
            Transcoder::with_resolver(&params, &FixedResolver(FixedSource::Staged(staged.clone())))
                .await
                .expect_err("a relative staged path must fail the construction");
        assert!(
            err.to_string().contains("not absolute"),
            "the error names the unusable path: {err}"
        );
        assert!(
            !staged.exists(),
            "the failed construction must delete the staged file, not strand it"
        );
        std::fs::remove_file(&staged).ok();
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
        // Unique dir per test: the two ffmpeg pin tests run concurrently and must
        // not share (and one of them delete) the other's fixture.
        let fixture_dir =
            std::env::temp_dir().join(format!("vod2pod-test-shortfall-{}", std::process::id()));
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
            staged_source: None,
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
        let fixture_dir =
            std::env::temp_dir().join(format!("vod2pod-test-pad-{}", std::process::id()));
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
            staged_source: None,
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

    // ---- staged end-to-end (issue #170) ----

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::Semaphore;

    use crate::transcoder::staging::{stage_gate_of, STAGED_FILE_PREFIX};

    use crate::provider::{GeneratedFeed, MediaProvider};
    use crate::rss_transcodizer::streamable_bytes;
    use crate::test_support::staging_test_limits;
    use crate::transcoder::staging::test_origin::{OriginScript, ScriptedOrigin};
    use crate::transcoder::staging::{StageLimits, StagingCheckStage};

    /// The media URL the RSS enclosure points at — a YouTube watch URL, as
    /// production would carry.
    fn e2e_media_url() -> Url {
        Url::parse("https://www.youtube.com/watch?v=UMO52N2vfk0").unwrap()
    }

    /// The size the enclosure would declare for `duration_secs` of audio at
    /// the e2e bitrate — the same arithmetic the server serves with, so the
    /// stdout pump's shortfall/truncation branches see a realistic contract.
    fn e2e_expected_bytes(duration_secs: u64) -> usize {
        streamable_bytes(duration_secs, 32) as usize
    }

    /// Params for the e2e transcodes: mp3 at 32k, no seek.
    fn e2e_params(expected_bytes_count: usize) -> FfmpegParameters {
        FfmpegParameters {
            seek_time: 0.0,
            url: e2e_media_url(),
            audio_codec: AudioCodec::MP3,
            bitrate_kbit: 32,
            max_rate_kbit: 960,
            expected_bytes_count,
            timeout_in_seconds: 600,
        }
    }

    /// Provider fake whose Stream URL is the media bytes themselves — a
    /// YouTube-like direct-media source resolved to the scripted origin.
    /// No yt-dlp, no Redis.
    struct OriginProvider(Url);

    #[async_trait]
    impl MediaProvider for OriginProvider {
        async fn generate_rss_feed(&self, _channel_url: Url) -> eyre::Result<GeneratedFeed> {
            panic!("staged end-to-end never converts feeds")
        }

        async fn get_stream_url(&self, _media_url: &Url) -> eyre::Result<Url> {
            Ok(self.0.clone())
        }

        fn streams_direct_media(&self) -> bool {
            true
        }

        fn domain_whitelist_regexes(&self) -> Vec<regex::Regex> {
            Vec::new()
        }
    }

    /// A small real mp3 fixture generated with ffmpeg's lavfi tone
    /// generator — the source the scripted origin serves and the transcodes
    /// re-encode. The fixture dir is removed when the test ends.
    struct MediaFixture {
        _dir: crate::test_support::TempDir,
        bytes: Vec<u8>,
    }

    impl MediaFixture {
        fn new(tag: &str, duration_secs: u32) -> Self {
            let dir = crate::test_support::TempDir::new(&format!("e2e-{tag}"));
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

    /// The real staging check stage with tight test limits and an isolated
    /// temp dir; the dir is removed when the returned [`TempDir`] drops.
    fn test_staging(chunk_size: u64) -> (StagingCheckStage, crate::test_support::TempDir) {
        let temp_dir = crate::test_support::TempDir::new("e2e-staging");
        (
            StagingCheckStage::new(
                reqwest::Client::new(),
                StageLimits {
                    chunk_size,
                    concurrency: 3,
                    attempt_timeout: Duration::from_secs(10),
                    stage_timeout: Duration::from_secs(120),
                    ..staging_test_limits()
                },
                temp_dir.path().to_path_buf(),
            ),
            temp_dir,
        )
    }

    /// A staged resolver wired like production: direct-media provider,
    /// reachable preflight, the real staging stage behind an open gate. The
    /// gate is injected (issue #177): tests pass one of their own capacity;
    /// a single transcode never queues, so its size is irrelevant there.
    fn e2e_resolver(
        origin_url: Url,
        staging: StagingCheckStage,
        stage_gate: Arc<Semaphore>,
    ) -> ProviderStreamUrlResolver {
        ProviderStreamUrlResolver::new(
            Arc::new(OriginProvider(origin_url)),
            Arc::new(stream_url::TcpPreflight::with_timeout(Duration::from_secs(
                2,
            ))),
            Some(Arc::new(staging)),
            true,
            Some(stage_gate),
        )
    }

    /// Consume a transcode stream to its end, collecting the audio bytes.
    async fn collect_transcode<S, E>(mut stream: S) -> Vec<u8>
    where
        S: futures::Stream<Item = Result<Bytes, E>> + std::marker::Unpin,
        E: std::error::Error,
    {
        use futures::StreamExt;
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(e) => panic!("the transcode must complete: {e}"),
            }
        }
        out
    }

    #[tokio::test]
    async fn staged_transcode_of_throttled_origin_completes_in_seconds() {
        // The measured-throttle regression, end to end (issue #170): the
        // scripted origin reproduces the EURONET edge — open-ended reads
        // crawl at ~27 KB/s, bounded ranges serve at full speed. The whole
        // transcode — resolution, source stage, ffmpeg re-encode — must
        // complete in seconds, from a staged file byte-identical to the
        // origin. (A regression to the open-ended fetch would spend over a
        // minute on this source's stage alone.)
        let fixture = MediaFixture::new("throttled-e2e", 240);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                throttle_open_ended: true,
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let (staging, _stage_dir) = test_staging(1024 * 1024);
        let resolver = e2e_resolver(origin.url(), staging, stage_gate_of(4));
        let media = e2e_media_url();

        let started = Instant::now();
        let staged = match resolver
            .resolve(&media)
            .await
            .expect("resolution with staging must succeed")
        {
            ResolvedSource::StagedFile(guard) => guard,
            other => panic!("a direct-media source must stage, got {other:?}"),
        };
        let staged_path = staged.path().to_path_buf();
        let staged_elapsed = started.elapsed();
        assert_eq!(
            std::fs::read(&staged_path).unwrap(),
            fixture.bytes,
            "the staged file must be byte-identical to the origin"
        );
        assert!(
            staged_elapsed < Duration::from_secs(20),
            "the stage must not crawl: took {staged_elapsed:?} \
             (open-ended at ~27 KB/s would need over a minute)"
        );

        let audio = collect_transcode(
            Transcoder::with_resolver(
                &e2e_params(e2e_expected_bytes(240)),
                &FixedResolver(FixedSource::Staged(staged_path.clone())),
            )
            .await
            .expect("a staged source must build the transcoder")
            .get_transcode_stream(),
        )
        .await;
        let total_elapsed = started.elapsed();
        assert_eq!(
            audio.len(),
            e2e_expected_bytes(240),
            "the transcode must deliver its declared length"
        );
        assert!(
            total_elapsed < Duration::from_secs(45),
            "the whole staged transcode must complete in seconds: took {total_elapsed:?}"
        );
        // Staging used ranged requests only, and ffmpeg did no network I/O.
        assert_eq!(
            origin.requests_at(None),
            0,
            "no open-ended GET may be issued on the staged path"
        );
        // The transcode ended; the staged file died with it.
        assert!(!staged_path.exists());
    }

    #[tokio::test]
    async fn range_ignored_origin_transcodes_end_to_end_via_the_open_ended_fallback() {
        // Rung 2 of the fallback ladder, end to end (issue #171): a scripted
        // origin that answers every GET with a 200 full body — ranged
        // staging unsupported — still transcodes. The probe's 200 engages
        // the open-ended fallback on its own (no operator action, no second
        // request storm), the staged file is byte-identical, and ffmpeg
        // reads it locally to a complete transfer.
        let fixture = MediaFixture::new("range-ignored-e2e", 30);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                ignore_range: true,
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let (staging, _stage_dir) = test_staging(1024 * 1024);
        let resolver = e2e_resolver(origin.url(), staging, stage_gate_of(4));
        let media = e2e_media_url();

        let staged = match resolver
            .resolve(&media)
            .await
            .expect("a range-ignoring origin must stage via the open-ended fallback")
        {
            ResolvedSource::StagedFile(guard) => guard,
            other => panic!("a direct-media source must stage, got {other:?}"),
        };
        let staged_path = staged.path().to_path_buf();
        assert_eq!(
            std::fs::read(&staged_path).unwrap(),
            fixture.bytes,
            "the staged file must be byte-identical to the origin"
        );
        // One ranged probe, one open-ended GET — the whole ladder, no storm.
        assert_eq!(origin.requests_at(Some(0)), 1, "one ranged probe");
        assert_eq!(origin.requests_at(None), 1, "one open-ended GET");
        assert_eq!(origin.total_requests(), 2);

        let audio = collect_transcode(
            Transcoder::with_resolver(
                &e2e_params(e2e_expected_bytes(30)),
                &FixedResolver(FixedSource::Staged(staged_path.clone())),
            )
            .await
            .expect("a staged source must build the transcoder")
            .get_transcode_stream(),
        )
        .await;
        assert_eq!(
            audio.len(),
            e2e_expected_bytes(30),
            "the transcode must deliver its declared length"
        );
        // The transcode ended; the staged file died with it.
        assert!(!staged_path.exists());
    }

    #[tokio::test]
    async fn staged_path_produces_the_legacy_path_audio() {
        // The ffmpeg command consumes the staged file and the produced audio
        // is equivalent to the legacy path's output: same origin, same
        // encoder settings — byte-identical mp3 out of a file read and a
        // network read of the same bytes.
        let fixture = MediaFixture::new("equivalence", 30);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let expected = e2e_expected_bytes(30);

        // Legacy: the resolved Stream URL goes straight to ffmpeg.
        let legacy = Transcoder::with_resolver(
            &e2e_params(expected),
            &ProviderStreamUrlResolver::new(
                Arc::new(OriginProvider(origin.url())),
                Arc::new(stream_url::TcpPreflight::with_timeout(Duration::from_secs(
                    2,
                ))),
                None,
                false,
                None,
            ),
        )
        .await
        .expect("the legacy path must build the transcoder");
        let legacy_audio = collect_transcode(legacy.get_transcode_stream()).await;
        assert_eq!(legacy_audio.len(), expected);

        // Staged: the real staging stage fetches the same origin — in small
        // chunks so the file is assembled from several ranged GETs — and
        // ffmpeg reads the file from disk.
        let (staging, _stage_dir) = test_staging(64 * 1024);
        let staged = match e2e_resolver(origin.url(), staging, stage_gate_of(4))
            .resolve(&e2e_media_url())
            .await
            .expect("staging must succeed")
        {
            ResolvedSource::StagedFile(guard) => guard,
            other => panic!("a direct-media source must stage, got {other:?}"),
        };
        let staged_path = staged.path().to_path_buf();
        assert_eq!(std::fs::read(&staged_path).unwrap(), fixture.bytes);
        let staged_audio = collect_transcode(
            Transcoder::with_resolver(
                &e2e_params(expected),
                &FixedResolver(FixedSource::Staged(staged_path.clone())),
            )
            .await
            .expect("a staged source must build the transcoder")
            .get_transcode_stream(),
        )
        .await;

        assert_eq!(staged_audio.len(), expected);
        assert_eq!(
            staged_audio, legacy_audio,
            "the staged path must produce the legacy path's audio, byte for byte"
        );
        // The transcode ended; the staged file died with it.
        assert!(!staged_path.exists());
    }

    #[tokio::test]
    async fn failed_transcode_deletes_the_staged_file() {
        // A staged source ffmpeg cannot decode fails the transcode — and the
        // staged file is deleted with it: the guard dies with the stream.
        // The origin's index-pattern content has no 0xFF byte, so no mp3
        // frame sync can ever appear in it: ffmpeg fails fast on it.
        let origin = ScriptedOrigin::start(5 * 1024, OriginScript::default());
        let (staging, _stage_dir) = test_staging(1024);
        let staged = match e2e_resolver(origin.url(), staging, stage_gate_of(4))
            .resolve(&e2e_media_url())
            .await
            .expect("staging the garbage source must succeed")
        {
            ResolvedSource::StagedFile(guard) => guard,
            other => panic!("a direct-media source must stage, got {other:?}"),
        };
        let staged_path = staged.path().to_path_buf();
        assert!(staged_path.exists());

        let transcoder = Transcoder::with_resolver(
            &e2e_params(1000),
            &FixedResolver(FixedSource::Staged(staged_path.clone())),
        )
        .await
        .expect("a staged source must build the transcoder");
        use futures::StreamExt;
        let mut stream = transcoder.get_transcode_stream();
        let mut stream_err = None;
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                stream_err = Some(e);
                break;
            }
        }
        let err = stream_err.expect("a garbage staged source must fail the transcode");
        info!("failed transcode as expected: {err}");
        // The guard dies with the stream — dropping it here is the transcode
        // ending in failure (a full consumption would end it the same way).
        drop(stream);
        assert!(
            !staged_path.exists(),
            "the failed transcode must delete the staged file"
        );
    }

    #[tokio::test]
    async fn client_gone_deletes_the_staged_file() {
        // The client walking away drops the transcode stream mid-flight;
        // the staged file is deleted with it, not left for the next request.
        let fixture = MediaFixture::new("client-gone", 30);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let (staging, _stage_dir) = test_staging(64 * 1024);
        let staged = match e2e_resolver(origin.url(), staging, stage_gate_of(4))
            .resolve(&e2e_media_url())
            .await
            .expect("staging must succeed")
        {
            ResolvedSource::StagedFile(guard) => guard,
            other => panic!("a direct-media source must stage, got {other:?}"),
        };
        let staged_path = staged.path().to_path_buf();

        let transcoder = Transcoder::with_resolver(
            &e2e_params(e2e_expected_bytes(30)),
            &FixedResolver(FixedSource::Staged(staged_path.clone())),
        )
        .await
        .expect("a staged source must build the transcoder");
        use futures::StreamExt;
        let mut stream = transcoder.get_transcode_stream();
        let first = stream
            .next()
            .await
            .expect("the transcode must produce audio")
            .expect("the first chunk must be good");
        assert!(!first.is_empty());

        drop(stream); // the client walked away mid-transcode
        assert!(
            !staged_path.exists(),
            "the staged file dies with the abandoned transcode"
        );
    }

    #[tokio::test]
    async fn concurrent_transcodes_beyond_the_staging_limit_queue() {
        // Operational hardening (issue #172): concurrent staged transcodes
        // are bounded by the staging concurrency (STAGE_CONCURRENCY, 4
        // unset) — one slot per staged file on disk, taken before the stage
        // creates the file and held until the transcode deletes it. With
        // more transcodes in flight than slots (three beyond this gate's
        // custom capacity of two), the excess queue instead of staging in
        // parallel: at most `limit` staged files ever exist at once, and
        // every transcode still completes. The gate is built and injected
        // right here (issue #177), so the capacity the test pins is the
        // test's own, not the environment's.
        let limit = 2;
        let fixture = MediaFixture::new("load", 30);
        let origin = ScriptedOrigin::start(
            0,
            OriginScript {
                content_override: Some(fixture.bytes.clone()),
                ..OriginScript::default()
            },
        );
        let (staging, stage_dir) = test_staging(64 * 1024);
        let resolver = Arc::new(e2e_resolver(
            origin.url(),
            staging,
            stage_gate_of(limit as u64),
        ));
        let media = e2e_media_url();
        let expected = e2e_expected_bytes(30);

        // Sample the staged temp dir for the whole run: the invariant is
        // held continuously (a file exists only while its transcode holds a
        // slot), so sampling just has to keep up.
        let max_seen = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let dir = stage_dir.path().to_path_buf();
            let max_seen = max_seen.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let staged = std::fs::read_dir(&dir)
                        .map(|entries| {
                            entries
                                .flatten()
                                .filter(|entry| {
                                    entry
                                        .file_name()
                                        .to_string_lossy()
                                        .starts_with(STAGED_FILE_PREFIX)
                                })
                                .count()
                        })
                        .unwrap_or(0);
                    max_seen.fetch_max(staged, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };

        let mut transcodes = Vec::new();
        for _ in 0..limit + 3 {
            let resolver = resolver.clone();
            let media = media.clone();
            transcodes.push(tokio::spawn(async move {
                let params = FfmpegParameters {
                    seek_time: 0.0,
                    url: media,
                    audio_codec: AudioCodec::MP3,
                    bitrate_kbit: 32,
                    max_rate_kbit: 960,
                    expected_bytes_count: expected,
                    timeout_in_seconds: 600,
                };
                let transcoder = Transcoder::with_resolver(&params, resolver.as_ref())
                    .await
                    .expect("a staged transcode must build");
                collect_transcode(transcoder.get_transcode_stream()).await
            }));
        }
        let mut completed = 0;
        for transcode in transcodes {
            let audio = transcode.await.expect("the transcode task must not panic");
            assert_eq!(
                audio.len(),
                expected,
                "every queued transcode must still deliver its full audio"
            );
            completed += 1;
        }
        stop.store(true, Ordering::Relaxed);
        sampler.await.unwrap();

        assert_eq!(
            completed,
            limit + 3,
            "all transcodes complete, the excess queued"
        );
        let peak = max_seen.load(Ordering::Relaxed);
        assert!(
            peak >= limit.min(2),
            "the transcodes must actually overlap (peak {peak}) — \
             the load must exercise the gate, not serialize"
        );
        assert!(
            peak <= limit,
            "at most the staging concurrency ({limit}) staged files may \
             exist at once, peak {peak}"
        );
        let left = std::fs::read_dir(stage_dir.path())
            .unwrap()
            .filter(|entry| entry.as_ref().is_ok())
            .count();
        assert_eq!(left, 0, "every staged file dies with its transcode");
    }
}
