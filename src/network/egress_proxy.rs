//! Host-side egress proxy: accepts vsock connections from guest transparent proxy,
//! reads destination header, opens real TCP connections to the internet.
//!
//! Architecture:
//!   Guest fc-agent → vsock → Firecracker → Unix socket → this proxy → real TCP → internet
//!
//! Runs OUTSIDE the network namespace, so it has direct internet access.

use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UnixListener};
use tracing::{debug, info, warn};

/// Vsock port for egress proxy (must match fc-agent/src/proxy.rs EGRESS_PROXY_PORT).
pub const VSOCK_EGRESS_PROXY_PORT: u32 = 52000;

/// Run the host-side egress proxy.
///
/// Listens on the Firecracker vsock Unix socket path for guest connections.
/// Each connection carries a header with the real destination, followed by
/// raw TCP data to relay.
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
                tokio::spawn(async move {
                    if let Err(e) = handle_proxy_connection(stream).await {
                        debug!(error = %e, "egress proxy connection error");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "egress proxy accept error");
            }
        }
    }
}

/// Handle a single proxy connection: read header, connect to destination, relay.
async fn handle_proxy_connection(mut vsock_stream: tokio::net::UnixStream) -> Result<()> {
    // Read 8-byte header: [version(1)][family(1)][ip(4)][port(2)]
    let mut header = [0u8; 8];
    vsock_stream
        .read_exact(&mut header)
        .await
        .context("reading proxy header")?;

    let version = header[0];
    if version != 0x01 {
        anyhow::bail!("unsupported proxy protocol version: {}", version);
    }

    let family = header[1];
    if family != 0x04 {
        anyhow::bail!(
            "unsupported address family: {} (only IPv4 supported)",
            family
        );
    }

    let dest_ip = Ipv4Addr::new(header[2], header[3], header[4], header[5]);
    let dest_port = u16::from_be_bytes([header[6], header[7]]);

    debug!(dest = %format!("{}:{}", dest_ip, dest_port), "connecting to destination");

    // Connect to real destination
    let mut upstream = TcpStream::connect((dest_ip, dest_port))
        .await
        .with_context(|| format!("connecting to {}:{}", dest_ip, dest_port))?;

    // Bidirectional relay with large buffers (256KB vs copy_bidirectional's 8KB)
    const RELAY_BUF_SIZE: usize = 256 * 1024;

    let (mut vsock_rd, mut vsock_wr) = vsock_stream.split();
    let (mut upstream_rd, mut upstream_wr) = upstream.split();

    let vsock_to_upstream = async {
        let mut buf = vec![0u8; RELAY_BUF_SIZE];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut vsock_rd, &mut buf).await?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut upstream_wr, &buf[..n]).await?;
        }
        tokio::io::AsyncWriteExt::shutdown(&mut upstream_wr).await?;
        Ok::<_, std::io::Error>(())
    };

    let upstream_to_vsock = async {
        let mut buf = vec![0u8; RELAY_BUF_SIZE];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut upstream_rd, &mut buf).await?;
            if n == 0 {
                break;
            }
            tokio::io::AsyncWriteExt::write_all(&mut vsock_wr, &buf[..n]).await?;
        }
        tokio::io::AsyncWriteExt::shutdown(&mut vsock_wr).await?;
        Ok::<_, std::io::Error>(())
    };

    let _ = tokio::join!(vsock_to_upstream, upstream_to_vsock);

    Ok(())
}
