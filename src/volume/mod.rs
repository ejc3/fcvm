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
    /// Where the guest mounts another filesystem inside this volume, relative to its root
    /// (`mount_points_inside`). The server keeps answering each with the directory the guest
    /// mounted over (`PassthroughFs::with_mount_points`).
    pub mount_points: Vec<PathBuf>,
}

/// The guest paths in `mount_points` that lie inside the volume mounted at `guest_path`,
/// relative to it.
pub fn mount_points_inside<'a>(
    guest_path: &Path,
    mount_points: impl IntoIterator<Item = &'a Path>,
) -> Vec<PathBuf> {
    mount_points
        .into_iter()
        .filter(|inner| *inner != guest_path)
        .filter_map(|inner| inner.strip_prefix(guest_path).ok())
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect()
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

    /// Serve `handler` over the VMM's vsock Unix socket and signal `ready` once it is bound.
    ///
    /// For guest-initiated connections, the VMM's vsock expects the host to listen on
    /// `{uds_path}_{port}` (e.g. `/path/to/v.sock_5000`), for Firecracker and Cloud Hypervisor
    /// alike. The caller keeps its own `Arc` of the handler to read the inode table at snapshot
    /// time while the server runs.
    async fn serve_vsock_arc<H: fuse_pipe::FilesystemHandler + 'static>(
        host_path: &Path,
        config: &VolumeConfig,
        vsock_socket_path: &Path,
        handler: Arc<H>,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<()> {
        let base_path = vsock_socket_path.to_string_lossy();

        info!(
            port = config.port,
            host_path = %host_path.display(),
            read_only = config.read_only,
            socket = format!("{}_{}", base_path, config.port),
            "VolumeServer starting"
        );

        let server = fuse_pipe::AsyncServer::from_arc(handler);
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

        let fs = fuse_pipe::PassthroughFs::for_volume(&self.host_path, self.config.read_only)
            .with_mount_points(&self.config.mount_points);
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

/// Name of the snapshot file that holds the inode table of the volume on vsock `port`.
pub fn inode_table_file_name(port: u32) -> String {
    format!("volume-{}-inode-table.json", port)
}

/// Socket, beside the volume servers' `{vsock}_{port}` sockets, on which the VM's fcvm process serves the
/// inode tables of its volumes.
///
/// `fcvm snapshot create` runs in a process of its own, so it cannot read the tables of the VM it
/// snapshots; it asks this socket for them. A clone restored without them starts with empty tables, and
/// every inode the guest held at the snapshot, such as an open file, a memory-mapped binary or a working
/// directory, then answers EIO for as long as the guest keeps it.
pub fn inode_table_socket_path(vsock_socket_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}_inode_tables", vsock_socket_path.display()))
}

/// Where a running volume server's inode table comes from at snapshot time.
#[derive(Clone)]
pub enum TableSource {
    /// A portable or restored volume: RemapFs's own table, which only gains entries while the VM
    /// runs (Forget does not remove them).
    Remap(Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>),
    /// A plain volume: the inodes its backend holds, captured while the guest is paused, by the
    /// paths they have when the table is read (`PassthroughFs::inode_table`).
    Plain(Arc<fuse_pipe::PassthroughFs>),
}

impl TableSource {
    /// What the table needs from the running server, taken while the guest is paused: a plain
    /// volume's held inodes, pinned (`PassthroughFs::capture_inodes`). A RemapFs table only
    /// gains entries, so it is read whole later. Cheap either way, so the pause does not grow
    /// with the number of inodes the guest holds.
    pub fn capture(&self) -> CapturedTable {
        match self {
            TableSource::Remap(remap) => CapturedTable::Remap(Arc::clone(remap)),
            TableSource::Plain(plain) => {
                CapturedTable::Plain(Arc::clone(plain), plain.capture_inodes())
            }
        }
    }
}

/// A volume's inode table as [`TableSource::capture`] took it, read with
/// [`CapturedTable::serialize`] once the guest runs again.
pub enum CapturedTable {
    Remap(Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>),
    Plain(Arc<fuse_pipe::PassthroughFs>, fuse_pipe::CapturedInodes),
}

impl CapturedTable {
    /// The table, in the form `RemapFs::restore_from_table` reads.
    pub fn serialize(self) -> String {
        match self {
            CapturedTable::Remap(remap) => remap.serialize_table(),
            CapturedTable::Plain(plain, captured) => plain.inode_table(captured),
        }
    }
}

/// Capture the inode tables of `sources`, with the guest paused.
pub fn capture_inode_tables(sources: &[(u32, TableSource)]) -> Vec<(u32, CapturedTable)> {
    sources
        .iter()
        .map(|(port, source)| (*port, source.capture()))
        .collect()
}

/// The captured inode tables, as the snapshot files that carry them.
pub fn inode_table_files(captured: Vec<(u32, CapturedTable)>) -> Vec<(String, Vec<u8>)> {
    captured
        .into_iter()
        .map(|(port, table)| {
            let json = table.serialize();
            info!(
                port,
                bytes = json.len(),
                "serialized inode table for snapshot"
            );
            (inode_table_file_name(port), json.into_bytes())
        })
        .collect()
}

/// Serve the inode tables of `sources` on `socket_path`. A connection is the snapshot: the
/// server captures the tables when it accepts one ([`TableSource::capture`]), answers
/// [`TABLES_CAPTURED`], then serializes them and sends them as a JSON list of
/// `[file name, table]` pairs, and closes it.
fn spawn_inode_table_server(
    socket_path: PathBuf,
    sources: Vec<(u32, TableSource)>,
) -> Result<JoinHandle<()>> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("binding inode table socket {}", socket_path.display()))?;
    Ok(tokio::spawn(serve_inode_tables(
        listener,
        socket_path,
        move || capture_inode_tables(&sources),
        |captured| {
            inode_table_files(captured)
                .into_iter()
                .map(|(name, json)| (name, String::from_utf8(json).unwrap_or_default()))
                .collect()
        },
    )))
}

