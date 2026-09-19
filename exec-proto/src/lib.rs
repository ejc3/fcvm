//! Length-prefixed binary protocol for exec sessions
//!
//! Wire format:
//!   [1-byte type][4-byte length (big-endian)][payload]
//!
//! Message types:
//!   0x01 DATA      - command stdout, or PTY output with -t (payload = bytes)
//!   0x02 EXIT      - process exit (payload = 4-byte i32 exit code)
//!   0x03 ERROR     - error message (payload = UTF-8 string)
//!   0x04 STDIN     - input from the host (payload = bytes)
//!   0x05 STDIN_EOF - the host's stdin reached end of file (no payload)
//!   0x06 STDERR    - command stderr, never sent with -t (payload = bytes)
//!   0x07 RESIZE    - the host terminal's size (payload = rows u16, cols u16, big-endian)
//!   0x08 STDIN_WINDOW - the guest will take this many more STDIN bytes (payload = u32, big-endian)
//!
//! Every exec mode uses this framing after the handshake, so output is
//! byte-exact: no line splitting, no UTF-8 requirement, no added newline.
//!
//! STDIN is flow-controlled by the guest, the way SSH does it. A host sends
//! STDIN bytes only against a window the guest has granted, and the guest
//! grants more as the command consumes them. The connection itself therefore
//! never backs up behind a command that is not reading its stdin, the guest
//! always has a read outstanding, and it learns at once when the host goes
//! away. That last part is what makes "kill the command when the client dies"
//! hold: a host hangup does not cross a vsock connection that is backed up.

use std::io::{self, Read, Write};

/// Maximum payload size for a single message (1MB, plenty for TTY data).
/// This prevents DoS via large length values.
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Three-phase exec handshake tokens, shared by the host client
/// (`fcvm exec`, src/commands/exec.rs), the guest server (fc-agent/src/exec.rs),
/// and the mock server (fc-mock/src/vsock_exec.rs).
///
/// Every exec connection starts with three newline-terminated lines:
///
/// 1. client → server: `ExecRequest` JSON
/// 2. server → client: `ACK2` ([`HANDSHAKE_ACK`]). The request line was fully
///    consumed; nothing has executed yet
/// 3. client → server: `GO` — the server may start executing ONLY after
///    consuming this line
///
/// Why: a VM snapshot pause (pre-start, startup, or a user-initiated
/// `fcvm snapshot create --pid`) resets the vsock transport and silently
/// orphans in-flight connections with no error on either side. Before ACK the
/// client can prove its request never reached execution, so it safely
/// reconnects and resends; once it has sent GO it must never resend (execution
/// may have started), and a dead connection is a loud bounded error instead of
/// a silent hang. The server closes any connection whose handshake stalls, so
/// orphaned connections can never execute and never leak a thread.
///
/// The token also names the protocol version. `ACK2` is a server that answers
/// every exec mode with exec-proto frames, understands STDIN_EOF, STDERR and
/// RESIZE, and grants STDIN_WINDOW.
pub const HANDSHAKE_ACK: &str = "ACK2";
/// The token servers sent before the framed protocol covered every mode. Such
/// a server answers a plain exec with JSON lines, and stops forwarding stdin
/// at the first frame type it does not know. A client that reads this token
/// must not send GO. It occurs when a VM was started, or a snapshot taken, by
/// an older fcvm.
pub const HANDSHAKE_ACK_V1: &str = "ACK";
/// See [`HANDSHAKE_ACK`].
pub const HANDSHAKE_GO: &str = "GO";

/// How much either side reads from a command or a terminal at a time, and so
/// the largest payload it puts in one frame.
pub const IO_CHUNK: usize = 64 * 1024;

/// Frame types of the exec protocol
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Data = 0x01,
    Exit = 0x02,
    ErrorMsg = 0x03,
    Stdin = 0x04,
    StdinEof = 0x05,
    Stderr = 0x06,
    Resize = 0x07,
    StdinWindow = 0x08,
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
            0x05 => Some(MessageType::StdinEof),
            0x06 => Some(MessageType::Stderr),
            0x07 => Some(MessageType::Resize),
            0x08 => Some(MessageType::StdinWindow),
            _ => None,
        }
    }
}

