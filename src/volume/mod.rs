//! FUSE-over-vsock volume mounting using fuse-pipe.
//!
//! This module provides host directory mounting inside VMs via vsock,
//! which works with both hypervisor backends (Firecracker and Cloud Hypervisor),
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
use std::sync::Arc;
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
    /// Use portable inode numbering (RemapFs wrapper)
    pub portable: bool,
}

/// Volume server that serves host directories to guests.
///
/// This is a thin wrapper around fuse-pipe's AsyncServer with PassthroughFs.
pub struct VolumeServer {
    config: VolumeConfig,
    host_path: PathBuf,
}

/// Resolve and validate a volume host path (canonicalize + check is_dir).
fn resolve_volume_path(config: &VolumeConfig) -> Result<PathBuf> {
    let host_path = config
        .host_path
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {:?}", config.host_path))?;

    if !host_path.is_dir() {
        anyhow::bail!("Volume path is not a directory: {:?}", host_path);
    }

    Ok(host_path)
}

impl VolumeServer {
    /// Create a new volume server.
    pub fn new(config: VolumeConfig) -> Result<Self> {
        let host_path = resolve_volume_path(&config)?;
        Ok(Self { config, host_path })
    }

    /// Serve volumes over the VMM's vsock Unix socket.
    ///
    /// For guest-initiated connections, the VMM's vsock expects the host to listen on:
    ///   `{uds_path}_{port}` (e.g., `/path/to/v.sock_5000`)
    ///
    /// When the guest connects to the host (CID 2) on port 5000, the VMM forwards to the
    /// host's `v.sock_5000`. This `{uds_path}_{port}` scheme is identical for both Firecracker
    /// and Cloud Hypervisor.
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

        let serve_err = || {
            format!(
                "VolumeServer failed for port {} serving {}",
                self.config.port,
                self.host_path.display()
            )
        };

        if self.config.portable {
            // Wrap in RemapFs for deterministic inode numbering
            info!("using portable inode remapping (RemapFs)");
            let remap = fuse_pipe::RemapFs::new(fs);
            let server = fuse_pipe::AsyncServer::new(remap);
            server
                .serve_vsock_forwarded_with_ready_signal(&base_path, self.config.port, ready)
                .await
                .with_context(serve_err)
        } else {
            let server = fuse_pipe::AsyncServer::new(fs);
            server
                .serve_vsock_forwarded_with_ready_signal(&base_path, self.config.port, ready)
                .await
                .with_context(serve_err)
        }
    }

    /// Serve with a pre-created `Arc<RemapFs>`, returning the reference for serialization.
    ///
    /// The caller retains the Arc to call `serialize_table()` at snapshot time
    /// while the server is running.
    async fn serve_vsock_with_remap_arc(
        host_path: &Path,
        config: &VolumeConfig,
        vsock_socket_path: &Path,
        remap: Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<()> {
        let base_path = vsock_socket_path.to_string_lossy();

        info!(
            port = config.port,
            host_path = %host_path.display(),
            read_only = config.read_only,
            socket = format!("{}_{}", base_path, config.port),
            "VolumeServer starting (portable, Arc)"
        );

        let server = fuse_pipe::AsyncServer::from_arc(remap);
        server
            .serve_vsock_forwarded_with_ready_signal(&base_path, config.port, ready)
            .await
            .with_context(|| {
                format!(
                    "VolumeServer failed for port {} serving {}",
                    config.port,
                    host_path.display()
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

/// Name of the snapshot file that holds the inode table of the portable volume on vsock `port`.
pub fn inode_table_file_name(port: u32) -> String {
    format!("volume-{}-inode-table.json", port)
}

/// Socket, beside the volume servers' `{vsock}_{port}` sockets, on which the VM's fcvm process serves the
/// inode tables of its portable volumes.
///
/// `fcvm snapshot create` runs in a process of its own, so it cannot read the RemapFs tables of the VM it
/// snapshots; it asks this socket for them. A clone restored without them starts with empty tables, and
/// every inode the guest held at the snapshot, such as an open file, a memory-mapped binary or a working
/// directory, then answers EIO for as long as the guest keeps it.
pub fn inode_table_socket_path(vsock_socket_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}_inode_tables", vsock_socket_path.display()))
}

/// The inode tables of `remaps`, as the snapshot files that carry them.
///
/// A table only gains entries while the VM runs (Forget does not remove them), so tables read at any
/// moment after the guest was paused cover every inode the snapshot's guest can reference.
pub fn inode_table_files(
    remaps: &[(u32, Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>)],
) -> Vec<(String, Vec<u8>)> {
    remaps
        .iter()
        .map(|(port, remap)| {
            let json = remap.serialize_table();
            info!(
                port,
                bytes = json.len(),
                "serialized inode table for snapshot"
            );
            (inode_table_file_name(*port), json.into_bytes())
        })
        .collect()
}

/// Serve `remaps`' inode tables on `socket_path`: each connection receives them as a JSON list of
/// `[file name, table]` pairs, then the server closes it.
fn spawn_inode_table_server(
    socket_path: PathBuf,
    remaps: Vec<(u32, Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>)>,
) -> Result<JoinHandle<()>> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("binding inode table socket {}", socket_path.display()))?;
    Ok(tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        loop {
            let mut stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    error!(socket = %socket_path.display(), error = %e, "inode table socket accept failed");
                    continue;
                }
            };
            let files: Vec<(String, String)> = inode_table_files(&remaps)
                .into_iter()
                .map(|(name, json)| (name, String::from_utf8(json).unwrap_or_default()))
                .collect();
            let body = serde_json::to_vec(&files).unwrap_or_default();
            if let Err(e) = stream.write_all(&body).await {
                error!(socket = %socket_path.display(), error = %e, "writing inode tables failed");
            }
            let _ = stream.shutdown().await;
        }
    }))
}

