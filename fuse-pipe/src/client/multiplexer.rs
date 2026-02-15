//! Lock-free socket multiplexer for sharing a single connection across FUSE readers.
//!
//! Uses crossbeam channels for lock-free request submission and DashMap
//! for lock-free response routing, eliminating mutex contention.
//!
//! Supports transparent reconnection: when the underlying socket dies (e.g.,
//! vsock reset after snapshot), pending requests are re-sent on the new socket
//! without the FUSE session or kernel knowing anything happened.

use crate::protocol::{
    Span, VolumeRequest, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE,
};
use crate::telemetry::SpanCollector;
use crossbeam_channel::{bounded, Receiver, Sender};
use dashmap::DashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Response channel payload: (response, optional span for tracing)
type ResponsePayload = (VolumeResponse, Option<Span>);

/// Entry in the pending requests map.
/// Stores both the serialized request data (for re-send on reconnection)
/// and the response channel (to deliver the response to the waiting caller).
struct PendingEntry {
    /// Serialized request bytes (length prefix + body) for re-sending
    data: Vec<u8>,
    /// Channel to send response back to the waiting reader
    response_tx: Sender<ResponsePayload>,
}

/// A pending request with its response channel.
struct PendingRequest {
    /// Pre-serialized request bytes (length prefix + body)
    data: Vec<u8>,
    /// Channel to send response back to the waiting reader.
    /// None for fire-and-forget requests (e.g., forget/batch_forget).
    response_tx: Option<Sender<ResponsePayload>>,
    /// Unique request ID for response routing
    unique: u64,
}

/// Shared multiplexer for all reader threads.
///
/// Uses lock-free channels:
/// - Readers submit requests via crossbeam channel (no mutex)
/// - Dedicated writer thread sends requests to socket
/// - Dedicated reader thread receives responses
/// - Per-request oneshot channel delivers response back to reader
pub struct Multiplexer {
    /// Channel for submitting requests (lock-free)
    request_tx: Sender<PendingRequest>,
    /// Global request ID counter (atomic)
    next_id: AtomicU64,
    /// Number of readers
    num_readers: usize,
    /// Trace every Nth request (0 = disabled)
    trace_rate: u64,
    /// Optional span collector for telemetry aggregation
    collector: Option<SpanCollector>,
}

impl Multiplexer {
    /// Create a new multiplexer with the given number of readers.
    ///
    /// Spawns background threads for reading and writing to the socket.
    /// Tracing is disabled by default.
    ///
    /// # Errors
    /// Returns an error if the socket cannot be cloned.
    pub fn new(socket: UnixStream, num_readers: usize) -> std::io::Result<Arc<Self>> {
        Self::with_trace_rate(socket, num_readers, 0)
    }

    /// Create a new multiplexer with tracing enabled.
    ///
    /// `trace_rate`: Trace every Nth request (0 = disabled, 100 = every 100th request)
    ///
    /// # Errors
    /// Returns an error if the socket cannot be cloned.
    pub fn with_trace_rate(
        socket: UnixStream,
        num_readers: usize,
        trace_rate: u64,
    ) -> std::io::Result<Arc<Self>> {
        Self::with_collector(socket, num_readers, trace_rate, None)
    }

