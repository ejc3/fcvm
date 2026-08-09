//! Transparent egress proxy: intercepts outbound TCP via iptables REDIRECT,
//! tunnels over a single multiplexed vsock connection to host-side relay.
//!
//! Architecture:
//!   Guest app → kernel TCP → iptables REDIRECT → this proxy → vsock mux → host relay → internet
//!
//! All TCP connections share ONE vsock connection, multiplexed by stream_id.
//! Frame format: [stream_id(4)][type(1)][flags(1)][payload_len(4)] + payload
//!
//! Supports both IPv4 and IPv6:
//!   - IPv4: iptables REDIRECT + SO_ORIGINAL_DST
//!   - IPv6: ip6tables REDIRECT + IP6T_SO_ORIGINAL_DST
//!
//! Backpressure: The writer channel is bounded. When the vsock is congested,
//! send_frame().await blocks, which blocks the TCP read loop, which triggers
//! TCP flow control back to the source. This prevents unbounded memory growth.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::vsock::{VsockReadHalf, VsockStream, VsockWriteHalf, EGRESS_PROXY_PORT, HOST_CID};

/// Wait for egress proxy generation to exceed `threshold`, with timeout.
///
/// Used after snapshot restore to confirm the proxy has reconnected its vsock.
/// If the proxy already reconnected, `wait_for` returns immediately (watch retains value).
pub async fn wait_for_egress_gen(
    rx: &watch::Receiver<u64>,
    threshold: u64,
    timeout: std::time::Duration,
    context: &str,
) {
    let mut rx = rx.clone();
    match tokio::time::timeout(timeout, async {
        rx.wait_for(|&v| v > threshold).await.map(|_| ())
    })
    .await
    {
        Ok(Ok(())) => eprintln!("[fc-agent] egress proxy {} (gen={})", context, *rx.borrow()),
        Ok(Err(_)) => eprintln!("[fc-agent] WARNING: egress proxy gen_tx dropped"),
        Err(_) => eprintln!(
            "[fc-agent] WARNING: egress proxy {} timed out ({}s)",
            context,
            timeout.as_secs()
        ),
    }
}

/// Guest-side iptables REDIRECT target port.
///
/// `pub(crate)` because network.rs must EXCLUDE it from `--publish`: a DNAT to
/// this port hands external clients the host-side relay. Importing it rather
/// than repeating the literal means moving this value moves the exclusion too.
pub(crate) const PROXY_LISTEN_PORT: u16 = 12345;

/// Linux SO_ORIGINAL_DST constant (from linux/netfilter_ipv4.h).
const SO_ORIGINAL_DST: libc::c_int = 80;

/// Linux IP6T_SO_ORIGINAL_DST constant (from linux/netfilter_ipv6/ip6_tables.h).
/// Same numeric value as SO_ORIGINAL_DST but used with SOL_IPV6.
const IP6T_SO_ORIGINAL_DST: libc::c_int = 80;

/// Frame header size: stream_id(4) + type(1) + flags(1) + payload_len(4) = 10
const FRAME_HEADER_SIZE: usize = 10;

/// Max DATA frame payload — 32KB prevents head-of-line blocking.
const MAX_DATA_PAYLOAD: usize = 32 * 1024;

/// Writer channel capacity. At 32KB per DATA frame, 1024 slots = max 32MB buffered.
/// Provides enough burst capacity while preventing unbounded memory growth.
const WRITER_CHANNEL_CAPACITY: usize = 1024;

/// Max frame payload we'll accept. Anything larger is a protocol error
/// (prevents OOM from malformed frames claiming enormous payloads).
const MAX_FRAME_PAYLOAD: usize = MAX_DATA_PAYLOAD + 1024;

// Frame types
const FRAME_OPEN: u8 = 1;
const FRAME_DATA: u8 = 2;
const FRAME_CLOSE: u8 = 3;
const FRAME_RST: u8 = 4;
const FRAME_OPEN_OK: u8 = 5;
const FRAME_OPEN_FAIL: u8 = 6;

// Address family constants for OPEN frame
const AF_INET: u8 = 0x04;
const AF_INET6: u8 = 0x06;

/// Per-stream channel sender for routing incoming frames from vsock reader.
/// Unbounded because the vsock reader must never block (would cause HOL blocking
/// across all streams). Backpressure comes from the bounded writer channel upstream.
type StreamSender = mpsc::UnboundedSender<(u8, Vec<u8>)>; // (frame_type, payload)

