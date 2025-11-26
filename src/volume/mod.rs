//! FUSE-over-vsock volume mounting.
//!
//! This module implements host directory mounting inside Firecracker VMs
//! using FUSE over vsock.
//!
//! # Architecture
//!
//! ```text
//! HOST (fcvm)                              GUEST (fc-agent)
//! ───────────────────────────────────────────────────────────
//!   VolumeServer                            FUSE Filesystem
//!   - Listen on vsock port                  - impl fuser::Filesystem
//!   - Handle VolumeRequests                 - Mount at /mnt/volumes/N
//!   - Do real fs ops on host                - Proxy ops to host via vsock
//! ```
//!
//! # Clone Support
//!
//! VolumeServer supports multiple concurrent clients:
//! - Original VM connects when it starts
//! - Clones connect to the same server
//! - Each client gets independent file handles
//! - Server maintains shared inode table, per-client handle tables

mod protocol;
mod server;

pub use protocol::{
    file_type, mode_to_file_type, DirEntry, FileAttr, VolumeRequest, VolumeResponse,
    MAX_MESSAGE_SIZE,
};
pub use server::{VolumeConfig, VolumeServer};
