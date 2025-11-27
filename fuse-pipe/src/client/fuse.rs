//! fuser::Filesystem implementation for remote FUSE.

use super::multiplexer::Multiplexer;
use crate::protocol::{file_type, FileAttr, VolumeRequest, VolumeResponse};
use fuser::{
    FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

/// FUSE client that uses a shared multiplexer.
pub struct FuseClient {
    mux: Arc<Multiplexer>,
    reader_id: u32,
}

impl FuseClient {
    /// Create a new client for a specific reader using shared multiplexer.
    pub fn new(mux: Arc<Multiplexer>, reader_id: u32) -> Self {
        Self { mux, reader_id }
    }

    /// Send request and wait for response.
    fn send_request_sync(&self, request: VolumeRequest) -> VolumeResponse {
        self.mux.send_request(self.reader_id, request)
    }
}

/// Convert FileAttr to fuser::FileAttr.
fn to_fuser_attr(attr: &FileAttr) -> fuser::FileAttr {
    let kind = match attr.mode & libc::S_IFMT as u32 {
        x if x == libc::S_IFDIR as u32 => FileType::Directory,
        x if x == libc::S_IFREG as u32 => FileType::RegularFile,
        x if x == libc::S_IFLNK as u32 => FileType::Symlink,
        x if x == libc::S_IFCHR as u32 => FileType::CharDevice,
        x if x == libc::S_IFBLK as u32 => FileType::BlockDevice,
        x if x == libc::S_IFIFO as u32 => FileType::NamedPipe,
        x if x == libc::S_IFSOCK as u32 => FileType::Socket,
        _ => FileType::RegularFile,
    };

    fuser::FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.blocks,
        atime: UNIX_EPOCH + Duration::new(attr.atime_secs as u64, attr.atime_nsecs),
        mtime: UNIX_EPOCH + Duration::new(attr.mtime_secs as u64, attr.mtime_nsecs),
        ctime: UNIX_EPOCH + Duration::new(attr.ctime_secs as u64, attr.ctime_nsecs),
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
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let response = self.send_request_sync(VolumeRequest::Lookup {
            parent,
            name: name.to_string_lossy().to_string(),
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

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
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
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let (atime_secs, atime_nsecs) = match atime {
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                (Some(d.as_secs() as i64), Some(d.subsec_nanos()))
            }
            _ => (None, None),
        };

        let (mtime_secs, mtime_nsecs) = match mtime {
            Some(TimeOrNow::SpecificTime(t)) => {
                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                (Some(d.as_secs() as i64), Some(d.subsec_nanos()))
            }
            _ => (None, None),
        };

        let response = self.send_request_sync(VolumeRequest::Setattr {
            ino,
            mode,
            uid,
            gid,
            size,
            atime_secs,
            atime_nsecs,
            mtime_secs,
            mtime_nsecs,
        });

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
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Mkdir {
            parent,
            name: name.to_string_lossy().to_string(),
            mode,
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

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Rmdir {
            parent,
            name: name.to_string_lossy().to_string(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn create(
        &mut self,
        _req: &Request,
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

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let response = self.send_request_sync(VolumeRequest::Open {
            ino,
            flags: flags as u32,
        });

        match response {
            VolumeResponse::Opened { fh, flags } => reply.opened(fh, flags),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
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
        });

        match response {
            VolumeResponse::Data { data } => reply.data(&data),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
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

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Unlink {
            parent,
            name: name.to_string_lossy().to_string(),
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
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
        });

        match response {
            VolumeResponse::Ok => reply.ok(),
            VolumeResponse::Error { errno } => reply.error(errno),
            _ => reply.error(libc::EIO),
        }
    }

    fn symlink(
        &mut self,
        _req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Symlink {
            parent,
            name: link_name.to_string_lossy().to_string(),
            target: target.to_string_lossy().to_string(),
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
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let response = self.send_request_sync(VolumeRequest::Link {
            ino,
            newparent,
            newname: newname.to_string_lossy().to_string(),
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

    fn access(&mut self, _req: &Request, ino: u64, mask: i32, reply: ReplyEmpty) {
        let response = self.send_request_sync(VolumeRequest::Access {
            ino,
            mask: mask as u32,
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
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let response = self.send_request_sync(VolumeRequest::Readdir {
            ino,
            offset: offset as u64,
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

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
        reply.ok();
    }
}