/// Shared state for the multiplexed proxy.
struct ProxyState {
    /// Maps stream_id → sender for routing incoming frames to per-stream handlers.
    streams: DashMap<u32, StreamSender>,
    /// Bounded channel to the single vsock writer task. When full, send_frame()
    /// blocks, propagating backpressure to TCP reads via flow control.
    writer_tx: mpsc::Sender<Vec<u8>>,
    /// Monotonically increasing stream ID counter.
    next_stream_id: AtomicU32,
}

impl ProxyState {
    /// Serialize and send a frame via the bounded writer channel.
    /// Blocks when the channel is full, providing backpressure.
    /// Returns false if the channel is closed (vsock writer died).
    async fn send_frame(&self, stream_id: u32, frame_type: u8, payload: &[u8]) -> bool {
        let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
        frame.extend_from_slice(&stream_id.to_le_bytes());
        frame.push(frame_type);
        frame.push(0); // flags
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        self.writer_tx.send(frame).await.is_ok()
    }
}

/// Set up iptables/ip6tables REDIRECT rules and start the transparent proxy listener.
///
/// `gen_tx` is a watch channel sender incremented after each successful vsock connect.
/// Waiters use `wait_for(|&v| v > captured)` to detect reconnection — if the proxy
/// already reconnected, the check returns immediately (watch retains latest value).
///
/// No external reconnect signal is needed: after snapshot restore, the kernel sets
/// EPOLLERR on all vsock fds. `VsockStream::wait_for_error()` detects this via
/// `Interest::ERROR`, which fires the select! arm and ends the session naturally.
pub async fn run_egress_proxy(gen_tx: watch::Sender<u64>) {
    // Bind on both IPv4 and IPv6 loopback so we catch redirected traffic from both stacks.
    // ip6tables REDIRECT sends IPv6 traffic to [::1]:PROXY_LISTEN_PORT.
    //
    // Bind BEFORE installing any REDIRECT rule: a rule pointing at a port nothing
    // listens on turns every redirected guest connection into an immediate refusal.
    //
    // Use TcpSocket to set a large listen backlog (8192 instead of default 128).
    // With 8000+ concurrent connections, the default backlog causes accept queue
    // overflow and connection drops under load.
    let listener_v4 = match bind_with_backlog("127.0.0.1", PROXY_LISTEN_PORT, false).await {
        Ok(l) => {
            eprintln!(
                "[fc-agent] egress proxy listening on 127.0.0.1:{} (backlog=8192)",
                PROXY_LISTEN_PORT
            );
            l
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to bind egress proxy on 127.0.0.1:{}: {} (egress proxy disabled)",
                PROXY_LISTEN_PORT, e
            );
            return;
        }
    };

    if !setup_iptables_redirect_v4() {
        eprintln!(
            "[fc-agent] WARNING: failed to set up iptables REDIRECT rules, egress proxy disabled"
        );
        return;
    }

    let listener_v6 = match bind_with_backlog("::1", PROXY_LISTEN_PORT, true).await {
        Ok(l) => {
            eprintln!(
                "[fc-agent] egress proxy listening on [::1]:{} (backlog=8192)",
                PROXY_LISTEN_PORT
            );
            // Only redirect IPv6 once its listener exists; failure leaves IPv6 unproxied.
            setup_ip6tables_redirect();
            Some(l)
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to bind egress proxy on [::1]:{}: {} (IPv6 proxy disabled)",
                PROXY_LISTEN_PORT, e
            );
            None
        }
    };

    let mut count: u64 = 0;

    loop {
        // Connect vsock and run multiplexed proxy until connection dies
        let vsock = match VsockStream::connect(HOST_CID, EGRESS_PROXY_PORT) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[fc-agent] egress proxy vsock connect failed: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        count += 1;
        eprintln!("[fc-agent] egress proxy vsock connected (gen={})", count);
        let _ = gen_tx.send(count);

        run_mux_session(&listener_v4, listener_v6.as_ref(), vsock).await;

        eprintln!("[fc-agent] egress proxy vsock disconnected, reconnecting...");
    }
}