    /// Create a new multiplexer with a span collector for telemetry aggregation.
    ///
    /// `trace_rate`: Trace every Nth request (0 = disabled, 100 = every 100th request)
    /// `collector`: Optional SpanCollector to aggregate spans (instead of printing each)
    ///
    /// # Errors
    /// Returns an error if the socket cannot be cloned.
    pub fn with_collector(
        socket: UnixStream,
        num_readers: usize,
        trace_rate: u64,
        collector: Option<SpanCollector>,
    ) -> std::io::Result<Arc<Self>> {
        let socket_reader = socket.try_clone()?;
        let socket_writer = socket;

        // Clear timeouts - threads should block indefinitely
        socket_reader.set_read_timeout(None).ok();
        socket_writer.set_write_timeout(None).ok();

        // Channel for request submission (bounded to provide backpressure)
        let (request_tx, request_rx) = bounded::<PendingRequest>(num_readers * 4);

        // Lock-free map for routing responses back to waiting readers
        let pending: Arc<DashMap<u64, PendingEntry>> =
            Arc::new(DashMap::with_capacity(num_readers * 2));

        let pending_for_writer = Arc::clone(&pending);
        let pending_for_reader = Arc::clone(&pending);

        // Spawn writer thread - receives requests from channel, writes to socket
        std::thread::Builder::new()
            .name("fuse-mux-writer".to_string())
            .stack_size(512 * 1024) // 512KB - sufficient for socket I/O
            .spawn(move || {
                writer_loop(socket_writer, request_rx, pending_for_writer);
            })
            .expect("failed to spawn fuse mux writer thread");

        // Spawn reader thread - reads responses from socket, routes to waiting readers
        std::thread::Builder::new()
            .name("fuse-mux-reader".to_string())
            .stack_size(512 * 1024) // 512KB - sufficient for socket I/O
            .spawn(move || {
                reader_loop(socket_reader, pending_for_reader);
            })
            .expect("failed to spawn fuse mux reader thread");

        Ok(Arc::new(Self {
            request_tx,
            next_id: AtomicU64::new(1),
            num_readers,
            trace_rate,
            collector,
        }))
    }

    /// Create a reconnectable multiplexer.
    ///
    /// Like `with_collector`, but supports transparent reconnection when the
    /// underlying socket dies. When the socket fails, the writer thread calls
    /// `reconnect_fn` in a retry loop to obtain a new socket, spawns a new
    /// reader, and re-sends all pending requests.
    ///
    /// This keeps the FUSE session alive across socket reconnections — the
    /// kernel never knows anything happened. Callers waiting on responses
    /// simply experience a brief delay, not an error.
    ///
    /// # Arguments
    ///
    /// * `reconnect_fn` - Called to create a new socket when the current one dies.
    ///   Retried with exponential backoff (500ms to 5s) until successful.
    pub fn new_reconnectable(
        socket: UnixStream,
        num_readers: usize,
        trace_rate: u64,
        reconnect_fn: Box<dyn Fn() -> std::io::Result<UnixStream> + Send>,
    ) -> std::io::Result<Arc<Self>> {
        let socket_reader = socket.try_clone()?;
        let socket_writer = socket;

        socket_reader.set_read_timeout(None).ok();
        socket_writer.set_write_timeout(None).ok();

        let (request_tx, request_rx) = bounded::<PendingRequest>(num_readers * 4);

        let pending: Arc<DashMap<u64, PendingEntry>> =
            Arc::new(DashMap::with_capacity(num_readers * 2));

        let pending_for_writer = Arc::clone(&pending);
        let pending_for_reader = Arc::clone(&pending);

        // Spawn reconnectable writer thread (manages reader lifecycle)
        std::thread::Builder::new()
            .name("fuse-mux-writer".to_string())
            .stack_size(512 * 1024)
            .spawn(move || {
                writer_loop_reconnectable(
                    socket_writer,
                    socket_reader,
                    request_rx,
                    pending_for_writer,
                    pending_for_reader,
                    reconnect_fn,
                );
            })
            .expect("failed to spawn fuse mux writer thread");

        Ok(Arc::new(Self {
            request_tx,
            next_id: AtomicU64::new(1),
            num_readers,
            trace_rate,
            collector: None,
        }))
    }

    /// Send a request and wait for response.
    ///
    /// This is called by reader threads. Uses lock-free channel for submission
    /// and per-request oneshot channel for response.
    pub fn send_request(&self, request: VolumeRequest) -> VolumeResponse {
        self.send_request_with_groups(request, Vec::new())
    }

