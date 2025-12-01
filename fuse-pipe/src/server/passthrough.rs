//! Passthrough filesystem implementation using fuse-backend-rs.
//!
//! This wraps the production-grade passthrough filesystem from the
//! Cloud Hypervisor project. It provides full POSIX semantics with
//! proper permission enforcement.

use super::credentials::CredentialsGuard;
use super::handler::FilesystemHandler;
use crate::protocol::{file_type, DirEntry, FileAttr, VolumeResponse};

use fuse_backend_rs::api::filesystem::{Context, FileSystem, Entry};
use fuse_backend_rs::passthrough::{Config, PassthroughFs as FuseBackendPassthrough};
use fuse_backend_rs::abi::fuse_abi::CreateIn;

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default attribute TTL in seconds.
const ATTR_TTL_SECS: u64 = 1;

/// File handle table for managing open files.
/// We maintain our own handle table for read/write operations.
struct HandleTable {
    handles: Mutex<HashMap<u64, File>>,
    next_fh: AtomicU64,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn insert(&self, file: File) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.handles.lock().unwrap().insert(fh, file);
        fh
    }

    fn with_file<F, R>(&self, fh: u64, f: F) -> Option<R>
    where
        F: FnOnce(&mut File) -> R,
    {
        let mut handles = self.handles.lock().unwrap();
        handles.get_mut(&fh).map(f)
    }

    fn remove(&self, fh: u64) -> Option<File> {
        self.handles.lock().unwrap().remove(&fh)
    }
}

/// A passthrough filesystem that maps operations to a local directory.
///
/// This implementation wraps fuse-backend-rs's production-grade PassthroughFs,
/// providing full POSIX semantics with proper permission enforcement, inode
/// tracking, and file handle management.
pub struct PassthroughFs {
    inner: Arc<FuseBackendPassthrough>,
    root_path: PathBuf,
    attr_ttl_secs: u64,
    // Our own handle table for simpler read/write operations
    handles: HandleTable,
}

impl PassthroughFs {
    /// Create a new passthrough filesystem rooted at the given path.
    pub fn new<P: Into<PathBuf>>(root_path: P) -> Self {
        let root_path = root_path.into();
        let root_dir = root_path.to_string_lossy().to_string();

        let cfg = Config {
            root_dir,
            do_import: true,
            writeback: false,
            no_open: false,
            no_opendir: false,
            xattr: true,
            cache_policy: fuse_backend_rs::passthrough::CachePolicy::Auto,
            attr_timeout: Duration::from_secs(ATTR_TTL_SECS),
            entry_timeout: Duration::from_secs(ATTR_TTL_SECS),
            ..Default::default()
        };

        let inner = FuseBackendPassthrough::new(cfg)
            .expect("Failed to create passthrough filesystem");

        // Initialize the filesystem
        inner.import().expect("Failed to import filesystem");

        Self {
            inner: Arc::new(inner),
            root_path,
            attr_ttl_secs: ATTR_TTL_SECS,
            handles: HandleTable::new(),
        }
    }

    /// Set the attribute TTL.
    pub fn with_attr_ttl(mut self, secs: u64) -> Self {
        self.attr_ttl_secs = secs;
        self
    }

    /// Get the root path.
    pub fn root_path(&self) -> &PathBuf {
        &self.root_path
    }

    /// Create a Context from uid/gid/pid.
    fn make_context(uid: u32, gid: u32, pid: u32) -> Context {
        let pid = if pid == 0 {
            std::process::id()
        } else {
            pid
        };
        Context {
            uid,
            gid,
            pid: pid as i32,
        }
    }

