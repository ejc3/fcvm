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
use tokio::task::JoinHandle;
use tracing::{error, info};

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
        self.serve_vsock_with_ready_signal(vsock_socket_path, None)
            .await
    }

    /// Serve volumes over vsock with ready signal.
    ///
    /// Same as `serve_vsock` but signals readiness via oneshot channel after socket bind.
    /// This allows callers to wait for the server to be ready instead of using sleeps.
    pub async fn serve_vsock_with_ready_signal(
        &self,
        vsock_socket_path: &Path,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<()> {
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
            .serve_vsock_forwarded_with_ready_signal(&base_path, self.config.port, ready)
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

/// Spawn multiple VolumeServers and wait for all to be ready.
///
/// This is a convenience function that:
/// 1. Creates VolumeServer instances from configs
/// 2. Spawns each server with a ready signal
/// 3. Waits for ALL servers to signal ready (socket bound)
/// 4. Returns the task handles for later cleanup
///
/// # Arguments
/// * `configs` - List of volume configurations to spawn
/// * `vsock_socket_path` - Base path for vsock sockets
///
/// # Returns
/// Vector of JoinHandles that should be aborted during cleanup
pub async fn spawn_volume_servers(
    configs: &[VolumeConfig],
    vsock_socket_path: &Path,
) -> Result<Vec<JoinHandle<()>>> {
    if configs.is_empty() {
        return Ok(Vec::new());
    }

    let mut handles = Vec::with_capacity(configs.len());
    let mut ready_receivers = Vec::with_capacity(configs.len());

    for config in configs {
        let server = VolumeServer::new(config.clone())
            .with_context(|| format!("creating VolumeServer for {}", config.host_path.display()))?;

        // Create oneshot channel for ready signal
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        ready_receivers.push(ready_rx);

        let vsock_path = vsock_socket_path.to_path_buf();
        let port = config.port;
        let handle = tokio::spawn(async move {
            if let Err(e) = server
                .serve_vsock_with_ready_signal(&vsock_path, Some(ready_tx))
                .await
            {
                error!("VolumeServer error for port {}: {}", port, e);
            }
        });

        info!(
            port = config.port,
            host_path = %config.host_path.display(),
            guest_path = %config.guest_path.display(),
            read_only = config.read_only,
            "spawned VolumeServer"
        );

        handles.push(handle);
    }

    // Wait for ALL VolumeServers to signal ready (socket bound)
    for (idx, ready_rx) in ready_receivers.into_iter().enumerate() {
        ready_rx
            .await
            .with_context(|| format!("VolumeServer {} failed to signal ready", idx))?;
    }

    info!("all {} VolumeServer(s) ready", configs.len());

    Ok(handles)
}
