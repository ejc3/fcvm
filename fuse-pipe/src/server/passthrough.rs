//! Passthrough filesystem implementation.
//!
//! This maps FUSE operations directly to the local filesystem,
//! allowing a directory to be served over the network.

use super::handler::FilesystemHandler;
use crate::protocol::{file_type, DirEntry, FileAttr, VolumeResponse};
use dashmap::DashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Default attribute TTL in seconds.
const ATTR_TTL_SECS: u64 = 1;

/// Inode table mapping inode numbers to parent/name pairs.
struct InodeTable {
    ino_to_entry: DashMap<u64, InodeEntry>,
    name_to_ino: DashMap<(u64, OsString), u64>,
    next_ino: AtomicU64,
    root_path: PathBuf,
}

#[derive(Clone)]
struct InodeEntry {
    parent: u64,
    name: OsString,
}

impl InodeTable {
    fn new(root_path: PathBuf) -> Self {
        let ino_to_entry = DashMap::new();
        let name_to_ino = DashMap::new();

        // Root inode is always 1; parent is 0 and name empty.
        ino_to_entry.insert(
            1,
            InodeEntry {
                parent: 0,
                name: OsString::new(),
            },
        );

        Self {
            ino_to_entry,
            name_to_ino,
            next_ino: AtomicU64::new(2),
            root_path,
        }
    }

    fn resolve_path(&self, ino: u64) -> Option<PathBuf> {
        if ino == 1 {
            return Some(self.root_path.clone());
        }

        let mut components = Vec::new();
        let mut current = ino;

        while current != 1 {
            let entry = self.ino_to_entry.get(&current)?;
            components.push(entry.name.clone());
            current = entry.parent;
        }

        let mut path = self.root_path.clone();
        for component in components.iter().rev() {
            path.push(component);
        }
        Some(path)
    }

    fn get_or_create_ino(&self, parent: u64, name: &OsStr) -> u64 {
        // Try read-only first (lock-free lookup).
        if let Some(ino) = self.name_to_ino.get(&(parent, name.to_os_string())) {
            return *ino;
        }

        // Need to create - use entry API for atomic insert.
        let key = (parent, name.to_os_string());
        let ino = *self.name_to_ino.entry(key.clone()).or_insert_with(|| {
            let new_ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
            self.ino_to_entry.insert(
                new_ino,
                InodeEntry {
                    parent,
                    name: name.to_os_string(),
                },
            );
            new_ino
        });
        ino
    }

    fn remove_entry(&self, parent: u64, name: &OsStr) {
        if let Some((_, ino)) = self.name_to_ino.remove(&(parent, name.to_os_string())) {
            self.ino_to_entry.remove(&ino);
        }
    }

    fn rename_entry(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
    ) -> Option<u64> {
        let key = (parent, name.to_os_string());
        let (_, ino) = self.name_to_ino.remove(&key)?;
        self.name_to_ino.insert((newparent, newname.to_os_string()), ino);

        if let Some(mut entry) = self.ino_to_entry.get_mut(&ino) {
            entry.parent = newparent;
            entry.name = newname.to_os_string();
        }
        Some(ino)
    }

    fn parent_of(&self, ino: u64) -> Option<u64> {
        self.ino_to_entry.get(&ino).map(|e| e.parent)
    }

    fn add_hard_link(&self, parent: u64, name: &OsStr, ino: u64) {
        self.name_to_ino.insert((parent, name.to_os_string()), ino);
    }
}

/// File handle table.
struct HandleTable {
    handles: DashMap<u64, Mutex<File>>,
    next_fh: AtomicU64,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
        }
    }

    fn insert(&self, file: File) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.handles.insert(fh, Mutex::new(file));
        fh
    }

    fn with_file<F, R>(&self, fh: u64, f: F) -> Option<R>
    where
        F: FnOnce(&mut File) -> R,
    {
        self.handles.get(&fh).map(|entry| {
            let mut file = entry.lock().unwrap();
            f(&mut file)
        })
    }

    fn remove(&self, fh: u64) -> Option<File> {
        self.handles
            .remove(&fh)
            .map(|(_, m)| m.into_inner().unwrap())
    }
}

/// Convert filesystem metadata to FileAttr.
fn metadata_to_attr(ino: u64, metadata: &std::fs::Metadata) -> FileAttr {
    FileAttr {
        ino,
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime_secs: metadata.atime(),
        atime_nsecs: metadata.atime_nsec() as u32,
        mtime_secs: metadata.mtime(),
        mtime_nsecs: metadata.mtime_nsec() as u32,
        ctime_secs: metadata.ctime(),
        ctime_nsecs: metadata.ctime_nsec() as u32,
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
    }
}

