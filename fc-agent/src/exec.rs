use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use exec_proto::ExecRequest;
use tokio::sync::Notify;

use crate::vsock;

/// Overall deadline for the pre-execution handshake (request line, ACK write,
/// GO line). A connection orphaned by a snapshot pause (the host's bytes were
/// lost in the vsock transport reset) stalls one of these phases; the deadline
/// bounds it — the connection is closed and its blocking thread reclaimed,
/// never leaked, and nothing executes. Generous vs. the sub-millisecond happy
/// path, tight vs. the previous forever-block.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the exec server. Sends ready signal when listening.
///
/// The `rebind_signal` + `rebind_needed` handle vsock transport reset after
/// snapshot restore. When Firecracker creates a snapshot and restores, the vsock
/// transport is reset (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET). The listener's AsyncFd
/// epoll registration becomes stale — accept() hangs forever because tokio never
/// delivers readability events for incoming connections. On signal, re-registers
/// the epoll via `VsockListener::re_register()` (extracts fd, re-wraps in new
/// AsyncFd) without closing or rebinding the socket. Falls back to full rebind
/// if re-register fails.
///
/// CRITICAL: We use both a `Notify` (to wake up the select loop) and an `AtomicBool`
/// flag (to persist the rebind request). `tokio::select!` polls all branches
/// concurrently — if both `accept()` and `notified()` return Ready simultaneously,
/// `select!` picks one and drops the other. The `Notified` future consumes the
/// permit during `poll()`, so if `accept()` wins, the notification is permanently
/// lost and `re_register()` never runs. The `AtomicBool` flag survives this race:
/// it's checked at the top of each loop iteration, catching any lost notifications.
pub async fn run_server(
    ready_tx: tokio::sync::oneshot::Sender<()>,
    rebind_signal: Arc<Notify>,
    rebind_needed: Arc<AtomicBool>,
    rebind_done: Arc<AtomicBool>,
    rebind_done_notify: Arc<Notify>,
) {
    eprintln!(
        "[fc-agent] starting exec server on vsock port {}",
        vsock::EXEC_PORT
    );

    let mut listener = match vsock::VsockListener::bind(vsock::EXEC_PORT) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[fc-agent] ERROR: failed to bind exec server: {}", e);
            return;
        }
    };

    eprintln!(
        "[fc-agent] exec server listening on vsock port {}",
        vsock::EXEC_PORT
    );

    tokio::task::yield_now().await;
    let _ = ready_tx.send(());

    loop {
        // Single rebind path: the AtomicBool flag is the source of truth, the Notify
        // below is only a wakeup. When both accept() and notified() are Ready
        // simultaneously, select! picks one and drops the other — if accept() wins,
        // the Notified permit may already be consumed, but the flag persists and is
        // handled here on the next iteration. Performing the rebind only here also
        // guarantees one rebind request produces exactly one rebind_done notification.
        if rebind_needed.swap(false, Ordering::AcqRel) {
            eprintln!(
                "[fc-agent] exec server: vsock transport reset (flag), re-registering listener"
            );
            listener = do_re_register(listener).await;
            rebind_done.store(true, Ordering::Release);
            rebind_done_notify.notify_one();
        }

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok(client_fd) => {
                        // Diagnostic for #617: confirms accept() actually fired after a
                        // restore. If a restored-VM exec hangs and this line is absent
                        // from the serial log while "re-registered" is present, the
                        // re-registered listener is not delivering readiness for new
                        // connections (vs. the hang being downstream in handle_connection).
                        eprintln!(
                            "[fc-agent] exec server: accepted connection on vsock port {}",
                            vsock::EXEC_PORT
                        );
                        tokio::spawn(handle_connection(client_fd));
                    }
                    Err(e) => {
                        eprintln!("[fc-agent] exec server accept error: {}", e);
                        // Persistent errors (e.g. a broken listener fd) would
                        // otherwise spin this select loop with no await point.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = rebind_signal.notified() => {
                // Wake-up only — the top-of-loop flag check performs the re-register.
                // Handling the rebind here as well would double-handle a single request
                // when accept() readiness raced the notification (the flag check wins,
                // then the stored permit fires this arm), producing a second
                // rebind_done notification with no waiter. That stale permit would let
                // a LATER restore proceed before its listener re-register completed.
            }
        }
    }
}

