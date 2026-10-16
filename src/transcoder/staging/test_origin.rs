//! Test-only scripted HTTP origin encoding the googlevideo edge behavior
//! measured on 2026-10-16 (the investigation behind issue #166):
//!
//! - bounded ranged GETs serve at full speed (measured 4–19 MB/s);
//! - open-ended GETs — no Range header, unbounded, or oversized — throttle to
//!   ~27 KB/s (the measured ~256 KB per 10 s), from byte 0, cold or warm;
//! - plus fault modes for the staging engine's failure paths: connections
//!   dropped mid-body, Range ignored (200 full-body), 5xx answers, stalled
//!   responses, and Content-Range lies.
//!
//! The origin's content is a deterministic pattern of `index % 251`, so any
//! misaligned or missing write shows up in a byte-for-byte comparison
//! (251 is prime and never divides an 8 MiB chunk boundary).

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::http::{header, StatusCode};
use actix_web::{web, App, HttpResponse, HttpServer};
use reqwest::Url;

/// Bytes served per throttled tick.
const THROTTLE_CHUNK_BYTES: u64 = 1024;
/// ~26.9 KB/s together with [`THROTTLE_CHUNK_BYTES`] — the measured
/// ~256 KB per 10 s, rounded to a scheduler-friendly sleep.
const THROTTLE_SLEEP: Duration = Duration::from_millis(38);
/// Bounded spans up to this size serve at full speed (the measured fast
/// ceiling the fix relies on); bigger spans throttle like open-ended reads.
/// Measured: ≤ 8 MB fast, ≥ 18 MB throttled; the 8–18 MB gap is unmeasured
/// and the fixture takes the ceiling the staged fetch depends on.
pub const MAX_FAST_SPAN: u64 = 8 * 1024 * 1024;
/// Bytes served before a scripted mid-body drop aborts the connection.
pub const DROP_AFTER_BYTES: u64 = 4096;
/// Body pieces served per write at full speed.
const FAST_CHUNK_BYTES: u64 = 64 * 1024;
/// The origin content pattern period — prime, so it never aligns with any
/// power-of-two chunk boundary.
const PATTERN_PERIOD: usize = 251;
/// How long a [`FaultPolicy::stall`]ing handler holds the request open
/// before giving up; the client's attempt timeout always fires first.
const STALL_HOLD: Duration = Duration::from_secs(30);

/// How often a fault fires: never; only on the first `n` eligible requests;
/// on every eligible request — surviving retries, which is what a stage must
/// detect and fail on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPolicy {
    Never,
    First(u32),
    Always,
}

impl FaultPolicy {
    /// Whether this request triggers the fault, consuming one firing.
    fn consumes(&self, served: &mut u32) -> bool {
        match self {
            FaultPolicy::Never => false,
            FaultPolicy::First(n) => {
                if *served < *n {
                    *served += 1;
                    true
                } else {
                    false
                }
            }
            FaultPolicy::Always => {
                *served += 1;
                true
            }
        }
    }
}

/// The behavior a [`ScriptedOrigin`] serves with. Defaults to a healthy
/// origin whose bounded ranges are fast and whose open-ended reads crawl.
#[derive(Clone, Debug)]
pub struct OriginScript {
    /// The measured throttle mode: open-ended GETs crawl at ~27 KB/s while
    /// bounded ranges serve at full speed. Off: everything serves fast.
    pub throttle_open_ended: bool,
    /// The origin answers any GET with a 200 full body, Range or not.
    pub ignore_range: bool,
    /// The origin reports this total in every Content-Range regardless of
    /// the content it actually holds — the oversized-lie mode. Ranges beyond
    /// the real content are then answered 416 against the lying total.
    pub lie_total: Option<u64>,
    /// If set, the first request reports the real total and every later
    /// request reports this lying total — an origin drifting its reported
    /// size mid-stage, which the engine must catch as a contradiction.
    pub lie_total_after_first: Option<u64>,
    /// Every request (probe included) answers 5xx.
    pub server_error: FaultPolicy,
    /// Every body aborts mid-stream after [`DROP_AFTER_BYTES`].
    pub drop_mid_body: FaultPolicy,
    /// Requests whose Range starts exactly at these offsets answer 5xx per
    /// policy — chunk-level transient/permanent scripting.
    pub server_errors_at_offset: Vec<(u64, FaultPolicy)>,
    /// Requests whose Range starts exactly at these offsets drop mid-body
    /// per policy — chunk-level transient/permanent scripting.
    pub drops_at_offset: Vec<(u64, FaultPolicy)>,
    /// Open-ended requests (no Range header) drop mid-body per policy — the
    /// Range-ignored fallback's scripting. Scoped to the open-ended shape so
    /// a probe's ranged request is never the one that drops: an aborting
    /// response body kills the connection before its head, and the probe
    /// then sees transport failures instead of the 200 the fallback keys
    /// off.
    pub drops_open_ended: FaultPolicy,
    /// Requests the origin answers by holding the connection open with no
    /// response at all — the client's attempt timeout is what ends these.
    pub stall: FaultPolicy,
    /// Serve these bytes instead of the index pattern — a real media fixture
    /// (e.g. a lavfi-generated tone) so an end-to-end transcode can run
    /// against the origin. The byte length then comes from the content, not
    /// `content_len`.
    pub content_override: Option<Vec<u8>>,
}

