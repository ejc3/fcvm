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
/// IMPORTANT: Only ONE rebind signal should be sent per snapshot/restore event.
/// A double re-register (from two concurrent signal sources) leaves the listener
/// in a broken state where epoll events are not delivered for ~150 seconds.
/// The signal comes from `handle_clone_restore()` only — agent.rs must NOT send
/// a duplicate signal after `notify_cache_ready_and_wait()` returns.
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
        // Check flag before polling — catches notifications lost due to select! races.
        // When both accept() and notified() are Ready simultaneously, select! picks one
        // and drops the other. If accept() wins, the Notified future's permit was already
        // consumed during poll() but the re_register branch never executed. The flag
        // persists across this race and is checked here on the next iteration.
        if rebind_needed.swap(false, Ordering::AcqRel) {
            eprintln!(
                "[fc-agent] exec server: vsock transport reset (flag), re-registering listener"
            );
            listener = do_re_register(listener).await;
        }

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok(client_fd) => {
                        tokio::spawn(handle_connection(client_fd));
                    }
                    Err(e) => {
                        eprintln!("[fc-agent] exec server accept error: {}", e);
                    }
                }
            }
            _ = rebind_signal.notified() => {
                // Clear the flag (we're handling it now).
                rebind_needed.store(false, Ordering::Release);
                eprintln!("[fc-agent] exec server: vsock transport reset (notify), re-registering listener");
                listener = do_re_register(listener).await;
            }
        }
    }
}

/// Re-register the vsock listener with epoll after transport reset.
///
/// After snapshot restore, the vsock transport is reset and the listener's AsyncFd
/// epoll registration becomes stale. Re-registering the same fd (extract from old
/// AsyncFd, wrap in new AsyncFd) restores epoll event delivery without closing the
/// socket. This avoids EADDRINUSE that would occur with close+rebind (active
/// connections from handle_connection tasks keep the port busy).
///
/// Falls back to full close+rebind if re_register fails.
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
            let mut retries = 0;
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
                        if retries >= 50 {
                            eprintln!(
                                "[fc-agent] exec server: re-bind failed after {} retries: {}",
                                retries, e2
                            );
                            // Fatal: exec server is unavailable. Panic to make the
                            // failure visible rather than silently hanging.
                            panic!("exec server: re-bind failed after 50 retries");
                        }
                        eprintln!(
                            "[fc-agent] exec server: re-bind failed: {}, retrying ({}/50)",
                            e2, retries
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
fn read_request_line(fd: i32) -> Option<(ExecRequest, i32)> {
    const MAX_EXEC_LINE_LENGTH: usize = 1_048_576;
    let mut line = String::new();
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
        line.push(buf[0] as char);
    }

    let request: ExecRequest = match serde_json::from_str(&line) {
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let response = ExecResponse::Error(format!("Failed to spawn: {}", e));
            write_line_to_fd(raw_fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(raw_fd) };
            return;
        }
    };

    // Spawn succeeded — set non-blocking, take ownership, wrap in AsyncFd
    nix::fcntl::fcntl(
        raw_fd,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .ok();
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

    let exit_status = child.wait().await;

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
