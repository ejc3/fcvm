use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::types::{CacheRequest, LogLine};

/// Upper bound on the post-cache-ack drain (see the positive close handshake in
/// [`run_status_listener`]).
///
/// This is NOT a race cover: the drain runs off the accept path, so nothing waits on
/// it. It only stops fcvm holding the fd forever when the guest's close never
/// propagates — which the vsock transport can do after the pre-start snapshot's
/// pause/resume. Matches the per-message read timeout used on status connections.
const CACHE_ACK_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Listen for fc-agent status messages on the status vsock port.
///
/// Firecracker forwards guest vsock connections to Unix sockets with format:
/// `{uds_path}_{port}` - so we listen on vsock.sock_4999 for port 4999.
///
/// Messages:
/// - "ready\n" - Container started, create ready file for health check
/// - "exit:{code}\n" - Container exited, write exit code to file
/// - "cache-ready:{digest}\n" - Image loaded, ready for caching (sends cache-ack back)
/// - "reboot\n" - Guest is rebooting; set the reboot flag and stay alive so the
///   relaunched fc-agent's fresh status connections are accepted (the listener must
///   NOT exit or remove its socket, unlike the "exit:" path).
pub(crate) async fn run_status_listener(
    socket_path: &str,
    runtime_dir: &std::path::Path,
    vm_id: &str,
    cache_tx: Option<mpsc::Sender<CacheRequest>>,
    reboot_requested: Arc<std::sync::atomic::AtomicBool>,
    container_exited: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding status listener to {}", socket_path))?;

    info!(socket = %socket_path, "Status listener started");

    let ready_file = runtime_dir.join("container-ready");
    let exit_file = runtime_dir.join("container-exit");

    // Post-ack drain tasks for finished cache-ready handshakes (see the positive
    // close handshake below). Owned by this task so VM cleanup, which aborts the
    // listener, aborts them too — dropping a JoinSet aborts everything in it.
    let mut drains = tokio::task::JoinSet::new();

    // Accept connections for the lifetime of the VM ("cache-ready", "ready",
    // "exit:", "reboot", host-side "drain" probes). No idle timeout: a VM can run
    // for days before its only end-of-life message arrives, and losing the
    // listener (plus its socket) would silently break reboot-in-place and exit
    // reporting. The run loop's cleanup aborts this task.
    loop {
        // Reap finished drains so a VM that reboots many times cannot accumulate
        // completed handles in the set.
        while drains.try_join_next().is_some() {}

        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!(vm_id = %vm_id, error = %e, "Error accepting status connection");
                continue;
            }
        };

        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        loop {
            line.clear();
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(300),
                reader.read_line(&mut line),
            )
            .await;

            let bytes = match read_result {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(e)) => {
                    warn!(vm_id = %vm_id, error = %e, "Error reading status connection");
                    break;
                }
                Err(_) => {
                    warn!(
                        vm_id = %vm_id,
                        "Timed out waiting for status message on status connection"
                    );
                    break;
                }
            };

            if bytes == 0 {
                break;
            }

            let msg = line.trim();
            if msg.is_empty() {
                continue;
            }

            if msg == "ready" {
                // Create ready file to signal container is running
                std::fs::write(&ready_file, "ready\n")
                    .with_context(|| format!("writing ready file: {:?}", ready_file))?;
                info!(vm_id = %vm_id, "Container ready notification received");
            } else if let Some(code_str) = msg.strip_prefix("exit:") {
                // Write exit code to file
                std::fs::write(&exit_file, format!("{}\n", code_str))
                    .with_context(|| format!("writing exit file: {:?}", exit_file))?;
                info!(
                    vm_id = %vm_id,
                    exit_code = %code_str,
                    "Container exit notification received"
                );
                // Signal the run loop, then KEEP ACCEPTING. A guest reboot can race
                // with a container exit (systemd stops the container, fc-agent sends
                // "exit:", THEN the reboot-notify unit sends "reboot" — both before
                // the firecracker reset). Exiting here would close the socket and
                // lose the trailing "reboot". The run loop owns termination; cleanup
                // aborts this task.
                container_exited.store(true, std::sync::atomic::Ordering::Release);
                break;
            } else if msg == "drain" {
                // Host-side probe (wait_for_reboot_decision): connections are
                // processed in accept order, so acking proves every message the
                // guest sent before the Firecracker exit has been handled — the
                // reboot/exit flags are authoritative once the ack is read.
                if let Err(e) = write_half.write_all(b"drain-ack\n").await {
                    warn!(vm_id = %vm_id, error = %e, "Failed to ack drain probe");
                }
                let _ = write_half.flush().await;
                break;
            } else if msg == "reboot" {
                // Guest is rebooting (systemd system-shutdown hook fired with verb
                // "reboot"). Record the intent so run_vm_loop relaunches Firecracker
                // in place when the child exits, then break to re-accept — the
                // relaunched fc-agent will open fresh cache-ready/ready connections.
                // Do NOT set exit_received and do NOT remove the socket.
                reboot_requested.store(true, std::sync::atomic::Ordering::Release);
                info!(vm_id = %vm_id, "Reboot notification received — VM will relaunch in place");
                break;
            } else if let Some(digest) = msg.strip_prefix("cache-ready:") {
                // fc-agent has loaded the image and is ready for caching
                info!(vm_id = %vm_id, digest = %digest, "Cache-ready notification received");

                // Whether to release fc-agent with "cache-ack". Stays true for
                // created/failed/no-channel cases; flips false when the run loop
                // drops the oneshot to signal "this VM is being replaced".
                let mut should_ack = true;

                if let Some(tx) = cache_tx.as_ref() {
                    // Create oneshot channel for ack
                    let (ack_tx, ack_rx) = oneshot::channel();

                    // Send cache request to main task
                    let request = CacheRequest {
                        digest: digest.to_string(),
                        ack_tx,
                    };

                    if tx.send(request).await.is_ok() {
                        // Wait for the main task to complete cache creation — no
                        // overall timeout (the host is responsible for completing),
                        // but emit a "cache-wait" keepalive every 5s. Under the
                        // SnapshotEnabled matrix the snapshot path queues on the
                        // global 10-permit snapshot semaphore BEFORE pausing the
                        // VM, so the guest keeps running with its fixed 30s
                        // cache-ack deadline ticking; without a liveness signal
                        // the guest abandons a perfectly alive handshake
                        // (CacheResult::Failed) and launches the container out of
                        // step with the host's pause (#627). The guest extends its
                        // deadline on each keepalive. While the VM is paused the
                        // bytes just queue in the socket and are drained on resume.
                        //
                        // The select also DRAINS the guest's 500ms liveness probe
                        // bytes ("\n" writes) as they arrive: leaving them unread
                        // would turn our eventual close into a vsock RST that can
                        // flush the guest's receive buffer before it reads the
                        // cache-ack, misclassifying a cold start as a warm restore
                        // (the failure mode documented in fc-agent's read path).
                        let mut ack_rx = ack_rx;
                        let mut probe_line = String::new();
                        // Pin the keepalive timer OUTSIDE the loop so it
                        // survives across select iterations: the guest sends
                        // probe bytes every ~500ms, and each probe-read
                        // restarts the select — an inline sleep(5) would
                        // reset on every probe and never fire while the guest
                        // is running (the pre-pause semaphore-queue scenario
                        // that #627 targets).
                        let keepalive_interval =
                            tokio::time::sleep(std::time::Duration::from_secs(5));
                        tokio::pin!(keepalive_interval);
                        loop {
                            probe_line.clear();
                            tokio::select! {
                                ack = &mut ack_rx => {
                                    match ack {
                                        Ok(()) => info!(vm_id = %vm_id, "Cache created, sending ack to fc-agent"),
                                        Err(_) => {
                                            // Sender dropped without a value: the run loop is
                                            // replacing this VM with a restore of the snapshot
                                            // it just created (or the process is going down).
                                            // Do NOT ack — an ack would let fc-agent start the
                                            // container in the doomed VM, racing teardown and
                                            // double-running container startup against shared
                                            // volumes. (Create FAILURES send Ok explicitly.)
                                            warn!(vm_id = %vm_id, "cache oneshot dropped; suppressing cache-ack (VM being replaced)");
                                            should_ack = false;
                                        }
                                    }
                                    break;
                                }
                                read = reader.read_line(&mut probe_line) => {
                                    match read {
                                        // Probe bytes drained; nothing to do.
                                        Ok(n) if n > 0 => {}
                                        // EOF / read error: guest side gone (e.g.
                                        // snapshot restore reset). Keep waiting for
                                        // the ack so the main task's oneshot isn't
                                        // dropped mid-snapshot.
                                        _ => {
                                            should_ack = (&mut ack_rx).await.is_ok();
                                            break;
                                        }
                                    }
                                }
                                _ = &mut keepalive_interval => {
                                    if write_half.write_all(b"cache-wait\n").await.is_err() {
                                        should_ack = (&mut ack_rx).await.is_ok();
                                        break;
                                    }
                                    let _ = write_half.flush().await;
                                    // Reset for the next keepalive interval.
                                    keepalive_interval.as_mut().reset(
                                        tokio::time::Instant::now()
                                            + std::time::Duration::from_secs(5),
                                    );
                                }
                            }
                        }
                    } else {
                        warn!(vm_id = %vm_id, "Failed to send cache request to main task");
                    }
                }

                // Send ack back to fc-agent (even if cache creation failed) —
                // unless the oneshot was dropped, which means this VM is being
                // replaced by a restore and must not start its container.
                if should_ack {
                    if let Err(e) = write_half.write_all(b"cache-ack\n").await {
                        warn!(vm_id = %vm_id, error = %e, "Failed to send cache-ack to fc-agent");
                    }
                    let _ = write_half.flush().await;
                }

                // POSITIVE CLOSE HANDSHAKE (#627). Closing this connection while a
                // guest probe byte sits unread turns the close into a vsock RST, and
                // an RST FLUSHES the guest's receive buffer — including a cache-ack
                // it has not read yet, so a cold start misreports itself as a warm
                // restore. The guest closes as soon as it has read the ack, so EOF
                // here is proof the ack landed and no further probe can arrive.
                //
                // Hand the connection to a drain task instead of waiting inline: the
                // accept loop must stay free (the guest opens a SEPARATE connection
                // for "ready", and this one is the one held across the pre-start
                // snapshot's pause/resume, where the close can fail to propagate at
                // all). The task holds both halves — so nothing is ever closed with
                // bytes outstanding — and ends at EOF, at the bound, or when the VM's
                // listener task is aborted at cleanup (JoinSet drop aborts it).
                drains.spawn(async move {
                    let mut sink = String::new();
                    let _ = tokio::time::timeout(CACHE_ACK_DRAIN_TIMEOUT, async move {
                        // Read to EOF. Probe bytes are consumed and discarded; the
                        // write half is held (not dropped) until EOF so the peer
                        // never sees a half-closed connection.
                        while matches!(reader.read_line(&mut sink).await, Ok(n) if n > 0) {
                            sink.clear();
                        }
                        drop(write_half);
                    })
                    .await;
                });

                // Stop reading this connection and go back to accept(). fc-agent
                // sends each status message ("cache-ready", then "ready", then
                // "exit") on a SEPARATE vsock connection, so nothing more arrives
                // here. Critically, this connection is the one held open across
                // the pre-start snapshot's pause/resume: after resume the vsock
                // transport can fail to propagate its close, so a second read_line
                // here would block for the full 300s timeout before the accept
                // loop could take the new connection carrying "ready" — stalling
                // container startup by ~5 minutes (intermittently, worse on the
                // NV2 nested kernel). Breaking to re-accept delivers "ready"
                // immediately regardless of when this connection's EOF arrives.
                break;
            } else {
                warn!(vm_id = %vm_id, msg = %msg, "Unexpected status message");
            }
        }
    }
}

