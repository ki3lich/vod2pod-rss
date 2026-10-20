//! The deep transcode-request hand-off (issue #184).
//!
//! The route states the raw request — media URL, duration, bitrate, range —
//! plus the deployment's ffmpeg timeout and target codec, and this module
//! derives everything else, once: the byte window and its seek offset, the
//! expected byte count (the one shared formula, ADR-0002), the ffmpeg argv,
//! and the construction deadline. The route keeps only HTTP shaping —
//! status selection, headers, error mapping — and no hand-off fact is
//! computed by the caller anymore.

use std::process::Command;
use std::time::Duration;

use actix_web::http::header::HeaderValue;
use log::{debug, error, info};
use regex::Regex;
use url::Url;

use super::media_sizing::streamable_bytes;
use crate::configs::{AudioCodec, DEFAULT_FFMPEG_TIMEOUT_SECS};

/// The transcode route's raw request, as one deep value: the facts the
/// client stated — media URL, duration, bitrate, range — with the ffmpeg
/// timeout and target codec the deployment transcodes by. Construction
/// derives the byte window, the seek offset, the expected byte count, and
/// fails with [`TranscodeRequestError`] on a window the route can only map
/// to 400 or 416.
pub(crate) struct TranscodeRequest {
    /// The URL the enclosure pointed at — what resolution consumes. The
    /// transcoder's ffmpeg input is the *resolved* source behind it.
    pub(crate) media_url: Url,
    pub(crate) bitrate_kbit: usize,
    pub(crate) timeout_in_seconds: usize,
    /// Also the answer's content type: the route reads it for shaping.
    pub(crate) codec: AudioCodec,
    /// The derived byte window: what to serve, seeking where, and whether
    /// the answer is 206 or 200.
    pub(crate) window: TranscodeWindow,
}

/// Why the request cannot be built: a malformed Range header maps to 400, a
/// window past the advertised length to 416 — the mapping is the route's,
/// the verdict is this module's.
#[derive(Debug, PartialEq)]
pub(crate) enum TranscodeRequestError {
    MalformedRange(String),
    RangeNotSatisfiable,
}

impl TranscodeRequest {
    /// Derive the request from its raw facts. The advertised length comes
    /// from the one shared byte-count formula ([`streamable_bytes`]) — the
    /// same call the RSS transcodizer makes for the enclosure `length`
    /// attribute, so the advertised size always equals the served size
    /// (ADR-0002).
    pub(crate) fn new(
        media_url: Url,
        duration_secs: usize,
        bitrate_kbit: usize,
        range_header: Option<&HeaderValue>,
        timeout_in_seconds: usize,
        codec: AudioCodec,
    ) -> Result<Self, TranscodeRequestError> {
        let total_streamable_bytes =
            usize::try_from(streamable_bytes(duration_secs as u64, bitrate_kbit as u64))
                .unwrap_or(usize::MAX);

        let window = decide_transcode_window(range_header, total_streamable_bytes, duration_secs)?;

        Ok(Self {
            media_url,
            bitrate_kbit,
            timeout_in_seconds,
            codec,
            window,
        })
    }

    /// The deadline the transcoder's construction (resolution, staging, the
    /// ffmpeg spawn) runs under: the configured ffmpeg timeout — the value
    /// the startup wiring asserts dominates the Stage Deadline (issue #182),
    /// because resolution and staging happen inside this budget.
    pub(crate) fn construction_deadline(&self) -> Duration {
        Duration::from_secs(
            self.timeout_in_seconds
                .try_into()
                .unwrap_or(DEFAULT_FFMPEG_TIMEOUT_SECS),
        )
    }

