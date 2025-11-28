//! Vsock transport implementation.
//!
//! Vsock (Virtual Socket) is a communication mechanism between
//! virtual machines and their host. This transport uses raw libc
//! calls to work without external dependencies.
//!
//! # CID Values
//!
//! - `VMADDR_CID_HYPERVISOR` (0): Reserved
//! - `VMADDR_CID_LOCAL` (1): Local communication (loopback)
//! - `VMADDR_CID_HOST` (2): The host machine
//! - 3+: Guest VMs (assigned by hypervisor)
//!
//! Note: Vsock is only available on Linux. This module provides stub
//! constants on other platforms for compilation compatibility.

/// Host CID for vsock connections (always 2).
pub const HOST_CID: u32 = 2;

/// Local CID for loopback vsock connections.
pub const LOCAL_CID: u32 = 1;

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{self, Read, Write};
    use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// A vsock transport.
    ///
    /// Uses raw libc vsock sockets, wrapped in a UnixStream for
    /// convenient Read/Write trait implementations.
    #[derive(Debug)]
    pub struct VsockTransport {
        /// The underlying socket wrapped as UnixStream for trait impls.
        stream: UnixStream,
        /// The CID we're connected to.
        cid: u32,
        /// The port we're connected to.
        port: u32,
    }

    impl VsockTransport {
        /// Connect to a vsock endpoint.
        ///
        /// # Arguments
        ///
        /// * `cid` - The context ID (use `HOST_CID` to connect to host from guest)
        /// * `port` - The port number
        ///
        /// # Example
        ///
        /// ```rust,ignore
        /// // Connect from guest to host on port 5000
        /// let transport = VsockTransport::connect(HOST_CID, 5000)?;
        /// ```
        pub fn connect(cid: u32, port: u32) -> io::Result<Self> {
            // Create vsock socket
            let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            // Build sockaddr_vm structure
            let addr = libc::sockaddr_vm {
                svm_family: libc::AF_VSOCK as u16,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: cid,
                svm_zero: [0u8; 4],
            };

            // Connect
            let result = unsafe {
                libc::connect(
                    fd,
                    &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_vm>() as u32,
                )
            };

            if result < 0 {
                let err = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(err);
            }

            // Wrap fd in UnixStream for Read/Write impls
            let stream = unsafe { UnixStream::from_raw_fd(fd) };

            Ok(Self { stream, cid, port })
        }

        /// Create a vsock transport from an existing file descriptor.
        ///
        /// # Safety
        ///
        /// The file descriptor must be a valid vsock socket.
        pub unsafe fn from_raw_fd(fd: RawFd, cid: u32, port: u32) -> Self {
            Self {
                stream: UnixStream::from_raw_fd(fd),
                cid,
                port,
            }
        }

        /// Get the connected CID.
        pub fn cid(&self) -> u32 {
            self.cid
        }

        /// Get the connected port.
        pub fn port(&self) -> u32 {
            self.port
        }

        /// Set the read timeout.
        pub fn set_read_timeout(&mut self, dur: Option<Duration>) -> io::Result<()> {
            self.stream.set_read_timeout(dur)
        }

        /// Set the write timeout.
        pub fn set_write_timeout(&mut self, dur: Option<Duration>) -> io::Result<()> {
            self.stream.set_write_timeout(dur)
        }

        /// Set non-blocking mode.
        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.stream.set_nonblocking(nonblocking)
        }

        /// Clone the transport.
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                stream: self.stream.try_clone()?,
                cid: self.cid,
                port: self.port,
            })
        }
    }

    impl Read for VsockTransport {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for VsockTransport {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.stream.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    impl AsRawFd for VsockTransport {
        fn as_raw_fd(&self) -> RawFd {
            self.stream.as_raw_fd()
        }
    }

    impl Clone for VsockTransport {
        fn clone(&self) -> Self {
            Self {
                stream: self.stream.try_clone().expect("failed to clone vsock"),
                cid: self.cid,
                port: self.port,
            }
        }
    }

    /// A vsock listener for accepting connections.
    pub struct VsockListener {
        fd: RawFd,
        port: u32,
    }

    impl VsockListener {
        /// Bind to a vsock port.
        ///
        /// The CID is determined by the hypervisor. Use `VMADDR_CID_ANY` (-1u32)
        /// to accept connections from any CID.
        ///
        /// # Arguments
        ///
        /// * `port` - The port number to listen on
        pub fn bind(port: u32) -> io::Result<Self> {
            // Create vsock socket
            let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            // Bind to any CID
            let addr = libc::sockaddr_vm {
                svm_family: libc::AF_VSOCK as u16,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: libc::VMADDR_CID_ANY,
                svm_zero: [0u8; 4],
            };

            let result = unsafe {
                libc::bind(
                    fd,
                    &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_vm>() as u32,
                )
            };

            if result < 0 {
                let err = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(err);
            }

            // Listen with backlog of 128
            let result = unsafe { libc::listen(fd, 128) };
            if result < 0 {
                let err = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(err);
            }

            Ok(Self { fd, port })
        }

        /// Accept a new connection.
        pub fn accept(&self) -> io::Result<(VsockTransport, u32)> {
            let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
            let mut addr_len = std::mem::size_of::<libc::sockaddr_vm>() as u32;

            let client_fd = unsafe {
                libc::accept(
                    self.fd,
                    &mut addr as *mut libc::sockaddr_vm as *mut libc::sockaddr,
                    &mut addr_len,
                )
            };

            if client_fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let transport =
                unsafe { VsockTransport::from_raw_fd(client_fd, addr.svm_cid, addr.svm_port) };

            Ok((transport, addr.svm_cid))
        }

        /// Set non-blocking mode for the listener.
        pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            let flags = unsafe { libc::fcntl(self.fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }

            let new_flags = if nonblocking {
                flags | libc::O_NONBLOCK
            } else {
                flags & !libc::O_NONBLOCK
            };

            if unsafe { libc::fcntl(self.fd, libc::F_SETFL, new_flags) } < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        }

        /// Get the port this listener is bound to.
        pub fn port(&self) -> u32 {
            self.port
        }
    }

    impl AsRawFd for VsockListener {
        fn as_raw_fd(&self) -> RawFd {
            self.fd
        }
    }

    impl Drop for VsockListener {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{VsockListener, VsockTransport};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vsock_constants() {
        assert_eq!(HOST_CID, 2);
        assert_eq!(LOCAL_CID, 1);
    }

    // Note: Actual vsock tests require a VM environment
    // These tests verify the API compiles but can't run on the host
}
