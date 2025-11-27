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
use crate::protocol::{VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

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

    // Channel for completed responses
    let (tx, rx) = mpsc::channel::<(u64, u32, VolumeResponse)>(config.response_channel_size);

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
    tx: mpsc::Sender<(u64, u32, VolumeResponse)>,
) -> anyhow::Result<()> {
    let mut len_buf = [0u8; 4];

    loop {
        // Read request length
        match read_half.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_SIZE {
            eprintln!("[pipelined] message too large: {}", len);
            continue;
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

        let unique = wire_req.unique;
        let reader_id = wire_req.reader_id;
        let request = wire_req.request;

        // Spawn handler task
        let handler_clone = Arc::clone(&handler);
        let tx_clone = tx.clone();

        tokio::spawn(async move {
            // Run FS operation on blocking thread
            let response = tokio::task::spawn_blocking(move || handler_clone.handle_request(&request))
                .await
                .unwrap_or_else(|_| VolumeResponse::error(libc::EIO));

            // Send response to writer
            let _ = tx_clone.send((unique, reader_id, response)).await;
        });
    }

    Ok(())
}

/// Write responses with batching.
async fn response_writer(
    write_half: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<(u64, u32, VolumeResponse)>,
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

        let (unique, reader_id, response) = item;
        let wire_resp = WireResponse::new(unique, reader_id, response);

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

        // Flush if batch is full
        if batch_count >= config.write_batch_size {
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
        let config = ServerConfig::high_throughput();
        let server = AsyncServer::with_config(handler, config);
        assert_eq!(server.config.write_batch_size, 64);
    }
}
