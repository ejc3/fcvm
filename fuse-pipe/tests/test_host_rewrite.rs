//! A file the backing directory changes while the mount holds it open.
//!
//! The kernel drops a file's cached pages when a refreshed size or mtime differs from its own
//! (FUSE_AUTO_INVAL_DATA), and with the writeback cache it ignores both. A host that rewrites a
//! file in place at the same length and puts the mtime back changes neither; the client finds
//! the change from the ctime and tells the kernel (`client/host_change.rs`). These run the client
//! against a local server, with each of the three ways fc-agent mounts a volume. They need
//! neither root nor a VM.

mod common;

use std::fs::{self, File};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::time::{Duration, SystemTime};

use common::{cleanup, unique_paths, FuseMount};
use fuse_pipe::MountSettings;

/// Longer than the fixture server's 1 s attribute timeout.
const PAST_ATTR_TIMEOUT: Duration = Duration::from_millis(1500);

/// The three mounts fc-agent makes: a read-only map; a read-write map with the writeback cache
/// (the default); a read-write map in a VM booted with FCVM_NO_WRITEBACK_CACHE=1.
const MOUNTS: [(&str, MountSettings); 3] = [
    ("read-only", MountSettings::for_volume(true, false)),
    ("read-write", MountSettings::for_volume(false, false)),
    (
        "read-write without the writeback cache",
        MountSettings::for_volume(false, true),
    ),
];

/// `len` bytes that name their version.
fn body(version: &str, len: usize) -> Vec<u8> {
    let mut text = format!("{version}:").into_bytes();
    text.resize(len, b'.');
    text
}

/// A fixed mtime with a nanosecond part, so putting it back has to be exact.
fn old_mtime() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(1_790_911_026, 257_640_420)
}

/// Rewrite `path` in place with `content` of its own length, without truncating, and put its
/// mtime back. Returns the ctime before and after.
fn rewrite_keeping_size_and_mtime(path: &Path, content: &[u8]) -> ((i64, i64), (i64, i64)) {
    let before = fs::metadata(path).expect("stat the backing file");
    assert_eq!(before.len(), content.len() as u64);
    let file = File::options()
        .write(true)
        .open(path)
        .expect("open the backing file for writing");
    file.write_all_at(content, 0)
        .expect("rewrite the backing file in place");
    file.set_modified(before.modified().expect("mtime"))
        .expect("put the mtime back");
    drop(file);
    let after = fs::metadata(path).expect("stat the rewritten backing file");
    assert_eq!(after.ino(), before.ino(), "the rewrite must keep the inode");
    assert_eq!(after.len(), before.len(), "the rewrite must keep the size");
    assert_eq!(
        (after.mtime(), after.mtime_nsec()),
        (before.mtime(), before.mtime_nsec()),
        "the rewrite must keep the mtime"
    );
    let ctimes = (
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec()),
    );
    assert_ne!(ctimes.0, ctimes.1, "the backing filesystem kept the ctime");
    ctimes
}

/// A read-only shared mapping of the first `len` bytes of `file`.
struct Mapping {
    addr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(file: &File, len: usize) -> Self {
        use std::os::fd::AsRawFd;
        // SAFETY: a fresh mapping of an open file; unmapped in Drop.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            addr,
            libc::MAP_FAILED,
            "mmap: {}",
            std::io::Error::last_os_error()
        );
        Self { addr, len }
    }

    fn bytes(&self) -> Vec<u8> {
        // SAFETY: the mapping is `len` bytes long and lives as long as self.
        unsafe { std::slice::from_raw_parts(self.addr as *const u8, self.len) }.to_vec()
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly what new() mapped.
        unsafe { libc::munmap(self.addr, self.len) };
    }
}

fn pread(file: &File, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, 0).expect("pread through the mount");
    buf.truncate(n);
    buf
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A descriptor held open across a rewrite that keeps the size and the mtime reads the new
/// bytes once the attribute timeout has passed. A mapping of the file keeps the old pages until
/// something asks for the file's attributes (here the read), and shows the new bytes after it.
/// A new open reads the new bytes.
///
/// RED BEFORE THE FIX: every mount read "v1" from the held descriptor 1.5 s after the rewrite,
/// because neither the size nor the mtime the kernel compares had changed, and with the writeback
/// cache it compares nothing. The read-only mount also read "v1" through a new open: it sends no
/// OPEN, so an open no longer drops the pages.
#[test]
fn a_held_descriptor_reads_a_rewrite_that_keeps_size_and_mtime() {
    let mut wrong = Vec::new();
    for (label, settings) in MOUNTS {
        let (data_dir, mount_dir) = unique_paths("fuse-held-rewrite");
        let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, settings);
        let backing = data_dir.join("same.txt");
        let mounted = fuse.mount_path().join("same.txt");

        fs::write(&backing, body("v1", 64)).expect("write v1");
        File::options()
            .write(true)
            .open(&backing)
            .and_then(|file| file.set_modified(old_mtime()))
            .expect("set v1's mtime");

        let held = File::open(&mounted).expect("open through the mount");
        let first = pread(&held, 64);
        let mapping = Mapping::new(&held, 64);
        let first_mapped = mapping.bytes();

        let (before, after) = rewrite_keeping_size_and_mtime(&backing, &body("v2", 64));
        std::thread::sleep(PAST_ATTR_TIMEOUT);

        let mapped_before_read = mapping.bytes();
        let held_read = pread(&held, 64);
        let mapped_after_read = mapping.bytes();
        let new_open = fs::read(&mounted).expect("read through a new open");
        let (v1, v2) = (body("v1", 64), body("v2", 64));
        for (what, got, want) in [
            ("the first read through the held descriptor", &first, &v1),
            ("the mapping before the rewrite", &first_mapped, &v1),
            (
                "the mapping before anything asked for the file's attributes",
                &mapped_before_read,
                &v1,
            ),
            ("the held descriptor", &held_read, &v2),
            (
                "the mapping after the held descriptor's read",
                &mapped_after_read,
                &v2,
            ),
            ("a new open", &new_open, &v2),
        ] {
            if got != want {
                wrong.push(format!(
                    "{label}: {what} read {:?}, expected {:?}",
                    show(got),
                    show(want)
                ));
            }
        }
        eprintln!("{label}: ctime {before:?} -> {after:?}");

        drop(mapping);
        drop(held);
        drop(fuse);
        cleanup(&data_dir, &mount_dir);
    }
    assert!(
        wrong.is_empty(),
        "{PAST_ATTR_TIMEOUT:?} after the backing file was rewritten in place with its size and \
         mtime kept:\n{}",
        wrong.join("\n")
    );
}

