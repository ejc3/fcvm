//! Ask btrfs to drop deleted subvolumes when fcvm deletes them, not at the next tick of the
//! transaction kthread.
//!
//! `btrfs subvolume delete` only detaches a subvolume. Its tree and every extent reference
//! it holds are dropped later by the filesystem's cleaner thread, and the cleaner runs when
//! the transaction kthread wakes it: once a commit interval, 30 s by default. A snapshot
//! create deletes the generation it replaces (`<tag>.old` and its `disk/`), so on a host
//! that sleeps VMs all the time the cleaner finds every generation deleted in the last 30 s
//! at once. Each generation's subvolume holds a reference on every extent of its memory
//! file, because the diff written into the next generation copied every leaf of the file's
//! extent items: in `potatobench burst` at 8 wakes and 8 sleeps a trial, a dead generation
//! cost the cleaner 6,008 data reference drops at the median and its `disk/` 1,162, about
//! 4 million in 70 trials (#2266).
//!
//! btrfs reserves metadata for every pending delayed reference head, so a pass that drops
//! a few hundred generations holds the metadata space at its reservation ceiling for two
//! to four seconds (bytes_may_use 8.7 GiB against 1 GiB of metadata on a 165 GiB store).
//! Every other reservation then waits on a ticket behind the cleaner's: a restore's
//! `mkdir` of its disk directory, its extent count's xattr and its reflink all waited 0.6
//! to 1.3 s together, ending at the same instant, about one trial in 30 to 150.
//!
//! [`drop_deleted_subvolumes`] wakes the cleaner when a deleted subvolume is queued for it.
//! The same work then runs in pieces of one generation, right after each deletion, instead
//! of all at once every 30 s.

use std::os::fd::AsRawFd;
use std::path::Path;

/// `BTRFS_IOC_SUBVOL_SYNC_WAIT`: `_IOW(0x94, 65, struct btrfs_ioctl_subvol_wait)`, Linux 6.8.
const BTRFS_IOC_SUBVOL_SYNC_WAIT: libc::c_ulong = 0x4010_9441;
/// The mode of [`BTRFS_IOC_SUBVOL_SYNC_WAIT`] that counts the deleted subvolumes the cleaner
/// has not dropped yet (`BTRFS_SUBVOL_SYNC_COUNT`).
const BTRFS_SUBVOL_SYNC_COUNT: u32 = 2;
/// `BTRFS_IOC_START_SYNC`: `_IOR(0x94, 24, __u64)`. Asks the transaction kthread to commit
/// and returns without waiting for the commit; the kthread wakes the cleaner on every pass.
const BTRFS_IOC_START_SYNC: libc::c_ulong = 0x8008_9418;

/// `struct btrfs_ioctl_subvol_wait` (include/uapi/linux/btrfs.h).
#[repr(C)]
#[derive(Default)]
struct SubvolWait {
    subvolid: u64,
    mode: u32,
    count: u32,
}

/// How many deleted subvolumes on the filesystem holding `dir` wait for the cleaner. None
/// where the filesystem cannot say: not btrfs, or a kernel older than 6.8.
fn queued(dir: &std::fs::File) -> Option<u32> {
    let mut args = SubvolWait {
        mode: BTRFS_SUBVOL_SYNC_COUNT,
        ..Default::default()
    };
    // SAFETY: the descriptor is open and `args` is a live, writable struct of the layout
    // the request number encodes.
    let rc = unsafe {
        libc::ioctl(
            dir.as_raw_fd(),
            BTRFS_IOC_SUBVOL_SYNC_WAIT as _,
            &mut args as *mut SubvolWait,
        )
    };
    (rc == 0).then_some(args.count)
}

/// Wake the btrfs cleaner of the filesystem holding `path` when deleted subvolumes wait for
/// it, so it drops them now rather than at the transaction kthread's next tick. Returns how
/// many were waiting when it asked; None when none were or it could not ask (not btrfs, an
/// older kernel, a path that cannot be opened). Never fails: a subvolume it cannot hurry is
/// still dropped at the next tick.
///
/// Asking costs a commit of the running transaction, which the commits a snapshot create
/// makes anyway usually join, and can wait for a commit already in progress, so it runs off
/// the async runtime ([`drop_deleted_subvolumes_async`]).
pub fn drop_deleted_subvolumes(path: &Path) -> Option<u32> {
    let dir = std::fs::File::open(path).ok()?;
    let waiting = queued(&dir).filter(|&count| count > 0)?;
    let mut transid = 0u64;
    // SAFETY: the descriptor is open and `transid` is the u64 the request writes.
    let rc = unsafe {
        libc::ioctl(
            dir.as_raw_fd(),
            BTRFS_IOC_START_SYNC as _,
            &mut transid as *mut u64,
        )
    };
    (rc == 0).then_some(waiting)
}

