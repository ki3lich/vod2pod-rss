//! The staging engine's executor: ranged GETs turning a resolved Stream URL
//! into a local, byte-verified source file.
//!
//! The pure [`chunk_plan`] decides; this module acts decisions out — one
//! HTTP attempt per observation, a per-chunk timeout (reqwest's request
//! timeout: connect through body streaming) and fresh-connection retries,
//! the total size learned from the first chunk's Content-Range (that request
//! doubling as the reachability probe), and the assembled file verified
//! against the origin total before it is handed on.
//!
//! A 200 answer to a ranged GET — an origin that ignores Range — engages
//! rung 2 of the fallback ladder: the source stages via one open-ended GET
//! into the same temp file, keeping the win that ffmpeg reads locally. The
//! fallback inherits the ranged path's budgets and clean-failure shape, so
//! a dead origin still fails the stage fast, with no partial file left
//! behind.

use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::{debug, info};
use reqwest::{Client, Url};

use super::chunk_plan::{
    attempt_verdict, open_ended_verdict, AttemptObservation, ChunkPlan, ChunkSpan, RangeIgnored,
    StageScheduler, Verdict,
};

/// The user agent staged fetches identify with.
const STAGING_USER_AGENT: &str =
    concat!("vod2pod-rss/", env!("CARGO_PKG_VERSION"), " source-staging");

/// Knobs bounding one stage. Production wiring maps configuration onto
/// these; tests shrink them for speed.
#[derive(Clone, Copy, Debug)]
pub struct StageLimits {
    /// Size of one ranged GET. googlevideo edges serve bounded requests up
    /// to ~8 MiB at full speed and throttle anything bigger — 8 MiB is the
    /// measured fast ceiling.
    pub chunk_size: u64,
    /// How many chunk fetches run in parallel (measured full aggregate: 4).
    pub concurrency: usize,
    /// Fresh-connection attempts per chunk before the stage fails.
    pub max_attempts: u32,
    /// Deadline for one chunk attempt: connect, response headers and body
    /// streaming combined.
    pub attempt_timeout: Duration,
    /// Deadline for the whole stage — the total stage budget. The
    /// open-ended fallback's single GET streams the whole source under
    /// this deadline rather than the per-chunk one: a 200 full body cannot
    /// be resumed from a range start, so the per-chunk deadline would
    /// strand every large Range-ignoring source.
    pub stage_timeout: Duration,
    /// Pause between a chunk's retries.
    pub retry_backoff: Duration,
}

/// The staging engine: stages a resolved stream URL's source media to a
/// local file with parallel bounded ranged GETs.
pub struct SourceStager {
    client: Client,
    limits: StageLimits,
}

/// A completed Source Stage's measured outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageReport {
    /// The staged byte count — verified equal to the origin total, or (on
    /// the open-ended fallback) the body's received count.
    pub bytes: u64,
    /// Fresh-connection retries the origin charged across the probe, the
    /// fallback and all chunks: every attempt beyond the first, summed.
    pub chunk_retries: u32,
}

impl SourceStager {
    /// A stager over `client` (shared, pooled) with `limits`.
    pub fn new(client: Client, limits: StageLimits) -> Self {
        Self { client, limits }
    }

