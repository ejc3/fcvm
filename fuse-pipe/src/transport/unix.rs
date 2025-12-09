//! Unix socket transport implementation.

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// A Unix socket transport.
///
/// This wraps a `UnixStream` to provide the Transport trait implementation.
/// Supports both client and server-side connections.
#[derive(Debug)]
pub struct UnixTransport {
    stream: UnixStream,
}

impl UnixTransport {
    /// Connect to a Unix socket at the given path.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self { stream })
    }

    /// Create a transport from an existing UnixStream.
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
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

    /// Get a reference to the underlying stream.
    pub fn get_ref(&self) -> &UnixStream {
        &self.stream
    }

    /// Get a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    /// Clone the transport.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }
}

impl Read for UnixTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for UnixTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl AsRawFd for UnixTransport {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

impl Clone for UnixTransport {
    /// Clone the transport.
    ///
    /// # Panics
    /// Panics if the underlying `UnixStream` cannot be cloned (e.g., due to
    /// resource exhaustion). Use [`try_clone()`](Self::try_clone) for a
    /// fallible version that returns `Result` instead of panicking.
    fn clone(&self) -> Self {
        Self {
            stream: self
                .stream
                .try_clone()
                .expect("UnixTransport::clone failed - use try_clone() for fallible clone"),
        }
    }
}

/// A Unix socket listener for accepting connections.
pub struct UnixListener {
    listener: std::os::unix::net::UnixListener,
}

impl UnixListener {
    /// Bind to a Unix socket path.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        // Remove existing socket file if present
        let _ = std::fs::remove_file(path.as_ref());
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        Ok(Self { listener })
    }

    /// Accept a new connection.
    pub fn accept(&self) -> io::Result<UnixTransport> {
        let (stream, _) = self.listener.accept()?;
        Ok(UnixTransport::from_stream(stream))
    }

    /// Set non-blocking mode for the listener.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.listener.set_nonblocking(nonblocking)
    }

    /// Get the local address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<std::os::unix::net::SocketAddr> {
        self.listener.local_addr()
    }
}

impl AsRawFd for UnixListener {
    fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_unix_socket_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        // Start server thread
        let socket_path_clone = socket_path.clone();
        let server_thread = thread::spawn(move || {
            let listener = UnixListener::bind(&socket_path_clone).unwrap();
            let mut transport = listener.accept().unwrap();

            let mut buf = [0u8; 5];
            transport.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hello");

            transport.write_all(b"world").unwrap();
        });

        // Give server time to start
        thread::sleep(Duration::from_millis(100));

        // Connect client
        let mut client = UnixTransport::connect(&socket_path).unwrap();
        client.write_all(b"hello").unwrap();

        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"world");

        server_thread.join().unwrap();
    }
}
