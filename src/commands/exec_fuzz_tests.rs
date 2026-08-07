//! TIER 0 protocol-interleaving fuzz: client side of the exec ACK/GO handshake,
//! plus the exactly-once execution property across every cut point.
//!
//! A scripted fake "agent" accepts one connection per script entry on a real
//! Unix socket (speaking Firecracker's `CONNECT <port>` preamble, exactly what
//! `connect_and_start_exec` expects) and kills the connection at an enumerated
//! protocol stage. The tests drive the REAL client — `connect_and_start_exec` +
//! `read_exec_responses` — and assert:
//!
//! * pre-ACK deaths are classified resend-safe (the client reconnects and
//!   resends, bounded by MAX_ACK_ATTEMPTS);
//! * deterministic rejections (pre-ACK Error line, garbage-instead-of-ACK)
//!   never retry;
//! * post-ACK deaths are loud errors, never retries;
//! * the exactly-once property: counting GO consumptions as "executions",
//!   every enumerated interleaving yields count ≤ 1, and count == 1 whenever
//!   the client reports success.
//!
//! Determinism: every cut is an event — the server closes or shuts down the
//! socket — never a sleep. The one timing-coupled case (`SilentAfterRequest`)
//! deliberately exercises the real 3s ACK bound with the two offsets that
//! cannot be timing-sensitive: silence forever vs. immediate ACK.

use super::{
    connect_and_start_exec, read_exec_responses, EpochGuardedReader, ExecRequest, ExecResponse,
};
use anyhow::Result;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the fake agent does with one accepted connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Conn {
    /// Respond `OK` to CONNECT, then die before reading the request.
    DieBeforeRequestRead,
    /// Read the request, then die without writing any ACK bytes.
    DieAfterRequest,
    /// Read the request, write a partial ACK ("AC", no newline), die.
    DiePartialAck,
    /// Read the request, write a non-ACK garbage line, die.
    GarbageAck,
    /// Read the request, write a pre-ACK Error line (deterministic rejection), die.
    ErrorLine,
    /// Read the request, shut down the read half, ACK, die: the client's GO
    /// write deterministically fails (the receive side was down before ACK
    /// was even sent) — the post-ACK loud-error path.
    AckThenReadShutdown,
    /// Read the request, ACK, die before GO. Whether the client's GO write or
    /// its response read observes the death is scheduling-dependent, but both
    /// paths must fail loudly without resending — asserted at that level.
    DieAfterAck,
    /// Read the request, ACK, consume GO (EXECUTION), die before any response.
    DieAfterGo,
    /// Full happy path: ACK, consume GO (EXECUTION), respond Exit(0).
    Complete,
    /// Read the request, then stay silent until the client gives up (real 3s
    /// ACK timeout) and drops the connection.
    SilentAfterRequest,
}

/// One connection's log entry: the request line the server read (None if it
/// died before reading one).
type ConnLog = Option<String>;

struct FakeAgent {
    sock: PathBuf,
    _dir: tempfile::TempDir,
    log: Arc<Mutex<Vec<ConnLog>>>,
    /// Number of GO lines consumed — the stand-in for "the command executed".
    executions: Arc<AtomicUsize>,
    handle: std::thread::JoinHandle<()>,
}

impl FakeAgent {
    fn start(script: &[Conn]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("x.sock");
        let listener = UnixListener::bind(&sock).expect("bind fake agent socket");
        let log: Arc<Mutex<Vec<ConnLog>>> = Arc::new(Mutex::new(Vec::new()));
        let executions = Arc::new(AtomicUsize::new(0));

        let script: Vec<Conn> = script.to_vec();
        let log2 = log.clone();
        let executions2 = executions.clone();
        let handle = std::thread::spawn(move || {
            for behavior in script {
                let (stream, _) = listener.accept().expect("accept");
                serve_one(stream, behavior, &log2, &executions2);
            }
        });

        FakeAgent {
            sock,
            _dir: dir,
            log,
            executions,
            handle,
        }
    }