    /// Stage the source at `stream_url` to `dest`, returning a
    /// [`StageReport`] — the staged byte count (verified equal to the origin
    /// total the responses reported) plus the retry cost the origin charged.
    ///
    /// The first ranged GET doubles as the reachability probe: a dead edge
    /// (dropped SYNs, refused connects, 5xx storms) fails here within the
    /// attempt budget. Surfacing that failure to the eviction-and-retry
    /// loop is the caller's job, exactly as for a preflight failure.
    pub async fn stage_to(&self, stream_url: &Url, dest: &Path) -> eyre::Result<StageReport> {
        tokio::fs::File::create(dest).await?;

        let mut chunk_retries: u32 = 0;

        // Probe: chunk 0's ranged GET learns the origin total from its
        // Content-Range and doubles as the reachability check.
        let probe_span = ChunkSpan {
            offset: 0,
            len: self.limits.chunk_size,
        };
        let probe = match drive_chunk_attempts(
            &self.client,
            &self.limits,
            stream_url,
            probe_span,
            None,
            dest,
            "probe",
        )
        .await
        {
            Ok(probe) => probe,
            Err(e) => {
                // Rung 2 of the fallback ladder: the origin ignores Range —
                // stage the source with one open-ended GET instead. No plan
                // is built, so exactly this one request shape follows: no
                // second request storm.
                if let Some(RangeIgnored { attempts_used }) = e.downcast_ref::<RangeIgnored>() {
                    info!(
                        "the origin ignored Range for {stream_url} (answered 200 with a \
                         full body); staging via one open-ended GET"
                    );
                    let mut report = self.stage_open_ended(stream_url, dest).await?;
                    // The probe's ranged attempts count too: every request
                    // beyond its first was a retry the origin charged.
                    report.chunk_retries += attempts_used - 1;
                    return Ok(report);
                }
                return Err(e);
            }
        };
        chunk_retries += probe.attempts - 1;
        let total = probe.total;

        let plan = ChunkPlan::new(total, self.limits.chunk_size)?;
        debug_assert_eq!(
            plan.spans()[0].len,
            probe.expected_len,
            "the probe covered the plan's first span"
        );

        let mut scheduler = StageScheduler::new(plan.len(), self.limits.concurrency)?;
        // The probe was chunk 0: hand it out and account it finished — its
        // bytes are already in the file.
        scheduler.launch_next();
        scheduler.mark_finished(0);

        let mut in_flight = tokio::task::JoinSet::new();
        let staging: eyre::Result<()> = loop {
            while let Some(index) = scheduler.launch_next() {
                let span = plan.spans()[index];
                in_flight.spawn(stage_chunk(
                    self.client.clone(),
                    self.limits,
                    stream_url.clone(),
                    span,
                    total,
                    dest.to_path_buf(),
                    index,
                ));
            }
            if scheduler.is_complete() {
                break Ok(());
            }
            match in_flight.join_next().await {
                Some(Ok((index, Ok(attempts)))) => {
                    chunk_retries += attempts - 1;
                    scheduler.mark_finished(index);
                }
                Some(Ok((_index, Err(e)))) => break Err(e),
                Some(Err(join_error)) => {
                    break Err(eyre::eyre!("staged chunk task crashed: {join_error}"))
                }
                None => {
                    break Err(eyre::eyre!(
                        "staged fetch stalled: no chunk in flight but the plan is incomplete"
                    ))
                }
            }
        };
        // Dropping the set aborts any in-flight chunks: a permanently
        // failing chunk must not let the stage crawl to its natural end.
        drop(in_flight);
        staging?;
        let bytes = self.verify_staged_size(dest, total).await?;
        Ok(StageReport {
            bytes,
            chunk_retries,
        })
    }

    /// The staged file must hold exactly the origin total: per-chunk byte
    /// counts prove every span was written; this proves nothing extra crept
    /// in or went missing.
    async fn verify_staged_size(&self, dest: &Path, total: u64) -> eyre::Result<u64> {
        let staged = tokio::fs::metadata(dest).await?.len();
        if staged != total {
            eyre::bail!(
                "the staged file is {staged} bytes but the origin reported {total}; \
                 refusing to hand ffmpeg a source that cannot be byte-complete"
            );
        }
        Ok(total)
    }

    /// Rung 2 of the fallback ladder: the origin ignored the Range header,
    /// so one open-ended GET stages the source — ffmpeg still reads locally,
    /// the win the ladder preserves. The attempt budget and backoff are the
    /// ranged path's for the failures a fresh connection can beat (drops,
    /// 5xx); a timeout is not one — the GET's deadline is the total stage
    /// deadline itself, so a too-slow origin consumes the whole budget in
    /// one bounded attempt. That is what keeps rung 2 size-independent; the
    /// fallback differs in the request shape only, not in how cleanly it
    /// fails.
    async fn stage_open_ended(&self, stream_url: &Url, dest: &Path) -> eyre::Result<StageReport> {
        let mut attempts_used = 0;
        let received = loop {
            attempts_used += 1;
            let observation =
                attempt_open_ended(&self.client, &self.limits, stream_url, dest).await;
            match open_ended_verdict(&observation, attempts_used, self.limits.max_attempts) {
                Verdict::Done => match observation {
                    AttemptObservation::Body { received, .. } => break received,
                    other => unreachable!("an open-ended Done always comes from a Body: {other:?}"),
                },
                Verdict::Retry => {
                    debug!(
                        "staged fetch: open-ended fallback attempt {attempts_used}/{} failed \
                         transiently ({observation:?}); retrying on a fresh connection",
                        self.limits.max_attempts
                    );
                    tokio::time::sleep(self.limits.retry_backoff).await;
                }
                Verdict::Fail(message) => return Err(eyre::eyre!(message)),
            }
        };
        Ok(StageReport {
            bytes: received,
            chunk_retries: attempts_used - 1,
        })
    }
}

/// Fetch one chunk with retries until the pure verdict says done or
/// hopeless. Runs as its own task under the concurrency window.
async fn stage_chunk(
    client: Client,
    limits: StageLimits,
    stream_url: Url,
    span: ChunkSpan,
    planned_total: u64,
    dest: PathBuf,
    index: usize,
) -> (usize, eyre::Result<u32>) {
    let result = drive_chunk_attempts(
        &client,
        &limits,
        &stream_url,
        span,
        Some(planned_total),
        &dest,
        &format!("chunk {index} at offset {}", span.offset),
    )
    .await
    .map(|fetched| fetched.attempts);
    (index, result)
}