    /// Send a request with supplementary groups and wait for response.
    ///
    /// The supplementary groups are forwarded to the server for proper permission checks.
    pub fn send_request_with_groups(
        &self,
        request: VolumeRequest,
        supplementary_groups: Vec<u32>,
    ) -> VolumeResponse {
        let unique = self.next_id.fetch_add(1, Ordering::Relaxed);
        let should_trace = self.trace_rate > 0 && unique.is_multiple_of(self.trace_rate);

        // Capture op_name before request is moved (needed for per-operation telemetry)
        let op_name = if should_trace {
            Some(request.op_name().to_string())
        } else {
            None
        };

        // Build wire request - span goes inside the request so server gets it
        // reader_id is set to 0 since routing is done by unique ID, not reader_id
        let wire = if should_trace {
            WireRequest::with_span_and_groups(
                unique,
                0, // reader_id not used for routing
                request,
                Span::new(),
                supplementary_groups,
            )
        } else {
            WireRequest::with_groups(unique, 0, request, supplementary_groups)
        };

        let body = match bincode::serialize(&wire) {
            Ok(b) => b,
            Err(_) => return VolumeResponse::error(libc::EIO),
        };

        // Note: client_serialize mark is set by the server as server_recv (time delta = network latency)
        // We can't mark client-side times since the span is already serialized

        // Prepare length-prefixed message
        let mut data = Vec::with_capacity(4 + body.len());
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(&body);

        // Create oneshot channel for response
        let (response_tx, response_rx) = bounded::<ResponsePayload>(1);

        // Submit request (lock-free via crossbeam channel)
        let pending = PendingRequest {
            data,
            response_tx: Some(response_tx),
            unique,
        };

        if self.request_tx.send(pending).is_err() {
            return VolumeResponse::error(libc::EIO);
        }

        // Wait for response on our oneshot channel
        let (response, span) = response_rx
            .recv()
            .unwrap_or_else(|_| (VolumeResponse::error(libc::EIO), None));

        // Handle trace span - either collect or print
        if let Some(mut s) = span {
            s.mark("client_done");
            if let Some(ref collector) = self.collector {
                // Collect span for later aggregation (with op_name for per-operation stats)
                let op = op_name.as_deref().unwrap_or("unknown");
                collector.record(unique, op, s);
            } else {
                // No collector - print trace directly
                let op = op_name.as_deref().unwrap_or("unknown");
                s.print(unique, op);
            }
        }

        response
    }

    /// Get the span collector, if one was configured.
    pub fn collector(&self) -> Option<&SpanCollector> {
        self.collector.as_ref()
    }

    /// Send a request without waiting for a response (fire-and-forget).
    ///
    /// Used for FUSE forget/batch_forget operations where the kernel does not
    /// expect a reply. The server must not send a response for these requests.
    pub fn send_request_no_reply(&self, request: VolumeRequest) {
        let unique = self.next_id.fetch_add(1, Ordering::Relaxed);

        let wire = WireRequest::with_groups(unique, 0, request, Vec::new());

        let body = match bincode::serialize(&wire) {
            Ok(b) => b,
            Err(_) => return,
        };

        let mut data = Vec::with_capacity(4 + body.len());
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(&body);

        let pending = PendingRequest {
            data,
            response_tx: None,
            unique,
        };

        let _ = self.request_tx.send(pending);
    }

    /// Get the number of readers this multiplexer supports.
    pub fn num_readers(&self) -> usize {
        self.num_readers
    }
}

/// Writer thread: receives requests from channel, writes to socket.
/// Non-reconnectable version: fails pending on write error and continues.
fn writer_loop(
    mut socket: UnixStream,
    request_rx: Receiver<PendingRequest>,
    pending: Arc<DashMap<u64, PendingEntry>>,
) {
    let mut count = 0u64;
    let mut total_bytes_written: u64 = 0;

    while let Ok(req) = request_rx.recv() {
        count += 1;
        let msg_len = req.data.len();

        if count <= 10 || count.is_multiple_of(100) {
            tracing::info!(
                target: "fuse-pipe::mux",
                count,
                unique = req.unique,
                msg_len,
                total_bytes_written,
                pending_count = pending.len(),
                "writer: sending request"
            );
        }

        // Validate the message structure before sending
        if req.data.len() < 4 {
            tracing::error!(
                target: "fuse-pipe::mux",
                unique = req.unique,
                data_len = req.data.len(),
                "writer: message too short (missing length prefix?)"
            );
            continue;
        }
        let len_prefix = u32::from_be_bytes([req.data[0], req.data[1], req.data[2], req.data[3]]);
        if len_prefix == 0 {
            tracing::error!(
                target: "fuse-pipe::mux",
                unique = req.unique,
                "writer: ZERO LENGTH PREFIX - corruption before send!"
            );
            continue;
        }
        if len_prefix as usize != req.data.len() - 4 {
            tracing::error!(
                target: "fuse-pipe::mux",
                unique = req.unique,
                len_prefix,
                actual_body_len = req.data.len() - 4,
                "writer: length prefix mismatch!"
            );
        }

        // Register the response channel BEFORE writing (to avoid race).
        // For fire-and-forget requests (forget/batch_forget), no channel is registered.
        if let Some(tx) = req.response_tx {
            pending.insert(
                req.unique,
                PendingEntry {
                    data: req.data.clone(),
                    response_tx: tx,
                },
            );
        }

        // Write to socket
        let write_result = socket.write_all(&req.data);
        let flush_result = if write_result.is_ok() {
            socket.flush()
        } else {
            Ok(())
        };
        if let Err(e) = write_result.as_ref().and(flush_result.as_ref()) {
            tracing::warn!(
                target: "fuse-pipe::mux",
                unique = req.unique,
                msg_len,
                total_bytes_written,
                error = %e,
                error_kind = ?e.kind(),
                "writer: socket write failed"
            );
            // Remove from pending and signal error
            if let Some((_, entry)) = pending.remove(&req.unique) {
                let _ = entry
                    .response_tx
                    .send((VolumeResponse::error(libc::EIO), None));
            }
        } else {
            total_bytes_written += msg_len as u64;
        }
    }
    tracing::info!(target: "fuse-pipe::mux", count, total_bytes_written, "writer: exiting");
}

