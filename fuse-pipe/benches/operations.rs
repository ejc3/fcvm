//! Single-operation latency benchmarks for FUSE passthrough.
//!
//! Tests individual FUSE operations to identify bottlenecks.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

use criterion::{criterion_group, criterion_main, Criterion};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

// Include the shared fixture module
#[path = "../tests/common/mod.rs"]
mod common;

use common::FuseMount;

/// Ensure ulimit is raised once at benchmark startup
static INIT: Once = Once::new();

fn ensure_ulimit() {
    INIT.call_once(|| {
        common::increase_ulimit();
    });
}

const FILE_SIZE: usize = 4096; // 4KB test file

/// Setup test files in the data directory
fn setup_test_files(dir: &PathBuf) {
    fs::create_dir_all(dir).unwrap();

    // Create test file
    let test_file = dir.join("test.dat");
    let mut f = File::create(&test_file).unwrap();
    f.write_all(&vec![0x42u8; FILE_SIZE]).unwrap();
    f.sync_all().unwrap();

    // Create subdirectory with files for readdir
    let subdir = dir.join("subdir");
    fs::create_dir_all(&subdir).unwrap();
    for i in 0..10 {
        let path = subdir.join(format!("file_{}.txt", i));
        fs::write(&path, format!("content {}", i)).unwrap();
    }
}

/// Name the lookup benchmarks resolve to get a server round trip. Nothing ever
/// creates it.
const MISSING_NAME: &str = "no_such_file.dat";

/// `statx` on an open descriptor, forcing the filesystem to answer rather than
/// the kernel's cached attributes. Returns the reported permission bits.
///
/// `AT_STATX_FORCE_SYNC` makes fuse's `getattr` inode operation call
/// `fuse_do_getattr()` unconditionally instead of serving the cached
/// attributes, and `AT_EMPTY_PATH` keeps name resolution out of the
/// measurement entirely. One call is therefore one GETATTR round trip and
/// nothing else. On a local filesystem the flag has nothing to revalidate
/// against; the `host_fs_forced` control prices that case so the FUSE figure
/// can be attributed to the round trip rather than to the flag.
///
/// The mode is returned rather than the size because fuse-pipe negotiates
/// `FUSE_WRITEBACK_CACHE`, under which the kernel owns `i_size` for regular
/// files and discards whatever size the server reports. Size therefore cannot
/// witness a round trip; permission bits are copied straight out of every
/// reply.
fn forced_statx_mode(fd: RawFd) -> u16 {
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    let empty = b"\0";
    let rc = unsafe {
        libc::statx(
            fd,
            empty.as_ptr() as *const libc::c_char,
            libc::AT_EMPTY_PATH | libc::AT_STATX_FORCE_SYNC,
            libc::STATX_BASIC_STATS,
            &mut stx,
        )
    };
    assert_eq!(
        rc,
        0,
        "forced statx failed: {}",
        std::io::Error::last_os_error()
    );
    stx.stx_mode & 0o7777
}

/// Prove, before publishing a number from it, that [`forced_statx_mode`] on a
/// FUSE descriptor reaches the server on every call.
///
/// Chmod the backing file behind the mount's back, then read the mode back two
/// ways. A plain stat still reports the old mode, because the mount's 1s
/// attr_timeout has not elapsed; the forced stat must report the new one. The
/// first half is what makes the second half meaningful: it shows the cache was
/// live and would have hidden the change.
///
/// Without this the benchmark could quietly become the attribute-cache
/// measurement it was written to replace, because an unhonoured
/// `AT_STATX_FORCE_SYNC` looks exactly like a very fast FUSE.
fn assert_forced_getattr_reaches_server(backing: &Path, via_mount: &Path, fd: RawFd) {
    use std::os::unix::fs::PermissionsExt;

    let before = 0o644;
    fs::set_permissions(backing, fs::Permissions::from_mode(before)).unwrap();
    assert_eq!(
        forced_statx_mode(fd) as u32,
        before,
        "the mount does not agree with the backing file even before anything is changed \
         behind its back, so neither half of this check would mean anything"
    );

    let after = 0o600;
    fs::set_permissions(backing, fs::Permissions::from_mode(after)).unwrap();

    let cached = fs::metadata(via_mount).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        cached,
        before,
        "a plain stat through {} already saw the out-of-band chmod, so the attribute cache is \
         not in play here and the forced case proves nothing",
        via_mount.display()
    );

    let forced = forced_statx_mode(fd) as u32;
    assert_eq!(
        forced, after,
        "AT_STATX_FORCE_SYNC returned the cached mode, so this benchmark would measure the \
         attribute cache rather than a GETATTR round trip"
    );
}

