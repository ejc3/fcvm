//! FUSE client with socket multiplexing for multi-reader support.
//!
//! This module provides a FUSE client that can share a single socket
//! connection across multiple reader threads, enabling high-throughput
//! parallel filesystem access.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────┐ ┌─────────┐ ┌─────────┐
//! │Reader 0 │ │Reader 1 │ │Reader 2 │   FUSE reader threads
//! └────┬────┘ └────┬────┘ └────┬────┘
//!      │          │          │
//!      └──────────┼──────────┘
//!                 │
//!          ┌──────┴──────┐
//!          │ Multiplexer │  Single shared socket
//!          └──────┬──────┘
//!                 │
//!          ┌──────┴──────┐
//!          │  Server     │
//!          └─────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use fuse_pipe::{mount, MountConfig};
//!
//! // Simple blocking mount
//! mount("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new())?;
//!
//! // Mount with 256 readers for parallelism
//! mount("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new().readers(256))?;
//! ```
//!
//! For non-blocking mount with automatic cleanup, use [`mount_spawn`]:
//!
//! ```rust,ignore
//! use fuse_pipe::{mount_spawn, MountConfig};
//!
//! let handle = mount_spawn("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new().readers(256))?;
//! // ... do work ...
//! // Unmount happens automatically when handle is dropped
//! ```
//!
//! # Feature
//!
//! This module requires the `fuse-client` feature (enabled by default).

mod fuse;
mod mount;
mod multiplexer;

pub use fuse::FuseClient;
pub use mount::{mount, mount_spawn, MountConfig, MountHandle};
#[cfg(target_os = "linux")]
pub use mount::{mount_vsock, mount_vsock_with_options, mount_vsock_with_readers};
pub use multiplexer::Multiplexer;