impl Default for OriginScript {
    fn default() -> Self {
        Self {
            throttle_open_ended: false,
            ignore_range: false,
            lie_total: None,
            lie_total_after_first: None,
            server_error: FaultPolicy::Never,
            drop_mid_body: FaultPolicy::Never,
            server_errors_at_offset: Vec::new(),
            drops_at_offset: Vec::new(),
            drops_open_ended: FaultPolicy::Never,
            stall: FaultPolicy::Never,
            content_override: None,
        }
    }
}

struct OriginState {
    script: OriginScript,
    server_errors_served: u32,
    drops_served: u32,
    open_ended_drops_served: u32,
    stalls_served: u32,
    offset_error_served: HashMap<u64, u32>,
    offset_drops_served: HashMap<u64, u32>,
    /// Requests seen per Range start (`None` for open-ended GETs).
    requests: HashMap<Option<u64>, u32>,
}

struct OriginShared {
    content: Arc<Vec<u8>>,
    state: Arc<Mutex<OriginState>>,
}

/// A scripted origin server on `127.0.0.1:0`, alive until the test process
/// ends. One OS thread runs its actix server; tests talk to it over loopback
/// TCP with plain reqwest — the same transports the staging engine uses.
pub struct ScriptedOrigin {
    base_url: Url,
    content: Arc<Vec<u8>>,
    state: Arc<Mutex<OriginState>>,
}

/// What one request will get, decided up front under the state lock so no
/// await point runs while it is held.
enum Decision {
    ServerError,
    Stall,
    RangeNotSatisfiable {
        content_range: String,
    },
    Serve {
        status: StatusCode,
        content_range: Option<String>,
        slice_start: u64,
        slice_len: u64,
        throttled: bool,
        drop_after: Option<u64>,
    },
}

/// The Range intent of a request: none (what ffmpeg sends), `bytes=a-`, or
/// `bytes=a-b` (inclusive). Anything unparseable counts as none.
enum RangeRequest {
    Open,
    OpenFrom(u64),
    Bounded { start: u64, end: u64 },
}

fn parse_range_request(header: Option<&str>) -> RangeRequest {
    let Some(value) = header else {
        return RangeRequest::Open;
    };
    let Some(rest) = value.trim().strip_prefix("bytes=") else {
        return RangeRequest::Open;
    };
    let rest = rest.trim();
    if rest == "-" {
        // Suffix ranges are not part of any staged exchange; serve openly.
        return RangeRequest::Open;
    }
    if let Some(start) = rest.strip_suffix('-') {
        return start
            .parse()
            .map(RangeRequest::OpenFrom)
            .unwrap_or(RangeRequest::Open);
    }
    match rest.split_once('-') {
        Some((start, end)) => match (start.parse(), end.parse()) {
            (Ok(start), Ok(end)) => RangeRequest::Bounded { start, end },
            _ => RangeRequest::Open,
        },
        None => RangeRequest::Open,
    }
}

fn offset_fault_consumes(
    entries: &[(u64, FaultPolicy)],
    counters: &mut HashMap<u64, u32>,
    key: Option<u64>,
) -> bool {
    let Some(key) = key else {
        return false;
    };
    let Some((_, policy)) = entries.iter().find(|(offset, _)| *offset == key) else {
        return false;
    };
    policy.consumes(counters.entry(key).or_insert(0))
}