    /// Convert fuse-backend-rs Entry to our FileAttr.
    fn entry_to_attr(entry: &Entry) -> FileAttr {
        let attr = &entry.attr;
        FileAttr {
            ino: entry.inode,
            size: attr.st_size as u64,
            blocks: attr.st_blocks as u64,
            atime_secs: attr.st_atime,
            atime_nsecs: attr.st_atime_nsec as u32,
            mtime_secs: attr.st_mtime,
            mtime_nsecs: attr.st_mtime_nsec as u32,
            ctime_secs: attr.st_ctime,
            ctime_nsecs: attr.st_ctime_nsec as u32,
            mode: attr.st_mode,
            nlink: attr.st_nlink as u32,
            uid: attr.st_uid,
            gid: attr.st_gid,
            rdev: attr.st_rdev as u32,
            blksize: attr.st_blksize as u32,
        }
    }

    /// Convert libc::stat64 to our FileAttr.
    fn stat_to_attr(ino: u64, st: &libc::stat64) -> FileAttr {
        FileAttr {
            ino,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            atime_secs: st.st_atime,
            atime_nsecs: st.st_atime_nsec as u32,
            mtime_secs: st.st_mtime,
            mtime_nsecs: st.st_mtime_nsec as u32,
            ctime_secs: st.st_ctime,
            ctime_nsecs: st.st_ctime_nsec as u32,
            mode: st.st_mode,
            nlink: st.st_nlink as u32,
            uid: st.st_uid,
            gid: st.st_gid,
            rdev: st.st_rdev as u32,
            blksize: st.st_blksize as u32,
        }
    }

    /// Get the file path for an inode by looking it up via fuse-backend-rs.
    /// This uses readlinkat on /proc/self/fd to get the path.
    fn get_inode_path(&self, ino: u64) -> Option<PathBuf> {
        // Use fuse-backend-rs's internal method to get the path
        // This returns a PathBuf directly
        self.inner.readlinkat_proc_file(ino).ok()
    }
}