/// [`drop_deleted_subvolumes`] on a blocking thread.
pub async fn drop_deleted_subvolumes_async(path: &Path) {
    let path = path.to_path_buf();
    let started = std::time::Instant::now();
    let asked = tokio::task::spawn_blocking({
        let path = path.clone();
        move || drop_deleted_subvolumes(&path)
    })
    .await
    .ok()
    .flatten();
    if let Some(waiting) = asked {
        tracing::debug!(path = %path.display(), waiting, elapsed_ms = started.elapsed().as_millis() as u64,
            "asked btrfs to drop the deleted subvolumes now");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `_IOC(dir, type, nr, size)` as asm-generic/ioctl.h encodes it, which arm64 and x86 use.
    fn ioc(dir: libc::c_ulong, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
        (dir << 30)
            | ((size as libc::c_ulong) << 16)
            | ((ty as libc::c_ulong) << 8)
            | nr as libc::c_ulong
    }

    /// The request numbers are the kernel's: a wrong size changes the number itself, and the
    /// kernel then answers ENOTTY, which [`drop_deleted_subvolumes`] takes for "not btrfs"
    /// and never hurries the cleaner.
    #[test]
    fn the_cleaner_requests_have_the_kernels_numbers() {
        const WRITE: libc::c_ulong = 1;
        const READ: libc::c_ulong = 2;
        assert_eq!(std::mem::size_of::<SubvolWait>(), 16);
        assert_eq!(std::mem::offset_of!(SubvolWait, mode), 8);
        assert_eq!(std::mem::offset_of!(SubvolWait, count), 12);
        assert_eq!(
            BTRFS_IOC_SUBVOL_SYNC_WAIT,
            ioc(WRITE, 0x94, 65, std::mem::size_of::<SubvolWait>())
        );
        assert_eq!(BTRFS_IOC_START_SYNC, ioc(READ, 0x94, 24, 8));
    }

    /// A path that cannot be opened asks nothing and fails nothing.
    #[test]
    fn a_missing_path_asks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(drop_deleted_subvolumes(&dir.path().join("gone")), None);
    }

    /// The body of `name` in `source`, up to the closing brace at the start of a line.
    fn body<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(name)
            .unwrap_or_else(|| panic!("{name} is not in the source"));
        let rest = &source[start..];
        &rest[..rest.find("\n}\n").expect("the end of the function")]
    }

    /// Every deletion of a snapshot subvolume asks the cleaner to drop it now (#2266). A
    /// generation the deletion left mapped by a running VM is queued for the cleaner only when
    /// that VM's VMM exits, so a VM's teardown asks too, after its processes are gone. Without
    /// either, the generations deleted in 30 s are dropped in one pass that stalls every
    /// restore on the host for up to 1.3 s; the stall is a kernel timing a unit test cannot
    /// produce on every run, so the two call sites are held here.
    #[test]
    fn deleting_a_generation_asks_the_cleaner_to_drop_it_now() {
        let disk = include_str!("disk.rs");
        let remove = body(disk, "pub async fn remove_snapshot_dir(");
        let deleted = remove
            .find("delete_subvolume(dir).await")
            .expect("remove_snapshot_dir deletes the snapshot's subvolume");
        let asked = remove
            .find("crate::storage::cleaner::drop_deleted_subvolumes_async(")
            .expect("remove_snapshot_dir asks the cleaner to drop what it deleted");
        assert!(asked > deleted, "the cleaner is asked after the delete");

        let common = include_str!("../commands/common.rs");
        let teardown = body(common, "async fn cleanup_vm_inner(");
        let reaped = teardown
            .find("--- Phase 3")
            .expect("the teardown's processes are gone before its network phase");
        let asked = teardown
            .find("crate::storage::cleaner::drop_deleted_subvolumes_async(")
            .expect("a VM's teardown asks the cleaner to drop the generation its VMM held");
        assert!(
            asked > reaped,
            "the cleaner is asked once the VMM has exited"
        );
    }
}
