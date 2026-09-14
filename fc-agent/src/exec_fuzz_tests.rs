//! TIER 0 protocol-interleaving fuzz: agent side of the exec ACK/GO handshake.
//!
//! Systematically enumerates peer death at EVERY protocol stage of
//! `read_request_and_handshake` over a socketpair:
//!
//! * before any bytes
//! * mid-request (first byte, mid-JSON, just before the newline)
//! * after the full request, before the ACK could be read (both shapes: peer
//!   EOF, and peer read-half shutdown so the ACK write itself fails)
//! * after reading ACK but before GO
//! * mid-GO
//! * after GO
//!
//! plus garbage-instead-of-GO, an oversized GO line, and an oversized request
//! (which must produce the pre-ACK Error line).
//!
//! For each cut the tests assert the three handshake invariants:
//! 1. the function returns within its deadline bound (thread reclaimed);
//! 2. every pre-GO cut returns None — nothing may execute;
//! 3. Some is returned ONLY when a full GO line was consumed.
//!
//! Determinism: every cut is an event (socket shutdown/close observed as EOF or
//! EPIPE), never a sleep. The generous 5s deadline is never actually waited on
//! in the EOF paths — the elapsed assertions prove that.

use super::read_request_and_handshake;
use crate::types::ExecRequest;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// A canonical valid request line (raw bytes, newline-terminated).
const REQUEST: &[u8] = b"{\"command\":[\"true\"],\"in_container\":false}\n";

/// Cuts that consume no deadline (EOF/EPIPE events) must return well before
/// the 5s handshake deadline; 2s is generous slack for a loaded CI host.
const EOF_CUT_BOUND: Duration = Duration::from_secs(2);

/// A connected pair: the server end as the OwnedFd the handshake consumes, the
/// client end as the peer stream the test drives.
fn socketpair() -> (OwnedFd, UnixStream) {
    let (server, client) = UnixStream::pair().expect("socketpair failed");
    (OwnedFd::from(server), client)
}

/// Read everything from the client end until EOF (terminates because the
/// handshake function closes the server fd on every path we drive it down). A
/// reset (the agent closed with peer bytes still unread) ends the read the same
/// way; bytes already received are kept.
fn read_to_eof(client: &mut UnixStream) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = client.read_to_end(&mut out);
    out
}

struct CutOutcome {
    result: Option<ExecRequest>,
    /// Every byte the agent wrote to the peer before closing.
    client_saw: Vec<u8>,
    elapsed: Duration,
}

/// Pre-buffer `prefix` as the peer's entire lifetime output, signal peer death
/// (SHUT_WR → the server sees clean EOF after the prefix), then run the
/// handshake. Fully deterministic: no threads, no sleeps.
fn run_prebuffered_cut(prefix: &[u8]) -> CutOutcome {
    let (server, mut client) = socketpair();
    client.write_all(prefix).expect("write to socketpair");
    client
        .shutdown(Shutdown::Write)
        .expect("shutdown write half");

    let start = Instant::now();
    let result =
        read_request_and_handshake(server, Duration::from_secs(5)).map(|(request, _conn)| request);
    let elapsed = start.elapsed();

    let client_saw = read_to_eof(&mut client);
    CutOutcome {
        result,
        client_saw,
        elapsed,
    }
}

fn ack_line() -> Vec<u8> {
    format!("{}\n", exec_proto::HANDSHAKE_ACK).into_bytes()
}

/// Peer dies before any bytes and at several byte offsets inside the request
/// line (first byte, mid-JSON, just before the newline): the handshake must
/// return None promptly and must never write an ACK — the request was never
/// fully consumed, so the client is allowed to resend it.
#[test]
fn fuzz_peer_death_at_every_request_offset() {
    let offsets = [0, 1, REQUEST.len() / 2, REQUEST.len() - 1];
    for &off in &offsets {
        let outcome = run_prebuffered_cut(&REQUEST[..off]);
        assert!(
            outcome.result.is_none(),
            "cut at request offset {} must not execute",
            off
        );
        assert!(
            outcome.client_saw.is_empty(),
            "cut at request offset {}: no ACK may be written for a partial request, saw {:?}",
            off,
            String::from_utf8_lossy(&outcome.client_saw)
        );
        assert!(
            outcome.elapsed < EOF_CUT_BOUND,
            "cut at request offset {}: EOF must return promptly, took {:?}",
            off,
            outcome.elapsed
        );
    }
}

