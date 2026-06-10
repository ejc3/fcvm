use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Notify};

use crate::types::{ExecRequest, ExecResponse};
use crate::vsock;

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

/// Read the request line synchronously (blocking byte-by-byte read).
/// Returns (ExecRequest, raw_fd) on success, or None if connection closed or parse error.
///
/// The request is accumulated as raw bytes and parsed as UTF-8 JSON. The host
/// serializes ExecRequest with serde_json::to_string, which emits non-ASCII
/// characters as raw UTF-8 — decoding byte-by-byte (Latin-1) would silently corrupt
/// multi-byte arguments and paths.
fn read_request_line(fd: i32) -> Option<(ExecRequest, i32)> {
    const MAX_EXEC_LINE_LENGTH: usize = 1_048_576;
    let mut line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n <= 0 {
            unsafe { libc::close(fd) };
            return None;
        }
        if buf[0] == b'\n' {
            break;
        }
        if line.len() >= MAX_EXEC_LINE_LENGTH {
            eprintln!(
                "[fc-agent] exec request line exceeds {} bytes, rejecting",
                MAX_EXEC_LINE_LENGTH
            );
            unsafe { libc::close(fd) };
            return None;
        }
        line.push(buf[0]);
    }

    let request: ExecRequest = match serde_json::from_slice(&line) {
        Ok(r) => r,
        Err(e) => {
            let response = ExecResponse::Error(format!("Invalid request: {}", e));
            write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(fd) };
            return None;
        }
    };

    Some((request, fd))
}

async fn handle_connection(client_fd: OwnedFd) {
    // Read request line in spawn_blocking (blocking byte-by-byte read, fast)
    let raw_fd = client_fd.into_raw_fd();
    let parsed = tokio::task::spawn_blocking(move || read_request_line(raw_fd)).await;

    let (request, raw_fd) = match parsed {
        Ok(Some((req, fd))) => (req, fd),
        Ok(None) => return, // connection closed or parse error (already handled)
        Err(_) => return,   // spawn_blocking panicked
    };

    if request.command.is_empty() {
        let response = ExecResponse::Error("Empty command".to_string());
        write_line_to_fd(raw_fd, &serde_json::to_string(&response).unwrap());
        unsafe { libc::close(raw_fd) };
        return;
    }

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
/// non-TTY pipe path the host sends nothing after the request (it only reads responses), so
/// any readable EOF is equivalent to "host gone" — used to kill the guest child (#636).
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
        // n > 0: the host should send nothing after the request (it only reads responses,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Send `data` over a socketpair and parse it with read_request_line.
    fn parse_request(data: &[u8]) -> Option<ExecRequest> {
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair failed");
        let (server_fd, client_fd) = (fds[0], fds[1]);

        let written = unsafe { libc::write(client_fd, data.as_ptr().cast(), data.len()) };
        assert_eq!(written, data.len() as isize, "short write to socketpair");

        // read_request_line closes server_fd on error; on success it returns it open.
        let request = match read_request_line(server_fd) {
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
    fn test_read_request_line_preserves_utf8_args() {
        // The host serializes ExecRequest with serde_json::to_string, which emits
        // non-ASCII characters as raw UTF-8 bytes — they must round-trip intact.
        let json = "{\"command\":[\"touch\",\"/data/héllo wörld.txt\"]}\n";
        let request = parse_request(json.as_bytes()).expect("request should parse");
        assert_eq!(request.command, vec!["touch", "/data/héllo wörld.txt"]);
        assert!(!request.in_container);
        assert!(!request.tty);
    }

    #[test]
    fn test_read_request_line_ascii() {
        let json = "{\"command\":[\"echo\",\"hello\"],\"in_container\":true}\n";
        let request = parse_request(json.as_bytes()).expect("request should parse");
        assert_eq!(request.command, vec!["echo", "hello"]);
        assert!(request.in_container);
    }

    #[test]
    fn test_read_request_line_rejects_invalid_json() {
        assert!(parse_request(b"not json\n").is_none());
    }
}
