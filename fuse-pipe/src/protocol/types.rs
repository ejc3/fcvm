//! Core types for FUSE protocol.

use serde::{Deserialize, Serialize};

/// File attributes returned by the filesystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime_secs: i64,
    pub atime_nsecs: u32,
    pub mtime_secs: i64,
    pub mtime_nsecs: u32,
    pub ctime_secs: i64,
    pub ctime_nsecs: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
}

impl FileAttr {
    /// Create a new FileAttr with the given inode number and default values.
    pub fn new(ino: u64) -> Self {
        Self {
            ino,
            size: 0,
            blocks: 0,
            atime_secs: 0,
            atime_nsecs: 0,
            mtime_secs: 0,
            mtime_nsecs: 0,
            ctime_secs: 0,
            ctime_nsecs: 0,
            mode: 0,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
        }
    }

    /// Check if this is a directory.
    pub fn is_dir(&self) -> bool {
        (self.mode & libc::S_IFMT as u32) == libc::S_IFDIR as u32
    }

    /// Check if this is a regular file.
    pub fn is_file(&self) -> bool {
        (self.mode & libc::S_IFMT as u32) == libc::S_IFREG as u32
    }

    /// Check if this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        (self.mode & libc::S_IFMT as u32) == libc::S_IFLNK as u32
    }
}

/// Directory entry for readdir responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub file_type: u8,
}

impl DirEntry {
    /// Create a new directory entry.
    pub fn new(ino: u64, name: impl Into<String>, file_type: u8) -> Self {
        Self {
            ino,
            name: name.into(),
            file_type,
        }
    }

    /// Create a "." entry for the current directory.
    pub fn dot(ino: u64) -> Self {
        Self::new(ino, ".", file_type::DIR)
    }

    /// Create a ".." entry for the parent directory.
    pub fn dotdot(parent_ino: u64) -> Self {
        Self::new(parent_ino, "..", file_type::DIR)
    }
}

/// File type constants matching FUSE/dirent d_type values.
pub mod file_type {
    pub const UNKNOWN: u8 = 0;
    pub const FIFO: u8 = 1;
    pub const CHR: u8 = 2;
    pub const DIR: u8 = 4;
    pub const BLK: u8 = 6;
    pub const REG: u8 = 8;
    pub const LNK: u8 = 10;
    pub const SOCK: u8 = 12;

    /// Convert from stat mode to file type.
    pub fn from_mode(mode: u32) -> u8 {
        match mode & libc::S_IFMT as u32 {
            x if x == libc::S_IFDIR as u32 => DIR,
            x if x == libc::S_IFREG as u32 => REG,
            x if x == libc::S_IFLNK as u32 => LNK,
            x if x == libc::S_IFCHR as u32 => CHR,
            x if x == libc::S_IFBLK as u32 => BLK,
            x if x == libc::S_IFIFO as u32 => FIFO,
            x if x == libc::S_IFSOCK as u32 => SOCK,
            _ => UNKNOWN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_attr_new() {
        let attr = FileAttr::new(42);
        assert_eq!(attr.ino, 42);
        assert_eq!(attr.nlink, 1);
        assert_eq!(attr.blksize, 4096);
    }

    #[test]
    fn test_dir_entry() {
        let dot = DirEntry::dot(1);
        assert_eq!(dot.name, ".");
        assert_eq!(dot.file_type, file_type::DIR);

        let dotdot = DirEntry::dotdot(1);
        assert_eq!(dotdot.name, "..");
    }

    #[test]
    fn test_file_type_from_mode() {
        assert_eq!(file_type::from_mode(libc::S_IFDIR), file_type::DIR);
        assert_eq!(file_type::from_mode(libc::S_IFREG), file_type::REG);
        assert_eq!(file_type::from_mode(libc::S_IFLNK), file_type::LNK);
    }
}
