//! VolumeServer - serves host directories to guests via vsock.
//!
//! Supports multiple concurrent clients (original VM + clones).

use crate::volume::protocol::{
    DirEntry, FileAttr, VolumeRequest, VolumeResponse, MAX_MESSAGE_SIZE,
};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

/// Root inode number (FUSE convention)
const ROOT_INO: u64 = 1;

/// TTL for cached attributes (seconds)
const ATTR_TTL_SECS: u64 = 1;

/// Inode table mapping inode numbers to paths.
/// Shared across all clients.
struct InodeTable {
    /// Inode to path mapping
    ino_to_path: RwLock<HashMap<u64, PathBuf>>,
    /// Path to inode mapping (for reusing inodes)
    path_to_ino: RwLock<HashMap<PathBuf, u64>>,
    /// Next inode number to allocate
    next_ino: AtomicU64,
    /// Base path (the mounted directory) - stored for future use
    #[allow(dead_code)]
    base_path: PathBuf,
}

impl InodeTable {
    fn new(base_path: PathBuf) -> Self {
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();

        // Root inode maps to base path
        ino_to_path.insert(ROOT_INO, base_path.clone());
        path_to_ino.insert(base_path.clone(), ROOT_INO);

        Self {
            ino_to_path: RwLock::new(ino_to_path),
            path_to_ino: RwLock::new(path_to_ino),
            next_ino: AtomicU64::new(ROOT_INO + 1),
            base_path,
        }
    }

    /// Get path for an inode
    fn get_path(&self, ino: u64) -> Option<PathBuf> {
        self.ino_to_path.read().unwrap().get(&ino).cloned()
    }

    /// Get or create inode for a path
    fn get_or_create_ino(&self, path: &Path) -> u64 {
        // Check if we already have this path
        {
            let path_to_ino = self.path_to_ino.read().unwrap();
            if let Some(&ino) = path_to_ino.get(path) {
                return ino;
            }
        }

        // Allocate new inode
        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);

        // Store mappings
        {
            let mut ino_to_path = self.ino_to_path.write().unwrap();
            let mut path_to_ino = self.path_to_ino.write().unwrap();
            ino_to_path.insert(ino, path.to_path_buf());
            path_to_ino.insert(path.to_path_buf(), ino);
        }

        ino
    }

    /// Remove inode mapping (for deleted files)
    fn remove_ino(&self, ino: u64) {
        let mut ino_to_path = self.ino_to_path.write().unwrap();
        let mut path_to_ino = self.path_to_ino.write().unwrap();

        if let Some(path) = ino_to_path.remove(&ino) {
            path_to_ino.remove(&path);
        }
    }
}

/// Per-client file handle table.
struct HandleTable {
    handles: HashMap<u64, File>,
    next_fh: u64,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            handles: HashMap::new(),
            next_fh: 1,
        }
    }

    fn insert(&mut self, file: File) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, file);
        fh
    }

    fn get_mut(&mut self, fh: u64) -> Option<&mut File> {
        self.handles.get_mut(&fh)
    }

    fn remove(&mut self, fh: u64) -> Option<File> {
        self.handles.remove(&fh)
    }
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
pub struct VolumeServer {
    config: VolumeConfig,
    inodes: Arc<InodeTable>,
}

impl VolumeServer {
    /// Create a new volume server.
    pub fn new(config: VolumeConfig) -> Result<Self> {
        let host_path = config
            .host_path
            .canonicalize()
            .with_context(|| format!("Failed to resolve path: {:?}", config.host_path))?;

        if !host_path.is_dir() {
            bail!("Volume path is not a directory: {:?}", host_path);
        }

        let inodes = Arc::new(InodeTable::new(host_path));

        Ok(Self { config, inodes })
    }

    /// Serve volumes over a Unix socket (for testing/development).
    pub async fn serve_unix(&self, socket_path: &Path) -> Result<()> {
        // Remove existing socket
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("Failed to bind Unix socket: {:?}", socket_path))?;

