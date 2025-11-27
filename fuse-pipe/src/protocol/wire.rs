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
}

impl WireRequest {
    /// Create a new wire request.
    pub fn new(unique: u64, reader_id: u32, request: VolumeRequest) -> Self {
        Self {
            unique,
            reader_id,
            request,
        }
    }

    /// Serialize to bytes with length prefix.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let payload = bincode::serialize(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

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

/// Wire message wrapping a response with routing information.
///
/// The `unique` and `reader_id` are echoed from the request for routing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WireResponse {
    pub unique: u64,
    pub reader_id: u32,
    pub response: VolumeResponse,
}

impl WireResponse {
    /// Create a new wire response.
    pub fn new(unique: u64, reader_id: u32, response: VolumeResponse) -> Self {
        Self {
            unique,
            reader_id,
            response,
        }
    }

    /// Serialize to bytes with length prefix.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let payload = bincode::serialize(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

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
