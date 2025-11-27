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
//! use fuse_pipe::client::mount_with_readers;
//! use std::path::PathBuf;
//!
//! // Mount with 4 reader threads for parallel access
//! mount_with_readers("/tmp/fuse.sock", &PathBuf::from("/mnt/fuse"), 4)?;
//! ```
//!
//! # Feature
//!
//! This module requires the `fuse-client` feature (enabled by default).

mod fuse;
mod mount;
mod multiplexer;

pub use fuse::FuseClient;
pub use mount::{mount, mount_with_readers};
pub use multiplexer::Multiplexer;
