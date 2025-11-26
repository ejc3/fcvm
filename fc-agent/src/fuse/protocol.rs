//! FUSE-over-vsock protocol types.
//!
//! These types mirror the host-side protocol for communication
//! between guest FUSE filesystem and host VolumeServer.

use serde::{Deserialize, Serialize};

/// File attributes returned by the filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttr {
    /// Inode number
    pub ino: u64,
    /// Size in bytes
    pub size: u64,
    /// Size in blocks (512-byte blocks)
    pub blocks: u64,
    /// Access time (seconds since epoch)
    pub atime_secs: i64,
    /// Access time (nanoseconds)
    pub atime_nsecs: u32,
    /// Modification time (seconds since epoch)
    pub mtime_secs: i64,
    /// Modification time (nanoseconds)
    pub mtime_nsecs: u32,
    /// Change time (seconds since epoch)
    pub ctime_secs: i64,
    /// Change time (nanoseconds)
    pub ctime_nsecs: u32,
    /// File mode (permissions + type)
    pub mode: u32,
    /// Number of hard links
    pub nlink: u32,
    /// Owner user ID
    pub uid: u32,
    /// Owner group ID
    pub gid: u32,
    /// Device ID (for special files)
    pub rdev: u32,
    /// Block size for filesystem I/O
    pub blksize: u32,
}

/// Directory entry for readdir responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    /// Inode number
    pub ino: u64,
    /// Entry name
    pub name: String,
    /// File type (from mode >> 12)
    pub file_type: u8,
}

/// Requests from guest to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VolumeRequest {
    // Metadata operations
    /// Look up a directory entry by name
    Lookup { parent: u64, name: String },
    /// Get file attributes
    Getattr { ino: u64 },
    /// Set file attributes
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
    },

    // Directory operations
    /// Read directory entries
    Readdir { ino: u64, offset: u64 },
    /// Create a directory
    Mkdir { parent: u64, name: String, mode: u32 },
    /// Remove a directory
    Rmdir { parent: u64, name: String },

    // File operations
    /// Create and open a file
    Create {
        parent: u64,
        name: String,
        mode: u32,
        flags: u32,
    },
    /// Open a file
    Open { ino: u64, flags: u32 },
    /// Read from a file
    Read {
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    },
    /// Write to a file
    Write {
        ino: u64,
        fh: u64,
        offset: u64,
        data: Vec<u8>,
    },
    /// Close a file handle
    Release { ino: u64, fh: u64 },
    /// Flush file data to disk
    Flush { ino: u64, fh: u64 },
    /// Sync file data
    Fsync { ino: u64, fh: u64, datasync: bool },

    // Link operations
    /// Remove a file
    Unlink { parent: u64, name: String },
    /// Rename a file or directory
    Rename {
        parent: u64,
        name: String,
        newparent: u64,
        newname: String,
    },
    /// Create a symbolic link
    Symlink {
        parent: u64,
        name: String,
        target: String,
    },
    /// Read a symbolic link target
    Readlink { ino: u64 },
    /// Create a hard link
    Link {
        ino: u64,
        newparent: u64,
        newname: String,
    },

    // Permission check
    /// Check file access permissions
    Access { ino: u64, mask: u32 },

    // Filesystem info
    /// Get filesystem statistics
    Statfs { ino: u64 },
}

/// Responses from host to guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VolumeResponse {
    /// File attributes response
    Attr { attr: FileAttr, ttl_secs: u64 },
    /// Directory entry response (lookup, create, mkdir, symlink, link)
    Entry {
        attr: FileAttr,
        generation: u64,
        ttl_secs: u64,
    },
    /// Read data response
    Data { data: Vec<u8> },
    /// Write result
    Written { size: u32 },
    /// File opened (returns file handle)
    Opened { fh: u64, flags: u32 },
    /// Directory entries
    DirEntries { entries: Vec<DirEntry> },
    /// Symlink target
    Symlink { target: String },
    /// Filesystem statistics
    Statfs {
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
        bsize: u32,
        namelen: u32,
        frsize: u32,
    },
    /// Success with no data
    Ok,
    /// Error response
    Error { errno: i32 },
}

/// Wire format: length-prefixed bincode messages.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024; // 16 MB max message

/// File type constants (compatible with FUSE)
#[allow(dead_code)]
pub mod file_type {
    pub const UNKNOWN: u8 = 0;
    pub const FIFO: u8 = 1;
    pub const CHR: u8 = 2;
    pub const DIR: u8 = 4;
    pub const BLK: u8 = 6;
    pub const REG: u8 = 8;
    pub const LNK: u8 = 10;
    pub const SOCK: u8 = 12;
}
