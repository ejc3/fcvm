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
///
/// This value is chosen to fit in a u32 length prefix while being large enough
/// for typical FUSE operations. The length prefix is always checked before
/// casting to u32 to prevent overflow.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

// Static assertion: MAX_MESSAGE_SIZE must fit in u32 for wire protocol
const _: () = assert!(MAX_MESSAGE_SIZE <= u32::MAX as usize);

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
    /// Supplementary groups of the calling process.
    /// FUSE protocol only passes uid and primary gid. For proper permission checks
    /// (especially chown to a supplementary group), we forward the caller's groups.
    /// The client reads these from /proc/<pid>/status and forwards them.
    #[serde(default)]
    pub supplementary_groups: Vec<u32>,
    /// CRC32 checksum of the serialized request field for corruption detection.
    /// Used to diagnose vsock data corruption under NV2 nested virtualization.
    #[serde(default)]
    pub checksum: Option<u32>,
}

impl WireRequest {
    /// Create a new wire request.
    pub fn new(unique: u64, reader_id: u32, request: VolumeRequest) -> Self {
        Self {
            unique,
            reader_id,
            request,
            span: None,
            supplementary_groups: Vec::new(),
            checksum: None,
        }
    }

    /// Create a new wire request with trace span
    pub fn with_span(unique: u64, reader_id: u32, request: VolumeRequest, span: Span) -> Self {
        Self {
            unique,
            reader_id,
            request,
            span: Some(span),
            supplementary_groups: Vec::new(),
            checksum: None,
        }
    }

    /// Create a new wire request with supplementary groups
    pub fn with_groups(
        unique: u64,
        reader_id: u32,
        request: VolumeRequest,
        supplementary_groups: Vec<u32>,
    ) -> Self {
        Self {
            unique,
            reader_id,
            request,
            span: None,
            supplementary_groups,
            checksum: None,
        }
    }

    /// Create a new wire request with trace span and supplementary groups
    pub fn with_span_and_groups(
        unique: u64,
        reader_id: u32,
        request: VolumeRequest,
        span: Span,
        supplementary_groups: Vec<u32>,
    ) -> Self {
        Self {
            unique,
            reader_id,
            request,
            span: Some(span),
            supplementary_groups,
            checksum: None,
        }
    }

    /// Compute CRC32 checksum of the serialized request field.
    pub fn compute_checksum(&self) -> u32 {
        let data = bincode::serialize(&self.request).unwrap_or_default();
        crc32fast::hash(&data)
    }

    /// Add checksum to this request (consumes and returns self with checksum set).
    pub fn with_checksum(mut self) -> Self {
        self.checksum = Some(self.compute_checksum());
        self
    }

    /// Validate checksum if present.
    /// Returns true if no checksum is set (backwards compatible) or if checksum matches.
    pub fn validate_checksum(&self) -> bool {
        match self.checksum {
            Some(expected) => self.compute_checksum() == expected,
            None => true, // No checksum = skip validation
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

/// Get current time as nanos since UNIX epoch.
///
/// Returns 0 if the system clock is before the Unix epoch (which should
/// be rare but can happen with clock corrections or misconfigured systems).
#[inline]
pub fn now_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
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
    ///
    /// Note: Cross-machine deltas (to_srv, to_cli) are unreliable when clocks differ.
    /// Server-side deltas (deser, spawn, fs, chan) are always accurate.
    /// Client-side total (t0 → client_done) is always accurate.
    pub fn print(&self, _unique: u64, op_name: &str) {
        // Safe delta that handles clock skew (returns None if b < a or either is 0)
        let delta = |a: u64, b: u64| -> Option<i64> {
            if a == 0 || b == 0 {
                return None;
            }
            // Use checked_sub to detect underflow from clock skew
            b.checked_sub(a).map(|d| (d / 1000) as i64)
        };

        // Format delta, showing "?" for invalid/skewed values
        let fmt = |d: Option<i64>| -> String {
            match d {
                Some(v) if v >= 0 => v.to_string(),
                _ => "?".to_string(),
            }
        };

        // Server-side deltas (same machine, always valid)
        let server_total = delta(self.server_recv, self.server_resp_chan);
        let fs = delta(self.server_spawn, self.server_fs_done);

        // Client-side round-trip (same machine, always valid)
        let client_rtt = delta(self.t0, self.client_done);

        // Cross-machine deltas (may be invalid due to clock skew)
        let to_srv = delta(self.t0, self.server_recv);
        let to_cli = delta(self.server_resp_chan, self.client_recv);

        // Server time is reliable; cross-machine times may show "?" if clocks differ
        eprintln!(
            "[TRACE {:>12}] total={}µs srv={}µs | fs={} | to_srv={} to_cli={}",
            op_name,
            fmt(client_rtt),
            fmt(server_total),
            fmt(fs),
            fmt(to_srv),
            fmt(to_cli),
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
    /// CRC32 checksum of the serialized response field for corruption detection.
    #[serde(default)]
    pub checksum: Option<u32>,
}

impl WireResponse {
    /// Create a new wire response.
    pub fn new(unique: u64, reader_id: u32, response: VolumeResponse) -> Self {
        Self {
            unique,
            reader_id,
            response,
            span: None,
            checksum: None,
        }
    }

    /// Create a new wire response with trace span
    pub fn with_span(unique: u64, reader_id: u32, response: VolumeResponse, span: Span) -> Self {
        Self {
            unique,
            reader_id,
            response,
            span: Some(span),
            checksum: None,
        }
    }

    /// Compute CRC32 checksum of the serialized response field.
    pub fn compute_checksum(&self) -> u32 {
        let data = bincode::serialize(&self.response).unwrap_or_default();
        crc32fast::hash(&data)
    }

    /// Add checksum to this response (consumes and returns self with checksum set).
    pub fn with_checksum(mut self) -> Self {
        self.checksum = Some(self.compute_checksum());
        self
    }

    /// Validate checksum if present.
    /// Returns true if no checksum is set (backwards compatible) or if checksum matches.
    pub fn validate_checksum(&self) -> bool {
        match self.checksum {
            Some(expected) => self.compute_checksum() == expected,
            None => true, // No checksum = skip validation
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
