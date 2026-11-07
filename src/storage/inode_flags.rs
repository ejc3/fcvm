//! Inode flags (FS_IOC_GETFLAGS / FS_IOC_SETFLAGS), for btrfs nodatacow.
//!
//! A nodatacow file (`chattr +C`) is written in place and never compressed, whatever the
//! mount's `compress-force` says: btrfs compresses only data it writes copy-on-write. The
//! flag can change only while a file is empty, so a file gets it at creation, from its
//! directory or from [`make_nodatacow`]. btrfs refuses a clone (FICLONE) between a nodatacow
//! file and a copy-on-write one with EINVAL; [`match_nodatacow`] gives an empty clone
//! target the source's flag so the clone can be retried.

use std::fs::File;
use std::os::fd::AsRawFd;
use tracing::debug;

/// `FS_NOCOW_FL` from linux/fs.h, which libc does not export: the file is written in place
/// instead of copy-on-write (btrfs nodatacow, `chattr +C`).
pub(crate) const FS_NOCOW_FL: libc::c_int = 0x0080_0000;

/// The inode flags of `file` (FS_IOC_GETFLAGS), or `None` on a filesystem without them.
pub(crate) fn inode_flags(file: &File) -> Option<libc::c_int> {
    let mut flags: libc::c_int = 0;
    // SAFETY: FS_IOC_GETFLAGS writes one int to `flags`, which outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS as _, &mut flags) };
    (rc == 0).then_some(flags)
}

/// Set the inode flags of `file` (FS_IOC_SETFLAGS).
pub(crate) fn set_inode_flags(file: &File, flags: libc::c_int) -> std::io::Result<()> {
    // SAFETY: FS_IOC_SETFLAGS reads one int from `flags`, which outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS as _, &flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether `file` is nodatacow. False on a filesystem without inode flags.
pub(crate) fn is_nodatacow(file: &File) -> bool {
    inode_flags(file).is_some_and(|flags| flags & FS_NOCOW_FL != 0)
}

/// Mark the empty `file` nodatacow, and report whether it is now. A filesystem without the
/// flag (tmpfs, ext4, a FUSE mount) leaves the file as it is and returns false.
pub(crate) fn make_nodatacow(file: &File) -> bool {
    let Some(flags) = inode_flags(file) else {
        return false;
    };
    if flags & FS_NOCOW_FL != 0 {
        return true;
    }
    match set_inode_flags(file, flags | FS_NOCOW_FL) {
        Ok(()) => is_nodatacow(file),
        Err(error) => {
            debug!(%error, "the filesystem refused the nodatacow flag");
            false
        }
    }
}

/// Set or clear the nodatacow flag of the empty `dst` to match `src`, and report whether
/// it changed. The flag can only change while a file is empty, and a temp inherits it
/// from a directory marked `chattr +C`. Callers use this only after btrfs refused a clone,
/// so a byte copy (from another filesystem, for one) keeps the flag the directory gives
/// its files.
pub(crate) fn match_nodatacow(src: &File, dst: &File) -> bool {
    let (Some(src_flags), Some(dst_flags)) = (inode_flags(src), inode_flags(dst)) else {
        return false;
    };
    let wanted = (dst_flags & !FS_NOCOW_FL) | (src_flags & FS_NOCOW_FL);
    if wanted == dst_flags {
        return false;
    }
    match set_inode_flags(dst, wanted) {
        Ok(()) => true,
        Err(error) => {
            debug!(%error, "could not match a clone target's nodatacow flag to its source");
            false
        }
    }
}
