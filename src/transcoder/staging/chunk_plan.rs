//! The pure brain of the staging engine: the chunk plan, the concurrency
//! window over it, the retry-vs-fail verdict for every chunk outcome, and —
//! beside them — the stage loop's step decision
//! ([`next_stage_step`]): relaunch, abort-and-fail, measure, and the final
//! byte verification.
//!
//! No I/O, no logging, no side effects — the fetcher acts these decisions out
//! and owns every await point (prior art: the stdout pump's
//! [`next_pump_decision`](crate::transcoder::stdout_pump::next_pump_decision)).

/// Attempts a chunk gets (fresh connections included) before the stage gives
/// up on it. Transient edge resets are recovered well within this; a chunk
/// failing every attempt is a broken origin.
pub const MAX_CHUNK_ATTEMPTS: u32 = 3;

/// Chunks one plan may hold. An origin total is untrusted until the bytes
/// are on disk — without a cap, a huge Content-Range lie would allocate a
/// plan proportional to the lie. The cap is far past any episode source
/// (800 GiB at the 8 MiB production chunk size) while keeping a lying plan
/// allocation trivial.
pub const MAX_PLAN_CHUNKS: u64 = 100_000;

/// One ranged GET of the plan: the bytes `[offset, offset + len)` of the
/// source. Spans tile the source exactly — contiguous, disjoint, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSpan {
    pub offset: u64,
    pub len: u64,
}

/// The ordered plan of ranged GETs covering the whole source.
#[derive(Clone, Debug)]
pub struct ChunkPlan {
    spans: Vec<ChunkSpan>,
}

impl ChunkPlan {
    /// Plan the ranged GETs covering `total_size` bytes — the origin total
    /// learned from the first chunk's Content-Range — in chunks of
    /// `chunk_size`, each clamped at the source end.
    pub fn new(total_size: u64, chunk_size: u64) -> eyre::Result<Self> {
        if chunk_size == 0 {
            eyre::bail!("staging chunk size must be at least 1 byte, got 0");
        }
        if total_size == 0 {
            eyre::bail!("origin reported an empty source; nothing to stage");
        }
        let span_count = total_size.div_ceil(chunk_size);
        if span_count > MAX_PLAN_CHUNKS {
            eyre::bail!(
                "origin reports a {total_size}-byte source, more than the \
                 {MAX_PLAN_CHUNKS}-chunk staging plan allows at a {chunk_size}-byte chunk size; \
                 refusing to plan against a total that large"
            );
        }
        let spans = (0..span_count)
            .map(|i| {
                let offset = i * chunk_size;
                ChunkSpan {
                    offset,
                    len: chunk_size.min(total_size - offset),
                }
            })
            .collect();
        Ok(Self { spans })
    }

    /// The spans in launch order: offset 0 first, contiguous to the end.
    pub fn spans(&self) -> &[ChunkSpan] {
        &self.spans
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }
}

/// Sliding-window scheduler over a plan's chunks: hands chunks out in plan
/// order, at most `concurrency` in flight, and counts them home. Pure — the
/// fetcher loops `launch_next` → spawn → `mark_finished` until complete.
#[derive(Debug)]
pub struct StageScheduler {
    chunk_count: usize,
    concurrency: usize,
    next_unlaunched: usize,
    finished: Vec<bool>,
    finished_count: usize,
}

impl StageScheduler {
    /// Schedule a plan of `chunk_count` chunks with `concurrency` fetches in
    /// flight at most.
    pub fn new(chunk_count: usize, concurrency: usize) -> eyre::Result<Self> {
        if concurrency == 0 {
            eyre::bail!("staging concurrency must be at least 1, got 0");
        }
        Ok(Self {
            chunk_count,
            concurrency,
            next_unlaunched: 0,
            finished: vec![false; chunk_count],
            finished_count: 0,
        })
    }

    /// The next chunk to launch, if the window has room for it. Chunks launch
    /// strictly in plan order; at most `concurrency` are unaccounted for
    /// (launched but not yet finished) at any time.
    pub fn launch_next(&mut self) -> Option<usize> {
        let in_flight = self.next_unlaunched - self.finished_count;
        if self.next_unlaunched < self.chunk_count && in_flight < self.concurrency {
            let index = self.next_unlaunched;
            self.next_unlaunched += 1;
            Some(index)
        } else {
            None
        }
    }

    /// Account a handed-out chunk as finished, freeing its window slot.
    ///
    /// The probe chunk (chunk 0) is recorded through this protocol too: the
    /// fetcher hands it out and immediately accounts it finished — its bytes
    /// were fetched (serially, before the plan existed) while learning the
    /// origin total.
    pub fn mark_finished(&mut self, index: usize) {
        assert!(
            index < self.next_unlaunched,
            "chunk {index} was never launched"
        );
        assert!(!self.finished[index], "chunk {index} marked finished twice");
        self.finished[index] = true;
        self.finished_count += 1;
    }

    /// Every chunk handed out and accounted for.
    pub fn is_complete(&self) -> bool {
        self.finished_count == self.chunk_count
    }