    /// Assert the client made exactly `expected` connections, then reap the
    /// server thread (it has exited its accept loop once the script is spent).
    fn finish(self, expected_conns: usize) -> (Vec<ConnLog>, usize) {
        let log = self.log.lock().unwrap().clone();
        assert_eq!(
            log.len(),
            expected_conns,
            "connection count mismatch: {:?}",
            log
        );
        self.handle.join().expect("fake agent thread panicked");
        (log, self.executions.load(Ordering::SeqCst))
    }
}

fn serve_one(
    mut stream: UnixStream,
    behavior: Conn,
    log: &Mutex<Vec<ConnLog>>,
    executions: &AtomicUsize,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

    // Firecracker vsock preamble: CONNECT <port> → OK <port>.
    let mut connect = String::new();
    reader.read_line(&mut connect).expect("read CONNECT");
    assert!(
        connect.starts_with("CONNECT "),
        "expected CONNECT preamble, got {:?}",
        connect
    );
    stream.write_all(b"OK 4998\n").expect("write OK");

    if behavior == Conn::DieBeforeRequestRead {
        log.lock().unwrap().push(None);
        return; // drop closes the socket
    }

    let mut request = String::new();
    reader.read_line(&mut request).expect("read request");
    log.lock().unwrap().push(Some(request.trim().to_string()));

    match behavior {
        Conn::DieBeforeRequestRead => unreachable!(),
        Conn::DieAfterRequest => {}
        Conn::DiePartialAck => {
            stream.write_all(b"AC").expect("write partial ACK");
        }
        Conn::GarbageAck => {
            stream.write_all(b"NOT-THE-ACK\n").expect("write garbage");
        }
        Conn::ErrorLine => {
            let line = serde_json::to_string(&ExecResponse::Error("boom".to_string())).unwrap();
            stream
                .write_all(format!("{}\n", line).as_bytes())
                .expect("write Error line");
        }
        Conn::AckThenReadShutdown => {
            // Shut the receive side BEFORE sending ACK: by the time the client
            // sees ACK, its GO write can only fail (EPIPE) — no race.
            stream.shutdown(Shutdown::Read).expect("shutdown read");
            let _ = stream.write_all(format!("{}\n", exec_proto::HANDSHAKE_ACK).as_bytes());
        }
        Conn::DieAfterAck => {
            stream
                .write_all(format!("{}\n", exec_proto::HANDSHAKE_ACK).as_bytes())
                .expect("write ACK");
        }
        Conn::DieAfterGo | Conn::Complete => {
            stream
                .write_all(format!("{}\n", exec_proto::HANDSHAKE_ACK).as_bytes())
                .expect("write ACK");
            let mut go = String::new();
            reader.read_line(&mut go).expect("read GO");
            assert_eq!(go.trim_end_matches('\n'), exec_proto::HANDSHAKE_GO);
            // GO consumed — this is the execution point.
            executions.fetch_add(1, Ordering::SeqCst);
            if behavior == Conn::Complete {
                let line = serde_json::to_string(&ExecResponse::Exit(0)).unwrap();
                stream
                    .write_all(format!("{}\n", line).as_bytes())
                    .expect("write Exit");
            }
        }
        Conn::SilentAfterRequest => {
            // Say nothing; block until the client times out (3s ACK bound) and
            // drops the connection, which lands here as EOF.
            let mut rest = String::new();
            let _ = reader.read_line(&mut rest);
        }
    }
}

fn test_request() -> ExecRequest {
    ExecRequest {
        command: vec!["true".to_string()],
        in_container: false,
        interactive: false,
        tty: false,
    }
}

/// Run the real client end-to-end: handshake, then read responses to the exit
/// code. This is exactly what `run_exec_in_vm` does inside spawn_blocking —
/// including the post-GO `EpochGuardedReader`. The vm_id names no state file,
/// so the snapshot-orphan guard captures a `None` baseline and never fires
/// (the documented behavior for VMs without persisted state); these tests cut
/// connections at the socket level, which the guard is not involved in.
fn run_client(sock: &Path) -> Result<i32> {
    let (stream, guard) = connect_and_start_exec(sock, &test_request(), "tier0-fuzz-no-such-vm")?;
    let reader = BufReader::new(EpochGuardedReader::new(stream, guard));
    read_exec_responses(reader, |_| {}, |_| {})
}

/// The request line every connection must carry (resends must be byte-identical).
fn expected_request_line() -> String {
    serde_json::to_string(&test_request()).unwrap()
}

