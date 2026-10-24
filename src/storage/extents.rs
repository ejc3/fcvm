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

/// One extent of a file: the bytes `[start, start + len)` of the file, and whether another
/// file shares its blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub start: u64,
    pub len: u64,
    pub shared: bool,
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
                len: extent.fe_length,
                shared: extent.fe_flags & FIEMAP_EXTENT_SHARED != 0,
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