    /// Chunks launched but not yet finished.
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.next_unlaunched - self.finished_count
    }

    /// Chunks not yet finished.
    #[cfg(test)]
    pub fn remaining(&self) -> usize {
        self.chunk_count - self.finished_count
    }
}

/// What one chunk-fetch attempt produced on the wire. The fetcher maps raw
/// transport events onto these; [`attempt_verdict`] decides what they mean.
#[derive(Debug)]
pub enum AttemptObservation {
    /// A body streamed to its end. On the ranged path: a 206 whose
    /// Content-Range matched the request — `expected_len` bytes declared by
    /// the headers, `received` bytes actually arrived, `total` the origin
    /// total the headers reported (validated against the plan for non-probe
    /// chunks). On the open-ended fallback there are no headers to trust:
    /// `total`/`expected_len` carry the received count and the clean EOF is
    /// the whole verdict.
    Body {
        total: u64,
        expected_len: u64,
        received: u64,
    },
    /// The origin ignored the Range header — a ranged GET answered with a
    /// 200 full body. Rung 2 of the fallback ladder keys off this: the
    /// fetcher stages the source with one open-ended GET instead. No body
    /// was written.
    RangeIgnored,
    /// The response contradicted the request in a way a retry cannot fix:
    /// 416, a missing or mismatched Content-Range. `reason` is the clear
    /// error text. No body was written.
    BadRange { reason: String },
    /// A retryable server error status (5xx, 429). No body written.
    ServerError { status: u16 },
    /// Any other non-206 status (403 expired signature, 404, ...): retrying
    /// the same URL cannot help. No body written.
    OtherStatus { status: u16 },
    /// The transport failed before or during the body: connect error,
    /// connection reset mid-body, body read error, local write error.
    Transport(String),
    /// The attempt exceeded its deadline.
    TimedOut,
}

/// The fate of one chunk after an attempt, decided purely from the
/// observation and the attempt budget.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The chunk delivered exactly its planned bytes.
    Done,
    /// Transient failure: retry the whole chunk on a fresh connection.
    Retry,
    /// The stage must fail with this error.
    Fail(String),
}

/// The clear error text for a Range-ignoring origin — one source of truth
/// for the typed error's `Display` and the verdict's `Fail` message, so
/// they cannot drift apart.
pub const RANGE_IGNORED: &str =
    "the origin ignored the Range header (answered 200 with a full body); \
     ranged staging unsupported";

/// The origin ignored the Range header (answered a ranged GET with a 200
/// full body). Travels as the stage error so the fetcher can key the
/// open-ended fallback (ladder rung 2) off this exact condition instead of
/// matching error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeIgnored {
    /// Ranged attempts the probe made up to and including the 200 — the
    /// retries it charged before the cue arrived.
    pub attempts_used: u32,
}

impl std::fmt::Display for RangeIgnored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(RANGE_IGNORED)
    }
}

impl std::error::Error for RangeIgnored {}

/// The attempt budget's verdict: retry while attempts remain, then fail the
/// stage naming the request shape, the cause and the budget — an operator
/// must be able to diagnose the origin from the log line alone.
fn retry_or_fail(at: &str, cause: String, attempts_used: u32, max_attempts: u32) -> Verdict {
    if attempts_used < max_attempts {
        Verdict::Retry
    } else {
        Verdict::Fail(format!(
            "{at}: {cause} after {attempts_used} of {max_attempts} attempts"
        ))
    }
}

/// The shared verdict shape behind [`attempt_verdict`] and
/// [`open_ended_verdict`]: one retry-or-fail budget over the transient
/// outcomes, immediate failure on the deterministic ones, and a caller rule
/// for the one arm that differs — what a streamed Body means. `at` names
/// the request shape in every failure message.
fn decide(
    at: &str,
    observation: &AttemptObservation,
    attempts_used: u32,
    max_attempts: u32,
    on_body: impl FnOnce(u64, u64) -> Verdict,
) -> Verdict {
    let retry = |cause: String| retry_or_fail(at, cause, attempts_used, max_attempts);
    match observation {
        AttemptObservation::Body {
            expected_len,
            received,
            ..
        } => on_body(*expected_len, *received),
        // Unreachable through the driver — the fetcher intercepts this
        // observation into the typed [`RangeIgnored`] error before the
        // verdict runs. The arm keeps the pure function total over every
        // observation.
        AttemptObservation::RangeIgnored => Verdict::Fail(RANGE_IGNORED.to_string()),
        AttemptObservation::BadRange { reason } => Verdict::Fail(format!("{at}: {reason}")),
        AttemptObservation::ServerError { status } => {
            retry(format!("origin returned HTTP {status}"))
        }
        AttemptObservation::OtherStatus { status } => Verdict::Fail(format!(
            "{at}: origin returned HTTP {status}; retrying the same url cannot fix it"
        )),
        AttemptObservation::Transport(detail) => retry(format!("connection failed: {detail}")),
        AttemptObservation::TimedOut => retry("the attempt timed out".to_string()),
    }
}