/// Writer thread with reconnection support.
///
/// Uses `crossbeam_channel::select!` to wake on either:
/// - A new request from the request channel
/// - A reader-death notification (socket died while writer was idle)
///
/// When reconnection is needed (write failure or reader death), this thread:
/// 1. Calls `reconnect_fn` in a retry loop with backoff to get a new socket
/// 2. Spawns a new reader thread for the new socket
/// 3. Re-sends all pending requests on the new socket
/// 4. Resumes normal operation
///
/// This keeps the FUSE session alive — callers waiting on responses
/// simply experience a delay, not an error.
fn writer_loop_reconnectable(
    mut writer_socket: UnixStream,
    reader_socket: UnixStream,
    request_rx: Receiver<PendingRequest>,
    pending: Arc<DashMap<u64, PendingEntry>>,
    pending_for_initial_reader: Arc<DashMap<u64, PendingEntry>>,
    reconnect_fn: Box<dyn Fn() -> std::io::Result<UnixStream> + Send>,
) {
    let mut count = 0u64;
    let mut total_bytes_written: u64 = 0;

    // Channel for reader-death notifications. When the reader thread exits
    // (socket EOF/error), it sends () to wake the writer for reconnection.
    let (reader_died_tx, reader_died_rx) = bounded::<()>(1);

    // Spawn initial reader thread
    let initial_died_tx = reader_died_tx.clone();
    std::thread::Builder::new()
        .name("fuse-mux-reader".to_string())
        .stack_size(512 * 1024)
        .spawn(move || {
            reader_loop_reconnectable(reader_socket, pending_for_initial_reader, initial_died_tx);
        })
        .expect("failed to spawn fuse mux reader thread");

    loop {
        // Wait for either a new request or reader death
        crossbeam_channel::select! {
            recv(request_rx) -> msg => {
                let req = match msg {
                    Ok(r) => r,
                    Err(_) => break, // request channel closed — shutdown
                };

                count += 1;
                let msg_len = req.data.len();

                if count <= 10 || count.is_multiple_of(100) {
                    tracing::info!(
                        target: "fuse-pipe::mux",
                        count,
                        unique = req.unique,
                        msg_len,
                        total_bytes_written,
                        pending_count = pending.len(),
                        "writer[reconnectable]: sending request"
                    );
                }

                // Validate message structure
                if req.data.len() < 4 {
                    continue;
                }
                let len_prefix = u32::from_be_bytes([req.data[0], req.data[1], req.data[2], req.data[3]]);
                if len_prefix == 0 {
                    continue;
                }

                // Register in pending BEFORE writing (stores data for re-send)
                if let Some(tx) = req.response_tx {
                    pending.insert(
                        req.unique,
                        PendingEntry {
                            data: req.data.clone(),
                            response_tx: tx,
                        },
                    );
                }

                // Try to write
                let write_ok = writer_socket
                    .write_all(&req.data)
                    .and_then(|_| writer_socket.flush())
                    .is_ok();

                if write_ok {
                    total_bytes_written += msg_len as u64;
                    continue;
                }

                // Write failed — socket is dead. Reconnect.
                tracing::warn!(
                    target: "fuse-pipe::mux",
                    unique = req.unique,
                    total_bytes_written,
                    pending_count = pending.len(),
                    "writer[reconnectable]: socket write failed, reconnecting"
                );

                do_reconnect(
                    &mut writer_socket,
                    &mut total_bytes_written,
                    &pending,
                    &reconnect_fn,
                    &reader_died_tx,
                );
            },
            recv(reader_died_rx) -> _ => {
                // Reader detected socket death while writer was idle.
                // If there are pending requests, reconnect now so they get responses.
                if pending.is_empty() {
                    // No pending requests — reconnect lazily on next write attempt.
                    // But mark the socket as dead so next write triggers reconnect.
                    tracing::info!(
                        target: "fuse-pipe::mux",
                        "writer[reconnectable]: reader died, no pending requests, reconnecting proactively"
                    );
                }

                if !pending.is_empty() {
                    tracing::warn!(
                        target: "fuse-pipe::mux",
                        pending_count = pending.len(),
                        "writer[reconnectable]: reader died with pending requests, reconnecting"
                    );
                }

                do_reconnect(
                    &mut writer_socket,
                    &mut total_bytes_written,
                    &pending,
                    &reconnect_fn,
                    &reader_died_tx,
                );
            },
        }
    }
    tracing::info!(target: "fuse-pipe::mux", count, total_bytes_written, "writer[reconnectable]: exiting");
}