/// Bind the boot-plan vsock listener and spawn its serve loop (#632 P0.5).
///
/// For VMMs without a metadata service (Cloud Hypervisor), fc-agent fetches its plan
/// from this listener instead of MMDS. Firecracker forwards a guest connection to vsock
/// port N to the Unix socket `{uds_path}_{N}`, so we bind that path and write the plan
/// JSON (the MMDS `latest` object: `{ "container-plan": {...}, "host-time": "..." }`) to
/// each connecting guest, then close so the guest's `read_to_end` sees EOF.
///
/// Serialization and bind happen SYNCHRONOUSLY so a failure is surfaced to the caller
/// (returns `Err`) instead of being swallowed in a detached task — otherwise the guest
/// would loop forever waiting for a plan that is never served. The returned task runs for
/// the VM's lifetime (the guest may reconnect for clock sync); the caller aborts it on
/// cleanup or on any later boot-config failure.
pub(crate) fn spawn_bootplan_listener(
    socket_path: &str,
    plan: &serde_json::Value,
) -> Result<tokio::task::JoinHandle<()>> {
    use tokio::net::UnixListener;

    let payload = serde_json::to_vec(plan).context("serializing boot plan")?;
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding boot-plan listener to {socket_path}"))?;
    info!(socket = %socket_path, bytes = payload.len(), "Boot-plan listener started (vsock)");

    Ok(tokio::spawn(serve_bootplan(listener, payload)))
}

