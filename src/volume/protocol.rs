//! FUSE-over-vsock protocol types.
//!
//! These types define the wire protocol between the host VolumeServer
//! and guest FUSE filesystem. Uses bincode for efficient serialization.

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
///
/// Each message is sent as:
/// - 4 bytes: big-endian u32 length
/// - N bytes: bincode-encoded VolumeRequest or VolumeResponse
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024; // 16 MB max message

/// File type constants (compatible with FUSE)
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

/// Convert mode to file type
pub fn mode_to_file_type(mode: u32) -> u8 {
    match mode & 0o170000 {
        0o140000 => file_type::SOCK,
        0o120000 => file_type::LNK,
        0o100000 => file_type::REG,
        0o060000 => file_type::BLK,
        0o040000 => file_type::DIR,
        0o020000 => file_type::CHR,
        0o010000 => file_type::FIFO,
        _ => file_type::UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = VolumeRequest::Lookup {
            parent: 1,
            name: "test.txt".to_string(),
        };
        let encoded = bincode::serialize(&req).unwrap();
        let decoded: VolumeRequest = bincode::deserialize(&encoded).unwrap();
        match decoded {
            VolumeRequest::Lookup { parent, name } => {
                assert_eq!(parent, 1);
                assert_eq!(name, "test.txt");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_response_serialization() {
        let resp = VolumeResponse::Written { size: 4096 };
        let encoded = bincode::serialize(&resp).unwrap();
        let decoded: VolumeResponse = bincode::deserialize(&encoded).unwrap();
        match decoded {
            VolumeResponse::Written { size } => assert_eq!(size, 4096),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_file_attr_serialization() {
        let attr = FileAttr {
            ino: 1,
            size: 1024,
            blocks: 2,
            atime_secs: 1700000000,
            atime_nsecs: 0,
            mtime_secs: 1700000000,
            mtime_nsecs: 0,
            ctime_secs: 1700000000,
            ctime_nsecs: 0,
            mode: 0o100644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
        };
        let encoded = bincode::serialize(&attr).unwrap();
        let decoded: FileAttr = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.ino, 1);
        assert_eq!(decoded.size, 1024);
        assert_eq!(decoded.mode, 0o100644);
    }

    #[test]
    fn test_mode_to_file_type() {
        assert_eq!(mode_to_file_type(0o100644), file_type::REG);
        assert_eq!(mode_to_file_type(0o040755), file_type::DIR);
        assert_eq!(mode_to_file_type(0o120777), file_type::LNK);
    }
}
