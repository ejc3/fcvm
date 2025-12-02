//! FUSE response types.

use super::types::{DirEntry, DirEntryPlus, FileAttr};
use serde::{Deserialize, Serialize};

/// Responses from server to FUSE client.
///
/// Each variant corresponds to the expected response for a FUSE operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VolumeResponse {
    /// File attributes response.
    Attr { attr: FileAttr, ttl_secs: u64 },

    /// Directory entry response (for lookup, mkdir, symlink, link).
    Entry {
        attr: FileAttr,
        generation: u64,
        ttl_secs: u64,
    },

    /// File created and opened response.
    Created {
        attr: FileAttr,
        generation: u64,
        ttl_secs: u64,
        fh: u64,
        flags: u32,
    },

    /// Data read from file.
    Data { data: Vec<u8> },

    /// Number of bytes written.
    Written { size: u32 },

    /// File opened response.
    Opened { fh: u64, flags: u32 },

    /// Directory opened response.
    Openeddir { fh: u64 },

    /// Directory entries response.
    DirEntries { entries: Vec<DirEntry> },

    /// Directory entries with full attributes response (readdirplus).
    DirEntriesPlus { entries: Vec<DirEntryPlus> },

    /// Symbolic link target.
    Symlink { target: String },

    /// Filesystem statistics.
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

    /// Extended attribute data.
    Xattr { data: Vec<u8> },

    /// Extended attribute size (when size=0 in request).
    XattrSize { size: u32 },

    /// Lseek result offset.
    Lseek { offset: u64 },

    /// Lock information response.
    Lock {
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
    },

    /// Success (no data).
    Ok,

    /// Error response with errno.
    Error { errno: i32 },
}

impl VolumeResponse {
    /// Create an error response.
    pub fn error(errno: i32) -> Self {
        VolumeResponse::Error { errno }
    }

    /// Create a not-found error.
    pub fn not_found() -> Self {
        VolumeResponse::Error {
            errno: libc::ENOENT,
        }
    }

    /// Create a permission denied error.
    pub fn permission_denied() -> Self {
        VolumeResponse::Error {
            errno: libc::EACCES,
        }
    }

    /// Create an I/O error.
    pub fn io_error() -> Self {
        VolumeResponse::Error { errno: libc::EIO }
    }

    /// Create a bad file descriptor error.
    pub fn bad_fd() -> Self {
        VolumeResponse::Error { errno: libc::EBADF }
    }

    /// Check if this is an error response.
    pub fn is_error(&self) -> bool {
        matches!(self, VolumeResponse::Error { .. })
    }

    /// Check if this is a success response.
    pub fn is_ok(&self) -> bool {
        !self.is_error()
    }

    /// Get the errno if this is an error response.
    pub fn errno(&self) -> Option<i32> {
        match self {
            VolumeResponse::Error { errno } => Some(*errno),
            _ => None,
        }
    }

    /// Get the data if this is a Data response.
    pub fn data(&self) -> Option<&[u8]> {
        match self {
            VolumeResponse::Data { data } => Some(data),
            _ => None,
        }
    }

    /// Get the file attributes if this is an Attr, Entry, or Created response.
    pub fn attr(&self) -> Option<&FileAttr> {
        match self {
            VolumeResponse::Attr { attr, .. }
            | VolumeResponse::Entry { attr, .. }
            | VolumeResponse::Created { attr, .. } => Some(attr),
            _ => None,
        }
    }
}

impl From<std::io::Error> for VolumeResponse {
    fn from(err: std::io::Error) -> Self {
        VolumeResponse::Error {
            errno: err.raw_os_error().unwrap_or(libc::EIO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_responses() {
        assert_eq!(VolumeResponse::not_found().errno(), Some(libc::ENOENT));
        assert_eq!(
            VolumeResponse::permission_denied().errno(),
            Some(libc::EACCES)
        );
        assert_eq!(VolumeResponse::io_error().errno(), Some(libc::EIO));
        assert_eq!(VolumeResponse::bad_fd().errno(), Some(libc::EBADF));
    }

    #[test]
    fn test_is_error() {
        assert!(VolumeResponse::error(1).is_error());
        assert!(!VolumeResponse::error(1).is_ok());
        assert!(!VolumeResponse::Ok.is_error());
        assert!(VolumeResponse::Ok.is_ok());
    }

    #[test]
    fn test_from_io_error() {
        let err = std::io::Error::from_raw_os_error(libc::ENOENT);
        let resp: VolumeResponse = err.into();
        assert_eq!(resp.errno(), Some(libc::ENOENT));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let resp = VolumeResponse::Data {
            data: vec![1, 2, 3, 4, 5],
        };

        let encoded = bincode::serialize(&resp).unwrap();
        let decoded: VolumeResponse = bincode::deserialize(&encoded).unwrap();
        assert_eq!(resp, decoded);
    }
}
