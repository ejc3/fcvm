//! Passthrough filesystem implementation using fuse-backend-rs.
//!
//! This wraps the production-grade passthrough filesystem from the
//! Cloud Hypervisor project. It provides full POSIX semantics with
//! proper permission enforcement.
//!
//! # Credential Handling
//!
//! fuse-backend-rs handles credentials internally via the `Context` parameter
//! passed to each operation. It uses the uid/gid from Context for permission
//! checks when performing filesystem operations.
//!
//! IMPORTANT: We must NOT use `setfsuid()`/`setfsgid()` before calling
//! fuse-backend-rs operations because fuse-backend-rs internally uses
//! `readlinkat` on `/proc/self/fd/` to resolve inode paths. When fsuid
//! is changed to a non-root user, these `/proc` operations fail because
//! `/proc/self/fd/` is owned by root (the process's effective UID).
//!
//! We only use `CredentialsGuard` for operations we handle directly
//! (not through fuse-backend-rs), such as:
//! - `setattr` with truncate: uses std::fs::File::set_len()
//! - `setattr` with utimensat: uses libc::utimensat() directly

use super::credentials::CredentialsGuard;
use super::handler::FilesystemHandler;
use super::zerocopy::{SliceReader, VecWriter};
use crate::protocol::{file_type, DirEntry, DirEntryPlus, FileAttr, VolumeResponse};

use fuse_backend_rs::abi::fuse_abi::CreateIn;
use fuse_backend_rs::api::filesystem::{Context, Entry, FileSystem};
use fuse_backend_rs::passthrough::{Config, PassthroughFs as FuseBackendPassthrough};

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Default attribute TTL in seconds.
const ATTR_TTL_SECS: u64 = 1;

/// A passthrough filesystem that maps operations to a local directory.
///
/// This implementation wraps fuse-backend-rs's production-grade PassthroughFs,
/// providing full POSIX semantics with proper permission enforcement, inode
/// tracking, and file handle management.
///
/// File handles are managed by fuse-backend-rs internally, providing:
/// - Inode validation on every access
/// - Thread-safe operations with per-handle locking
/// - Proper open flag tracking
pub struct PassthroughFs {
    inner: Arc<FuseBackendPassthrough>,
    root_path: PathBuf,
    attr_ttl_secs: u64,
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
    /// Only needed for setattr which still does direct path operations.
    fn get_inode_path(&self, ino: u64) -> Option<PathBuf> {
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
        atime_now: bool,
        mtime_secs: Option<i64>,
        mtime_nsecs: Option<u32>,
        mtime_now: bool,
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

        // utimensat with specific times: POSIX requires owner or root
        // UTIME_NOW permission check is done by kernel via write access
        if (atime_secs.is_some() || mtime_secs.is_some()) && caller_uid != 0 && caller_uid != file_uid {
            return VolumeResponse::error(libc::EPERM);
        }

        // Determine if we need to call utimensat
        let need_time_update = atime_now || mtime_now || atime_secs.is_some() || mtime_secs.is_some();

        // Get the file path to perform operations directly
        let path = match self.get_inode_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::error(libc::EIO),
        };