/// Re-register or rebind the vsock listener after transport reset.
///
/// Does not return until a working listener exists. Failures are not fatal: this runs
/// in a spawned task, so a panic here would only kill the exec-server task while the
/// rest of fc-agent kept running with no exec listener at all. Instead, bind failures
/// are logged loudly (visible on the serial console) and retried with backoff — if the
/// vsock device recovers, the exec server recovers with it.
async fn do_re_register(listener: vsock::VsockListener) -> vsock::VsockListener {
    match listener.re_register() {
        Ok(l) => {
            eprintln!(
                "[fc-agent] exec server: re-registered on vsock port {}",
                vsock::EXEC_PORT
            );
            l
        }
        Err(e) => {
            // re_register consumed the listener; socket is closed.
            eprintln!(
                "[fc-agent] exec server: re-register failed: {}, trying full rebind",
                e
            );
            let mut retries: u32 = 0;
            loop {
                match vsock::VsockListener::bind(vsock::EXEC_PORT) {
                    Ok(l) => {
                        eprintln!(
                            "[fc-agent] exec server: re-bound to vsock port {}",
                            vsock::EXEC_PORT
                        );
                        return l;
                    }
                    Err(e2) => {
                        retries += 1;
                        // Fast retries for the first ~5s (transient EADDRINUSE while
                        // pre-snapshot connections drain), then back off to 1s and log
                        // periodically so a broken vsock device stays visible without
                        // flooding the console.
                        let delay_ms = if retries < 50 { 100 } else { 1000 };
                        if retries <= 50 || retries.is_multiple_of(30) {
                            eprintln!(
                                "[fc-agent] ERROR: exec re-bind failed (attempt {}): {}, exec unavailable, retrying in {}ms",
                                retries, e2, delay_ms
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                }
            }
        }
    }
}

/// Why a line read failed (used for handshake diagnostics).
#[derive(Debug, PartialEq, Eq)]
enum LineReadError {
    /// The overall handshake deadline passed before a full line arrived.
    TimedOut,
    /// The peer closed the connection (EOF).
    Closed,
    /// The line exceeded the length cap.
    TooLong,
    /// read()/poll() failed.
    Failed,
}

/// Read one `\n`-terminated line from `fd` (byte-by-byte, so no bytes past the
/// terminating newline are ever consumed), polling for readability so the read
/// is bounded by `deadline` instead of blocking forever. Does NOT close the fd.
fn read_line_bounded(fd: i32, deadline: Instant, max_len: usize) -> Result<Vec<u8>, LineReadError> {
    let mut line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(LineReadError::TimedOut);
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc == 0 {
            return Err(LineReadError::TimedOut);
        }
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(LineReadError::Failed);
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n < 0 {
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock => continue,
                _ => return Err(LineReadError::Failed),
            }
        }
        if n == 0 {
            return Err(LineReadError::Closed);
        }
        if buf[0] == b'\n' {
            return Ok(line);
        }
        if line.len() >= max_len {
            return Err(LineReadError::TooLong);
        }
        line.push(buf[0]);
    }
}

/// Write `line` + `\n` to `fd`, polling for writability so the write is bounded
/// by `deadline`. Returns false on error/timeout. Does NOT close the fd.
fn write_line_bounded(fd: i32, deadline: Instant, line: &str) -> bool {
    let bytes = format!("{}\n", line);
    let buf = bytes.as_bytes();
    let mut written = 0;
    while written < buf.len() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc == 0 {
            return false;
        }
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        let n = unsafe {
            libc::write(
                fd,
                buf[written..].as_ptr() as *const libc::c_void,
                buf.len() - written,
            )
        };
        if n < 0 {
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock => continue,
                _ => return false,
            }
        }
        if n == 0 {
            return false;
        }
        written += n as usize;
    }
    true
}