/// One frame of the exec protocol
#[derive(Debug, Clone)]
pub enum Message {
    /// Command stdout, or everything a PTY produced
    Data(Vec<u8>),
    /// Process exit code
    Exit(i32),
    /// Error message
    Error(String),
    /// Input from the host, for the command's stdin or its PTY
    Stdin(Vec<u8>),
    /// The host's stdin reached end of file. Without a PTY the guest closes
    /// the command's stdin, so a command that reads to end of input finishes.
    StdinEof,
    /// Command stderr. Only sent without a PTY; a PTY merges both streams.
    Stderr(Vec<u8>),
    /// The host terminal's size. A client with a terminal on stdin sends it
    /// when a PTY session starts and on every window change, and the guest
    /// applies it to the PTY.
    Resize(TtySize),
    /// Guest to host: this many more STDIN bytes may be sent. The host starts
    /// with none, and the guest's first grant opens the window.
    StdinWindow(u32),
}

impl Message {
    /// Encode a message as one frame.
    pub fn encode(&self) -> Vec<u8> {
        let exit;
        let resize;
        let window;
        let (msg_type, payload): (MessageType, &[u8]) = match self {
            Message::Data(data) => (MessageType::Data, data),
            Message::Exit(code) => {
                exit = code.to_be_bytes();
                (MessageType::Exit, &exit)
            }
            Message::Error(msg) => (MessageType::ErrorMsg, msg.as_bytes()),
            Message::Stdin(data) => (MessageType::Stdin, data),
            Message::StdinEof => (MessageType::StdinEof, &[]),
            Message::Stderr(data) => (MessageType::Stderr, data),
            Message::Resize(size) => {
                resize = size.to_payload();
                (MessageType::Resize, &resize)
            }
            Message::StdinWindow(bytes) => {
                window = bytes.to_be_bytes();
                (MessageType::StdinWindow, &window)
            }
        };
        encode_frame(msg_type, payload)
    }

    /// Write a message to a writer using the binary protocol
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.encode())?;
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
        let len = validate_len(u32::from_be_bytes(len_buf))?;

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

        Self::from_parts(msg_type, payload)
    }

    /// Read a message from an async reader using the binary protocol.
    ///
    /// Async equivalent of [`Message::read_from`], for bridging the framed
    /// exec stream into async contexts (e.g. WebSocket terminal sessions).
    pub async fn read_from_async<R>(reader: &mut R) -> io::Result<Self>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;

        // Read type byte
        let mut type_buf = [0u8; 1];
        reader.read_exact(&mut type_buf).await?;
        let msg_type = MessageType::from_u8(type_buf[0])?;

        // Read length (4 bytes big-endian)
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let len = validate_len(u32::from_be_bytes(len_buf))?;

        // Read payload progressively to avoid allocating large buffers upfront
        let mut payload = Vec::with_capacity(len.min(64 * 1024)); // Start with at most 64KB
        let mut remaining = len;
        let mut chunk = [0u8; 8192]; // Read in 8KB chunks

        while remaining > 0 {
            let to_read = remaining.min(chunk.len());
            reader.read_exact(&mut chunk[..to_read]).await?;
            payload.extend_from_slice(&chunk[..to_read]);
            remaining -= to_read;
        }

        Self::from_parts(msg_type, payload)
    }

    /// Build a message from its decoded type byte and payload.
    fn from_parts(msg_type: MessageType, payload: Vec<u8>) -> io::Result<Self> {
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
            MessageType::StdinEof => {
                if !payload.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "stdin-eof message must have an empty payload",
                    ));
                }
                Ok(Message::StdinEof)
            }
            MessageType::Stderr => Ok(Message::Stderr(payload)),
            MessageType::Resize => match payload.as_slice() {
                [r0, r1, c0, c1] => Ok(Message::Resize(TtySize {
                    rows: u16::from_be_bytes([*r0, *r1]),
                    cols: u16::from_be_bytes([*c0, *c1]),
                })),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resize message must have a 4-byte payload",
                )),
            },
            MessageType::StdinWindow => match <[u8; 4]>::try_from(payload.as_slice()) {
                Ok(bytes) => Ok(Message::StdinWindow(u32::from_be_bytes(bytes))),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stdin-window message must have a 4-byte payload",
                )),
            },
        }
    }
}

