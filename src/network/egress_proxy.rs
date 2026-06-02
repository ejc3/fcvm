//! Host-side egress proxy: accepts a single multiplexed vsock connection from
//! guest transparent proxy, demultiplexes streams, opens real TCP connections.
//!
//! Architecture:
//!   Guest fc-agent → vsock mux → Firecracker → Unix socket → this proxy → real TCP → internet
//!
//! All guest TCP connections share ONE vsock connection, multiplexed by stream_id.
//! Frame format: [stream_id(4)][type(1)][flags(1)][payload_len(4)] + payload
//!
//! Supports both IPv4 and IPv6 destinations (address family in OPEN frame payload).
//!
//! Runs OUTSIDE the network namespace, so it has direct internet access.
//!
//! Backpressure (both directions are bounded — no unbounded buffering):
//! - destination → guest: the writer channel is bounded. When the vsock is
//!   congested, send_frame().await blocks, which blocks the TCP read loop,
//!   which triggers TCP flow control back to the destination.
//! - guest → destination: each per-stream channel is bounded. When a stream's
//!   destination is slower than the guest, the channel fills and the vsock read
//!   loop blocks on send().await. The resulting vsock backpressure reaches the
//!   guest's bounded writer channel, which stops reading the guest application's
//!   socket (TCP flow control), so the guest is throttled to the destination's
//!   rate instead of the upload accumulating in host memory. The trade-off is
//!   that a stalled destination also stalls the other streams of the session
//!   until the queue drains; memory stays bounded either way.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Vsock port for egress proxy (must match fc-agent/src/vsock.rs EGRESS_PROXY_PORT).
pub const VSOCK_EGRESS_PROXY_PORT: u32 = 52000;

/// Frame header size: stream_id(4) + type(1) + flags(1) + payload_len(4) = 10
const FRAME_HEADER_SIZE: usize = 10;

/// Max DATA frame payload — 32KB prevents head-of-line blocking.
const MAX_DATA_PAYLOAD: usize = 32 * 1024;

/// Writer channel capacity. At 32KB per DATA frame, 1024 slots = max 32MB buffered.
/// Provides enough burst capacity while preventing unbounded memory growth.
const WRITER_CHANNEL_CAPACITY: usize = 1024;

/// Per-stream channel capacity (guest → destination direction).
/// At 32KB per DATA frame, 256 slots = max 8MB buffered per stream — comparable
/// to a kernel TCP socket buffer. When full, the mux reader stops reading the
/// vsock, propagating backpressure to the guest instead of buffering the upload.
const STREAM_CHANNEL_CAPACITY: usize = 256;

/// Max frame payload we'll accept. Anything larger is a protocol error
/// (prevents OOM from malformed frames claiming enormous payloads).
const MAX_FRAME_PAYLOAD: usize = MAX_DATA_PAYLOAD + 1024;

// Frame types (must match guest side)
const FRAME_OPEN: u8 = 1;
const FRAME_DATA: u8 = 2;
const FRAME_CLOSE: u8 = 3;
const FRAME_RST: u8 = 4;
const FRAME_OPEN_OK: u8 = 5;
const FRAME_OPEN_FAIL: u8 = 6;

// Address family constants (must match guest side)
const AF_INET: u8 = 0x04;
const AF_INET6: u8 = 0x06;

/// Per-stream channel sender for routing incoming frames.
/// Bounded so a guest uploading to a slow or stalled destination cannot grow
/// fcvm's heap without limit: when a stream's queue is full the mux reader
/// awaits the send, and the resulting vsock backpressure throttles the guest
/// via its bounded writer channel and TCP flow control.
type StreamSender = mpsc::Sender<(u8, Vec<u8>)>;

/// Run the host-side egress proxy.
///
/// Listens on the Firecracker vsock Unix socket path for the guest's
/// single multiplexed connection. Each accepted connection runs a
/// multiplexed session handling many TCP streams.
pub async fn run_egress_proxy(vsock_socket_path: &Path) -> Result<()> {
    let socket_path = format!("{}_{VSOCK_EGRESS_PROXY_PORT}", vsock_socket_path.display());

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding egress proxy to {}", socket_path))?;

    info!(socket = %socket_path, "egress proxy started");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                info!("egress proxy: new multiplexed session");
                tokio::spawn(async move {
                    if let Err(e) = handle_mux_session(stream).await {
                        debug!(error = %e, "egress proxy session ended");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "egress proxy accept error");
            }
        }
    }
}

/// Parse an OPEN frame payload into destination address and port.
/// Returns None if the payload is malformed.
fn parse_open_payload(payload: &[u8]) -> Option<(IpAddr, u16)> {
    if payload.is_empty() {
        return None;
    }

    match payload[0] {
        AF_INET => {
            // [family(1)][ip(4)][port(2)] = 7 bytes
            if payload.len() < 7 {
                return None;
            }
            let ip = Ipv4Addr::new(payload[1], payload[2], payload[3], payload[4]);
            let port = u16::from_be_bytes([payload[5], payload[6]]);
            Some((IpAddr::V4(ip), port))
        }
        AF_INET6 => {
            // [family(1)][ip(16)][port(2)] = 19 bytes
            if payload.len() < 19 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&payload[1..17]);
            let ip = Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([payload[17], payload[18]]);
            Some((IpAddr::V6(ip), port))
        }
        _ => None,
    }
}

