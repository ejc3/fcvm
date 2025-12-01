//! High-performance FUSE-over-pipe/vsock implementation.
//!
//! `fuse-pipe` provides a complete stack for remote FUSE filesystem operations:
//!
//! - **Protocol**: Wire format for requests/responses with multiplexing support
//! - **Transport**: Unix socket and vsock transports
//! - **Server**: Async pipelined server with response batching
//! - **Client**: Multi-reader FUSE client with socket multiplexing
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use fuse_pipe::{AsyncServer, PassthroughFs, ServerConfig};
//!
//! // Create a passthrough filesystem
//! let fs = PassthroughFs::new("/path/to/serve");
//!
//! // Start the async server
//! let server = AsyncServer::with_config(fs, ServerConfig::default());
//! server.serve_unix("/tmp/fuse.sock").await?;
//! ```
//!
//! # Features
//!
//! - `fuse-client` (default): Enable FUSE client support via `fuser`
//! - `concurrent`: Use `DashMap` for concurrent inode/handle tables
//! - `metrics`: Enable metrics collection via the `metrics` crate

pub mod protocol;
pub mod server;
pub mod transport;

#[cfg(feature = "fuse-client")]
pub mod client;

// Re-export protocol types at crate root for convenience
pub use protocol::{
    file_type, read_message, read_message_async, write_message, write_message_async, DirEntry,
    FileAttr, VolumeRequest, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE,
};

// Re-export transport types
pub use transport::{Transport, TransportError, UnixListener, UnixTransport, HOST_CID, LOCAL_CID};
#[cfg(target_os = "linux")]
pub use transport::{VsockListener, VsockTransport};

// Re-export server types
pub use server::{AsyncServer, FilesystemHandler, PassthroughFs, ServerConfig};

// Re-export client types
#[cfg(feature = "fuse-client")]
pub use client::{mount, mount_with_options, mount_with_readers, FuseClient, Multiplexer};
#[cfg(all(feature = "fuse-client", target_os = "linux"))]
pub use client::{mount_vsock, mount_vsock_with_options, mount_vsock_with_readers};

/// Prelude for common imports.
pub mod prelude {
    pub use crate::protocol::{
        DirEntry, FileAttr, VolumeRequest, VolumeResponse, WireRequest, WireResponse,
    };
    #[cfg(target_os = "linux")]
    pub use crate::transport::VsockTransport;
    pub use crate::transport::{UnixTransport, HOST_CID};
}