#[test]
fn fuzz_client_happy_path_immediate_ack() {
    let agent = FakeAgent::start(&[Conn::Complete]);
    let code = run_client(&agent.sock).expect("exec should succeed");
    assert_eq!(code, 0);
    let (log, execs) = agent.finish(1);
    assert_eq!(log[0].as_deref(), Some(expected_request_line().as_str()));
    assert_eq!(execs, 1);
}

/// Server dies before even reading the request: NotAcked → resend-safe → the
/// second connection succeeds. Exactly one execution.
#[test]
fn fuzz_client_resends_after_death_before_request_read() {
    let agent = FakeAgent::start(&[Conn::DieBeforeRequestRead, Conn::Complete]);
    let code = run_client(&agent.sock).expect("resend should succeed");
    assert_eq!(code, 0);
    let (log, execs) = agent.finish(2);
    assert_eq!(log[0], None);
    assert_eq!(log[1].as_deref(), Some(expected_request_line().as_str()));
    assert_eq!(execs, 1, "the dead first connection must not execute");
}

/// Server reads the request then dies with no ACK bytes: NotAcked → the resend
/// carries the byte-identical request and succeeds. Exactly one execution.
#[test]
fn fuzz_client_resends_after_death_after_request() {
    let agent = FakeAgent::start(&[Conn::DieAfterRequest, Conn::Complete]);
    let code = run_client(&agent.sock).expect("resend should succeed");
    assert_eq!(code, 0);
    let (log, execs) = agent.finish(2);
    let expected = expected_request_line();
    assert_eq!(log[0].as_deref(), Some(expected.as_str()));
    assert_eq!(log[1].as_deref(), Some(expected.as_str()));
    assert_eq!(execs, 1);
}

/// A partial ACK ("AC" then EOF) is NOT an ACK: still resend-safe.
#[test]
fn fuzz_client_resends_after_partial_ack() {
    let agent = FakeAgent::start(&[Conn::DiePartialAck, Conn::Complete]);
    let code = run_client(&agent.sock).expect("resend should succeed");
    assert_eq!(code, 0);
    let (_, execs) = agent.finish(2);
    assert_eq!(execs, 1);
}

/// Every attempt dies pre-ACK: the client must give up loudly after exactly
/// MAX_ACK_ATTEMPTS (5) connections — bounded, no hang, zero executions.
#[test]
fn fuzz_client_notacked_retries_are_bounded() {
    let agent = FakeAgent::start(&[Conn::DieAfterRequest; 5]);
    let err = run_client(&agent.sock).expect_err("must give up after bounded retries");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("never acknowledged after 5 attempts"),
        "unexpected error: {msg}"
    );
    let (_, execs) = agent.finish(5);
    assert_eq!(execs, 0);
}

/// A pre-ACK Error line is a deterministic rejection: exactly one line is
/// consumed, the agent's message is surfaced, and the client never resends.
#[test]
fn fuzz_client_rejected_error_line_never_resends() {
    let agent = FakeAgent::start(&[Conn::ErrorLine]);
    let err = run_client(&agent.sock).expect_err("rejection must fail");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("fc-agent rejected the exec request: boom"),
        "unexpected error: {msg}"
    );
    let (_, execs) = agent.finish(1);
    assert_eq!(execs, 0);
}

/// Garbage instead of ACK is a protocol violation → Rejected, never resent.
#[test]
fn fuzz_client_garbage_ack_never_resends() {
    let agent = FakeAgent::start(&[Conn::GarbageAck]);
    let err = run_client(&agent.sock).expect_err("protocol violation must fail");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("protocol violation"),
        "unexpected error: {msg}"
    );
    let (_, execs) = agent.finish(1);
    assert_eq!(execs, 0);
}

/// The GO write fails after ACK was received: a loud post-GO error naming the
/// double-execution hazard — and provably no retry (one connection).
#[test]
fn fuzz_client_go_write_failure_is_loud_and_never_resends() {
    let agent = FakeAgent::start(&[Conn::AckThenReadShutdown]);
    let err = run_client(&agent.sock).expect_err("GO write failure must fail loudly");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("Not resending") && msg.contains("run twice"),
        "unexpected error: {msg}"
    );
    let (_, execs) = agent.finish(1);
    assert_eq!(execs, 0, "the agent never consumed GO");
}

