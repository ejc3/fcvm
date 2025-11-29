//! Lock-free socket multiplexer for sharing a single connection across FUSE readers.
//!
//! Uses crossbeam channels for lock-free request submission and DashMap
//! for lock-free response routing, eliminating mutex contention.

use crate::protocol::{
    Span, VolumeRequest, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use dashmap::DashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Response channel payload: (response, optional span for tracing)
type ResponsePayload = (VolumeResponse, Option<Span>);

/// A pending request with its response channel.
struct PendingRequest {
    /// Pre-serialized request bytes (length prefix + body)
    data: Vec<u8>,
    /// Channel to send response back to the waiting reader
    response_tx: Sender<ResponsePayload>,
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
}

impl Multiplexer {
    /// Create a new multiplexer with the given number of readers.
    ///
    /// Spawns background threads for reading and writing to the socket.
    /// Tracing is disabled by default.
    pub fn new(socket: UnixStream, num_readers: usize) -> Arc<Self> {
        Self::with_trace_rate(socket, num_readers, 0)
    }

    /// Create a new multiplexer with tracing enabled.
    ///
    /// `trace_rate`: Trace every Nth request (0 = disabled, 100 = every 100th request)
    pub fn with_trace_rate(socket: UnixStream, num_readers: usize, trace_rate: u64) -> Arc<Self> {
        let socket_reader = socket.try_clone().expect("failed to clone socket");
        let socket_writer = socket;

        // Clear timeouts - threads should block indefinitely
        socket_reader.set_read_timeout(None).ok();
        socket_writer.set_write_timeout(None).ok();

        // Channel for request submission (bounded to provide backpressure)
        let (request_tx, request_rx) = bounded::<PendingRequest>(num_readers * 4);

        // Lock-free map for routing responses back to waiting readers
        // Key: unique request ID, Value: oneshot sender for response
        let pending: Arc<DashMap<u64, Sender<ResponsePayload>>> =
            Arc::new(DashMap::with_capacity(num_readers * 2));

        let pending_for_writer = Arc::clone(&pending);
        let pending_for_reader = Arc::clone(&pending);

        // Spawn writer thread - receives requests from channel, writes to socket
        std::thread::spawn(move || {
            writer_loop(socket_writer, request_rx, pending_for_writer);
        });

        // Spawn reader thread - reads responses from socket, routes to waiting readers
        std::thread::spawn(move || {
            reader_loop(socket_reader, pending_for_reader);
        });

        Arc::new(Self {
            request_tx,
            next_id: AtomicU64::new(1),
            num_readers,
            trace_rate,
        })
    }

    /// Send a request and wait for response.
    ///
    /// This is called by reader threads. Uses lock-free channel for submission
    /// and per-request oneshot channel for response.
    pub fn send_request(&self, reader_id: u32, request: VolumeRequest) -> VolumeResponse {
        let unique = self.next_id.fetch_add(1, Ordering::Relaxed);
        let should_trace = self.trace_rate > 0 && unique % self.trace_rate == 0;

        // Build wire request - span goes inside the request so server gets it
        let wire = if should_trace {
            WireRequest::with_span(unique, reader_id, request, Span::new())
        } else {
            WireRequest::new(unique, reader_id, request)
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
            response_tx,
            unique,
        };

        if self.request_tx.send(pending).is_err() {
            return VolumeResponse::error(libc::EIO);
        }

        // Wait for response on our oneshot channel
        let (response, span) = response_rx
            .recv()
            .unwrap_or_else(|_| (VolumeResponse::error(libc::EIO), None));

        // Print trace if we have a complete span
        if let Some(mut s) = span {
            s.mark("client_done");
            s.print(unique);
        }

        response
    }

    /// Get the number of readers this multiplexer supports.
    pub fn num_readers(&self) -> usize {
        self.num_readers
    }
}

/// Writer thread: receives requests from channel, writes to socket.
fn writer_loop(
    mut socket: UnixStream,
    request_rx: Receiver<PendingRequest>,
    pending: Arc<DashMap<u64, Sender<ResponsePayload>>>,
) {
    while let Ok(req) = request_rx.recv() {
        // Register the response channel BEFORE writing (to avoid race)
        pending.insert(req.unique, req.response_tx);

        // Write to socket
        if socket.write_all(&req.data).is_err() || socket.flush().is_err() {
            // Remove from pending and signal error
            if let Some((_, tx)) = pending.remove(&req.unique) {
                let _ = tx.send((VolumeResponse::error(libc::EIO), None));
            }
        }
        // Note: client_socket_write is marked by server_recv on the server side
        // since we can't update the span after serialization
    }
}

/// Reader thread: reads responses from socket, routes to waiting readers.
fn reader_loop(mut socket: UnixStream, pending: Arc<DashMap<u64, Sender<ResponsePayload>>>) {
    let mut len_buf = [0u8; 4];

    loop {
        // Read response length
        if socket.read_exact(&mut len_buf).is_err() {
            // Server disconnected - fail all pending requests
            fail_all_pending(&pending);
            break;
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_SIZE {
            continue;
        }

        // Read response body
        let mut resp_buf = vec![0u8; len];
        if socket.read_exact(&mut resp_buf).is_err() {
            fail_all_pending(&pending);
            break;
        }

        // Deserialize and route to waiting reader (lock-free lookup + remove)
        if let Ok(wire) = bincode::deserialize::<WireResponse>(&resp_buf) {
            // Mark client receive time on the span
            let mut span = wire.span;
            if let Some(ref mut s) = span {
                s.mark("client_recv");
            }

            if let Some((_, tx)) = pending.remove(&wire.unique) {
                let _ = tx.send((wire.response, span));
            }
        }
    }
}

/// Fail all pending requests on disconnect.
fn fail_all_pending(pending: &DashMap<u64, Sender<ResponsePayload>>) {
    // Collect keys first to avoid holding shard locks during send
    let keys: Vec<u64> = pending.iter().map(|r| *r.key()).collect();
    for key in keys {
        if let Some((_, tx)) = pending.remove(&key) {
            let _ = tx.send((VolumeResponse::error(libc::EIO), None));
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
        let mux = Multiplexer::new(client, 1);
        let mux_clone = Arc::clone(&mux);

        let (done_tx, done_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let resp = mux_clone.send_request(0, VolumeRequest::Getattr { ino: 1 });
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
        let mux = Multiplexer::new(client, 2);
        let mux0 = Arc::clone(&mux);
        let mux1 = Arc::clone(&mux);

        let (tx0, rx0) = mpsc::channel();
        let (tx1, rx1) = mpsc::channel();

        std::thread::spawn(move || {
            let resp = mux0.send_request(0, VolumeRequest::Getattr { ino: 10 });
            let _ = tx0.send(resp);
        });

        std::thread::spawn(move || {
            let resp = mux1.send_request(
                1,
                VolumeRequest::Lookup {
                    parent: 1,
                    name: "file".into(),
                    uid: 1000,
                    gid: 1000,
                },
            );
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
}