/// Read the request line and complete the three-phase handshake
/// (request → ACK → GO), all bounded by `timeout`. See exec_proto::HANDSHAKE_ACK.
///
/// Phase 1 — read the request line under the deadline. A connection orphaned by
/// a snapshot pause (host bytes lost in the vsock transport reset) stalls here;
/// it is closed and its thread reclaimed instead of blocking forever.
///
/// Phase 2 — after consuming the FULL request line (parsed, non-empty command),
/// write an ACK line. Nothing has executed yet: a client that never sees ACK
/// may safely resend its request on a fresh connection.
///
/// Phase 3 — read one GO line under the deadline. ONLY after consuming GO may
/// execution start. If any phase errors or times out, the connection is closed
/// WITHOUT executing — this is what makes the client's resend provably unable
/// to double-execute.
///
/// The request is accumulated as raw bytes and parsed as UTF-8 JSON. The host
/// serializes ExecRequest with serde_json::to_string, which emits non-ASCII
/// characters as raw UTF-8 — decoding byte-by-byte (Latin-1) would silently corrupt
/// multi-byte arguments and paths.
///
/// Returns the request and the still-open connection once GO is consumed. Every
/// other path returns None and closes the connection by dropping it.
fn read_request_and_handshake(conn: OwnedFd, timeout: Duration) -> Option<(ExecRequest, OwnedFd)> {
    const MAX_EXEC_LINE_LENGTH: usize = 1_048_576;
    /// GO is 2 bytes; anything longer is a protocol violation.
    const MAX_GO_LINE_LENGTH: usize = 16;

    // failpoint: hold after accept, before consuming the request line — forces the
    // client's 3s ACK timeout and exercises reconnect+resend against a REAL agent
    // (not a mock). Sync hit(): this fn runs inside spawn_blocking.
    failpoint::hit("exec.post_accept_pre_read");

    let fd = conn.as_raw_fd();
    let deadline = Instant::now() + timeout;

    // Phase 1: request line.
    let line = match read_line_bounded(fd, deadline, MAX_EXEC_LINE_LENGTH) {
        Ok(line) => line,
        Err(reason) => {
            match reason {
                LineReadError::TimedOut => {
                    eprintln!(
                        "[fc-agent] exec handshake: no request line within {:?} \
                         (connection likely orphaned by a snapshot pause); closing without executing",
                        timeout
                    );
                }
                LineReadError::TooLong => {
                    // A pre-ACK Error line makes the client fail deterministically
                    // (Rejected); a silent close would read as no-ACK and trigger
                    // futile resends of the same oversized request.
                    write_line_bounded(
                        fd,
                        deadline,
                        &exec_proto::rejection_line(&format!(
                            "Request line exceeds {} bytes",
                            MAX_EXEC_LINE_LENGTH
                        )),
                    );
                }
                LineReadError::Closed | LineReadError::Failed => {}
            }
            return None;
        }
    };

    let request: ExecRequest = match serde_json::from_slice(&line) {
        Ok(r) => r,
        Err(e) => {
            write_line_bounded(
                fd,
                deadline,
                &exec_proto::rejection_line(&format!("Invalid request: {}", e)),
            );
            return None;
        }
    };

    if request.command.is_empty() {
        write_line_bounded(fd, deadline, &exec_proto::rejection_line("Empty command"));
        return None;
    }

    // Phase 2: ACK — the request is fully consumed and will execute iff GO arrives.
    if !write_line_bounded(fd, deadline, exec_proto::HANDSHAKE_ACK) {
        return None;
    }

    // failpoint: hold after the ACK write and BEFORE go_deadline is computed, so a
    // hold delays the GO read without eating its 2s floor (Instant::now() below is
    // taken after the hold) — makes "GO races a snapshot pause after ACK" testable.
    failpoint::hit("exec.post_ack_pre_go");

    // Phase 3: GO — only after consuming this may execution start. A request
    // that arrived slowly can leave the shared deadline nearly exhausted here;
    // the client sends GO immediately after ACK, so give this read a small
    // fresh floor instead of expiring spuriously (which would surface a loud
    // post-GO error on the client for a command that never ran).
    let go_deadline = deadline.max(Instant::now() + Duration::from_secs(2));
    match read_line_bounded(fd, go_deadline, MAX_GO_LINE_LENGTH) {
        Ok(go) if go == exec_proto::HANDSHAKE_GO.as_bytes() => Some((request, conn)),
        Ok(other) => {
            eprintln!(
                "[fc-agent] exec handshake: expected GO, got {:?}; closing without executing",
                String::from_utf8_lossy(&other)
            );
            None
        }
        Err(reason) => {
            eprintln!(
                "[fc-agent] exec handshake: ACK sent but no GO ({:?}) \
                 (connection likely orphaned by a snapshot pause); closing without executing",
                reason
            );
            None
        }
    }
}