        info!(
            "VolumeServer listening on {:?} for {:?}",
            socket_path, self.config.host_path
        );

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let inodes = Arc::clone(&self.inodes);
                    let read_only = self.config.read_only;
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_client(stream, inodes, read_only).await {
                            error!("Client error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
        }
    }

    /// Serve volumes over Firecracker's vsock Unix socket.
    ///
    /// For guest-initiated connections, Firecracker's vsock expects the host to listen on:
    ///   `{uds_path}_{port}` (e.g., `/path/to/v.sock_5000`)
    ///
    /// When guest connects to CID 2, port 5000, Firecracker forwards to host's v.sock_5000.
    /// No handshake is needed - the connection is directly forwarded.
    pub async fn serve_vsock(&self, vsock_base_path: &Path) -> Result<()> {
        // For guest-initiated connections, listen on {base_path}_{port}
        let socket_path = PathBuf::from(format!(
            "{}_{}",
            vsock_base_path.display(),
            self.config.port
        ));

        // Remove existing socket file
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind vsock socket: {:?}", socket_path))?;

        info!(
            "VolumeServer listening on {:?} for {:?}",
            socket_path, self.config.host_path
        );

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let inodes = Arc::clone(&self.inodes);
                    let read_only = self.config.read_only;
                    let port = self.config.port;
                    tokio::spawn(async move {
                        info!("Guest connected on vsock port {}", port);
                        if let Err(e) = Self::handle_client(stream, inodes, read_only).await {
                            error!("Vsock client error on port {}: {}", port, e);
                        }
                    });
                }
                Err(e) => {
                    error!("Vsock accept error: {}", e);
                }
            }
        }
    }

    /// Handle a single client connection.
    async fn handle_client(
        mut stream: tokio::net::UnixStream,
        inodes: Arc<InodeTable>,
        read_only: bool,
    ) -> Result<()> {
        let mut handles = HandleTable::new();
        let mut len_buf = [0u8; 4];

        info!("New client connected");

        loop {
            // Read message length
            match stream.read_exact(&mut len_buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    info!("Client disconnected");
                    break;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }

            let len = u32::from_be_bytes(len_buf) as usize;
            if len > MAX_MESSAGE_SIZE {
                bail!("Message too large: {} bytes", len);
            }

            // Read message body
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;

            // Decode request
            let request: VolumeRequest = bincode::deserialize(&buf)
                .with_context(|| "Failed to decode request")?;

            debug!("Request: {:?}", request);

            // Handle request
            let response =
                Self::handle_request(&request, &inodes, &mut handles, read_only);

            debug!("Response: {:?}", response);

            // Encode response
            let response_buf = bincode::serialize(&response)?;
            let response_len = (response_buf.len() as u32).to_be_bytes();

            // Send response
            stream.write_all(&response_len).await?;
            stream.write_all(&response_buf).await?;
        }

        Ok(())
    }

    /// Handle a single request.
    fn handle_request(
        request: &VolumeRequest,
        inodes: &InodeTable,
        handles: &mut HandleTable,
        read_only: bool,
    ) -> VolumeResponse {
        match request {
            VolumeRequest::Lookup { parent, name } => {
                Self::handle_lookup(inodes, *parent, name)
            }
            VolumeRequest::Getattr { ino } => Self::handle_getattr(inodes, *ino),
            VolumeRequest::Setattr {
                ino,
                mode,
                uid,
                gid,
                size,
                atime_secs,
                atime_nsecs,
                mtime_secs,
                mtime_nsecs,
            } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_setattr(
                    inodes,
                    *ino,
                    *mode,
                    *uid,
                    *gid,
                    *size,
                    *atime_secs,
                    *atime_nsecs,
                    *mtime_secs,
                    *mtime_nsecs,
                )
            }
            VolumeRequest::Readdir { ino, offset } => {
                Self::handle_readdir(inodes, *ino, *offset)
            }
            VolumeRequest::Mkdir { parent, name, mode } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_mkdir(inodes, *parent, name, *mode)
            }
            VolumeRequest::Rmdir { parent, name } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_rmdir(inodes, *parent, name)
            }
            VolumeRequest::Create {
                parent,
                name,
                mode,
                flags,
            } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_create(inodes, handles, *parent, name, *mode, *flags)
            }
            VolumeRequest::Open { ino, flags } => {
                // Check read-only for write flags
                if read_only && (*flags & (libc::O_WRONLY | libc::O_RDWR) as u32) != 0 {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_open(inodes, handles, *ino, *flags)
            }
            VolumeRequest::Read {
                ino: _,
                fh,
                offset,
                size,
            } => Self::handle_read(handles, *fh, *offset, *size),
            VolumeRequest::Write {
                ino: _,
                fh,
                offset,
                data,
            } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_write(handles, *fh, *offset, data)
            }
            VolumeRequest::Release { ino: _, fh } => Self::handle_release(handles, *fh),
            VolumeRequest::Flush { ino: _, fh } => Self::handle_flush(handles, *fh),
            VolumeRequest::Fsync {
                ino: _,
                fh,
                datasync,
            } => Self::handle_fsync(handles, *fh, *datasync),
            VolumeRequest::Unlink { parent, name } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_unlink(inodes, *parent, name)
            }
            VolumeRequest::Rename {
                parent,
                name,
                newparent,
                newname,
            } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_rename(inodes, *parent, name, *newparent, newname)
            }
            VolumeRequest::Symlink {
                parent,
                name,
                target,
            } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_symlink(inodes, *parent, name, target)
            }
            VolumeRequest::Readlink { ino } => Self::handle_readlink(inodes, *ino),
            VolumeRequest::Link {
                ino,
                newparent,
                newname,
            } => {
                if read_only {
                    return VolumeResponse::Error { errno: libc::EROFS };
                }
                Self::handle_link(inodes, *ino, *newparent, newname)
            }
            VolumeRequest::Access { ino, mask } => Self::handle_access(inodes, *ino, *mask),
            VolumeRequest::Statfs { ino } => Self::handle_statfs(inodes, *ino),
        }
    }

    fn handle_lookup(inodes: &InodeTable, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let path = parent_path.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let ino = inodes.get_or_create_ino(&path);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: ATTR_TTL_SECS,
        }
    }

    fn handle_getattr(inodes: &InodeTable, ino: u64) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let attr = metadata_to_attr(ino, &metadata);
        VolumeResponse::Attr {
            attr,
            ttl_secs: ATTR_TTL_SECS,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_setattr(
        inodes: &InodeTable,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime_secs: Option<i64>,
        _atime_nsecs: Option<u32>,
        mtime_secs: Option<i64>,
        _mtime_nsecs: Option<u32>,
    ) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        // Set mode
        if let Some(mode) = mode {
            if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(mode)) {
                return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
            }
        }

        // Set ownership
        if uid.is_some() || gid.is_some() {
            let uid = uid.map(|u| nix::unistd::Uid::from_raw(u));
            let gid = gid.map(|g| nix::unistd::Gid::from_raw(g));
            if let Err(e) = nix::unistd::chown(&path, uid, gid) {
                return VolumeResponse::Error { errno: e as i32 };
            }
        }

        // Set size (truncate)
        if let Some(size) = size {
            let file = match OpenOptions::new().write(true).open(&path) {
                Ok(f) => f,
                Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
            };
            if let Err(e) = file.set_len(size) {
                return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
            }
        }

        // Set times
        if atime_secs.is_some() || mtime_secs.is_some() {
            use std::time::{Duration, UNIX_EPOCH};

            let atime = atime_secs.map(|s| {
                if s >= 0 {
                    UNIX_EPOCH + Duration::from_secs(s as u64)
                } else {
                    UNIX_EPOCH - Duration::from_secs((-s) as u64)
                }
            });
            let mtime = mtime_secs.map(|s| {
                if s >= 0 {
                    UNIX_EPOCH + Duration::from_secs(s as u64)
                } else {
                    UNIX_EPOCH - Duration::from_secs((-s) as u64)
                }
            });

            let file = match OpenOptions::new().write(true).open(&path) {
                Ok(f) => f,
                Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
            };

            if let (Some(atime), Some(mtime)) = (atime, mtime) {
                if let Err(e) = file.set_times(std::fs::FileTimes::new().set_accessed(atime).set_modified(mtime)) {
                    warn!("Failed to set times: {}", e);
                }
            }
        }

        // Return updated attributes
        Self::handle_getattr(inodes, ino)
    }

    fn handle_readdir(inodes: &InodeTable, ino: u64, offset: u64) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let read_dir = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let mut entries = Vec::new();

        // Add . and ..
        if offset == 0 {
            entries.push(DirEntry {
                ino,
                name: ".".to_string(),
                file_type: crate::volume::protocol::file_type::DIR,
            });
        }
        if offset <= 1 {
            // For .., use parent inode or self for root
            let parent_ino = if ino == ROOT_INO {
                ROOT_INO
            } else if let Some(parent_path) = path.parent() {
                inodes.get_or_create_ino(parent_path)
            } else {
                ROOT_INO
            };
            entries.push(DirEntry {
                ino: parent_ino,
                name: "..".to_string(),
                file_type: crate::volume::protocol::file_type::DIR,
            });
        }

        // Add directory entries
        let skip = if offset >= 2 { offset - 2 } else { 0 };
        for entry in read_dir.skip(skip as usize) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let entry_path = entry.path();
            let entry_ino = inodes.get_or_create_ino(&entry_path);

            let file_type = match entry.file_type() {
                Ok(ft) => {
                    if ft.is_dir() {
                        crate::volume::protocol::file_type::DIR
                    } else if ft.is_symlink() {
                        crate::volume::protocol::file_type::LNK
                    } else {
                        crate::volume::protocol::file_type::REG
                    }
                }
                Err(_) => crate::volume::protocol::file_type::UNKNOWN,
            };

            entries.push(DirEntry {
                ino: entry_ino,
                name: entry.file_name().to_string_lossy().to_string(),
                file_type,
            });
        }

        VolumeResponse::DirEntries { entries }
    }

    fn handle_mkdir(inodes: &InodeTable, parent: u64, name: &str, mode: u32) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let path = parent_path.join(name);
        if let Err(e) = fs::create_dir(&path) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        // Set mode
        if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(mode)) {
            warn!("Failed to set mode on new directory: {}", e);
        }

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let ino = inodes.get_or_create_ino(&path);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: ATTR_TTL_SECS,
        }
    }

    fn handle_rmdir(inodes: &InodeTable, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let path = parent_path.join(name);

        // Get inode before removal
        if let Some(ino) = inodes.path_to_ino.read().unwrap().get(&path).copied() {
            inodes.remove_ino(ino);
        }

        if let Err(e) = fs::remove_dir(&path) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        VolumeResponse::Ok
    }

    fn handle_create(
        inodes: &InodeTable,
        handles: &mut HandleTable,
        parent: u64,
        name: &str,
        mode: u32,
        flags: u32,
    ) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let path = parent_path.join(name);

        let mut opts = OpenOptions::new();
        opts.create(true).write(true).mode(mode);

        if (flags & libc::O_TRUNC as u32) != 0 {
            opts.truncate(true);
        }
        if (flags & libc::O_APPEND as u32) != 0 {
            opts.append(true);
        }

        let file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let ino = inodes.get_or_create_ino(&path);
        let attr = metadata_to_attr(ino, &metadata);
        let _fh = handles.insert(file);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: ATTR_TTL_SECS,
        }
    }

    fn handle_open(
        inodes: &InodeTable,
        handles: &mut HandleTable,
        ino: u64,
        flags: u32,
    ) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let mut opts = OpenOptions::new();

        let access_mode = flags & (libc::O_RDONLY | libc::O_WRONLY | libc::O_RDWR) as u32;
        match access_mode as i32 {
            libc::O_RDONLY => {
                opts.read(true);
            }
            libc::O_WRONLY => {
                opts.write(true);
            }
            libc::O_RDWR => {
                opts.read(true).write(true);
            }
            _ => {
                opts.read(true);
            }
        }

        if (flags & libc::O_APPEND as u32) != 0 {
            opts.append(true);
        }
        if (flags & libc::O_TRUNC as u32) != 0 {
            opts.truncate(true);
        }

        let file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let fh = handles.insert(file);

        VolumeResponse::Opened { fh, flags }
    }

    fn handle_read(handles: &mut HandleTable, fh: u64, offset: u64, size: u32) -> VolumeResponse {
        let file = match handles.get_mut(fh) {
            Some(f) => f,
            None => return VolumeResponse::Error { errno: libc::EBADF },
        };

        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        let mut buf = vec![0u8; size as usize];
        match file.read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                VolumeResponse::Data { data: buf }
            }
            Err(e) => VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        }
    }

    fn handle_write(handles: &mut HandleTable, fh: u64, offset: u64, data: &[u8]) -> VolumeResponse {
        let file = match handles.get_mut(fh) {
            Some(f) => f,
            None => return VolumeResponse::Error { errno: libc::EBADF },
        };

        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        match file.write(data) {
            Ok(n) => VolumeResponse::Written { size: n as u32 },
            Err(e) => VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        }
    }

    fn handle_release(handles: &mut HandleTable, fh: u64) -> VolumeResponse {
        handles.remove(fh);
        VolumeResponse::Ok
    }

    fn handle_flush(handles: &mut HandleTable, fh: u64) -> VolumeResponse {
        let file = match handles.get_mut(fh) {
            Some(f) => f,
            None => return VolumeResponse::Error { errno: libc::EBADF },
        };

        match file.sync_data() {
            Ok(_) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        }
    }

    fn handle_fsync(handles: &mut HandleTable, fh: u64, datasync: bool) -> VolumeResponse {
        let file = match handles.get_mut(fh) {
            Some(f) => f,
            None => return VolumeResponse::Error { errno: libc::EBADF },
        };

        let result = if datasync {
            file.sync_data()
        } else {
            file.sync_all()
        };

        match result {
            Ok(_) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        }
    }

    fn handle_unlink(inodes: &InodeTable, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let path = parent_path.join(name);

        // Get inode before removal
        if let Some(ino) = inodes.path_to_ino.read().unwrap().get(&path).copied() {
            inodes.remove_ino(ino);
        }

        if let Err(e) = fs::remove_file(&path) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        VolumeResponse::Ok
    }

    fn handle_rename(
        inodes: &InodeTable,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
    ) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let newparent_path = match inodes.get_path(newparent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let old_path = parent_path.join(name);
        let new_path = newparent_path.join(newname);

        // Update inode mapping
        if let Some(ino) = inodes.path_to_ino.read().unwrap().get(&old_path).copied() {
            let mut ino_to_path = inodes.ino_to_path.write().unwrap();
            let mut path_to_ino = inodes.path_to_ino.write().unwrap();
            path_to_ino.remove(&old_path);
            path_to_ino.insert(new_path.clone(), ino);
            ino_to_path.insert(ino, new_path.clone());
        }

        if let Err(e) = fs::rename(&old_path, &new_path) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        VolumeResponse::Ok
    }

    fn handle_symlink(
        inodes: &InodeTable,
        parent: u64,
        name: &str,
        target: &str,
    ) -> VolumeResponse {
        let parent_path = match inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let path = parent_path.join(name);

        #[cfg(unix)]
        {
            if let Err(e) = std::os::unix::fs::symlink(target, &path) {
                return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
            }
        }

        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let ino = inodes.get_or_create_ino(&path);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: ATTR_TTL_SECS,
        }
    }

    fn handle_readlink(inodes: &InodeTable, ino: u64) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        match fs::read_link(&path) {
            Ok(target) => VolumeResponse::Symlink {
                target: target.to_string_lossy().to_string(),
            },
            Err(e) => VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        }
    }

    fn handle_link(
        inodes: &InodeTable,
        ino: u64,
        newparent: u64,
        newname: &str,
    ) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let newparent_path = match inodes.get_path(newparent) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        let new_path = newparent_path.join(newname);

        if let Err(e) = fs::hard_link(&path, &new_path) {
            return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) };
        }

        // Reuse same inode for hard link
        inodes.path_to_ino.write().unwrap().insert(new_path, ino);

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::Error { errno: e.raw_os_error().unwrap_or(libc::EIO) },
        };

        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: ATTR_TTL_SECS,
        }
    }

    fn handle_access(inodes: &InodeTable, ino: u64, mask: u32) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        // Use libc::access for proper permission checking
        use std::ffi::CString;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return VolumeResponse::Error { errno: libc::EINVAL },
        };

        let result = unsafe { libc::access(c_path.as_ptr(), mask as i32) };

        if result == 0 {
            VolumeResponse::Ok
        } else {
            VolumeResponse::Error {
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EACCES),
            }
        }
    }

    fn handle_statfs(inodes: &InodeTable, ino: u64) -> VolumeResponse {
        let path = match inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::Error { errno: libc::ENOENT },
        };

        use std::ffi::CString;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return VolumeResponse::Error { errno: libc::EINVAL },
        };

        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };

        if result != 0 {
            return VolumeResponse::Error {
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO),
            };
        }

        VolumeResponse::Statfs {
            blocks: stat.f_blocks as u64,
            bfree: stat.f_bfree as u64,
            bavail: stat.f_bavail as u64,
            files: stat.f_files as u64,
            ffree: stat.f_ffree as u64,
            bsize: stat.f_bsize as u32,
            namelen: stat.f_namelen as u32,
            frsize: stat.f_frsize as u32,
        }
    }
}