/// A held descriptor reads a file the host grew in place whole on the first read past the
/// attribute timeout. The kernel drops the pages itself when the size it is given changed; a
/// notification as well would make it discard the reply that carries the new size, and that
/// read would stop at the old one. With the writeback cache the kernel keeps its own size
/// whatever it is told (#1041), so that mount is not run here.
#[test]
fn a_held_descriptor_reads_a_file_grown_in_place_whole_on_the_first_read() {
    for (label, settings) in MOUNTS {
        if settings.writeback_cache() {
            continue;
        }
        let (data_dir, mount_dir) = unique_paths("fuse-held-grow");
        let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, settings);
        let backing = data_dir.join("grows.txt");
        let mounted = fuse.mount_path().join("grows.txt");

        fs::write(&backing, body("g1", 8)).expect("write g1");
        let held = File::open(&mounted).expect("open through the mount");
        assert_eq!(show(&pread(&held, 256)), show(&body("g1", 8)), "{label}");

        File::options()
            .write(true)
            .open(&backing)
            .and_then(|file| file.write_all_at(&body("g2", 92), 0))
            .expect("rewrite the backing file longer in place");
        std::thread::sleep(PAST_ATTR_TIMEOUT);

        assert_eq!(
            show(&pread(&held, 256)),
            show(&body("g2", 92)),
            "{label}: the first read through the held descriptor after the attribute timeout"
        );

        drop(held);
        drop(fuse);
        cleanup(&data_dir, &mount_dir);
    }
}

/// A file nobody changed keeps its cached pages through a held descriptor: the refresh after the
/// attribute timeout happens (one GETATTR) and no READ follows it.
#[test]
fn a_held_descriptor_on_an_unchanged_file_reads_from_the_page_cache() {
    for (label, settings) in MOUNTS {
        let (data_dir, mount_dir) = unique_paths("fuse-held-unchanged");
        let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, settings);
        let backing = data_dir.join("still.txt");
        let mounted = fuse.mount_path().join("still.txt");

        fs::write(&backing, body("s1", 4096)).expect("write s1");
        let held = File::open(&mounted).expect("open through the mount");
        assert_eq!(
            show(&pread(&held, 4096)),
            show(&body("s1", 4096)),
            "{label}"
        );
        std::thread::sleep(PAST_ATTR_TIMEOUT);

        let (getattrs, reads) = (
            fuse.server_requests("getattr"),
            fuse.server_requests("read"),
        );
        assert_eq!(
            show(&pread(&held, 4096)),
            show(&body("s1", 4096)),
            "{label}"
        );
        assert!(
            fuse.server_requests("getattr") > getattrs,
            "{label}: the read past the attribute timeout refreshed nothing, so this test \
             proves nothing"
        );
        assert_eq!(
            fuse.server_requests("read"),
            reads,
            "{label}: READ requests for a file nobody changed"
        );

        drop(held);
        drop(fuse);
        cleanup(&data_dir, &mount_dir);
    }
}

/// With the writeback cache the mount's own writes move the host file's ctime. They must not
/// count as the host's: a file the guest wrote keeps its cached pages.
#[test]
fn own_writes_keep_their_cached_pages_with_the_writeback_cache() {
    let (data_dir, mount_dir) = unique_paths("fuse-own-writes");
    let fuse = FuseMount::with_settings(
        &data_dir,
        &mount_dir,
        1,
        MountSettings::for_volume(false, false),
    );
    let mounted = fuse.mount_path().join("written.txt");

    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&mounted)
        .expect("create through the mount");
    // Two rounds, so the second write lands on a file the first refresh already recorded.
    for round in ["w1", "w2"] {
        file.write_all_at(&body(round, 4096), 0)
            .expect("write through the mount");
        file.sync_all().expect("fsync through the mount");
        std::thread::sleep(PAST_ATTR_TIMEOUT);

        let (writes, getattrs, reads) = (
            fuse.server_requests("write"),
            fuse.server_requests("getattr"),
            fuse.server_requests("read"),
        );
        assert!(writes > 0, "{round}: the write never reached the server");
        assert_eq!(
            show(&pread(&file, 4096)),
            show(&body(round, 4096)),
            "{round}"
        );
        assert!(
            fuse.server_requests("getattr") > getattrs,
            "{round}: the read past the attribute timeout refreshed nothing, so this test \
             proves nothing"
        );
        assert_eq!(
            fuse.server_requests("read"),
            reads,
            "{round}: READ requests after the mount's own write"
        );
    }

    drop(file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}