/// Run a single multiplexed session over one vsock connection.
/// Returns when the vsock connection dies (reader/writer error or EPOLLERR).
async fn run_mux_session(
    listener_v4: &TcpListener,
    listener_v6: Option<&TcpListener>,
    vsock: VsockStream,
) {
    let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(WRITER_CHANNEL_CAPACITY);
    let state = Arc::new(ProxyState {
        streams: DashMap::new(),
        writer_tx,
        next_stream_id: AtomicU32::new(1),
    });

    let (vsock_read, vsock_write) = vsock.split();

    // Writer task: drains channel and writes frames to vsock
    let writer_handle = tokio::spawn(vsock_writer(writer_rx, vsock_write));

    // Reader task: reads frames from vsock and routes to per-stream handlers
    let reader_state = state.clone();
    let reader_handle = tokio::spawn(vsock_reader(vsock_read, reader_state));

    // Accept loop: runs inline until vsock dies (via select!)
    let accept_state = state.clone();
    let accept_loop = async {
        loop {
            // Accept from both IPv4 and IPv6 listeners
            let client = if let Some(v6) = listener_v6 {
                tokio::select! {
                    result = listener_v4.accept() => result,
                    result = v6.accept() => result,
                }
            } else {
                listener_v4.accept().await
            };

            match client {
                Ok((client, _)) => {
                    let s = accept_state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_redirected_connection(client, s).await {
                            eprintln!("[fc-agent] egress proxy connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[fc-agent] WARNING: egress proxy accept error: {}", e);
                }
            }
        }
    };

    // Wait for reader/writer to finish (vsock died) or EPOLLERR (transport reset).
    // After snapshot restore, the kernel sets EPOLLERR on all vsock fds.
    // wait_for_error() detects this via Interest::ERROR — tokio's poll_read_ready
    // misses EPOLLERR (Direction::Read mask excludes ERROR), but Interest::ERROR
    // catches it natively. This fires before the reader/writer notice, providing
    // instant session teardown without external signals.
    let session_start = std::time::Instant::now();
    tokio::select! {
        _ = reader_handle => {
            eprintln!("[fc-agent] egress proxy session ended: reader exited ({}ms since session start)", session_start.elapsed().as_millis());
        },
        _ = writer_handle => {
            eprintln!("[fc-agent] egress proxy session ended: writer exited ({}ms since session start)", session_start.elapsed().as_millis());
        },
        _ = accept_loop => {
            eprintln!("[fc-agent] egress proxy session ended: accept loop exited ({}ms since session start)", session_start.elapsed().as_millis());
        },
        _ = vsock.wait_for_error() => {
            eprintln!("[fc-agent] egress proxy session ended: vsock EPOLLERR (transport reset) ({}ms since session start)", session_start.elapsed().as_millis());
        },
    }

    // RST all active streams
    for entry in state.streams.iter() {
        let _ = entry.value().send((FRAME_RST, Vec::new()));
    }
    state.streams.clear();
}

/// Writer task: reads serialized frames from channel and writes to vsock.
async fn vsock_writer(mut rx: mpsc::Receiver<Vec<u8>>, mut writer: VsockWriteHalf) {
    while let Some(frame) = rx.recv().await {
        if let Err(e) = writer.write_all(&frame).await {
            eprintln!("[fc-agent] egress proxy writer error: {}", e);
            break;
        }
    }
}

/// Reader task: reads frames from vsock and dispatches to per-stream handlers.
async fn vsock_reader(mut reader: VsockReadHalf, state: Arc<ProxyState>) {
    let mut header_buf = [0u8; FRAME_HEADER_SIZE];

    loop {
        // Read frame header
        if let Err(e) = reader.read_exact(&mut header_buf).await {
            eprintln!("[fc-agent] egress proxy reader error: {}", e);
            break;
        }

        let stream_id =
            u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        let frame_type = header_buf[4];
        // header_buf[5] = flags (reserved)
        let payload_len =
            u32::from_le_bytes([header_buf[6], header_buf[7], header_buf[8], header_buf[9]])
                as usize;

        // Reject oversized payloads to prevent OOM from malformed frames
        if payload_len > MAX_FRAME_PAYLOAD {
            eprintln!(
                "[fc-agent] egress proxy: rejecting frame with payload_len={}, max={}",
                payload_len, MAX_FRAME_PAYLOAD
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

        // Route to stream handler
        if let Some(sender) = state.streams.get(&stream_id) {
            let _ = sender.send((frame_type, payload));
        }
        // If stream not found, drop the frame (stream already closed)

        // Clean up on CLOSE/RST
        if frame_type == FRAME_CLOSE || frame_type == FRAME_RST {
            state.streams.remove(&stream_id);
        }
    }
}

/// Handle a single redirected TCP connection: get original destination,
/// open a multiplexed stream, relay data bidirectionally.
async fn handle_redirected_connection(
    mut client: tokio::net::TcpStream,
    state: Arc<ProxyState>,
) -> anyhow::Result<()> {
    // Determine if this is an IPv4 or IPv6 connection from the local address
    let local_addr = client.local_addr()?;
    let (dest_addr, dest_port) = match local_addr {
        SocketAddr::V4(_) => {
            let (ip, port) = get_original_dst_v4(client.as_raw_fd())?;
            (IpAddr::V4(ip), port)
        }
        SocketAddr::V6(_) => {
            let (ip, port) = get_original_dst_v6(client.as_raw_fd())?;
            (IpAddr::V6(ip), port)
        }
    };

    // Assign stream ID
    let stream_id = state.next_stream_id.fetch_add(1, Ordering::Relaxed);

    // Create per-stream channel for receiving frames from vsock reader
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
    state.streams.insert(stream_id, stream_tx);

    // Send OPEN frame with address family
    let open_payload = match dest_addr {
        IpAddr::V4(ip) => {
            // [family(1)][ip(4)][port(2)] = 7 bytes
            let mut buf = [0u8; 7];
            buf[0] = AF_INET;
            buf[1..5].copy_from_slice(&ip.octets());
            buf[5..7].copy_from_slice(&dest_port.to_be_bytes());
            buf.to_vec()
        }
        IpAddr::V6(ip) => {
            // [family(1)][ip(16)][port(2)] = 19 bytes
            let mut buf = [0u8; 19];
            buf[0] = AF_INET6;
            buf[1..17].copy_from_slice(&ip.octets());
            buf[17..19].copy_from_slice(&dest_port.to_be_bytes());
            buf.to_vec()
        }
    };
    state.send_frame(stream_id, FRAME_OPEN, &open_payload).await;

    // Wait for OPEN_OK or OPEN_FAIL
    let response = tokio::time::timeout(std::time::Duration::from_secs(30), stream_rx.recv()).await;

    match response {
        Ok(Some((FRAME_OPEN_OK, _))) => {} // Connection succeeded
        Ok(Some((FRAME_OPEN_FAIL, _))) | Ok(Some((FRAME_RST, _))) => {
            state.streams.remove(&stream_id);
            anyhow::bail!("host refused connection to {}:{}", dest_addr, dest_port);
        }
        _ => {
            state.streams.remove(&stream_id);
            anyhow::bail!(
                "timeout waiting for OPEN_OK for {}:{}",
                dest_addr,
                dest_port
            );
        }
    }

    // Bidirectional relay
    let (mut client_rd, mut client_wr) = client.split();

    // TCP → vsock: read from client, send DATA frames.
    // send_frame().await blocks when writer channel is full → backpressure to TCP read.
    let writer_state = state.clone();
    let tcp_to_vsock = async move {
        let mut buf = vec![0u8; MAX_DATA_PAYLOAD];
        loop {
            let n = match client_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !writer_state
                .send_frame(stream_id, FRAME_DATA, &buf[..n])
                .await
            {
                break; // Writer channel closed (vsock died)
            }
        }
        // Client closed — send CLOSE (best-effort, channel may be dead)
        writer_state.send_frame(stream_id, FRAME_CLOSE, &[]).await;
    };

    // vsock → TCP: receive DATA frames from per-stream channel, write to client
    let vsock_to_tcp = async move {
        while let Some((frame_type, payload)) = stream_rx.recv().await {
            match frame_type {
                FRAME_DATA => {
                    if client_wr.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                FRAME_CLOSE | FRAME_RST => break,
                _ => {}
            }
        }
        let _ = client_wr.shutdown().await;
    };

    tokio::join!(tcp_to_vsock, vsock_to_tcp);

    state.streams.remove(&stream_id);
    Ok(())
}

/// Listen backlog for the proxy listener. With 8000+ concurrent connections,
/// the default backlog of 128 causes accept queue overflow under load.
const LISTEN_BACKLOG: u32 = 8192;

/// Bind a TCP listener with a large backlog.
/// Uses TcpSocket to control the listen() backlog parameter, which
/// TcpListener::bind() hardcodes to 128.
async fn bind_with_backlog(
    addr: &str,
    port: u16,
    ipv6: bool,
) -> Result<TcpListener, std::io::Error> {
    let socket = if ipv6 {
        tokio::net::TcpSocket::new_v6()?
    } else {
        tokio::net::TcpSocket::new_v4()?
    };
    socket.set_reuseaddr(true)?;
    let bind_addr: std::net::SocketAddr = if ipv6 {
        format!("[{}]:{}", addr, port)
    } else {
        format!("{}:{}", addr, port)
    }
    .parse()
    .map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad addr: {}", e))
    })?;
    socket.bind(bind_addr)?;
    socket.listen(LISTEN_BACKLOG)
}

/// Set up iptables (IPv4) NAT OUTPUT REDIRECT rules.
fn setup_iptables_redirect_v4() -> bool {
    let port_str = PROXY_LISTEN_PORT.to_string();

    // IPv4 rules
    let v4_rules: &[&[&str]] = &[
        // Don't redirect localhost traffic (proxy listens on 127.0.0.1)
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "127.0.0.0/8",
            "-j",
            "RETURN",
        ],
        // Don't redirect local subnet (pasta gateway, DNS, health checks)
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "10.0.2.0/24",
            "-j",
            "RETURN",
        ],
        // Don't redirect link-local traffic (169.254.0.0/16) — includes MMDS at 169.254.169.254
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "169.254.0.0/16",
            "-j",
            "RETURN",
        ],
        // Redirect all other outbound TCP to proxy
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-j",
            "REDIRECT",
            "--to-port",
            &port_str,
        ],
    ];

    for rule in v4_rules {
        let output = std::process::Command::new("iptables").args(*rule).output();
        match output {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "[fc-agent] WARNING: iptables rule {:?} failed: {}",
                    rule, stderr
                );
                return false;
            }
            Err(e) => {
                eprintln!("[fc-agent] WARNING: failed to run iptables: {}", e);
                return false;
            }
        }
    }
    eprintln!("[fc-agent] iptables REDIRECT rules configured for egress proxy");
    true
}

