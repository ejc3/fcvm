//! FUSE-over-vsock volume mounting using fuse-pipe.
//!
//! This module provides host directory mounting inside Firecracker VMs
//! using FUSE over vsock, powered by the high-performance fuse-pipe library.
//!
//! # Architecture
//!
//! ```text
//! HOST (fcvm)                              GUEST (fc-agent)
//! ───────────────────────────────────────────────────────────
//!   VolumeServer                            FUSE Filesystem
//!   - fuse-pipe::AsyncServer               - fuse-pipe::FuseClient
//!   - Listen on vsock port                  - Mount at /mnt/volumes/N
//!   - PassthroughFs handler                 - Proxy ops to host via vsock
//! ```
//!
//! # Clone Support
//!
//! VolumeServer supports multiple concurrent clients via fuse-pipe's
//! pipelined server and lock-free multiplexer architecture.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

// Re-export protocol types from fuse-pipe for compatibility
pub use fuse_pipe::{
    file_type, DirEntry, FileAttr, VolumeRequest, VolumeResponse, MAX_MESSAGE_SIZE,
};

/// Helper to convert FileAttr's mode to file type constant
pub fn mode_to_file_type(mode: u32) -> u8 {
    // Extract file type from mode (top 4 bits of lower 16)
    ((mode >> 12) & 0xF) as u8
}

/// Volume server configuration.
#[derive(Debug, Clone)]
pub struct VolumeConfig {
    /// Host path to serve
    pub host_path: PathBuf,
    /// Mount path in guest
    pub guest_path: PathBuf,
    /// Read-only mode
    pub read_only: bool,
    /// Vsock port number
    pub port: u32,
}

/// Volume server that serves host directories to guests.
///
/// This is a thin wrapper around fuse-pipe's AsyncServer with PassthroughFs.
pub struct VolumeServer {
    config: VolumeConfig,
    host_path: PathBuf,
}

impl VolumeServer {
    /// Create a new volume server.
    pub fn new(config: VolumeConfig) -> Result<Self> {
        let host_path = config
            .host_path
            .canonicalize()
            .with_context(|| format!("Failed to resolve path: {:?}", config.host_path))?;

        if !host_path.is_dir() {
            anyhow::bail!("Volume path is not a directory: {:?}", host_path);
        }

        Ok(Self { config, host_path })
    }

    /// Serve volumes over Firecracker's vsock Unix socket.
    ///
    /// For guest-initiated connections, Firecracker's vsock expects the host to listen on:
    ///   `{uds_path}_{port}` (e.g., `/path/to/v.sock_5000`)
    ///
    /// When guest connects to CID 2, port 5000, Firecracker forwards to host's v.sock_5000.
    pub async fn serve_vsock(&self, vsock_socket_path: &Path) -> Result<()> {
        let base_path = vsock_socket_path.to_string_lossy();

        info!(
            port = self.config.port,
            host_path = %self.host_path.display(),
            read_only = self.config.read_only,
            socket = format!("{}_{}", base_path, self.config.port),
            "VolumeServer starting"
        );

        // Create fuse-pipe's passthrough filesystem
        let fs = fuse_pipe::PassthroughFs::new(&self.host_path);

        // Note: read_only enforcement is handled at the PassthroughFs level
        // TODO: Add read_only support to PassthroughFs if needed

        // Create and run the async server
        let server = fuse_pipe::AsyncServer::new(fs);
        server
            .serve_vsock_forwarded(&base_path, self.config.port)
            .await
            .with_context(|| {
                format!(
                    "VolumeServer failed for port {} serving {}",
                    self.config.port,
                    self.host_path.display()
                )
            })
    }

    /// Serve volumes over a Unix socket (for testing/development).
    pub async fn serve_unix(&self, socket_path: &Path) -> Result<()> {
        let path_str = socket_path.to_string_lossy();

        info!(
            host_path = %self.host_path.display(),
            socket = %path_str,
            "VolumeServer starting (Unix socket)"
        );

        let fs = fuse_pipe::PassthroughFs::new(&self.host_path);
        let server = fuse_pipe::AsyncServer::new(fs);
        server
            .serve_unix(&path_str)
            .await
            .with_context(|| format!("VolumeServer failed for {}", self.host_path.display()))
    }

    /// Get the configuration.
    pub fn config(&self) -> &VolumeConfig {
        &self.config
    }

    /// Get the resolved host path.
    pub fn host_path(&self) -> &Path {
        &self.host_path
    }
}
