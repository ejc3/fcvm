use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Notify};

use crate::types::{ExecRequest, ExecResponse};
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

/// Async write helper — writes a JSON line to the vsock fd using AsyncFd.
async fn write_line_async(conn: &AsyncFd<OwnedFd>, data: &str) {
    let bytes = format!("{}\n", data);
    let buf = bytes.as_bytes();
    let mut pos = 0;
    while pos < buf.len() {
        let mut guard = match conn.writable().await {
            Ok(g) => g,
            Err(_) => break,
        };
        match guard.try_io(|inner| {
            let n = unsafe {
                libc::write(
                    inner.as_raw_fd(),
                    buf[pos..].as_ptr().cast(),
                    buf.len() - pos,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else if n == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "write returned 0",
                ))
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(n)) => pos += n,
            Ok(Err(_)) => break,
            Err(_would_block) => continue,
        }
    }
}

/// Blocking write helper — used for error responses before fd is made non-blocking.
fn write_line_to_fd(fd: i32, data: &str) {
    let bytes = format!("{}\n", data);
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
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
/// Returns (ExecRequest, raw_fd) once GO is consumed, or None (fd closed).
fn read_request_and_handshake(fd: i32, timeout: Duration) -> Option<(ExecRequest, i32)> {
    const MAX_EXEC_LINE_LENGTH: usize = 1_048_576;
    /// GO is 2 bytes; anything longer is a protocol violation.
    const MAX_GO_LINE_LENGTH: usize = 16;

    // failpoint: hold after accept, before consuming the request line — forces the
    // client's 3s ACK timeout and exercises reconnect+resend against a REAL agent
    // (not a mock). Sync hit(): this fn runs inside spawn_blocking.
    failpoint::hit("exec.post_accept_pre_read");

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
                    let response = ExecResponse::Error(format!(
                        "Request line exceeds {} bytes",
                        MAX_EXEC_LINE_LENGTH
                    ));
                    write_line_bounded(fd, deadline, &serde_json::to_string(&response).unwrap());
                }
                LineReadError::Closed | LineReadError::Failed => {}
            }
            unsafe { libc::close(fd) };
            return None;
        }
    };

    let request: ExecRequest = match serde_json::from_slice(&line) {
        Ok(r) => r,
        Err(e) => {
            let response = ExecResponse::Error(format!("Invalid request: {}", e));
            write_line_bounded(fd, deadline, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(fd) };
            return None;
        }
    };

    if request.command.is_empty() {
        let response = ExecResponse::Error("Empty command".to_string());
        write_line_bounded(fd, deadline, &serde_json::to_string(&response).unwrap());
        unsafe { libc::close(fd) };
        return None;
    }

    // Phase 2: ACK — the request is fully consumed and will execute iff GO arrives.
    if !write_line_bounded(fd, deadline, exec_proto::HANDSHAKE_ACK) {
        unsafe { libc::close(fd) };
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
        Ok(go) if go == exec_proto::HANDSHAKE_GO.as_bytes() => Some((request, fd)),
        Ok(other) => {
            eprintln!(
                "[fc-agent] exec handshake: expected GO, got {:?}; closing without executing",
                String::from_utf8_lossy(&other)
            );
            unsafe { libc::close(fd) };
            None
        }
        Err(reason) => {
            eprintln!(
                "[fc-agent] exec handshake: ACK sent but no GO ({:?}) \
                 (connection likely orphaned by a snapshot pause); closing without executing",
                reason
            );
            unsafe { libc::close(fd) };
            None
        }
    }
}