/// The Body observation that satisfied a chunk's verdict, plus what it cost:
/// the origin total the headers reported, the span length that was owed, and
/// the attempts used.
struct ChunkFetched {
    total: u64,
    expected_len: u64,
    attempts: u32,
}

/// Drive one chunk's attempts — the probe's or a plan chunk's — until the
/// pure verdict says done or the budget says hopeless, backing off between
/// retries. Each attempt is a fresh request; a connection the previous
/// attempt broke is never reused. Returns the Body observation that
/// satisfied the verdict, with its attempt count.
async fn drive_chunk_attempts(
    client: &Client,
    limits: &StageLimits,
    stream_url: &Url,
    span: ChunkSpan,
    planned_total: Option<u64>,
    dest: &Path,
    label: &str,
) -> eyre::Result<ChunkFetched> {
    let mut attempts_used = 0;
    loop {
        attempts_used += 1;
        let observation =
            attempt_chunk(client, limits, stream_url, span, planned_total, dest).await;
        // The fallback ladder's cue travels as a typed error, not message
        // text: the probe's caller downcasts it to engage the open-ended
        // fallback. Retrying a Range-ignoring origin would only reproduce
        // the 200.
        if matches!(observation, AttemptObservation::RangeIgnored) {
            return Err(eyre::Report::new(RangeIgnored { attempts_used }));
        }
        match attempt_verdict(span, &observation, attempts_used, limits.max_attempts) {
            Verdict::Done => match observation {
                AttemptObservation::Body {
                    total,
                    expected_len,
                    // The verdict already validated the received count.
                    received: _,
                } => {
                    return Ok(ChunkFetched {
                        total,
                        expected_len,
                        attempts: attempts_used,
                    })
                }
                other => unreachable!("a Done verdict always comes from a Body: {other:?}"),
            },
            Verdict::Retry => {
                debug!(
                    "staged fetch: {label} attempt {attempts_used}/{} failed transiently \
                     ({observation:?}); retrying on a fresh connection",
                    limits.max_attempts
                );
                tokio::time::sleep(limits.retry_backoff).await;
            }
            Verdict::Fail(message) => return Err(eyre::eyre!(message)),
        }
    }
}

/// One HTTP GET attempt against the Stream URL — the staging user agent,
/// an optional Range header, and the request's deadline. Transport-level
/// failures map onto [`AttemptObservation`]; both request shapes (the
/// ranged chunk and the open-ended fallback) send through here.
async fn send_attempt(
    client: &Client,
    stream_url: &Url,
    range: Option<(u64, u64)>,
    deadline: Duration,
) -> Result<reqwest::Response, AttemptObservation> {
    let mut request = client.get(stream_url.clone());
    if let Some((start, end)) = range {
        request = request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    }
    match request
        .header(reqwest::header::USER_AGENT, STAGING_USER_AGENT)
        .timeout(deadline)
        .send()
        .await
    {
        Ok(response) => Ok(response),
        Err(e) if e.is_timeout() => Err(AttemptObservation::TimedOut),
        Err(e) => Err(AttemptObservation::Transport(e.to_string())),
    }
}

