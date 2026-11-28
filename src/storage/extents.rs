//! A file's extents, read with FS_IOC_FIEMAP.
//!
//! An in-place diff snapshot clones its parent's memory file and has Firecracker write the
//! dirty pages into the clone. Every extent the snapshot wrote is then new to the clone,
//! and every extent it did not write is still shared with the parent, so the extents
//! FIEMAP reports without `FIEMAP_EXTENT_SHARED` are what the snapshot wrote. btrfs reports a
//! dirty page over an existing extent as that extent until writeback, so every walk asks
//! FIEMAP to sync the file first (`FIEMAP_FLAG_SYNC`). Firecracker already syncs the memory
//! file before its snapshot create returns (`sync_snapshot_files`, default true), so that
//! sync normally finds nothing to write.

use std::fs::File;
use std::ops::ControlFlow;
use std::os::fd::AsRawFd;

/// `_IOWR('f', 11, struct fiemap)` from linux/fs.h.
const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 0x0000_0001;
const FIEMAP_EXTENT_LAST: u32 = 0x0000_0001;
const FIEMAP_EXTENT_ENCODED: u32 = 0x0000_0008;
const FIEMAP_EXTENT_SHARED: u32 = 0x0000_2000;

/// Extents asked for in one call.
const EXTENTS_PER_CALL: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
    fm_extents: [FiemapExtent; EXTENTS_PER_CALL],
}

/// One extent of a file: the bytes `[start, start + len)` of the file, where they are on the
/// device (`physical`, the filesystem's own address, comparable between files of one
/// filesystem), whether another file shares its blocks, and whether it is stored encoded
/// (compressed: `physical` is then where the whole compressed extent starts, whatever part of
/// it this is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub start: u64,
    pub physical: u64,
    pub len: u64,
    pub shared: bool,
    pub encoded: bool,
}

/// Call `visit` with each extent of `file` in file order, until it breaks or the extents end.
/// Holes are not extents. Errors when the filesystem cannot report extents.
pub fn for_each_extent(
    file: &File,
    mut visit: impl FnMut(Extent) -> ControlFlow<()>,
) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    let mut map = Box::new(Fiemap {
        fm_start: 0,
        fm_length: 0,
        fm_flags: 0,
        fm_mapped_extents: 0,
        fm_extent_count: 0,
        fm_reserved: 0,
        fm_extents: [FiemapExtent::default(); EXTENTS_PER_CALL],
    });
    let mut start = 0u64;
    while start < len {
        map.fm_start = start;
        map.fm_length = len - start;
        map.fm_flags = FIEMAP_FLAG_SYNC;
        map.fm_mapped_extents = 0;
        map.fm_extent_count = EXTENTS_PER_CALL as u32;
        // SAFETY: `map` is a struct fiemap with room for fm_extent_count extents, alive for the call.
        let rc = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                FS_IOC_FIEMAP as _,
                &mut *map as *mut Fiemap,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mapped = map.fm_mapped_extents as usize;
        if mapped == 0 {
            return Ok(());
        }
        for extent in &map.fm_extents[..mapped] {
            let end = extent.fe_logical + extent.fe_length;
            let flow = visit(Extent {
                start: extent.fe_logical,
                physical: extent.fe_physical,
                len: extent.fe_length,
                shared: extent.fe_flags & FIEMAP_EXTENT_SHARED != 0,
                encoded: extent.fe_flags & FIEMAP_EXTENT_ENCODED != 0,
            });
            if flow.is_break() || extent.fe_flags & FIEMAP_EXTENT_LAST != 0 {
                return Ok(());
            }
            start = end;
        }
    }
    Ok(())
}