/// Build one frame: type byte, big-endian length, payload.
fn encode_frame(msg_type: MessageType, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(msg_type as u8);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Write one frame and flush.
fn write_frame<W: Write>(writer: &mut W, msg_type: MessageType, payload: &[u8]) -> io::Result<()> {
    writer.write_all(&encode_frame(msg_type, payload))?;
    writer.flush()
}

/// Validate a payload length against the maximum message size.
fn validate_len(len: u32) -> io::Result<usize> {
    let len = len as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "message too large: {} bytes (max {})",
                len, MAX_MESSAGE_SIZE
            ),
        ));
    }
    Ok(len)
}

/// Write a Data message directly (convenience function for high-frequency writes)
pub fn write_data<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    write_frame(writer, MessageType::Data, data)
}

/// Write a Stderr message directly
pub fn write_stderr<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    write_frame(writer, MessageType::Stderr, data)
}

/// Write an Exit message directly
pub fn write_exit<W: Write>(writer: &mut W, code: i32) -> io::Result<()> {
    write_frame(writer, MessageType::Exit, &code.to_be_bytes())
}

/// Write an Error message directly
pub fn write_error<W: Write>(writer: &mut W, msg: &str) -> io::Result<()> {
    write_frame(writer, MessageType::ErrorMsg, msg.as_bytes())
}

/// Write a Stdin message directly
pub fn write_stdin<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    write_frame(writer, MessageType::Stdin, data)
}

/// Write a StdinEof message directly
pub fn write_stdin_eof<W: Write>(writer: &mut W) -> io::Result<()> {
    write_frame(writer, MessageType::StdinEof, &[])
}

/// Write a Resize message directly
pub fn write_resize<W: Write>(writer: &mut W, size: TtySize) -> io::Result<()> {
    write_frame(writer, MessageType::Resize, &size.to_payload())
}

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TtySize {
    pub rows: u16,
    pub cols: u16,
}

impl TtySize {
    fn to_payload(self) -> [u8; 4] {
        let (rows, cols) = (self.rows.to_be_bytes(), self.cols.to_be_bytes());
        [rows[0], rows[1], cols[0], cols[1]]
    }
}

// ---- Exec request and pre-ACK rejection ----
//
// Shared by the host client, fc-agent and fc-mock so the three cannot drift.

/// The request line a client sends before the ACK/GO handshake.
///
/// Fields a plain exec does not use are left out of the JSON, so the line for
/// such a request stays the same as fields are added.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    /// Run inside the container (true) or in the guest OS (false).
    #[serde(default)]
    pub in_container: bool,
    /// Forward the client's stdin (-i)
    #[serde(default)]
    pub interactive: bool,
    /// Allocate a pseudo-TTY (-t)
    #[serde(default)]
    pub tty: bool,
    /// Size of the client's terminal, so the PTY has it before the command
    /// starts. Absent without -t, or when the client has no terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty_size: Option<TtySize>,
    /// Environment for the command as `KEY=VALUE`, already resolved by the
    /// client (-e, --env-file). Later entries win.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Working directory for the command (-w)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// `USER[:GROUP]`, names or numbers, to run the command as (-u)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Extended capabilities inside the container (--privileged)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,
    /// Start the command and return without waiting for it (-d)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detach: bool,
}

/// A server refuses a request before ACK with this one JSON line, so the
/// client fails with the reason instead of resending a request that can
/// never succeed.
#[derive(serde::Serialize, serde::Deserialize)]
struct Rejection {
    #[serde(rename = "type")]
    kind: String,
    data: String,
}

/// Build the pre-ACK rejection line (no trailing newline).
pub fn rejection_line(reason: &str) -> String {
    serde_json::to_string(&Rejection {
        kind: "error".to_string(),
        data: reason.to_string(),
    })
    .expect("a struct of two strings always serializes")
}

/// Parse a pre-ACK line as a rejection, returning its reason.
pub fn parse_rejection(line: &[u8]) -> Option<String> {
    let rejection: Rejection = serde_json::from_slice(line).ok()?;
    (rejection.kind == "error").then_some(rejection.data)
}

// ---- Restore-completion ACK framing ----
//
// Shared by fc-agent (producer) and fcvm's restore listener (consumer) so the
// frame budget cannot drift between the two binaries: the consumer fails any
// frame over the cap, so the producer must be structurally unable to build
// one.