/// Stream a response body into a positioned file, returning the received
/// byte count. `cap` stops the flood early on over-delivery — the ranged
/// path's span bound: the verdict fails the attempt, and the bytes past the
/// span are never written; the open-ended path has no cap. A failed
/// attempt's partial bytes are harmless: a retry rewrites from its start.
/// Small buffered writes to a local temp file never block meaningfully, so
/// sync I/O keeps the attempt a single future.
async fn stream_body(
    response: &mut reqwest::Response,
    file: &mut std::fs::File,
    cap: Option<u64>,
) -> Result<u64, AttemptObservation> {
    let mut received: u64 = 0;
    loop {
        match response.chunk().await {
            Ok(Some(bytes)) => {
                received += bytes.len() as u64;
                if cap.is_some_and(|cap| received > cap) {
                    break;
                }
                if let Err(e) = file.write_all(&bytes) {
                    return Err(AttemptObservation::Transport(format!(
                        "writing the staged file failed: {e}"
                    )));
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(AttemptObservation::Transport(format!(
                    "reading the response body failed: {e}"
                )))
            }
        }
    }
    Ok(received)
}

/// One chunk-fetch attempt: a fresh ranged GET, its headers validated
/// against the request and the plan, its body streamed into the chunk's
/// file span. Transport-level events map onto [`AttemptObservation`]; the
/// pure verdict decides what they mean.
///
/// `planned_total` is `None` for the probe — the origin total is learned
/// from its Content-Range there.
async fn attempt_chunk(
    client: &Client,
    limits: &StageLimits,
    stream_url: &Url,
    span: ChunkSpan,
    planned_total: Option<u64>,
    dest: &Path,
) -> AttemptObservation {
    let range_end = span.offset + span.len - 1;
    let mut response = match send_attempt(
        client,
        stream_url,
        Some((span.offset, range_end)),
        limits.attempt_timeout,
    )
    .await
    {
        Ok(response) => response,
        Err(observation) => return observation,
    };

    let status = response.status().as_u16();
    if status != 206 {
        return classify_status(status);
    }
    let Some(content_range) = response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return AttemptObservation::BadRange {
            reason: "HTTP 206 without a Content-Range header — the span and the \
                     origin total cannot be learned"
                .to_string(),
        };
    };
    let Some((start, end, total)) = parse_content_range(&content_range) else {
        return AttemptObservation::BadRange {
            reason: format!("invalid Content-Range header: {content_range:?}"),
        };
    };
    if total == 0 {
        return AttemptObservation::BadRange {
            reason: "the origin reports a total of 0 bytes; nothing to stage".to_string(),
        };
    }

    // The origin's headers must serve exactly the requested span and report
    // the planned total. The probe (no planned total yet) instead derives
    // its expected span from the response: a truthful origin clamps the
    // requested span at the real end of the content.
    let expected_len = match planned_total {
        None => {
            if start != 0 {
                return AttemptObservation::BadRange {
                    reason: format!(
                        "the probe asked for bytes 0-{} but the origin answered \
                         starting at {start}",
                        span.len - 1
                    ),
                };
            }
            span.len.min(total)
        }
        Some(planned_total) => {
            if total != planned_total {
                return AttemptObservation::BadRange {
                    reason: format!(
                        "the origin's Content-Range total {total} contradicts the \
                         {planned_total} the plan was built from"
                    ),
                };
            }
            if start != span.offset {
                return AttemptObservation::BadRange {
                    reason: format!(
                        "the chunk asked for bytes starting at {} but the origin \
                         answered starting at {start}",
                        span.offset
                    ),
                };
            }
            span.len
        }
    };
    let served_len = end - start + 1;
    if served_len != expected_len {
        return AttemptObservation::BadRange {
            reason: format!(
                "the origin's Content-Range serves {served_len} bytes from {start} \
                 but the staged span is {expected_len} bytes from {}",
                span.offset
            ),
        };
    }

    // Stream the body into the chunk's span. A failed attempt's partial
    // bytes are harmless: a retry rewrites the whole span from its start.
    // Small buffered writes to a local temp file never block meaningfully,
    // so sync I/O keeps the attempt a single future.
    let mut file = match std::fs::OpenOptions::new().write(true).open(dest) {
        Ok(file) => file,
        Err(e) => {
            return AttemptObservation::Transport(format!(
                "cannot open the staged file for writing: {e}"
            ))
        }
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(span.offset)) {
        return AttemptObservation::Transport(format!("cannot seek the staged file: {e}"));
    }
    let received = match stream_body(&mut response, &mut file, Some(expected_len)).await {
        Ok(received) => received,
        Err(observation) => return observation,
    };
    AttemptObservation::Body {
        total,
        expected_len,
        received,
    }
}

/// One open-ended GET attempt — the Range-ignored fallback (ladder rung 2):
/// no Range header, the whole body under a 200, and no Content-Range to
/// verify against. Streams from byte 0 to the connection's end; the
/// previous attempt's partial bytes are truncated away first, so a retry
/// rewrites the body from its start.
///
/// The request runs under the total stage deadline, not the per-chunk one:
/// a 200 full body cannot be resumed from a range start, so bounding it by
/// the per-chunk deadline would strand every source bigger than one
/// chunk's worth of throughput (the fallback ladder's rung 2 must stay
/// size-independent).
async fn attempt_open_ended(
    client: &Client,
    limits: &StageLimits,
    stream_url: &Url,
    dest: &Path,
) -> AttemptObservation {
    let mut response = match send_attempt(client, stream_url, None, limits.stage_timeout).await {
        Ok(response) => response,
        Err(observation) => return observation,
    };

    let status = response.status().as_u16();
    if status != 200 {
        // 206 to a plain GET, 416, 5xx, ... — the same classifier the ranged
        // path uses; its 200 arm cannot fire here (200 is the success path).
        return classify_status(status);
    }

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(dest)
    {
        Ok(file) => file,
        Err(e) => {
            return AttemptObservation::Transport(format!(
                "cannot open the staged file for writing: {e}"
            ))
        }
    };
    let received = match stream_body(&mut response, &mut file, None).await {
        Ok(received) => received,
        Err(observation) => return observation,
    };
    if received == 0 {
        return AttemptObservation::BadRange {
            reason: "the open-ended response carried no bytes; nothing to stage".to_string(),
        };
    }
    // The body ran to its end: whatever arrived is the source's whole truth
    // — there is no declared total to verify against. `total` and
    // `expected_len` carry the received count so the verdict sees a
    // self-consistent Body.
    AttemptObservation::Body {
        total: received,
        expected_len: received,
        received,
    }
}

