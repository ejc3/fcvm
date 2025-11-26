//! VolumeClient - connects to host VolumeServer via vsock.

use crate::fuse::protocol::{VolumeRequest, VolumeResponse, MAX_MESSAGE_SIZE};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// Host CID for vsock (always 2)
pub const HOST_CID: u32 = 2;

/// Client for communicating with host VolumeServer.
/// Supports automatic reconnection after VM pause/resume breaks the connection.
pub struct VolumeClient {
    stream: Option<UnixStream>,
    port: u32,
}

impl VolumeClient {
    /// Connect to VolumeServer via vsock.
    ///
    /// For vsock, port is the vsock port number (e.g., 5000 for first volume).
    /// Guest connects to CID 2 (host), port N. Firecracker forwards the connection
    /// directly to the host's Unix socket at vsock.sock_{port}.
    pub fn connect_vsock(port: u32) -> Result<Self> {
        // Connect via vsock to CID 2 (host), port N
        // Firecracker forwards guest-initiated connections directly to vsock.sock_{port}
        // No handshake needed - the connection is forwarded as-is
        let stream = Self::connect_vsock_raw(HOST_CID, port)?;

        eprintln!("[fc-agent] vsock connected to port {}", port);

        Ok(Self { stream: Some(stream), port })
    }

    /// Connect via Unix socket (for testing).
    #[allow(dead_code)]
    pub fn connect_unix(path: &str) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("Failed to connect to Unix socket: {}", path))?;
        Ok(Self { stream: Some(stream), port: 0 })
    }

    /// Reconnect to the VolumeServer (after VM pause/resume breaks connection).
    fn reconnect(&mut self) -> Result<()> {
        eprintln!("[fc-agent] attempting vsock reconnect to port {}", self.port);

        // Close old stream if any
        self.stream = None;

        // Reconnect
        let stream = Self::connect_vsock_raw(HOST_CID, self.port)?;
        self.stream = Some(stream);

        eprintln!("[fc-agent] vsock reconnected to port {}", self.port);
        Ok(())
    }

    /// Check if an error indicates a broken connection that might be fixed by reconnecting.
    fn is_connection_error(err: &std::io::Error) -> bool {
        use std::io::ErrorKind;
        matches!(
            err.kind(),
            ErrorKind::NotConnected
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
        )
    }

    /// Low-level vsock connection using socket2.
    fn connect_vsock_raw(cid: u32, port: u32) -> Result<UnixStream> {
        use std::os::unix::io::FromRawFd;

        // Create vsock socket
        let fd = unsafe {
            libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM,
                0,
            )
        };

        if fd < 0 {
            bail!(
                "Failed to create vsock socket: {}",
                std::io::Error::last_os_error()
            );
        }

        // Connect to host
        let addr = libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: cid,
            svm_zero: [0u8; 4],
        };

        let result = unsafe {
            libc::connect(
                fd,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as u32,
            )
        };

        if result < 0 {
            unsafe { libc::close(fd) };
            bail!(
                "Failed to connect vsock to CID {} port {}: {}",
                cid,
                port,
                std::io::Error::last_os_error()
            );
        }

        // Convert to UnixStream for easier I/O
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        Ok(stream)
    }

    /// Send a request and receive a response.
    /// Automatically attempts to reconnect if the connection was broken (e.g., after VM pause/resume).
    pub fn request(&mut self, req: &VolumeRequest) -> Result<VolumeResponse> {
        // Try the request, reconnecting once if we get a connection error
        match self.request_inner(req) {
            Ok(resp) => Ok(resp),
            Err(e) => {
                // Check if this is a connection error that might be fixed by reconnecting
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    if Self::is_connection_error(io_err) {
                        // Try to reconnect and retry once
                        if self.reconnect().is_ok() {
                            return self.request_inner(req);
                        }
                    }
                }
                Err(e)
            }
        }
    }

    /// Internal request implementation.
    fn request_inner(&mut self, req: &VolumeRequest) -> Result<VolumeResponse> {
        let stream = self.stream.as_mut().context("no connection")?;

        // Encode request
        let req_buf = bincode::serialize(req)?;
        let req_len = (req_buf.len() as u32).to_be_bytes();

        // Send request
        stream.write_all(&req_len)?;
        stream.write_all(&req_buf)?;

        // Read response length
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > MAX_MESSAGE_SIZE {
            bail!("Response too large: {} bytes", len);
        }

        // Read response body
        let mut resp_buf = vec![0u8; len];
        stream.read_exact(&mut resp_buf)?;

        // Decode response
        let resp: VolumeResponse = bincode::deserialize(&resp_buf)?;
        Ok(resp)
    }
}