/// The byte the table server sends once it has captured the tables.
const TABLES_CAPTURED: u8 = b'c';

/// Accept loop of [`spawn_inode_table_server`]: `capture` runs when a connection is accepted
/// and `serialize` after the server has told the client so.
async fn serve_inode_tables<C, F, S>(
    listener: tokio::net::UnixListener,
    socket_path: PathBuf,
    capture: F,
    serialize: S,
) where
    C: Send + 'static,
    F: Fn() -> C + Send + Sync + 'static,
    S: Fn(C) -> Vec<(String, String)> + Send + Sync + 'static,
{
    use tokio::io::AsyncWriteExt;
    let capture = Arc::new(capture);
    let serialize = Arc::new(serialize);
    loop {
        let mut stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                error!(socket = %socket_path.display(), error = %e, "inode table socket accept failed");
                continue;
            }
        };
        // Each connection gets its own task, so a client that never reads cannot hold up the next
        // one. Serializing reads paths and waits for any rename in flight
        // (RemapFs::serialize_table), which may be a host rename, so it runs off the async
        // workers.
        let capture = Arc::clone(&capture);
        let serialize = Arc::clone(&serialize);
        let socket_path = socket_path.clone();
        tokio::spawn(async move {
            let captured = match tokio::task::spawn_blocking(move || capture()).await {
                Ok(captured) => captured,
                Err(e) => {
                    error!(socket = %socket_path.display(), error = %e, "capturing inode tables failed");
                    return;
                }
            };
            if let Err(e) = stream.write_all(&[TABLES_CAPTURED]).await {
                error!(socket = %socket_path.display(), error = %e, "acknowledging the inode table capture failed");
                return;
            }
            let files = match tokio::task::spawn_blocking(move || serialize(captured)).await {
                Ok(files) => files,
                Err(e) => {
                    error!(socket = %socket_path.display(), error = %e, "serializing inode tables failed");
                    return;
                }
            };
            let body = serde_json::to_vec(&files).unwrap_or_default();
            if let Err(e) = stream.write_all(&body).await {
                error!(socket = %socket_path.display(), error = %e, "writing inode tables failed");
            }
            let _ = stream.shutdown().await;
        });
    }
}

/// The inode tables of a VM captured by [`capture_remote_inode_tables`], read with
/// [`RemoteInodeTables::read`].
pub struct RemoteInodeTables {
    stream: std::os::unix::net::UnixStream,
    socket_path: PathBuf,
}

/// Have the VM whose volume servers listen beside `vsock_socket_path` capture its inode tables.
///
/// Blocking, with a 30 s limit: it is called from the snapshot path's extra-files hook while the
/// VM is paused, and returns once the VM's fcvm process has captured the tables.
pub fn capture_remote_inode_tables(vsock_socket_path: &Path) -> Result<RemoteInodeTables> {
    use std::io::Read;
    let socket_path = inode_table_socket_path(vsock_socket_path);
    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "connecting to {} for the VM's volume inode tables (a VM started by an fcvm without \
             this socket cannot be snapshotted with volumes; restart it)",
            socket_path.display()
        )
    })?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let mut ack = [0u8; 1];
    stream.read_exact(&mut ack).with_context(|| {
        format!(
            "waiting for {} to capture the inode tables",
            socket_path.display()
        )
    })?;
    anyhow::ensure!(
        ack[0] == TABLES_CAPTURED,
        "{} answered {:#04x} where it acknowledges the inode table capture",
        socket_path.display(),
        ack[0]
    );
    Ok(RemoteInodeTables {
        stream,
        socket_path,
    })
}

