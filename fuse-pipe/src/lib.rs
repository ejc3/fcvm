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

// Re-export protocol types at crate root for convenience
pub use protocol::{
    file_type, read_message, read_message_async, write_message, write_message_async, DirEntry,
    FileAttr, VolumeRequest, VolumeResponse, WireRequest, WireResponse, MAX_MESSAGE_SIZE,
};

/// Prelude for common imports.
pub mod prelude {
    pub use crate::protocol::{
        DirEntry, FileAttr, VolumeRequest, VolumeResponse, WireRequest, WireResponse,
    };
}