/// Perform reconnection: get new socket, spawn new reader, re-send pending.
fn do_reconnect(
    writer_socket: &mut UnixStream,
    total_bytes_written: &mut u64,
    pending: &Arc<DashMap<u64, PendingEntry>>,
    reconnect_fn: &dyn Fn() -> std::io::Result<UnixStream>,
    reader_died_tx: &Sender<()>,
) {
    // Retry reconnect_fn with exponential backoff, capped at ~3 minutes total
    let mut backoff_ms = 500u64;
    let max_backoff_ms = 5000u64;
    let max_attempts = 60u32;
    let mut attempt = 0u32;
    let new_socket = loop {
        match (reconnect_fn)() {
            Ok(s) => break s,
            Err(e) => {
                attempt += 1;
                if attempt >= max_attempts {
                    tracing::error!(
                        target: "fuse-pipe::mux",
                        error = %e,
                        attempt,
                        max_attempts,
                        "writer[reconnectable]: reconnect failed after max attempts, failing pending requests"
                    );
                    fail_all_pending(pending);
                    return;
                }
                tracing::warn!(
                    target: "fuse-pipe::mux",
                    error = %e,
                    attempt,
                    max_attempts,
                    backoff_ms,
                    "writer[reconnectable]: reconnect attempt failed, retrying"
                );
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
            }
        }
    };

    tracing::info!(
        target: "fuse-pipe::mux",
        pending_count = pending.len(),
        "writer[reconnectable]: reconnected, setting up new socket"
    );

    // Set up new socket pair
    let new_reader_socket = match new_socket.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "fuse-pipe::mux", error = %e, "writer[reconnectable]: failed to clone new socket");
            fail_all_pending(pending);
            return;
        }
    };
    *writer_socket = new_socket;
    writer_socket.set_write_timeout(None).ok();
    new_reader_socket.set_read_timeout(None).ok();

    // Spawn new reader thread for the new socket
    let pending_for_reader = Arc::clone(pending);
    let died_tx = reader_died_tx.clone();
    std::thread::Builder::new()
        .name("fuse-mux-reader".to_string())
        .stack_size(512 * 1024)
        .spawn(move || {
            reader_loop_reconnectable(new_reader_socket, pending_for_reader, died_tx);
        })
        .expect("failed to spawn fuse mux reader thread");

    // Re-send all pending requests on the new socket.
    //
    // For baselines (reconnecting to the SAME VolumeServer):
    //   Safe — same PassthroughFs, stable inode numbers, valid file handles.
    //
    // For clones (reconnecting to a NEW VolumeServer with fresh PassthroughFs):
    //   Pending requests may reference unknown inodes → server returns ENOENT.
    //   This is acceptable: pending requests at snapshot time are rare (usually 0),
    //   and the affected process can retry. All NEW operations after reconnect work
    //   because FUSE TTL (1s) expires during boot, forcing fresh LOOKUPs from root.
    let keys: Vec<u64> = pending.iter().map(|r| *r.key()).collect();
    let mut resent = 0;
    let mut resend_failed = 0;
    for key in &keys {
        if let Some(entry) = pending.get(key) {
            if writer_socket
                .write_all(&entry.data)
                .and_then(|_| writer_socket.flush())
                .is_err()
            {
                resend_failed += 1;
                tracing::error!(
                    target: "fuse-pipe::mux",
                    unique = key,
                    "writer[reconnectable]: re-send failed on new socket"
                );
            } else {
                resent += 1;
            }
        }
    }

    tracing::info!(
        target: "fuse-pipe::mux",
        resent,
        resend_failed,
        total_pending = keys.len(),
        "writer[reconnectable]: re-sent pending requests"
    );

    if resend_failed > 0 {
        tracing::error!(target: "fuse-pipe::mux", "writer[reconnectable]: new socket failed during re-send");
    }

    *total_bytes_written = 0;
}

