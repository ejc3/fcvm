//! Length-prefixed binary protocol for TTY exec
//!
//! Wire format:
//!   [1-byte type][4-byte length (big-endian)][payload]
//!
//! Message types:
//!   0x01 DATA  - raw PTY output (payload = bytes)
//!   0x02 EXIT  - process exit (payload = 4-byte i32 exit code)
//!   0x03 ERROR - error message (payload = UTF-8 string)
//!   0x04 STDIN - input to PTY from host (payload = bytes)
//!
//! This protocol is used for TTY mode exec to cleanly separate
//! control messages (exit code) from raw terminal data.

use std::io::{self, Read, Write};

/// Message types for the TTY exec protocol
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Data = 0x01,
    Exit = 0x02,
    ErrorMsg = 0x03,
    Stdin = 0x04,
}

impl MessageType {
    fn from_u8(value: u8) -> io::Result<Self> {
        Self::from_u8_opt(value).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown message type: {:#x}", value),
            )
        })
    }

    /// Parse message type from byte, returning None for unknown types
    pub fn from_u8_opt(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(MessageType::Data),
            0x02 => Some(MessageType::Exit),
            0x03 => Some(MessageType::ErrorMsg),
            0x04 => Some(MessageType::Stdin),
            _ => None,
        }
    }
}

/// A message in the TTY exec protocol
#[derive(Debug, Clone)]
pub enum Message {
    /// Raw terminal output data
    Data(Vec<u8>),
    /// Process exit code
    Exit(i32),
    /// Error message
    Error(String),
    /// Input data from host to PTY
    Stdin(Vec<u8>),
}

impl Message {
    /// Write a message to a writer using the binary protocol
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Message::Data(data) => {
                writer.write_all(&[MessageType::Data as u8])?;
                writer.write_all(&(data.len() as u32).to_be_bytes())?;
                writer.write_all(data)?;
            }
            Message::Exit(code) => {
                writer.write_all(&[MessageType::Exit as u8])?;
                writer.write_all(&4u32.to_be_bytes())?;
                writer.write_all(&code.to_be_bytes())?;
            }
            Message::Error(msg) => {
                let bytes = msg.as_bytes();
                writer.write_all(&[MessageType::ErrorMsg as u8])?;
                writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
                writer.write_all(bytes)?;
            }
            Message::Stdin(data) => {
                writer.write_all(&[MessageType::Stdin as u8])?;
                writer.write_all(&(data.len() as u32).to_be_bytes())?;
                writer.write_all(data)?;
            }
        }
        writer.flush()
    }

    /// Read a message from a reader using the binary protocol
    pub fn read_from<R: Read>(reader: &mut R) -> io::Result<Self> {
        // Read type byte
        let mut type_buf = [0u8; 1];
        reader.read_exact(&mut type_buf)?;
        let msg_type = MessageType::from_u8(type_buf[0])?;

        // Read length (4 bytes big-endian)
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;

        // Sanity check: limit message size to 1MB (plenty for TTY data)
        // This prevents DoS via large length values
        const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
        if len > MAX_MESSAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "message too large: {} bytes (max {})",
                    len, MAX_MESSAGE_SIZE
                ),
            ));
        }

        // Read payload progressively to avoid allocating large buffers upfront
        // This prevents memory exhaustion if sender disconnects mid-transfer
        let mut payload = Vec::with_capacity(len.min(64 * 1024)); // Start with at most 64KB
        let mut remaining = len;
        let mut chunk = [0u8; 8192]; // Read in 8KB chunks

        while remaining > 0 {
            let to_read = remaining.min(chunk.len());
            reader.read_exact(&mut chunk[..to_read])?;
            payload.extend_from_slice(&chunk[..to_read]);
            remaining -= to_read;
        }

        // Parse based on type
        match msg_type {
            MessageType::Data => Ok(Message::Data(payload)),
            MessageType::Exit => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exit message must have 4-byte payload",
                    ));
                }
                let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Message::Exit(code))
            }
            MessageType::ErrorMsg => {
                let msg = String::from_utf8(payload).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("invalid UTF-8: {}", e))
                })?;
                Ok(Message::Error(msg))
            }
            MessageType::Stdin => Ok(Message::Stdin(payload)),
        }
    }
}

/// Write a Data message directly (convenience function for high-frequency writes)
pub fn write_data<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    writer.write_all(&[MessageType::Data as u8])?;
    writer.write_all(&(data.len() as u32).to_be_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}

/// Write an Exit message directly
pub fn write_exit<W: Write>(writer: &mut W, code: i32) -> io::Result<()> {
    writer.write_all(&[MessageType::Exit as u8])?;
    writer.write_all(&4u32.to_be_bytes())?;
    writer.write_all(&code.to_be_bytes())?;
    writer.flush()
}

/// Write an Error message directly
pub fn write_error<W: Write>(writer: &mut W, msg: &str) -> io::Result<()> {
    let bytes = msg.as_bytes();
    writer.write_all(&[MessageType::ErrorMsg as u8])?;
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

/// Write a Stdin message directly
pub fn write_stdin<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    writer.write_all(&[MessageType::Stdin as u8])?;
    writer.write_all(&(data.len() as u32).to_be_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_data_roundtrip() {
        let msg = Message::Data(b"hello world".to_vec());
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Message::read_from(&mut cursor).unwrap();

        match decoded {
            Message::Data(data) => assert_eq!(data, b"hello world"),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_exit_roundtrip() {
        let msg = Message::Exit(42);
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Message::read_from(&mut cursor).unwrap();

        match decoded {
            Message::Exit(code) => assert_eq!(code, 42),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_negative_exit_code() {
        let msg = Message::Exit(-1);
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Message::read_from(&mut cursor).unwrap();

        match decoded {
            Message::Exit(code) => assert_eq!(code, -1),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_error_roundtrip() {
        let msg = Message::Error("something went wrong".to_string());
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Message::read_from(&mut cursor).unwrap();

        match decoded {
            Message::Error(s) => assert_eq!(s, "something went wrong"),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_stdin_roundtrip() {
        let msg = Message::Stdin(b"user input".to_vec());
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Message::read_from(&mut cursor).unwrap();

        match decoded {
            Message::Stdin(data) => assert_eq!(data, b"user input"),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_binary_data() {
        // Test with binary data including null bytes and escape sequences
        let binary = vec![0x00, 0x01, 0x1b, 0x5b, 0x31, 0x6d, 0xff];
        let msg = Message::Data(binary.clone());
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = Message::read_from(&mut cursor).unwrap();

        match decoded {
            Message::Data(data) => assert_eq!(data, binary),
            _ => panic!("wrong message type"),
        }
    }
}