/// Handle a single multiplexed vsock session.
/// Reads frames, dispatches to per-stream handlers, relays to real TCP destinations.
async fn handle_mux_session(vsock_stream: UnixStream) -> Result<()> {
    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(WRITER_CHANNEL_CAPACITY);

    let (vsock_read, vsock_write) = vsock_stream.into_split();

    // Writer task: drains channel and writes frames to vsock
    let writer_handle = tokio::spawn(async move {
        let mut writer = vsock_write;
        while let Some(frame) = writer_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    // Reader: process frames inline (no separate task needed since this IS the session task)
    let mut reader = vsock_read;
    let mut streams: HashMap<u32, StreamSender> = HashMap::new();
    let mut header_buf = [0u8; FRAME_HEADER_SIZE];

    loop {
        // Read frame header
        if reader.read_exact(&mut header_buf).await.is_err() {
            break;
        }

        let stream_id =
            u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        let frame_type = header_buf[4];
        let payload_len =
            u32::from_le_bytes([header_buf[6], header_buf[7], header_buf[8], header_buf[9]])
                as usize;

        // Reject oversized payloads to prevent OOM from malformed frames
        if payload_len > MAX_FRAME_PAYLOAD {
            warn!(
                payload_len,
                max = MAX_FRAME_PAYLOAD,
                "rejecting oversized frame"
            );
            break;
        }

        // Read payload
        let payload = if payload_len > 0 {
            let mut buf = vec![0u8; payload_len];
            if reader.read_exact(&mut buf).await.is_err() {
                break;
            }
            buf
        } else {
            Vec::new()
        };

        match frame_type {
            FRAME_OPEN => {
                // Parse destination from payload (IPv4 or IPv6)
                let (dest_ip, dest_port) = match parse_open_payload(&payload) {
                    Some(addr) => addr,
                    None => {
                        send_frame(&writer_tx, stream_id, FRAME_OPEN_FAIL, &[]).await;
                        continue;
                    }
                };

                // Create per-stream channel (bounded — see STREAM_CHANNEL_CAPACITY)
                let (stream_tx, stream_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
                streams.insert(stream_id, stream_tx);

                // Spawn handler for this stream
                let wtx = writer_tx.clone();
                tokio::spawn(async move {
                    handle_stream(stream_id, dest_ip, dest_port, stream_rx, wtx).await;
                });
            }
            FRAME_DATA | FRAME_CLOSE | FRAME_RST => {
                // Bounded send: when this stream's queue is full (its destination is
                // slower than the guest), awaiting here pauses the vsock read loop.
                // The vsock backpressure propagates to the guest's bounded writer
                // channel and from there to the guest application via TCP flow
                // control, so guest → destination data is never buffered beyond
                // STREAM_CHANNEL_CAPACITY frames per stream.
                let send_failed = match streams.get(&stream_id) {
                    Some(sender) => sender.send((frame_type, payload)).await.is_err(),
                    None => false,
                };
                if send_failed || frame_type == FRAME_CLOSE || frame_type == FRAME_RST {
                    streams.remove(&stream_id);
                }
            }
            _ => {} // Unknown frame type, ignore
        }
    }

    // Session ended — clean up all streams
    streams.clear();
    writer_handle.abort();

    Ok(())
}

/// Handle a single TCP stream: connect to destination, relay DATA frames.
async fn handle_stream(
    stream_id: u32,
    dest_ip: IpAddr,
    dest_port: u16,
    mut stream_rx: mpsc::Receiver<(u8, Vec<u8>)>,
    writer_tx: mpsc::Sender<Vec<u8>>,
) {
    // Connect to real destination (works for both IPv4 and IPv6)
    let tcp = match TcpStream::connect((dest_ip, dest_port)).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                stream_id,
                dest = %format!("{}:{}", dest_ip, dest_port),
                error = %e,
                "egress proxy: TCP connect failed"
            );
            send_frame(&writer_tx, stream_id, FRAME_OPEN_FAIL, &[]).await;
            return;
        }
    };

    // Send OPEN_OK
    send_frame(&writer_tx, stream_id, FRAME_OPEN_OK, &[]).await;

    let (mut tcp_rd, mut tcp_wr) = tcp.into_split();

    // TCP → vsock: read from TCP, send DATA frames.
    // send_frame().await blocks when writer channel is full → backpressure to TCP read.
    let wtx = writer_tx.clone();
    let tcp_to_vsock = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATA_PAYLOAD];
        loop {
            let n = match tcp_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !send_frame(&wtx, stream_id, FRAME_DATA, &buf[..n]).await {
                break; // Writer channel closed (vsock died)
            }
        }
        // Best-effort CLOSE — channel may already be dead
        send_frame(&wtx, stream_id, FRAME_CLOSE, &[]).await;
    });

    // vsock → TCP: receive DATA frames, write to TCP
    let vsock_to_tcp = tokio::spawn(async move {
        while let Some((frame_type, payload)) = stream_rx.recv().await {
            match frame_type {
                FRAME_DATA => {
                    if tcp_wr.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                FRAME_CLOSE | FRAME_RST => break,
                _ => {}
            }
        }
        let _ = tcp_wr.shutdown().await;
    });

    let _ = tokio::join!(tcp_to_vsock, vsock_to_tcp);
}

/// Serialize and send a frame via the bounded writer channel.
/// Blocks when the channel is full, providing backpressure.
/// Returns false if the channel is closed (vsock writer died).
async fn send_frame(
    writer_tx: &mpsc::Sender<Vec<u8>>,
    stream_id: u32,
    frame_type: u8,
    payload: &[u8],
) -> bool {
    let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    frame.extend_from_slice(&stream_id.to_le_bytes());
    frame.push(frame_type);
    frame.push(0); // flags
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    writer_tx.send(frame).await.is_ok()
}