/// Convert std::fs::Metadata to FileAttr
fn metadata_to_attr(ino: u64, metadata: &fs::Metadata) -> FileAttr {
    FileAttr {
        ino,
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime_secs: metadata.atime(),
        atime_nsecs: metadata.atime_nsec() as u32,
        mtime_secs: metadata.mtime(),
        mtime_nsecs: metadata.mtime_nsec() as u32,
        ctime_secs: metadata.ctime(),
        ctime_nsecs: metadata.ctime_nsec() as u32,
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_inode_table() {
        let tmp = tempdir().unwrap();
        let table = InodeTable::new(tmp.path().to_path_buf());

        // Root inode should exist
        assert_eq!(table.get_path(ROOT_INO), Some(tmp.path().to_path_buf()));

        // Create a new file path
        let file_path = tmp.path().join("test.txt");
        let ino = table.get_or_create_ino(&file_path);
        assert!(ino > ROOT_INO);

        // Should return same inode for same path
        assert_eq!(table.get_or_create_ino(&file_path), ino);

        // Path lookup should work
        assert_eq!(table.get_path(ino), Some(file_path.clone()));

        // Remove inode
        table.remove_ino(ino);
        assert_eq!(table.get_path(ino), None);
    }

    #[test]
    fn test_handle_table() {
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let mut handles = HandleTable::new();

        // Open file and add handle
        let file = File::open(&file_path).unwrap();
        let fh = handles.insert(file);
        assert_eq!(fh, 1);

        // Get handle
        assert!(handles.get_mut(fh).is_some());

        // Remove handle
        assert!(handles.remove(fh).is_some());
        assert!(handles.get_mut(fh).is_none());
    }
}
