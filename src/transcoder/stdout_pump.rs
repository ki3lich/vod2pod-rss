//! The stdout pump: forwarding ffmpeg's transcoded stdout to the client
//! channel, and enforcing the stderr policy.
//!
//! Every decision lives in one pure function ([`next_pump_decision`]); the
//! thread bodies are thin executors that observe, decide, and act — no
//! policy of their own.

use std::io::Read;
use std::thread::sleep;
use std::time::Duration;

use actix_web::web::Bytes;
use log::{debug, error, info, warn};
use tokio::sync::mpsc::Sender;

use super::ChildGuard;

/// One item yielded to the client: a chunk of transcoded audio, or the error
/// that ends the transfer.
pub(super) type ChannelBytes = Result<Bytes, std::io::Error>;

/// What the pump observed between two decisions: the outcome of one stdout
/// read, or the client channel rejecting a forwarded chunk.
#[derive(Debug)]
pub(super) enum PumpObservation {
    /// A completed `read` on ffmpeg's stdout: `Ok(n)` bytes read (`n == 0` is
    /// EOF), or the read error.
    Read(std::io::Result<usize>),
    /// The client channel rejected a forwarded chunk — the receiver is gone.
    ClientGone,
}

/// The declared transfer length and the pump's running counters. The
/// expected length is fixed for the whole transfer; the other two move with
/// every step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PumpState {
    expected_bytes_count: usize,
    sent_bytes_count: usize,
    interrupt_streak: u32,
}

impl PumpState {
    /// The state a pump starts in: nothing sent, no interruptions stacked.
    pub(super) fn start(expected_bytes_count: usize) -> Self {
        Self {
            expected_bytes_count,
            sent_bytes_count: 0,
            interrupt_streak: 0,
        }
    }
}

/// The mode the pump's next step takes, decided purely from what was
/// observed and the running state. Every way the transfer can proceed or end
/// is named here; the executor only acts modes out.
#[derive(Debug)]
pub(super) enum PumpDecision {
    /// Forward `chunk_len` bytes of the just-read buffer and keep going.
    Continue { chunk_len: usize },
    /// ffmpeg produced past the declared length: forward the remaining
    /// `remaining` bytes of the just-read buffer, then end the transfer —
    /// the declared length is fulfilled exactly.
    Truncate { remaining: usize },
    /// Clean EOF within the shortfall tolerance (see
    /// [`MAX_OK_SHORTFALL_RATIO`]): pad `padding` NUL bytes up to the
    /// declared length, then end the transfer as complete.
    Pad { padding: usize },
    /// The transfer failed: surface this error as the terminating item and
    /// stop.
    Fail(std::io::Error),
    /// The client is gone: stop without sending anything further.
    Kill,
    /// Transient interruption: back off, then read again.
    Retry,
}

/// Consecutive `Interrupted` reads tolerated on the transcoded stdout before
/// the transfer is failed. The budget is per *consecutive* streak: any
/// successful read restarts it (see [`next_pump_decision`]), so transient
/// interruptions spread across a long transcode never accumulate into a
/// failure — only a sustained outage exhausts it.
pub(super) const MAX_CONSECUTIVE_READ_INTERRUPTS: u32 = 10;

/// The shortfall tolerance, named: how far below the declared length a clean
/// EOF may land — as a ratio of the declared length — and still count as a
/// finished, slightly-short stream to pad, rather than a prematurely dead
/// one to fail. Encoder overhead and duration rounding make real streams
/// land a fraction of a percent short; a googlevideo edge that delivers ~2s
/// of a 50-minute episode (the incident pinned by
/// `transcode_stream_errors_when_input_ends_far_short_of_expected`) lands
/// orders of magnitude beyond it. Current value: 10%.
pub(super) const MAX_OK_SHORTFALL_RATIO: f64 = 0.1;