/// Decide a chunk's fate from one attempt's outcome and the attempt budget.
///
/// Transient outcomes (short reads, transport failures, timeouts, 5xx) retry
/// while attempts remain, then fail the stage with a clear error — a mid-body
/// drop that survives every retry must surface, not hang. Deterministic
/// contradictions (Range ignored, 416, Content-Range mismatch, over-delivery,
/// unexpected statuses) fail immediately: another identical request would
/// only reproduce them.
///
/// `attempts_used` counts the attempt this observation came from (1-based).
pub fn attempt_verdict(
    requested: ChunkSpan,
    observation: &AttemptObservation,
    attempts_used: u32,
    max_attempts: u32,
) -> Verdict {
    let at = format!(
        "chunk at offset {} ({} bytes)",
        requested.offset, requested.len
    );
    decide(
        &at,
        observation,
        attempts_used,
        max_attempts,
        |expected_len, received| match received.cmp(&expected_len) {
            std::cmp::Ordering::Equal => Verdict::Done,
            std::cmp::Ordering::Greater => Verdict::Fail(format!(
                "{at}: origin sent {received} bytes for the requested {expected_len}, \
                 ignoring the range end — its range handling cannot be trusted"
            )),
            std::cmp::Ordering::Less => retry_or_fail(
                &at,
                format!("origin ended the response after {received} of {expected_len} bytes"),
                attempts_used,
                max_attempts,
            ),
        },
    )
}

/// Decide an open-ended fallback attempt's fate — rung 2 of the fallback
/// ladder, the same budget rules as [`attempt_verdict`] under a different
/// success shape: an open-ended body has no declared total, so its clean
/// EOF is the completion criterion and whatever arrived is the truth.
/// Transport trouble, timeouts and 5xx retry on a fresh connection; a
/// deterministic answer (416, an unexpected status) fails outright.
pub fn open_ended_verdict(
    observation: &AttemptObservation,
    attempts_used: u32,
    max_attempts: u32,
) -> Verdict {
    // The body streamed to its end: `total`/`expected_len` carry the
    // received count — there is no declared total to verify against, so a
    // clean EOF is the completion criterion and whatever arrived is the
    // truth.
    decide(
        "open-ended GET",
        observation,
        attempts_used,
        max_attempts,
        |_, _| Verdict::Done,
    )
}

/// The stage loop's running state: the retry cost the origin has charged so
/// far, across the probe, the open-ended fallback and every chunk. Threaded
/// through [`next_stage_step`] the way the stdout pump threads its counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageState {
    retries_charged: u32,
}

impl StageState {
    /// Charge a finished run of `attempts` fresh-connection attempts — the
    /// one charging rule for the whole stage: every attempt beyond the
    /// run's first is a retry the origin charged. The probe, the open-ended
    /// fallback and each delivered chunk account through here.
    pub fn charge(&mut self, attempts: u32) {
        self.retries_charged += attempts.saturating_sub(1);
    }

    /// The retries charged so far.
    pub fn retries_charged(&self) -> u32 {
        self.retries_charged
    }
}

/// What the stage loop observed at its one decision point — the executor
/// builds one per iteration from the scheduler state and a single join over
/// the in-flight chunk set.
#[derive(Debug)]
pub enum StageObservation {
    /// Every chunk the plan holds is accounted finished: nothing is left to
    /// launch or join. The executor measures the assembled file next and
    /// observes its byte count.
    PlanComplete,
    /// One join over the in-flight chunks returned.
    Joined(JoinOutcome),
    /// The staged file's byte count, measured after the plan completed,
    /// against the Origin Total the plan was built from — the observation
    /// the final byte verification decides on.
    StagedByteCount { staged: u64, origin_total: u64 },
}

/// The outcome of one join over the in-flight chunk set.
#[derive(Debug)]
pub enum JoinOutcome {
    /// A chunk delivered exactly its planned bytes: `index` its plan
    /// position, `attempts` the fresh-connection attempts it charged
    /// (1-based).
    Delivered { index: usize, attempts: u32 },
    /// A chunk failed within its attempt budget or on a deterministic
    /// contradiction — `error` is the clear failure text its verdict
    /// composed.
    Failed { error: String },
    /// The chunk task itself crashed (panicked); `detail` is the join
    /// error's text.
    Crashed { detail: String },
    /// The join found nothing in flight while the plan is incomplete — the
    /// loop can only be stalled.
    NothingInFlight,
}

/// The stage loop's next step, decided purely from one observation and the
/// running state. Every way the loop can proceed or end is named here; the
/// executor only acts steps out — spawn, abort, and I/O.
#[derive(Debug, PartialEq, Eq)]
pub enum StageStep {
    /// Account chunk `index` finished — its retries are already charged
    /// into the returned state — and keep the loop going: the freed window
    /// may admit the next launch.
    Relaunch { index: usize },
    /// Every chunk is staged: the executor measures the assembled file's
    /// byte count and brings it back as a
    /// [`StageObservation::StagedByteCount`].
    MeasureStagedFile,
    /// The staged file verified against the Origin Total: the stage is
    /// done, the state carries the retries the origin charged.
    Verified,
    /// Abort every in-flight chunk and fail the stage with this error.
    AbortAndFail { error: String },
}