impl ScriptedOrigin {
    /// Start the origin holding `content_len` bytes of the index-pattern
    /// content, serving `script`.
    pub fn start(content_len: usize, script: OriginScript) -> Self {
        let content: Arc<Vec<u8>> = Arc::new(match script.content_override.clone() {
            Some(bytes) => bytes,
            None => (0..content_len)
                .map(|i| (i % PATTERN_PERIOD) as u8)
                .collect(),
        });
        let state = Arc::new(Mutex::new(OriginState {
            script,
            server_errors_served: 0,
            drops_served: 0,
            open_ended_drops_served: 0,
            stalls_served: 0,
            offset_error_served: HashMap::new(),
            offset_drops_served: HashMap::new(),
            requests: HashMap::new(),
        }));
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("scripted origin: bind");
        let addr = listener.local_addr().expect("scripted origin: local addr");
        let shared = web::Data::new(OriginShared {
            content: content.clone(),
            state: state.clone(),
        });
        std::thread::Builder::new()
            .name("scripted-origin".to_string())
            .spawn(move || {
                actix_web::rt::System::new().block_on(async move {
                    HttpServer::new(move || {
                        App::new()
                            .app_data(shared.clone())
                            .default_service(web::to(serve_scripted))
                    })
                    .listen(listener)
                    .expect("scripted origin: listen")
                    .workers(1)
                    .run()
                    .await
                    .expect("scripted origin: run");
                });
            })
            .expect("scripted origin: spawn thread");
        Self {
            base_url: Url::parse(&format!("http://{addr}/videoplayback")).expect("origin url"),
            content,
            state,
        }
    }

    /// The URL the staged fetch (and raw probes) read from.
    pub fn url(&self) -> Url {
        self.base_url.clone()
    }

    /// The bytes the origin holds — the truth a staged file is compared
    /// against.
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// How many requests arrived whose Range started at `offset`
    /// (`None` for open-ended GETs).
    pub fn requests_at(&self, offset: Option<u64>) -> u32 {
        self.state
            .lock()
            .unwrap()
            .requests
            .get(&offset)
            .copied()
            .unwrap_or(0)
    }

    /// How many requests arrived in total.
    pub fn total_requests(&self) -> u32 {
        self.state.lock().unwrap().requests.values().sum()
    }
}

async fn serve_scripted(
    req: actix_web::HttpRequest,
    origin: web::Data<OriginShared>,
) -> HttpResponse {
    let range_header = req
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let requested = parse_range_request(range_header.as_deref());
    let content_len = origin.content.len() as u64;

    // Everything is decided before any await: the state lock is held only
    // while consuming fault policies and recording the request.
    let decision = {
        let mut state = origin.state.lock().unwrap();
        let offset_key = match &requested {
            RangeRequest::Open => None,
            RangeRequest::OpenFrom(start) | RangeRequest::Bounded { start, .. } => Some(*start),
        };
        *state.requests.entry(offset_key).or_insert(0) += 1;
        let script = state.script.clone();
        let display_total = match script.lie_total {
            Some(lie) => lie,
            None => match script.lie_total_after_first {
                // The probe learns the truth; every later request sees the
                // lie — the drift the engine must catch as a contradiction.
                Some(lie) if state.requests.values().sum::<u32>() > 1 => lie,
                _ => content_len,
            },
        };
        // The drop counter is consumed only for requests that actually
        // stream a body — a 503 or a stall never drops. The open-ended
        // policy fires only for the open-ended shape (no Range header): an
        // aborting body kills the connection before its head, and the probe
        // must keep seeing its 200 for the fallback to key off.
        let open_ended = matches!(requested, RangeRequest::Open);
        let drop_fires = |state: &mut OriginState| {
            script.drop_mid_body.consumes(&mut state.drops_served)
                || offset_fault_consumes(
                    &script.drops_at_offset,
                    &mut state.offset_drops_served,
                    offset_key,
                )
                || (open_ended
                    && script
                        .drops_open_ended
                        .consumes(&mut state.open_ended_drops_served))
        };

        if script
            .server_error
            .consumes(&mut state.server_errors_served)
            || offset_fault_consumes(
                &script.server_errors_at_offset,
                &mut state.offset_error_served,
                offset_key,
            )
        {
            Decision::ServerError
        } else if script.stall.consumes(&mut state.stalls_served) {
            Decision::Stall
        } else if script.ignore_range {
            // A 200 full body regardless of the Range header — open-ended by
            // nature, so the measured throttle mode applies to it too.
            Decision::Serve {
                status: StatusCode::OK,
                content_range: None,
                slice_start: 0,
                slice_len: content_len,
                throttled: script.throttle_open_ended,
                drop_after: drop_fires(&mut state).then_some(DROP_AFTER_BYTES.min(content_len)),
            }
        } else {
            match plan_serve(&script, requested, content_len, display_total) {
                ServeShape::NotSatisfiable => Decision::RangeNotSatisfiable {
                    content_range: format!("bytes */{display_total}"),
                },
                ServeShape::Body {
                    status,
                    content_range,
                    slice_start,
                    slice_len,
                    throttled,
                } => Decision::Serve {
                    status,
                    content_range,
                    slice_start,
                    slice_len,
                    throttled,
                    drop_after: drop_fires(&mut state).then_some(DROP_AFTER_BYTES.min(content_len)),
                },
            }
        }
    };

    match decision {
        Decision::ServerError => HttpResponse::build(StatusCode::SERVICE_UNAVAILABLE).finish(),
        Decision::Stall => {
            // Hold the request open without answering; the client's attempt
            // timeout is what ends this — the handler never gets to respond.
            tokio::time::sleep(STALL_HOLD).await;
            HttpResponse::build(StatusCode::SERVICE_UNAVAILABLE).finish()
        }
        Decision::RangeNotSatisfiable { content_range } => {
            HttpResponse::build(StatusCode::RANGE_NOT_SATISFIABLE)
                .insert_header((header::CONTENT_RANGE, content_range))
                .finish()
        }
        Decision::Serve {
            status,
            content_range,
            slice_start,
            slice_len,
            throttled,
            drop_after,
        } => {
            let mut builder = HttpResponse::build(status);
            builder.content_type("application/octet-stream");
            builder.insert_header((header::ACCEPT_RANGES, "bytes"));
            if let Some(content_range) = content_range {
                builder.insert_header((header::CONTENT_RANGE, content_range));
            }
            builder.streaming(body_stream(
                origin.content.clone(),
                slice_start,
                slice_len,
                throttled,
                drop_after,
            ))
        }
    }
}