/// Peer dies right after the full request (EOF before it could read ACK): the
/// agent ACKs the consumed request, then the GO read hits EOF — no execution,
/// and the peer-visible bytes are exactly one ACK line.
#[test]
fn fuzz_peer_death_after_full_request() {
    let outcome = run_prebuffered_cut(REQUEST);
    assert!(outcome.result.is_none(), "no GO consumed → no execution");
    assert_eq!(
        outcome.client_saw,
        ack_line(),
        "peer must see exactly ACK then EOF"
    );
    assert!(
        outcome.elapsed < EOF_CUT_BOUND,
        "EOF after request must return promptly, took {:?}",
        outcome.elapsed
    );
}

/// Peer shuts down its READ half after sending the request: the ACK write
/// itself fails (EPIPE). The agent must return None promptly instead of
/// proceeding to the GO phase.
#[test]
fn fuzz_peer_read_shutdown_fails_ack_write() {
    let (server, mut client) = socketpair();
    client.write_all(REQUEST).expect("write to socketpair");
    // Peer will never read: the ACK write must fail with EPIPE. The write half
    // stays open, so a buggy path that survived the failed ACK would then park
    // on the GO read for the full 5s deadline — the elapsed bound catches it.
    client.shutdown(Shutdown::Read).expect("shutdown read half");

    let start = Instant::now();
    let result = read_request_and_handshake(server, Duration::from_secs(5));
    let elapsed = start.elapsed();

    assert!(result.is_none(), "failed ACK write must not execute");
    assert!(
        elapsed < EOF_CUT_BOUND,
        "EPIPE on ACK must return promptly, took {:?}",
        elapsed
    );
    drop(client);
}

/// Peer reads the ACK (proving the ACK write completed) and then dies before
/// sending GO: no execution. This is the client-crash-between-ACK-and-GO
/// interleaving, driven by a real reader thread rather than pre-buffered bytes.
#[test]
fn fuzz_peer_death_after_reading_ack_before_go() {
    let (server, mut client) = socketpair();
    client.write_all(REQUEST).expect("write to socketpair");

    let reader = std::thread::spawn(move || {
        // Read exactly one line (the ACK), then die (SHUT_WR → EOF at the GO read).
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            client
                .read_exact(&mut byte)
                .expect("agent closed before writing full ACK");
            seen.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        client
            .shutdown(Shutdown::Write)
            .expect("shutdown write half");
        seen
    });

    let start = Instant::now();
    let result = read_request_and_handshake(server, Duration::from_secs(5));
    let elapsed = start.elapsed();

    assert!(
        result.is_none(),
        "death after ACK, before GO → no execution"
    );
    assert!(
        elapsed < EOF_CUT_BOUND,
        "EOF at GO must return promptly, took {:?}",
        elapsed
    );

    let seen = reader.join().unwrap();
    assert_eq!(seen, ack_line(), "peer must have read exactly the ACK line");
}

/// Peer dies mid-GO (one byte of "GO" then EOF): a partial GO line must never
/// authorize execution.
#[test]
fn fuzz_peer_death_mid_go() {
    let mut prefix = REQUEST.to_vec();
    prefix.push(b'G');
    let outcome = run_prebuffered_cut(&prefix);
    assert!(outcome.result.is_none(), "partial GO must not execute");
    assert_eq!(outcome.client_saw, ack_line());
    assert!(
        outcome.elapsed < EOF_CUT_BOUND,
        "EOF mid-GO must return promptly, took {:?}",
        outcome.elapsed
    );
}

/// Garbage instead of GO: a complete line that is not GO must never authorize
/// execution, and the agent writes nothing after the ACK.
#[test]
fn fuzz_garbage_instead_of_go() {
    for garbage in [&b"NO\n"[..], &b"GONE\n"[..], &b"go\n"[..], &b"\n"[..]] {
        let mut prefix = REQUEST.to_vec();
        prefix.extend_from_slice(garbage);
        let outcome = run_prebuffered_cut(&prefix);
        assert!(
            outcome.result.is_none(),
            "garbage {:?} instead of GO must not execute",
            String::from_utf8_lossy(garbage)
        );
        assert_eq!(
            outcome.client_saw,
            ack_line(),
            "nothing may be written after ACK for garbage {:?}",
            String::from_utf8_lossy(garbage)
        );
        assert!(
            outcome.elapsed < EOF_CUT_BOUND,
            "garbage GO must resolve promptly, took {:?}",
            outcome.elapsed
        );
    }
}