/// Decide the stage loop's next step from one observation and the running
/// state.
///
/// Pure: no I/O, no logging, no side effects — the executor acts the
/// returned step out and threads the returned state into the next call.
pub fn next_stage_step(
    observation: StageObservation,
    state: StageState,
) -> (StageStep, StageState) {
    match observation {
        StageObservation::Joined(JoinOutcome::Delivered { index, attempts }) => {
            let mut next = state;
            next.charge(attempts);
            (StageStep::Relaunch { index }, next)
        }
        StageObservation::Joined(JoinOutcome::Failed { error }) => {
            (StageStep::AbortAndFail { error }, state)
        }
        StageObservation::Joined(JoinOutcome::Crashed { detail }) => (
            StageStep::AbortAndFail {
                error: format!("staged chunk task crashed: {detail}"),
            },
            state,
        ),
        StageObservation::Joined(JoinOutcome::NothingInFlight) => (
            StageStep::AbortAndFail {
                error: "staged fetch stalled: no chunk in flight but the plan is incomplete"
                    .to_string(),
            },
            state,
        ),
        StageObservation::PlanComplete => (StageStep::MeasureStagedFile, state),
        StageObservation::StagedByteCount {
            staged,
            origin_total,
        } => {
            if staged == origin_total {
                (StageStep::Verified, state)
            } else {
                (
                    StageStep::AbortAndFail {
                        error: format!(
                            "the staged file is {staged} bytes but the origin reported \
                             {origin_total}; refusing to hand ffmpeg a source that cannot \
                             be byte-complete"
                        ),
                    },
                    state,
                )
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // ---- ChunkPlan ----

    #[test]
    fn plan_rejects_zero_chunk_size() {
        let err = ChunkPlan::new(1000, 0).expect_err("zero chunk size must be rejected");
        assert!(
            err.to_string().contains("chunk size"),
            "error should name the bad knob: {err}"
        );
    }

    #[test]
    fn plan_rejects_zero_total() {
        let err = ChunkPlan::new(0, 1000).expect_err("an empty source cannot be staged");
        assert!(
            err.to_string().contains("empty"),
            "error should say the source is empty: {err}"
        );
    }

    #[test]
    fn plan_rejects_a_total_that_lies_past_the_chunk_cap() {
        // The origin total is untrusted: a huge Content-Range lie must fail
        // the plan with a clear error, not allocate a plan its size.
        let err = ChunkPlan::new(u64::MAX, 1).expect_err("a u64::MAX lie must be rejected");
        let message = err.to_string();
        assert!(
            message.contains(&MAX_PLAN_CHUNKS.to_string()),
            "error names the chunk cap: {message}"
        );
        // Just under the cap still plans.
        assert!(ChunkPlan::new(MAX_PLAN_CHUNKS, 1).is_ok());
    }

    #[test]
    fn plan_exact_multiple() {
        // 16 MiB in 8 MiB chunks: two full spans, no remainder.
        let plan = ChunkPlan::new(16 * 1024 * 1024, 8 * 1024 * 1024).unwrap();
        assert_eq!(
            plan.spans(),
            &[
                ChunkSpan {
                    offset: 0,
                    len: 8 * 1024 * 1024
                },
                ChunkSpan {
                    offset: 8 * 1024 * 1024,
                    len: 8 * 1024 * 1024
                },
            ]
        );
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn plan_with_remainder_chunk() {
        // 8 MiB + 5 bytes: the tail span carries exactly the remainder.
        let plan = ChunkPlan::new(8 * 1024 * 1024 + 5, 8 * 1024 * 1024).unwrap();
        assert_eq!(
            plan.spans(),
            &[
                ChunkSpan {
                    offset: 0,
                    len: 8 * 1024 * 1024
                },
                ChunkSpan {
                    offset: 8 * 1024 * 1024,
                    len: 5
                },
            ]
        );
    }

    #[test]
    fn plan_single_span_when_total_below_chunk_size() {
        let plan = ChunkPlan::new(5, 8 * 1024 * 1024).unwrap();
        assert_eq!(plan.spans(), &[ChunkSpan { offset: 0, len: 5 }]);
    }

    #[test]
    fn plan_spans_tile_the_total_exactly() {
        // Across awkward combinations — exact multiples, ±1 off, primes,
        // unit chunks — the spans must be contiguous from 0, disjoint, in
        // order, non-empty, within the chunk size, and sum to the total.
        let chunk = 8 * 1024 * 1024u64;
        let totals = [
            1,
            chunk - 1,
            chunk,
            chunk + 1,
            3 * chunk - 1,
            3 * chunk,
            3 * chunk + 1,
            28 * 1024 * 1024 + 7777,
        ];
        for total in totals {
            for chunk_size in [1, 2, 7, chunk, 28 * 1024 * 1024 + 7777] {
                // Tiny chunks against big totals exceed the plan cap and are
                // rejected — the tiling property covers the plannable space.
                if total.div_ceil(chunk_size) > MAX_PLAN_CHUNKS {
                    continue;
                }
                let plan = ChunkPlan::new(total, chunk_size).unwrap();
                let spans = plan.spans();
                assert!(!spans.is_empty(), "total {total} chunk {chunk_size}");
                let mut expected_offset = 0u64;
                for (i, span) in spans.iter().enumerate() {
                    assert_eq!(
                        span.offset, expected_offset,
                        "total {total} chunk {chunk_size} span {i}"
                    );
                    assert!(
                        span.len > 0 && span.len <= chunk_size,
                        "total {total} chunk {chunk_size} span {i} len {}",
                        span.len
                    );
                    // Only the tail span may be shorter than the chunk size.
                    let is_tail = i == spans.len() - 1;
                    let expected_len = if is_tail {
                        total - span.offset
                    } else {
                        chunk_size
                    };
                    assert_eq!(span.len, expected_len);
                    expected_offset += span.len;
                }
                assert_eq!(expected_offset, total);
            }
        }
    }

    // ---- StageScheduler ----

    fn scheduler(chunks: usize, concurrency: usize) -> StageScheduler {
        StageScheduler::new(chunks, concurrency).unwrap()
    }

    #[test]
    fn scheduler_rejects_zero_concurrency() {
        let err = StageScheduler::new(10, 0).expect_err("zero concurrency would launch nothing");
        assert!(
            err.to_string().contains("concurrency"),
            "error should name the bad knob: {err}"
        );
    }

    #[test]
    fn scheduler_respects_the_window() {
        // 10 chunks, window 3: exactly three launches before the window
        // fills, then nothing until a chunk finishes.
        let mut sched = scheduler(10, 3);
        assert_eq!(sched.launch_next(), Some(0));
        assert_eq!(sched.launch_next(), Some(1));
        assert_eq!(sched.launch_next(), Some(2));
        assert_eq!(sched.launch_next(), None, "window is full");
        assert_eq!(sched.in_flight(), 3);
        assert_eq!(sched.remaining(), 10);
        assert!(!sched.is_complete());
    }

    #[test]
    fn scheduler_launches_in_plan_order() {
        let mut sched = scheduler(4, 4);
        assert_eq!(
            (
                sched.launch_next(),
                sched.launch_next(),
                sched.launch_next()
            ),
            (Some(0), Some(1), Some(2))
        );
        assert_eq!(sched.launch_next(), Some(3));
        assert_eq!(sched.launch_next(), None);
    }

    #[test]
    fn scheduler_frees_a_slot_on_finish() {
        // Finishing any in-flight chunk frees exactly one slot; the next
        // chunk in order takes it.
        let mut sched = scheduler(5, 2);
        assert_eq!(sched.launch_next(), Some(0));
        assert_eq!(sched.launch_next(), Some(1));
        assert_eq!(sched.launch_next(), None);

        sched.mark_finished(1);
        assert_eq!(sched.in_flight(), 1);
        assert_eq!(sched.launch_next(), Some(2));
        assert_eq!(sched.launch_next(), None);

        sched.mark_finished(0);
        assert_eq!(sched.launch_next(), Some(3));

        sched.mark_finished(2);
        sched.mark_finished(3);
        assert_eq!(sched.launch_next(), Some(4));
        sched.mark_finished(4);
        assert!(sched.is_complete());
        assert_eq!(sched.remaining(), 0);
        assert_eq!(sched.in_flight(), 0);
        assert_eq!(sched.launch_next(), None, "nothing left to launch");
    }

    #[test]
    fn scheduler_concurrency_one_is_serial() {
        let mut sched = scheduler(3, 1);
        assert_eq!(sched.launch_next(), Some(0));
        assert_eq!(sched.launch_next(), None);
        sched.mark_finished(0);
        assert_eq!(sched.launch_next(), Some(1));
        assert_eq!(sched.launch_next(), None);
        sched.mark_finished(1);
        assert_eq!(sched.launch_next(), Some(2));
        sched.mark_finished(2);
        assert!(sched.is_complete());
    }

    #[test]
    fn scheduler_window_exceeding_chunk_count_launchs_all() {
        let mut sched = scheduler(2, 8);
        assert_eq!(sched.launch_next(), Some(0));
        assert_eq!(sched.launch_next(), Some(1));
        assert_eq!(sched.launch_next(), None);
        sched.mark_finished(0);
        sched.mark_finished(1);
        assert!(sched.is_complete());
    }

    #[test]
    fn scheduler_can_pre_complete_the_probe_chunk() {
        // Chunk 0's bytes were fetched while learning the origin total, so
        // the fetcher hands it out and accounts it finished before any other
        // launch — the window then fills from chunk 1 with the full
        // concurrency.
        let mut sched = scheduler(5, 4);
        assert_eq!(sched.launch_next(), Some(0));
        sched.mark_finished(0);
        assert_eq!(sched.remaining(), 4);
        assert_eq!(sched.in_flight(), 0);
        assert_eq!(
            (
                sched.launch_next(),
                sched.launch_next(),
                sched.launch_next(),
                sched.launch_next()
            ),
            (Some(1), Some(2), Some(3), Some(4))
        );
        assert_eq!(sched.launch_next(), None);
        sched.mark_finished(1);
        sched.mark_finished(2);
        sched.mark_finished(3);
        sched.mark_finished(4);
        assert!(sched.is_complete());
    }

    #[test]
    #[should_panic(expected = "was never launched")]
    fn scheduler_rejects_finishing_an_unlaunched_chunk() {
        let mut sched = scheduler(3, 2);
        sched.launch_next();
        sched.mark_finished(1); // 1 was never handed out
    }

    #[test]
    #[should_panic(expected = "twice")]
    fn scheduler_rejects_double_finish() {
        let mut sched = scheduler(3, 2);
        sched.launch_next();
        sched.mark_finished(0);
        sched.mark_finished(0);
    }

    // ---- attempt_verdict ----

    const SPAN: ChunkSpan = ChunkSpan {
        offset: 8 * 1024 * 1024,
        len: 8 * 1024 * 1024,
    };
    const MAX: u32 = MAX_CHUNK_ATTEMPTS;

    fn delivered(received: u64) -> AttemptObservation {
        AttemptObservation::Body {
            total: 28 * 1024 * 1024,
            expected_len: SPAN.len,
            received,
        }
    }

    #[test]
    fn exact_delivery_is_done() {
        assert_eq!(
            attempt_verdict(SPAN, &delivered(SPAN.len), 1, MAX),
            Verdict::Done
        );
        assert_eq!(
            attempt_verdict(SPAN, &delivered(SPAN.len), MAX, MAX),
            Verdict::Done,
            "a last-attempt success is still a success"
        );
    }

    #[test]
    fn short_read_retries_within_budget() {
        let short = delivered(SPAN.len - 1);
        assert_eq!(
            attempt_verdict(SPAN, &short, 1, MAX),
            Verdict::Retry,
            "one dropped mid-body read gets a fresh connection"
        );
        assert_eq!(attempt_verdict(SPAN, &short, MAX - 1, MAX), Verdict::Retry);
    }

    #[test]
    fn short_read_surviving_every_retry_fails_with_a_clear_error() {
        let short = delivered(SPAN.len - 4096);
        let verdict = attempt_verdict(SPAN, &short, MAX, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("a drop that survives the budget must fail, got {verdict:?}");
        };
        assert!(
            msg.contains("8384512"),
            "error names the received count: {msg}"
        );
        assert!(msg.contains("3 of 3"), "error names the attempts: {msg}");
        assert!(msg.contains("bytes"), "error is about bytes: {msg}");
    }

    #[test]
    fn over_delivery_fails_immediately() {
        // A server that ignores the range end cannot be trusted with any
        // span; retrying would only fetch more junk.
        let over = delivered(SPAN.len + 1);
        let verdict = attempt_verdict(SPAN, &over, 1, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("over-delivery must fail, got {verdict:?}");
        };
        assert!(msg.contains("8388609"), "error names the over-read: {msg}");
        assert!(
            msg.contains("8388608"),
            "error names the expected size: {msg}"
        );
    }

    #[test]
    fn bad_range_fails_immediately_even_with_budget_left() {
        // Content-Range lies and Range-ignoring origins answer the same way
        // every time — retries would only reproduce the contradiction.
        let observation = AttemptObservation::BadRange {
            reason: "origin ignored the Range header".to_string(),
        };
        let verdict = attempt_verdict(SPAN, &observation, 1, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("a deterministic contradiction must fail, got {verdict:?}");
        };
        assert!(
            msg.contains("origin ignored the Range header"),
            "error carries the reason: {msg}"
        );
        assert!(msg.contains("8388608"), "error names the span: {msg}");
    }

    #[test]
    fn server_error_retries_within_budget() {
        let observation = AttemptObservation::ServerError { status: 503 };
        assert_eq!(attempt_verdict(SPAN, &observation, 1, MAX), Verdict::Retry);
        assert_eq!(
            attempt_verdict(SPAN, &observation, MAX - 1, MAX),
            Verdict::Retry
        );
    }

    #[test]
    fn permanent_server_error_fails_after_the_budget() {
        let observation = AttemptObservation::ServerError { status: 503 };
        let verdict = attempt_verdict(SPAN, &observation, MAX, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("a permanently 5xxing chunk must fail, got {verdict:?}");
        };
        assert!(msg.contains("503"), "error names the status: {msg}");
        assert!(msg.contains("3 of 3"), "error names the attempts: {msg}");
    }

    #[test]
    fn other_status_fails_immediately() {
        // 403 (expired signature), 404, ... — the same URL will answer the
        // same way; the evict-and-retry loop upstream owns URL refresh.
        let observation = AttemptObservation::OtherStatus { status: 403 };
        let verdict = attempt_verdict(SPAN, &observation, 1, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("an unexpected status must fail, got {verdict:?}");
        };
        assert!(msg.contains("403"), "error names the status: {msg}");
    }

    #[test]
    fn transport_failure_retries_within_budget() {
        let observation = AttemptObservation::Transport("connection reset".to_string());
        assert_eq!(attempt_verdict(SPAN, &observation, 1, MAX), Verdict::Retry);
    }

    #[test]
    fn sustained_transport_failure_fails_after_the_budget() {
        let observation = AttemptObservation::Transport("connection reset".to_string());
        let verdict = attempt_verdict(SPAN, &observation, MAX, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("a dead connection must fail, got {verdict:?}");
        };
        assert!(
            msg.contains("connection reset"),
            "error carries the cause: {msg}"
        );
        assert!(msg.contains("3 of 3"), "error names the attempts: {msg}");
    }

    #[test]
    fn timeout_retries_within_budget_and_fails_beyond() {
        let observation = AttemptObservation::TimedOut;
        assert_eq!(attempt_verdict(SPAN, &observation, 1, MAX), Verdict::Retry);
        let verdict = attempt_verdict(SPAN, &observation, MAX, MAX);
        let Verdict::Fail(msg) = verdict else {
            panic!("a chunk that times out every attempt must fail, got {verdict:?}");
        };
        assert!(msg.contains("timed out"), "error names the timeout: {msg}");
    }

    #[test]
    fn single_attempt_budget_fails_on_first_transient() {
        // A zero-tolerance budget turns any transient into an immediate,
        // clear stage failure.
        for observation in [
            AttemptObservation::TimedOut,
            AttemptObservation::Transport("reset".to_string()),
            AttemptObservation::ServerError { status: 500 },
            delivered(SPAN.len - 1),
        ] {
            let verdict = attempt_verdict(SPAN, &observation, 1, 1);
            assert!(
                matches!(verdict, Verdict::Fail(_)),
                "budget 1 must fail on {observation:?}, got {verdict:?}"
            );
        }
    }

    // ---- open_ended_verdict (the Range-ignored fallback's attempts) ----

    #[test]
    fn open_ended_body_to_its_end_is_done_whatever_its_length() {
        // No declared total exists on the fallback path: a clean EOF is the
        // completion criterion, and whatever arrived is the truth.
        for received in [1_u64, 4096, 5 * 1024 * 1024] {
            let observation = AttemptObservation::Body {
                total: received,
                expected_len: received,
                received,
            };
            assert_eq!(
                open_ended_verdict(&observation, 1, MAX),
                Verdict::Done,
                "a clean EOF at {received} bytes is done"
            );
        }
    }

    #[test]
    fn open_ended_transient_outcomes_retry_within_budget_then_fail() {
        let outcomes = [
            AttemptObservation::TimedOut,
            AttemptObservation::Transport("connection reset".to_string()),
            AttemptObservation::ServerError { status: 503 },
        ];
        for observation in &outcomes {
            assert_eq!(
                open_ended_verdict(observation, 1, MAX),
                Verdict::Retry,
                "a transient open-ended failure gets a fresh connection: {observation:?}"
            );
        }
        for observation in &outcomes {
            let verdict = open_ended_verdict(observation, MAX, MAX);
            let Verdict::Fail(msg) = verdict else {
                panic!("a hopeless open-ended attempt must fail, got {verdict:?}");
            };
            assert!(
                msg.contains("open-ended GET") && msg.contains("3 of 3"),
                "the error names the fallback and the exhausted budget: {msg}"
            );
        }
    }

    // ---- next_stage_step: the stage loop's step decision ----

    #[test]
    fn a_delivered_chunk_relaunches_the_loop_and_charges_its_retries() {
        // The loop's happy step: a chunk that reported home frees its window
        // slot and the loop relaunches. A first-attempt delivery charges
        // nothing; a recovered chunk's retries are every attempt beyond its
        // first.
        let (step, state) = next_stage_step(
            StageObservation::Joined(JoinOutcome::Delivered {
                index: 3,
                attempts: 1,
            }),
            StageState::default(),
        );
        assert_eq!(step, StageStep::Relaunch { index: 3 });
        assert_eq!(state.retries_charged(), 0);

        let (step, state) = next_stage_step(
            StageObservation::Joined(JoinOutcome::Delivered {
                index: 5,
                attempts: 3,
            }),
            state,
        );
        assert_eq!(step, StageStep::Relaunch { index: 5 });
        assert_eq!(
            state.retries_charged(),
            2,
            "two of the three attempts were retries"
        );
    }

    #[test]
    fn a_failed_chunk_aborts_the_stage_without_charging_its_retries() {
        // A chunk that dies within its budget never reports home, so its
        // attempts charge nothing — the stage fails with the chunk's own
        // clear error, exactly as its verdict composed it.
        let (step, state) = next_stage_step(
            StageObservation::Joined(JoinOutcome::Failed {
                error: "chunk at offset 8388608 (8388608 bytes): origin returned HTTP 503 \
                        after 3 of 3 attempts"
                    .to_string(),
            }),
            StageState::default(),
        );
        let StageStep::AbortAndFail { error } = step else {
            panic!("a failed chunk must abort the stage, got {step:?}");
        };
        assert!(
            error.contains("503") && error.contains("3 of 3"),
            "the stage fails with the chunk's error: {error}"
        );
        assert_eq!(state.retries_charged(), 0, "a failed chunk charges nothing");
    }

    #[test]
    fn a_crashed_chunk_task_aborts_the_stage_naming_the_crash() {
        // A chunk task that panicked never reported a verdict: the stage
        // must fail naming the crash, not silently hang on a window slot
        // that will never free.
        let (step, _) = next_stage_step(
            StageObservation::Joined(JoinOutcome::Crashed {
                detail: "chunk task panicked".to_string(),
            }),
            StageState::default(),
        );
        let StageStep::AbortAndFail { error } = step else {
            panic!("a crashed chunk task must abort the stage, got {step:?}");
        };
        assert!(
            error.contains("staged chunk task crashed") && error.contains("chunk task panicked"),
            "the error names the crash and its detail: {error}"
        );
    }

    #[test]
    fn an_empty_join_over_an_incomplete_plan_aborts_as_stalled() {
        // The loop can only see an empty join while the plan is incomplete
        // if something consumed a window slot without a task behind it: a
        // stall, never a wait — fail it as one.
        let (step, _) = next_stage_step(
            StageObservation::Joined(JoinOutcome::NothingInFlight),
            StageState::default(),
        );
        let StageStep::AbortAndFail { error } = step else {
            panic!("a stalled loop must abort the stage, got {step:?}");
        };
        assert!(
            error.contains("stalled") && error.contains("incomplete"),
            "the error names the stall: {error}"
        );
    }

    #[test]
    fn a_complete_plan_hands_over_to_the_byte_measurement() {
        // Every chunk accounted finished: the loop's staging phase ends and
        // the final byte verification begins — the executor measures the
        // assembled file and observes its byte count.
        let (step, state) = next_stage_step(StageObservation::PlanComplete, StageState::default());
        assert_eq!(step, StageStep::MeasureStagedFile);
        assert_eq!(state.retries_charged(), 0, "completion charges nothing");
    }

    #[test]
    fn staged_bytes_matching_the_origin_total_verify() {
        // The stage's success outcome: the assembled file holds exactly the
        // Origin Total the plan was built from.
        let (step, state) = next_stage_step(
            StageObservation::StagedByteCount {
                staged: 8 * 1024 * 1024,
                origin_total: 8 * 1024 * 1024,
            },
            StageState::default(),
        );
        assert_eq!(step, StageStep::Verified);
        assert_eq!(state.retries_charged(), 0);
    }

    #[test]
    fn a_falsified_origin_total_fails_the_byte_verification_naming_both_numbers() {
        // The last-line guard, reachable with no origin at all: every chunk
        // verdict passed, yet the assembled file's byte count misses the
        // Origin Total the probe's Content-Range declared — a falsified
        // observation no scripted origin can produce at the HTTP layer (the
        // chunk verdicts catch every serving contradiction first). Both the
        // truncated and the padded direction must refuse to hand ffmpeg a
        // source that cannot be byte-complete, naming both numbers.
        for (staged, direction) in [(9, "truncated"), (8 * 1024 * 1024 + 1, "padded")] {
            let (step, _) = next_stage_step(
                StageObservation::StagedByteCount {
                    staged,
                    origin_total: 8 * 1024 * 1024,
                },
                StageState::default(),
            );
            let StageStep::AbortAndFail { error } = step else {
                panic!("a {direction} staged file must fail verification, got {step:?}");
            };
            assert!(
                error.contains(&staged.to_string())
                    && error.contains(&(8 * 1024 * 1024).to_string()),
                "the {direction} file's error names the staged length and the origin total: \
                 {error}"
            );
            assert!(
                error.contains("byte-complete"),
                "the {direction} file's error says what was refused: {error}"
            );
        }
    }

    #[test]
    fn retry_charging_is_one_rule_for_the_probe_the_fallback_and_the_chunks() {
        // The charging rule — of a finished run, every attempt beyond its
        // first — is the stage's one accounting seam: the probe's run, the
        // open-ended fallback's and each delivered chunk's charge into the
        // same ledger, and the report reads nothing else.
        let mut state = StageState::default();
        state.charge(1); // the probe: one clean attempt
        assert_eq!(state.retries_charged(), 0);
        state.charge(3); // a chunk recovered on its third attempt
        assert_eq!(state.retries_charged(), 2);
        state.charge(2); // the open-ended fallback's one retry
        assert_eq!(state.retries_charged(), 3);
        state.charge(0); // defensive: a run reporting no attempts
        assert_eq!(state.retries_charged(), 3);
    }

    #[test]
    fn open_ended_deterministic_outcomes_fail_immediately() {
        // A 416 or an unexpected status to a plain GET answers the same way
        // every time; the empty-body 200 is equally hopeless.
        let outcomes = [
            AttemptObservation::BadRange {
                reason: "416".to_string(),
            },
            AttemptObservation::OtherStatus { status: 403 },
            AttemptObservation::RangeIgnored,
        ];
        for observation in &outcomes {
            let verdict = open_ended_verdict(observation, 1, MAX);
            assert!(
                matches!(verdict, Verdict::Fail(_)),
                "a deterministic open-ended outcome must fail on its first attempt: {observation:?}"
            );
        }
    }
}