/// Set up ip6tables NAT OUTPUT REDIRECT rules — best-effort (ip6tables may not be available).
/// Only called once the IPv6 proxy listener is bound, so a failure here leaves IPv6
/// egress unproxied rather than redirected to a dead port.
fn setup_ip6tables_redirect() -> bool {
    let port_str = PROXY_LISTEN_PORT.to_string();

    let v6_rules: &[&[&str]] = &[
        // Don't redirect localhost traffic (proxy listens on [::1])
        &[
            "-t", "nat", "-A", "OUTPUT", "-p", "tcp", "-d", "::1/128", "-j", "RETURN",
        ],
        // Don't redirect link-local (fe80::/10)
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "fe80::/10",
            "-j",
            "RETURN",
        ],
        // Don't redirect pasta ULA subnet (fd00::/64)
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "fd00::/64",
            "-j",
            "RETURN",
        ],
        // Redirect all other outbound IPv6 TCP to proxy
        &[
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-j",
            "REDIRECT",
            "--to-port",
            &port_str,
        ],
    ];

    for rule in v6_rules {
        let output = std::process::Command::new("ip6tables").args(*rule).output();
        match output {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "[fc-agent] WARNING: ip6tables rule {:?} failed: {} (IPv6 proxy disabled)",
                    rule, stderr
                );
                return false;
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: ip6tables not available: {} (IPv6 proxy disabled)",
                    e
                );
                return false;
            }
        }
    }
    eprintln!("[fc-agent] ip6tables REDIRECT rules configured for egress proxy");

    true
}

/// Get the original IPv4 destination address from a REDIRECT'd socket via SO_ORIGINAL_DST.
fn get_original_dst_v4(fd: std::os::fd::RawFd) -> anyhow::Result<(Ipv4Addr, u16)> {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };

    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);

    Ok((ip, port))
}

/// Get the original IPv6 destination address from a REDIRECT'd socket via IP6T_SO_ORIGINAL_DST.
fn get_original_dst_v6(fd: std::os::fd::RawFd) -> anyhow::Result<(Ipv6Addr, u16)> {
    let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IPV6,
            IP6T_SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };

    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let ip = Ipv6Addr::from(addr.sin6_addr.s6_addr);
    let port = u16::from_be(addr.sin6_port);

    Ok((ip, port))
}