async fn handle_connection(client_fd: OwnedFd) {
    // Read the request line and run the ACK/GO handshake in spawn_blocking
    // (blocking byte-by-byte I/O, bounded by HANDSHAKE_TIMEOUT). The task owns
    // the connection, so a task that panics or is dropped unstarted closes it.
    let parsed = tokio::task::spawn_blocking(move || {
        read_request_and_handshake(client_fd, HANDSHAKE_TIMEOUT)
    })
    .await;

    let (request, conn) = match parsed {
        Ok(Some(handshaken)) => handshaken,
        Ok(None) => return, // closed, timed out, or invalid (conn already dropped)
        Err(_) => return,   // task panicked or was cancelled (conn dropped with it)
    };

    crate::tty::run_session(conn, session_spec(&request)).await;
}

/// Turn a request into the command fc-agent runs for it.
///
/// A container exec runs `podman exec` with the same -i and -t the client
/// asked for, so podman attaches the container process the same way.
fn session_spec(request: &ExecRequest) -> crate::tty::SessionSpec {
    let proxy_settings = crate::system::read_proxy_settings();
    let (argv, env) = if request.in_container {
        let mut argv: Vec<String> = crate::container::podman_cmd_prefix().to_vec();
        argv.extend(["podman".to_string(), "exec".to_string()]);
        if request.interactive {
            argv.push("-i".to_string());
        }
        if request.tty {
            argv.push("-t".to_string());
        }
        if request.detach {
            argv.push("-d".to_string());
        }
        if request.privileged {
            argv.push("--privileged".to_string());
        }
        if let Some(workdir) = &request.workdir {
            argv.extend(["-w".to_string(), workdir.clone()]);
        }
        if let Some(user) = &request.user {
            argv.extend(["-u".to_string(), user.clone()]);
        }
        // The request's entries come last, so they win over the proxy settings.
        let proxy_env = proxy_settings
            .iter()
            .map(|(key, value)| format!("{key}={value}"));
        for entry in proxy_env.chain(request.env.iter().cloned()) {
            argv.extend(["-e".to_string(), entry]);
        }
        argv.push("--latest".to_string());
        argv.extend(request.command.iter().cloned());
        (argv, Vec::new())
    } else {
        let mut env = proxy_settings;
        env.extend(request.env.iter().map(|entry| {
            let (key, value) = entry.split_once('=').unwrap_or((entry, ""));
            (key.to_string(), value.to_string())
        }));
        (request.command.clone(), env)
    };
    // In a container, podman applies the directory, the user and the detach.
    // For a guest command fc-agent does.
    let guest = !request.in_container;
    crate::tty::SessionSpec {
        argv,
        env,
        // A detached command keeps nothing attached here. For a container,
        // `-d -t` went to podman above, which gives the command its own TTY.
        tty: request.tty && !request.detach,
        interactive: request.interactive && !request.detach,
        size: request.tty_size,
        raw_pty: request.in_container,
        workdir: request.workdir.clone().filter(|_| guest),
        user: request.user.clone().filter(|_| guest),
        detach: request.detach && guest,
        kill_on_disconnect: true,
    }
}