    /// The ffmpeg argv for this request, reading `input_url` — the resolved
    /// source (Stream URL or staged file) the resolver hands over, the one
    /// fact the transcoder adds. Everything else derives from the request:
    /// the seek offset, the codec, the bitrate family (`-ab`/`-bufsize`/
    /// `-maxrate`, the max rate being the bitrate's 30× headroom), and the
    /// ffmpeg-side timeout.
    pub(crate) fn ffmpeg_command(&self, input_url: &Url) -> Command {
        debug!("generating ffmpeg command");
        let mut command = Command::new("ffmpeg");
        let command_ref = &mut command;
        // the max-rate family shares one headroom: 30× the bitrate, for both
        // the VBR cap (with its "k" suffix) and the decoder buffer (without)
        let max_rate_kbit = self.bitrate_kbit * 30;

        command_ref
            .args(["-ss", self.window.seek_secs.to_string().as_str()])
            .args([
                "-protocol_whitelist",
                "file,http,https,tcp,tls",
                "-i",
                input_url.as_str(),
            ])
            .args(["-acodec", self.codec.get_ffmpeg_codec_str()])
            .args(["-threads", "0"])
            .args(["-ab", format!("{}k", self.bitrate_kbit).as_str()])
            .args(["-f", self.codec.get_extension_str()])
            .args(["-bufsize", max_rate_kbit.to_string().as_str()])
            .args(["-maxrate", format!("{max_rate_kbit}k").as_str()])
            .args(["-timeout", self.timeout_in_seconds.to_string().as_str()])
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
}

/// The byte window a transcode serves: derived from the raw range against
/// the advertised length. The route echoes its counts in the Content-Range
/// and Content-Length headers and picks 200 vs 206 from `partial` — reading
/// the derived facts, never computing them (issue #184).
#[derive(Debug, PartialEq)]
pub(crate) struct TranscodeWindow {
    /// The advertised length both answers echo in Content-Range.
    pub(crate) total_streamable_bytes: usize,
    pub(crate) start_bytes: usize,
    pub(crate) end_bytes: usize,
    /// The count the transcode stream must deliver — the window's own
    /// length; the stdout pump pads or truncates to exactly this.
    pub(crate) expected_bytes: usize,
    /// Where the window starts in the source, in seconds.
    pub(crate) seek_secs: f32,
    /// True when the window starts mid-stream: the answer is 206, not 200.
    /// A seek within a tenth of a second of the start still counts as "from
    /// the start" — podcatchers send `bytes=0-` with rounding noise.
    pub(crate) partial: bool,
}

/// Parse a Range header into the requested byte window (inclusive on both
/// ends), defaulting the end to the last advertised byte. Whether the window
/// can be served at all is the decision core's call — the caller checks it
/// before deriving any byte counts from the pair, so a reversed or
/// past-the-end window never reaches arithmetic that would underflow on it
/// (surfaced by the decision-core pins, issue #183).
fn parse_range_header(
    content_range_str: &str,
    advertised_bytes: usize,
) -> eyre::Result<(usize, usize)> {
    let re = Regex::new(r"(?P<start>[0-9]{1,20})-?(?P<end>[0-9]{1,20})?")?;
    let captures = if let Some(x) = re.captures_iter(content_range_str).next() {
        x
    } else {
        return Err(eyre::eyre!("content range regex failed"));
    };

    let mut start = 0;
    if let Some(x) = captures.name("start") {
        start = x.as_str().parse()?;
    }

    if advertised_bytes == 0 {
        error!("The requested Range header with a length of 0 is invalid: {content_range_str}");
        return Err(eyre::eyre!(
            "The requested Range header with a length of 0 is invalid: {content_range_str}"
        ));
    }
    let mut end = advertised_bytes - 1;
    if let Some(x) = captures.name("end") {
        end = x.as_str().parse()?;
    }

    if end == start {
        return Err(eyre::eyre!(
            "The requested Range header with a length of 0 is invalid: {content_range_str}"
        ));
    }

    Ok((start, end))
}

/// The transcode request's decision core: from the raw request facts — the
/// Range header (if any), the advertised byte count and the declared
/// duration — decide the byte window to serve, the seek it maps to in the
/// source, and whether the answer is partial. Pure, like the server's
/// `conditional_response` beside it: the route maps the verdict onto HTTP
/// (issue #183), and since #184 the verdict arrives here, inside the
/// transcoder, before the route sees anything but a built request.
fn decide_transcode_window(
    range_header: Option<&HeaderValue>,
    total_streamable_bytes: usize,
    duration_secs: usize,
) -> Result<TranscodeWindow, TranscodeRequestError> {
    const DEFAULT_CONTENT_RANGE: &str = "0-";
    let content_range_str = match range_header {
        Some(x) => x.to_str().unwrap_or_default(),
        None => DEFAULT_CONTENT_RANGE,
    };

    debug!("received content range {content_range_str}");

    let (start_bytes, end_bytes) = parse_range_header(content_range_str, total_streamable_bytes)
        .map_err(|e| TranscodeRequestError::MalformedRange(e.to_string()))?;

    debug!("requested content-range: bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}");

    // Before any byte arithmetic: a window past the advertised length is
    // not satisfiable. Checking here keeps the window count below free of
    // the underflow a reversed or past-the-end start used to produce
    // (surfaced by the decision-core pins, issue #183).
    if start_bytes > end_bytes || start_bytes > total_streamable_bytes {
        return Err(TranscodeRequestError::RangeNotSatisfiable);
    }

    let expected_bytes = end_bytes - start_bytes + 1;

    let seek_secs =
        ((start_bytes as f32) / (total_streamable_bytes as f32)) * (duration_secs as f32);
    debug!("choosen seek_time: {seek_secs}");

    Ok(TranscodeWindow {
        total_streamable_bytes,
        start_bytes,
        end_bytes,
        expected_bytes,
        seek_secs,
        partial: seek_secs > 0.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_start_and_end_start_to_end() {
        let content_range_str = "bytes=0-99";
        let advertised_bytes = 100;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (0, 99));
    }

    #[test]
    fn test_get_start_and_end_middle1_to_middle2() {
        let content_range_str = "bytes=50-199";
        let advertised_bytes = 200;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (50, 199));
    }

    #[test]
    fn test_get_start_and_end_middle_to_undefined() {
        let content_range_str = "bytes=100-";
        let advertised_bytes = 200;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (100, 199));
    }