impl RemoteInodeTables {
    /// Read the captured tables, as the snapshot files that carry them.
    pub fn read(mut self) -> Result<Vec<(String, Vec<u8>)>> {
        use std::io::Read;
        let mut body = Vec::new();
        self.stream
            .read_to_end(&mut body)
            .with_context(|| format!("reading inode tables from {}", self.socket_path.display()))?;
        let files: Vec<(String, String)> = serde_json::from_slice(&body)
            .with_context(|| format!("parsing inode tables from {}", self.socket_path.display()))?;
        Ok(files
            .into_iter()
            .map(|(name, json)| (name, json.into_bytes()))
            .collect())
    }
}

/// Result of spawning volume servers: the task handles, and where each volume's inode table
/// comes from at snapshot time.
pub struct SpawnedVolumes {
    pub handles: Vec<JoinHandle<()>>,
    /// One entry per volume config, in order.
    pub tables: Vec<TableSource>,
}

/// Spawn multiple VolumeServers and wait for all to be ready.
///
/// Each volume's server is kept in `SpawnedVolumes::tables`, so callers can read its inode table
/// at snapshot time.
pub async fn spawn_volume_servers(
    configs: &[VolumeConfig],
    vsock_socket_path: &Path,
) -> Result<SpawnedVolumes> {
    spawn_volume_servers_with_tables(configs, vsock_socket_path, &[]).await
}

