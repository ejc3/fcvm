//! fuser::Filesystem implementation for remote FUSE.

use super::multiplexer::Multiplexer;
use crate::protocol::{file_type, FileAttr, VolumeRequest, VolumeResponse};
use fuser::{
    FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyLock, ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
};
use std::ffi::OsStr;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

/// Parse supplementary groups from /proc/<pid>/status.
///
/// FUSE protocol only passes uid and primary gid. For proper permission checks
/// (like chown to a supplementary group), we need to read the caller's groups.
fn get_supplementary_groups(pid: u32) -> Vec<u32> {
    if pid == 0 {
        return Vec::new();
    }

    let status_path = format!("/proc/{}/status", pid);
    let content = match fs::read_to_string(&status_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // Look for line: "Groups:\t1000 1001 1002"
    for line in content.lines() {
        if let Some(groups_str) = line.strip_prefix("Groups:") {
            return groups_str
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
        }
    }

    Vec::new()
}

/// Callback to spawn additional readers after INIT completes.
pub type InitCallback = Box<dyn FnOnce() + Send>;

/// FUSE client that uses a shared multiplexer.
pub struct FuseClient {
    mux: Arc<Multiplexer>,
    reader_id: u32,
    /// Optional callback to run when init() completes (spawns additional readers)
    init_callback: Option<InitCallback>,
    /// Shared flag set by destroy() to signal clean shutdown to reader threads
    destroyed: Arc<AtomicBool>,
}

impl FuseClient {
    /// Create a new client for a specific reader using shared multiplexer.
    pub fn new(mux: Arc<Multiplexer>, reader_id: u32) -> Self {
        Self::with_destroyed_flag(mux, reader_id, Arc::new(AtomicBool::new(false)))
    }

    /// Create a new client with a shared destroyed flag.
    ///
    /// The destroyed flag is set by `destroy()` when the filesystem is unmounted.
    /// Reader threads can check this flag to distinguish clean shutdown from errors.
    pub fn with_destroyed_flag(
        mux: Arc<Multiplexer>,
        reader_id: u32,
        destroyed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            mux,
            reader_id,
            init_callback: None,
            destroyed,
        }
    }

    /// Create a new client with a callback to run after INIT completes.
    ///
    /// The callback should spawn additional reader threads. This is only
    /// used for the primary reader (reader 0).
    pub fn with_init_callback(
        mux: Arc<Multiplexer>,
        reader_id: u32,
        callback: InitCallback,
        destroyed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            mux,
            reader_id,
            init_callback: Some(callback),
            destroyed,
        }
    }

    /// Get a clone of the destroyed flag for sharing with reader threads.
    pub fn destroyed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.destroyed)
    }

    /// Send request and wait for response.
    fn send_request_sync(&self, request: VolumeRequest) -> VolumeResponse {
        let pid = Self::request_pid(&request);
        if let Some(pid) = pid {
            let groups = get_supplementary_groups(pid);
            self.mux
                .send_request_with_groups(self.reader_id, request, groups)
        } else {
            self.mux.send_request(self.reader_id, request)
        }
    }

    /// Send request with supplementary groups and wait for response.
    ///
    /// Reads the caller's supplementary groups from /proc/<pid>/status
    /// and forwards them to the server for proper permission checks.
    fn send_request_with_groups(&self, request: VolumeRequest, pid: u32) -> VolumeResponse {
        let groups = get_supplementary_groups(pid);
        self.mux
            .send_request_with_groups(self.reader_id, request, groups)
    }

    /// Extract the caller PID from a request (when available).
    fn request_pid(request: &VolumeRequest) -> Option<u32> {
        match request {
            VolumeRequest::Lookup { pid, .. }
            | VolumeRequest::Readdir { pid, .. }
            | VolumeRequest::Mkdir { pid, .. }
            | VolumeRequest::Mknod { pid, .. }
            | VolumeRequest::Rmdir { pid, .. }
            | VolumeRequest::Create { pid, .. }
            | VolumeRequest::Open { pid, .. }
            | VolumeRequest::Read { pid, .. }
            | VolumeRequest::Write { pid, .. }
            | VolumeRequest::Unlink { pid, .. }
            | VolumeRequest::Rename { pid, .. }
            | VolumeRequest::Symlink { pid, .. }
            | VolumeRequest::Link { pid, .. }
            | VolumeRequest::Access { pid, .. }
            | VolumeRequest::Opendir { pid, .. }
            | VolumeRequest::Setxattr { pid, .. }
            | VolumeRequest::Getxattr { pid, .. }
            | VolumeRequest::Listxattr { pid, .. }
            | VolumeRequest::Removexattr { pid, .. }
            | VolumeRequest::Readdirplus { pid, .. }
            | VolumeRequest::Getlk { pid, .. }
            | VolumeRequest::Setlk { pid, .. } => Some(*pid),
            VolumeRequest::Setattr { caller_pid, .. } => Some(*caller_pid),
            _ => None,
        }
    }
}