/// Parse `bytes {start}-{end}/{total}` — the one Content-Range shape a
/// staged exchange can use. Anything else (`*`, an open total, another
/// unit, end before start, an end past the declared total) is `None`: a
/// header the engine cannot trust.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.trim().strip_prefix("bytes")?;
    let (range, total) = rest.trim_start().split_once('/')?;
    let total = total.trim();
    if total == "*" {
        return None;
    }
    let total: u64 = total.parse().ok()?;
    let (start, end) = range.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start || end >= total {
        return None;
    }
    Some((start, end, total))
}

/// Classify a non-206 answer to a ranged GET. 200 is the one message the
/// fallback ladder's rung 2 (staging an open-ended GET) keys off — kept
/// distinct and typed. 5xx/429 are the edge having a bad moment; everything
/// else is deterministic from this URL.
fn classify_status(status: u16) -> AttemptObservation {
    match status {
        200 => AttemptObservation::RangeIgnored,
        416 => AttemptObservation::BadRange {
            reason: "the origin answered 416 (range not satisfiable) for a planned \
                     chunk — the origin total the plan was built from cannot be served"
                .to_string(),
        },
        429 | 500..=599 => AttemptObservation::ServerError { status },
        _ => AttemptObservation::OtherStatus { status },
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::transcoder::staging::test_origin::{
        bytes_within, FaultPolicy, OriginScript, ScriptedOrigin,
    };
    use crate::transcoder::staging::MAX_CHUNK_ATTEMPTS;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;

    /// Fast-but-real limits for tests: 1 MiB chunks, 3 in flight, tight
    /// timeouts, near-zero backoff.
    fn test_limits() -> StageLimits {
        StageLimits {
            chunk_size: MIB,
            concurrency: 3,
            max_attempts: MAX_CHUNK_ATTEMPTS,
            attempt_timeout: Duration::from_secs(5),
            stage_timeout: Duration::from_secs(30),
            retry_backoff: Duration::from_millis(50),
        }
    }

    fn stager(limits: StageLimits) -> SourceStager {
        SourceStager::new(reqwest::Client::new(), limits)
    }

    /// A unique temp path, removed when the test ends — failed or not.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vod2pod-staging-{tag}-{}.tmp",
                uuid::Uuid::new_v4()
            ));
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn staged_bytes(dest: &Path) -> Vec<u8> {
        tokio::fs::read(dest).await.expect("read the staged file")
    }

    #[tokio::test]
    async fn stages_byte_identical_file_from_a_ranged_origin() {
        // Multi-chunk with a remainder chunk, out-of-order completions under
        // a 3-wide window, over a stale destination file: the staged bytes
        // must equal the origin's, exactly.
        let origin = ScriptedOrigin::start((10 * MIB + 12345) as usize, OriginScript::default());
        let dest = TempFile::new("identical");
        tokio::fs::write(dest.path(), b"stale garbage from a previous stage")
            .await
            .unwrap();

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("staging must succeed on a ranged origin");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        // Every chunk fetched exactly once: the probe (offset 0) plus the
        // ten 1 MiB chunks.
        assert_eq!(origin.total_requests(), 11);
        assert_eq!(origin.requests_at(Some(0)), 1);
        assert_eq!(origin.requests_at(Some(5 * MIB)), 1);
    }

    #[tokio::test]
    async fn stages_a_single_chunk_source_via_the_probe_alone() {
        // A source smaller than one chunk: the probe's Content-Range total
        // is the whole story, the server clamps the probe at the real end,
        // and no further request is needed.
        let origin = ScriptedOrigin::start(5120, OriginScript::default());
        let dest = TempFile::new("single");

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("staging must succeed");
        assert_eq!(staged.bytes, 5120);
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        assert_eq!(origin.total_requests(), 1, "only the probe ran");
    }

    #[tokio::test]
    async fn probe_retries_transient_5xx_and_stages() {
        // The probe is the reachability check: one transient 503 must cost
        // a fresh-connection retry, not the stage.
        let origin = ScriptedOrigin::start(
            (2 * MIB + 7) as usize,
            OriginScript {
                server_error: FaultPolicy::First(1),
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("probe-retry");

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("a transient probe failure must recover");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        assert_eq!(origin.requests_at(Some(0)), 2, "the probe ran twice");
        assert_eq!(staged.chunk_retries, 1, "one probe retry was charged");
    }

    #[tokio::test]
    async fn transient_chunk_failures_are_retried_and_recovered() {
        // Two chunks fail once each and the retry re-fetches the whole
        // chunk; the staged file is still byte-identical.
        let origin = ScriptedOrigin::start(
            (3 * MIB) as usize,
            OriginScript {
                server_errors_at_offset: vec![
                    (MIB, FaultPolicy::First(1)),
                    (2 * MIB, FaultPolicy::First(1)),
                ],
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("chunk-retry");

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("transient chunk failures must recover");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        assert_eq!(origin.requests_at(Some(MIB)), 2);
        assert_eq!(origin.requests_at(Some(2 * MIB)), 2);
        assert_eq!(origin.requests_at(Some(0)), 1);
        assert_eq!(staged.chunk_retries, 2, "one retry per recovered chunk");
    }

    #[tokio::test]
    async fn permanently_failing_chunk_fails_the_stage_fast() {
        // A chunk that 503s on every attempt fails the stage after its
        // budget — fast, and with no budget-burning beyond it.
        let origin = ScriptedOrigin::start(
            (3 * MIB) as usize,
            OriginScript {
                server_errors_at_offset: vec![(MIB, FaultPolicy::Always)],
                ..OriginScript::default()
            },
        );
        let limits = StageLimits {
            max_attempts: 2,
            ..test_limits()
        };
        let dest = TempFile::new("chunk-dead");

        let started = tokio::time::Instant::now();
        let err = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a permanently failing chunk must fail the stage");
        assert!(
            err.to_string().contains("503") && err.to_string().contains("2 of 2"),
            "the error names the status and the exhausted budget: {err}"
        );
        assert_eq!(
            origin.requests_at(Some(MIB)),
            2,
            "the chunk got exactly its budget, then the stage gave up"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "failing must be fast, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn content_range_lie_fails_the_stage_without_burning_retries() {
        // The origin reports a 5 MiB total while holding 1 MiB: the plan
        // extends past the real content, the origin answers 416 there, and
        // the stage fails on the first such chunk — a lie is not transient,
        // so not one retry is spent on it.
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                lie_total: Some(5 * MIB),
                ..OriginScript::default()
            },
        );
        let limits = StageLimits {
            chunk_size: 256 * KIB,
            ..test_limits()
        };
        let dest = TempFile::new("lie");

        let err = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a content-range lie must fail the stage");
        assert!(
            err.to_string().contains("416"),
            "the error names the contradiction: {err}"
        );
        assert_eq!(
            origin.requests_at(Some(MIB)),
            1,
            "the out-of-content chunk fails on its first attempt"
        );
        // And the file on disk holds only what the origin really served —
        // never a padded stretch to the lying total.
        let staged_len = tokio::fs::metadata(dest.path()).await.unwrap().len();
        assert!(
            staged_len <= MIB,
            "the staged file must never grow to the lying total: {staged_len}"
        );
    }

    #[tokio::test]
    async fn mid_body_drops_that_survive_retries_fail_the_stage() {
        // The measured edge resets long reads mid-stream. When the drop
        // recurs on every attempt, the stage must surface a clear error
        // instead of delivering a truncated file.
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                drops_at_offset: vec![(0, FaultPolicy::Always)],
                ..OriginScript::default()
            },
        );
        let limits = StageLimits {
            max_attempts: 2,
            ..test_limits()
        };
        let dest = TempFile::new("drops");

        let err = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a drop that survives every retry must fail the stage");
        let message = err.to_string();
        assert!(
            message.contains("2 of 2"),
            "the error names the exhausted budget: {message}"
        );
        assert_eq!(
            origin.requests_at(Some(0)),
            2,
            "the probe got exactly its budget"
        );
    }

    #[tokio::test]
    async fn mid_body_drop_on_one_attempt_is_retried_and_recovered() {
        // One dropped attempt, then a fresh connection that completes: the
        // staged file is byte-identical.
        let origin = ScriptedOrigin::start(
            (2 * MIB) as usize,
            OriginScript {
                drops_at_offset: vec![(MIB, FaultPolicy::First(1))],
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("drop-retry");

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("a single dropped attempt must recover");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        assert_eq!(origin.requests_at(Some(MIB)), 2);
        assert_eq!(staged.chunk_retries, 1, "one dropped attempt was retried");
    }

    #[tokio::test]
    async fn throttled_origin_stages_rapidly_via_ranges_while_open_ended_crawls() {
        // The measured-throttle regression signal, both halves: the same
        // fixture serves bounded ranges at full speed — a 28 MiB source
        // stages in seconds via 4 parallel 8 MiB chunks, the exact shape
        // measured at 1.09 s on the real edge — while a plain open-ended
        // GET (what ffmpeg does today) trickles at ~27 KB/s.
        let origin = ScriptedOrigin::start(
            (28 * MIB) as usize,
            OriginScript {
                throttle_open_ended: true,
                ..OriginScript::default()
            },
        );
        let limits = StageLimits {
            chunk_size: 8 * MIB,
            concurrency: 4,
            ..test_limits()
        };
        let dest = TempFile::new("throttled");

        let started = tokio::time::Instant::now();
        let staged = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("bounded ranges must beat the throttle");
        let stage_elapsed = started.elapsed();
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        // No retries, no drops: one request per chunk, four chunks.
        assert_eq!(origin.total_requests(), 4);
        assert_eq!(origin.requests_at(Some(0)), 1);
        assert_eq!(origin.requests_at(Some(8 * MIB)), 1);
        assert_eq!(origin.requests_at(Some(16 * MIB)), 1);
        assert_eq!(origin.requests_at(Some(24 * MIB)), 1);
        assert!(
            stage_elapsed < Duration::from_secs(30),
            "a 28 MiB source must stage in seconds via bounded ranges \
             (throttled at ~27 KB/s it would take ~18 minutes), took {stage_elapsed:?}"
        );

        // The other half: an open-ended GET against the same origin
        // demonstrably crawls — in a 2 s window it delivers only what the
        // ~27 KB/s throttle allows, nowhere near the source.
        let mut response = reqwest::Client::new()
            .get(origin.url())
            .send()
            .await
            .expect("open-ended get");
        let crawled = bytes_within(&mut response, Duration::from_secs(2)).await;
        assert!(
            crawled > 0,
            "the throttled read trickles; it does not stall"
        );
        assert!(
            crawled < 256 * KIB,
            "an open-ended GET must crawl at ~27 KB/s (~54 KB in 2 s): \
             got {crawled} bytes while the staged path finished in {stage_elapsed:?}"
        );
    }

    #[tokio::test]
    async fn content_range_total_drift_fails_the_stage() {
        // The probe learns the true total; a later chunk's Content-Range
        // then reports a different one — a mid-stage lie the engine must
        // catch as a contradiction, with no body written for the lying
        // response.
        let origin = ScriptedOrigin::start(
            (2 * MIB) as usize,
            OriginScript {
                lie_total_after_first: Some(5 * MIB),
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("drift");

        let err = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a drifting Content-Range total must fail the stage");
        let message = err.to_string();
        assert!(
            message.contains("5242880") && message.contains("2097152"),
            "the error names both totals: {message}"
        );
        // The lying chunk got one request, not a retry budget.
        assert_eq!(origin.requests_at(Some(MIB)), 1);
    }

    #[tokio::test]
    async fn range_ignoring_origin_stages_via_one_open_ended_get() {
        // Rung 2 of the fallback ladder: an origin that answers 200
        // full-body cannot be ranged, so the probe's 200 engages the
        // open-ended fallback — exactly one more request, staged into the
        // same temp file, byte-identical.
        let origin = ScriptedOrigin::start(
            (2 * MIB + 7) as usize,
            OriginScript {
                ignore_range: true,
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("ignored-range");

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("a range-ignoring origin must stage via the open-ended fallback");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        assert_eq!(origin.requests_at(Some(0)), 1, "one ranged probe");
        assert_eq!(origin.requests_at(None), 1, "one open-ended GET");
        assert_eq!(origin.total_requests(), 2, "no second request storm");
        assert_eq!(staged.chunk_retries, 0);
    }

    #[tokio::test]
    async fn open_ended_fallback_streams_past_the_per_chunk_deadline() {
        // Rung 2 stays size-independent: a Range-ignoring origin whose body
        // trickles slower than one chunk's deadline still stages — the
        // open-ended GET runs under the total stage deadline, not the
        // per-chunk one. A 200 full body cannot be resumed from a range
        // start, so the per-chunk budget would strand every large
        // Range-ignoring source behind a timeout.
        let origin = ScriptedOrigin::start(
            (64 * KIB) as usize,
            OriginScript {
                ignore_range: true,
                throttle_open_ended: true,
                ..OriginScript::default()
            },
        );
        // 64 KiB at the scripted ~27 KB/s crawl takes ~2.5 s: far past the
        // 200 ms per-chunk deadline, comfortably inside the 30 s stage one.
        let limits = StageLimits {
            attempt_timeout: Duration::from_millis(200),
            stage_timeout: Duration::from_secs(30),
            ..test_limits()
        };
        let dest = TempFile::new("open-ended-deadline");

        let staged = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("the open-ended fallback must stage under the stage deadline");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
    }

    #[tokio::test]
    async fn open_ended_fallback_retries_a_dropped_body_and_recovers() {
        // The fallback's body drops once mid-stream: a fresh connection
        // rewrites the whole body from byte 0 and the stage still lands a
        // byte-identical file. The drop is scoped to the open-ended shape —
        // an aborting body kills the connection before its head, and the
        // probe must keep seeing its 200 for the fallback to engage.
        let origin = ScriptedOrigin::start(
            (2 * MIB) as usize,
            OriginScript {
                ignore_range: true,
                drops_open_ended: FaultPolicy::First(1),
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("fallback-drop");

        let staged = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect("a single dropped fallback attempt must recover");
        assert_eq!(staged.bytes as usize, origin.content().len());
        assert_eq!(staged_bytes(dest.path()).await, origin.content());
        assert_eq!(origin.requests_at(None), 2, "the open-ended GET ran twice");
        assert_eq!(staged.chunk_retries, 1);
    }

    #[tokio::test]
    async fn open_ended_fallback_that_keeps_dropping_fails_within_its_budget() {
        // A drop on every attempt is hopeless: the fallback exhausts its
        // attempt budget and fails with the same clear error shape as a
        // ranged chunk — fast, never an unbounded hang.
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                ignore_range: true,
                drops_open_ended: FaultPolicy::Always,
                ..OriginScript::default()
            },
        );
        let limits = StageLimits {
            max_attempts: 2,
            ..test_limits()
        };
        let dest = TempFile::new("fallback-dead");

        let started = tokio::time::Instant::now();
        let err = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a drop that survives every retry must fail the stage");
        let message = err.to_string();
        assert!(
            message.contains("open-ended") && message.contains("2 of 2"),
            "the error names the fallback and the exhausted budget: {message}"
        );
        assert_eq!(origin.requests_at(None), 2);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "failing must be fast, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn dead_origin_fails_the_probe_without_engaging_the_fallback() {
        // 5xx on every attempt fails the stage naming the status, never
        // hanging — and the open-ended fallback is never reached: a dead
        // origin's answer must not be papered over by retrying the source
        // open-ended. (The fallback's own 5xx verdict is covered by the
        // pure-decision tests; the scripting cannot scope server errors to
        // the open-ended shape.)
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                server_error: FaultPolicy::Always,
                ..OriginScript::default()
            },
        );
        let dest = TempFile::new("fallback-5xx");

        let err = stager(test_limits())
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a dead origin must fail the probe");
        assert!(
            err.to_string().contains("503"),
            "the error names the origin's answer: {err}"
        );
        assert_eq!(
            origin.requests_at(None),
            0,
            "the fallback must not fire for a dead ranged origin"
        );
    }

    #[tokio::test]
    async fn size_verification_rejects_a_file_that_misses_the_origin_total() {
        // The last-line guard: if the assembled file's length ever misses
        // the origin total despite every chunk verdict passing, the stage
        // fails naming both numbers instead of handing ffmpeg a padded or
        // truncated source. No scripted origin can produce this state — the
        // chunk verdicts catch every serving contradiction first — so the
        // guard is exercised directly; the failure leaves the file the same
        // way every stage error does, through the Err path whose
        // no-leak behavior the check-stage tests prove for its siblings.
        let origin = ScriptedOrigin::start(MIB as usize, OriginScript::default());
        let dest = TempFile::new("verify");
        tokio::fs::write(dest.path(), b"truncated").await.unwrap();

        let err = stager(test_limits())
            .verify_staged_size(dest.path(), origin.content().len() as u64)
            .await
            .expect_err("a file short of the origin total must fail verification");
        let message = err.to_string();
        assert!(
            message.contains("9") && message.contains(&MIB.to_string()),
            "the error names the staged length and the origin total: {message}"
        );
    }

    #[tokio::test]
    async fn chunk_timeout_retries_then_fails() {
        // An origin that holds connections open without answering: each
        // attempt burns its deadline, the budget exhausts, the stage fails
        // naming the timeout.
        let origin = ScriptedOrigin::start(
            MIB as usize,
            OriginScript {
                stall: FaultPolicy::Always,
                ..OriginScript::default()
            },
        );
        let limits = StageLimits {
            max_attempts: 2,
            attempt_timeout: Duration::from_millis(400),
            ..test_limits()
        };
        let dest = TempFile::new("stall");

        let started = tokio::time::Instant::now();
        let err = stager(limits)
            .stage_to(&origin.url(), dest.path())
            .await
            .expect_err("a stalled origin must fail within the attempt budget");
        assert!(
            err.to_string().contains("timed out"),
            "the error names the timeout: {err}"
        );
        assert_eq!(origin.requests_at(Some(0)), 2);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(800),
            "each attempt burns its deadline: took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "failing must stay bounded: took {elapsed:?}"
        );
    }

    // ---- parse_content_range ----

    #[test]
    fn parses_the_canonical_content_range() {
        assert_eq!(
            parse_content_range("bytes 0-8388607/28311552"),
            Some((0, 8388607, 28311552))
        );
        assert_eq!(
            parse_content_range("bytes 4096-8191/12345678"),
            Some((4096, 8191, 12345678))
        );
    }

    #[test]
    fn rejects_untrustworthy_content_ranges() {
        // Wrong unit, wildcard total, wildcard range, reversed bounds, an
        // end past the declared total — none can be staged against.
        assert_eq!(parse_content_range("items 0-10/100"), None);
        assert_eq!(parse_content_range("bytes 0-100/*"), None);
        assert_eq!(parse_content_range("bytes */1234"), None);
        assert_eq!(parse_content_range("bytes 5-3/100"), None);
        assert_eq!(parse_content_range("bytes 0-100/50"), None);
        assert_eq!(parse_content_range("bytes 0-4294967296/100"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn parses_content_range_with_liberal_whitespace() {
        assert_eq!(
            parse_content_range("  bytes  0 - 9  /  100  "),
            Some((0, 9, 100))
        );
    }
}