/// Frame prefix; the restore epoch follows, then optional telemetry after one
/// space, then a newline.
pub const RESTORE_COMPLETE_PREFIX: &str = "restore-complete:";

/// Whole-frame cap, prefix through newline. Sized for a UUID epoch plus the
/// guest's phase-timing telemetry JSON with generous headroom (a dozen
/// float-valued phases render near 450 bytes).
pub const RESTORE_COMPLETE_MAX_FRAME_BYTES: usize = 1024;

/// Build the restore-completion ACK frame.
///
/// Telemetry is advisory and must never cost a healthy clone its ACK: when it
/// does not fit the budget alongside the epoch, it is dropped (replaced by
/// `{}`) rather than truncated, so the consumer always sees either complete
/// JSON or none.
pub fn restore_complete_frame(epoch: &str, telemetry: &str) -> String {
    let frame = if telemetry.is_empty() {
        format!("{RESTORE_COMPLETE_PREFIX}{epoch}\n")
    } else {
        format!("{RESTORE_COMPLETE_PREFIX}{epoch} {telemetry}\n")
    };
    if frame.len() <= RESTORE_COMPLETE_MAX_FRAME_BYTES {
        return frame;
    }
    format!("{RESTORE_COMPLETE_PREFIX}{epoch} {{}}\n")
}

#[cfg(test)]
mod restore_frame_tests {
    use super::*;

    #[test]
    fn telemetry_within_budget_rides_after_the_epoch() {
        assert_eq!(
            restore_complete_frame("epoch-1", "{\"total_ms\":40.2}"),
            "restore-complete:epoch-1 {\"total_ms\":40.2}\n"
        );
        assert_eq!(
            restore_complete_frame("epoch-1", ""),
            "restore-complete:epoch-1\n"
        );
    }

    #[test]
    fn oversized_telemetry_is_dropped_never_truncated() {
        let oversized = "x".repeat(RESTORE_COMPLETE_MAX_FRAME_BYTES);
        let frame = restore_complete_frame("epoch-1", &oversized);
        assert_eq!(frame, "restore-complete:epoch-1 {}\n");
        assert!(frame.len() <= RESTORE_COMPLETE_MAX_FRAME_BYTES);
    }
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
    fn test_stdin_eof_roundtrip_has_no_payload() {
        let mut buf = Vec::new();
        write_stdin_eof(&mut buf).unwrap();
        assert_eq!(buf, [MessageType::StdinEof as u8, 0, 0, 0, 0]);

        let decoded = Message::read_from(&mut Cursor::new(buf)).unwrap();
        assert!(matches!(decoded, Message::StdinEof));
    }

