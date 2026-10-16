mod stdout_pump;
mod stream_url;

use self::stdout_pump::{spawn_pump_threads, ChannelBytes};
use self::stream_url::{ProviderStreamUrlResolver, StreamUrlResolver};
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
        let stream_url = resolver.resolve(&ffmpeg_paramenters.url).await?;

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
        Gen::new(|co| generetor_coroutine(self.ffmpeg_command, self.expected_bytes_count, co))
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

    /// Resolver fake handing back one fixed stream URL.
    struct FixedResolver(Url);

    #[async_trait]
    impl StreamUrlResolver for FixedResolver {
        async fn resolve(&self, _media_url: &Url) -> eyre::Result<Url> {
            Ok(self.0.clone())
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

        let transcoder = Transcoder::with_resolver(&params, &FixedResolver(resolved.clone()))
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
}