/// Decide the pump's next step from one observation and the running state.
///
/// Pure: no I/O, no logging, no side effects — the executor acts the
/// returned decision out and threads the returned state into the next call.
pub(super) fn next_pump_decision(
    observation: PumpObservation,
    state: PumpState,
) -> (PumpDecision, PumpState) {
    match observation {
        PumpObservation::Read(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
            if state.interrupt_streak >= MAX_CONSECUTIVE_READ_INTERRUPTS {
                // The budget is consecutive: this streak never saw a
                // successful read, so the sustained outage fails the transfer.
                (PumpDecision::Fail(e), state)
            } else {
                (
                    PumpDecision::Retry,
                    PumpState {
                        interrupt_streak: state.interrupt_streak + 1,
                        ..state
                    },
                )
            }
        }
        PumpObservation::Read(Err(e)) => (PumpDecision::Fail(e), state),
        PumpObservation::ClientGone => (PumpDecision::Kill, state),
        PumpObservation::Read(Ok(read_bytes)) => {
            // Any successful read restarts the interruption budget: only
            // *consecutive* interruptions may exhaust it, so isolated
            // transient interruptions across a long transcode can never
            // accumulate into a failure.
            let state = PumpState {
                interrupt_streak: 0,
                ..state
            };
            if read_bytes == 0 {
                // An input that dies early is indistinguishable from a finished
                // one at the stdout level: both close the pipe, and ffmpeg
                // exits 0 with an empty stderr. Only the byte shortfall tells
                // them apart. Within MAX_OK_SHORTFALL_RATIO the EOF is a
                // finished, slightly-shorter-than-declared stream (encoder
                // overhead, duration rounding) and keeps the historical
                // padding; beyond it the upstream ended prematurely, and
                // padding would yield a full-length 200 of mostly NUL bytes
                // that podcatchers cache and then fail to play — fail the
                // stream instead so the client sees a broken transfer and
                // retries later. Tail ranges mapping past the real end of a
                // shorter-than-declared stream land on the same error path on
                // purpose: a broken transfer beats cached zeros.
                let shortfall = state.expected_bytes_count - state.sent_bytes_count;
                if shortfall as f64 > state.expected_bytes_count as f64 * MAX_OK_SHORTFALL_RATIO
                {
                    return (
                        PumpDecision::Fail(std::io::Error::other(format!(
                            "media stream ended prematurely: got {} of {} bytes",
                            state.sent_bytes_count, state.expected_bytes_count
                        ))),
                        state,
                    );
                }
                return (PumpDecision::Pad { padding: shortfall }, state);
            }
            if state.sent_bytes_count + read_bytes > state.expected_bytes_count {
                // The declared length is the transfer's total: hand over only
                // what is still owed and stop counting.
                return (
                    PumpDecision::Truncate {
                        remaining: state.expected_bytes_count - state.sent_bytes_count,
                    },
                    state,
                );
            }
            (
                PumpDecision::Continue { chunk_len: read_bytes },
                PumpState {
                    sent_bytes_count: state.sent_bytes_count + read_bytes,
                    ..state
                },
            )
        }
    }
}

/// The stderr policy, named: ANY line ffmpeg writes to stderr fails the
/// stream. ffmpeg runs with `-loglevel error`, so stderr stays silent unless
/// something is genuinely wrong — there is no severity ranking and no line
/// is ever ignored; the first line surfaced becomes the transfer's
/// terminating error. A closed stderr (EOF) is the normal end of output,
/// not an error.
#[derive(Debug)]
pub(super) enum StderrDecision {
    /// stderr reached EOF: ffmpeg said everything it had to say.
    Closed,
    /// An ffmpeg error line: fail the stream with this error. The executor
    /// keeps draining stderr so later lines still reach the operator's log.
    Fail(std::io::Error),
    /// Reading stderr itself failed: fail the stream with this error and
    /// stop reading.
    ReadFailed(std::io::Error),
}

/// Decide what one stderr read means, per the stderr policy above.
pub(super) fn stderr_read_decision(read: std::io::Result<String>) -> StderrDecision {
    match read {
        // `read_to_string` reports EOF as zero bytes read: nothing written.
        Ok(line) if line.is_empty() => StderrDecision::Closed,
        Ok(line) => StderrDecision::Fail(std::io::Error::other(line)),
        Err(e) => StderrDecision::ReadFailed(e),
    }
}

/// How long to wait before retrying an `Interrupted` stdout read.
const READ_INTERRUPT_BACKOFF: Duration = Duration::from_secs(1);

/// Spawn the pump's two threads — the stdout forwarder (owns `child` and
/// stops/reaps it when the transfer ends) and the stderr policy enforcer —
/// both thin executors of their decision functions.
pub(super) fn spawn_pump_threads(
    child: ChildGuard,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    tx: Sender<ChannelBytes>,
    expected_bytes_count: usize,
) {
    let tx_stdout = tx.clone();
    let tx_stderr = tx;

    // stderr thread
    std::thread::spawn(move || run_stderr_thread(stderr, tx_stderr));

    // stdout thread
    std::thread::spawn(move || {
        pump_ffmpeg_stdout(
            stdout,
            child,
            tx_stdout,
            expected_bytes_count,
            READ_INTERRUPT_BACKOFF,
        )
    });
}

