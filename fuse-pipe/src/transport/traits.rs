//! Transport trait definitions.
//!
//! Transports provide the underlying I/O for FUSE-over-pipe communication.
//! Both synchronous and asynchronous variants are supported.

use std::io::{self, Read, Write};

/// A synchronous transport for FUSE protocol messages.
///
/// Implementors should handle framing (length-prefixed messages) internally
/// or expose raw read/write methods for use with the wire module.
pub trait Transport: Send {
    /// Read raw bytes from the transport.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Read exactly `buf.len()` bytes from the transport.
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;

    /// Write raw bytes to the transport.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>;

    /// Write all bytes to the transport.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;

    /// Flush any buffered data.
    fn flush(&mut self) -> io::Result<()>;

    /// Clone this transport (for multi-reader support).
    fn try_clone(&self) -> io::Result<Box<dyn Transport>>;

    /// Set read timeout.
    fn set_read_timeout(&mut self, dur: Option<std::time::Duration>) -> io::Result<()>;

    /// Set write timeout.
    fn set_write_timeout(&mut self, dur: Option<std::time::Duration>) -> io::Result<()>;

    /// Get the underlying raw file descriptor (for poll/select).
    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd>;
}

/// Blanket implementation for types that implement Read + Write + Clone.
impl<T> Transport for T
where
    T: Read + Write + Send + Clone + 'static,
    T: std::os::unix::io::AsRawFd,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Read::read(self, buf)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        Read::read_exact(self, buf)
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Write::write(self, buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Write::write_all(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(self)
    }

    fn try_clone(&self) -> io::Result<Box<dyn Transport>> {
        Ok(Box::new(self.clone()))
    }

    fn set_read_timeout(&mut self, _dur: Option<std::time::Duration>) -> io::Result<()> {
        // Default implementation - no-op for types without timeout support
        Ok(())
    }

    fn set_write_timeout(&mut self, _dur: Option<std::time::Duration>) -> io::Result<()> {
        // Default implementation - no-op for types without timeout support
        Ok(())
    }

    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        Some(std::os::unix::io::AsRawFd::as_raw_fd(self))
    }
}

/// An async transport for FUSE protocol messages.
///
/// Used with the async pipelined server for high-throughput operation.
#[async_trait::async_trait]
pub trait AsyncTransport: Send + Unpin {
    /// Read raw bytes from the transport.
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Read exactly `buf.len()` bytes from the transport.
    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;

    /// Write raw bytes to the transport.
    async fn write(&mut self, buf: &[u8]) -> io::Result<usize>;

    /// Write all bytes to the transport.
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;

    /// Flush any buffered data.
    async fn flush(&mut self) -> io::Result<()>;

    /// Split into read and write halves for concurrent I/O.
    fn into_split(self) -> (Box<dyn AsyncReadHalf>, Box<dyn AsyncWriteHalf>);
}

/// Read half of a split async transport.
#[async_trait::async_trait]
pub trait AsyncReadHalf: Send + Unpin {
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

/// Write half of a split async transport.
#[async_trait::async_trait]
pub trait AsyncWriteHalf: Send + Unpin {
    async fn write(&mut self, buf: &[u8]) -> io::Result<usize>;
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    async fn flush(&mut self) -> io::Result<()>;
}

/// Transport error types.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Connection refused")]
    ConnectionRefused,

    #[error("Connection reset")]
    ConnectionReset,

    #[error("Timeout")]
    Timeout,

    #[error("Transport not supported on this platform")]
    NotSupported,
}

impl From<TransportError> for io::Error {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::Io(e) => e,
            TransportError::ConnectionRefused => {
                io::Error::new(io::ErrorKind::ConnectionRefused, err)
            }
            TransportError::ConnectionReset => io::Error::new(io::ErrorKind::ConnectionReset, err),
            TransportError::Timeout => io::Error::new(io::ErrorKind::TimedOut, err),
            TransportError::NotSupported => io::Error::new(io::ErrorKind::Unsupported, err),
        }
    }
}