/// Connection dies after GO was consumed (execution started) but before any
/// response: the outcome is unknown → error, not success, and no retry.
#[test]
fn fuzz_client_post_go_death_is_error_not_retry() {
    let agent = FakeAgent::start(&[Conn::DieAfterGo]);
    let err = run_client(&agent.sock).expect_err("unknown outcome must not be success");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("before an exit status"),
        "unexpected error: {msg}"
    );
    let (_, execs) = agent.finish(1);
    assert_eq!(execs, 1, "execution started exactly once, never re-sent");
}

/// Silence instead of ACK: the real 3s ACK bound fires (this is the one
/// deliberately timing-coupled case — silence-forever cannot race the bound),
/// classified resend-safe, and the follow-up connection succeeds.
#[test]
fn fuzz_client_ack_timeout_is_resend_safe() {
    let agent = FakeAgent::start(&[Conn::SilentAfterRequest, Conn::Complete]);
    let start = Instant::now();
    let code = run_client(&agent.sock).expect("resend after ACK timeout should succeed");
    let elapsed = start.elapsed();
    assert_eq!(code, 0);
    assert!(
        elapsed >= Duration::from_secs(3),
        "the 3s ACK bound must actually be waited out, took {:?}",
        elapsed
    );
    let (_, execs) = agent.finish(2);
    assert_eq!(execs, 1);
}

/// EXACTLY-ONCE PROPERTY, enumerated: for every cut point in the handshake,
/// executions ≤ 1 per logical request, and == 1 whenever the client reported
/// success. This pins the no-double-execution invariant as a property over the
/// whole interleaving space, not as individual examples.
#[test]
fn fuzz_exactly_once_across_all_cut_points() {
    struct Case {
        name: &'static str,
        script: &'static [Conn],
        /// Some(expected executions) when the client outcome is deterministic;
        /// the property assertions below hold in every case regardless.
        expect_success: bool,
        expect_execs: usize,
    }
    let cases = [
        Case {
            name: "no cut",
            script: &[Conn::Complete],
            expect_success: true,
            expect_execs: 1,
        },
        Case {
            name: "cut before request read",
            script: &[Conn::DieBeforeRequestRead, Conn::Complete],
            expect_success: true,
            expect_execs: 1,
        },
        Case {
            name: "cut after request, before ACK",
            script: &[Conn::DieAfterRequest, Conn::Complete],
            expect_success: true,
            expect_execs: 1,
        },
        Case {
            name: "cut mid-ACK",
            script: &[Conn::DiePartialAck, Conn::Complete],
            expect_success: true,
            expect_execs: 1,
        },
        Case {
            name: "cut after ACK, before GO (GO write fails)",
            script: &[Conn::AckThenReadShutdown],
            expect_success: false,
            expect_execs: 0,
        },
        Case {
            name: "cut after ACK, before GO (close)",
            script: &[Conn::DieAfterAck],
            expect_success: false,
            expect_execs: 0,
        },
        Case {
            name: "cut after GO, before response",
            script: &[Conn::DieAfterGo],
            expect_success: false,
            expect_execs: 1,
        },
        Case {
            name: "cut at every attempt",
            script: &[Conn::DieAfterRequest; 5],
            expect_success: false,
            expect_execs: 0,
        },
    ];

    for case in &cases {
        let agent = FakeAgent::start(case.script);
        let result = run_client(&agent.sock);
        let (_, execs) = agent.finish(case.script.len());

        // The invariant proper: never more than one execution, and success
        // implies exactly one.
        assert!(
            execs <= 1,
            "[{}] executed {} times — double execution!",
            case.name,
            execs
        );
        if result.is_ok() {
            assert_eq!(
                execs, 1,
                "[{}] client reported success but {} executions",
                case.name, execs
            );
        }

        // Per-cut determinism: outcome and count match the enumeration.
        assert_eq!(
            result.is_ok(),
            case.expect_success,
            "[{}] outcome mismatch: {:?}",
            case.name,
            result.err().map(|e| format!("{:#}", e))
        );
        assert_eq!(execs, case.expect_execs, "[{}] execution count", case.name);
    }
}
