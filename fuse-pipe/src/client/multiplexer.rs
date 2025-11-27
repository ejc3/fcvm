//! Socket multiplexer for sharing a single connection across FUSE readers.
//!
//! Multiple FUSE reader threads can share one socket connection to the server.
//! Each reader has its own pending queue, and responses are routed back by
//! unique request ID.

use crate::protocol::{VolumeRequest, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Pending response slot for a single request.
struct PendingSlot {
    response: Option<VolumeResponse>,
    ready: bool,
}

/// Per-reader pending requests map.
struct ReaderPending {
    slots: Mutex<HashMap<u64, PendingSlot>>,
    condvar: Condvar,
}

impl ReaderPending {
    fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            condvar: Condvar::new(),
        }
    }

    /// Register a pending request and wait for response.
    fn wait_for(&self, unique: u64) -> VolumeResponse {
        let mut slots = self.slots.lock().unwrap();

        // Insert placeholder
        slots.insert(
            unique,
            PendingSlot {
                response: None,
                ready: false,
            },
        );

        // Wait for response
        loop {
            if let Some(slot) = slots.get(&unique) {
                if slot.ready {
                    let slot = slots.remove(&unique).unwrap();
                    return slot.response.unwrap();
                }
            }
            slots = self.condvar.wait(slots).unwrap();
        }
    }

    /// Complete a pending request with a response.
    fn complete(&self, unique: u64, response: VolumeResponse) {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&unique) {
            slot.response = Some(response);
            slot.ready = true;
        }
        self.condvar.notify_all();
    }

    /// Fail all pending requests (e.g., on disconnect).
    fn fail_all(&self, response: VolumeResponse) {
        let mut slots = self.slots.lock().unwrap();
        for slot in slots.values_mut() {
            if !slot.ready {
                slot.response = Some(response.clone());
                slot.ready = true;
            }
        }
        self.condvar.notify_all();
    }
}

/// Shared multiplexer for all reader threads.
///
/// Manages a single socket connection, with request/response routing
/// to multiple FUSE reader threads.
pub struct Multiplexer {
    /// Socket write half (protected by mutex for concurrent writers)
    socket_writer: Mutex<UnixStream>,
    /// Per-reader pending response maps
    reader_pending: Vec<Arc<ReaderPending>>,
    /// Global request ID counter
    next_id: AtomicU64,
}

impl Multiplexer {
    /// Create a new multiplexer with the given number of readers.
    ///
    /// Spawns a background thread to read responses from the socket.
    pub fn new(socket: UnixStream, num_readers: usize) -> Arc<Self> {
        let socket_clone = socket.try_clone().expect("failed to clone socket");

        // Clear timeouts on the reader socket - it should block indefinitely
        socket_clone.set_read_timeout(None).ok();

        let mut reader_pending = Vec::with_capacity(num_readers);
        for _ in 0..num_readers {
            reader_pending.push(Arc::new(ReaderPending::new()));
        }

        let mux = Arc::new(Self {
            socket_writer: Mutex::new(socket),
            reader_pending,
            next_id: AtomicU64::new(1),
        });

        // Spawn response reader thread
        let mux_clone = Arc::clone(&mux);
        std::thread::spawn(move || {
            eprintln!("[mux] response reader thread started");
            mux_clone.response_reader_loop(socket_clone);
            eprintln!("[mux] response reader thread exited");
        });

        mux
    }

    /// Background thread that reads responses and dispatches to waiting readers.
    fn response_reader_loop(&self, mut socket: UnixStream) {
        let mut len_buf = [0u8; 4];
        let mut response_count = 0u64;

        loop {
            // Read response length
            if let Err(e) = socket.read_exact(&mut len_buf) {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    eprintln!("[mux] server disconnected after {} responses", response_count);
                } else {
                    eprintln!("[mux] read error after {} responses: {}", response_count, e);
                }
                for pending in &self.reader_pending {
                    pending.fail_all(VolumeResponse::error(libc::EIO));
                }
                break;
            }
            response_count += 1;

            let len = u32::from_be_bytes(len_buf) as usize;
            if len > MAX_MESSAGE_SIZE {
                eprintln!("[mux] response too large: {}", len);
                continue;
            }

            // Read response body
            let mut resp_buf = vec![0u8; len];
            if let Err(e) = socket.read_exact(&mut resp_buf) {
                eprintln!("[mux] read body error: {}", e);
                for pending in &self.reader_pending {
                    pending.fail_all(VolumeResponse::error(libc::EIO));
                }
                break;
            }

            // Deserialize
            let wire: WireResponse = match bincode::deserialize(&resp_buf) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[mux] deserialize error: {}", e);
                    continue;
                }
            };

            // Dispatch to correct reader's pending queue
            let reader_id = wire.reader_id as usize;
            if reader_id < self.reader_pending.len() {
                self.reader_pending[reader_id].complete(wire.unique, wire.response);
            } else {
                eprintln!("[mux] invalid reader_id: {}", reader_id);
            }
        }
    }

    /// Send a request and wait for response.
    ///
    /// This is called by reader threads and blocks until the response arrives.
    pub fn send_request(&self, reader_id: u32, request: VolumeRequest) -> VolumeResponse {
        let unique = self.next_id.fetch_add(1, Ordering::Relaxed);

        // Log first 20 requests
        if unique <= 20 {
            eprintln!("[mux] request {} from reader {}: {:?}", unique, reader_id, std::mem::discriminant(&request));
        }

        // Serialize request
        let wire = WireRequest::new(unique, reader_id, request);
        let buf = match bincode::serialize(&wire) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[reader {}] serialize error: {}", reader_id, e);
                return VolumeResponse::error(libc::EIO);
            }
        };

        // Write to socket (mutex-protected)
        {
            let mut socket = self.socket_writer.lock().unwrap();
            let len_bytes = (buf.len() as u32).to_be_bytes();
            if let Err(e) = socket.write_all(&len_bytes) {
                eprintln!("[reader {}] write len error: {}", reader_id, e);
                return VolumeResponse::error(libc::EIO);
            }
            if let Err(e) = socket.write_all(&buf) {
                eprintln!("[reader {}] write body error: {}", reader_id, e);
                return VolumeResponse::error(libc::EIO);
            }
            if let Err(e) = socket.flush() {
                eprintln!("[reader {}] flush error: {}", reader_id, e);
                return VolumeResponse::error(libc::EIO);
            }
        }

        // Wait for response on this reader's pending queue
        let reader_idx = reader_id as usize;
        if reader_idx < self.reader_pending.len() {
            let resp = self.reader_pending[reader_idx].wait_for(unique);
            if unique <= 20 {
                eprintln!("[mux] response {} for reader {}: {:?}", unique, reader_id, std::mem::discriminant(&resp));
            }
            resp
        } else {
            eprintln!("[reader {}] invalid reader_id", reader_id);
            VolumeResponse::error(libc::EIO)
        }
    }

    /// Get the number of readers this multiplexer supports.
    pub fn num_readers(&self) -> usize {
        self.reader_pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_slot() {
        let pending = ReaderPending::new();

        // Spawn thread to complete the request
        let pending_clone = Arc::new(pending);
        let pending_ref = Arc::clone(&pending_clone);

        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            pending_ref.complete(42, VolumeResponse::Ok);
        });

        // This would block forever without the complete() call
        // In a real test we'd need proper timeout handling
    }

    #[test]
    fn test_disconnect_wakes_pending_request() {
        use std::io::Read;
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
}