async fn handle_connection(client_fd: OwnedFd) {
    // Read the request line and run the ACK/GO handshake in spawn_blocking
    // (blocking byte-by-byte I/O, bounded by HANDSHAKE_TIMEOUT).
    let raw_fd = client_fd.into_raw_fd();
    let parsed =
        tokio::task::spawn_blocking(move || read_request_and_handshake(raw_fd, HANDSHAKE_TIMEOUT))
            .await;

    let (request, raw_fd) = match parsed {
        Ok(Some((req, fd))) => (req, fd),
        Ok(None) => return, // closed, timed out, or invalid (already handled)
        Err(_) => return,   // spawn_blocking panicked
    };

    // TTY path: must be blocking (fork/PTY)
    if request.tty || request.interactive {
        let command = if request.in_container {
            let prefix = crate::container::podman_cmd_prefix();
            let mut cmd: Vec<String> = prefix.to_vec();
            cmd.extend(["podman".to_string(), "exec".to_string()]);
            if request.interactive {
                cmd.push("-i".to_string());
            }
            if request.tty {
                cmd.push("-t".to_string());
            }
            for (key, value) in crate::system::read_proxy_settings() {
                cmd.push("-e".to_string());
                cmd.push(format!("{}={}", key, value));
            }
            cmd.push("--latest".to_string());
            cmd.extend(request.command.iter().cloned());
            cmd
        } else {
            request.command.clone()
        };

        tokio::task::spawn_blocking(move || {
            crate::tty::run_with_pty_fd(raw_fd, &command, request.tty, request.interactive);
        });
    } else {
        // Pipe path: fully async
        handle_pipe_async(raw_fd, &request).await;
    }
}

