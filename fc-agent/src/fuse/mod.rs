//! FUSE-over-vsock volume mounting for guest.
//!
//! This module implements a FUSE filesystem that proxies operations
//! to the host VolumeServer via vsock.

pub mod client;
pub mod fusefs;
pub mod protocol;

pub use fusefs::VolumeFs;
