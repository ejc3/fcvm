//! Passthrough filesystem implementation.
//!
//! This maps FUSE operations directly to the local filesystem,
//! allowing a directory to be served over the network.

use super::handler::FilesystemHandler;
use crate::protocol::{file_type, DirEntry, FileAttr, VolumeResponse};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

/// Default attribute TTL in seconds.
const ATTR_TTL_SECS: u64 = 1;

/// Inode table mapping inode numbers to paths.
struct InodeTable {
    ino_to_path: RwLock<HashMap<u64, PathBuf>>,
    path_to_ino: RwLock<HashMap<PathBuf, u64>>,
    next_ino: AtomicU64,
    root_path: PathBuf,
}

impl InodeTable {
    fn new(root_path: PathBuf) -> Self {
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();

        // Root inode is always 1
        ino_to_path.insert(1, root_path.clone());
        path_to_ino.insert(root_path.clone(), 1);

        Self {
            ino_to_path: RwLock::new(ino_to_path),
            path_to_ino: RwLock::new(path_to_ino),
            next_ino: AtomicU64::new(2),
            root_path,
        }
    }

    fn get_path(&self, ino: u64) -> Option<PathBuf> {
        self.ino_to_path.read().unwrap().get(&ino).cloned()
    }

    fn get_or_create_ino(&self, path: &PathBuf) -> u64 {
        // Try read-only first
        if let Some(&ino) = self.path_to_ino.read().unwrap().get(path) {
            return ino;
        }

        // Need to create - acquire write locks
        let mut path_to_ino = self.path_to_ino.write().unwrap();
        let mut ino_to_path = self.ino_to_path.write().unwrap();

        // Double-check after acquiring write lock
        if let Some(&ino) = path_to_ino.get(path) {
            return ino;
        }

        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        path_to_ino.insert(path.clone(), ino);
        ino_to_path.insert(ino, path.clone());
        ino
    }

    fn remove_path(&self, path: &PathBuf) {
        let mut path_to_ino = self.path_to_ino.write().unwrap();
        let mut ino_to_path = self.ino_to_path.write().unwrap();

        if let Some(ino) = path_to_ino.remove(path) {
            ino_to_path.remove(&ino);
        }
    }

    fn rename_path(&self, old_path: &PathBuf, new_path: PathBuf) {
        let mut path_to_ino = self.path_to_ino.write().unwrap();
        let mut ino_to_path = self.ino_to_path.write().unwrap();

        if let Some(ino) = path_to_ino.remove(old_path) {
            ino_to_path.insert(ino, new_path.clone());
            path_to_ino.insert(new_path, ino);
        }
    }
}

/// File handle table.
struct HandleTable {
    handles: RwLock<HashMap<u64, Mutex<File>>>,
    next_fh: AtomicU64,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            handles: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn insert(&self, file: File) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.handles.write().unwrap().insert(fh, Mutex::new(file));
        fh
    }

    fn with_file<F, R>(&self, fh: u64, f: F) -> Option<R>
    where
        F: FnOnce(&mut File) -> R,
    {
        let handles = self.handles.read().unwrap();
        handles.get(&fh).map(|mutex| {
            let mut file = mutex.lock().unwrap();
            f(&mut file)
        })
    }

    fn remove(&self, fh: u64) -> Option<File> {
        self.handles
            .write()
            .unwrap()
            .remove(&fh)
            .map(|m| m.into_inner().unwrap())
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
        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO)),
        };

        let ino = self.inodes.get_or_create_ino(&path);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn getattr(&self, ino: u64) -> VolumeResponse {
        let path = match self.inodes.get_path(ino) {
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
        _atime_secs: Option<i64>,
        _atime_nsecs: Option<u32>,
        _mtime_secs: Option<i64>,
        _mtime_nsecs: Option<u32>,
    ) -> VolumeResponse {
        let path = match self.inodes.get_path(ino) {
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

        self.getattr(ino)
    }

    fn readdir(&self, ino: u64, offset: u64) -> VolumeResponse {
        let path = match self.inodes.get_path(ino) {
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
            } else if let Some(parent) = path.parent() {
                self.inodes.get_or_create_ino(&parent.to_path_buf())
            } else {
                1
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

            let entry_path = entry.path();
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let entry_ino = self.inodes.get_or_create_ino(&entry_path);
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
        let parent_path = match self.inodes.get_path(parent) {
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

        let ino = self.inodes.get_or_create_ino(&path);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn rmdir(&self, parent: u64, name: &str) -> VolumeResponse {
        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        if let Err(e) = fs::remove_dir(&path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        self.inodes.remove_path(&path);
        VolumeResponse::Ok
    }

    fn create(&self, parent: u64, name: &str, mode: u32, flags: u32) -> VolumeResponse {
        let parent_path = match self.inodes.get_path(parent) {
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

        let ino = self.inodes.get_or_create_ino(&path);
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
        let path = match self.inodes.get_path(ino) {
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
        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let path = parent_path.join(name);

        if let Err(e) = fs::remove_file(&path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        self.inodes.remove_path(&path);
        VolumeResponse::Ok
    }

    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> VolumeResponse {
        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let newparent_path = match self.inodes.get_path(newparent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let old_path = parent_path.join(name);
        let new_path = newparent_path.join(newname);

        if let Err(e) = fs::rename(&old_path, &new_path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        self.inodes.rename_path(&old_path, new_path);
        VolumeResponse::Ok
    }

    fn symlink(&self, parent: u64, name: &str, target: &str) -> VolumeResponse {
        let parent_path = match self.inodes.get_path(parent) {
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

        let ino = self.inodes.get_or_create_ino(&link_path);
        let attr = metadata_to_attr(ino, &metadata);

        VolumeResponse::Entry {
            attr,
            generation: 0,
            ttl_secs: self.attr_ttl_secs,
        }
    }

    fn readlink(&self, ino: u64) -> VolumeResponse {
        let path = match self.inodes.get_path(ino) {
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
        let path = match self.inodes.get_path(ino) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let newparent_path = match self.inodes.get_path(newparent) {
            Some(p) => p,
            None => return VolumeResponse::not_found(),
        };

        let new_path = newparent_path.join(newname);

        if let Err(e) = fs::hard_link(&path, &new_path) {
            return VolumeResponse::error(e.raw_os_error().unwrap_or(libc::EIO));
        }

        // Reuse same inode for hard link
        {
            let mut path_to_ino = self.inodes.path_to_ino.write().unwrap();
            path_to_ino.insert(new_path, ino);
        }

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
        let path = match self.inodes.get_path(ino) {
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
        let path = match self.inodes.get_path(ino) {
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
}