/// What a healthy serve looks like for one request, given the script — pure.
enum ServeShape {
    /// The range cannot be served from the real content (it starts at or
    /// past the end, or it is malformed).
    NotSatisfiable,
    Body {
        status: StatusCode,
        content_range: Option<String>,
        slice_start: u64,
        slice_len: u64,
        throttled: bool,
    },
}

fn plan_serve(
    script: &OriginScript,
    requested: RangeRequest,
    content_len: u64,
    display_total: u64,
) -> ServeShape {
    let throttled_for = |span: u64| script.throttle_open_ended && span > MAX_FAST_SPAN;
    match requested {
        // No Range header: the whole body from byte 0 — the read the edge
        // throttles, and the read ffmpeg does today.
        RangeRequest::Open => ServeShape::Body {
            status: StatusCode::OK,
            content_range: None,
            slice_start: 0,
            slice_len: content_len,
            throttled: script.throttle_open_ended,
        },
        RangeRequest::OpenFrom(start) => {
            if start >= content_len {
                ServeShape::NotSatisfiable
            } else {
                ServeShape::Body {
                    status: StatusCode::PARTIAL_CONTENT,
                    content_range: Some(format!(
                        "bytes {start}-{}/{display_total}",
                        content_len - 1
                    )),
                    slice_start: start,
                    slice_len: content_len - start,
                    // An unbounded end is an open-ended read — the shape a
                    // resuming player sends — so it throttles with the mode.
                    throttled: script.throttle_open_ended,
                }
            }
        }
        RangeRequest::Bounded { start, end } => {
            if start >= content_len || end < start {
                ServeShape::NotSatisfiable
            } else {
                let end = end.min(content_len - 1);
                ServeShape::Body {
                    status: StatusCode::PARTIAL_CONTENT,
                    content_range: Some(format!("bytes {start}-{end}/{display_total}")),
                    slice_start: start,
                    slice_len: end - start + 1,
                    throttled: throttled_for(end - start + 1),
                }
            }
        }
    }
}

