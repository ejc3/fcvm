//! Server components for FUSE-over-pipe.
//!
//! This module provides:
//!
//! - `FilesystemHandler`: Trait for implementing filesystem operations
//! - `PassthroughFs`: Passthrough filesystem mapping to a local directory
//! - `AsyncServer`: High-performance async pipelined server
//! - `ServerConfig`: Configuration for tuning performance
//!
//! # Example
//!
//! ```rust,ignore
//! use fuse_pipe::server::{AsyncServer, PassthroughFs, ServerConfig};
//!
//! let fs = PassthroughFs::new("/path/to/serve");
//! let config = ServerConfig::default();
//! let server = AsyncServer::with_config(fs, config);
//! server.run_blocking("/tmp/fuse.sock")?;
//! ```

mod config;
mod handler;
mod passthrough;
mod pipelined;

pub use config::ServerConfig;
pub use handler::FilesystemHandler;
pub use passthrough::PassthroughFs;
pub use pipelined::AsyncServer;