/// Prove that resolving [`MISSING_NAME`] reaches the server on every call.
///
/// fuse only caches a negative dentry when the server answers with a zero
/// nodeid and an entry timeout. fuse-pipe answers a miss with a plain ENOENT,
/// so `fuse_lookup()` invalidates the entry instead of caching it and every
/// resolution of an absent name is a fresh LOOKUP, with no pool of distinct
/// paths to exhaust and no dependence on how many iterations criterion decides
/// to run.
///
/// Checked rather than assumed: resolve a probe name, create it behind the
/// mount's back, resolve again. A cached negative answer still reports it
/// missing. The probe uses its own name so the positive dentry it leaves
/// behind cannot bleed into the benchmark's.
fn assert_missing_lookup_reaches_server(mount: &Path, backing: &Path) {
    let probe = "negative_cache_probe.dat";
    let via_mount = mount.join(probe);
    let via_backing = backing.join(probe);

    assert!(
        !via_mount.exists(),
        "{} must start absent for this check to mean anything",
        via_mount.display()
    );
    fs::write(&via_backing, b"x").unwrap();
    assert!(
        via_mount.exists(),
        "the mount still reports {} absent after it was created behind its back, so misses are \
         answered from a cached negative dentry and the benchmark below would measure that cache",
        via_mount.display()
    );
    fs::remove_file(&via_backing).unwrap();
}

fn cleanup(data_dir: &PathBuf, mount_dir: &PathBuf) {
    // Only unmount if actually a FUSE mount
    if common::is_fuse_mount(mount_dir) {
        let _ = Command::new("fusermount3")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
    }
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(mount_dir);
}