/// Convert i64 seconds + u32 nanoseconds to SystemTime.
/// Handles negative timestamps (times before Unix epoch).
fn to_system_time(secs: i64, nsecs: u32) -> std::time::SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs)
    } else {
        // For negative timestamps, subtract from epoch
        // Note: nsecs is always positive, so we need to handle it correctly
        UNIX_EPOCH - Duration::new((-secs) as u64, 0) + Duration::new(0, nsecs)
    }
}

/// Convert FileAttr to fuser::FileAttr.
fn to_fuser_attr(attr: &FileAttr) -> fuser::FileAttr {
    let kind = match attr.mode & libc::S_IFMT {
        x if x == libc::S_IFDIR => FileType::Directory,
        x if x == libc::S_IFREG => FileType::RegularFile,
        x if x == libc::S_IFLNK => FileType::Symlink,
        x if x == libc::S_IFCHR => FileType::CharDevice,
        x if x == libc::S_IFBLK => FileType::BlockDevice,
        x if x == libc::S_IFIFO => FileType::NamedPipe,
        x if x == libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    };

    fuser::FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.blocks,
        atime: to_system_time(attr.atime_secs, attr.atime_nsecs),
        mtime: to_system_time(attr.mtime_secs, attr.mtime_nsecs),
        ctime: to_system_time(attr.ctime_secs, attr.ctime_nsecs),
        crtime: UNIX_EPOCH,
        kind,
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev,
        blksize: attr.blksize,
        flags: 0,
    }
}

