//! Wire protocol framing for FUSE-over-pipe/socket communication.
//!
//! # Frame Format
//!
//! ```text
//! +----------+---------+
//! |  length  | payload |
//! | (4 bytes)| (N bytes)|
//! +----------+---------+
//! ```
//!
//! - Length is a big-endian u32 specifying the payload size
//! - Payload is bincode-serialized WireRequest or WireResponse

use super::{VolumeRequest, VolumeResponse};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Maximum message size (16 MB).
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Wire message wrapping a request with routing information.
///
/// The `unique` field is a request ID for matching responses.
/// The `reader_id` identifies which FUSE reader thread sent this request,
/// so responses can be routed back to the correct thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WireRequest {
    pub unique: u64,
    pub reader_id: u32,
    pub request: VolumeRequest,
    /// Trace span - passed through request/response for e2e latency tracking
    /// Only populated for traced requests (unique % 100 == 0)
    #[serde(default)]
    pub span: Option<Span>,
}

impl WireRequest {
    /// Create a new wire request.
    pub fn new(unique: u64, reader_id: u32, request: VolumeRequest) -> Self {
        Self {
            unique,
            reader_id,
            request,
            span: None,
        }
    }

    /// Create a new wire request with trace span
    pub fn with_span(unique: u64, reader_id: u32, request: VolumeRequest, span: Span) -> Self {
        Self {
            unique,
            reader_id,
            request,
            span: Some(span),
        }
    }

    /// Serialize to bytes with length prefix.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let payload =
            bincode::serialize(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("message too large: {} bytes", payload.len()),
            ));
        }

        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        Ok(buf)
    }

    /// Deserialize from bytes (without length prefix).
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        bincode::deserialize(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Get current time as nanos since UNIX epoch
#[inline]
pub fn now_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Distributed trace span - passed through request/response for e2e latency tracking.
///
/// The span follows the request:
/// 1. Client creates span with t0, embeds in WireRequest
/// 2. Server receives, marks server_recv/deser/spawn/fs_done/resp_chan
/// 3. Server serializes span into WireResponse (marks BEFORE this point are visible)
/// 4. Client receives, marks client_recv/done, prints
///
/// Note: Timestamps after server serialization (like "flush time") cannot be in the span
/// since they occur after the span is serialized into the response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Span {
    /// Start time (nanos since epoch) - set by client when creating request
    pub t0: u64,
    // Server side (set during request processing, BEFORE response serialization)
    pub server_recv: u64,      // When server received from socket
    pub server_deser: u64,     // After deserializing request
    pub server_spawn: u64,     // When spawn_blocking task started running
    pub server_fs_done: u64,   // After fs operation completed
    pub server_resp_chan: u64, // After received from response channel (just before serializing response)
    // Client side (set after response received)
    pub client_recv: u64, // When client received from socket
    pub client_done: u64, // When response delivered to caller
}

impl Span {
    /// Create a new span with current time as t0
    pub fn new() -> Self {
        Self {
            t0: now_nanos(),
            ..Default::default()
        }
    }

    /// Record current time for a field
    #[inline]
    pub fn mark(&mut self, field: &str) {
        let now = now_nanos();
        match field {
            "server_recv" => self.server_recv = now,
            "server_deser" => self.server_deser = now,
            "server_spawn" => self.server_spawn = now,
            "server_fs_done" => self.server_fs_done = now,
            "server_resp_chan" => self.server_resp_chan = now,
            "client_recv" => self.client_recv = now,
            "client_done" => self.client_done = now,
            _ => {}
        }
    }

