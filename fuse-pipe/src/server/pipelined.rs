//! Async pipelined server for high-throughput FUSE request handling.
//!
//! # Architecture
//!
//! - Single connection, multiple requests in flight (pipelining)
//! - Read requests in a loop, spawn task for each
//! - Write responses as they complete (out-of-order OK due to unique IDs)
//! - FS operations run on `spawn_blocking` to not block the runtime
//! - Large blocking thread pool for I/O-bound saturation
//! - Response batching for reduced syscall overhead

use super::{FilesystemHandler, ServerConfig};
use crate::protocol::{
    now_nanos, Span, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Response with optional tracing span
struct PendingResponse {
    unique: u64,
    reader_id: u32,
    response: VolumeResponse,
    span: Option<Span>,
}

/// Async pipelined server.
pub struct AsyncServer<H> {
    handler: Arc<H>,
    config: ServerConfig,
}

impl<H: FilesystemHandler + 'static> AsyncServer<H> {
    /// Create a new server with default configuration.
    pub fn new(handler: H) -> Self {
        Self::with_config(handler, ServerConfig::default())
    }

    /// Create a new server with custom configuration.
    pub fn with_config(handler: H, config: ServerConfig) -> Self {
        Self {
            handler: Arc::new(handler),
            config,
        }
    }

    /// Serve on a Unix socket.
    ///
    /// This function blocks forever, accepting and handling connections.
    pub async fn serve_unix(self, socket_path: &str) -> anyhow::Result<()> {
        self.serve_unix_with_ready_signal(socket_path, None).await
    }

    /// Serve on a Unix socket with an optional ready signal.
    ///
    /// The ready signal (oneshot channel) is sent after the socket is bound and before
    /// entering the accept loop. This allows callers to synchronize on server readiness
    /// instead of using arbitrary sleeps.
    ///
    /// # Arguments
    /// * `socket_path` - Path for the Unix socket
    /// * `ready` - Optional oneshot sender that will be notified when the socket is bound
    pub async fn serve_unix_with_ready_signal(
        self,
        socket_path: &str,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> anyhow::Result<()> {
        // Remove existing socket
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;

        // Make socket accessible by Firecracker running in user namespace (UID 100000)
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o777))?;

        // Signal ready BEFORE entering accept loop
        if let Some(tx) = ready {
            let _ = tx.send(());
        }

        info!(target: "fuse-pipe::server", socket_path, "listening");

        let mut client_id = 0u32;

        loop {
            let (stream, _) = listener.accept().await?;
            let handler = Arc::clone(&self.handler);
            let config = self.config.clone();
            let id = client_id;
            client_id += 1;

            info!(target: "fuse-pipe::server", client_id = id, "client connected");

            tokio::spawn(async move {
                if let Err(e) = handle_client_pipelined(handler, stream, config, id).await {
                    error!(target: "fuse-pipe::server", client_id = id, error = %e, "client error");
                }
                debug!(target: "fuse-pipe::server", client_id = id, "client disconnected");
            });
        }
    }

    /// Serve on Firecracker's vsock-forwarded Unix socket.
    ///
    /// Firecracker implements vsock by forwarding guest connections to Unix sockets
    /// named `{uds_path}_{port}`. This method binds to that path pattern.
    ///
    /// # Arguments
    ///
    /// * `uds_base_path` - Base path for vsock sockets (e.g., `/tmp/vm/vsock.sock`)
    /// * `port` - The vsock port number (e.g., 5000)
    ///
    /// The server will listen on `{uds_base_path}_{port}` (e.g., `/tmp/vm/vsock.sock_5000`).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let server = AsyncServer::new(PassthroughFs::new("/data"));
    /// server.serve_vsock_forwarded("/tmp/vm/vsock.sock", 5000).await?;
    /// // Listens on /tmp/vm/vsock.sock_5000
    /// ```
    pub async fn serve_vsock_forwarded(self, uds_base_path: &str, port: u32) -> anyhow::Result<()> {
        self.serve_vsock_forwarded_with_ready_signal(uds_base_path, port, None)
            .await
    }

    /// Serve on Firecracker's vsock-forwarded Unix socket with ready signal.
    ///
    /// Same as `serve_vsock_forwarded` but signals readiness after binding.
    pub async fn serve_vsock_forwarded_with_ready_signal(
        self,
        uds_base_path: &str,
        port: u32,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> anyhow::Result<()> {
        let socket_path = format!("{}_{}", uds_base_path, port);
        info!(target: "fuse-pipe::server", uds_base_path, port, socket_path = %socket_path, "serving vsock-forwarded");
        self.serve_unix_with_ready_signal(&socket_path, ready).await
    }

    /// Run the server with a tuned tokio runtime.
    ///
    /// This creates a new runtime optimized for the server configuration.
    pub fn run_blocking(self, socket_path: &str) -> anyhow::Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .max_blocking_threads(self.config.max_blocking_threads)
            .thread_keep_alive(self.config.thread_keep_alive)
            .build()?
            .block_on(self.serve_unix(socket_path))
    }
}

