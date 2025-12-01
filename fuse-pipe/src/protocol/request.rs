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
    Setattr {
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
    },

    /// Write data to an open file.
    Write {
        ino: u64,
        fh: u64,
        offset: u64,
        data: Vec<u8>,
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
        };
        assert!(read.is_read_op());

        let write = VolumeRequest::Write {
            ino: 1,
            fh: 1,
            offset: 0,
            data: vec![0; 100],
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