/// Resolve when the vsock peer (host `fcvm exec`) closes the connection.
///
/// Polls a dup'd, independent `AsyncFd` for readability and peeks one byte: 0 (EOF), or a
/// reset/disconnect error (ECONNRESET/ENOTCONN/EPIPE), means the host is gone. In the
/// non-TTY pipe path the host sends nothing after the handshake's GO line (which was fully
/// consumed before this watcher exists — it only reads responses from here on), so any
/// readable EOF is equivalent to "host gone" — used to kill the guest child (#636).
async fn wait_for_peer_close(watch: &AsyncFd<OwnedFd>) {
    loop {
        let mut guard = match watch.readable().await {
            Ok(g) => g,
            Err(_) => return, // fd error -> treat as closed
        };
        let fd = watch.get_ref().as_raw_fd();
        let mut byte = [0u8; 1];
        let n = unsafe {
            libc::recv(
                fd,
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if n == 0 {
            return; // EOF: peer closed its write half
        }
        if n < 0 {
            // EAGAIN/EWOULDBLOCK (same value on Linux): spurious readiness, keep watching.
            // Any other error (ECONNRESET/ENOTCONN/EPIPE/…) means the peer is gone.
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN) {
                guard.clear_ready();
                continue;
            }
            return;
        }
        // n > 0: the host should send nothing after the handshake (it only reads responses,
        // and never half-closes its write side while waiting — see the host exec client).
        // This branch is therefore defensive: drain one byte and loop WITHOUT clear_ready
        // (calling clear_ready after a successful read is a tokio anti-pattern). The fd
        // stays marked ready, so the next readable() drains again until EAGAIN, which then
        // clears readiness and parks until the eventual close edge.
        let _ = unsafe { libc::recv(fd, byte.as_mut_ptr().cast(), 1, libc::MSG_DONTWAIT) };
    }
}

async fn handle_pipe_async(raw_fd: i32, request: &ExecRequest) {
    let proxy_settings = crate::system::read_proxy_settings();

    let mut cmd = if request.in_container {
        let prefix = crate::container::podman_cmd_prefix();
        let mut cmd = if prefix.is_empty() {
            let mut c = tokio::process::Command::new("podman");
            c.arg("exec");
            c
        } else {
            // Run as target user: env XDG_RUNTIME_DIR=... runuser -u user -- podman exec
            let mut c = tokio::process::Command::new(&prefix[0]);
            c.args(&prefix[1..]);
            c.arg("podman").arg("exec");
            c
        };
        if request.interactive {
            cmd.arg("-i");
        }
        for (key, value) in &proxy_settings {
            cmd.arg("-e").arg(format!("{}={}", key, value));
        }
        cmd.arg("--latest");
        cmd.args(&request.command);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new(&request.command[0]);
        cmd.args(&request.command[1..]);
        for (key, value) in &proxy_settings {
            cmd.env(key, value);
        }
        cmd
    };

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if request.interactive {
        cmd.stdin(std::process::Stdio::piped());
    }
    // Own process group so a host disconnect can kill the whole subtree (the command and
    // anything it spawns), not just the immediate child (#636).
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let response = ExecResponse::Error(format!("Failed to spawn: {}", e));
            write_line_to_fd(raw_fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(raw_fd) };
            return;
        }
    };

    // Capture the child's PID now (used as the PGID for killpg on host disconnect, #636).
    let child_pid = child.id();

    // Spawn succeeded — set non-blocking, take ownership, wrap in AsyncFd
    nix::fcntl::fcntl(
        raw_fd,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .ok();
    // Independent CLOEXEC dup of the connection for read-side peer-close detection. Polling
    // readability on a separate fd avoids holding the write Mutex across readable().await
    // (which would deadlock the stdout/stderr writer tasks). The two OwnedFds own distinct
    // fd numbers, so there is no double-close.
    let peer_watch: Option<AsyncFd<OwnedFd>> =
        match nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(0)) {
            Ok(dup) => match AsyncFd::new(unsafe { OwnedFd::from_raw_fd(dup) }) {
                Ok(afd) => Some(afd),
                // Extremely rare (reactor registration failure). Don't fail the exec, but
                // don't fail silently either: without the watcher, host-disconnect kill is
                // disabled for this exec (degrades to the pre-#636 wait-only behavior).
                Err(e) => {
                    eprintln!(
                        "[fc-agent] WARN: peer-close watcher AsyncFd failed ({e}); \
                         host-disconnect kill disabled for this exec"
                    );
                    None
                }
            },
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARN: peer-close watcher dup failed ({e}); \
                     host-disconnect kill disabled for this exec"
                );
                None
            }
        };
    let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let async_fd = match AsyncFd::new(owned_fd) {
        Ok(fd) => Arc::new(Mutex::new(fd)),
        Err(_) => return, // fd closed by OwnedFd drop
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Spawn async stdout reader
    let conn_stdout = async_fd.clone();
    let stdout_task = stdout.map(|stdout| {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let response = ExecResponse::Stdout(format!("{}\n", line));
                let conn = conn_stdout.lock().await;
                write_line_async(&conn, &serde_json::to_string(&response).unwrap()).await;
            }
        })
    });

    // Spawn async stderr reader
    let conn_stderr = async_fd.clone();
    let stderr_task = stderr.map(|stderr| {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let response = ExecResponse::Stderr(format!("{}\n", line));
                let conn = conn_stderr.lock().await;
                write_line_async(&conn, &serde_json::to_string(&response).unwrap()).await;
            }
        })
    });

    // Wait for the child, but also watch for the host (vsock peer) disconnecting. If the
    // host `fcvm exec` dies (e.g. host-side timeout, #622), the connection closes; without
    // this we would `child.wait()` forever while the guest command keeps running (#636).
    let mut peer_closed = false;
    let exit_status = match peer_watch {
        Some(watch) => {
            tokio::select! {
                status = child.wait() => status,
                _ = wait_for_peer_close(&watch) => {
                    peer_closed = true;
                    if let Some(pid) = child_pid {
                        // Negative pid -> the child's process group (process_group(0) above),
                        // so the whole subtree is killed, not just the immediate child.
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
            }
        }
        None => child.wait().await,
    };

    if peer_closed {
        // Host is gone: stop the writer tasks (nothing to deliver to) and don't bother
        // writing Exit. fds close on drop.
        if let Some(task) = stdout_task {
            task.abort();
        }
        if let Some(task) = stderr_task {
            task.abort();
        }
        return;
    }

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    let exit_code = match exit_status {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            let response = ExecResponse::Error(format!("Wait failed: {}", e));
            let conn = async_fd.lock().await;
            write_line_async(&conn, &serde_json::to_string(&response).unwrap()).await;
            1
        }
    };

    let response = ExecResponse::Exit(exit_code);
    let conn = async_fd.lock().await;
    write_line_async(&conn, &serde_json::to_string(&response).unwrap()).await;
    // fd closed by OwnedFd drop (inside AsyncFd, inside Mutex, inside Arc)
}

