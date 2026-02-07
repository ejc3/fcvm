//! fuser::Filesystem implementation for remote FUSE.

use super::multiplexer::Multiplexer;
use crate::protocol::{file_type, FileAttr, VolumeRequest, VolumeResponse};
use fuser::{
    AccessFlags, BsdFileFlags, CopyFileRangeFlags, Errno, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, InitFlags, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyLock,
    ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
///
/// This can be shared across multiple fuser threads via Arc. Response routing
/// is done by unique request ID, not by reader/thread ID.
pub struct FuseClient {
    mux: Arc<Multiplexer>,
    /// Optional callback to run when init() completes.
    /// Wrapped in Mutex to satisfy Sync requirement for Filesystem trait.
    init_callback: Mutex<Option<InitCallback>>,
    /// Shared flag set by destroy() to signal clean shutdown to reader threads
    destroyed: Arc<AtomicBool>,
    /// Maximum write size (0 = unbounded). Passed explicitly to avoid env var races.
    max_write: u32,
}

impl FuseClient {
    /// Create a new client using shared multiplexer.
    pub fn new(mux: Arc<Multiplexer>) -> Self {
        Self::with_options(mux, Arc::new(AtomicBool::new(false)), 0)
    }

    /// Create a new client with a shared destroyed flag.
    ///
    /// The destroyed flag is set by `destroy()` when the filesystem is unmounted.
    /// Reader threads can check this flag to distinguish clean shutdown from errors.
    pub fn with_destroyed_flag(mux: Arc<Multiplexer>, destroyed: Arc<AtomicBool>) -> Self {
        Self::with_options(mux, destroyed, 0)
    }

    /// Create a new client with a shared destroyed flag and max_write limit.
    pub fn with_options(mux: Arc<Multiplexer>, destroyed: Arc<AtomicBool>, max_write: u32) -> Self {
        Self {
            mux,
            init_callback: Mutex::new(None),
            destroyed,
            max_write,
        }
    }

    /// Create a new client with a callback to run after INIT completes.
    pub fn with_init_callback(
        mux: Arc<Multiplexer>,
        callback: InitCallback,
        destroyed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            mux,
            init_callback: Mutex::new(Some(callback)),
            destroyed,
            max_write: 0,
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
            self.mux.send_request_with_groups(request, groups)
        } else {
            self.mux.send_request(request)
        }
    }

    /// Send request with supplementary groups and wait for response.
    ///
    /// Reads the caller's supplementary groups from /proc/<pid>/status
    /// and forwards them to the server for proper permission checks.
    fn send_request_with_groups(&self, request: VolumeRequest, pid: u32) -> VolumeResponse {
        let groups = get_supplementary_groups(pid);
        self.mux.send_request_with_groups(request, groups)
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
        ino: INodeNo(attr.ino),
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

impl Filesystem for FuseClient {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> Result<(), io::Error> {
        // Enable writeback cache for better write performance (kernel batches writes).
        // Can be disabled via FCVM_NO_WRITEBACK_CACHE=1 for debugging.
        let enable_writeback = std::env::var("FCVM_NO_WRITEBACK_CACHE").is_err();
        if enable_writeback {
            if let Err(unsupported) = config.add_capabilities(InitFlags::FUSE_WRITEBACK_CACHE) {
                tracing::warn!(
                    target: "fuse-pipe::client",
                    unsupported_flags = ?unsupported,
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
        if let Err(unsupported) = config.add_capabilities(InitFlags::FUSE_AUTO_INVAL_DATA) {
            tracing::warn!(
                target: "fuse-pipe::client",
                unsupported_flags = ?unsupported,
                "Kernel doesn't support FUSE_AUTO_INVAL_DATA"
            );
        } else {
            tracing::debug!(
                target: "fuse-pipe::client",
                "Enabled FUSE_AUTO_INVAL_DATA for cache coherency"
            );
        }

        // Limit max_write to avoid vsock data loss under nested virtualization.
        // Passed explicitly via mount_vsock_with_options to avoid env var races.
        let max_write = self.max_write;

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
        if let Some(callback) = self.init_callback.lock().unwrap().take() {
            callback();
        }
        Ok(())
    }

    fn destroy(&mut self) {
        // Signal to reader threads that this is a clean shutdown.
        // The kernel calls destroy() before closing cloned fds, so reader
        // threads will see this flag when they get ECONNABORTED.
        let was = self.destroyed.swap(true, Ordering::SeqCst);
        tracing::debug!(target: "fuse-pipe::client", was_already_set = was, "destroy() called - signaling clean shutdown");
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.mux.send_request_no_reply(VolumeRequest::Forget {
            ino: ino.into(),
            nlookup,
        });
    }

    // batch_forget: fuser's default impl calls forget() per node,
    // which sends individual VolumeRequest::Forget messages.
    // The server-side handler also supports VolumeRequest::BatchForget
    // for clients that can batch them.

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Lookup {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
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
                    Generation(generation),
                );
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let response = self.send_request_sync(VolumeRequest::Getattr { ino: ino.into() });

        match response {
            VolumeResponse::Attr { attr, ttl_secs } => {
                reply.attr(&Duration::from_secs(ttl_secs), &to_fuser_attr(&attr));
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<BsdFileFlags>,
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
                ino: ino.into(),
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
                fh: fh.map(|h| h.into()),
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
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!(target: "fuse-pipe::client", ?parent, ?name, mode, uid = req.uid(), gid = req.gid(), pid = req.pid(), "mkdir request");
        let response = self.send_request_sync(VolumeRequest::Mkdir {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
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
                    Generation(generation),
                );
            }
            VolumeResponse::Error { errno } => {
                tracing::debug!(target: "fuse-pipe::client", errno, "mkdir error");
                reply.error(Errno::from_i32(errno));
            }
            _ => {
                tracing::debug!(target: "fuse-pipe::client", "mkdir unexpected response");
                reply.error(Errno::EIO);
            }
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Mknod {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
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
                    Generation(generation),
                );
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Rmdir {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let response = self.send_request_sync(VolumeRequest::Create {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
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
                    Generation(generation),
                    FileHandle(fh),
                    FopenFlags::from_bits_retain(flags),
                );
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let response = self.send_request_sync(VolumeRequest::Open {
            ino: ino.into(),
            flags: flags.0 as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Opened { fh, flags } => {
                reply.opened(FileHandle(fh), FopenFlags::from_bits_truncate(flags))
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let response = self.send_request_sync(VolumeRequest::Read {
            ino: ino.into(),
            fh: fh.into(),
            offset,
            size,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Data { data } => reply.data(&data),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let response = self.send_request_sync(VolumeRequest::Write {
            ino: ino.into(),
            fh: fh.into(),
            offset: offset as u64,
            data: data.to_vec(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Written { size } => reply.written(size as u32),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Release {
            ino: ino.into(),
            fh: fh.into(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Flush {
            ino: ino.into(),
            fh: fh.into(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Fsync {
            ino: ino.into(),
            fh: fh.into(),
            datasync,
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Unlink {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Rename {
            parent: parent.into(),
            name: name.as_bytes().to_vec(),
            newparent: newparent.into(),
            newname: newname.as_bytes().to_vec(),
            flags: flags.bits(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Symlink {
            parent: parent.into(),
            name: link_name.as_bytes().to_vec(),
            target: target.as_os_str().as_bytes().to_vec(),
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
                    Generation(generation),
                );
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let response = self.send_request_sync(VolumeRequest::Readlink { ino: ino.into() });

        match response {
            VolumeResponse::Symlink { target } => reply.data(&target),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Link {
            ino: ino.into(),
            newparent: newparent.into(),
            newname: newname.as_bytes().to_vec(),
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
                    Generation(generation),
                );
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Access {
            ino: ino.into(),
            mask: mask.bits() as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        let response = self.send_request_sync(VolumeRequest::Statfs { ino: ino.into() });

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
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let response = self.send_request_sync(VolumeRequest::Readdir {
            ino: ino.into(),
            offset,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::DirEntries { entries } => {
                for (i, entry) in entries.iter().enumerate() {
                    let entry_offset = offset + i as u64 + 1;
                    let ft = protocol_file_type_to_fuser(entry.file_type);
                    if reply.add(
                        INodeNo(entry.ino),
                        entry_offset,
                        ft,
                        OsStr::from_bytes(&entry.name),
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn readdirplus(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let response = self.send_request_sync(VolumeRequest::Readdirplus {
            ino: ino.into(),
            fh: fh.into(),
            offset,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::DirEntriesPlus { entries } => {
                for (i, entry) in entries.iter().enumerate() {
                    let entry_offset = offset + i as u64 + 1;
                    let attr = to_fuser_attr(&entry.attr);
                    let ttl = Duration::from_secs(entry.attr_ttl_secs);
                    if reply.add(
                        INodeNo(entry.ino),
                        entry_offset,
                        OsStr::from_bytes(&entry.name),
                        &ttl,
                        &attr,
                        Generation(entry.generation),
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn opendir(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let response = self.send_request_sync(VolumeRequest::Opendir {
            ino: ino.into(),
            flags: flags.0 as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Openeddir { fh } => reply.opened(FileHandle(fh), FopenFlags::empty()),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Releasedir {
            ino: ino.into(),
            fh: fh.into(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Fsyncdir {
            ino: ino.into(),
            fh: fh.into(),
            datasync,
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn setxattr(
        &self,
        req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Setxattr {
            ino: ino.into(),
            name: name.as_bytes().to_vec(),
            value: value.to_vec(),
            flags: flags as u32,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let response = self.send_request_sync(VolumeRequest::Getxattr {
            ino: ino.into(),
            name: name.as_bytes().to_vec(),
            size,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Xattr { data } => reply.data(&data),
            VolumeResponse::XattrSize { size } => reply.size(size),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let response = self.send_request_sync(VolumeRequest::Listxattr {
            ino: ino.into(),
            size,
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Xattr { data } => reply.data(&data),
            VolumeResponse::XattrSize { size } => reply.size(size),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn removexattr(&self, req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Removexattr {
            ino: ino.into(),
            name: name.as_bytes().to_vec(),
            uid: req.uid(),
            gid: req.gid(),
            pid: req.pid(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Fallocate {
            ino: ino.into(),
            fh: fh.into(),
            offset,
            length,
            mode: mode as u32,
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let response = self.send_request_sync(VolumeRequest::Lseek {
            ino: ino.into(),
            fh: fh.into(),
            offset,
            whence: whence as u32,
        });

        match response {
            VolumeResponse::Lseek { offset } => reply.offset(offset as i64),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let response = self.send_request_sync(VolumeRequest::Getlk {
            ino: ino.into(),
            fh: fh.into(),
            lock_owner: lock_owner.0,
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
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        let response = self.send_request_sync(VolumeRequest::Setlk {
            ino: ino.into(),
            fh: fh.into(),
            lock_owner: lock_owner.0,
            start,
            end,
            typ,
            pid,
            sleep,
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: i64,
        ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: i64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let response = self.send_request_sync(VolumeRequest::CopyFileRange {
            ino_in: ino_in.into(),
            fh_in: fh_in.into(),
            offset_in: offset_in as u64,
            ino_out: ino_out.into(),
            fh_out: fh_out.into(),
            offset_out: offset_out as u64,
            len,
            flags: flags.bits() as u32,
        });

        match response {
            VolumeResponse::Written { size } => reply.written(size as u32),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }

    fn remap_file_range(
        &self,
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
            VolumeResponse::Written { size } => reply.written(size as u32),
            VolumeResponse::Error { errno } => reply.error(Errno::from_i32(errno)),
            _ => reply.error(Errno::EIO),
        }
    }
}