/// Spawn VolumeServers, restoring each volume that has an inode table.
///
/// When `inode_tables[i]` is `Some(json)`, the volume at index `i` is served by
/// `RemapFs::restore_from_table()`, so the guest's inodes and handles from the snapshot's
/// source keep naming the same files, whether or not the source's volume was portable.
pub async fn spawn_volume_servers_with_tables(
    configs: &[VolumeConfig],
    vsock_socket_path: &Path,
    inode_tables: &[Option<String>],
) -> Result<SpawnedVolumes> {
    if configs.is_empty() {
        return Ok(SpawnedVolumes {
            handles: Vec::new(),
            tables: Vec::new(),
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
    let mut tables = Vec::with_capacity(configs.len());
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

        // A read-only volume's server answers OPEN and FLUSH with ENOSYS, so the guest kernel
        // stops sending OPEN, FLUSH and RELEASE and keeps file pages between opens
        // (PassthroughFs::try_for_volume). fc-agent mounts it read-only, and the server refuses
        // every request that could change it with EROFS (PassthroughFs::handle_request), so a
        // guest that remounts it read-write still cannot write to the host directory.
        let fs = fuse_pipe::PassthroughFs::for_volume(&host_path, config.read_only)
            .with_mount_points(&config.mount_points);
        let table = inode_tables.get(idx).and_then(|t| t.as_ref());
        let cfg = config.clone();
        let vsock_path = vsock_socket_path.to_path_buf();
        // A restored volume always goes through RemapFs: the guest holds the source's inode
        // numbers, which only the table can map to files (#1684).
        let handle = if config.portable || table.is_some() {
            let remap = if let Some(json) = table {
                info!(port, "restoring RemapFs from serialized inode table");
                Arc::new(fuse_pipe::RemapFs::restore_from_table(fs, json))
            } else {
                Arc::new(fuse_pipe::RemapFs::new(fs))
            };
            tables.push(TableSource::Remap(Arc::clone(&remap)));
            tokio::spawn(async move {
                if let Err(e) = VolumeServer::serve_vsock_arc(
                    &host_path,
                    &cfg,
                    &vsock_path,
                    remap,
                    Some(ready_tx),
                )
                .await
                {
                    error!("VolumeServer error for port {}: {}", port, e);
                }
            })
        } else {
            let plain = Arc::new(fs);
            tables.push(TableSource::Plain(Arc::clone(&plain)));
            tokio::spawn(async move {
                if let Err(e) = VolumeServer::serve_vsock_arc(
                    &host_path,
                    &cfg,
                    &vsock_path,
                    plain,
                    Some(ready_tx),
                )
                .await
                {
                    error!("VolumeServer error for port {}: {}", port, e);
                }
            })
        };
        handles.0.push(handle);

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

    let sources: Vec<_> = configs
        .iter()
        .zip(&tables)
        .map(|(config, source)| (config.port, source.clone()))
        .collect();
    handles.0.push(spawn_inode_table_server(
        inode_table_socket_path(vsock_socket_path),
        sources,
    )?);

    Ok(SpawnedVolumes {
        handles: std::mem::take(&mut handles.0),
        tables,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client that connects and never reads must not hold up the next `snapshot create`. The
    /// table here is larger than a Unix socket's buffer, so writing it to the idle client blocks.
    #[tokio::test]
    async fn an_idle_client_does_not_block_the_inode_table_server() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vsock.sock_inode_tables");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let big = "x".repeat(4 << 20);
        let server = tokio::spawn(serve_inode_tables(
            listener,
            socket.clone(),
            || (),
            move |()| vec![("volume-5000-inode-table.json".to_string(), big.clone())],
        ));

        let _idle = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let mut reader = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let mut body = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_to_end(&mut body),
        )
        .await;
        server.abort();
        assert!(
            read.is_ok(),
            "the second client got {} bytes in 5 s while the first connection sat unread",
            body.len()
        );
        assert!(body.len() > 4 << 20, "short table: {} bytes", body.len());
    }

    /// `fcvm snapshot create` gets exactly the tables the VM's volume servers hold, a portable one's
    /// and a plain one's, under the file names the restore path reads.
    #[tokio::test]
    async fn inode_tables_round_trip_through_the_socket() {
        use fuse_pipe::{FilesystemHandler, VolumeRequest, VolumeResponse};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("held.txt"), b"x").unwrap();
        let lookup = |fs: &dyn FilesystemHandler| {
            // The volume server's entry point; RemapFs does not implement the per-operation methods.
            match fs.handle_request_with_groups(
                &VolumeRequest::Lookup {
                    parent: 1,
                    name: b"held.txt".to_vec(),
                    uid: nix::unistd::Uid::effective().as_raw(),
                    gid: nix::unistd::Gid::effective().as_raw(),
                    pid: 0,
                },
                &[],
            ) {
                VolumeResponse::Entry { attr, .. } => attr.ino,
                other => panic!("lookup: {other:?}"),
            }
        };
        let remap = Arc::new(fuse_pipe::RemapFs::new(
            fuse_pipe::PassthroughFs::for_volume(dir.path(), false),
        ));
        let plain = Arc::new(fuse_pipe::PassthroughFs::for_volume(dir.path(), false));
        lookup(remap.as_ref());
        lookup(plain.as_ref());

        let vsock = dir.path().join("vsock.sock");
        let server = spawn_inode_table_server(
            inode_table_socket_path(&vsock),
            vec![
                (5001, TableSource::Remap(Arc::clone(&remap))),
                (5002, TableSource::Plain(Arc::clone(&plain))),
            ],
        )
        .unwrap();
        let captured = {
            let vsock = vsock.clone();
            tokio::task::spawn_blocking(move || capture_remote_inode_tables(&vsock))
                .await
                .unwrap()
                .unwrap()
        };
        let files = tokio::task::spawn_blocking(move || captured.read())
            .await
            .unwrap()
            .unwrap();
        server.abort();

        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [inode_table_file_name(5001), inode_table_file_name(5002)]
        );
        for (name, table) in &files {
            let table = String::from_utf8_lossy(table);
            assert!(
                table.contains("held.txt"),
                "{name} does not name the file the guest held: {table}"
            );
        }
    }

    /// The capture returns once the VM's server has captured the tables, without waiting for it to
    /// read their paths: the creator calls it while the VM is paused and resumes the VM before
    /// reading (#1684). The serializer here blocks until the test releases it, so a capture that
    /// waited for it would never return.
    #[tokio::test]
    async fn the_capture_returns_after_the_capture_and_before_the_read() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let vsock = dir.path().join("vsock.sock");
        let socket = inode_table_socket_path(&vsock);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let captured = Arc::new(AtomicBool::new(false));
        let serialized = Arc::new(AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        let server = tokio::spawn(serve_inode_tables(
            listener,
            socket,
            {
                let captured = Arc::clone(&captured);
                move || captured.store(true, Ordering::SeqCst)
            },
            {
                let serialized = Arc::clone(&serialized);
                move |()| {
                    released.lock().unwrap().recv().unwrap();
                    serialized.store(true, Ordering::SeqCst);
                    vec![("volume-5000-inode-table.json".to_string(), "{}".to_string())]
                }
            },
        ));
        let remote = {
            let vsock = vsock.clone();
            tokio::task::spawn_blocking(move || capture_remote_inode_tables(&vsock))
                .await
                .unwrap()
                .unwrap()
        };
        assert!(
            captured.load(Ordering::SeqCst),
            "returned before the capture"
        );
        assert!(
            !serialized.load(Ordering::SeqCst),
            "the serializer finished before it was released"
        );
        release.send(()).unwrap();
        let files = tokio::task::spawn_blocking(move || remote.read())
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert!(serialized.load(Ordering::SeqCst));
        assert_eq!(files.len(), 1);
    }

    /// With no server, the fetch fails instead of returning no tables, so a snapshot of a VM with
    /// volumes cannot silently be written without them.
    #[test]
    fn fetching_without_a_server_fails() {
        let dir = tempfile::tempdir().unwrap();
        let error = capture_remote_inode_tables(&dir.path().join("vsock.sock"))
            .err()
            .expect("no server, no tables");
        assert!(format!("{error:#}").contains("inode tables"), "{error:#}");
    }
}
