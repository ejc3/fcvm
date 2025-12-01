//! Transport layer for FUSE-over-pipe/socket communication.
//!
//! This module provides transport abstractions for different connection types:
//!
//! - **Unix sockets**: For local host communication (e.g., stress testing)
//! - **Vsock**: For guest-to-host communication in VMs
//!
//! # Example
//!
//! ```rust,ignore
//! use fuse_pipe::transport::{UnixTransport, VsockTransport, HOST_CID};
//!
//! // Unix socket (for testing)
//! let transport = UnixTransport::connect("/tmp/fuse.sock")?;
//!
//! // Vsock (for VM guest)
//! let transport = VsockTransport::connect(HOST_CID, 5000)?;
//! ```

mod traits;
mod unix;
mod vsock;

pub use traits::{AsyncReadHalf, AsyncTransport, AsyncWriteHalf, Transport, TransportError};
pub use unix::{UnixListener, UnixTransport};
#[cfg(target_os = "linux")]
pub use vsock::{VsockListener, VsockTransport};
pub use vsock::{HOST_CID, LOCAL_CID};