fn bench_getattr(c: &mut Criterion) {
    ensure_ulimit();

    let data_dir = PathBuf::from("/tmp/fuse-ops-data-getattr");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-getattr");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/getattr");
    group.sample_size(100);

    // Host filesystem baseline
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let _ = fs::metadata(&test_file).unwrap();
        })
    });

    // Control for the forced case below: the identical call on a local
    // filesystem, which has nothing to revalidate against and so answers from
    // its in-core inode either way. It is not directly comparable to `host_fs`
    // (no name to resolve, so it comes out lower), and it is not meant to be.
    // Its job is to price the syscall and the flag on their own, so that the
    // FUSE figure below can be read as the round trip rather than as the cost
    // of asking synchronously.
    let host_fd = File::open(&test_file).unwrap();
    group.bench_function("host_fs_forced", |b| {
        b.iter(|| forced_statx_mode(host_fd.as_raw_fd()))
    });

    // FUSE with 256 readers (our recommended default)
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");

    // Restating one path measures the kernel's attribute cache: FUSE mounts
    // default to a 1s attr_timeout, so after the first stat the daemon is not
    // contacted at all. That is a real number for a hot working set, but it is
    // not the cost of a FUSE getattr, so it is named for what it measures.
    group.bench_function("fuse_256_readers_attr_cache_hit", |b| {
        b.iter(|| {
            let _ = fs::metadata(&fuse_file).unwrap();
        })
    });

    // The GETATTR round trip.
    //
    // Statting a path the mount has not seen does NOT measure this: pathname
    // resolution sends a LOOKUP whose reply already carries attributes, so the
    // stat is satisfied without a GETATTR ever being issued. That is a LOOKUP
    // measurement, and it lives in `single_op/lookup`. Isolating GETATTR takes
    // a descriptor (no name to resolve) plus a forced revalidation.
    let fuse_fd = File::open(&fuse_file).unwrap();
    assert_forced_getattr_reaches_server(&test_file, &fuse_file, fuse_fd.as_raw_fd());
    group.bench_function("fuse_256_readers_forced_round_trip", |b| {
        b.iter(|| forced_statx_mode(fuse_fd.as_raw_fd()))
    });

    drop(fuse_fd);
    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_lookup(c: &mut Criterion) {
    ensure_ulimit();

    let data_dir = PathBuf::from("/tmp/fuse-ops-data-lookup");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-lookup");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/lookup");
    group.sample_size(100);

    // Host filesystem baseline - lookup via exists()
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let _ = test_file.exists();
        })
    });

    // Control for the miss case below: what resolving an absent name costs
    // when there is no server to ask.
    let host_missing = data_dir.join(MISSING_NAME);
    group.bench_function("host_fs_miss", |b| {
        b.iter(|| {
            let _ = host_missing.exists();
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");

    // As with getattr: one repeated path is served by the kernel's dentry
    // cache (entry_timeout), not by the server.
    group.bench_function("fuse_256_readers_dentry_cache_hit", |b| {
        b.iter(|| {
            let _ = fuse_file.exists();
        })
    });

    // The LOOKUP round trip, via a name that does not exist.
    //
    // A pool of distinct existing files also produces cold lookups, but only
    // until the walk wraps, and sizing the pool against `sample_size` does not
    // prevent that: `sample_size` bounds criterion's samples, not its `iter`
    // calls. Measured at criterion's defaults, a 20,000-file pool got 20,000
    // measurement iterations plus an untimed warm-up that had already walked
    // thousands. Past the wrap, whether a revisited inode is still cached comes
    // down to how long one pass takes against the 1s entry_timeout, which is a
    // property of the host rather than of the benchmark, and the case cannot
    // report which blend it measured. An absent name has no such dependence:
    // fuse-pipe answers a miss with a plain ENOENT, which fuse does not cache,
    // so iteration one and iteration ten million both reach the server.
    assert_missing_lookup_reaches_server(fuse.mount_path(), &data_dir);
    let fuse_missing = fuse.mount_path().join(MISSING_NAME);
    group.bench_function("fuse_256_readers_miss_round_trip", |b| {
        b.iter(|| {
            let _ = fuse_missing.exists();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_open_close(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-open");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-open");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/open_close");
    group.sample_size(100);

    // Host filesystem baseline
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let f = File::open(&test_file).unwrap();
            drop(f);
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let f = File::open(&fuse_file).unwrap();
            drop(f);
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_read_4kb(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-read");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-read");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/read_4kb");
    group.sample_size(100);

    // Host filesystem baseline
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        let mut f = File::open(&test_file).unwrap();
        let mut buf = vec![0u8; FILE_SIZE];
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.read_exact(&mut buf).unwrap();
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        let mut f = File::open(&fuse_file).unwrap();
        let mut buf = vec![0u8; FILE_SIZE];
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.read_exact(&mut buf).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_write_4kb(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-write");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-write");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/write_4kb");
    group.sample_size(100);

    let data = vec![0x42u8; FILE_SIZE];

    // Host filesystem baseline (no sync)
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        let mut f = OpenOptions::new().write(true).open(&test_file).unwrap();
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&data).unwrap();
        })
    });

    // FUSE (no sync)
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        let mut f = OpenOptions::new().write(true).open(&fuse_file).unwrap();
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&data).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_readdir(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-readdir");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-readdir");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/readdir");
    group.sample_size(100);

    // Host filesystem baseline
    let subdir = data_dir.join("subdir");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let entries: Vec<_> = fs::read_dir(&subdir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(entries.len(), 10);
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_subdir = fuse.mount_path().join("subdir");
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let entries: Vec<_> = fs::read_dir(&fuse_subdir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(entries.len(), 10);
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_create_unlink(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-create");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-create");

    cleanup(&data_dir, &mount_dir);
    fs::create_dir_all(&data_dir).unwrap();

    let mut group = c.benchmark_group("single_op/create_unlink");
    group.sample_size(100);

    let counter = AtomicU64::new(0);

    // Host filesystem baseline
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let path = data_dir.join(format!("tmp_{}.txt", n));
            File::create(&path).unwrap();
            fs::remove_file(&path).unwrap();
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let path = fuse.mount_path().join(format!("tmp_{}.txt", n));
            File::create(&path).unwrap();
            fs::remove_file(&path).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

criterion_group!(
    benches,
    bench_getattr,
    bench_lookup,
    bench_open_close,
    bench_read_4kb,
    bench_write_4kb,
    bench_readdir,
    bench_create_unlink,
);

criterion_main!(benches);