    /// Print the span as a breakdown (all times in µs)
    pub fn print(&self, unique: u64) {
        let delta = |a: u64, b: u64| -> i64 {
            if a == 0 || b == 0 {
                -1
            } else {
                ((b - a) / 1000) as i64
            }
        };
        let total = delta(self.t0, self.client_done);

        // to_server: client send → server receive (includes client serialize, socket write, network)
        // deser: deserialize request
        // spawn: task scheduling delay
        // fs: filesystem operation
        // chan: response channel wait
        // to_client: server serialize + write + flush + network + client receive
        eprintln!(
            "[TRACE {}] total={}µs | to_srv={} deser={} spawn={} fs={} chan={} | to_cli={} done={}",
            unique,
            total,
            delta(self.t0, self.server_recv),
            delta(self.server_recv, self.server_deser),
            delta(self.server_deser, self.server_spawn),
            delta(self.server_spawn, self.server_fs_done),
            delta(self.server_fs_done, self.server_resp_chan),
            delta(self.server_resp_chan, self.client_recv), // Includes serialize + write + flush + network
            delta(self.client_recv, self.client_done),
        );
    }
}

/// Wire message wrapping a response with routing information.
///
/// The `unique` and `reader_id` are echoed from the request for routing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WireResponse {
    pub unique: u64,
    pub reader_id: u32,
    pub response: VolumeResponse,
    /// Trace span - passed back from server with timing data
    #[serde(default)]
    pub span: Option<Span>,
}

impl WireResponse {
    /// Create a new wire response.
    pub fn new(unique: u64, reader_id: u32, response: VolumeResponse) -> Self {
        Self {
            unique,
            reader_id,
            response,
            span: None,
        }
    }

    /// Create a new wire response with trace span
    pub fn with_span(unique: u64, reader_id: u32, response: VolumeResponse, span: Span) -> Self {
        Self {
            unique,
            reader_id,
            response,
            span: Some(span),
        }
    }

    /// Serialize to bytes with length prefix.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let payload =
            bincode::serialize(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("message too large: {} bytes", payload.len()),
            ));
        }

        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        Ok(buf)
    }

    /// Deserialize from bytes (without length prefix).
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        bincode::deserialize(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Read a length-prefixed message from a reader.
///
/// Returns the raw payload bytes (without the length prefix).
pub fn read_message<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {} bytes", len),
        ));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a length-prefixed message to a writer.
pub fn write_message<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {} bytes", data.len()),
        ));
    }

    let len_bytes = (data.len() as u32).to_be_bytes();
    writer.write_all(&len_bytes)?;
    writer.write_all(data)?;
    Ok(())
}

/// Async version of read_message using tokio.
pub async fn read_message_async<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {} bytes", len),
        ));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Async version of write_message using tokio.
pub async fn write_message_async<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    data: &[u8],
) -> io::Result<()> {
    if data.len() > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {} bytes", data.len()),
        ));
    }

    let len_bytes = (data.len() as u32).to_be_bytes();
    writer.write_all(&len_bytes).await?;
    writer.write_all(data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_wire_request_encode_decode() {
        let req = WireRequest::new(
            42,
            1,
            VolumeRequest::Lookup {
                parent: 1,
                name: "test.txt".to_string(),
                uid: 1000,
                gid: 1000,
                pid: 0,
            },
        );

        let encoded = req.encode().unwrap();

        // Verify length prefix
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len, encoded.len() - 4);

        // Decode
        let decoded = WireRequest::decode(&encoded[4..]).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_wire_response_encode_decode() {
        let resp = WireResponse::new(42, 1, VolumeResponse::Ok);

        let encoded = resp.encode().unwrap();
        let decoded = WireResponse::decode(&encoded[4..]).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_read_write_message() {
        let data = b"hello world";
        let mut buf = Vec::new();

        write_message(&mut buf, data).unwrap();

        let mut cursor = Cursor::new(buf);
        let read_data = read_message(&mut cursor).unwrap();

        assert_eq!(read_data, data);
    }

    #[test]
    fn test_message_too_large() {
        let huge_data = vec![0u8; MAX_MESSAGE_SIZE + 1];
        let mut buf = Vec::new();

        let result = write_message(&mut buf, &huge_data);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_async_read_write_message() {
        let data = b"async hello";
        let mut buf = Vec::new();

        write_message_async(&mut buf, data).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let read_data = read_message_async(&mut cursor).await.unwrap();

        assert_eq!(read_data, data);
    }
}
