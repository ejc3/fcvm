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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How long a stream may wait on its destination before the session reports it: a
/// connect still pending, or bytes written to the destination with nothing back since.
/// A pull's first reply arrived within 5.5 s in 7,610 of 7,611 CI pulls (2026-11-13);
/// the other waited 117 s with no record of where its request was.
const STALL_REPORT_AFTER: Duration = Duration::from_secs(10);
const STALL_SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Milliseconds since this process first asked, the clock the stream records use.
fn now_ms() -> u64 {
    static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    EPOCH.elapsed().as_millis() as u64
}

/// What one stream has carried, the host half of a stall report. Times are
/// `now_ms() + 1`; 0 means never.
struct StreamActivity {
    stream_id: u32,
    dest: SocketAddr,
    opened_ms: u64,
    connected: AtomicBool,
    to_dest: AtomicU64,
    from_dest: AtomicU64,
    last_to_dest_ms: AtomicU64,
    last_from_dest_ms: AtomicU64,
    /// Which way the latest bytes went ([`SENT`] or [`RECEIVED`], 0 before any). The
    /// timestamps above are milliseconds, and a request written in the millisecond its
    /// predecessor's reply arrived has the same stamp as that reply, so they cannot order
    /// the two.
    last_direction: std::sync::atomic::AtomicU8,
    /// When a write to the destination began that has not completed (`now_ms() + 1`; 0
    /// when none is pending), and how many bytes it carries. A destination that stops
    /// reading leaves the write pending once the socket buffers fill: those bytes have not
    /// been written to it.
    writing_since_ms: AtomicU64,
    pending: AtomicU64,
    /// The wait last reported (a `waiting_on` string), so each wait is reported once and
    /// a change of wait is reported again; `None` once the stream stops waiting.
    reported: Mutex<Option<&'static str>>,
}

impl StreamActivity {
    fn new(stream_id: u32, dest: SocketAddr) -> Self {
        Self {
            stream_id,
            dest,
            opened_ms: now_ms() + 1,
            connected: AtomicBool::new(false),
            to_dest: AtomicU64::new(0),
            from_dest: AtomicU64::new(0),
            last_to_dest_ms: AtomicU64::new(0),
            last_from_dest_ms: AtomicU64::new(0),
            last_direction: std::sync::atomic::AtomicU8::new(0),
            writing_since_ms: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            reported: Mutex::new(None),
        }
    }

    /// Before a write of `n` bytes to the destination. The request is the latest event from
    /// here on: a reply can arrive before the write returns, and must then stay the latest.
    fn begin_write(&self, n: usize) {
        let now = now_ms() + 1;
        self.pending.store(n as u64, Ordering::Relaxed);
        self.writing_since_ms.store(now, Ordering::Relaxed);
        self.last_to_dest_ms.store(now, Ordering::Relaxed);
        self.last_direction.store(SENT, Ordering::Relaxed);
    }

    /// After the write [`Self::begin_write`] began has returned: its bytes are written.
    fn end_write(&self, n: usize) {
        self.to_dest.fetch_add(n as u64, Ordering::Relaxed);
        self.writing_since_ms.store(0, Ordering::Relaxed);
    }

    fn record(&self, direction: u8, n: usize) {
        let (counter, last) = if direction == SENT {
            (&self.to_dest, &self.last_to_dest_ms)
        } else {
            (&self.from_dest, &self.last_from_dest_ms)
        };
        counter.fetch_add(n as u64, Ordering::Relaxed);
        last.store(now_ms() + 1, Ordering::Relaxed);
        self.last_direction.store(direction, Ordering::Relaxed);
    }