/// The stderr thread: drain ffmpeg's stderr, deciding each read through
/// [`stderr_read_decision`]. Error lines are surfaced — the first one ends
/// the transfer at the client — and keep being logged for the operator; a
/// read failure surfaces and stops.
fn run_stderr_thread(mut err: impl Read, tx_stderr: Sender<ChannelBytes>) {
    loop {
        let mut buf = String::new();
        let read = err.read_to_string(&mut buf).map(|_| buf);
        match stderr_read_decision(read) {
            StderrDecision::Closed => {
                debug!("ffmpeg stderr closed");
                break;
            }
            StderrDecision::Fail(e) => {
                error!("{e}");
                let _ = tx_stderr.blocking_send(Err(e));
            }
            StderrDecision::ReadFailed(e) => {
                error!("failed to read from stderr: {e}");
                let _ = tx_stderr.blocking_send(Err(e));
                break;
            }
        }
    }
}

/// Forward ffmpeg's transcoded stdout to the client channel: the thin
/// executor of [`next_pump_decision`].
///
/// Runs on its own thread (blocking reads): reads `out` in chunks, asks
/// [`next_pump_decision`] what the step means, and acts the decision out —
/// forwarding chunks, truncating over-delivery, padding shortfalls, failing
/// the transfer, or backing off retries. It holds no policy of its own.
fn pump_ffmpeg_stdout<R: Read>(
    mut out: R,
    mut child: ChildGuard,
    tx_stdout: Sender<ChannelBytes>,
    expected_bytes_count: usize,
    interrupt_backoff: Duration,
) {
    const BUFFER_SIZE: usize = 16384;
    let mut buff: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];
    let mut state = PumpState::start(expected_bytes_count);
    loop {
        let (decision, next_state) =
            next_pump_decision(PumpObservation::Read(out.read(&mut buff)), state);
        state = next_state;
        match decision {
            PumpDecision::Continue { chunk_len } => {
                if let Err(e) =
                    tx_stdout.blocking_send(Ok(Bytes::copy_from_slice(&buff[..chunk_len])))
                {
                    debug!("{}", e);
                    // The receiver is gone. That observation goes through the
                    // same decision function as everything else; it can only
                    // decide kill.
                    match next_pump_decision(PumpObservation::ClientGone, state).0 {
                        PumpDecision::Kill => {
                            info!("connection to client dropped, stopping transcode");
                            stop_child(&mut child);
                            break;
                        }
                        other => unreachable!(
                            "a client-gone observation decides kill, got {other:?}"
                        ),
                    }
                }
            }
            PumpDecision::Truncate { remaining } => {
                _ = tx_stdout.blocking_send(Ok(Bytes::copy_from_slice(&buff[..remaining])));
                info!("transcoded everything in partial request");
                stop_child(&mut child);
                break;
            }
            PumpDecision::Pad { padding } => {
                info!("transcoded everything");
                debug!("sending {padding} bytes of padding");
                const NULL_BUFF: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];
                let mut padding = padding;
                while padding > 0 {
                    let chunk = padding.min(BUFFER_SIZE);
                    _ = tx_stdout
                        .blocking_send(Ok(Bytes::copy_from_slice(&NULL_BUFF[..chunk])));
                    padding -= chunk;
                }
                _ = child.wait();
                break;
            }
            PumpDecision::Fail(e) => {
                error!("{e}");
                fail_transfer(&tx_stdout, &mut child, e);
                break;
            }
            PumpDecision::Kill => unreachable!("only a client-gone observation decides kill"),
            PumpDecision::Retry => {
                warn!("read was interrupted, retrying in {interrupt_backoff:?}");
                sleep(interrupt_backoff);
            }
        }
    }
}

/// Stop ffmpeg and reap it: the transfer is over while the child may still
/// be producing. Killing an already-exited child is a no-op error, ignored.
fn stop_child(child: &mut ChildGuard) {
    _ = child.kill();
    _ = child.wait();
}