/// The (possibly throttled, possibly aborting) body stream for a slice of the
/// origin's content.
///
/// Read a response body but give up after `window`: returns how many bytes
/// arrived within the window. A crawling body stays incomplete. Shared by
/// the fixture's and the fetcher's throttle regression tests.
pub(crate) async fn bytes_within(response: &mut reqwest::Response, window: Duration) -> u64 {
    let deadline = tokio::time::Instant::now() + window;
    let mut received = 0u64;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(deadline - tokio::time::Instant::now(), response.chunk()).await {
            Ok(Ok(Some(bytes))) => received += bytes.len() as u64,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => panic!("read within the window failed: {e}"),
            Err(_elapsed) => break,
        }
    }
    received
}

/// Throttled bodies pace one tick per piece: ~1 KB every 38 ms ≈ 27 KB/s.
/// A scripted drop yields a body error after [`DROP_AFTER_BYTES`], which
/// aborts the connection mid-stream.
fn body_stream(
    content: Arc<Vec<u8>>,
    start: u64,
    len: u64,
    throttled: bool,
    drop_after: Option<u64>,
) -> impl futures::Stream<Item = Result<web::Bytes, std::io::Error>> {
    let piece = if throttled {
        THROTTLE_CHUNK_BYTES
    } else {
        FAST_CHUNK_BYTES
    };
    // The drop lands strictly inside the body when there is one: aborting
    // after the last byte would read as a clean end plus a framing error,
    // not a mid-body drop.
    let served_before_drop = drop_after
        .map(|point| point.min(len.max(1) - 1))
        .unwrap_or(len);
    futures::stream::unfold(
        (content, start, served_before_drop, drop_after.is_some()),
        move |(content, mut pos, mut left, mut aborting)| async move {
            if left > 0 {
                let n = piece.min(left);
                let bytes = web::Bytes::copy_from_slice(&content[pos as usize..(pos + n) as usize]);
                pos += n;
                left -= n;
                if throttled {
                    tokio::time::sleep(THROTTLE_SLEEP).await;
                }
                return Some((Ok(bytes), (content, pos, left, aborting)));
            }
            if aborting {
                aborting = false;
                return Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "scripted mid-body drop",
                    )),
                    (content, pos, left, aborting),
                ));
            }
            None
        },
    )
}

#[cfg(test)]
mod test {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;