// TIER 0 protocol-interleaving fuzz: systematic peer-death enumeration for the
// ACK/GO handshake (kept in its own file — see exec_fuzz_tests.rs).
#[cfg(test)]
#[path = "exec_fuzz_tests.rs"]
mod exec_fuzz_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn socketpair() -> (i32, i32) {
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair failed");
        (fds[0], fds[1])
    }

    fn write_all(fd: i32, data: &[u8]) {
        let written = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        assert_eq!(written, data.len() as isize, "short write to socketpair");
    }

    /// Read everything available from `fd` until EOF (bounded, blocking).
    fn read_to_eof(fd: i32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }

    /// Send `data` over a socketpair (with a pre-buffered GO line so the
    /// handshake can complete) and parse it with read_request_and_handshake.
    fn parse_request(data: &[u8]) -> Option<ExecRequest> {
        let (server_fd, client_fd) = socketpair();
        write_all(client_fd, data);
        write_all(
            client_fd,
            format!("{}\n", exec_proto::HANDSHAKE_GO).as_bytes(),
        );

        // read_request_and_handshake closes server_fd on error; on success it
        // returns it open.
        let request = match read_request_and_handshake(server_fd, Duration::from_secs(5)) {
            Some((request, fd)) => {
                unsafe { libc::close(fd) };
                Some(request)
            }
            None => None,
        };
        unsafe { libc::close(client_fd) };
        request
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
        let (server_fd, client_fd) = socketpair();

        let client = std::thread::spawn(move || {
            write_all(
                client_fd,
                b"{\"command\":[\"true\"],\"in_container\":false}\n",
            );
            // Wait for the full ACK line before sending GO.
            let mut ack = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = unsafe { libc::read(client_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
                assert!(n > 0, "server closed before ACK");
                if byte[0] == b'\n' {
                    break;
                }
                ack.push(byte[0]);
            }
            assert_eq!(ack, exec_proto::HANDSHAKE_ACK.as_bytes());
            write_all(
                client_fd,
                format!("{}\n", exec_proto::HANDSHAKE_GO).as_bytes(),
            );
            client_fd
        });

        let (request, fd) = read_request_and_handshake(server_fd, Duration::from_secs(5))
            .expect("handshake should complete");
        assert_eq!(request.command, vec!["true"]);
        unsafe { libc::close(fd) };
        let client_fd = client.join().unwrap();
        unsafe { libc::close(client_fd) };
    }

    /// No GO after ACK (the snapshot-pause orphan shape): the server must time
    /// out, close the connection, and never hand the request to execution. The
    /// client must observe exactly ACK then EOF — no Exit/Error response, which
    /// is the proof nothing executed.
    #[test]
    fn test_handshake_no_go_times_out_without_executing() {
        let (server_fd, client_fd) = socketpair();
        write_all(
            client_fd,
            b"{\"command\":[\"true\"],\"in_container\":false}\n",
        );

        let start = Instant::now();
        let result = read_request_and_handshake(server_fd, Duration::from_millis(200));
        assert!(result.is_none(), "handshake without GO must not execute");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout must be bounded (thread reclaimed), took {:?}",
            start.elapsed()
        );

        // Server closed the fd; the client sees the ACK it sent, then clean EOF.
        let seen = read_to_eof(client_fd);
        assert_eq!(
            seen,
            format!("{}\n", exec_proto::HANDSHAKE_ACK).into_bytes()
        );
        unsafe { libc::close(client_fd) };
    }

    /// No request line at all (connection accepted, host bytes lost in a vsock
    /// reset): bounded timeout, no ACK, fd closed, thread reclaimed.
    #[test]
    fn test_handshake_request_timeout_is_bounded() {
        let (server_fd, client_fd) = socketpair();

        let start = Instant::now();
        let result = read_request_and_handshake(server_fd, Duration::from_millis(200));
        assert!(result.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "orphaned connection must be reclaimed quickly, took {:?}",
            start.elapsed()
        );

        // No ACK was ever written — the request was never consumed.
        let seen = read_to_eof(client_fd);
        assert!(seen.is_empty(), "no bytes expected, got {:?}", seen);
        unsafe { libc::close(client_fd) };
    }

    /// Client vanishes after the request (EOF before GO): close without executing.
    #[test]
    fn test_handshake_client_close_before_go() {
        let (server_fd, client_fd) = socketpair();
        write_all(
            client_fd,
            b"{\"command\":[\"true\"],\"in_container\":false}\n",
        );
        unsafe { libc::close(client_fd) };

        let result = read_request_and_handshake(server_fd, Duration::from_secs(5));
        assert!(result.is_none(), "EOF before GO must not execute");
    }
}