/// Surface `e` to the client as the transfer's terminating error, then stop
/// ffmpeg and the pump: the transfer is over, and reading on could only
/// produce items after the error the client already saw.
fn fail_transfer(tx_stdout: &Sender<ChannelBytes>, child: &mut ChildGuard, e: std::io::Error) {
    if let Err(err) = tx_stdout.blocking_send(Err(e)) {
        error!("unexpected error occurred:");
        error!("{}", err);
    }
    stop_child(child);
}

#[cfg(test)]
mod test {
    use super::*;
    use tokio::sync::mpsc::channel;

    fn interrupted() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "synthetic read interruption",
        )
    }

    #[test]
    fn interruptions_interleaved_with_reads_do_not_exhaust_retry_budget() {
        // The retry budget is consecutive, not cumulative: any successful
        // read restarts it, so a long transcode survives arbitrarily many
        // isolated interruptions. A counter that never resets treats each
        // interruption as draining one shared lifetime budget, and enough of
        // them over a long transcode kill an otherwise healthy stream.
        const CHUNK_LEN: usize = 100;
        // Far more interruptions than the budget tolerates in a row.
        const INTERLEAVED_INTERRUPTIONS: usize = 15;
        let expected = INTERLEAVED_INTERRUPTIONS * CHUNK_LEN;
        let mut state = PumpState::start(expected);
        for _ in 0..INTERLEAVED_INTERRUPTIONS {
            let (decision, next) =
                next_pump_decision(PumpObservation::Read(Err(interrupted())), state);
            assert!(
                matches!(decision, PumpDecision::Retry),
                "stream failed under interruptions interleaved with successful reads: {decision:?}"
            );
            state = next;
            let (decision, next) = next_pump_decision(PumpObservation::Read(Ok(CHUNK_LEN)), state);
            assert!(
                matches!(decision, PumpDecision::Continue { .. }),
                "a successful read must keep the stream going, got {decision:?}"
            );
            state = next;
        }
        // Every interruption hit a fresh budget because every read restarted
        // it: the clean EOF lands as a complete transfer with no shortfall.
        assert_eq!(state.sent_bytes_count, expected);
        let (decision, _) = next_pump_decision(PumpObservation::Read(Ok(0)), state);
        assert!(
            matches!(decision, PumpDecision::Pad { padding: 0 }),
            "expected a complete transfer at EOF, got {decision:?}"
        );
    }

    #[test]
    fn consecutive_interruptions_beyond_retry_budget_fail_the_stream() {
        // The other half of the retry contract: a sustained outage —
        // interruptions back to back, never a successful read in between —
        // must still fail the transfer instead of retrying forever. The
        // reset-on-success repair must not turn the budget into a no-op.
        let mut state = PumpState::start(1000);
        for _ in 0..MAX_CONSECUTIVE_READ_INTERRUPTS {
            let (decision, next) =
                next_pump_decision(PumpObservation::Read(Err(interrupted())), state);
            assert!(
                matches!(decision, PumpDecision::Retry),
                "interrupted within the budget, got {decision:?}"
            );
            state = next;
        }
        let (decision, _) = next_pump_decision(PumpObservation::Read(Err(interrupted())), state);
        assert!(
            matches!(decision, PumpDecision::Fail(_)),
            "a sustained outage must fail the transfer, got {decision:?}"
        );
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

    /// A real child for the pump's stop/reap semantics; `true` exits
    /// immediately and is reaped without side effects.
    fn spawned_child() -> ChildGuard {
        ChildGuard(std::process::Command::new("true").spawn().unwrap())
    }

    #[test]
    fn pump_executor_survives_interruptions_interleaved_with_reads() {
        // Executor-level pin of the retry contract: the decision logic is
        // tested directly above; this proves the executor threads the state
        // through the real loop without dropping it.
        const CHUNK_LEN: usize = 100;
        // Far more interruptions than the budget tolerates in a row.
        const INTERLEAVED_INTERRUPTIONS: usize = 15;
        let mut steps = std::collections::VecDeque::new();
        for _ in 0..INTERLEAVED_INTERRUPTIONS {
            steps.push_back(Err(interrupted()));
            steps.push_back(Ok(vec![0xAA; CHUNK_LEN]));
        }
        steps.push_back(Ok(Vec::new())); // clean EOF
        let expected = INTERLEAVED_INTERRUPTIONS * CHUNK_LEN;

        let (tx, mut rx) = channel(100);
        std::thread::spawn(move || {
            pump_ffmpeg_stdout(
                ScriptedSource { steps },
                spawned_child(),
                tx,
                expected,
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
            received, expected,
            "stream was cut short by interruptions"
        );
    }

    #[test]
    fn pump_executor_fails_sustained_outage_and_stops() {
        // The executor surfaces exactly one terminating error for a
        // sustained outage and then stops producing.
        let steps: std::collections::VecDeque<_> = (0..MAX_CONSECUTIVE_READ_INTERRUPTS + 1)
            .map(|_| Err(interrupted()))
            .collect();

        let (tx, mut rx) = channel(100);
        std::thread::spawn(move || {
            pump_ffmpeg_stdout(
                ScriptedSource { steps },
                spawned_child(),
                tx,
                1000,
                Duration::ZERO,
            );
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

    #[test]
    fn pump_executor_truncates_over_delivery_to_the_declared_length() {
        // Over-delivery is cut at the declared length: the executor forwards
        // the just-read buffer sliced to what is still owed, then stops.
        let mut steps = std::collections::VecDeque::new();
        steps.push_back(Ok(vec![0xAA; 60]));
        steps.push_back(Ok(vec![0xBB; 60])); // 60 + 60 > 100: only 40 owed

        let (tx, mut rx) = channel(100);
        std::thread::spawn(move || {
            pump_ffmpeg_stdout(ScriptedSource { steps }, spawned_child(), tx, 100, Duration::ZERO);
        });

        let mut received: Vec<u8> = Vec::new();
        while let Some(item) = rx.blocking_recv() {
            match item {
                Ok(bytes) => received.extend_from_slice(&bytes),
                Err(e) => panic!("over-delivery must truncate, not fail: {e}"),
            }
        }

        assert_eq!(
            received.len(),
            100,
            "transfer must end at the declared length"
        );
        assert!(received[..60].iter().all(|&b| b == 0xAA));
        assert!(
            received[60..].iter().all(|&b| b == 0xBB),
            "the owed tail of the over-delivering chunk must be forwarded"
        );
    }

    #[test]
    fn pump_executor_pads_a_short_eof_to_the_declared_length() {
        // A clean EOF inside the shortfall tolerance completes the transfer
        // with NUL padding up to the declared length.
        let mut steps = std::collections::VecDeque::new();
        steps.push_back(Ok(vec![0xAA; 950]));
        steps.push_back(Ok(Vec::new())); // clean EOF, 50 short of 1000 (5%)

        let (tx, mut rx) = channel(100);
        std::thread::spawn(move || {
            pump_ffmpeg_stdout(
                ScriptedSource { steps },
                spawned_child(),
                tx,
                1000,
                Duration::ZERO,
            );
        });

        let mut received: Vec<u8> = Vec::new();
        while let Some(item) = rx.blocking_recv() {
            match item {
                Ok(bytes) => received.extend_from_slice(&bytes),
                Err(e) => panic!("a small undershoot must be padded, not failed: {e}"),
            }
        }

        assert_eq!(
            received.len(),
            1000,
            "padding must complete the declared length"
        );
        assert!(received[..950].iter().all(|&b| b == 0xAA));
        assert!(
            received[950..].iter().all(|&b| b == 0),
            "the shortfall must be padded with NUL bytes"
        );
    }

    #[test]
    fn stderr_eof_is_not_an_error() {
        // ffmpeg runs with `-loglevel error`: a closed stderr with nothing
        // written is the normal end of output, not a problem.
        assert!(matches!(
            stderr_read_decision(Ok(String::new())),
            StderrDecision::Closed
        ));
    }

    #[test]
    fn stderr_error_line_fails_the_stream() {
        // Any line ffmpeg writes to stderr is a genuine error and fails the
        // stream — there is no severity ranking and no line is ignored.
        let decision = stderr_read_decision(Ok("Invalid data found when processing input\n".into()));
        match decision {
            StderrDecision::Fail(e) => assert!(
                e.to_string().contains("Invalid data found"),
                "the surfaced error must be the ffmpeg line itself, got: {e}"
            ),
            other => panic!("an ffmpeg error line must fail the stream, got {other:?}"),
        }
    }

    #[test]
    fn stderr_read_failure_fails_and_stops() {
        // Failing to read stderr at all is not an ffmpeg error line, but it
        // is still a failure — and there is nothing left to drain.
        let read_failure = std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "synthetic stderr fault",
        );
        assert!(matches!(
            stderr_read_decision(Err(read_failure)),
            StderrDecision::ReadFailed(_)
        ));
    }

    #[test]
    fn fatal_read_error_fails_the_transfer() {
        // Any read error that is not a transient interruption — a broken
        // pipe, an I/O fault on ffmpeg's stdout — ends the transfer with
        // that error; there is no retry for it.
        let state = PumpState::start(1000);
        let broken_pipe = std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "synthetic stdout fault",
        );

        let (decision, _) = next_pump_decision(PumpObservation::Read(Err(broken_pipe)), state);

        assert!(
            matches!(decision, PumpDecision::Fail(_)),
            "a fatal read error must fail the transfer, got {decision:?}"
        );
    }

    #[test]
    fn client_disconnect_decides_kill() {
        // The channel rejecting a forwarded chunk means the podcatcher is
        // gone: stop the transfer without sending anything further.
        let state = PumpState::start(1000);

        let (decision, state) = next_pump_decision(PumpObservation::ClientGone, state);

        assert!(
            matches!(decision, PumpDecision::Kill),
            "expected kill on a client disconnect, got {decision:?}"
        );
        assert_eq!(state, PumpState::start(1000));
    }

    #[test]
    fn interrupted_read_below_the_budget_retries_and_stacks_the_streak() {
        let state = PumpState {
            expected_bytes_count: 1000,
            sent_bytes_count: 400,
            interrupt_streak: 3,
        };

        let (decision, state) = next_pump_decision(PumpObservation::Read(Err(interrupted())), state);

        assert!(
            matches!(decision, PumpDecision::Retry),
            "expected a retry for a transient interruption, got {decision:?}"
        );
        assert_eq!(state.interrupt_streak, 4);
        assert_eq!(state.sent_bytes_count, 400);
    }

    #[test]
    fn eof_beyond_shortfall_ratio_fails_the_transfer() {
        // 900 short of 1000 is 90% — far beyond the tolerance: the upstream
        // ended prematurely, and padding would dress the corpse up as a
        // full-length transfer.
        let state = PumpState {
            expected_bytes_count: 1000,
            sent_bytes_count: 100,
            interrupt_streak: 0,
        };

        let (decision, _) = next_pump_decision(PumpObservation::Read(Ok(0)), state);

        match decision {
            PumpDecision::Fail(e) => assert!(
                e.to_string().contains("got 100 of 1000"),
                "the failure must carry the shortfall for diagnosis, got: {e}"
            ),
            other => panic!("expected a transfer failure, got {other:?}"),
        }
    }

    #[test]
    fn eof_within_shortfall_ratio_pads_to_the_declared_length() {
        // 50 short of 1000 is 5% — inside the shortfall tolerance: a finished,
        // slightly-short stream, completed by padding rather than failed.
        let state = PumpState {
            expected_bytes_count: 1000,
            sent_bytes_count: 950,
            interrupt_streak: 0,
        };

        let (decision, state) = next_pump_decision(PumpObservation::Read(Ok(0)), state);

        assert!(
            matches!(decision, PumpDecision::Pad { padding: 50 }),
            "expected padding of the 50-byte shortfall, got {decision:?}"
        );
        assert_eq!(state.sent_bytes_count, 950);
    }

    #[test]
    fn over_delivery_truncates_to_the_declared_length() {
        let state = PumpState {
            expected_bytes_count: 1000,
            sent_bytes_count: 800,
            interrupt_streak: 0,
        };

        let (decision, state) = next_pump_decision(PumpObservation::Read(Ok(400)), state);

        assert!(
            matches!(decision, PumpDecision::Truncate { remaining: 200 }),
            "expected truncation to the 200 still-declared bytes, got {decision:?}"
        );
        // The over-delivering read is not counted: the declared length is the
        // transfer's total.
        assert_eq!(state.sent_bytes_count, 800);
    }

    #[test]
    fn read_within_declared_length_continues_and_counts() {
        let state = PumpState::start(1000);

        let (decision, state) = next_pump_decision(PumpObservation::Read(Ok(400)), state);

        assert!(
            matches!(decision, PumpDecision::Continue { chunk_len: 400 }),
            "expected continue with the read chunk, got {decision:?}"
        );
        assert_eq!(state.sent_bytes_count, 400);
        assert_eq!(state.interrupt_streak, 0);
    }
}