/// Accept loop for the boot-plan listener: write the plan to each connecting guest and
/// close the write side so the guest's `read_to_end` observes EOF.
async fn serve_bootplan(listener: tokio::net::UnixListener, payload: Vec<u8>) {
    use tokio::io::AsyncWriteExt;
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                if let Err(e) = stream.write_all(&payload).await {
                    debug!(error = %e, "boot-plan listener: write failed");
                    continue;
                }
                let _ = stream.shutdown().await;
            }
            Err(e) => {
                warn!(error = %e, "boot-plan listener: accept error");
            }
        }
    }
}

/// Bidirectional I/O listener for container stdin/stdout/stderr.
///
/// Listens on port 4997 for raw output from fc-agent.
/// Protocol (all lines are newline-terminated):
///   Guest -> Host: "stdout:content" or "stderr:content"
///   Host -> Guest: "stdin:content" (written to container stdin)
///
/// Returns collected output lines as Vec<(stream, line)>.
pub(crate) async fn run_output_listener(
    socket_path: &str,
    vm_id: &str,
    log_tx: Option<tokio::sync::broadcast::Sender<LogLine>>,
    reconnect_notify: Arc<tokio::sync::Notify>,
    non_blocking_output: bool,
    connected_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<Vec<(String, String)>> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding output listener to {}", socket_path))?;

    // In non-blocking mode, use a bounded channel + writer thread so the
    // listener never blocks on stdout/stderr. Messages are dropped when the
    // channel is full, preventing backpressure from cascading into the container.
    let nb_tx = if non_blocking_output {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, String)>(1024);
        std::thread::Builder::new()
            .name("output-writer".into())
            .spawn(move || {
                for (is_stdout, content) in rx {
                    if is_stdout {
                        let _ = writeln!(std::io::stdout(), "{}", content);
                    } else {
                        let _ = writeln!(std::io::stderr(), "{}", content);
                    }
                }
            })
            .expect("spawn output writer thread");
        info!(vm_id = %vm_id, "Non-blocking output mode: dropping messages when pipe is full");
        Some(tx)
    } else {
        None
    };

    info!(socket = %socket_path, "Output listener started");

    let mut output_lines: Vec<(String, String)> = Vec::new();
    let mut connection_count: u64 = 0;
    let mut lines_read = 0u64;

    // Accept the first connection (no timeout — image import can take 10+ min)
    let (initial_stream, _) = match listener.accept().await {
        Ok(conn) => conn,
        Err(e) => {
            warn!(vm_id = %vm_id, error = %e, "Error accepting initial output connection");
            let _ = std::fs::remove_file(socket_path);
            return Ok(output_lines);
        }
    };
    connection_count += 1;
    debug!(vm_id = %vm_id, connection_count, "Output connection established");
    if let Some(tx) = connected_tx {
        let _ = tx.send(());
    }

    let mut reader = BufReader::new(initial_stream);
    let mut line_buf = String::new();

    // Read lines from the current connection.
    // fc-agent may reconnect multiple times (snapshot create/restore cycles).
    // Always prefer the newest connection — if a new connection arrives while
    // reading from the current one, switch to it. The latest connection is
    // always the right one because it's from fc-agent's latest vsock connect.
    loop {
        line_buf.clear();

        // Race: read data vs new connection vs reconnect signal
        tokio::select! {
            result = reader.read_line(&mut line_buf) => {
                match result {
                    Ok(0) => {
                        // EOF — connection closed. Wait for next.
                        info!(vm_id = %vm_id, lines_read, "Output connection EOF");
                        match listener.accept().await {
                            Ok((s, _)) => {
                                connection_count += 1;
                                lines_read = 0;
                                debug!(vm_id = %vm_id, connection_count, "Output connection established (after EOF)");
                                reader = BufReader::new(s);
                                continue;
                            }
                            Err(e) => {
                                warn!(vm_id = %vm_id, error = %e, "Accept failed after EOF");
                                break;
                            }
                        }
                    }
                    Ok(n) => {
                        lines_read += 1;
                        if lines_read <= 3 || lines_read % 1000 == 0 {
                            debug!(vm_id = %vm_id, lines_read, bytes = n, "Output line received");
                        }
                        let line = line_buf.trim_end();
                        if let Some((stream, content)) = line.split_once(':') {
                            if stream == "heartbeat" {
                                info!(vm_id = %vm_id, phase = %content, "VM heartbeat");
                            } else {
                                // Use writeln! instead of println!/eprintln! to avoid
                                // panicking on broken pipe. println! panics on write
                                // error, which kills the listener task and stops
                                // draining the vsock — deadlocking the container.
                                let is_stdout = stream == "stdout";
                                if let Some(ref tx) = nb_tx {
                                    // Non-blocking mode: try_send drops if channel full.
                                    // Writer thread handles the blocking stdout write.
                                    let _ = tx.try_send((is_stdout, content.to_string()));
                                } else if is_stdout {
                                    let _ = writeln!(std::io::stdout(), "{}", content);
                                } else {
                                    let _ = writeln!(std::io::stderr(), "{}", content);
                                }
                                output_lines.push((stream.to_string(), content.to_string()));
                            }
                            if let Some(ref tx) = log_tx {
                                let _ = tx.send(LogLine {
                                    stream: stream.to_string(),
                                    content: content.to_string(),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        warn!(vm_id = %vm_id, error = %e, "Read error on output connection");
                        break;
                    }
                }
            }
            accept = listener.accept() => {
                // New connection arrived while reading — switch to it.
                // The latest connection is always from fc-agent's latest reconnect.
                match accept {
                    Ok((new_stream, _)) => {
                        connection_count += 1;
                        info!(vm_id = %vm_id, connection_count, lines_read, "Switching to newer output connection");
                        reader = BufReader::new(new_stream);
                        lines_read = 0;
                    }
                    Err(e) => {
                        warn!(vm_id = %vm_id, error = %e, "Accept failed for new connection");
                        break;
                    }
                }
            }
            _ = reconnect_notify.notified() => {
                info!(vm_id = %vm_id, lines_read, "Reconnect signal, waiting for new connection");
                match listener.accept().await {
                    Ok((s, _)) => {
                        connection_count += 1;
                        lines_read = 0;
                        debug!(vm_id = %vm_id, connection_count, "Output connection established (after signal)");
                        reader = BufReader::new(s);
                    }
                    Err(e) => {
                        warn!(vm_id = %vm_id, error = %e, "Accept failed after signal");
                        break;
                    }
                }
            }
        }
    }

    // Clean up
    let _ = std::fs::remove_file(socket_path);

    info!(vm_id = %vm_id, lines = output_lines.len(), "Output listener finished");
    Ok(output_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Verify that writeln! to a broken pipe doesn't panic (returns Err),
    /// while println! would panic with "failed printing to stdout".
    #[test]
    fn test_writeln_survives_broken_pipe() {
        use std::io::Write;
        use std::os::unix::io::FromRawFd;

        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        drop(read_fd);

        let mut writer = unsafe {
            std::fs::File::from_raw_fd(std::os::unix::io::IntoRawFd::into_raw_fd(write_fd))
        };
        let result = writeln!(writer, "test output");

        assert!(result.is_err(), "writeln! should return Err on broken pipe");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// Without --non-blocking-output: blocking send blocks on a full channel.
    /// This is what causes the backpressure deadlock in default mode.
    #[test]
    fn test_sync_channel_send_blocks_when_full() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<String>(1);
        tx.send("msg1".to_string()).unwrap();

        let result = tx.try_send("msg2".to_string());
        assert!(result.is_err(), "channel should be full");
        assert!(matches!(
            result.unwrap_err(),
            std::sync::mpsc::TrySendError::Full(_)
        ));
    }

    /// With --non-blocking-output: try_send drops messages when channel is full.
    /// This prevents backpressure from cascading into the container.
    #[test]
    fn test_lossy_try_send_drops_on_full_channel() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, String)>(2);

        tx.try_send((true, "line1".to_string())).unwrap();
        tx.try_send((true, "line2".to_string())).unwrap();

        let result = tx.try_send((true, "line3-dropped".to_string()));
        assert!(result.is_err(), "try_send should fail on full channel");

        assert_eq!(rx.recv().unwrap().1, "line1");
        assert_eq!(rx.recv().unwrap().1, "line2");
        assert!(rx.try_recv().is_err());
    }

    /// Helper: spawn the output listener, wait for socket, return JoinHandle.
    async fn spawn_listener(
        socket_path: &std::path::Path,
        lossy: bool,
    ) -> tokio::task::JoinHandle<Result<Vec<(String, String)>>> {
        let socket_str = socket_path.to_string_lossy().to_string();
        let reconnect = std::sync::Arc::new(tokio::sync::Notify::new());
        let handle = tokio::spawn(async move {
            run_output_listener(&socket_str, "test-vm", None, reconnect, lossy, None).await
        });
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists(), "output socket not created");
        handle
    }

    /// With --non-blocking-output, the listener processes all input without blocking.
    #[tokio::test]
    async fn test_output_listener_lossy_mode_processes_all_input() {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("output-lossy.sock");

        let listener = spawn_listener(&socket_path, true).await;

        let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        for i in 0..100 {
            stream
                .write_all(format!("stdout:lossy-line-{}\n", i).as_bytes())
                .await
                .unwrap();
        }
        drop(stream);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        listener.abort();
        let err = listener.await.unwrap_err();
        assert!(
            err.is_cancelled(),
            "lossy listener should be cancelled, not panicked: {:?}",
            err
        );
    }

    /// Default mode processes lines normally when stdout is healthy.
    #[tokio::test]
    async fn test_output_listener_default_mode_processes_lines() {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("output-default.sock");

        let listener = spawn_listener(&socket_path, false).await;

        let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        stream
            .write_all(b"stdout:hello\nstderr:world\n")
            .await
            .unwrap();
        drop(stream);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        listener.abort();
        let err = listener.await.unwrap_err();
        assert!(
            err.is_cancelled(),
            "default listener should be cancelled, not panicked: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_status_listener_handles_multiple_messages_on_single_connection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        let reboot_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag_for_task = exit_flag.clone();
        let listener_task = tokio::spawn(async move {
            run_status_listener(
                &socket_path_string,
                &runtime_dir_for_task,
                "vm-test",
                None,
                reboot_flag,
                exit_flag_for_task,
            )
            .await
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists(), "status socket was not created");

        let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(b"ready\nexit:0\n").await.unwrap();
        drop(stream);

        // The listener stays alive after "exit:" (it must catch a racing "reboot");
        // wait for the exit flag instead of joining, then abort.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !exit_flag.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "listener did not process exit notification"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !listener_task.is_finished(),
            "listener must stay alive after exit: to catch a racing reboot signal"
        );
        listener_task.abort();

        assert_eq!(
            std::fs::read_to_string(runtime_dir.join("container-ready")).unwrap(),
            "ready\n"
        );
        assert_eq!(
            std::fs::read_to_string(runtime_dir.join("container-exit")).unwrap(),
            "0\n"
        );
    }

    /// The drain probe is the positive handshake wait_for_reboot_decision uses:
    /// connections are processed in accept order, so the "drain-ack" reply proves
    /// every earlier guest message has been handled.
    #[tokio::test]
    async fn test_status_listener_acks_drain_probe_after_prior_messages() {
        use tokio::io::AsyncReadExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        let reboot_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reboot_for_task = reboot_flag.clone();
        let exit_for_task = exit_flag.clone();
        let listener_task = tokio::spawn(async move {
            run_status_listener(
                &socket_path_string,
                &runtime_dir_for_task,
                "vm-test",
                None,
                reboot_for_task,
                exit_for_task,
            )
            .await
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Earlier guest message (reboot), then the host's drain probe.
        let mut s1 = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        s1.write_all(b"reboot\n").await.unwrap();
        drop(s1);
        let mut probe = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        probe.write_all(b"drain\n").await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), probe.read(&mut buf))
            .await
            .expect("drain ack timed out")
            .unwrap();
        assert!(n > 0, "listener must ack the drain probe");
        // By accept-order, the earlier reboot message MUST be processed by now.
        assert!(
            reboot_flag.load(std::sync::atomic::Ordering::Acquire),
            "drain-ack received but earlier reboot message not yet processed"
        );
        listener_task.abort();
    }

    /// A slow cache-snapshot (host queued on the snapshot semaphore) must emit
    /// "cache-wait" keepalives on the cache-ready connection BEFORE the final
    /// "cache-ack", so the guest can extend its fixed deadline instead of
    /// abandoning a live handshake (#627).
    #[tokio::test]
    async fn test_status_listener_emits_cache_wait_keepalive_before_ack() {
        use tokio::io::AsyncReadExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        // cache handler that takes >5s (one keepalive interval) to ack.
        let (cache_tx, mut cache_rx) = mpsc::channel::<CacheRequest>(1);
        tokio::spawn(async move {
            if let Some(req) = cache_rx.recv().await {
                tokio::time::sleep(std::time::Duration::from_millis(5600)).await;
                let _ = req.ack_tx.send(());
            }
        });

        let reboot_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener_task = tokio::spawn(async move {
            run_status_listener(
                &socket_path_string,
                &runtime_dir_for_task,
                "vm-test",
                Some(cache_tx),
                reboot_flag,
                exit_flag,
            )
            .await
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(b"cache-ready:sha256:abc\n").await.unwrap();

        // Collect everything the host writes until the ack arrives.
        let mut received = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut buf = [0u8; 256];
        while !received.contains("cache-ack") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no cache-ack within deadline; got: {received:?}"
            );
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut buf))
                .await
                .expect("read timed out")
                .unwrap();
            assert!(n > 0, "connection closed before ack; got: {received:?}");
            received.push_str(std::str::from_utf8(&buf[..n]).unwrap());
        }

        let wait_pos = received.find("cache-wait");
        let ack_pos = received.find("cache-ack").unwrap();
        assert!(
            wait_pos.is_some() && wait_pos.unwrap() < ack_pos,
            "expected at least one cache-wait keepalive before cache-ack; got: {received:?}"
        );
        listener_task.abort();
    }

    /// After sending "cache-ack" the host must NOT close the cache-ready connection:
    /// it drains to EOF and lets the GUEST close first.
    ///
    /// The guest write-probes this connection every 500ms while it waits. If the host
    /// closes with a probe byte unread, the close becomes a vsock RST — and an RST
    /// FLUSHES the guest's receive buffer, discarding a cache-ack it has not read yet.
    /// The guest then classifies a cold start as a warm restore (#627). The old fixed
    /// 250ms grace window only narrowed that race; the guest's close is the actual
    /// proof the ack landed. Draining off the accept path also keeps the listener free
    /// to take the next connection, which carries "ready".
    #[tokio::test]
    async fn test_status_listener_holds_cache_connection_until_guest_closes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt as _};

        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        // Ack as soon as the request arrives: this is the cold-start path.
        let (cache_tx, mut cache_rx) = mpsc::channel::<CacheRequest>(1);
        tokio::spawn(async move {
            if let Some(req) = cache_rx.recv().await {
                let _ = req.ack_tx.send(());
            }
        });

        let reboot_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener_task = tokio::spawn(async move {
            run_status_listener(
                &socket_path_string,
                &runtime_dir_for_task,
                "vm-test",
                Some(cache_tx),
                reboot_flag,
                exit_flag,
            )
            .await
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (mut guest_rx, mut guest_tx) = stream.into_split();
        guest_tx
            .write_all(b"cache-ready:sha256:abc\n")
            .await
            .unwrap();

        let mut received = String::new();
        let mut buf = [0u8; 256];
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !received.contains("cache-ack") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no cache-ack within deadline; got: {received:?}"
            );
            let n =
                tokio::time::timeout(std::time::Duration::from_secs(5), guest_rx.read(&mut buf))
                    .await
                    .expect("read timed out")
                    .unwrap();
            assert!(n > 0, "connection closed before ack; got: {received:?}");
            received.push_str(std::str::from_utf8(&buf[..n]).unwrap());
        }

        // Keep probing well past the old 250ms grace window. Every probe must be
        // accepted: a failure means the host let go of the connection first.
        for probe in 0..15 {
            if let Err(e) = guest_tx.write_all(b"\n").await {
                panic!(
                    "host closed the cache connection after the ack (probe {probe} failed: {e}); \
                     that close is the RST which flushes the guest's unread cache-ack (#627)"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // ...and the host must not have hung up on its side either.
        match tokio::time::timeout(
            std::time::Duration::from_millis(300),
            guest_rx.read(&mut buf),
        )
        .await
        {
            Err(_) => {} // Nothing sent, no EOF: the host is still holding it open.
            Ok(Ok(0)) => panic!("host closed the cache connection (EOF) after the ack"),
            Ok(Ok(n)) => panic!("unexpected {n} bytes after cache-ack: {:?}", &buf[..n]),
            Ok(Err(e)) => panic!("cache connection broke after the ack: {e}"),
        }

        // The drain must run OFF the accept path: the next connection carries "ready",
        // and container startup stalls if the listener is still stuck on the old one.
        let mut ready_conn = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        ready_conn.write_all(b"ready\n").await.unwrap();
        let ready_file = runtime_dir.join("container-ready");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready_file.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "listener did not accept the 'ready' connection while draining the cache one"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        listener_task.abort();
    }

    /// Same as above, but the guest continuously sends probe bytes ("\n") every
    /// 500ms — matching production behaviour. Before the pinned-timer fix, the
    /// inline `sleep(5)` was reset by each probe-read and would NEVER fire,
    /// leaving the guest without keepalives during the pre-pause semaphore queue.
    #[tokio::test]
    async fn test_status_listener_keepalive_fires_despite_probe_traffic() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt as _};

        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        // cache handler that takes >5s (one keepalive interval) to ack.
        let (cache_tx, mut cache_rx) = mpsc::channel::<CacheRequest>(1);
        tokio::spawn(async move {
            if let Some(req) = cache_rx.recv().await {
                tokio::time::sleep(std::time::Duration::from_millis(5600)).await;
                let _ = req.ack_tx.send(());
            }
        });

        let reboot_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener_task = tokio::spawn(async move {
            run_status_listener(
                &socket_path_string,
                &runtime_dir_for_task,
                "vm-test",
                Some(cache_tx),
                reboot_flag,
                exit_flag,
            )
            .await
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Split the stream so a background task can write probes while the
        // main task reads the host's responses.
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();

        write_half
            .write_all(b"cache-ready:sha256:abc\n")
            .await
            .unwrap();

        // Background task: send probe "\n" every 500ms (mimics fc-agent).
        // Before the pinned-timer fix, these resets prevented the keepalive
        // timer from ever firing.
        let probe_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_done_clone = probe_done.clone();
        let probe_task = tokio::spawn(async move {
            while !probe_done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if write_half.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        });

        // Collect everything the host writes until the ack arrives.
        let mut received = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut buf = [0u8; 256];
        let mut read_half = tokio::io::BufReader::new(read_half);
        while !received.contains("cache-ack") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no cache-ack within deadline; got: {received:?}"
            );
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                AsyncReadExt::read(&mut read_half, &mut buf),
            )
            .await
            .expect("read timed out")
            .unwrap();
            assert!(n > 0, "connection closed before ack; got: {received:?}");
            received.push_str(std::str::from_utf8(&buf[..n]).unwrap());
        }

        probe_done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = probe_task.await;

        let wait_pos = received.find("cache-wait");
        let ack_pos = received.find("cache-ack").unwrap();
        assert!(
            wait_pos.is_some() && wait_pos.unwrap() < ack_pos,
            "expected cache-wait keepalive before cache-ack even with probe traffic; got: {received:?}"
        );
        listener_task.abort();
    }

    /// A container "exit:" followed by a "reboot" on a separate connection (the
    /// guest-reboot race) must leave BOTH flags set — the listener may not exit
    /// after "exit:" or the trailing reboot notification would be lost.
    #[tokio::test]
    async fn test_status_listener_reboot_after_exit_race() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        let reboot_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exit_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reboot_for_task = reboot_flag.clone();
        let exit_for_task = exit_flag.clone();
        let listener_task = tokio::spawn(async move {
            run_status_listener(
                &socket_path_string,
                &runtime_dir_for_task,
                "vm-test",
                None,
                reboot_for_task,
                exit_for_task,
            )
            .await
        });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Connection 1: container exit (systemd stopping the container mid-reboot).
        let mut s1 = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        s1.write_all(b"exit:0\n").await.unwrap();
        drop(s1);
        // Connection 2: the reboot-notify unit fires afterwards.
        let mut s2 = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        s2.write_all(b"reboot\n").await.unwrap();
        drop(s2);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !reboot_flag.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "reboot signal after exit: was lost (listener exited too early?)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(exit_flag.load(std::sync::atomic::Ordering::Acquire));
        listener_task.abort();
    }
}