/// Fetch the inode tables of the VM whose volume servers listen beside `vsock_socket_path`.
///
/// Blocking, with a 30 s limit: it is called from the snapshot path's extra-files hook.
pub fn fetch_inode_tables(vsock_socket_path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    use std::io::Read;
    let socket_path = inode_table_socket_path(vsock_socket_path);
    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "connecting to {} for the VM's portable-volume inode tables (a VM started by an fcvm \
             without this socket cannot be snapshotted with portable volumes; restart it)",
            socket_path.display()
        )
    })?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let mut body = Vec::new();
    stream
        .read_to_end(&mut body)
        .with_context(|| format!("reading inode tables from {}", socket_path.display()))?;
    let files: Vec<(String, String)> = serde_json::from_slice(&body)
        .with_context(|| format!("parsing inode tables from {}", socket_path.display()))?;
    Ok(files
        .into_iter()
        .map(|(name, json)| (name, json.into_bytes()))
        .collect())
}

/// Result of spawning volume servers. Holds task handles and optional RemapFs
/// references for portable volumes (needed for inode table serialization at snapshot time).
pub struct SpawnedVolumes {
    pub handles: Vec<JoinHandle<()>>,
    /// One entry per volume config. `Some(arc)` for portable volumes, `None` for plain.
    pub remap_refs: Vec<Option<Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>>>,
}

/// Spawn multiple VolumeServers and wait for all to be ready.
///
/// For portable volumes, creates `Arc<RemapFs>` and retains references in `SpawnedVolumes`
/// so callers can serialize the inode table at snapshot time.
pub async fn spawn_volume_servers(
    configs: &[VolumeConfig],
    vsock_socket_path: &Path,
) -> Result<SpawnedVolumes> {
    spawn_volume_servers_with_tables(configs, vsock_socket_path, &[]).await
}

