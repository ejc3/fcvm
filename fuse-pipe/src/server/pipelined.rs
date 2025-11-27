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
use crate::protocol::{now_nanos, Span, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

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
        // Remove existing socket
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;
        eprintln!("[pipelined] listening on {}", socket_path);

        let mut client_id = 0u32;

        loop {
            let (stream, _) = listener.accept().await?;
            let handler = Arc::clone(&self.handler);
            let config = self.config.clone();
            let id = client_id;
            client_id += 1;

            eprintln!("[pipelined] client {} connected", id);

            tokio::spawn(async move {
                if let Err(e) = handle_client_pipelined(handler, stream, config, id).await {
                    eprintln!("[pipelined] client {} error: {}", id, e);
                }
                eprintln!("[pipelined] client {} disconnected", id);
            });
        }
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

    loop {
        // Read request length
        match read_half.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }

        // Mark server_recv as soon as we have the length header
        let t_recv = now_nanos();

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_SIZE {
            eprintln!("[pipelined] message too large: {}", len);
            return Err(anyhow::anyhow!("message too large: {}", len));
        }

        // Read request body
        let mut req_buf = vec![0u8; len];
        read_half.read_exact(&mut req_buf).await?;

        // Deserialize
        let wire_req: WireRequest = match bincode::deserialize(&req_buf) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[pipelined] deserialize error: {}", e);
                continue;
            }
        };

        // Mark deserialize done on span if present
        let t_deser = now_nanos();

        let unique = wire_req.unique;
        let reader_id = wire_req.reader_id;
        let request = wire_req.request;
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

            // Run FS operation on blocking thread
            let response =
                tokio::task::spawn_blocking(move || handler_clone.handle_request(&request))
                    .await
                    .unwrap_or_else(|_| VolumeResponse::error(libc::EIO));

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

        let PendingResponse { unique, reader_id, response, mut span } = item;

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
            Err(_) => continue,
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