impl FilesystemHandler for PassthroughFs {
    fn lookup(
        &self,
        parent: u64,
        name: &str,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials for permission check
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.lookup(&ctx, parent, &cname) {
            Ok(entry) => {
                let attr = Self::entry_to_attr(&entry);
                VolumeResponse::Entry {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn getattr(&self, ino: u64) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.getattr(&ctx, ino, None) {
            Ok((st, _timeout)) => {
                let attr = Self::stat_to_attr(ino, &st);
                VolumeResponse::Attr {
                    attr,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime_secs: Option<i64>,
        atime_nsecs: Option<u32>,
        mtime_secs: Option<i64>,
        mtime_nsecs: Option<u32>,
        caller_uid: u32,
        caller_gid: u32,
        caller_pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(caller_uid, caller_gid, caller_pid);

        // Get current file metadata first
        let current_attr = match self.inner.getattr(&ctx, ino, None) {
            Ok((st, _)) => st,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };
        let file_uid = current_attr.st_uid;

        // Check permissions for operations that require ownership
        // chmod: POSIX requires caller to be owner or root
        if mode.is_some() && caller_uid != 0 && caller_uid != file_uid {
            return VolumeResponse::error(libc::EPERM);
        }

        // chown: Only root can change uid
        if uid.is_some() && caller_uid != 0 {
            return VolumeResponse::error(libc::EPERM);
        }

        // chown gid: non-root must be owner
        if gid.is_some() && caller_uid != 0 && caller_uid != file_uid {
            return VolumeResponse::error(libc::EPERM);
        }

        // Get the file path to perform operations directly
        let path = match self.get_inode_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::error(libc::EIO),
        };

        // Use credentials guard for permission check
        let _guard = match CredentialsGuard::new(caller_uid, caller_gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        // Handle size truncation
        if let Some(new_size) = size {
            if let Err(e) = std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(new_size))
            {
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }

        // Handle mode change
        if let Some(new_mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(new_mode);
            if let Err(e) = std::fs::set_permissions(&path, permissions) {
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }

        // Handle uid/gid change using libc::chown
        if uid.is_some() || gid.is_some() {
            let new_uid = uid.map(|u| u as libc::uid_t).unwrap_or(u32::MAX as libc::uid_t);
            let new_gid = gid.map(|g| g as libc::gid_t).unwrap_or(u32::MAX as libc::gid_t);
            let path_cstr = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let result = unsafe { libc::chown(path_cstr.as_ptr(), new_uid, new_gid) };
            if result != 0 {
                return VolumeResponse::error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            }
        }

        // Handle time changes
        if atime_secs.is_some() || mtime_secs.is_some() {
            let path_cstr = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let times = [
                libc::timespec {
                    tv_sec: atime_secs.unwrap_or(current_attr.st_atime),
                    tv_nsec: atime_nsecs.map(|n| n as i64).unwrap_or(current_attr.st_atime_nsec),
                },
                libc::timespec {
                    tv_sec: mtime_secs.unwrap_or(current_attr.st_mtime),
                    tv_nsec: mtime_nsecs.map(|n| n as i64).unwrap_or(current_attr.st_mtime_nsec),
                },
            ];
            let result = unsafe { libc::utimensat(libc::AT_FDCWD, path_cstr.as_ptr(), times.as_ptr(), 0) };
            if result != 0 {
                return VolumeResponse::error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO));
            }
        }

        // Get updated attributes
        match self.inner.getattr(&ctx, ino, None) {
            Ok((new_st, _timeout)) => {
                let attr = Self::stat_to_attr(ino, &new_st);
                VolumeResponse::Attr {
                    attr,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn readdir(
        &self,
        ino: u64,
        offset: u64,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials for permission check
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        // First open the directory
        let (handle, _) = match self.inner.opendir(&ctx, ino, libc::O_RDONLY as u32) {
            Ok(h) => h,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let mut entries = Vec::new();

        // Read directory entries using fuse-backend-rs's readdir
        // The callback takes a single DirEntry argument
        let mut add_entry = |entry: fuse_backend_rs::api::filesystem::DirEntry| -> std::io::Result<usize> {
            // entry.name is already a &[u8]
            let name_str = String::from_utf8_lossy(entry.name).to_string();

            // Skip . and .. as we add them manually for offset 0
            if name_str != "." && name_str != ".." {
                entries.push(DirEntry {
                    ino: entry.ino,
                    name: name_str,
                    file_type: file_type::from_mode(entry.type_ as u32),
                });
            }
            Ok(1)
        };

        if let Err(e) = self.inner.readdir(&ctx, ino, handle.unwrap_or(0), 8192, offset, &mut add_entry) {
            let _ = self.inner.releasedir(&ctx, ino, 0, handle.unwrap_or(0));
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        // Release the directory handle
        let _ = self.inner.releasedir(&ctx, ino, 0, handle.unwrap_or(0));

        // Add . and .. at the beginning for offset 0
        if offset == 0 {
            let mut full_entries = Vec::new();
            full_entries.push(DirEntry::dot(ino));

            // Get parent inode - for root, parent is self
            let parent_ino = if ino == 1 {
                1
            } else {
                let dotdot = CString::new("..").unwrap();
                match self.inner.lookup(&ctx, ino, &dotdot) {
                    Ok(entry) => entry.inode,
                    Err(_) => ino, // Fallback to self if can't find parent
                }
            };
            full_entries.push(DirEntry::dotdot(parent_ino));

            full_entries.extend(entries);
            entries = full_entries;
        }

        VolumeResponse::DirEntries { entries }
    }

    fn mkdir(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.mkdir(&ctx, parent, &cname, mode, 0) {
            Ok(entry) => {
                let attr = Self::entry_to_attr(&entry);
                VolumeResponse::Entry {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn mknod(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        rdev: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.mknod(&ctx, parent, &cname, mode, rdev, 0) {
            Ok(entry) => {
                let attr = Self::entry_to_attr(&entry);
                VolumeResponse::Entry {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn rmdir(
        &self,
        parent: u64,
        name: &str,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.rmdir(&ctx, parent, &cname) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn create(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);
        tracing::debug!(target: "passthrough", parent, name, mode, flags, uid, gid, "create");

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!(target: "passthrough", error = ?e, "credentials guard failed");
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM));
            }
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        let create_in = CreateIn {
            flags,
            mode,
            umask: 0,
            fuse_flags: 0,
        };

        match self.inner.create(&ctx, parent, &cname, create_in) {
            Ok((entry, _handle, _, _)) => {
                tracing::debug!(target: "passthrough", inode = entry.inode, "create succeeded");
                // Get the file path and open it ourselves for simpler read/write
                let path = match self.get_inode_path(entry.inode) {
                    Some(p) => p,
                    None => {
                        tracing::error!(target: "passthrough", inode = entry.inode, "failed to get inode path");
                        return VolumeResponse::error(libc::EIO);
                    }
                };

                tracing::debug!(target: "passthrough", path = ?path, "opening file");
                let file = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(target: "passthrough", path = ?path, error = ?e, "open failed");
                        return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
                    }
                };

                let fh = self.handles.insert(file);
                let attr = Self::entry_to_attr(&entry);
                tracing::debug!(target: "passthrough", fh, "create returning file handle");

                VolumeResponse::Created {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                    fh,
                    flags: 0,
                }
            }
            Err(e) => {
                tracing::error!(target: "passthrough", error = ?e, "inner.create failed");
                VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn open(&self, ino: u64, flags: u32, uid: u32, gid: u32, _pid: u32) -> VolumeResponse {
        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        // Get the file path via fuse-backend-rs
        let path = match self.get_inode_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::error(libc::ENOENT),
        };

        // Open the file ourselves for simpler read/write
        let mut opts = OpenOptions::new();

        let access_mode = flags & libc::O_ACCMODE as u32;
        if access_mode == libc::O_RDONLY as u32 {
            opts.read(true);
        } else if access_mode == libc::O_WRONLY as u32 {
            opts.write(true);
        } else {
            opts.read(true).write(true);
        }

        if flags & libc::O_APPEND as u32 != 0 {
            opts.append(true);
        }

        let file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let fh = self.handles.insert(file);
        VolumeResponse::Opened { fh, flags: 0 }
    }

    fn read(&self, _ino: u64, fh: u64, offset: u64, size: u32) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                return Err(e.raw_os_error().unwrap_or(libc::EIO));
            }

            let mut buf = vec![0u8; size as usize];
            match file.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    Ok(buf)
                }
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        }) {
            Some(Ok(data)) => VolumeResponse::Data { data },
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn write(&self, _ino: u64, fh: u64, offset: u64, data: &[u8]) -> VolumeResponse {
        tracing::debug!(target: "passthrough", fh, offset, len = data.len(), "write");
        match self.handles.with_file(fh, |file| {
            if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                tracing::error!(target: "passthrough", fh, error = ?e, "seek failed");
                return Err(e.raw_os_error().unwrap_or(libc::EIO));
            }

            match file.write(data) {
                Ok(n) => {
                    tracing::debug!(target: "passthrough", fh, written = n, "write succeeded");
                    Ok(n as u32)
                }
                Err(e) => {
                    tracing::error!(target: "passthrough", fh, error = ?e, "write failed");
                    Err(e.raw_os_error().unwrap_or(libc::EIO))
                }
            }
        }) {
            Some(Ok(size)) => VolumeResponse::Written { size },
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => {
                tracing::error!(target: "passthrough", fh, "write: bad fd - handle not found");
                VolumeResponse::bad_fd()
            }
        }
    }

    fn release(&self, _ino: u64, fh: u64) -> VolumeResponse {
        self.handles.remove(fh);
        VolumeResponse::Ok
    }

    fn flush(&self, _ino: u64, fh: u64) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            file.sync_all()
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
        }) {
            Some(Ok(())) => VolumeResponse::Ok,
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn fsync(&self, _ino: u64, fh: u64, datasync: bool) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            let result = if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            };
            result.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
        }) {
            Some(Ok(())) => VolumeResponse::Ok,
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn unlink(&self, parent: u64, name: &str, uid: u32, gid: u32, pid: u32) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.unlink(&ctx, parent, &cname) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        let cnewname = match CString::new(newname) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.rename(&ctx, parent, &cname, newparent, &cnewname, 0) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn symlink(
        &self,
        parent: u64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        let ctarget = match CString::new(target) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.symlink(&ctx, &ctarget, parent, &cname) {
            Ok(entry) => {
                let attr = Self::entry_to_attr(&entry);
                VolumeResponse::Entry {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn readlink(&self, ino: u64) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.readlink(&ctx, ino) {
            Ok(target_bytes) => {
                let target = String::from_utf8_lossy(&target_bytes).to_string();
                VolumeResponse::Symlink { target }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn link(
        &self,
        ino: u64,
        newparent: u64,
        newname: &str,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        let cnewname = match CString::new(newname) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.link(&ctx, ino, newparent, &cnewname) {
            Ok(entry) => {
                let attr = Self::entry_to_attr(&entry);
                VolumeResponse::Entry {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn access(&self, ino: u64, mask: u32, uid: u32, gid: u32, pid: u32) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // Use caller's credentials
        let _guard = match CredentialsGuard::new(uid, gid) {
            Ok(g) => g,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM)),
        };

        match self.inner.access(&ctx, ino, mask) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EACCES)),
        }
    }

    fn statfs(&self, ino: u64) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.statfs(&ctx, ino) {
            Ok(st) => {
                VolumeResponse::Statfs {
                    blocks: st.f_blocks,
                    bfree: st.f_bfree,
                    bavail: st.f_bavail,
                    files: st.f_files,
                    ffree: st.f_ffree,
                    bsize: st.f_bsize as u32,
                    namelen: st.f_namemax as u32,
                    frsize: st.f_frsize as u32,
                }
            }
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_fs_creation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());
        assert_eq!(fs.root_path(), &dir.path().to_path_buf());
    }

    #[test]
    fn test_passthrough_getattr_root() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());

        let resp = fs.getattr(1); // Root inode
        match resp {
            VolumeResponse::Attr { attr, .. } => {
                assert_eq!(attr.ino, 1);
                assert!(attr.is_dir());
            }
            _ => panic!("Expected Attr response"),
        }
    }

    #[test]
    fn test_passthrough_lookup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let fs = PassthroughFs::new(dir.path());

        let uid = nix::unistd::Uid::effective().as_raw();
        let gid = nix::unistd::Gid::effective().as_raw();
        let resp = fs.lookup(1, "test.txt", uid, gid, 0);
        match resp {
            VolumeResponse::Entry { attr, .. } => {
                assert!(attr.is_file());
                assert_eq!(attr.size, 5);
            }
            _ => panic!("Expected Entry response"),
        }
    }

    #[test]
    fn test_passthrough_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());

        // Create file (use current user's uid/gid)
        let uid = nix::unistd::Uid::effective().as_raw();
        let gid = nix::unistd::Gid::effective().as_raw();
        let resp = fs.create(1, "test.txt", 0o644, 0, uid, gid, 0);
        let fh = match resp {
            VolumeResponse::Created { fh, .. } => fh,
            VolumeResponse::Error { errno } => panic!("Expected Created response, got error: {}", errno),
            _ => panic!("Expected Created response"),
        };

        // Write
        let resp = fs.write(0, fh, 0, b"hello");
        assert!(matches!(resp, VolumeResponse::Written { size: 5 }));

        // Read back
        let resp = fs.read(0, fh, 0, 100);
        match resp {
            VolumeResponse::Data { data } => {
                assert_eq!(data, b"hello");
            }
            _ => panic!("Expected Data response"),
        }
    }
}