    /// What this stream has waited on for `threshold_ms` at `now_ms`, if anything.
    fn waiting_on(&self, now_ms: u64, threshold_ms: u64) -> Option<&'static str> {
        if !self.connected.load(Ordering::Relaxed) {
            return (now_ms.saturating_sub(self.opened_ms - 1) >= threshold_ms)
                .then_some("connecting to its destination");
        }
        let writing = self.writing_since_ms.load(Ordering::Relaxed);
        if writing != 0 {
            return (now_ms.saturating_sub(writing - 1) >= threshold_ms)
                .then_some("blocked writing to its destination");
        }
        let sent = self.last_to_dest_ms.load(Ordering::Relaxed);
        // The latest bytes went to the destination, and nothing has come back since.
        // `sent` is read before the direction, so a first send recorded in between shows
        // the direction with the stamp still 0; that read is not yet a wait.
        (sent != 0
            && self.last_direction.load(Ordering::Relaxed) == SENT
            && now_ms.saturating_sub(sent - 1) >= threshold_ms)
            .then_some("awaiting a reply from its destination")
    }

    fn describe(&self, waiting_on: &str, now_ms: u64) -> String {
        let ago = |stamp: u64| {
            if stamp == 0 {
                "never".to_string()
            } else {
                format!(
                    "{:.1}s ago",
                    now_ms.saturating_sub(stamp - 1) as f64 / 1000.0
                )
            }
        };
        format!(
            "stream {} to {} {waiting_on} for {:.1}s since open: wrote {} B to it (last {}), {} B pending, read {} B from it (last {})",
            self.stream_id,
            self.dest,
            now_ms.saturating_sub(self.opened_ms - 1) as f64 / 1000.0,
            self.to_dest.load(Ordering::Relaxed),
            ago(self.last_to_dest_ms.load(Ordering::Relaxed)),
            if self.writing_since_ms.load(Ordering::Relaxed) == 0 {
                0
            } else {
                self.pending.load(Ordering::Relaxed)
            },
            self.from_dest.load(Ordering::Relaxed),
            ago(self.last_from_dest_ms.load(Ordering::Relaxed)),
        )
    }
}

/// [`StreamActivity::last_direction`]: bytes went to the destination.
const SENT: u8 = 1;
/// [`StreamActivity::last_direction`]: bytes came back from the destination.
const RECEIVED: u8 = 2;

/// A session's open streams, shared with its stall scan.
type ActivityRegistry = Arc<Mutex<HashMap<u32, Arc<StreamActivity>>>>;

/// Streams that started waiting on their destination at least `threshold_ms` before
/// `now_ms`, each once per wait.
fn newly_stalled(registry: &ActivityRegistry, now_ms: u64, threshold_ms: u64) -> Vec<String> {
    let streams: Vec<Arc<StreamActivity>> = registry.lock().unwrap().values().cloned().collect();
    let mut lines = Vec::new();
    for stream in streams {
        match stream.waiting_on(now_ms, threshold_ms) {
            Some(waiting_on) => {
                let mut reported = stream.reported.lock().unwrap();
                if *reported != Some(waiting_on) {
                    *reported = Some(waiting_on);
                    lines.push(stream.describe(waiting_on, now_ms));
                }
            }
            None => *stream.reported.lock().unwrap() = None,
        }
    }
    lines
}

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
    let registry = ActivityRegistry::default();
    let scan_registry = registry.clone();
    let stall_scan = tokio::spawn(async move {
        let mut interval = tokio::time::interval(STALL_SCAN_INTERVAL);
        loop {
            interval.tick().await;
            for line in newly_stalled(
                &scan_registry,
                now_ms(),
                STALL_REPORT_AFTER.as_millis() as u64,
            ) {
                info!("egress proxy: {line}");
            }
        }
    });
    let result = relay_mux_session(vsock_stream, registry).await;
    stall_scan.abort();
    result
}

