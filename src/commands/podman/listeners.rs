use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::types::{CacheRequest, LogLine};

/// Listen for fc-agent status messages on the status vsock port.
///
/// Firecracker forwards guest vsock connections to Unix sockets with format:
/// `{uds_path}_{port}` - so we listen on vsock.sock_4999 for port 4999.
///
/// Messages:
/// - "ready\n" - Container started, create ready file for health check
/// - "exit:{code}\n" - Container exited, write exit code to file
/// - "cache-ready:{digest}\n" - Image loaded, ready for caching (sends cache-ack back)
pub(super) async fn run_status_listener(
    socket_path: &str,
    runtime_dir: &std::path::Path,
    vm_id: &str,
    cache_tx: Option<mpsc::Sender<CacheRequest>>,
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

    let mut exit_received = false;

    // Accept connections in a loop (we get "cache-ready" then "ready" then "exit")
    loop {
        let accept_result = tokio::time::timeout(
            std::time::Duration::from_secs(3600), // 1 hour timeout
            listener.accept(),
        )
        .await;

        let (stream, _) = match accept_result {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                warn!(vm_id = %vm_id, error = %e, "Error accepting status connection");
                continue;
            }
            Err(_) => {
                // Timeout - VM probably shut down without sending exit
                break;
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
            } else if let Some(debug_msg) = msg.strip_prefix("debug:") {
                // Debug message from fc-agent (useful when serial console is broken after restore)
                info!(vm_id = %vm_id, debug = %debug_msg, "fc-agent debug message");
            } else if let Some(code_str) = msg.strip_prefix("exit:") {
                // Write exit code to file
                std::fs::write(&exit_file, format!("{}\n", code_str))
                    .with_context(|| format!("writing exit file: {:?}", exit_file))?;
                info!(
                    vm_id = %vm_id,
                    exit_code = %code_str,
                    "Container exit notification received"
                );
                exit_received = true;
                break;
            } else if let Some(digest) = msg.strip_prefix("cache-ready:") {
                // fc-agent has loaded the image and is ready for caching
                info!(vm_id = %vm_id, digest = %digest, "Cache-ready notification received");

                if let Some(tx) = cache_tx.as_ref() {
                    // Create oneshot channel for ack
                    let (ack_tx, ack_rx) = oneshot::channel();

                    // Send cache request to main task
                    let request = CacheRequest {
                        digest: digest.to_string(),
                        ack_tx,
                    };

                    if tx.send(request).await.is_ok() {
                        // Wait for main task to complete cache creation
                        // No timeout - host is responsible for completing
                        if ack_rx.await.is_ok() {
                            info!(vm_id = %vm_id, "Cache created, sending ack to fc-agent");
                        } else {
                            warn!(vm_id = %vm_id, "Cache creation failed or was cancelled");
                        }
                    } else {
                        warn!(vm_id = %vm_id, "Failed to send cache request to main task");
                    }
                }

                // Send ack back to fc-agent (even if cache creation failed)
                if let Err(e) = write_half.write_all(b"cache-ack\n").await {
                    warn!(vm_id = %vm_id, error = %e, "Failed to send cache-ack to fc-agent");
                }
            } else {
                warn!(vm_id = %vm_id, msg = %msg, "Unexpected status message");
            }
        }

        if exit_received {
            break;
        }
    }

    // Clean up socket
    let _ = std::fs::remove_file(socket_path);

    Ok(())
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
) -> Result<Vec<(String, String)>> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding output listener to {}", socket_path))?;

    info!(socket = %socket_path, "Output listener started");

    let mut output_lines: Vec<(String, String)> = Vec::new();

    // Outer loop: accept connections repeatedly.
    // Firecracker resets all vsock connections during snapshot creation, so fc-agent
    // will reconnect after each snapshot. We must keep accepting new connections.
    loop {
        // Accept connection from fc-agent (no timeout - image import can take 10+ min)
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!(vm_id = %vm_id, error = %e, "Error accepting output connection");
                break;
            }
        };

        debug!(vm_id = %vm_id, "Output connection established");

        let mut reader = BufReader::new(stream);
        let mut line_buf = String::new();

        // Read lines until connection closes or snapshot triggers reconnect.
        // During snapshot, Firecracker resets vsock but the host-side Unix socket
        // stays open (no EOF). The reconnect_notify signals us to drop this
        // connection so fc-agent's new vsock connection can be accepted.
        loop {
            line_buf.clear();
            let read_result = tokio::select! {
                result = reader.read_line(&mut line_buf) => result,
                _ = reconnect_notify.notified() => {
                    info!(vm_id = %vm_id, "Snapshot reconnect signal, dropping old connection");
                    break;
                }
            };
            match read_result {
                Ok(0) => {
                    // EOF - connection closed (vsock reset from snapshot, or VM exit)
                    info!(vm_id = %vm_id, "Output connection closed, waiting for reconnect");
                    break;
                }
                Ok(_) => {
                    // Parse raw line format: stream:content
                    let line = line_buf.trim_end();
                    if let Some((stream, content)) = line.split_once(':') {
                        if stream == "heartbeat" {
                            // Heartbeat from fc-agent during long operations (image import/pull)
                            info!(vm_id = %vm_id, phase = %content, "VM heartbeat");
                        } else {
                            // Print container output directly (stdout to stdout, stderr to stderr)
                            // No prefix - clean output for scripting
                            if stream == "stdout" {
                                println!("{}", content);
                            } else {
                                eprintln!("{}", content);
                            }
                            output_lines.push((stream.to_string(), content.to_string()));
                        }

                        // Forward to broadcast channel for library consumers
                        if let Some(ref tx) = log_tx {
                            let _ = tx.send(LogLine {
                                stream: stream.to_string(),
                                content: content.to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    warn!(vm_id = %vm_id, error = %e, "Error reading output, waiting for reconnect");
                    break;
                }
            }
        }
    } // outer accept loop

    // Clean up
    let _ = std::fs::remove_file(socket_path);

    info!(vm_id = %vm_id, lines = output_lines.len(), "Output listener finished");
    Ok(output_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_status_listener_handles_multiple_messages_on_single_connection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let runtime_dir = temp_dir.path().to_path_buf();
        let socket_path = runtime_dir.join("status.sock");
        let socket_path_string = socket_path.to_string_lossy().to_string();
        let runtime_dir_for_task = runtime_dir.clone();

        let listener_task = tokio::spawn(async move {
            run_status_listener(&socket_path_string, &runtime_dir_for_task, "vm-test", None).await
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

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), listener_task)
            .await
            .expect("status listener timed out")
            .expect("status listener task panicked");
        result.expect("status listener returned error");

        assert_eq!(
            std::fs::read_to_string(runtime_dir.join("container-ready")).unwrap(),
            "ready\n"
        );
        assert_eq!(
            std::fs::read_to_string(runtime_dir.join("container-exit")).unwrap(),
            "0\n"
        );
    }
}