/// Spawn VolumeServers with optional inode table restoration for portable volumes.
///
/// When `inode_tables[i]` is `Some(json)`, the portable volume at index `i` uses
/// `RemapFs::restore_from_table()` instead of `RemapFs::new()`, preserving inode
/// numbering from the snapshot baseline.
pub async fn spawn_volume_servers_with_tables(
    configs: &[VolumeConfig],
    vsock_socket_path: &Path,
    inode_tables: &[Option<String>],
) -> Result<SpawnedVolumes> {
    if configs.is_empty() {
        return Ok(SpawnedVolumes {
            handles: Vec::new(),
            remap_refs: Vec::new(),
        });
    }

    // A later volume can fail validation/bind after earlier tasks were already
    // spawned. JoinHandle::drop detaches, so keep them behind an abort-on-drop
    // guard until the whole set has reported ready. This also makes cancelling
    // the setup future safe: no half-built VolumeServer survives its caller.
    struct AbortOnDrop(Vec<JoinHandle<()>>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            for handle in &self.0 {
                handle.abort();
            }
        }
    }
    let mut handles = AbortOnDrop(Vec::with_capacity(configs.len()));
    let mut remap_refs = Vec::with_capacity(configs.len());
    let mut ready_receivers = Vec::with_capacity(configs.len());
    // Validate every source before the first task is spawned. A bad later
    // volume therefore cannot force an async task teardown through Drop.
    let host_paths = configs
        .iter()
        .map(resolve_volume_path)
        .collect::<Result<Vec<_>>>()?;

    for (idx, (config, host_path)) in configs.iter().zip(host_paths).enumerate() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        ready_receivers.push(ready_rx);

        let port = config.port;

        if config.portable {
            let fs = fuse_pipe::PassthroughFs::new(&host_path);
            let table = inode_tables.get(idx).and_then(|t| t.as_ref());

            let remap = if let Some(json) = table {
                info!(port, "restoring RemapFs from serialized inode table");
                Arc::new(fuse_pipe::RemapFs::restore_from_table(fs, json))
            } else {
                Arc::new(fuse_pipe::RemapFs::new(fs))
            };

            remap_refs.push(Some(Arc::clone(&remap)));

            let cfg = config.clone();
            let hp = host_path.clone();
            let vsock_path = vsock_socket_path.to_path_buf();
            let handle = tokio::spawn(async move {
                if let Err(e) = VolumeServer::serve_vsock_with_remap_arc(
                    &hp,
                    &cfg,
                    &vsock_path,
                    remap,
                    Some(ready_tx),
                )
                .await
                {
                    error!("VolumeServer error for port {}: {}", port, e);
                }
            });
            handles.0.push(handle);
        } else {
            remap_refs.push(None);

            let server = VolumeServer {
                config: config.clone(),
                host_path,
            };

            let vsock_path = vsock_socket_path.to_path_buf();
            let handle = tokio::spawn(async move {
                if let Err(e) = server
                    .serve_vsock_with_ready_signal(&vsock_path, Some(ready_tx))
                    .await
                {
                    error!("VolumeServer error for port {}: {}", port, e);
                }
            });
            handles.0.push(handle);
        }

        info!(
            port = config.port,
            host_path = %config.host_path.display(),
            guest_path = %config.guest_path.display(),
            read_only = config.read_only,
            portable = config.portable,
            "spawned VolumeServer"
        );
    }

    // Wait for ALL VolumeServers to signal ready (socket bound)
    for (idx, ready_rx) in ready_receivers.into_iter().enumerate() {
        if let Err(error) = ready_rx.await {
            for handle in &handles.0 {
                handle.abort();
            }
            for handle in handles.0.drain(..) {
                let _ = handle.await;
            }
            return Err(error)
                .with_context(|| format!("VolumeServer {} failed to signal ready", idx));
        }
    }

    info!("all {} VolumeServer(s) ready", configs.len());

    let portable: Vec<_> = configs
        .iter()
        .zip(&remap_refs)
        .filter_map(|(config, remap)| remap.as_ref().map(|r| (config.port, Arc::clone(r))))
        .collect();
    if !portable.is_empty() {
        handles.0.push(spawn_inode_table_server(
            inode_table_socket_path(vsock_socket_path),
            portable,
        )?);
    }

    Ok(SpawnedVolumes {
        handles: std::mem::take(&mut handles.0),
        remap_refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fcvm snapshot create` gets exactly the tables the VM's RemapFs holds, under the file names the
    /// restore path reads.
    #[tokio::test]
    async fn inode_tables_round_trip_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("held.txt"), b"x").unwrap();
        let remap = Arc::new(fuse_pipe::RemapFs::new(fuse_pipe::PassthroughFs::new(
            dir.path(),
        )));
        use fuse_pipe::FilesystemHandler;
        // The volume server's entry point; RemapFs does not implement the per-operation methods.
        let lookup = remap.handle_request_with_groups(
            &fuse_pipe::VolumeRequest::Lookup {
                parent: 1,
                name: b"held.txt".to_vec(),
                uid: nix::unistd::Uid::effective().as_raw(),
                gid: nix::unistd::Gid::effective().as_raw(),
                pid: 0,
            },
            &[],
        );
        let expected = remap.serialize_table();
        assert!(
            expected.contains("held.txt"),
            "lookup did not register the file: {expected}, lookup answered {lookup:?}"
        );

        let vsock = dir.path().join("vsock.sock");
        let server = spawn_inode_table_server(
            inode_table_socket_path(&vsock),
            vec![(5001, Arc::clone(&remap))],
        )
        .unwrap();
        let files = tokio::task::spawn_blocking(move || fetch_inode_tables(&vsock))
            .await
            .unwrap()
            .unwrap();
        server.abort();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, inode_table_file_name(5001));
        assert_eq!(String::from_utf8(files[0].1.clone()).unwrap(), expected);
    }

    /// With no server, the fetch fails instead of returning no tables, so a snapshot of a VM with
    /// portable volumes cannot silently be written without them.
    #[test]
    fn fetching_without_a_server_fails() {
        let dir = tempfile::tempdir().unwrap();
        let error = fetch_inode_tables(&dir.path().join("vsock.sock")).unwrap_err();
        assert!(format!("{error:#}").contains("inode tables"), "{error:#}");
    }
}