/// Handle a single client connection with pipelining.
async fn handle_client_pipelined<H: FilesystemHandler + 'static>(
    handler: Arc<H>,
    stream: tokio::net::UnixStream,
    config: ServerConfig,
    _client_id: u32,
) -> anyhow::Result<()> {
    let (read_half, write_half) = stream.into_split();

    // Channel for completed responses (includes optional span for tracing)
    let (tx, rx) = mpsc::channel::<PendingResponse>(config.response_channel_size);

    // Spawn writer task
    let writer_config = config.clone();
    let writer_handle = tokio::spawn(response_writer(write_half, rx, writer_config));

    // Run reader in current task
    let reader_result = request_reader(read_half, handler, tx).await;

    // Wait for writer to finish
    let _ = writer_handle.await;

    reader_result
}

/// Read requests and spawn handler tasks for each.
async fn request_reader<H: FilesystemHandler + 'static>(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    handler: Arc<H>,
    tx: mpsc::Sender<PendingResponse>,
) -> anyhow::Result<()> {
    let mut len_buf = [0u8; 4];
    let mut count = 0u64;
    let mut last_len: usize = 0;
    let mut min_len: usize = usize::MAX;
    let mut max_len: usize = 0;
    let mut total_bytes_read: u64 = 0; // Track cumulative bytes for corruption debugging
    let mut last_unique: u64 = 0; // Track last successful unique ID
    let mut expected_unique: u64 = 1; // Track expected sequence number
    let mut zero_byte_runs: u64 = 0; // Track consecutive zero bytes seen (for corruption detection)

    loop {
        // Read request length
        match read_half.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::debug!(target: "fuse-pipe::server", count, total_bytes_read, "client disconnected");
                break;
            }
            Err(e) => return Err(e.into()),
        }

        total_bytes_read += 4;
        count += 1;

        // Mark server_recv as soon as we have the length header
        let t_recv = now_nanos();

        let len = u32::from_be_bytes(len_buf) as usize;

        // Track length statistics for debugging
        min_len = min_len.min(len);
        max_len = max_len.max(len);

        // Track zero bytes for corruption pattern analysis
        let all_zeros = len_buf.iter().all(|&b| b == 0);
        if all_zeros {
            zero_byte_runs += 1;
        } else {
            zero_byte_runs = 0;
        }

        // CRITICAL: Detect zero-length messages - this is the corruption pattern we're seeing
        if len == 0 {
            error!(
                target: "fuse-pipe::server",
                count,
                total_bytes_read,
                last_len,
                last_unique,
                expected_unique,
                zero_byte_runs,
                len_hex = format!("{:02x} {:02x} {:02x} {:02x}", len_buf[0], len_buf[1], len_buf[2], len_buf[3]),
                "STREAM CORRUPTION: zero-length message detected"
            );

            // Try to scan ahead for a non-zero byte to understand corruption extent
            let mut scan_buf = [0u8; 4096];
            let mut zeros_scanned = 0u64;
            let mut found_nonzero = false;

            loop {
                match read_half.read(&mut scan_buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        // Count leading zeros and find first non-zero
                        for (i, &b) in scan_buf[..n].iter().enumerate() {
                            if b != 0 {
                                // Found non-zero byte - dump context
                                let context_start = i.saturating_sub(16);
                                let context_end = (i + 48).min(n);
                                let hex_dump: String = scan_buf[context_start..context_end]
                                    .iter()
                                    .map(|x| format!("{:02x}", x))
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                error!(
                                    target: "fuse-pipe::server",
                                    zeros_scanned = zeros_scanned + i as u64,
                                    first_nonzero_byte = format!("{:02x}", b),
                                    context_offset = i - context_start,
                                    hex = %hex_dump,
                                    "CORRUPTION EXTENT: found non-zero after zeros"
                                );
                                found_nonzero = true;
                                break;
                            }
                        }
                        if found_nonzero {
                            break;
                        }
                        zeros_scanned += n as u64;
                        // Stop scanning after 1MB of zeros
                        if zeros_scanned > 1024 * 1024 {
                            error!(
                                target: "fuse-pipe::server",
                                zeros_scanned,
                                "CORRUPTION EXTENT: >1MB of zeros, stopping scan"
                            );
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            return Err(anyhow::anyhow!(
                "Stream corruption: zero-length message at count={} after {} bytes, {} zeros scanned",
                count,
                total_bytes_read,
                zeros_scanned
            ));
        }

        if count <= 10 || count.is_multiple_of(100) {
            tracing::info!(target: "fuse-pipe::server", count, len, min_len, max_len, total_bytes_read, "server: received requests");
        }

        if len > MAX_MESSAGE_SIZE {
            warn!(target: "fuse-pipe::server", len, max = MAX_MESSAGE_SIZE, total_bytes_read, "message too large");
            return Err(anyhow::anyhow!("message too large: {}", len));
        }

        // Sanity check: typical FUSE requests should be < 1MB
        // Log anomalies that might indicate corruption
        if len < 12 {
            // WireRequest minimum size: unique(8) + reader_id(4) = 12 bytes
            error!(
                target: "fuse-pipe::server",
                count,
                len,
                total_bytes_read,
                len_hex = format!("{:02x} {:02x} {:02x} {:02x}", len_buf[0], len_buf[1], len_buf[2], len_buf[3]),
                last_len,
                last_unique,
                "SUSPICIOUS: message too small (min expected 12 bytes)"
            );
        }

        last_len = len;

        // Read request body
        let mut req_buf = vec![0u8; len];
        read_half.read_exact(&mut req_buf).await?;
        total_bytes_read += len as u64;

        // Deserialize
        let wire_req: WireRequest = match bincode::deserialize(&req_buf) {
            Ok(r) => r,
            Err(e) => {
                // Log detailed diagnostics for stream corruption debugging
                let first_64: Vec<u8> = req_buf.iter().take(64).copied().collect();
                let hex_dump: String = first_64
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ascii_dump: String = first_64
                    .iter()
                    .map(|b| {
                        if *b >= 0x20 && *b < 0x7f {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();

                // Try to extract what looks like a unique ID from the corrupted data
                let maybe_unique = if req_buf.len() >= 8 {
                    u64::from_le_bytes([
                        req_buf[0], req_buf[1], req_buf[2], req_buf[3], req_buf[4], req_buf[5],
                        req_buf[6], req_buf[7],
                    ])
                } else {
                    0
                };

                error!(
                    target: "fuse-pipe::server",
                    count,
                    len,
                    total_bytes_read,
                    last_len,
                    last_unique,
                    maybe_unique,
                    error = %e,
                    hex = %hex_dump,
                    ascii = %ascii_dump,
                    "DESERIALIZE FAILED - raw bytes dumped"
                );
                continue;
            }
        };

        // Mark deserialize done on span if present
        let t_deser = now_nanos();

        let unique = wire_req.unique;
        last_unique = unique; // Track for corruption debugging

        // Validate sequence: unique IDs should be monotonically increasing
        // This helps detect both dropped messages and stream corruption
        if unique != expected_unique {
            if unique > expected_unique {
                // Gap - messages might have been dropped
                warn!(
                    target: "fuse-pipe::server",
                    count,
                    expected = expected_unique,
                    actual = unique,
                    gap = unique - expected_unique,
                    total_bytes_read,
                    "SEQUENCE GAP: missing {} message(s)",
                    unique - expected_unique
                );
            } else {
                // Out of order or duplicate - could indicate corruption
                error!(
                    target: "fuse-pipe::server",
                    count,
                    expected = expected_unique,
                    actual = unique,
                    last_unique,
                    total_bytes_read,
                    "SEQUENCE ERROR: out-of-order or duplicate unique ID"
                );
            }
        }
        expected_unique = unique + 1;
        let reader_id = wire_req.reader_id;
        let request = wire_req.request;
        let supplementary_groups = wire_req.supplementary_groups;
        let mut span = wire_req.span;

        // Update span with server timing
        if let Some(ref mut s) = span {
            s.server_recv = t_recv;
            s.server_deser = t_deser;
        }

        // Spawn handler task
        let handler_clone = Arc::clone(&handler);
        let tx_clone = tx.clone();

        tokio::spawn(async move {
            // Mark when spawn_blocking task actually starts running
            let mut span = span;
            if let Some(ref mut s) = span {
                s.server_spawn = now_nanos();
            }

            // Run FS operation on blocking thread with supplementary groups
            let response = tokio::task::spawn_blocking(move || {
                handler_clone.handle_request_with_groups(&request, &supplementary_groups)
            })
            .await
            .unwrap_or_else(|e| {
                error!(target: "fuse-pipe::server", unique, "handler task panicked: {:?}", e);
                VolumeResponse::error(libc::EIO)
            });

            // Mark fs operation done
            if let Some(ref mut s) = span {
                s.server_fs_done = now_nanos();
            }

            // Send response to writer
            let pending = PendingResponse {
                unique,
                reader_id,
                response,
                span,
            };
            let _ = tx_clone.send(pending).await;
        });
    }

    Ok(())
}

/// Write responses with batching.
async fn response_writer(
    write_half: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<PendingResponse>,
    config: ServerConfig,
) {
    // Use buffered writer for batching
    let mut writer = BufWriter::with_capacity(config.write_buffer_size, write_half);
    let mut batch_count = 0;
    let batch_timeout = config.write_batch_timeout;

    loop {
        // Try to receive with timeout for batching
        let recv_result = if batch_count > 0 {
            tokio::time::timeout(batch_timeout, rx.recv()).await
        } else {
            Ok(rx.recv().await)
        };

        let item = match recv_result {
            Ok(Some(item)) => item,
            Ok(None) => break, // Channel closed
            Err(_) => {
                // Timeout - flush what we have
                if writer.flush().await.is_err() {
                    break;
                }
                batch_count = 0;
                continue;
            }
        };

        let PendingResponse {
            unique,
            reader_id,
            response,
            mut span,
        } = item;

        // Mark channel receive time (last mark before we serialize the response)
        if let Some(ref mut s) = span {
            s.server_resp_chan = now_nanos();
        }

        // Build wire response with span (span is cloned/moved into response here)
        let wire_resp = match span {
            Some(s) => WireResponse::with_span(unique, reader_id, response, s),
            None => WireResponse::new(unique, reader_id, response),
        };

        let resp_buf = match bincode::serialize(&wire_resp) {
            Ok(b) => b,
            Err(e) => {
                error!(target: "fuse-pipe::server", unique, "response serialization failed: {}", e);
                continue;
            }
        };

        let resp_len = (resp_buf.len() as u32).to_be_bytes();

        // Write length + body to buffer
        if writer.write_all(&resp_len).await.is_err() {
            break;
        }
        if writer.write_all(&resp_buf).await.is_err() {
            break;
        }

        batch_count += 1;

        // Check if channel is empty - if so, flush immediately (adaptive batching)
        let channel_empty = rx.is_empty();

        // Flush if batch is full OR channel is empty (no point waiting)
        if batch_count >= config.write_batch_size || channel_empty {
            if writer.flush().await.is_err() {
                break;
            }
            batch_count = 0;
        }
    }

    // Final flush
    let _ = writer.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::handler::FilesystemHandler;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    struct EchoHandler;

    impl FilesystemHandler for EchoHandler {
        fn getattr(&self, ino: u64) -> VolumeResponse {
            VolumeResponse::Attr {
                attr: crate::FileAttr::new(ino),
                ttl_secs: 1,
            }
        }
    }

    #[test]
    fn test_server_creation() {
        let handler = EchoHandler;
        let _server = AsyncServer::new(handler);
    }

    #[test]
    fn test_server_with_config() {
        let handler = EchoHandler;
        let config = ServerConfig::default();
        let server = AsyncServer::with_config(handler, config);
        assert_eq!(server.config.write_batch_size, 64);
    }

    #[tokio::test]
    async fn test_request_reader_exits_on_oversized_frame() {
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let (read_half, _write_half) = server.into_split();

        let handler = Arc::new(EchoHandler);
        let (tx, _rx) = mpsc::channel::<PendingResponse>(1);

        let reader_task = tokio::spawn(request_reader(read_half, handler, tx));

        // Write an oversized length and keep the connection open to surface hangs.
        let oversized_len = ((MAX_MESSAGE_SIZE as u32) + 1).to_be_bytes();
        client.write_all(&oversized_len).await.unwrap();

        let result = tokio::time::timeout(Duration::from_millis(200), reader_task).await;
        assert!(
            result.is_ok(),
            "request_reader did not exit after receiving an oversized frame"
        );
    }
}