/// A passthrough filesystem that maps operations to a local directory.
pub struct PassthroughFs {
    inodes: InodeTable,
    handles: HandleTable,
    attr_ttl_secs: u64,
}

impl PassthroughFs {
    /// Create a new passthrough filesystem rooted at the given path.
    pub fn new<P: Into<PathBuf>>(root_path: P) -> Self {
        Self {
            inodes: InodeTable::new(root_path.into()),
            handles: HandleTable::new(),
            attr_ttl_secs: ATTR_TTL_SECS,
        }
    }

    /// Set the attribute TTL.
    pub fn with_attr_ttl(mut self, secs: u64) -> Self {
        self.attr_ttl_secs = secs;
        self
    }

    /// Get the root path.
    pub fn root_path(&self) -> &PathBuf {
        &self.inodes.root_path
    }
}

impl FilesystemHandler for PassthroughFs {
    fn lookup(&self, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let ino = self.inodes.get_or_create_ino(parent, OsStr::new(name));
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn getattr(&self, ino: u64) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let attr = metadata_to_attr(ino, &metadata);
        VolumeResponse::Attr {
            attr,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime_secs: Option<i64>,
        atime_nsecs: Option<u32>,
        mtime_secs: Option<i64>,
        mtime_nsecs: Option<u32>,
    ) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        if let Some(mode) = mode {
            if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(mode)) {
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }

        if uid.is_some() || gid.is_some() {
            let uid = uid.map(nix::unistd::Uid::from_raw);
            let gid = gid.map(nix::unistd::Gid::from_raw);
            if let Err(e) = nix::unistd::chown(&path, uid, gid) {
                return VolumeResponse::error(e as i32);
            }
        }

        if let Some(size) = size {
            let file = match File::options().write(true).open(&path) {
                Ok(f) => f,
                Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
            };
            if let Err(e) = file.set_len(size) {
                return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }

        // Handle atime/mtime updates
        if atime_secs.is_some() || mtime_secs.is_some() {
            use nix::sys::stat::{utimensat, UtimensatFlags};
            use nix::sys::time::TimeSpec;

            let atime = match atime_secs {
                Some(secs) => TimeSpec::new(secs, atime_nsecs.unwrap_or(0) as i64),
                None => TimeSpec::UTIME_OMIT,
            };

            let mtime = match mtime_secs {
                Some(secs) => TimeSpec::new(secs, mtime_nsecs.unwrap_or(0) as i64),
                None => TimeSpec::UTIME_OMIT,
            };

            if let Err(e) = utimensat(None, &path, &atime, &mtime, UtimensatFlags::NoFollowSymlink)
            {
                return VolumeResponse::error(e as i32);
            }
        }

        self.getattr(ino)
    }

    fn readdir(&self, ino: u64, offset: u64) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let mut result = Vec::new();

        // Add . and .. for offset 0
        if offset == 0 {
            result.push(DirEntry::dot(ino));

            let parent_ino = if ino == 1 {
                1
            } else {
                self.inodes.parent_of(ino).unwrap_or(1)
            };
            result.push(DirEntry::dotdot(parent_ino));
        }

        for (i, entry) in entries.enumerate() {
            if (i as u64) < offset.saturating_sub(2) {
                continue;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let entry_ino = self
                .inodes
                .get_or_create_ino(ino, entry.file_name().as_os_str());
            let ft = file_type::from_mode(metadata.mode());

            result.push(DirEntry::new(
                entry_ino,
                entry.file_name().to_string_lossy(),
                ft,
            ));
        }

        VolumeResponse::DirEntries { entries: result }
    }

    fn mkdir(&self, parent: u64, name: &str, mode: u32) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        if let Err(e) = fs::create_dir(&path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let ino = self.inodes.get_or_create_ino(parent, OsStr::new(name));
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn rmdir(&self, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        if let Err(e) = fs::remove_dir(&path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        self.inodes.remove_entry(parent, OsStr::new(name));
        VolumeResponse::Ok
    }

    fn create(&self, parent: u64, name: &str, mode: u32, flags: u32) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(flags & libc::O_TRUNC as u32 != 0)
            .mode(mode)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let ino = self.inodes.get_or_create_ino(parent, OsStr::new(name));
        let fh = self.handles.insert(file);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Created {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
            fh,
            flags: 0,
        }
    }

    fn open(&self, ino: u64, flags: u32) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let mut opts = OpenOptions::new();

        let access_mode = flags & libc::O_ACCMODE as u32;
        if access_mode == libc::O_RDONLY as u32 {
            opts.read(true);
        } else if access_mode == libc::O_WRONLY as u32 {
            opts.write(true);
        } else {
            opts.read(true).write(true);
        }

        if flags & libc::O_APPEND as u32 != 0 {
            opts.append(true);
        }

        let file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let fh = self.handles.insert(file);
        VolumeResponse::Opened { fh, flags: 0 }
    }

    fn read(&self, _ino: u64, fh: u64, offset: u64, size: u32) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                return Err(e.raw_os_error().unwrap_or(libc::EIO));
            }

