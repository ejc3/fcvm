//! Filesystem handler trait for FUSE operations.

use crate::protocol::{VolumeRequest, VolumeResponse};

/// Handler for FUSE filesystem operations.
///
/// Implementors provide the actual filesystem logic. Operations are
/// executed synchronously (they may block) but are called from async
/// context via `spawn_blocking`.
///
/// The default implementation returns ENOSYS for all operations.
pub trait FilesystemHandler: Send + Sync {
    /// Handle a complete FUSE request.
    ///
    /// This is the main entry point. The default implementation
    /// dispatches to individual operation methods.
    fn handle_request(&self, request: &VolumeRequest) -> VolumeResponse {
        match request {
            VolumeRequest::Lookup { parent, name } => self.lookup(*parent, name),
            VolumeRequest::Getattr { ino } => self.getattr(*ino),
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
            } => self.setattr(
                *ino,
                *mode,
                *uid,
                *gid,
                *size,
                *atime_secs,
                *atime_nsecs,
                *mtime_secs,
                *mtime_nsecs,
            ),
            VolumeRequest::Readdir { ino, offset } => self.readdir(*ino, *offset),
            VolumeRequest::Mkdir { parent, name, mode } => self.mkdir(*parent, name, *mode),
            VolumeRequest::Rmdir { parent, name } => self.rmdir(*parent, name),
            VolumeRequest::Create {
                parent,
                name,
                mode,
                flags,
            } => self.create(*parent, name, *mode, *flags),
            VolumeRequest::Open { ino, flags } => self.open(*ino, *flags),
            VolumeRequest::Read {
                ino,
                fh,
                offset,
                size,
            } => self.read(*ino, *fh, *offset, *size),
            VolumeRequest::Write {
                ino,
                fh,
                offset,
                data,
            } => self.write(*ino, *fh, *offset, data),
            VolumeRequest::Release { ino, fh } => self.release(*ino, *fh),
            VolumeRequest::Flush { ino, fh } => self.flush(*ino, *fh),
            VolumeRequest::Fsync { ino, fh, datasync } => self.fsync(*ino, *fh, *datasync),
            VolumeRequest::Unlink { parent, name } => self.unlink(*parent, name),
            VolumeRequest::Rename {
                parent,
                name,
                newparent,
                newname,
            } => self.rename(*parent, name, *newparent, newname),
            VolumeRequest::Symlink {
                parent,
                name,
                target,
            } => self.symlink(*parent, name, target),
            VolumeRequest::Readlink { ino } => self.readlink(*ino),
            VolumeRequest::Link {
                ino,
                newparent,
                newname,
            } => self.link(*ino, *newparent, newname),
            VolumeRequest::Access { ino, mask } => self.access(*ino, *mask),
            VolumeRequest::Statfs { ino } => self.statfs(*ino),
        }
    }

    /// Look up a directory entry by name.
    fn lookup(&self, _parent: u64, _name: &str) -> VolumeResponse {
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
        _mtime_secs: Option<i64>,
        _mtime_nsecs: Option<u32>,
    ) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Read directory contents.
    fn readdir(&self, _ino: u64, _offset: u64) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create a directory.
    fn mkdir(&self, _parent: u64, _name: &str, _mode: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Remove a directory.
    fn rmdir(&self, _parent: u64, _name: &str) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create and open a file.
    fn create(&self, _parent: u64, _name: &str, _mode: u32, _flags: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Open a file.
    fn open(&self, _ino: u64, _flags: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Read data from an open file.
    fn read(&self, _ino: u64, _fh: u64, _offset: u64, _size: u32) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Write data to an open file.
    fn write(&self, _ino: u64, _fh: u64, _offset: u64, _data: &[u8]) -> VolumeResponse {
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
    fn unlink(&self, _parent: u64, _name: &str) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Rename a file or directory.
    fn rename(&self, _parent: u64, _name: &str, _newparent: u64, _newname: &str) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Create a symbolic link.
    fn symlink(&self, _parent: u64, _name: &str, _target: &str) -> VolumeResponse {
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
    fn link(&self, _ino: u64, _newparent: u64, _newname: &str) -> VolumeResponse {
        VolumeResponse::Error {
            errno: libc::ENOSYS,
        }
    }

    /// Check file access permissions.
    fn access(&self, _ino: u64, _mask: u32) -> VolumeResponse {
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
        assert_eq!(handler.lookup(1, "test").errno(), Some(libc::ENOSYS));
        assert_eq!(handler.getattr(1).errno(), Some(libc::ENOSYS));

        // Some have permissive defaults
        assert!(handler.access(1, 0).is_ok());
        assert!(handler.release(1, 1).is_ok());
    }

    #[test]
    fn test_handle_request_dispatch() {
        let handler = NoopHandler;

        let req = VolumeRequest::Lookup {
            parent: 1,
            name: "test".to_string(),
        };
        let resp = handler.handle_request(&req);
        assert_eq!(resp.errno(), Some(libc::ENOSYS));
    }
}