/// The bytes of `file` in extents no other file shares, counted only until they reach `min`:
/// the exact count when it is below `min`, otherwise at least `min`.
pub fn unshared_bytes_up_to(file: &File, min: u64) -> std::io::Result<u64> {
    let mut unshared = 0u64;
    for_each_extent(file, |extent| {
        if !extent.shared {
            unshared += extent.len;
        }
        if unshared >= min {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    Ok(unshared)
}

/// How many extents `file` has, counted only until they pass `limit`: the exact count when it
/// is at most `limit`, otherwise `limit + 1`. Holes are not extents.
pub fn extents_up_to(file: &File, limit: u64) -> std::io::Result<u64> {
    let mut extents = 0u64;
    for_each_extent(file, |_| {
        extents += 1;
        if extents > limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    Ok(extents)
}

/// How many extents the files at `paths` have together, counted only until they pass
/// `limit`: the exact total when it is at most `limit`, otherwise `limit + 1`.
pub fn extents_in_files_up_to(paths: &[std::path::PathBuf], limit: u64) -> std::io::Result<u64> {
    let mut total = 0u64;
    for path in paths {
        total += extents_up_to(&File::open(path)?, limit - total)?;
        if total > limit {
            return Ok(limit + 1);
        }
    }
    Ok(total)
}

/// The ranges of `file` in extents no other file shares, with touching ranges joined.
pub fn unshared_ranges(file: &File) -> std::io::Result<Vec<std::ops::Range<u64>>> {
    let mut ranges: Vec<std::ops::Range<u64>> = Vec::new();
    for_each_extent(file, |extent| {
        if !extent.shared {
            let range = extent.start..extent.start + extent.len;
            match ranges.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => ranges.push(range),
            }
        }
        ControlFlow::Continue(())
    })?;
    Ok(ranges)
}

/// Whether `file`'s filesystem reports which extents are shared: true once any extent of
/// `file` is reported shared. Read only as far as the first shared extent. Asked of a fresh
/// clone, whose every extent is shared with its source; a source with no extents at all
/// (every byte a hole) gives a clone with none, which answers false.
pub fn reports_shared_extents(file: &File) -> std::io::Result<bool> {
    let mut shared = false;
    for_each_extent(file, |extent| {
        shared = extent.shared;
        if shared {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    Ok(shared)
}

/// The directory for tests that read the extents of files they create.
#[cfg(test)]
pub(crate) mod test_dir {
    use std::path::Path;

    /// Says why tests cannot read file extents in `dir`, or nothing when they can: it writes a
    /// file there and asks the filesystem for its extents.
    pub(crate) fn extent_mapping_unavailable(dir: &Path) -> Option<String> {
        let probe = dir.join("extent-probe");
        std::fs::write(&probe, [0u8; 4096]).unwrap();
        let asked = super::extents_up_to(&std::fs::File::open(&probe).unwrap(), 1);
        std::fs::remove_file(&probe).unwrap();
        let err = asked.err()?;
        Some(format!(
            "this test reads the extents (FIEMAP) of files it creates in {}, and that filesystem \
             does not map extents ({err}). tmpfs does not. Check the repository out on ext4, btrfs \
             or xfs",
            dir.display()
        ))
    }

    /// A temporary directory beside the crate, on the checkout's filesystem. A checkout on a
    /// filesystem that does not map extents fails the test here, with what to change, where the
    /// test's own extent call would fail with "Operation not supported".
    pub(crate) fn extent_mapped_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        if let Some(why) = extent_mapping_unavailable(dir.path()) {
            panic!("{why}");
        }
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    /// A file of three 4 KiB blocks with holes between them: three extents.
    fn three_extents(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = File::create(&path).unwrap();
        for block in 0..3u64 {
            file.seek(SeekFrom::Start(block * 64 * 1024)).unwrap();
            file.write_all(&[0xa5; 4096]).unwrap();
        }
        file.sync_all().unwrap();
        path
    }

    /// The preflight speaks for a filesystem without extent mapping and is silent for one with
    /// it. /dev/shm is tmpfs on every Linux host, and the three tests below need the crate's
    /// directory to pass the same check.
    #[test]
    fn the_extent_preflight_names_what_the_filesystem_lacks() {
        let tmpfs = tempfile::tempdir_in("/dev/shm").unwrap();
        let why = test_dir::extent_mapping_unavailable(tmpfs.path())
            .expect("tmpfs answered FIEMAP, so this check proves nothing here");
        for expected in ["FIEMAP", "tmpfs", "ext4, btrfs or xfs", "/dev/shm"] {
            assert!(why.contains(expected), "no {expected:?} in: {why}");
        }
        assert!(std::fs::read_dir(tmpfs.path()).unwrap().next().is_none());
    }

    /// The count stops one past the limit. The files are made in the crate's directory, on
    /// the build's filesystem: tmpfs does not answer FIEMAP.
    #[test]
    fn extents_are_counted_up_to_one_past_the_limit() {
        let dir = test_dir::extent_mapped_tempdir();
        let file = File::open(three_extents(dir.path(), "a")).unwrap();
        assert_eq!(extents_up_to(&file, 10).unwrap(), 3);
        assert_eq!(extents_up_to(&file, 3).unwrap(), 3);
        assert_eq!(extents_up_to(&file, 1).unwrap(), 2);
    }

    /// Several files count together, up to one past the limit.
    #[test]
    fn extents_of_several_files_are_counted_together() {
        let dir = test_dir::extent_mapped_tempdir();
        let paths = [
            three_extents(dir.path(), "a"),
            three_extents(dir.path(), "b"),
        ];
        assert_eq!(extents_in_files_up_to(&paths, 10).unwrap(), 6);
        assert_eq!(extents_in_files_up_to(&paths, 6).unwrap(), 6);
        assert_eq!(extents_in_files_up_to(&paths, 4).unwrap(), 5);
        assert_eq!(extents_in_files_up_to(&paths, 2).unwrap(), 3);
    }
}