/// A GO line exceeding MAX_GO_LINE_LENGTH (16 bytes) is a protocol violation:
/// no execution.
#[test]
fn fuzz_oversized_go_line() {
    let mut prefix = REQUEST.to_vec();
    prefix.extend_from_slice(b"XXXXXXXXXXXXXXXXXXXX\n"); // 20 X's > 16-byte cap
    let outcome = run_prebuffered_cut(&prefix);
    assert!(outcome.result.is_none(), "oversized GO must not execute");
    assert_eq!(outcome.client_saw, ack_line());
}

/// Full GO consumed, then peer death: the ONLY cut point where Some is
/// returned — execution is authorized exactly when a complete GO line was
/// consumed, regardless of what the peer does afterwards.
#[test]
fn fuzz_full_go_then_peer_death_executes_exactly_once() {
    let mut prefix = REQUEST.to_vec();
    prefix.extend_from_slice(format!("{}\n", exec_proto::HANDSHAKE_GO).as_bytes());
    let outcome = run_prebuffered_cut(&prefix);
    let request = outcome
        .result
        .expect("full GO must authorize execution even if the peer dies right after");
    assert_eq!(request.command, vec!["true"]);
    assert_eq!(outcome.client_saw, ack_line());
    assert!(
        outcome.elapsed < EOF_CUT_BOUND,
        "happy path must be fast, took {:?}",
        outcome.elapsed
    );
}

/// Oversized request (> 1 MiB without a newline): the agent must reject it
/// with the pre-ACK Error line — a silent close would read as no-ACK on the
/// client and trigger futile resends of the same oversized request.
#[test]
fn fuzz_oversized_request_gets_pre_ack_error_line() {
    // MAX_EXEC_LINE_LENGTH is 1 MiB; the cap check fires when byte cap+1 arrives.
    const OVERSIZED: usize = 1_048_576 + 1;
    let (server, mut client) = socketpair();

    // The payload exceeds the socketpair buffer, so a writer thread feeds it
    // while the server consumes; it then collects the server's response.
    let writer = std::thread::spawn(move || {
        let data = vec![b'a'; OVERSIZED];
        client.write_all(&data).expect("write to socketpair");
        client
            .shutdown(Shutdown::Write)
            .expect("shutdown write half");
        read_to_eof(&mut client)
    });

    // Generous deadline: the byte-by-byte 1 MiB consume takes a few seconds of
    // syscalls; the point of this case is the Error line, not the latency bound
    // (which the EOF cuts above already pin).
    let result = read_request_and_handshake(server, Duration::from_secs(60));
    assert!(result.is_none(), "oversized request must not execute");

    let seen = String::from_utf8_lossy(&writer.join().unwrap()).into_owned();
    assert!(
        seen.contains("\"error\"") && seen.contains("exceeds"),
        "peer must receive the pre-ACK Error line, saw {:?}",
        seen
    );
    assert!(
        !seen.contains(exec_proto::HANDSHAKE_ACK),
        "an oversized request must never be ACKed, saw {:?}",
        seen
    );
}

/// Invalid JSON and empty-command requests are rejected with a pre-ACK Error
/// line (deterministic rejection, not a silent close that would look resend-safe).
#[test]
fn fuzz_invalid_requests_get_pre_ack_error_line() {
    for (bad, why) in [
        (&b"not json\n"[..], "invalid JSON"),
        (
            &b"{\"command\":[],\"in_container\":false}\n"[..],
            "empty command",
        ),
    ] {
        let outcome = run_prebuffered_cut(bad);
        assert!(outcome.result.is_none(), "{} must not execute", why);
        let seen = String::from_utf8_lossy(&outcome.client_saw).into_owned();
        assert!(
            seen.contains("\"error\""),
            "{} must produce a pre-ACK Error line, saw {:?}",
            why,
            seen
        );
        assert!(
            !seen.contains(exec_proto::HANDSHAKE_ACK),
            "{} must never be ACKed, saw {:?}",
            why,
            seen
        );
    }
}
