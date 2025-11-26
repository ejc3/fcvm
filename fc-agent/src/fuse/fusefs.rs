//! FUSE filesystem implementation that proxies to host VolumeServer.

use crate::fuse::client::VolumeClient;
use crate::fuse::protocol::{self, FileAttr, VolumeRequest, VolumeResponse};
use fuser::{
    FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use libc::c_int;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

/// Convert protocol FileAttr to fuser FileAttr
fn to_fuser_attr(attr: &FileAttr) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.blocks,
        atime: UNIX_EPOCH + Duration::new(attr.atime_secs as u64, attr.atime_nsecs),
        mtime: UNIX_EPOCH + Duration::new(attr.mtime_secs as u64, attr.mtime_nsecs),
        ctime: UNIX_EPOCH + Duration::new(attr.ctime_secs as u64, attr.ctime_nsecs),
        crtime: UNIX_EPOCH, // Creation time not tracked
        kind: mode_to_file_type(attr.mode),
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev,
        blksize: attr.blksize,
        flags: 0,
    }
}

/// Convert mode to fuser FileType
fn mode_to_file_type(mode: u32) -> FileType {
    match mode & 0o170000 {
        0o140000 => FileType::Socket,
        0o120000 => FileType::Symlink,
        0o100000 => FileType::RegularFile,
        0o060000 => FileType::BlockDevice,
        0o040000 => FileType::Directory,
        0o020000 => FileType::CharDevice,
        0o010000 => FileType::NamedPipe,
        _ => FileType::RegularFile,
    }
}