        // Set filesystem credentials for truncate and utimensat permission checks
        // Note: chown must run as root, chmod checks ownership above
        let _creds = if size.is_some() || need_time_update {
            match CredentialsGuard::new(caller_uid, caller_gid) {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::error!(target: "passthrough", error = ?e, "failed to switch credentials");
                    return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EPERM));
                }
            }
        } else {
            None
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

        // Handle mode change (owner check done above, so this runs as root to bypass fs perms)
        if let Some(new_mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(new_mode);
            if let Err(e) = std::fs::set_permissions(&path, permissions) {
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }

        // Handle uid/gid change using libc::chown (must run as root)
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
        // UTIME_NOW and UTIME_OMIT are special values for tv_nsec in utimensat:
        // - UTIME_NOW (0x3fffffff): Set to current time, requires write access or ownership
        // - UTIME_OMIT (0x3ffffffe): Don't change this timestamp
        if need_time_update {
            let path_cstr = CString::new(path.to_string_lossy().as_bytes()).unwrap();

            // Build atime timespec
            let atime_spec = if atime_now {
                // UTIME_NOW: set to current time (kernel checks write permission)
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_NOW,
                }
            } else if let Some(secs) = atime_secs {
                // Specific time value
                libc::timespec {
                    tv_sec: secs,
                    tv_nsec: atime_nsecs.map(|n| n as i64).unwrap_or(0),
                }
            } else {
                // UTIME_OMIT: don't change
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                }
            };

            // Build mtime timespec
            let mtime_spec = if mtime_now {
                // UTIME_NOW: set to current time (kernel checks write permission)
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_NOW,
                }
            } else if let Some(secs) = mtime_secs {
                // Specific time value
                libc::timespec {
                    tv_sec: secs,
                    tv_nsec: mtime_nsecs.map(|n| n as i64).unwrap_or(0),
                }
            } else {
                // UTIME_OMIT: don't change
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                }
            };

            // Check if this is a symlink - if so, use AT_SYMLINK_NOFOLLOW
            // to set times on the symlink itself, not its target
            let is_symlink = (current_attr.st_mode & libc::S_IFMT) == libc::S_IFLNK;
            let flags = if is_symlink { libc::AT_SYMLINK_NOFOLLOW } else { 0 };

            let times = [atime_spec, mtime_spec];
            let result = unsafe { libc::utimensat(libc::AT_FDCWD, path_cstr.as_ptr(), times.as_ptr(), flags) };
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
        tracing::debug!(target: "passthrough", ino, offset, uid, gid, "readdir");
        let ctx = Self::make_context(uid, gid, pid);

        tracing::debug!(target: "passthrough", ino, "readdir opening directory");
        // First open the directory
        let (handle, _) = match self.inner.opendir(&ctx, ino, libc::O_RDONLY as u32) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(target: "passthrough", error = ?e, "readdir opendir failed");
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        };
        tracing::debug!(target: "passthrough", ino, handle = ?handle, "readdir directory opened");

        let mut entries = Vec::new();

        // Read ALL directory entries using fuse-backend-rs's readdir
        // We always read from offset 0 and filter ourselves because we open a fresh
        // directory handle each time (fuse-backend-rs offset is per-handle state)
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

        tracing::debug!(target: "passthrough", ino, "readdir reading entries");
        // Always read from offset 0 - we handle offset filtering ourselves
        if let Err(e) = self.inner.readdir(&ctx, ino, handle.unwrap_or(0), 8192, 0, &mut add_entry) {
            tracing::error!(target: "passthrough", error = ?e, "readdir read failed");
            let _ = self.inner.releasedir(&ctx, ino, 0, handle.unwrap_or(0));
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }
        tracing::debug!(target: "passthrough", ino, count = entries.len(), "readdir got raw entries");

        // Release the directory handle
        let _ = self.inner.releasedir(&ctx, ino, 0, handle.unwrap_or(0));

        // Build full entry list with . and .. at the beginning
        let mut full_entries = Vec::new();

        // Get parent inode - for root, parent is self
        let parent_ino = if ino == 1 {
            1
        } else {
            tracing::debug!(target: "passthrough", ino, "readdir looking up parent");
            let dotdot = CString::new("..").unwrap();
            match self.inner.lookup(&ctx, ino, &dotdot) {
                Ok(entry) => entry.inode,
                Err(_) => ino, // Fallback to self if can't find parent
            }
        };

        full_entries.push(DirEntry::dot(ino));
        full_entries.push(DirEntry::dotdot(parent_ino));
        full_entries.extend(entries);

        // Now apply offset: skip entries before offset and return the rest
        // This implements the FUSE readdir contract where offset is the index
        // of the first entry to return
        let offset_usize = offset as usize;
        let result_entries = if offset_usize >= full_entries.len() {
            // Offset is past all entries - return empty to signal end of directory
            Vec::new()
        } else {
            full_entries.into_iter().skip(offset_usize).collect()
        };

        tracing::debug!(target: "passthrough", ino, offset, total = result_entries.len(), "readdir succeeded");
        VolumeResponse::DirEntries { entries: result_entries }
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
        tracing::debug!(target: "passthrough", parent, name, mode, uid, gid, "mkdir");
        let ctx = Self::make_context(uid, gid, pid);

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        // fuse-backend-rs handles credentials via Context internally.
        // Do NOT use CredentialsGuard here - it breaks fuse-backend-rs's
        // internal readlinkat on /proc/self/fd/.

        match self.inner.mkdir(&ctx, parent, &cname, mode, 0) {
            Ok(entry) => {
                tracing::debug!(target: "passthrough", inode = entry.inode, "mkdir succeeded");
                let attr = Self::entry_to_attr(&entry);
                VolumeResponse::Entry {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                }
            }
            Err(e) => {
                tracing::error!(target: "passthrough", error = ?e, "mkdir failed");
                VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO))
            }
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

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        // fuse-backend-rs handles credentials via Context internally.

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

        // fuse-backend-rs handles credentials via Context internally.

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

        // fuse-backend-rs handles credentials via Context internally.

        match self.inner.create(&ctx, parent, &cname, create_in) {
            Ok((entry, handle, _opts, _open_opts)) => {
                tracing::debug!(target: "passthrough", inode = entry.inode, handle = ?handle, "create succeeded");
                let attr = Self::entry_to_attr(&entry);

                VolumeResponse::Created {
                    attr,
                    generation: entry.generation,
                    ttl_secs: self.attr_ttl_secs,
                    fh: handle.unwrap_or(0),
                    flags: 0,
                }
            }
            Err(e) => {
                tracing::error!(target: "passthrough", error = ?e, "inner.create failed");
                VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn open(&self, ino: u64, flags: u32, uid: u32, gid: u32, pid: u32) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // fuse-backend-rs handles credentials via Context internally.

        match self.inner.open(&ctx, ino, flags, 0) {
            Ok((handle, _opts, _passthrough)) => VolumeResponse::Opened {
                fh: handle.unwrap_or(0),
                flags: 0,
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn read(&self, ino: u64, fh: u64, offset: u64, size: u32) -> VolumeResponse {
        let ctx = Context::new();
        let mut writer = VecWriter::new(size as usize);

        match self.inner.read(&ctx, ino, fh, &mut writer, size, offset, None, 0) {
            Ok(_) => VolumeResponse::Data {
                data: writer.into_vec(),
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn write(&self, ino: u64, fh: u64, offset: u64, data: &[u8]) -> VolumeResponse {
        let ctx = Context::new();
        let mut reader = SliceReader::new(data);

        tracing::debug!(target: "passthrough", ino, fh, offset, len = data.len(), "write");

        match self.inner.write(
            &ctx,
            ino,
            fh,
            &mut reader,
            data.len() as u32,
            offset,
            None,  // lock_owner
            false, // delayed_write
            0,     // flags
            0,     // fuse_flags
        ) {
            Ok(n) => {
                tracing::debug!(target: "passthrough", fh, written = n, "write succeeded");
                VolumeResponse::Written { size: n as u32 }
            }
            Err(e) => {
                tracing::error!(target: "passthrough", fh, error = ?e, "write failed");
                VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn release(&self, ino: u64, fh: u64) -> VolumeResponse {
        tracing::debug!(target: "passthrough", ino, fh, "release");
        let ctx = Context::new();

        match self.inner.release(&ctx, ino, 0, fh, false, true, None) {
            Ok(()) => {
                tracing::debug!(target: "passthrough", ino, fh, "release succeeded");
                VolumeResponse::Ok
            }
            Err(e) => {
                tracing::error!(target: "passthrough", ino, fh, error = ?e, "release failed");
                VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn flush(&self, ino: u64, fh: u64) -> VolumeResponse {
        tracing::debug!(target: "passthrough", ino, fh, "flush");
        let ctx = Context::new();

        match self.inner.flush(&ctx, ino, fh, 0) {
            Ok(()) => {
                tracing::debug!(target: "passthrough", ino, fh, "flush succeeded");
                VolumeResponse::Ok
            }
            Err(e) => {
                tracing::error!(target: "passthrough", ino, fh, error = ?e, "flush failed");
                VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn fsync(&self, ino: u64, fh: u64, datasync: bool) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.fsync(&ctx, ino, datasync, fh) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn unlink(&self, parent: u64, name: &str, uid: u32, gid: u32, pid: u32) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // fuse-backend-rs handles credentials via Context internally.

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

        // fuse-backend-rs handles credentials via Context internally.

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

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        let ctarget = match CString::new(target) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        // fuse-backend-rs handles credentials via Context internally.

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

        // fuse-backend-rs handles credentials via Context internally.

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

    fn opendir(&self, ino: u64, flags: u32, uid: u32, gid: u32, pid: u32) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // fuse-backend-rs handles credentials via Context internally.

        match self.inner.opendir(&ctx, ino, flags) {
            Ok((handle, _opts)) => VolumeResponse::Openeddir {
                fh: handle.unwrap_or(0),
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn releasedir(&self, ino: u64, fh: u64) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.releasedir(&ctx, ino, 0, fh) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn fsyncdir(&self, ino: u64, fh: u64, datasync: bool) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.fsyncdir(&ctx, ino, datasync, fh) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn setxattr(
        &self,
        ino: u64,
        name: &str,
        value: &[u8],
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // fuse-backend-rs handles credentials via Context internally.

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.setxattr(&ctx, ino, &cname, value, flags) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn getxattr(
        &self,
        ino: u64,
        name: &str,
        size: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        use fuse_backend_rs::api::filesystem::GetxattrReply;

        let ctx = Self::make_context(uid, gid, pid);

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.getxattr(&ctx, ino, &cname, size) {
            Ok(reply) => match reply {
                GetxattrReply::Value(data) => VolumeResponse::Xattr { data },
                GetxattrReply::Count(count) => VolumeResponse::XattrSize { size: count },
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn listxattr(&self, ino: u64, size: u32, uid: u32, gid: u32, pid: u32) -> VolumeResponse {
        use fuse_backend_rs::api::filesystem::ListxattrReply;

        let ctx = Self::make_context(uid, gid, pid);

        match self.inner.listxattr(&ctx, ino, size) {
            Ok(reply) => match reply {
                ListxattrReply::Names(data) => VolumeResponse::Xattr { data },
                ListxattrReply::Count(count) => VolumeResponse::XattrSize { size: count },
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn removexattr(
        &self,
        ino: u64,
        name: &str,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        let ctx = Self::make_context(uid, gid, pid);

        // fuse-backend-rs handles credentials via Context internally.

        let cname = match CString::new(name) {
            Ok(c) => c,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        match self.inner.removexattr(&ctx, ino, &cname) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn fallocate(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> VolumeResponse {
        let ctx = Context::new();

        match self.inner.fallocate(&ctx, ino, fh, mode, offset, length) {
            Ok(()) => VolumeResponse::Ok,
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn lseek(&self, ino: u64, fh: u64, offset: i64, whence: u32) -> VolumeResponse {
        let ctx = Context::new();

        // fuse-backend-rs expects u64 for offset
        let offset_u64 = if offset >= 0 {
            offset as u64
        } else {
            return VolumeResponse::error(libc::EINVAL);
        };

        match self.inner.lseek(&ctx, ino, fh, offset_u64, whence) {
            Ok(new_offset) => VolumeResponse::Lseek {
                offset: new_offset as u64,
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn getlk(
        &self,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        start: u64,
        end: u64,
        _typ: i32,
        pid: u32,
    ) -> VolumeResponse {
        // fuse-backend-rs doesn't expose file locking, and we can't access the raw fd.
        // Return "no conflicting lock" which is the most permissive behavior.
        // This means we report that the requested lock would succeed.
        VolumeResponse::Lock {
            start,
            end,
            typ: libc::F_UNLCK,  // No conflicting lock
            pid,
        }
    }

    fn setlk(
        &self,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        _sleep: bool,
    ) -> VolumeResponse {
        // fuse-backend-rs doesn't expose file locking, and we can't access the raw fd.
        // Return success - this is permissive but prevents applications from failing.
        VolumeResponse::Ok
    }

    fn readdirplus(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        uid: u32,
        gid: u32,
        pid: u32,
    ) -> VolumeResponse {
        tracing::debug!(target: "passthrough", ino, fh, offset, uid, gid, "readdirplus");
        let ctx = Self::make_context(uid, gid, pid);

        let mut entries = Vec::new();

        // Delegate to fuse-backend-rs readdirplus which handles everything
        // including . and .. entries and does lookups for full attributes
        let mut add_entry = |dir_entry: fuse_backend_rs::api::filesystem::DirEntry,
                            entry: Entry|
         -> std::io::Result<usize> {
            let name_str = String::from_utf8_lossy(dir_entry.name).to_string();
            let attr = Self::entry_to_attr(&entry);
            entries.push(DirEntryPlus {
                ino: entry.inode,
                name: name_str,
                attr,
                generation: entry.generation,
                attr_ttl_secs: self.attr_ttl_secs,
                entry_ttl_secs: self.attr_ttl_secs,
            });
            Ok(1)
        };

        if let Err(e) = self.inner.readdirplus(&ctx, ino, fh, 8192, offset, &mut add_entry) {
            tracing::error!(target: "passthrough", error = ?e, "readdirplus failed");
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        tracing::debug!(target: "passthrough", ino, offset, count = entries.len(), "readdirplus succeeded");
        VolumeResponse::DirEntriesPlus { entries }
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
        // Note: flags must include O_RDWR for read/write access
        let uid = nix::unistd::Uid::effective().as_raw();
        let gid = nix::unistd::Gid::effective().as_raw();
        let resp = fs.create(1, "test.txt", 0o644, libc::O_RDWR as u32, uid, gid, 0);
        let (ino, fh) = match resp {
            VolumeResponse::Created { attr, fh, .. } => (attr.ino, fh),
            VolumeResponse::Error { errno } => panic!("Expected Created response, got error: {}", errno),
            _ => panic!("Expected Created response"),
        };

        // Write (now using correct inode)
        let resp = fs.write(ino, fh, 0, b"hello");
        match &resp {
            VolumeResponse::Written { size } => assert_eq!(*size, 5),
            VolumeResponse::Error { errno } => panic!("Write failed with errno: {}", errno),
            other => panic!("Unexpected response: {:?}", other),
        }

        // Read back
        let resp = fs.read(ino, fh, 0, 100);
        match resp {
            VolumeResponse::Data { data } => {
                assert_eq!(data, b"hello");
            }
            _ => panic!("Expected Data response"),
        }

        // Release the file handle
        let resp = fs.release(ino, fh);
        assert!(resp.is_ok());
    }

    #[test]
    fn test_passthrough_open_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let test_file = dir.path().join("existing.txt");
        std::fs::write(&test_file, "initial content").unwrap();

        let fs = PassthroughFs::new(dir.path());

        let uid = nix::unistd::Uid::effective().as_raw();
        let gid = nix::unistd::Gid::effective().as_raw();

        // Lookup the file first to get its inode
        let resp = fs.lookup(1, "existing.txt", uid, gid, 0);
        let ino = match resp {
            VolumeResponse::Entry { attr, .. } => attr.ino,
            _ => panic!("Expected Entry response"),
        };

        // Open the file
        let resp = fs.open(ino, libc::O_RDWR as u32, uid, gid, 0);
        let fh = match resp {
            VolumeResponse::Opened { fh, .. } => fh,
            VolumeResponse::Error { errno } => panic!("Expected Opened response, got error: {}", errno),
            _ => panic!("Expected Opened response"),
        };

        // Read initial content
        let resp = fs.read(ino, fh, 0, 100);
        match resp {
            VolumeResponse::Data { data } => {
                assert_eq!(data, b"initial content");
            }
            _ => panic!("Expected Data response"),
        }

        // Write new content at offset 8
        let resp = fs.write(ino, fh, 8, b"REPLACED");
        assert!(matches!(resp, VolumeResponse::Written { size: 8 }));

        // Read back to verify
        let resp = fs.read(ino, fh, 0, 100);
        match resp {
            VolumeResponse::Data { data } => {
                assert_eq!(data, b"initial REPLACED");
            }
            _ => panic!("Expected Data response"),
        }

        // Flush
        let resp = fs.flush(ino, fh);
        assert!(resp.is_ok());

        // Fsync
        let resp = fs.fsync(ino, fh, false);
        assert!(resp.is_ok());

        // Release
        let resp = fs.release(ino, fh);
        assert!(resp.is_ok());
    }

    #[test]
    fn test_passthrough_hardlink() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());

        let uid = nix::unistd::Uid::effective().as_raw();
        let gid = nix::unistd::Gid::effective().as_raw();

        // Create source file
        let resp = fs.create(1, "source.txt", 0o644, libc::O_RDWR as u32, uid, gid, 0);
        let (source_ino, fh) = match resp {
            VolumeResponse::Created { attr, fh, .. } => (attr.ino, fh),
            VolumeResponse::Error { errno } => panic!("Create failed with errno: {}", errno),
            _ => panic!("Expected Created response"),
        };

        // Write to source
        let resp = fs.write(source_ino, fh, 0, b"hardlink test content");
        assert!(matches!(resp, VolumeResponse::Written { .. }));
        fs.release(source_ino, fh);

        // Create hardlink
        let resp = fs.link(source_ino, 1, "link.txt", uid, gid, 0);
        let link_ino = match resp {
            VolumeResponse::Entry { attr, .. } => {
                // Hardlinks share the same inode
                assert_eq!(attr.ino, source_ino);
                attr.ino
            }
            VolumeResponse::Error { errno } => panic!("Link failed with errno: {}", errno),
            _ => panic!("Expected Entry response"),
        };

        // Delete source file
        let resp = fs.unlink(1, "source.txt", uid, gid, 0);
        assert!(resp.is_ok(), "Unlink source failed");

        // Verify source is gone
        let resp = fs.lookup(1, "source.txt", uid, gid, 0);
        assert!(matches!(resp, VolumeResponse::Error { errno } if errno == libc::ENOENT));

        // Hardlink should still be accessible and readable
        let resp = fs.lookup(1, "link.txt", uid, gid, 0);
        let lookup_ino = match resp {
            VolumeResponse::Entry { attr, .. } => attr.ino,
            VolumeResponse::Error { errno } => panic!("Lookup link.txt failed with errno: {}", errno),
            _ => panic!("Expected Entry response"),
        };
        assert_eq!(lookup_ino, link_ino);

        // Open and read from hardlink
        let resp = fs.open(link_ino, libc::O_RDONLY as u32, uid, gid, 0);
        let fh = match resp {
            VolumeResponse::Opened { fh, .. } => fh,
            VolumeResponse::Error { errno } => panic!("Open link.txt failed with errno: {}", errno),
            _ => panic!("Expected Opened response"),
        };

        let resp = fs.read(link_ino, fh, 0, 100);
        match resp {
            VolumeResponse::Data { data } => {
                assert_eq!(data, b"hardlink test content");
            }
            VolumeResponse::Error { errno } => panic!("Read failed with errno: {}", errno),
            _ => panic!("Expected Data response"),
        }

        fs.release(link_ino, fh);
    }
}
