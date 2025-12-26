//! Filesystem handler trait for FUSE operations.

// Allow many arguments - FUSE operations have fixed parameter sets
#![allow(clippy::too_many_arguments)]

use crate::protocol::{VolumeRequest, VolumeResponse};

/// Handler for FUSE filesystem operations.
///
/// Implementors provide the actual filesystem logic. Operations are
/// executed synchronously (they may block) but are called from async
/// context via `spawn_blocking`.
///
/// The default implementation returns ENOSYS for all operations.
pub trait FilesystemHandler: Send + Sync {
    /// Handle a complete FUSE request with supplementary groups.
    ///
    /// This is the main entry point. The supplementary_groups parameter contains
    /// the caller's supplementary groups, which are needed for proper permission
    /// checks (especially chown to a supplementary group).
    ///
    /// Real handlers should override this method. The default ignores groups
    /// and delegates to handle_request (suitable for simple test handlers).
    fn handle_request_with_groups(
        &self,
        request: &VolumeRequest,
        supplementary_groups: &[u32],
    ) -> VolumeResponse {
        let _ = supplementary_groups;
        self.handle_request(request)
    }

    /// Handle a complete FUSE request (without supplementary groups).
    ///
    /// Used by the default handle_request_with_groups. The default implementation
    /// dispatches to individual operation methods (returning ENOSYS).
    fn handle_request(&self, request: &VolumeRequest) -> VolumeResponse {
        match request {
            VolumeRequest::Lookup {
                parent,
                name,
                uid,
                gid,
                pid,
            } => self.lookup(*parent, name, *uid, *gid, *pid),
            VolumeRequest::Getattr { ino } => self.getattr(*ino),
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
                caller_uid,
                caller_gid,
                caller_pid,
            } => self.setattr(
                *ino,
                *mode,
                *uid,
                *gid,
                *size,
                *atime_secs,
                *atime_nsecs,
                *atime_now,
                *mtime_secs,
                *mtime_nsecs,
                *mtime_now,
                *fh,
                *caller_uid,
                *caller_gid,
                *caller_pid,
            ),
            VolumeRequest::Readdir {
                ino,
                offset,
                uid,
                gid,
                pid,
            } => self.readdir(*ino, *offset, *uid, *gid, *pid),
            VolumeRequest::Mkdir {
                parent,
                name,
                mode,
                uid,
                gid,
                pid,
            } => self.mkdir(*parent, name, *mode, *uid, *gid, *pid),
            VolumeRequest::Mknod {
                parent,
                name,
                mode,
                rdev,
                uid,
                gid,
                pid,
            } => self.mknod(*parent, name, *mode, *rdev, *uid, *gid, *pid),
            VolumeRequest::Rmdir {
                parent,
                name,
                uid,
                gid,
                pid,
            } => self.rmdir(*parent, name, *uid, *gid, *pid),
            VolumeRequest::Create {
                parent,
                name,
                mode,
                flags,
                uid,
                gid,
                pid,
            } => self.create(*parent, name, *mode, *flags, *uid, *gid, *pid),
            VolumeRequest::Open {
                ino,
                flags,
                uid,
                gid,
                pid,
            } => self.open(*ino, *flags, *uid, *gid, *pid),
            VolumeRequest::Read {
                ino,
                fh,
                offset,
                size,
                uid,
                gid,
                pid,
            } => self.read(*ino, *fh, *offset, *size, *uid, *gid, *pid),
            VolumeRequest::Write {
                ino,
                fh,
                offset,
                data,
                uid,
                gid,
                pid,
            } => self.write(*ino, *fh, *offset, data, *uid, *gid, *pid),
            VolumeRequest::Release { ino, fh } => self.release(*ino, *fh),
            VolumeRequest::Flush { ino, fh } => self.flush(*ino, *fh),
            VolumeRequest::Fsync { ino, fh, datasync } => self.fsync(*ino, *fh, *datasync),
            VolumeRequest::Unlink {
                parent,
                name,
                uid,
                gid,
                pid,
            } => self.unlink(*parent, name, *uid, *gid, *pid),
            VolumeRequest::Rename {
                parent,
                name,
                newparent,
                newname,
                uid,
                gid,
                pid,
            } => self.rename(*parent, name, *newparent, newname, *uid, *gid, *pid),
            VolumeRequest::Symlink {
                parent,
                name,
                target,
                uid,
                gid,
                pid,
            } => self.symlink(*parent, name, target, *uid, *gid, *pid),
            VolumeRequest::Readlink { ino } => self.readlink(*ino),
            VolumeRequest::Link {
                ino,
                newparent,
                newname,
                uid,
                gid,
                pid,
            } => self.link(*ino, *newparent, newname, *uid, *gid, *pid),
            VolumeRequest::Access {
                ino,
                mask,
                uid,
                gid,
                pid,
            } => self.access(*ino, *mask, *uid, *gid, *pid),
            VolumeRequest::Statfs { ino } => self.statfs(*ino),
            VolumeRequest::Opendir {
                ino,
                flags,
                uid,
                gid,
                pid,
            } => self.opendir(*ino, *flags, *uid, *gid, *pid),
            VolumeRequest::Releasedir { ino, fh } => self.releasedir(*ino, *fh),
            VolumeRequest::Fsyncdir { ino, fh, datasync } => self.fsyncdir(*ino, *fh, *datasync),
            VolumeRequest::Setxattr {
                ino,
                name,
                value,
                flags,
                uid,
                gid,
                pid,
            } => self.setxattr(*ino, name, value, *flags, *uid, *gid, *pid),
            VolumeRequest::Getxattr {
                ino,
                name,
                size,
                uid,
                gid,
                pid,
            } => self.getxattr(*ino, name, *size, *uid, *gid, *pid),
            VolumeRequest::Listxattr {
                ino,
                size,
                uid,
                gid,
                pid,
            } => self.listxattr(*ino, *size, *uid, *gid, *pid),
            VolumeRequest::Removexattr {
                ino,
                name,
                uid,
                gid,
                pid,
            } => self.removexattr(*ino, name, *uid, *gid, *pid),
            VolumeRequest::Fallocate {
                ino,
                fh,
                offset,
                length,
                mode,
            } => self.fallocate(*ino, *fh, *offset, *length, *mode),
            VolumeRequest::Lseek {
                ino,
                fh,
                offset,
                whence,
            } => self.lseek(*ino, *fh, *offset, *whence),
            VolumeRequest::Getlk {
                ino,
                fh,
                lock_owner,
                start,
                end,
                typ,
                pid,
            } => self.getlk(*ino, *fh, *lock_owner, *start, *end, *typ, *pid),
            VolumeRequest::Setlk {
                ino,
                fh,
                lock_owner,
                start,
                end,
                typ,
                pid,
                sleep,
            } => self.setlk(*ino, *fh, *lock_owner, *start, *end, *typ, *pid, *sleep),
            VolumeRequest::Readdirplus {
                ino,
                fh,
                offset,
                uid,
                gid,
                pid,
            } => self.readdirplus(*ino, *fh, *offset, *uid, *gid, *pid),
            VolumeRequest::CopyFileRange {
                ino_in,
                fh_in,
                offset_in,
                ino_out,
                fh_out,
                offset_out,
                len,
                flags,
            } => self.copy_file_range(
                *ino_in, *fh_in, *offset_in, *ino_out, *fh_out, *offset_out, *len, *flags,
            ),
        }
    }

    /// Look up a directory entry by name.
    fn lookup(&self, _parent: u64, _name: &str, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Get file attributes.
    fn getattr(&self, _ino: u64) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Set file attributes.
    /// Note: `uid`/`gid` are the values to set on the file.
    /// `caller_uid`/`caller_gid` are the credentials of the requesting process.
    ///
    /// Time handling:
    /// - `atime_now`/`mtime_now` = true: Use UTIME_NOW (set to current time)
    /// - `atime_secs`/`mtime_secs` = Some: Use specific time value
    /// - Both false/None: Use UTIME_OMIT (don't change)
    ///
    /// File handle (fh):
    /// - When present, indicates this is ftruncate() on an already-open fd
    /// - Should use fd-based truncate which doesn't re-check permissions
    /// - When None, this is path-based truncate() which needs permission check
    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime_secs: Option<i64>,
        _atime_nsecs: Option<u32>,
        _atime_now: bool,
        _mtime_secs: Option<i64>,
        _mtime_nsecs: Option<u32>,
        _mtime_now: bool,
        _fh: Option<u64>,
        _caller_uid: u32,
        _caller_gid: u32,
        _caller_pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Read directory contents.
    fn readdir(&self, _ino: u64, _offset: u64, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create a directory.
    #[allow(clippy::too_many_arguments)]
    fn mkdir(
        &self,
        _parent: u64,
        _name: &str,
        _mode: u32,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create a special file (device node, FIFO, socket).
    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &self,
        _parent: u64,
        _name: &str,
        _mode: u32,
        _rdev: u32,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Remove a directory.
    fn rmdir(&self, _parent: u64, _name: &str, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create and open a file.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        _parent: u64,
        _name: &str,
        _mode: u32,
        _flags: u32,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Open a file.
    fn open(&self, _ino: u64, _flags: u32, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Read data from an open file.
    fn read(
        &self,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _size: u32,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Write data to an open file.
    fn write(
        &self,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _data: &[u8],
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Release an open file.
    fn release(&self, _ino: u64, _fh: u64) -> VolumeResponse {
        VolumeResponse::Ok
    }

    /// Flush file data.
    fn flush(&self, _ino: u64, _fh: u64) -> VolumeResponse {
        VolumeResponse::Ok
    }

    /// Synchronize file contents.
    fn fsync(&self, _ino: u64, _fh: u64, _datasync: bool) -> VolumeResponse {
        VolumeResponse::Ok
    }

    /// Remove a file.
    fn unlink(&self, _parent: u64, _name: &str, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Rename a file or directory.
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &self,
        _parent: u64,
        _name: &str,
        _newparent: u64,
        _newname: &str,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create a symbolic link.
    #[allow(clippy::too_many_arguments)]
    fn symlink(
        &self,
        _parent: u64,
        _name: &str,
        _target: &str,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Read the target of a symbolic link.
    fn readlink(&self, _ino: u64) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create a hard link.
    #[allow(clippy::too_many_arguments)]
    fn link(
        &self,
        _ino: u64,
        _newparent: u64,
        _newname: &str,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Check file access permissions.
    fn access(&self, _ino: u64, _mask: u32, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Ok // Default: allow all access
    }

    /// Get filesystem statistics.
    fn statfs(&self, _ino: u64) -> VolumeResponse {
        // Return default statfs
        VolumeResponse::Statfs {
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        }
    }

    /// Open a directory.
    fn opendir(&self, _ino: u64, _flags: u32, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Release a directory handle.
    fn releasedir(&self, _ino: u64, _fh: u64) -> VolumeResponse {
        VolumeResponse::Ok
    }

    /// Synchronize directory contents.
    fn fsyncdir(&self, _ino: u64, _fh: u64, _datasync: bool) -> VolumeResponse {
        VolumeResponse::Ok
    }

    /// Set an extended attribute.
    #[allow(clippy::too_many_arguments)]
    fn setxattr(
        &self,
        _ino: u64,
        _name: &str,
        _value: &[u8],
        _flags: u32,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Get an extended attribute.
    #[allow(clippy::too_many_arguments)]
    fn getxattr(
        &self,
        _ino: u64,
        _name: &str,
        _size: u32,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// List extended attributes.
    fn listxattr(&self, _ino: u64, _size: u32, _uid: u32, _gid: u32, _pid: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Remove an extended attribute.
    fn removexattr(
        &self,
        _ino: u64,
        _name: &str,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Preallocate file space.
    fn fallocate(
        &self,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _length: u64,
        _mode: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Seek with SEEK_HOLE/SEEK_DATA support.
    fn lseek(&self, _ino: u64, _fh: u64, _offset: i64, _whence: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Test for a POSIX file lock.
    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Acquire, modify or release a POSIX file lock.
    #[allow(clippy::too_many_arguments)]
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
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Read directory contents with full attributes (combined readdir + lookup).
    #[allow(clippy::too_many_arguments)]
    fn readdirplus(
        &self,
        _ino: u64,
        _fh: u64,
        _offset: u64,
        _uid: u32,
        _gid: u32,
        _pid: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Copy data between two files without going through userspace.
    /// On btrfs, this creates a reflink (instant copy-on-write).
    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _ino_in: u64,
        _fh_in: u64,
        _offset_in: u64,
        _ino_out: u64,
        _fh_out: u64,
        _offset_out: u64,
        _len: u64,
        _flags: u32,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopHandler;
    impl FilesystemHandler for NoopHandler {}

    #[test]
    fn test_default_implementations() {
        let handler = NoopHandler;

        // Most operations should return ENOSYS
        assert_eq!(
            handler.lookup(1, "test", 1000, 1000, 1234).errno(),
            Some(libc::ENOSYS)
        );
        assert_eq!(handler.getattr(1).errno(), Some(libc::ENOSYS));

        // Some have permissive defaults
        assert!(handler.access(1, 0, 1000, 1000, 1234).is_ok());
        assert!(handler.release(1, 1).is_ok());
    }

    #[test]
    fn test_handle_request_dispatch() {
        let handler = NoopHandler;

        let req = VolumeRequest::Lookup {
            parent: 1,
            name: "test".to_string(),
            uid: 1000,
            gid: 1000,
            pid: 1234,
        };
        let resp = handler.handle_request(&req);
        assert_eq!(resp.errno(), Some(libc::ENOSYS));
    }
}