    #[test]
    fn test_get_start_and_end_start_to_undefined() {
        let content_range_str = "bytes=0-";
        let advertised_bytes = 200;
        let (start, end) = parse_range_header(content_range_str, advertised_bytes).unwrap();
        assert_eq!((start, end), (0, 199));
    }

    #[test]
    fn test_get_start_and_end_past_the_advertised_length_does_not_underflow() {
        // The parse is window-shape only: a start past the advertised length
        // is the 416 case, decided by the caller before any count is derived
        // — it must not underflow the arithmetic on the way there.
        let (start, end) = parse_range_header("bytes=9000-", 8000).unwrap();
        assert_eq!((start, end), (9000, 7999));
    }

    // ---- the decision core's unit pins (issue #183) ----
    //
    // The request's decisions — window, seek, partial — as pure function
    // pins, beside the core they pin. Each pin is a verdict a podcatcher
    // would observe: which bytes, seeking where, 200 or 206 or an error.

    fn range(value: &str) -> Option<&'static HeaderValue> {
        // Leaked on purpose: the decision core borrows the header, and the
        // pins have no owner to hold it.
        Some(Box::leak(Box::new(HeaderValue::from_str(value).unwrap())))
    }

    /// The pin episode: 2 s of audio at 32 kbit — the same numbers the
    /// route-level tests (issue #183) use as literals, and the shared
    /// formula's worked example.
    const PIN_DURATION_SECS: usize = 2;
    const PIN_BITRATE_KBIT: usize = 32;
    /// 2 × 32 × 1000 / 8 — the test's own arithmetic, not the code's.
    const PIN_TOTAL_BYTES: usize = 8000;

    #[test]
    fn no_range_covers_the_whole_stream_from_the_start() {
        let window = decide_transcode_window(None, PIN_TOTAL_BYTES, PIN_DURATION_SECS).unwrap();
        assert_eq!(window.start_bytes, 0);
        assert_eq!(window.end_bytes, PIN_TOTAL_BYTES - 1);
        assert_eq!(window.expected_bytes, PIN_TOTAL_BYTES);
        assert_eq!(window.seek_secs, 0.0);
        assert!(!window.partial, "a whole-stream window is a 200");
    }

    #[test]
    fn an_open_ended_mid_stream_window_seeks_and_serves_206() {
        let window =
            decide_transcode_window(range("bytes=4000-"), PIN_TOTAL_BYTES, PIN_DURATION_SECS)
                .unwrap();
        assert_eq!(window.start_bytes, 4000);
        assert_eq!(window.end_bytes, PIN_TOTAL_BYTES - 1);
        assert_eq!(window.expected_bytes, PIN_TOTAL_BYTES - 4000);
        assert_eq!(
            window.seek_secs, 1.0,
            "4000 of 8000 bytes into a 2 s stream"
        );
        assert!(window.partial, "a mid-stream window is a 206");
    }

    #[test]
    fn a_bounded_window_keeps_the_requested_end_and_serves_200_while_the_seek_is_tiny() {
        // `bytes=50-199` of an 8000-byte stream: the seek is 50/8000 × 2 s =
        // 0.0125 s — inside the "from the start" threshold, so 200.
        let window =
            decide_transcode_window(range("bytes=50-199"), PIN_TOTAL_BYTES, PIN_DURATION_SECS)
                .unwrap();
        assert_eq!(
            (window.start_bytes, window.end_bytes, window.expected_bytes),
            (50, 199, 150)
        );
        assert!(!window.partial);
    }

    #[test]
    fn a_seek_of_exactly_a_tenth_of_a_second_is_still_from_the_start() {
        // 400 of 8000 bytes into a 2 s stream is exactly the 0.1 s
        // threshold — `<=` keeps it a 200. Podcatchers send `bytes=0-` with
        // rounding noise; the threshold absorbs it.
        let window =
            decide_transcode_window(range("bytes=400-"), PIN_TOTAL_BYTES, PIN_DURATION_SECS)
                .unwrap();
        assert_eq!(window.seek_secs, 0.1);
        assert!(!window.partial);
    }

    #[test]
    fn a_window_past_the_advertised_length_is_not_satisfiable() {
        assert_eq!(
            decide_transcode_window(range("bytes=9000-"), PIN_TOTAL_BYTES, PIN_DURATION_SECS),
            Err(TranscodeRequestError::RangeNotSatisfiable)
        );
    }

    #[test]
    fn a_malformed_range_is_a_bad_request() {
        assert!(matches!(
            decide_transcode_window(range("bytes=abc"), PIN_TOTAL_BYTES, PIN_DURATION_SECS),
            Err(TranscodeRequestError::MalformedRange(_))
        ));
    }

    #[test]
    fn a_zero_advertised_length_cannot_serve_any_window() {
        // duration 0 advertises 0 bytes — the enclosure arithmetic rejects
        // the zero-length window before any byte math can divide by it.
        assert!(matches!(
            decide_transcode_window(range("bytes=0-"), 0, 0),
            Err(TranscodeRequestError::MalformedRange(_))
        ));
    }

    // ---- the derivation pins (issue #184) ----
    //
    // The deep hand-off itself: from the raw request facts to the derived
    // window, deadline, and argv. Each pin's expected value is the test's
    // own arithmetic or a literal, never the code's recomputation.

    fn request_at(duration_secs: usize, bitrate_kbit: usize) -> TranscodeRequest {
        let media_url = Url::parse("https://www.youtube.com/watch?v=UMO52N2vpk0").unwrap();
        TranscodeRequest::new(
            media_url,
            duration_secs,
            bitrate_kbit,
            None,
            600,
            AudioCodec::MP3,
        )
        .expect("a whole-stream request of a positive length must build")
    }

    #[test]
    fn the_request_derives_the_advertised_length_from_the_raw_facts() {
        // 2 s at 32 kbit → 2 × 32 × 1000 / 8 = 8000 advertised bytes, served
        // whole from the start. The enclosure `length` attribute carries the
        // same formula's answer (ADR-0002), so this derived total is what
        // both the enclosure and the served Content-Length say.
        let request = request_at(PIN_DURATION_SECS, PIN_BITRATE_KBIT);
        assert_eq!(request.window.total_streamable_bytes, 8000);
        assert_eq!(request.window.expected_bytes, 8000);
        assert_eq!(request.window.seek_secs, 0.0);
        assert!(!request.window.partial, "a whole-stream request is a 200");
    }

    #[test]
    fn the_construction_deadline_is_the_configured_timeout() {
        // The timeout the caller states is, verbatim, the budget the
        // transcoder's construction runs under — the same ffmpeg timeout the
        // startup wiring asserts dominates the Stage Deadline (issue #182).
        let request = request_at(PIN_DURATION_SECS, PIN_BITRATE_KBIT);
        assert_eq!(request.construction_deadline(), Duration::from_secs(600));
    }

    #[test]
    fn check_ffmpeg_command() {
        // The argv derives from the raw request end to end: the range
        // `bytes=120000-` on a 60 s stream at 32 kbit (240000 advertised
        // bytes — the test's own arithmetic) seeks to the halfway byte,
        // which is 30 s in, and the timeout rides through to `-timeout`.
        // Exercised pure: no resolution, no preflight, no network.
        let media_url = Url::parse("http://url.mp3").unwrap();
        let request = TranscodeRequest::new(
            media_url,
            60,
            32,
            range("bytes=120000-"),
            600,
            AudioCodec::MP3,
        )
        .expect("a mid-stream window of a positive length must build");
        let input_url = Url::parse("http://resolved.example.com/audio.mp3").unwrap();

        let command = request.ffmpeg_command(&input_url);
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
                    assert_eq!(value, "30", "halfway into a 60 s stream");
                }
                Some("-protocol_whitelist") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-protocol_whitelist {}", value);
                    assert_eq!(value, "file,http,https,tcp,tls");
                }
                Some("-i") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-i {}", value);
                    assert_eq!(value, input_url.as_str());
                }
                Some("-acodec") => {
                    let value = args.next().unwrap().to_str().unwrap();
                    info!("-acodec {}", value);
                    assert_eq!(value, AudioCodec::MP3.get_ffmpeg_codec_str());
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
}
