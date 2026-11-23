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

/// The extended attribute that records a file's extent count, as
/// `<identity> <limit> <count>` ([`recorded_extents_in_files_up_to`]).
const EXTENTS_XATTR: &std::ffi::CStr = c"user.fcvm.extents";

/// The identity a recorded count was taken for: device, inode, length and modification time.
fn extents_identity(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}.{:09}",
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec()
    )
}

/// What a recorded `<identity> <limit> <count>` says about the count up to `limit`, when its
/// identity is `identity`: the answer, or None when it does not say.
fn recorded_answer(recorded: &str, identity: &str, limit: u64) -> Option<u64> {
    let mut fields = recorded.split(' ');
    if fields.next()? != identity {
        return None;
    }
    let recorded_limit: u64 = fields.next()?.parse().ok()?;
    let count: u64 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    if count <= recorded_limit {
        // Exact.
        Some(count.min(limit.saturating_add(1)))
    } else if limit <= recorded_limit {
        // More than a limit at least as high as this one.
        Some(limit + 1)
    } else {
        None
    }
}

fn read_recorded(file: &File) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            EXTENTS_XATTR.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    (n > 0)
        .then(|| String::from_utf8(buf[..n as usize].to_vec()).ok())
        .flatten()
}

/// [`extents_in_files_up_to`] for files that are never written again, a published snapshot's
/// disks: each file's count is recorded on the file the first time it is counted and read back
/// while the file keeps the identity it was counted with. FIEMAP reports whether each extent is
/// shared, and on btrfs that walks the extent's back references, one per clone that reflinked
/// it: with 1000 clones of a snapshot alive, counting its 854-extent disk was most of the CPU of
/// a clone's disk step. A file whose count cannot be recorded (another user's snapshot, a
/// filesystem without user extended attributes) is counted every time, as before.
pub fn recorded_extents_in_files_up_to(
    paths: &[std::path::PathBuf],
    limit: u64,
) -> std::io::Result<u64> {
    let mut total = 0u64;
    for path in paths {
        let file = File::open(path)?;
        let identity = extents_identity(&file.metadata()?);
        let remaining = limit - total;
        let count = match read_recorded(&file)
            .and_then(|recorded| recorded_answer(&recorded, &identity, remaining))
        {
            Some(count) => count,
            None => {
                let count = extents_up_to(&file, remaining)?;
                let value = format!("{identity} {remaining} {count}");
                let rc = unsafe {
                    libc::fsetxattr(
                        file.as_raw_fd(),
                        EXTENTS_XATTR.as_ptr(),
                        value.as_ptr().cast(),
                        value.len(),
                        0,
                    )
                };
                if rc != 0 {
                    tracing::debug!(path = %path.display(), error = %std::io::Error::last_os_error(),
                        "cannot record the extent count on the file; it is counted again next time");
                }
                count
            }
        };
        total += count;
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

    /// The count stops one past the limit. The files are made in the crate's directory, on
    /// the build's filesystem: tmpfs does not answer FIEMAP.
    #[test]
    fn extents_are_counted_up_to_one_past_the_limit() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let file = File::open(three_extents(dir.path(), "a")).unwrap();
        assert_eq!(extents_up_to(&file, 10).unwrap(), 3);
        assert_eq!(extents_up_to(&file, 3).unwrap(), 3);
        assert_eq!(extents_up_to(&file, 1).unwrap(), 2);
    }

    /// A recorded count answers for the file while its identity holds, and only for limits it
    /// can answer; any other identity is counted again.
    #[test]
    fn a_recorded_count_answers_only_for_its_identity_and_limit() {
        assert_eq!(recorded_answer("i 100 7", "i", 10), Some(7));
        assert_eq!(recorded_answer("i 100 7", "i", 5), Some(6));
        assert_eq!(recorded_answer("i 100 101", "i", 50), Some(51));
        assert_eq!(recorded_answer("i 100 101", "i", 100), Some(101));
        assert_eq!(recorded_answer("i 100 101", "i", 200), None);
        assert_eq!(recorded_answer("j 100 7", "i", 10), None);
        assert_eq!(recorded_answer("i 100 7 x", "i", 10), None);
        assert_eq!(recorded_answer("garbage", "i", 10), None);
    }

    /// Writing a file changes its identity, and it is counted again.
    #[test]
    fn a_written_file_is_counted_again() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = three_extents(dir.path(), "disk.raw");
        let paths = [path.clone()];
        assert_eq!(recorded_extents_in_files_up_to(&paths, 10).unwrap(), 3);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // A block past a hole: a new extent on every filesystem (an appended block can join
        // the last extent on ext4 and xfs, which CI's checkout is on).
        let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::Start(1 << 20)).unwrap();
        writer.write_all(&[1; 4096]).unwrap();
        writer.sync_all().unwrap();
        assert_eq!(recorded_extents_in_files_up_to(&paths, 10).unwrap(), 4);
    }

    /// Several files count together, up to one past the limit.
    #[test]
    fn extents_of_several_files_are_counted_together() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