    #[test]
    fn test_stdin_eof_with_payload_is_rejected() {
        let mut buf = vec![MessageType::StdinEof as u8];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.push(b'x');
        let err = Message::read_from(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_stderr_is_a_distinct_stream_from_data() {
        let mut buf = Vec::new();
        write_data(&mut buf, b"out").unwrap();
        write_stderr(&mut buf, b"err").unwrap();

        let mut cursor = Cursor::new(buf);
        match Message::read_from(&mut cursor).unwrap() {
            Message::Data(data) => assert_eq!(data, b"out"),
            other => panic!("expected Data, got {:?}", other),
        }
        match Message::read_from(&mut cursor).unwrap() {
            Message::Stderr(data) => assert_eq!(data, b"err"),
            other => panic!("expected Stderr, got {:?}", other),
        }
    }

    #[test]
    fn test_encode_matches_write_to_for_every_message() {
        for msg in [
            Message::Data(vec![0, 255, b'\n']),
            Message::Exit(-7),
            Message::Error("boom".to_string()),
            Message::Stdin(b"in".to_vec()),
            Message::StdinEof,
            Message::Stderr(vec![0xff, 0xfe]),
            Message::Resize(TtySize { rows: 24, cols: 80 }),
            Message::StdinWindow(262_144),
        ] {
            let mut written = Vec::new();
            msg.write_to(&mut written).unwrap();
            assert_eq!(written, msg.encode(), "{:?}", msg);
        }
    }

    #[test]
    fn test_rejection_line_roundtrip() {
        let line = rejection_line("Empty command");
        assert_eq!(line, r#"{"type":"error","data":"Empty command"}"#);
        assert_eq!(
            parse_rejection(line.as_bytes()).as_deref(),
            Some("Empty command")
        );
        assert_eq!(parse_rejection(b"ACK"), None);
        assert_eq!(parse_rejection(br#"{"type":"exit","data":"0"}"#), None);
    }

    #[test]
    fn test_exec_request_defaults_missing_flags_to_false() {
        let request: ExecRequest = serde_json::from_str(r#"{"command":["true"]}"#).unwrap();
        assert_eq!(
            request,
            ExecRequest {
                command: vec!["true".to_string()],
                in_container: false,
                interactive: false,
                tty: false,
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_exec_request_omits_the_fields_a_plain_exec_does_not_use() {
        let request = ExecRequest {
            command: vec!["true".to_string()],
            in_container: false,
            interactive: false,
            tty: false,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"command":["true"],"in_container":false,"interactive":false,"tty":false}"#
        );
    }

    #[test]
    fn test_exec_request_flag_fields_roundtrip() {
        let request = ExecRequest {
            command: vec!["id".to_string()],
            in_container: true,
            env: vec!["A=1".to_string(), "B=two words".to_string()],
            workdir: Some("/tmp".to_string()),
            user: Some("nobody:users".to_string()),
            privileged: true,
            detach: true,
            ..Default::default()
        };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<ExecRequest>(&line).unwrap(), request);
    }

    #[test]
    fn test_stdin_window_roundtrip_and_payload_length() {
        let frame = Message::StdinWindow(0x0004_0000).encode();
        assert_eq!(
            frame,
            [MessageType::StdinWindow as u8, 0, 0, 0, 4, 0, 4, 0, 0]
        );
        match Message::read_from(&mut Cursor::new(frame)).unwrap() {
            Message::StdinWindow(bytes) => assert_eq!(bytes, 262_144),
            other => panic!("expected StdinWindow, got {:?}", other),
        }

        let mut short = vec![MessageType::StdinWindow as u8];
        short.extend_from_slice(&3u32.to_be_bytes());
        short.extend_from_slice(&[0, 0, 1]);
        let err = Message::read_from(&mut Cursor::new(short)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_resize_roundtrip_and_payload_length() {
        let size = TtySize {
            rows: 41,
            cols: 300,
        };
        let mut buf = Vec::new();
        write_resize(&mut buf, size).unwrap();
        assert_eq!(buf, [MessageType::Resize as u8, 0, 0, 0, 4, 0, 41, 1, 44]);
        assert_eq!(buf, Message::Resize(size).encode());
        match Message::read_from(&mut Cursor::new(buf)).unwrap() {
            Message::Resize(decoded) => assert_eq!(decoded, size),
            other => panic!("expected Resize, got {:?}", other),
        }

        let mut short = vec![MessageType::Resize as u8];
        short.extend_from_slice(&2u32.to_be_bytes());
        short.extend_from_slice(&[0, 41]);
        let err = Message::read_from(&mut Cursor::new(short)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
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

    #[tokio::test]
    async fn test_async_read_decodes_data_then_exit() {
        // Stream as written by the guest: terminal output frames followed by Exit
        let mut buf = Vec::new();
        write_data(&mut buf, b"terminal output").unwrap();
        write_exit(&mut buf, 3).unwrap();

        let mut reader = buf.as_slice();
        match Message::read_from_async(&mut reader).await.unwrap() {
            Message::Data(data) => assert_eq!(data, b"terminal output"),
            other => panic!("expected Data, got {:?}", other),
        }
        match Message::read_from_async(&mut reader).await.unwrap() {
            Message::Exit(code) => assert_eq!(code, 3),
            other => panic!("expected Exit(3), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_async_read_error_message() {
        let mut buf = Vec::new();
        write_error(&mut buf, "spawn failed").unwrap();

        let mut reader = buf.as_slice();
        match Message::read_from_async(&mut reader).await.unwrap() {
            Message::Error(msg) => assert_eq!(msg, "spawn failed"),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_async_read_eof() {
        let mut reader: &[u8] = &[];
        let err = Message::read_from_async(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn test_async_read_truncated_frame() {
        // Frame header claims 16 bytes but the stream ends after 4, as if
        // the connection dropped mid-message.
        let mut buf = Vec::new();
        buf.push(MessageType::Data as u8);
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"oops");

        let mut reader = buf.as_slice();
        let err = Message::read_from_async(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