/// Reader thread for reconnectable mode.
///
/// When the socket dies, notifies the writer via `died_tx` so it can
/// proactively reconnect (even if no new write is pending).
fn reader_loop_reconnectable(
    mut socket: UnixStream,
    pending: Arc<DashMap<u64, PendingEntry>>,
    died_tx: Sender<()>,
) {
    let mut len_buf = [0u8; 4];
    let mut count = 0u64;

    loop {
        if socket.read_exact(&mut len_buf).is_err() {
            tracing::warn!(target: "fuse-pipe::mux", count, pending_count = pending.len(), "reader[reconnectable]: socket read failed, notifying writer");
            let _ = died_tx.send(());
            break;
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_SIZE {
            tracing::error!(target: "fuse-pipe::mux", len, "reader[reconnectable]: oversized message");
            let _ = died_tx.send(());
            break;
        }

        let mut resp_buf = vec![0u8; len];
        if socket.read_exact(&mut resp_buf).is_err() {
            tracing::warn!(target: "fuse-pipe::mux", count, "reader[reconnectable]: failed to read response body");
            let _ = died_tx.send(());
            break;
        }

        count += 1;
        if count <= 10 || count.is_multiple_of(100) {
            tracing::info!(target: "fuse-pipe::mux", count, pending_count = pending.len(), "reader[reconnectable]: received response");
        }

        match bincode::deserialize::<WireResponse>(&resp_buf) {
            Ok(wire) => {
                let mut span = wire.span;
                if let Some(ref mut s) = span {
                    s.mark("client_recv");
                }
                if let Some((_, entry)) = pending.remove(&wire.unique) {
                    let _ = entry.response_tx.send((wire.response, span));
                }
            }
            Err(e) => {
                tracing::error!(target: "fuse-pipe::mux", count, len, error = %e, "reader[reconnectable]: deserialization failed");
                let _ = died_tx.send(());
                break;
            }
        }
    }
    tracing::info!(target: "fuse-pipe::mux", count, "reader[reconnectable]: exiting");
}

/// Reader thread: reads responses from socket, routes to waiting readers.
///
/// Used by non-reconnectable multiplexers. Calls fail_all_pending() when
/// the socket dies to unblock waiting callers with EIO.
fn reader_loop(mut socket: UnixStream, pending: Arc<DashMap<u64, PendingEntry>>) {
    let mut len_buf = [0u8; 4];
    let mut count = 0u64;

    loop {
        // Read response length
        if socket.read_exact(&mut len_buf).is_err() {
            tracing::warn!(target: "fuse-pipe::mux", count, pending_count = pending.len(), "reader: socket read failed, disconnected");
            fail_all_pending(&pending);
            break;
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_SIZE {
            tracing::error!(target: "fuse-pipe::mux", len, "reader: oversized message");
            fail_all_pending(&pending);
            break;
        }

        // Read response body
        let mut resp_buf = vec![0u8; len];
        if socket.read_exact(&mut resp_buf).is_err() {
            tracing::warn!(target: "fuse-pipe::mux", count, "reader: failed to read response body");
            fail_all_pending(&pending);
            break;
        }

        count += 1;
        if count <= 10 || count.is_multiple_of(100) {
            tracing::info!(target: "fuse-pipe::mux", count, pending_count = pending.len(), "reader: received responses");
        }

        // Deserialize and route to waiting reader (lock-free lookup + remove)
        match bincode::deserialize::<WireResponse>(&resp_buf) {
            Ok(wire) => {
                // Mark client receive time on the span
                let mut span = wire.span;
                if let Some(ref mut s) = span {
                    s.mark("client_recv");
                }

                if let Some((_, entry)) = pending.remove(&wire.unique) {
                    let _ = entry.response_tx.send((wire.response, span));
                }
            }
            Err(e) => {
                tracing::error!(
                    target: "fuse-pipe::mux",
                    count,
                    len,
                    error = %e,
                    "reader: response deserialization failed"
                );
                fail_all_pending(&pending);
                break;
            }
        }
    }
    tracing::info!(target: "fuse-pipe::mux", count, "reader: exiting");
}

/// Fail all pending requests on disconnect.
fn fail_all_pending(pending: &DashMap<u64, PendingEntry>) {
    // Collect keys first to avoid holding shard locks during send
    let keys: Vec<u64> = pending.iter().map(|r| *r.key()).collect();
    for key in keys {
        if let Some((_, entry)) = pending.remove(&key) {
            let _ = entry
                .response_tx
                .send((VolumeResponse::error(libc::EIO), None));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disconnect_wakes_pending_request() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let mux = Multiplexer::new(client, 1).unwrap();
        let mux_clone = Arc::clone(&mux);

        let (done_tx, done_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let resp = mux_clone.send_request(VolumeRequest::Getattr { ino: 1 });
            let _ = done_tx.send(resp.errno());
        });

        // Drain the request so it is fully sent before we drop the server side.
        let mut len_buf = [0u8; 4];
        server.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        server.read_exact(&mut body).unwrap();

        // Drop the server end to simulate a disconnect without a response.
        drop(server);

        // The client should not hang forever waiting for a response.
        let result = done_rx.recv_timeout(Duration::from_millis(200));
        assert!(
            result.is_ok(),
            "pending request was not completed after disconnect"
        );
    }

    #[test]
    fn test_routing_multiple_readers_out_of_order() {
        use crate::protocol::{VolumeRequest, VolumeResponse, WireRequest, WireResponse};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::Duration;

        let (client, mut server) = UnixStream::pair().unwrap();
        let mux = Multiplexer::new(client, 2).unwrap();
        let mux0 = Arc::clone(&mux);
        let mux1 = Arc::clone(&mux);

        let (tx0, rx0) = mpsc::channel();
        let (tx1, rx1) = mpsc::channel();

        std::thread::spawn(move || {
            let resp = mux0.send_request(VolumeRequest::Getattr { ino: 10 });
            let _ = tx0.send(resp);
        });

        std::thread::spawn(move || {
            let resp = mux1.send_request(VolumeRequest::Lookup {
                parent: 1,
                name: "file".into(),
                uid: 1000,
                gid: 1000,
                pid: 0,
            });
            let _ = tx1.send(resp);
        });

        // Collect the two requests from the wire
        let mut requests = Vec::new();
        for _ in 0..2 {
            let mut len_buf = [0u8; 4];
            server.read_exact(&mut len_buf).unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            server.read_exact(&mut buf).unwrap();
            let wire: WireRequest = bincode::deserialize(&buf).unwrap();
            requests.push((wire.unique, wire.reader_id));
        }

        // Respond out of order to ensure correct routing
        for (unique, reader_id) in requests.iter().rev() {
            let wire_resp = WireResponse::new(*unique, *reader_id, VolumeResponse::Ok);
            let body = bincode::serialize(&wire_resp).unwrap();
            let len = (body.len() as u32).to_be_bytes();
            server.write_all(&len).unwrap();
            server.write_all(&body).unwrap();
        }

        let r0 = rx0.recv_timeout(Duration::from_millis(500)).unwrap();
        let r1 = rx1.recv_timeout(Duration::from_millis(500)).unwrap();

        assert!(r0.is_ok());
        assert!(r1.is_ok());
    }

    #[test]
    fn test_oversized_response_fails_pending_request() {
        use crate::protocol::MAX_MESSAGE_SIZE;
        use std::sync::mpsc;
        use std::time::Duration;

        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let mux = Multiplexer::new(client, 1).unwrap();
        let mux_clone = Arc::clone(&mux);

        let (done_tx, done_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let resp = mux_clone.send_request(VolumeRequest::Getattr { ino: 1 });
            let _ = done_tx.send(resp.errno());
        });

        // Drain the outgoing request so the mux writer isn't blocked.
        let mut len_buf = [0u8; 4];
        server.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        server.read_exact(&mut body).unwrap();

        // Send an oversized frame to trigger disconnect/error handling.
        let bad_len = (MAX_MESSAGE_SIZE as u32 + 1).to_be_bytes();
        server.write_all(&bad_len).unwrap();

        // Pending request should complete with an error instead of hanging.
        let result = done_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(result, Some(libc::EIO));
    }

    #[test]
    fn test_deserialize_failure_fails_pending_request() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let mux = Multiplexer::new(client, 1).unwrap();
        let mux_clone = Arc::clone(&mux);

        let (done_tx, done_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let resp = mux_clone.send_request(VolumeRequest::Getattr { ino: 1 });
            let _ = done_tx.send(resp.errno());
        });

        // Drain outgoing request.
        let mut len_buf = [0u8; 4];
        server.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        server.read_exact(&mut body).unwrap();

        // Send syntactically-framed but semantically-invalid bincode payload.
        let bad_payload = vec![0xff; 16];
        let bad_len = (bad_payload.len() as u32).to_be_bytes();
        server.write_all(&bad_len).unwrap();
        server.write_all(&bad_payload).unwrap();

        let result = done_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(result, Some(libc::EIO));
    }

    #[test]
    fn test_reconnectable_resends_pending_on_reconnect() {
        use crate::protocol::{VolumeRequest, VolumeResponse, WireRequest, WireResponse};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::Duration;

        // Create initial socket pair
        let (client1, mut server1) = UnixStream::pair().unwrap();

        // Use a channel-based closure for test control:
        // the test sends a new socket when it's ready
        let (reconnect_tx, reconnect_rx) = mpsc::channel::<UnixStream>();
        let reconnect_fn = Box::new(move || {
            reconnect_rx.recv().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "reconnect channel closed")
            })
        });

        let mux = Multiplexer::new_reconnectable(client1, 1, 0, reconnect_fn).unwrap();
        let mux_clone = Arc::clone(&mux);

        let (done_tx, done_rx) = mpsc::channel();

        // Send a request
        std::thread::spawn(move || {
            let resp = mux_clone.send_request(VolumeRequest::Getattr { ino: 42 });
            let _ = done_tx.send(resp);
        });

        // Read the request on server1
        let mut len_buf = [0u8; 4];
        server1.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        server1.read_exact(&mut body).unwrap();
        let wire1: WireRequest = bincode::deserialize(&body).unwrap();

        // Kill server1 to simulate disconnect (don't respond)
        drop(server1);

        // Wait a bit for the writer to detect disconnect
        std::thread::sleep(Duration::from_millis(100));

        // Create new socket pair and send via reconnect closure
        let (client2, mut server2) = UnixStream::pair().unwrap();
        reconnect_tx.send(client2).unwrap();

        // The multiplexer should re-send the pending request on server2
        let mut len_buf2 = [0u8; 4];
        server2.read_exact(&mut len_buf2).unwrap();
        let len2 = u32::from_be_bytes(len_buf2) as usize;
        let mut body2 = vec![0u8; len2];
        server2.read_exact(&mut body2).unwrap();
        let wire2: WireRequest = bincode::deserialize(&body2).unwrap();

        // Same unique ID — it's a re-send of the same request
        assert_eq!(wire2.unique, wire1.unique);

        // Now respond on server2
        let wire_resp = WireResponse::new(wire2.unique, 0, VolumeResponse::Ok);
        let resp_body = bincode::serialize(&wire_resp).unwrap();
        let resp_len = (resp_body.len() as u32).to_be_bytes();
        server2.write_all(&resp_len).unwrap();
        server2.write_all(&resp_body).unwrap();

        // The caller should get the response (not EIO)
        let result = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            result.is_ok(),
            "expected Ok response after reconnect, got {:?}",
            result
        );
    }
}