// TIER 0 protocol-interleaving fuzz: systematic peer-death enumeration for the
// ACK/GO handshake (kept in its own file — see exec_fuzz_tests.rs).
#[cfg(test)]
#[path = "exec_fuzz_tests.rs"]
mod exec_fuzz_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: &[&str], in_container: bool) -> ExecRequest {
        ExecRequest {
            command: command.iter().map(|arg| arg.to_string()).collect(),
            in_container,
            ..Default::default()
        }
    }

    /// The part of a container exec's argv that follows `podman exec`.
    fn podman_exec_args(spec: &crate::tty::SessionSpec) -> Vec<&str> {
        let exec = spec
            .argv
            .iter()
            .position(|arg| arg == "exec")
            .expect("argv runs podman exec");
        assert_eq!(spec.argv[exec - 1], "podman");
        spec.argv[exec + 1..].iter().map(String::as_str).collect()
    }

    #[test]
    fn a_container_exec_hands_every_flag_to_podman() {
        let mut request = request(&["id", "-u"], true);
        request.interactive = true;
        request.tty = true;
        request.privileged = true;
        request.workdir = Some("/tmp".to_string());
        request.user = Some("nobody:users".to_string());
        request.env = vec!["A=1".to_string(), "B=two words".to_string()];

        let spec = session_spec(&request);
        let args = podman_exec_args(&spec);
        for flag in ["-i", "-t", "--privileged"] {
            assert!(args.contains(&flag), "{flag} missing from {args:?}");
        }
        let pair = |flag: &str, value: &str| args.windows(2).any(|w| w == [flag, value]);
        assert!(pair("-w", "/tmp") && pair("-u", "nobody:users"), "{args:?}");
        assert!(pair("-e", "A=1") && pair("-e", "B=two words"), "{args:?}");
        // The command comes last, after --latest, so its own flags stay its own.
        assert_eq!(&args[args.len() - 3..], ["--latest", "id", "-u"]);
        // podman applies these; fc-agent must not apply them to the podman client.
        assert!(spec.workdir.is_none() && spec.user.is_none() && !spec.detach);
        assert!(spec.tty && spec.interactive && spec.raw_pty);
    }

    #[test]
    fn a_detached_container_exec_keeps_its_tty_flag_but_attaches_nothing() {
        let mut request = request(&["sleep", "30"], true);
        request.detach = true;
        request.tty = true;
        let spec = session_spec(&request);
        let args = podman_exec_args(&spec);
        assert!(args.contains(&"-d") && args.contains(&"-t"), "{args:?}");
        assert!(!spec.tty && !spec.interactive && !spec.detach);
    }

    #[test]
    fn a_guest_command_is_run_as_given_and_fc_agent_applies_the_flags() {
        let mut request = request(&["id", "-u"], false);
        request.workdir = Some("/tmp".to_string());
        request.user = Some("nobody".to_string());
        request.detach = true;
        request.env = vec!["A=1".to_string(), "EQ=a=b".to_string(), "BARE".to_string()];

        let spec = session_spec(&request);
        assert_eq!(spec.argv, ["id", "-u"]);
        assert_eq!(spec.workdir.as_deref(), Some("/tmp"));
        assert_eq!(spec.user.as_deref(), Some("nobody"));
        assert!(spec.detach && !spec.raw_pty);
        let env = |key: &str| {
            spec.env
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(env("A"), Some("1"));
        assert_eq!(env("EQ"), Some("a=b"), "only the first = separates");
        assert_eq!(env("BARE"), Some(""));
    }
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// A connected pair: the server end as the OwnedFd the handshake consumes,
    /// the client end as a stream the test drives.
    fn socketpair() -> (OwnedFd, UnixStream) {
        let (server, client) = UnixStream::pair().expect("socketpair failed");
        (OwnedFd::from(server), client)
    }

    /// Read everything from the client end until EOF. A reset (the agent closed
    /// with client bytes still unread) ends the read the same way; bytes already
    /// received are kept.
    fn read_to_eof(client: &mut UnixStream) -> Vec<u8> {
        let mut out = Vec::new();
        let _ = client.read_to_end(&mut out);
        out
    }

    /// Send `data` over a socketpair (with a pre-buffered GO line so the
    /// handshake can complete) and parse it with read_request_and_handshake.
    fn parse_request(data: &[u8]) -> Option<ExecRequest> {
        let (server, mut client) = socketpair();
        client.write_all(data).expect("write request");
        client
            .write_all(format!("{}\n", exec_proto::HANDSHAKE_GO).as_bytes())
            .expect("write GO");
        read_request_and_handshake(server, Duration::from_secs(5)).map(|(request, _conn)| request)
    }

    #[test]
    fn test_parse_request_preserves_utf8_args() {
        // The host serializes ExecRequest with serde_json::to_string, which emits
        // non-ASCII characters as raw UTF-8 bytes — they must round-trip intact.
        let json = "{\"command\":[\"touch\",\"/data/héllo wörld.txt\"]}\n";
        let request = parse_request(json.as_bytes()).expect("request should parse");
        assert_eq!(request.command, vec!["touch", "/data/héllo wörld.txt"]);
        assert!(!request.in_container);
        assert!(!request.tty);
    }

    #[test]
    fn test_parse_request_ascii() {
        let json = "{\"command\":[\"echo\",\"hello\"],\"in_container\":true}\n";
        let request = parse_request(json.as_bytes()).expect("request should parse");
        assert_eq!(request.command, vec!["echo", "hello"]);
        assert!(request.in_container);
    }

    #[test]
    fn test_parse_request_rejects_invalid_json() {
        assert!(parse_request(b"not json\n").is_none());
    }

    /// Protocol-faithful happy path: the client sends the request, waits for the
    /// ACK line, then sends GO — the server returns the request only after GO.
    #[test]
    fn test_handshake_ack_then_go() {
        let (server, mut client) = socketpair();

        let client = std::thread::spawn(move || {
            client
                .write_all(b"{\"command\":[\"true\"],\"in_container\":false}\n")
                .expect("write request");
            // Wait for the full ACK line before sending GO.
            let mut ack = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                client
                    .read_exact(&mut byte)
                    .expect("server closed before ACK");
                if byte[0] == b'\n' {
                    break;
                }
                ack.push(byte[0]);
            }
            assert_eq!(ack, exec_proto::HANDSHAKE_ACK.as_bytes());
            client
                .write_all(format!("{}\n", exec_proto::HANDSHAKE_GO).as_bytes())
                .expect("write GO");
            client
        });

        let (request, _conn) = read_request_and_handshake(server, Duration::from_secs(5))
            .expect("handshake should complete");
        assert_eq!(request.command, vec!["true"]);
        client.join().unwrap();
    }

    /// No GO after ACK (the snapshot-pause orphan shape): the server must time
    /// out, close the connection, and never hand the request to execution. The
    /// client must observe exactly ACK then EOF — no Exit/Error response, which
    /// is the proof nothing executed.
    #[test]
    fn test_handshake_no_go_times_out_without_executing() {
        let (server, mut client) = socketpair();
        client
            .write_all(b"{\"command\":[\"true\"],\"in_container\":false}\n")
            .expect("write request");

        let start = Instant::now();
        let result = read_request_and_handshake(server, Duration::from_millis(200));
        assert!(result.is_none(), "handshake without GO must not execute");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout must be bounded (thread reclaimed), took {:?}",
            start.elapsed()
        );

        // Server closed the fd; the client sees the ACK it sent, then clean EOF.
        let seen = read_to_eof(&mut client);
        assert_eq!(
            seen,
            format!("{}\n", exec_proto::HANDSHAKE_ACK).into_bytes()
        );
    }

    /// No request line at all (connection accepted, host bytes lost in a vsock
    /// reset): bounded timeout, no ACK, fd closed, thread reclaimed.
    #[test]
    fn test_handshake_request_timeout_is_bounded() {
        let (server, mut client) = socketpair();

        let start = Instant::now();
        let result = read_request_and_handshake(server, Duration::from_millis(200));
        assert!(result.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "orphaned connection must be reclaimed quickly, took {:?}",
            start.elapsed()
        );

        // No ACK was ever written — the request was never consumed.
        let seen = read_to_eof(&mut client);
        assert!(seen.is_empty(), "no bytes expected, got {:?}", seen);
    }

    /// The blocking handshake task owns the connection, so a task that never runs
    /// (spawn_blocking on a runtime that is shutting down drops it unstarted)
    /// still closes the fd instead of leaking it.
    #[test]
    fn handshake_task_dropped_unstarted_closes_connection() {
        use std::future::Future;

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        rt.shutdown_background();
        let _enter = handle.enter();

        let (server, mut client) = socketpair();
        let mut conn = std::pin::pin!(handle_connection(server));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        // One poll reaches spawn_blocking, which drops the task unstarted because
        // the pool is shut down; the join then resolves to a cancellation error.
        let _ = conn.as_mut().poll(&mut cx);

        client.set_nonblocking(true).unwrap();
        let mut byte = [0u8; 1];
        match client.read(&mut byte) {
            Ok(0) => {}
            other => panic!("connection left open after the handshake task was dropped: {other:?}"),
        }
    }

    /// Client vanishes after the request (EOF before GO): close without executing.
    #[test]
    fn test_handshake_client_close_before_go() {
        let (server, mut client) = socketpair();
        client
            .write_all(b"{\"command\":[\"true\"],\"in_container\":false}\n")
            .expect("write request");
        drop(client);

        let result = read_request_and_handshake(server, Duration::from_secs(5));
        assert!(result.is_none(), "EOF before GO must not execute");
    }
}