    /// A reqwest client as the staged fetch uses one: redirects on, explicit
    /// timeout per request left to the caller.
    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    async fn body_of(response: &mut reqwest::Response) -> eyre::Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// What one GET yields end to end: `Err` on any transport failure —
    /// including a mid-body connection abort, which reqwest can surface at
    /// `send` (the chunked body never terminates) or at body read.
    async fn get_body(url: Url) -> eyre::Result<Vec<u8>> {
        let mut response = match client().get(url).send().await {
            Ok(response) => response,
            Err(e) => return Err(eyre::eyre!("transport failure before the body: {e}")),
        };
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => body.extend_from_slice(&bytes),
                Ok(None) => return Ok(body),
                Err(e) => {
                    return Err(eyre::eyre!(
                        "transport failure mid-body after {} bytes: {e}",
                        body.len()
                    ))
                }
            }
        }
    }

    /// The Content-Range header as the origin sent it. reqwest and actix sit
    /// on different `http` crate versions, so header names cross the boundary
    /// as plain strings.
    fn content_range(response: &reqwest::Response) -> Option<String> {
        response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    #[tokio::test]
    async fn bounded_range_serves_exact_bytes_at_full_speed() {
        let origin = ScriptedOrigin::start(4 * MIB as usize, OriginScript::default());
        let start = tokio::time::Instant::now();
        let mut response = client()
            .get(origin.url())
            .header("range", "bytes=5000-6999")
            .send()
            .await
            .expect("bounded range get");
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(
            content_range(&response).as_deref(),
            Some("bytes 5000-6999/4194304")
        );
        let body = body_of(&mut response).await.expect("body");
        assert_eq!(body, origin.content()[5000..7000]);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a 2 KB bounded range must serve at full speed, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn bounded_range_up_to_the_fast_ceiling_is_fast_and_exact() {
        let origin = ScriptedOrigin::start(28 * MIB as usize, OriginScript::default());
        let span = MAX_FAST_SPAN; // 8 MiB — the measured fast ceiling
        let start = tokio::time::Instant::now();
        let mut response = client()
            .get(origin.url())
            .header("range", format!("bytes=0-{}", span - 1))
            .send()
            .await
            .expect("bounded range get");
        assert_eq!(response.status().as_u16(), 206);
        let body = body_of(&mut response).await.expect("body");
        assert_eq!(body, origin.content()[..span as usize]);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "an 8 MiB bounded range must serve at full speed, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn open_ended_get_crawls_at_the_measured_throttle() {
        // The regression signal for the measured edge: a plain GET with no
        // Range header (what ffmpeg does today) trickles at ~27 KB/s. In a
        // 2 s window it can deliver only ~54 KB — far short of any episode.
        let origin = ScriptedOrigin::start(
            4 * MIB as usize,
            OriginScript {
                throttle_open_ended: true,
                ..OriginScript::default()
            },
        );
        let start = tokio::time::Instant::now();
        let mut response = client()
            .get(origin.url())
            .send()
            .await
            .expect("open-ended get");
        assert_eq!(response.status().as_u16(), 200);
        assert!(
            content_range(&response).is_none(),
            "a 200 full body has no Content-Range"
        );
        let received = bytes_within(&mut response, Duration::from_secs(2)).await;
        assert!(
            received > 0,
            "the throttled read still trickles, it does not stall"
        );
        assert!(
            received < 256 * KIB,
            "an open-ended read must crawl (~27 KB/s): got {received} bytes in {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn oversized_bounded_range_throttles_like_open_ended() {
        // Single bounded requests far past the fast ceiling throttle too
        // (measured at ≥ 18 MB; the fixture takes the 8 MiB ceiling).
        let origin = ScriptedOrigin::start(
            28 * MIB as usize,
            OriginScript {
                throttle_open_ended: true,
                ..OriginScript::default()
            },
        );
        let mut response = client()
            .get(origin.url())
            .header("range", format!("bytes=0-{}", 16 * MIB - 1))
            .send()
            .await
            .expect("oversized bounded get");
        assert_eq!(response.status().as_u16(), 206);
        let received = bytes_within(&mut response, Duration::from_secs(2)).await;
        assert!(
            received < 256 * KIB,
            "a 16 MiB bounded read must crawl: got {received} bytes in the window"
        );
    }

    #[tokio::test]
    async fn open_from_offset_throttles_when_the_mode_is_on() {
        // `bytes=a-` is an unbounded read — the shape a resuming player
        // sends — so the open-ended mode throttles it like a plain GET.
        let origin = ScriptedOrigin::start(
            4 * MIB as usize,
            OriginScript {
                throttle_open_ended: true,
                ..OriginScript::default()
            },
        );
        let mut response = client()
            .get(origin.url())
            .header("range", "bytes=100-")
            .send()
            .await
            .expect("open-from get");
        assert_eq!(response.status().as_u16(), 206);
        let received = bytes_within(&mut response, Duration::from_secs(2)).await;
        assert!(
            received < 256 * KIB,
            "an unbounded `bytes=a-` read must crawl: got {received} bytes in the window"
        );
    }

    #[tokio::test]
    async fn drop_mid_body_first_attempt_only_then_serves_fully() {
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                drop_mid_body: FaultPolicy::First(1),
                ..OriginScript::default()
            },
        );
        // First request: the connection aborts mid-body — an error or a
        // short body, never a complete one.
        let first = get_body(origin.url()).await;
        match &first {
            Ok(bytes) => assert!(
                bytes.len() < origin.content().len(),
                "the aborted attempt must not serve a full body"
            ),
            Err(e) => assert!(e.to_string().contains("transport failure"), "{e}"),
        }
        // Second request: a fresh connection serves everything.
        let second = get_body(origin.url()).await.expect("second body");
        assert_eq!(second, origin.content());
        assert_eq!(origin.requests_at(None), 2);
    }

    #[tokio::test]
    async fn drop_mid_body_always_aborts_every_request() {
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                drop_mid_body: FaultPolicy::Always,
                ..OriginScript::default()
            },
        );
        let outcome = get_body(origin.url()).await;
        match &outcome {
            Ok(bytes) => assert!(
                bytes.len() < origin.content().len(),
                "every attempt aborts mid-body"
            ),
            Err(e) => assert!(e.to_string().contains("transport failure"), "{e}"),
        }
        assert_eq!(origin.requests_at(None), 1);
    }

    #[tokio::test]
    async fn range_ignored_serves_200_with_full_body() {
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                ignore_range: true,
                ..OriginScript::default()
            },
        );
        let mut response = client()
            .get(origin.url())
            .header("range", "bytes=100-199")
            .send()
            .await
            .expect("ranged get");
        assert_eq!(response.status().as_u16(), 200);
        let body = body_of(&mut response).await.expect("body");
        assert_eq!(body, origin.content(), "the Range header was ignored");
    }

    #[tokio::test]
    async fn server_error_first_then_recovers() {
        let origin = ScriptedOrigin::start(
            KIB as usize,
            OriginScript {
                server_error: FaultPolicy::First(1),
                ..OriginScript::default()
            },
        );
        let first = client().get(origin.url()).send().await.expect("first get");
        assert_eq!(first.status().as_u16(), 503);
        let mut second = client()
            .get(origin.url())
            .header("range", "bytes=0-1023")
            .send()
            .await
            .expect("second get");
        assert_eq!(second.status().as_u16(), 206);
        let body = body_of(&mut second).await.expect("body");
        assert_eq!(body, origin.content()[..1024]);
    }

    #[tokio::test]
    async fn server_error_always_answers_5xx() {
        let origin = ScriptedOrigin::start(
            KIB as usize,
            OriginScript {
                server_error: FaultPolicy::Always,
                ..OriginScript::default()
            },
        );
        for _ in 0..2 {
            let response = client().get(origin.url()).send().await.expect("get");
            assert_eq!(response.status().as_u16(), 503);
        }
        assert_eq!(origin.total_requests(), 2);
    }

    #[tokio::test]
    async fn content_range_lie_reports_the_lying_total() {
        let real = MIB;
        let lie = 99 * MIB;
        let origin = ScriptedOrigin::start(
            real as usize,
            OriginScript {
                lie_total: Some(lie),
                ..OriginScript::default()
            },
        );
        // An in-content range: 206, but the total is the lie.
        let mut response = client()
            .get(origin.url())
            .header("range", format!("bytes=0-{}", 64 * KIB - 1))
            .send()
            .await
            .expect("ranged get");
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(
            content_range(&response),
            Some(format!("bytes 0-{}/{lie}", 64 * KIB - 1))
        );
        let body = body_of(&mut response).await.expect("body");
        assert_eq!(body, origin.content()[..64 * KIB as usize]);
        // A range at the real content's end: 416 against the lying total.
        let response = client()
            .get(origin.url())
            .header("range", format!("bytes={real}-{}", real + 1))
            .send()
            .await
            .expect("out-of-content get");
        assert_eq!(response.status().as_u16(), 416);
        assert_eq!(content_range(&response), Some(format!("bytes */{lie}")));
    }

    #[tokio::test]
    async fn out_of_content_start_is_416_even_without_a_lie() {
        let real = MIB;
        let origin = ScriptedOrigin::start(real as usize, OriginScript::default());
        let response = client()
            .get(origin.url())
            .header("range", format!("bytes={real}-{}", real + 1))
            .send()
            .await
            .expect("out-of-content get");
        assert_eq!(response.status().as_u16(), 416);
        assert_eq!(content_range(&response), Some(format!("bytes */{real}")));
    }

    #[tokio::test]
    async fn open_from_offset_serves_to_the_end() {
        let origin = ScriptedOrigin::start(MIB as usize, OriginScript::default());
        let mut response = client()
            .get(origin.url())
            .header("range", "bytes=100-")
            .send()
            .await
            .expect("open-from get");
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(
            content_range(&response),
            Some(format!("bytes 100-{}/{}", MIB - 1, MIB))
        );
        let body = body_of(&mut response).await.expect("body");
        assert_eq!(body, origin.content()[100..]);
    }

    #[tokio::test]
    async fn stall_holds_the_connection_until_the_client_gives_up() {
        let origin = ScriptedOrigin::start(
            KIB as usize,
            OriginScript {
                stall: FaultPolicy::First(1),
                ..OriginScript::default()
            },
        );
        let first = client()
            .get(origin.url())
            .timeout(Duration::from_millis(300))
            .send()
            .await;
        let err = first.expect_err("the stalled request must hit the client timeout");
        assert!(
            err.is_timeout(),
            "the failure is the attempt timeout: {err}"
        );
        let mut second = client()
            .get(origin.url())
            .header("range", "bytes=0-1023")
            .send()
            .await
            .expect("second get");
        assert_eq!(second.status().as_u16(), 206);
        let body = body_of(&mut second).await.expect("body");
        assert_eq!(body, origin.content()[..1024]);
    }
}
