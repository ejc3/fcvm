//! Transparent egress proxy: intercepts outbound TCP via iptables REDIRECT,
//! tunnels over vsock to host-side relay for near-native egress performance.
//!
//! Architecture:
//!   Guest app → kernel TCP → iptables REDIRECT → this proxy → vsock → host relay → internet
//!
//! This bypasses the TAP/bridge/pasta data path entirely for outbound TCP.

use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;

use tokio::net::TcpListener;

use crate::vsock::{VsockStream, EGRESS_PROXY_PORT, HOST_CID};

/// Guest-side iptables REDIRECT target port.
const PROXY_LISTEN_PORT: u16 = 12345;

/// Linux SO_ORIGINAL_DST constant (from linux/netfilter_ipv4.h).
const SO_ORIGINAL_DST: libc::c_int = 80;

/// Set up iptables REDIRECT rules and start the transparent proxy listener.
///
/// This function runs forever, accepting redirected TCP connections and
/// tunneling them over vsock to the host-side relay.
pub async fn run_egress_proxy() {
    if !setup_iptables_redirect() {
        eprintln!(
            "[fc-agent] WARNING: failed to set up iptables REDIRECT rules, egress proxy disabled"
        );
        return;
    }

    let listener = match TcpListener::bind(("127.0.0.1", PROXY_LISTEN_PORT)).await {
        Ok(l) => {
            eprintln!(
                "[fc-agent] egress proxy listening on 127.0.0.1:{}",
                PROXY_LISTEN_PORT
            );
            l
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to bind egress proxy on port {}: {}",
                PROXY_LISTEN_PORT, e
            );
            return;
        }
    };

    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_redirected_connection(client).await {
                        // Only log non-routine errors at debug level
                        let _ = e;
                    }
                });
            }
            Err(e) => {
                eprintln!("[fc-agent] WARNING: egress proxy accept error: {}", e);
            }
        }
    }
}

/// Set up iptables NAT OUTPUT REDIRECT rules.
fn setup_iptables_redirect() -> bool {
    let port_str = PROXY_LISTEN_PORT.to_string();
    let rules: &[&[&str]] = &[
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

    for rule in rules {
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

/// Buffer size for relay — larger buffers reduce syscall overhead.
/// 256KB matches typical TCP window sizes and vsock buffer limits.
const RELAY_BUF_SIZE: usize = 256 * 1024;

/// Handle a single redirected TCP connection: get original destination,
/// connect to host via vsock, relay data bidirectionally.
async fn handle_redirected_connection(mut client: tokio::net::TcpStream) -> anyhow::Result<()> {
    // Get original destination via SO_ORIGINAL_DST
    let (dest_ip, dest_port) = get_original_dst(client.as_raw_fd())?;

    // Connect to host via vsock
    let mut vsock = VsockStream::connect(HOST_CID, EGRESS_PROXY_PORT)?;

    // Send connection header: [version(1)][family(1)][ip(4)][port(2)] = 8 bytes
    let mut header = [0u8; 8];
    header[0] = 0x01; // version
    header[1] = 0x04; // IPv4
    header[2..6].copy_from_slice(&dest_ip.octets());
    header[6..8].copy_from_slice(&dest_port.to_be_bytes());
    vsock.write_all(&header).await?;

    // Bidirectional relay with large buffers (256KB vs copy_bidirectional's 8KB)
    let (mut client_rd, mut client_wr) = client.split();
    let (mut vsock_rd, mut vsock_wr) = tokio::io::split(&mut vsock);

    let client_to_vsock = async {
        let mut buf = vec![0u8; RELAY_BUF_SIZE];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut client_rd, &mut buf).await?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut vsock_wr, &buf[..n]).await?;
        }
        tokio::io::AsyncWriteExt::shutdown(&mut vsock_wr).await?;
        Ok::<_, std::io::Error>(())
    };

    let vsock_to_client = async {
        let mut buf = vec![0u8; RELAY_BUF_SIZE];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut vsock_rd, &mut buf).await?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut client_wr, &buf[..n]).await?;
        }
        tokio::io::AsyncWriteExt::shutdown(&mut client_wr).await?;
        Ok::<_, std::io::Error>(())
    };

    let _ = tokio::join!(client_to_vsock, vsock_to_client);

    Ok(())
}

/// Get the original destination address from a REDIRECT'd socket via SO_ORIGINAL_DST.
fn get_original_dst(fd: std::os::fd::RawFd) -> anyhow::Result<(Ipv4Addr, u16)> {
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
