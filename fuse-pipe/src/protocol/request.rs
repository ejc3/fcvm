//! FUSE request types.

use serde::{Deserialize, Serialize};

/// Requests from FUSE client to server.
///
/// Each variant represents a FUSE operation that the client wants
/// the server to perform on the underlying filesystem.
///
/// Many operations include `uid` and `gid` fields which represent the
/// credentials of the calling process. The server should use these to
/// perform proper permission checking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VolumeRequest {
    /// Look up a directory entry by name.
    Lookup {
        parent: u64,
        name: String,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Get file attributes.
    Getattr { ino: u64 },

    /// Set file attributes.
    /// Note: `uid`/`gid` here are the *values to set*, not caller credentials.
    /// The caller credentials are in `caller_uid`/`caller_gid`.
    ///
    /// Time handling:
    /// - `atime_now`/`mtime_now` = true: Use UTIME_NOW (current time)
    /// - `atime_secs`/`mtime_secs` = Some: Use specific time
    /// - Both false/None: Use UTIME_OMIT (don't change)
    Setattr {
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
        /// File handle for ftruncate - when present, use fd-based truncate
        /// which doesn't re-check file permissions (they were validated at open time).
        fh: Option<u64>,
        caller_uid: u32,
        caller_gid: u32,
        caller_pid: u32,
    },

    /// Read directory contents.
    Readdir {
        ino: u64,
        offset: u64,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Create a directory.
    Mkdir {
        parent: u64,
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Create a special file (device node, FIFO, socket).
    Mknod {
        parent: u64,
        name: String,
        mode: u32,
        rdev: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Remove a directory.
    Rmdir {
        parent: u64,
        name: String,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Create and open a file.
    Create {
        parent: u64,
        name: String,
        mode: u32,
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Open a file.
    Open {
        ino: u64,
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Read data from an open file.
    Read {
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Write data to an open file.
    Write {
        ino: u64,
        fh: u64,
        offset: u64,
        data: Vec<u8>,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Release an open file (close).
    Release { ino: u64, fh: u64 },

    /// Flush file data.
    Flush { ino: u64, fh: u64 },

    /// Synchronize file contents.
    Fsync { ino: u64, fh: u64, datasync: bool },

    /// Remove a file.
    Unlink {
        parent: u64,
        name: String,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Rename a file or directory.
    Rename {
        parent: u64,
        name: String,
        newparent: u64,
        newname: String,
        /// Rename flags: RENAME_NOREPLACE (1), RENAME_EXCHANGE (2), RENAME_WHITEOUT (4)
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Create a symbolic link.
    Symlink {
        parent: u64,
        name: String,
        target: String,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Read the target of a symbolic link.
    Readlink { ino: u64 },

    /// Create a hard link.
    Link {
        ino: u64,
        newparent: u64,
        newname: String,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Check file access permissions.
    Access {
        ino: u64,
        mask: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Get filesystem statistics.
    Statfs { ino: u64 },

    /// Open a directory.
    Opendir {
        ino: u64,
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Release a directory handle.
    Releasedir { ino: u64, fh: u64 },

    /// Synchronize directory contents.
    Fsyncdir { ino: u64, fh: u64, datasync: bool },

    /// Set an extended attribute.
    Setxattr {
        ino: u64,
        name: String,
        value: Vec<u8>,
        flags: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Get an extended attribute.
    Getxattr {
        ino: u64,
        name: String,
        size: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// List extended attributes.
    Listxattr {
        ino: u64,
        size: u32,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Remove an extended attribute.
    Removexattr {
        ino: u64,
        name: String,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Preallocate file space.
    Fallocate {
        ino: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    },

    /// Seek with SEEK_HOLE/SEEK_DATA support.
    Lseek {
        ino: u64,
        fh: u64,
        offset: i64,
        whence: u32,
    },

    /// Test for a POSIX file lock.
    Getlk {
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
    },

    /// Acquire, modify or release a POSIX file lock.
    Setlk {
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
    },

    /// Read directory contents with full attributes (combined readdir + lookup).
    Readdirplus {
        ino: u64,
        fh: u64,
        offset: u64,
        uid: u32,
        gid: u32,
        pid: u32,
    },

    /// Copy data between two files without going through userspace.
    /// On btrfs, this creates a reflink (instant copy-on-write).
    CopyFileRange {
        ino_in: u64,
        fh_in: u64,
        offset_in: u64,
        ino_out: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        flags: u32,
    },

    /// Remap file range (FICLONE/FICLONERANGE support).
    /// Creates instant copy-on-write clones on btrfs/xfs.
    /// Requires kernel patch - not yet upstream.
    RemapFileRange {
        ino_in: u64,
        fh_in: u64,
        offset_in: u64,
        ino_out: u64,
        fh_out: u64,
        offset_out: u64,
        len: u64,
        /// REMAP_FILE_DEDUP (1), REMAP_FILE_CAN_SHORTEN (2)
        remap_flags: u32,
    },
}

impl VolumeRequest {
    /// Get the operation name for logging/metrics.
    pub fn op_name(&self) -> &'static str {
        match self {
            VolumeRequest::Lookup { .. } => "lookup",
            VolumeRequest::Getattr { .. } => "getattr",
            VolumeRequest::Setattr { .. } => "setattr",
            VolumeRequest::Readdir { .. } => "readdir",
            VolumeRequest::Mkdir { .. } => "mkdir",
            VolumeRequest::Mknod { .. } => "mknod",
            VolumeRequest::Rmdir { .. } => "rmdir",
            VolumeRequest::Create { .. } => "create",
            VolumeRequest::Open { .. } => "open",
            VolumeRequest::Read { .. } => "read",
            VolumeRequest::Write { .. } => "write",
            VolumeRequest::Release { .. } => "release",
            VolumeRequest::Flush { .. } => "flush",
            VolumeRequest::Fsync { .. } => "fsync",
            VolumeRequest::Unlink { .. } => "unlink",
            VolumeRequest::Rename { .. } => "rename",
            VolumeRequest::Symlink { .. } => "symlink",
            VolumeRequest::Readlink { .. } => "readlink",
            VolumeRequest::Link { .. } => "link",
            VolumeRequest::Access { .. } => "access",
            VolumeRequest::Statfs { .. } => "statfs",
            VolumeRequest::Opendir { .. } => "opendir",
            VolumeRequest::Releasedir { .. } => "releasedir",
            VolumeRequest::Fsyncdir { .. } => "fsyncdir",
            VolumeRequest::Setxattr { .. } => "setxattr",
            VolumeRequest::Getxattr { .. } => "getxattr",
            VolumeRequest::Listxattr { .. } => "listxattr",
            VolumeRequest::Removexattr { .. } => "removexattr",
            VolumeRequest::Fallocate { .. } => "fallocate",
            VolumeRequest::Lseek { .. } => "lseek",
            VolumeRequest::Getlk { .. } => "getlk",
            VolumeRequest::Setlk { .. } => "setlk",
            VolumeRequest::Readdirplus { .. } => "readdirplus",
            VolumeRequest::CopyFileRange { .. } => "copy_file_range",
            VolumeRequest::RemapFileRange { .. } => "remap_file_range",
        }
    }

    /// Check if this is a read operation (doesn't modify filesystem).
    pub fn is_read_op(&self) -> bool {
        matches!(
            self,
            VolumeRequest::Lookup { .. }
                | VolumeRequest::Getattr { .. }
                | VolumeRequest::Readdir { .. }
                | VolumeRequest::Read { .. }
                | VolumeRequest::Readlink { .. }
                | VolumeRequest::Access { .. }
                | VolumeRequest::Statfs { .. }
                | VolumeRequest::Opendir { .. }
                | VolumeRequest::Getxattr { .. }
                | VolumeRequest::Listxattr { .. }
                | VolumeRequest::Lseek { .. }
                | VolumeRequest::Getlk { .. }
                | VolumeRequest::Readdirplus { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_op_name() {
        let req = VolumeRequest::Lookup {
            parent: 1,
            name: "test".to_string(),
            uid: 1000,
            gid: 1000,
            pid: 1234,
        };
        assert_eq!(req.op_name(), "lookup");
    }

    #[test]
    fn test_is_read_op() {
        let read = VolumeRequest::Read {
            ino: 1,
            fh: 1,
            offset: 0,
            size: 4096,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        assert!(read.is_read_op());

        let write = VolumeRequest::Write {
            ino: 1,
            fh: 1,
            offset: 0,
            data: vec![0; 100],
            uid: 0,
            gid: 0,
            pid: 0,
        };
        assert!(!write.is_read_op());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let req = VolumeRequest::Create {
            parent: 1,
            name: "newfile.txt".to_string(),
            mode: 0o644,
            flags: libc::O_RDWR as u32,
            uid: 1000,
            gid: 1000,
            pid: 1234,
        };

        let encoded = bincode::serialize(&req).unwrap();
        let decoded: VolumeRequest = bincode::deserialize(&encoded).unwrap();
        assert_eq!(req, decoded);
    }
}