            let mut buf = vec![0u8; size as usize];
            match file.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    Ok(buf)
                }
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        }) {
            Some(Ok(data)) => VolumeResponse::Data { data },
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn write(&self, _ino: u64, fh: u64, offset: u64, data: &[u8]) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                return Err(e.raw_os_error().unwrap_or(libc::EIO));
            }

            match file.write(data) {
                Ok(n) => Ok(n as u32),
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        }) {
            Some(Ok(size)) => VolumeResponse::Written { size },
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn release(&self, _ino: u64, fh: u64) -> VolumeResponse {
        self.handles.remove(fh);
        VolumeResponse::Ok
    }

    fn flush(&self, _ino: u64, fh: u64) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            file.sync_all()
                .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
        }) {
            Some(Ok(())) => VolumeResponse::Ok,
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn fsync(&self, _ino: u64, fh: u64, datasync: bool) -> VolumeResponse {
        match self.handles.with_file(fh, |file| {
            let result = if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            };
            result.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
        }) {
            Some(Ok(())) => VolumeResponse::Ok,
            Some(Err(errno)) => VolumeResponse::error(errno),
            None => VolumeResponse::bad_fd(),
        }
    }

    fn unlink(&self, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        if let Err(e) = fs::remove_file(&path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        self.inodes.remove_entry(parent, OsStr::new(name));
        VolumeResponse::Ok
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let newparent_path = match self.inodes.resolve_path(newparent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let old_path = parent_path.join(name);
        let new_path = newparent_path.join(newname);

        if let Err(e) = fs::rename(&old_path, &new_path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        self.inodes
            .rename_entry(parent, OsStr::new(name), newparent, OsStr::new(newname));
        VolumeResponse::Ok
    }

    fn symlink(&self, parent: u64, name: &str, target: &str) -> VolumeResponse {
        let parent_path = match self.inodes.resolve_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let link_path = parent_path.join(name);

        if let Err(e) = std::os::unix::fs::symlink(target, &link_path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        let metadata = match fs::symlink_metadata(&link_path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let ino = self.inodes.get_or_create_ino(parent, OsStr::new(name));
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn readlink(&self, ino: u64) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        match fs::read_link(&path) {
            Ok(target) => VolumeResponse::Symlink {
                target: target.to_string_lossy().to_string(),
            },
            Err(e) => VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn link(&self, ino: u64, newparent: u64, newname: &str) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let newparent_path = match self.inodes.resolve_path(newparent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let new_path = newparent_path.join(newname);

        if let Err(e) = fs::hard_link(&path, &new_path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        // Reuse same inode for hard link (canonical path stays unchanged).
        self.inodes
            .add_hard_link(newparent, OsStr::new(newname), ino);

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn access(&self, ino: u64, mask: u32) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        use std::ffi::CString;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        let result = unsafe { libc::access(c_path.as_ptr(), mask as i32) };

        if result == 0 {
            VolumeResponse::Ok
        } else {
            VolumeResponse::error(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EACCES),
            )
        }
    }

    fn statfs(&self, ino: u64) -> VolumeResponse {
        let path = match self.inodes.resolve_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        use std::ffi::CString;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return VolumeResponse::error(libc::EINVAL),
        };

        let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::statfs(c_path.as_ptr(), &mut statfs) };

        if result != 0 {
            return VolumeResponse::error(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            );
        }

        VolumeResponse::Statfs {
            blocks: statfs.f_blocks as u64,
            bfree: statfs.f_bfree as u64,
            bavail: statfs.f_bavail as u64,
            files: statfs.f_files as u64,
            ffree: statfs.f_ffree as u64,
            bsize: statfs.f_bsize as u32,
            namelen: 255, // statfs.f_namelen varies by platform
            frsize: statfs.f_bsize as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_fs_creation() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());
        assert_eq!(fs.root_path(), &dir.path().to_path_buf());
    }

    #[test]
    fn test_passthrough_getattr_root() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());

        let resp = fs.getattr(1); // Root inode
        match resp {
            VolumeResponse::Attr { attr, .. } => {
                assert_eq!(attr.ino, 1);
                assert!(attr.is_dir());
            }
            _ => panic!("Expected Attr response"),
        }
    }

    #[test]
    fn test_passthrough_lookup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let fs = PassthroughFs::new(dir.path());

        let resp = fs.lookup(1, "test.txt");
        match resp {
            VolumeResponse::Entry { attr, .. } => {
                assert!(attr.is_file());
                assert_eq!(attr.size, 5);
            }
            _ => panic!("Expected Entry response"),
        }
    }

    #[test]
    fn test_passthrough_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());

        // Create file
        let resp = fs.create(1, "test.txt", 0o644, 0);
        let fh = match resp {
            VolumeResponse::Created { fh, .. } => fh,
            _ => panic!("Expected Created response"),
        };

        // Write
        let resp = fs.write(0, fh, 0, b"hello");
        assert!(matches!(resp, VolumeResponse::Written { size: 5 }));

        // Read back
        let resp = fs.read(0, fh, 0, 100);
        match resp {
            VolumeResponse::Data { data } => {
                assert_eq!(data, b"hello");
            }
            _ => panic!("Expected Data response"),
        }
    }

    #[test]
    fn test_passthrough_setattr_chown_errno() {
        if nix::unistd::Uid::effective().is_root() {
            // Root can chown successfully, so skip to avoid false positives.
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());
        std::fs::write(dir.path().join("file.txt"), "data").unwrap();

        let lookup = fs.lookup(1, "file.txt");
        let ino = match lookup {
            VolumeResponse::Entry { attr, .. } => attr.ino,
            _ => panic!("Expected Entry response"),
        };

        let resp = fs.setattr(
            ino,
            None,
            Some(0), // Force a chown we do not have permission for
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(
            resp.errno(),
            Some(libc::EPERM),
            "chown failure should propagate EPERM"
        );
    }

    #[test]
    fn test_passthrough_rename_updates_child_inodes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let subdir = root.join("dir");
        let file_path = subdir.join("file.txt");

        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(&file_path, "hello").unwrap();

        let fs = PassthroughFs::new(root);

        let dir_ino = match fs.lookup(1, "dir") {
            VolumeResponse::Entry { attr, .. } => attr.ino,
            _ => panic!("Expected Entry for dir"),
        };

        let file_ino = match fs.lookup(dir_ino, "file.txt") {
            VolumeResponse::Entry { attr, .. } => attr.ino,
            _ => panic!("Expected Entry for file"),
        };

        let rename_resp = fs.rename(1, "dir", 1, "renamed");
        assert!(rename_resp.is_ok());

        let getattr_resp = fs.getattr(file_ino);
        match getattr_resp {
            VolumeResponse::Attr { attr, .. } => assert_eq!(attr.size, 5),
            VolumeResponse::Error { errno } => {
                panic!("Expected Attr after rename, got errno {}", errno)
            }
            _ => panic!("Unexpected response after rename"),
        }
    }

    #[test]
    fn test_passthrough_setattr_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let fs = PassthroughFs::new(dir.path());
        std::fs::write(dir.path().join("file.txt"), "data").unwrap();

        let lookup = fs.lookup(1, "file.txt");
        let ino = match lookup {
            VolumeResponse::Entry { attr, .. } => attr.ino,
            _ => panic!("Expected Entry response"),
        };

        // Set specific timestamps: 2020-01-01 00:00:00 UTC
        let timestamp_secs: i64 = 1577836800;
        let timestamp_nsecs: u32 = 123456789;

        let resp = fs.setattr(
            ino,
            None,
            None,
            None,
            None,
            Some(timestamp_secs),
            Some(timestamp_nsecs),
            Some(timestamp_secs),
            Some(timestamp_nsecs),
        );

        match resp {
            VolumeResponse::Attr { attr, .. } => {
                assert_eq!(attr.atime_secs, timestamp_secs, "atime_secs mismatch");
                assert_eq!(attr.mtime_secs, timestamp_secs, "mtime_secs mismatch");
                // nsecs may be rounded by filesystem, just check they're set
                assert!(attr.atime_nsecs > 0, "atime_nsecs should be set");
                assert!(attr.mtime_nsecs > 0, "mtime_nsecs should be set");
            }
            VolumeResponse::Error { errno } => {
                panic!("setattr failed with errno {}", errno)
            }
            _ => panic!("Unexpected response"),
        }
    }
}