/// Convert protocol file type to fuser FileType.
fn protocol_file_type_to_fuser(ft: u8) -> FileType {
    match ft {
        file_type::DIR => FileType::Directory,
        file_type::REG => FileType::RegularFile,
        file_type::LNK => FileType::Symlink,
        file_type::CHR => FileType::CharDevice,
        file_type::BLK => FileType::BlockDevice,
        file_type::FIFO => FileType::NamedPipe,
        file_type::SOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// Default max_write size for FUSE operations (0 = unbounded, use kernel default).
///
/// For nested virtualization (L2 VMs), set FCVM_FUSE_MAX_WRITE=32768 to avoid
/// vsock data loss due to cache coherency issues in double Stage 2 translation.
const DEFAULT_FUSE_MAX_WRITE: u32 = 0;

impl Filesystem for FuseClient {
    fn init(
        &mut self,
        _req: &Request<'_>,
        config: &mut fuser::KernelConfig,
    ) -> Result<(), libc::c_int> {
        // Enable writeback cache for better write performance (kernel batches writes).
        // Can be disabled via FCVM_NO_WRITEBACK_CACHE=1 for debugging.
        let enable_writeback = std::env::var("FCVM_NO_WRITEBACK_CACHE").is_err();
        if enable_writeback {
            if let Err(unsupported) = config.add_capabilities(fuser::consts::FUSE_WRITEBACK_CACHE) {
                tracing::warn!(
                    target: "fuse-pipe::client",
                    unsupported,
                    "Kernel doesn't support FUSE_WRITEBACK_CACHE"
                );
            } else {
                tracing::debug!(
                    target: "fuse-pipe::client",
                    "Enabled FUSE_WRITEBACK_CACHE for better write performance"
                );
            }
        } else {
            tracing::debug!(
                target: "fuse-pipe::client",
                "FUSE_WRITEBACK_CACHE disabled via FCVM_NO_WRITEBACK_CACHE"
            );
        }

        // Enable auto-invalidation: kernel checks mtime and invalidates cached pages
        // when file is modified. Essential for FICLONE/reflink where content changes
        // without going through normal write path.
        if let Err(unsupported) = config.add_capabilities(fuser::consts::FUSE_AUTO_INVAL_DATA) {
            tracing::warn!(
                target: "fuse-pipe::client",
                unsupported,
                "Kernel doesn't support FUSE_AUTO_INVAL_DATA"
            );
        } else {
            tracing::debug!(
                target: "fuse-pipe::client",
                "Enabled FUSE_AUTO_INVAL_DATA for cache coherency"
            );
        }

        // Limit max_write to avoid vsock data loss under nested virtualization.
        // Override with FCVM_FUSE_MAX_WRITE env var (0 = unbounded).
        let max_write = std::env::var("FCVM_FUSE_MAX_WRITE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(DEFAULT_FUSE_MAX_WRITE);

        if max_write > 0 {
            if let Err(max) = config.set_max_write(max_write) {
                tracing::warn!(
                    target: "fuse-pipe::client",
                    requested = max_write,
                    max,
                    "Failed to set max_write, using kernel max"
                );
            } else {
                tracing::debug!(
                    target: "fuse-pipe::client",
                    max_write,
                    "Set FUSE max_write"
                );
            }
        }

        // Spawn additional readers now that INIT is done
        if let Some(callback) = self.init_callback.take() {
            callback();
        }
        Ok(())
    }

    fn destroy(&mut self) {
        // Signal to reader threads that this is a clean shutdown.
        // The kernel calls destroy() before closing cloned fds, so reader
        // threads will see this flag when they get ECONNABORTED.
        let was = self.destroyed.swap(true, Ordering::SeqCst);
        tracing::debug!(target: "fuse-pipe::client", reader_id = self.reader_id, was_already_set = was, "destroy() called - signaling clean shutdown");
    }

    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Lookup {
            parent,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry {
                attr,
                generation,
                ttl_secs,
            } => {
                reply.entry(
                    &Duration::from_secs(ttl_secs),
                    &to_fuser_attr(&attr),
                    generation,
                );
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let response = self.send_request_sync(VolumeRequest::Getattr { ino });

        match response {
            VolumeResponse::Attr { attr, ttl_secs } => {
                reply.attr(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr));
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn setattr(
        &mut self,
        req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Handle atime: UTIME_NOW, specific time, or UTIME_OMIT
        let (atime_secs, atime_nsecs, atime_now) = match atime {
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                (Some(d.as_secs() as i64), Some(d.subsec_nanos()), false)
            }
            Some(TimeOrNow::Now) => (None, None, true),
            None => (None, None, false),
        };

        // Handle mtime: UTIME_NOW, specific time, or UTIME_OMIT
        let (mtime_secs, mtime_nsecs, mtime_now) = match mtime {
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                (Some(d.as_secs() as i64), Some(d.subsec_nanos()), false)
            }
            Some(TimeOrNow::Now) => (None, None, true),
            None => (None, None, false),
        };

        // Use send_request_with_groups for setattr (which handles chown).
        // Supplementary groups are needed for proper permission checks when
        // a non-root user chowns to one of their supplementary groups.
        let response = self.send_request_with_groups(
            VolumeRequest::Setattr {
                ino,
                mode,
                uid,
                gid,
                size,
                atime_secs,
                atime_nsecs,
                atime_now,
                mtime_secs,
                mtime_nsecs,
                mtime_now,
                fh,
                caller_uid: req.uid(),
                caller_gid: req.gid(),
                caller_pid: req.pid(),
            },
            req.pid(),
        );

        match response {
            VolumeResponse::Attr { attr, ttl_secs } => {
                reply.attr(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr));
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!(target: "fuse-pipe::client", parent, ?name, mode, uid = req.uid(), gid = req.gid(), pid = req.pid(), "mkdir request");
        let response = self.send_request_sync(VolumeRequest::Mkdir {
            parent,
            name: name.to_string_lossy().to_string(),
            mode,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry {
                attr,
                generation,
                ttl_secs,
            } => {
                tracing::debug!(target: "fuse-pipe::client", ino = attr.ino, "mkdir succeeded");
                reply.entry(
                    &Duration::from_secs(ttl_secs),
                    &to_fuser_attr(&attr),
                    generation,
                );
            }
            VolumeResponse::Error { errno } => {
                tracing::debug!(target: "fuse-pipe::client", errno, "mkdir error");
                reply.error(errno);
            }
            _ => {
                tracing::debug!(target: "fuse-pipe::client", "mkdir unexpected response");
                reply.error(libc::EIO);
            }
        }
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Mknod {
            parent,
            name: name.to_string_lossy().to_string(),
            mode,
            rdev,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry {
                attr,
                generation,
                ttl_secs,
            } => {
                reply.entry(
                    &Duration::from_secs(ttl_secs),
                    &to_fuser_attr(&attr),
                    generation,
                );
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Rmdir {
            parent,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let response = self.send_request_sync(VolumeRequest::Create {
            parent,
            name: name.to_string_lossy().to_string(),
            mode,
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Created {
                attr,
                generation,
                ttl_secs,
                fh,
                flags,
            } => {
                reply.created(
                    &Duration::from_secs(ttl_secs),
                    &to_fuser_attr(&attr),
                    generation,
                    fh,
                    flags,
                );
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn open(&mut self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let response = self.send_request_sync(VolumeRequest::Open {
            ino,
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Opened { fh, flags } => reply.opened(fh, flags),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn read(
        &mut self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let response = self.send_request_sync(VolumeRequest::Read {
            ino,
            fh,
            offset: offset as u64,
            size,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Data { data } => reply.data(&data),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn write(
        &mut self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let response = self.send_request_sync(VolumeRequest::Write {
            ino,
            fh,
            offset: offset as u64,
            data: data.to_vec(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Written { size } => reply.written(size),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Release { ino, fh });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn flush(&mut self, _req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Flush { ino, fh });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Fsync { ino, fh, datasync });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Unlink {
            parent,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn rename(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Rename {
            parent,
            name: name.to_string_lossy().to_string(),
            newparent,
            newname: newname.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Symlink {
            parent,
            name: link_name.to_string_lossy().to_string(),
            target: target.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry {
                attr,
                generation,
                ttl_secs,
            } => {
                reply.entry(
                    &Duration::from_secs(ttl_secs),
                    &to_fuser_attr(&attr),
                    generation,
                );
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let response = self.send_request_sync(VolumeRequest::Readlink { ino });

        match response {
            VolumeResponse::Symlink { target } => reply.data(target.as_bytes()),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn link(
        &mut self,
        req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Link {
            ino,
            newparent,
            newname: newname.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Entry {
                attr,
                generation,
                ttl_secs,
            } => {
                reply.entry(
                    &Duration::from_secs(ttl_secs),
                    &to_fuser_attr(&attr),
                    generation,
                );
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn access(&mut self, req: &Request, ino: u64, mask: i32, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Access {
            ino,
            mask: mask as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn statfs(&mut self, _req: &Request, ino: u64, reply: ReplyStatfs) {
        let response = self.send_request_sync(VolumeRequest::Statfs { ino });

        match response {
            VolumeResponse::Statfs {
                blocks,
                bfree,
                bavail,
                files,
                ffree,
                bsize,
                namelen,
                frsize,
            } => {
                reply.statfs(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize);
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn readdir(
        &mut self,
        req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let response = self.send_request_sync(VolumeRequest::Readdir {
            ino,
            offset: offset as u64,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::DirEntries { entries } => {
                for (i, entry) in entries.iter().enumerate() {
                    let offset = (offset as usize + i + 1) as i64;
                    let ft = protocol_file_type_to_fuser(entry.file_type);
                    if reply.add(entry.ino, offset, ft, &entry.name) {
                        break;
                    }
                }
                reply.ok();
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn readdirplus(
        &mut self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let response = self.send_request_sync(VolumeRequest::Readdirplus {
            ino,
            fh,
            offset: offset as u64,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::DirEntriesPlus { entries } => {
                for (i, entry) in entries.iter().enumerate() {
                    let offset = (offset as usize + i + 1) as i64;
                    let attr = to_fuser_attr(&entry.attr);
                    let ttl = Duration::from_secs(entry.attr_ttl_secs);
                    if reply.add(
                        entry.ino,
                        offset,
                        &entry.name,
                        &ttl,
                        &attr,
                        entry.generation,
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn opendir(&mut self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let response = self.send_request_sync(VolumeRequest::Opendir {
            ino,
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Openeddir { fh } => reply.opened(fh, 0),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn releasedir(&mut self, _req: &Request, ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Releasedir { ino, fh });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn fsyncdir(&mut self, _req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Fsyncdir { ino, fh, datasync });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn setxattr(
        &mut self,
        req: &Request,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Setxattr {
            ino,
            name: name.to_string_lossy().to_string(),
            value: value.to_vec(),
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn getxattr(&mut self, req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        // Fast path: The kernel calls getxattr("security.capability") on every write
        // to check if file capabilities need to be cleared. This is extremely common
        // and almost always returns ENODATA (no capabilities set). Short-circuit this
        // to avoid the expensive server round-trip (~32µs savings per write).
        //
        // This is safe because:
        // 1. If capabilities ARE set, they're preserved (we'd need setxattr to clear)
        // 2. The kernel's capability check is advisory - it clears caps on successful write
        // 3. Container workloads rarely use file capabilities
        //
        // Can be disabled via FCVM_NO_XATTR_FASTPATH=1 for debugging.
        if std::env::var("FCVM_NO_XATTR_FASTPATH").is_err() {
            if let Some(name_str) = name.to_str() {
                if name_str == "security.capability" {
                    reply.error(libc::ENODATA);
                    return;
                }
            }
        }

        let response = self.send_request_sync(VolumeRequest::Getxattr {
            ino,
            name: name.to_string_lossy().to_string(),
            size,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Xattr { data } => reply.data(&data),
            VolumeResponse::XattrSize { size } => reply.size(size),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn listxattr(&mut self, req: &Request, ino: u64, size: u32, reply: ReplyXattr) {
        let response = self.send_request_sync(VolumeRequest::Listxattr {
            ino,
            size,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Xattr { data } => reply.data(&data),
            VolumeResponse::XattrSize { size } => reply.size(size),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn removexattr(&mut self, req: &Request, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Removexattr {
            ino,
            name: name.to_string_lossy().to_string(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn fallocate(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Fallocate {
            ino,
            fh,
            offset: offset as u64,
            length: length as u64,
            mode: mode as u32,
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn lseek(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let response = self.send_request_sync(VolumeRequest::Lseek {
            ino,
            fh,
            offset,
            whence: whence as u32,
        });

        match response {
            VolumeResponse::Lseek { offset } => reply.offset(offset as i64),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn getlk(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let response = self.send_request_sync(VolumeRequest::Getlk {
            ino,
            fh,
            lock_owner,
            start,
            end,
            typ,
            pid,
        });

        match response {
            VolumeResponse::Lock {
                start,
                end,
                typ,
                pid,
            } => reply.locked(start, end, typ, pid),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn setlk(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Setlk {
            ino,
            fh,
            lock_owner,
            start,
            end,
            typ,
            pid,
            sleep,
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn copy_file_range(
        &mut self,
        _req: &Request,
        ino_in: u64,
        fh_in: u64,
        offset_in: i64,
        ino_out: u64,
        fh_out: u64,
        offset_out: i64,
        len: u64,
        flags: u32,
        reply: ReplyWrite,
    ) {
        let response = self.send_request_sync(VolumeRequest::CopyFileRange {
            ino_in,
            fh_in,
            offset_in: offset_in as u64,
            ino_out,
            fh_out,
            offset_out: offset_out as u64,
            len,
            flags,
        });

        match response {
            VolumeResponse::Written { size } => reply.written(size),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn remap_file_range(
        &mut self,
        _req: &Request,
        ino_in: u64,
        fh_in: u64,
        offset_in: i64,
        ino_out: u64,
        fh_out: u64,
        offset_out: i64,
        len: u64,
        remap_flags: u32,
        reply: ReplyWrite,
    ) {
        tracing::info!(
            target: "fuse-pipe::client",
            ino_in, fh_in, offset_in, ino_out, fh_out, offset_out, len, remap_flags,
            "remap_file_range called"
        );

        let response = self.send_request_sync(VolumeRequest::RemapFileRange {
            ino_in,
            fh_in,
            offset_in: offset_in as u64,
            ino_out,
            fh_out,
            offset_out: offset_out as u64,
            len,
            remap_flags,
        });

        tracing::info!(
            target: "fuse-pipe::client",
            ?response,
            "remap_file_range response"
        );

        match response {
            VolumeResponse::Written { size } => reply.written(size),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }
}