/// Convert protocol file_type to fuser FileType
fn protocol_file_type_to_fuser(ft: u8) -> FileType {
    match ft {
        protocol::file_type::DIR => FileType::Directory,
        protocol::file_type::REG => FileType::RegularFile,
        protocol::file_type::LNK => FileType::Symlink,
        protocol::file_type::CHR => FileType::CharDevice,
        protocol::file_type::BLK => FileType::BlockDevice,
        protocol::file_type::FIFO => FileType::NamedPipe,
        protocol::file_type::SOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// FUSE filesystem that proxies operations to host VolumeServer.
pub struct VolumeFs {
    client: Mutex<VolumeClient>,
}

impl VolumeFs {
    /// Create a new VolumeFs connected to the given vsock port.
    pub fn new_vsock(port: u32) -> anyhow::Result<Self> {
        let client = VolumeClient::connect_vsock(port)?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    /// Create a new VolumeFs connected via Unix socket (for testing).
    #[allow(dead_code)]
    pub fn new_unix(path: &str) -> anyhow::Result<Self> {
        let client = VolumeClient::connect_unix(path)?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    /// Send a request to the host.
    fn request(&self, req: &VolumeRequest) -> Result<VolumeResponse, c_int> {
        let mut client = self.client.lock().unwrap();
        client.request(req).map_err(|e| {
            eprintln!("VolumeFs request error: {}", e);
            libc::EIO
        })
    }

    /// Handle error response.
    fn handle_error(resp: &VolumeResponse) -> c_int {
        match resp {
            VolumeResponse::Error { errno } => *errno,
            _ => libc::EIO,
        }
    }
}

impl Filesystem for VolumeFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_string_lossy().to_string();
        let req = VolumeRequest::Lookup { parent, name };

        match self.request(&req) {
            Ok(VolumeResponse::Entry { attr, generation, ttl_secs }) => {
                let ttl = Duration::from_secs(ttl_secs);
                reply.entry(&ttl, &to_fuser_attr(&attr), generation);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let req = VolumeRequest::Getattr { ino };

        match self.request(&req) {
            Ok(VolumeResponse::Attr { attr, ttl_secs }) => {
                let ttl = Duration::from_secs(ttl_secs);
                reply.attr(&ttl, &to_fuser_attr(&attr));
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let atime_secs = atime.map(|t| match t {
            TimeOrNow::SpecificTime(st) => st
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            TimeOrNow::Now => std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        });
        let mtime_secs = mtime.map(|t| match t {
            TimeOrNow::SpecificTime(st) => st
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            TimeOrNow::Now => std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        });

        let req = VolumeRequest::Setattr {
            ino,
            mode,
            uid,
            gid,
            size,
            atime_secs,
            atime_nsecs: None,
            mtime_secs,
            mtime_nsecs: None,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Attr { attr, ttl_secs }) => {
                let ttl = Duration::from_secs(ttl_secs);
                reply.attr(&ttl, &to_fuser_attr(&attr));
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let req = VolumeRequest::Readdir {
            ino,
            offset: offset as u64,
        };

        match self.request(&req) {
            Ok(VolumeResponse::DirEntries { entries }) => {
                for (i, entry) in entries.iter().enumerate() {
                    let file_type = protocol_file_type_to_fuser(entry.file_type);
                    // Offset is 1-based for FUSE
                    let entry_offset = offset + i as i64 + 1;
                    if reply.add(entry.ino, entry_offset, file_type, &entry.name) {
                        break; // Buffer full
                    }
                }
                reply.ok();
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let name = name.to_string_lossy().to_string();
        let req = VolumeRequest::Mkdir { parent, name, mode };

        match self.request(&req) {
            Ok(VolumeResponse::Entry { attr, generation, ttl_secs }) => {
                let ttl = Duration::from_secs(ttl_secs);
                reply.entry(&ttl, &to_fuser_attr(&attr), generation);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_string_lossy().to_string();
        let req = VolumeRequest::Rmdir { parent, name };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let name = name.to_string_lossy().to_string();
        let req = VolumeRequest::Create {
            parent,
            name,
            mode,
            flags: flags as u32,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Entry { attr, generation, ttl_secs }) => {
                // We need to also open the file to get a file handle
                let open_req = VolumeRequest::Open {
                    ino: attr.ino,
                    flags: flags as u32,
                };
                match self.request(&open_req) {
                    Ok(VolumeResponse::Opened { fh, flags: open_flags }) => {
                        let ttl = Duration::from_secs(ttl_secs);
                        reply.created(&ttl, &to_fuser_attr(&attr), generation, fh, open_flags);
                    }
                    Ok(resp) => reply.error(Self::handle_error(&resp)),
                    Err(e) => reply.error(e),
                }
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let req = VolumeRequest::Open {
            ino,
            flags: flags as u32,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Opened { fh, flags: open_flags }) => {
                reply.opened(fh, open_flags);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let req = VolumeRequest::Read {
            ino,
            fh,
            offset: offset as u64,
            size,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Data { data }) => {
                reply.data(&data);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let req = VolumeRequest::Write {
            ino,
            fh,
            offset: offset as u64,
            data: data.to_vec(),
        };

        match self.request(&req) {
            Ok(VolumeResponse::Written { size }) => {
                reply.written(size);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let req = VolumeRequest::Release { ino, fh };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn flush(&mut self, _req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let req = VolumeRequest::Flush { ino, fh };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let req = VolumeRequest::Fsync { ino, fh, datasync };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_string_lossy().to_string();
        let req = VolumeRequest::Unlink { parent, name };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let name = name.to_string_lossy().to_string();
        let newname = newname.to_string_lossy().to_string();
        let req = VolumeRequest::Rename {
            parent,
            name,
            newparent,
            newname,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let name = link_name.to_string_lossy().to_string();
        let target = target.to_string_lossy().to_string();
        let req = VolumeRequest::Symlink {
            parent,
            name,
            target,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Entry { attr, generation, ttl_secs }) => {
                let ttl = Duration::from_secs(ttl_secs);
                reply.entry(&ttl, &to_fuser_attr(&attr), generation);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: fuser::ReplyData) {
        let req = VolumeRequest::Readlink { ino };

        match self.request(&req) {
            Ok(VolumeResponse::Symlink { target }) => {
                reply.data(target.as_bytes());
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
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
        let newname = newname.to_string_lossy().to_string();
        let req = VolumeRequest::Link {
            ino,
            newparent,
            newname,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Entry { attr, generation, ttl_secs }) => {
                let ttl = Duration::from_secs(ttl_secs);
                reply.entry(&ttl, &to_fuser_attr(&attr), generation);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn access(&mut self, _req: &Request, ino: u64, mask: i32, reply: ReplyEmpty) {
        let req = VolumeRequest::Access {
            ino,
            mask: mask as u32,
        };

        match self.request(&req) {
            Ok(VolumeResponse::Ok) => reply.ok(),
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&mut self, _req: &Request, ino: u64, reply: ReplyStatfs) {
        let req = VolumeRequest::Statfs { ino };

        match self.request(&req) {
            Ok(VolumeResponse::Statfs {
                blocks,
                bfree,
                bavail,
                files,
                ffree,
                bsize,
                namelen,
                frsize,
            }) => {
                reply.statfs(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize);
            }
            Ok(resp) => reply.error(Self::handle_error(&resp)),
            Err(e) => reply.error(e),
        }
    }
}