/// Read frames from one vsock session, dispatch them to per-stream handlers, and relay to
/// the real TCP destinations, recording each stream in `registry`.
async fn relay_mux_session(vsock_stream: UnixStream, registry: ActivityRegistry) -> Result<()> {
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
                let activity = Arc::new(StreamActivity::new(
                    stream_id,
                    SocketAddr::new(dest_ip, dest_port),
                ));
                registry.lock().unwrap().insert(stream_id, activity.clone());
                let registry = registry.clone();
                tokio::spawn(async move {
                    handle_stream(stream_id, dest_ip, dest_port, stream_rx, wtx, &activity).await;
                    let mut open = registry.lock().unwrap();
                    // A stream id the guest has reused since names another stream.
                    if open
                        .get(&stream_id)
                        .is_some_and(|current| Arc::ptr_eq(current, &activity))
                    {
                        open.remove(&stream_id);
                    }
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
    activity: &Arc<StreamActivity>,
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

    activity.connected.store(true, Ordering::Relaxed);
    // Send OPEN_OK
    send_frame(&writer_tx, stream_id, FRAME_OPEN_OK, &[]).await;

    let (mut tcp_rd, mut tcp_wr) = tcp.into_split();

    // TCP → vsock: read from TCP, send DATA frames.
    // send_frame().await blocks when writer channel is full → backpressure to TCP read.
    let wtx = writer_tx.clone();
    let down = activity.clone();
    let tcp_to_vsock = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATA_PAYLOAD];
        loop {
            let n = match tcp_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            down.record(RECEIVED, n);
            if !send_frame(&wtx, stream_id, FRAME_DATA, &buf[..n]).await {
                break; // Writer channel closed (vsock died)
            }
        }
        // Best-effort CLOSE — channel may already be dead
        send_frame(&wtx, stream_id, FRAME_CLOSE, &[]).await;
    });

    // vsock → TCP: receive DATA frames, write to TCP
    let up = activity.clone();
    let vsock_to_tcp = tokio::spawn(async move {
        while let Some((frame_type, payload)) = stream_rx.recv().await {
            match frame_type {
                FRAME_DATA => {
                    // Pending before the write and sent after it, so a write the destination
                    // stops taking reads as pending, and the bytes it has read are counted
                    // by the time the write returns.
                    up.begin_write(payload.len());
                    if tcp_wr.write_all(&payload).await.is_err() {
                        break;
                    }
                    up.end_write(payload.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream_id: u32, frame_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = stream_id.to_le_bytes().to_vec();
        frame.extend([frame_type, 0]);
        frame.extend((payload.len() as u32).to_le_bytes());
        frame.extend(payload);
        frame
    }

    async fn read_frame(guest: &mut UnixStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; FRAME_HEADER_SIZE];
        guest.read_exact(&mut header).await.unwrap();
        let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        guest.read_exact(&mut payload).await.unwrap();
        (header[4], payload)
    }

    /// A request the destination never answers is reported once, with the bytes written
    /// to it and none read back, and a reply ends the wait. Each count is recorded before
    /// the step the test waits on, and the scan is handed a clock past the threshold, so
    /// nothing here depends on timing.
    /// A destination that stops reading leaves the request's write pending once its socket
    /// buffers fill. Those bytes were not written to it, and the stream is blocked writing,
    /// not awaiting a reply.
    #[tokio::test]
    async fn a_write_the_destination_never_takes_is_reported_as_blocked() {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = destination.local_addr().unwrap().port();
        let (mut guest, host) = UnixStream::pair().unwrap();
        let registry = ActivityRegistry::default();
        let session = tokio::spawn(relay_mux_session(host, registry.clone()));

        let mut open = vec![AF_INET, 127, 0, 0, 1];
        open.extend(port.to_be_bytes());
        guest.write_all(&frame(7, FRAME_OPEN, &open)).await.unwrap();
        // Accepted and never read: the request fills the socket buffers and then waits.
        let (_server, _) = destination.accept().await.unwrap();
        assert_eq!(read_frame(&mut guest).await.0, FRAME_OPEN_OK);

        // 64 MiB is several times what loopback buffers hold, so a write is left pending.
        let (guest_rd, mut guest_wr) = guest.into_split();
        let writer = tokio::spawn(async move {
            let chunk = vec![0u8; MAX_DATA_PAYLOAD];
            for _ in 0..(64 * 1024 * 1024 / MAX_DATA_PAYLOAD) {
                if guest_wr
                    .write_all(&frame(7, FRAME_DATA, &chunk))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let threshold = STALL_REPORT_AFTER.as_millis() as u64;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        // The report for a time past the threshold. Until a write is left pending the
        // stream can read as awaiting a reply; the pending write must then be reported.
        let mut seen = Vec::new();
        let line = loop {
            let lines = newly_stalled(&registry, now_ms() + threshold, threshold);
            if let Some(line) = lines.iter().find(|l| l.contains("blocked writing")) {
                break line.clone();
            }
            seen.extend(lines);
            assert!(
                std::time::Instant::now() < deadline,
                "a write the destination never took was never reported as pending; reported: {seen:?}"
            );
            tokio::task::yield_now().await;
        };
        assert!(
            line.contains(&format!("stream 7 to 127.0.0.1:{port} blocked writing")),
            "{line}"
        );

        writer.abort();
        drop(guest_rd);
        session.abort();
    }

    /// A reply can be read before the relay's write of the request returns. Finishing the
    /// write's bookkeeping afterwards must not make the reply look older than the request.
    #[test]
    fn a_reply_read_before_the_write_returns_still_answers_it() {
        let stream = StreamActivity::new(9, "127.0.0.1:443".parse().unwrap());
        stream.connected.store(true, Ordering::Relaxed);
        stream.begin_write(30);
        stream.record(RECEIVED, 20);
        stream.end_write(30);
        let threshold = STALL_REPORT_AFTER.as_millis() as u64;
        assert_eq!(
            stream.waiting_on(now_ms() + threshold, threshold),
            None,
            "an answered request was reported as waiting"
        );
        assert_eq!(stream.to_dest.load(Ordering::Relaxed), 30);
    }

    /// A request written in the millisecond the previous reply arrived carries the same
    /// stamp as that reply. It is still the latest event, and still unanswered.
    #[test]
    fn a_request_in_the_reply_s_millisecond_is_still_awaiting_its_answer() {
        let stream = StreamActivity::new(9, "127.0.0.1:443".parse().unwrap());
        stream.connected.store(true, Ordering::Relaxed);
        stream.record(SENT, 10);
        stream.record(RECEIVED, 20);
        stream.record(SENT, 30);
        let stamp = stream.last_to_dest_ms.load(Ordering::Relaxed);
        stream.last_from_dest_ms.store(stamp, Ordering::Relaxed);
        let threshold = STALL_REPORT_AFTER.as_millis() as u64;
        assert_eq!(
            stream.waiting_on(stamp - 1 + threshold, threshold),
            Some("awaiting a reply from its destination"),
            "a request sent after the last reply, in the same millisecond, was read as answered"
        );
        stream.record(RECEIVED, 5);
        assert_eq!(stream.waiting_on(stamp - 1 + threshold, threshold), None);
    }

    #[tokio::test]
    async fn a_request_its_destination_never_answers_is_reported_once_with_its_bytes() {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = destination.local_addr().unwrap().port();
        let (mut guest, host) = UnixStream::pair().unwrap();
        let registry = ActivityRegistry::default();
        let session = tokio::spawn(relay_mux_session(host, registry.clone()));

        let mut open = vec![AF_INET, 127, 0, 0, 1];
        open.extend(port.to_be_bytes());
        guest.write_all(&frame(7, FRAME_OPEN, &open)).await.unwrap();
        let (mut server, _) = destination.accept().await.unwrap();
        assert_eq!(read_frame(&mut guest).await.0, FRAME_OPEN_OK);

        let request = b"GET /v2/ HTTP/1.1\r\n\r\n";
        guest
            .write_all(&frame(7, FRAME_DATA, request))
            .await
            .unwrap();
        let mut received = vec![0u8; request.len()];
        server.read_exact(&mut received).await.unwrap();
        // The destination has the bytes; the relay counts them once its write returns.
        let stream = registry.lock().unwrap().get(&7).cloned().expect("stream 7");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while stream.to_dest.load(Ordering::Relaxed) != request.len() as u64 {
            assert!(
                std::time::Instant::now() < deadline,
                "the relay's write never completed"
            );
            tokio::task::yield_now().await;
        }

        let threshold = STALL_REPORT_AFTER.as_millis() as u64;
        let lines = newly_stalled(&registry, now_ms() + threshold, threshold);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(
            line.contains(&format!("stream 7 to 127.0.0.1:{port} awaiting a reply")),
            "{line}"
        );
        assert!(
            line.contains(&format!("wrote {} B to it", request.len())),
            "{line}"
        );
        assert!(line.contains("read 0 B from it (last never)"), "{line}");
        assert!(
            newly_stalled(&registry, now_ms() + threshold, threshold).is_empty(),
            "one wait was reported twice"
        );

        let reply = b"HTTP/1.1 401 Unauthorized\r\n\r\n";
        server.write_all(reply).await.unwrap();
        assert_eq!(read_frame(&mut guest).await, (FRAME_DATA, reply.to_vec()));
        assert!(
            newly_stalled(&registry, now_ms() + threshold, threshold).is_empty(),
            "an answered request is still reported"
        );

        drop(guest);
        session.await.unwrap().unwrap();
    }
}
