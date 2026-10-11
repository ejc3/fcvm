//! A mount made with a read-only volume's settings.
//!
//! These run the mount code the guest runs (`MountSettings::for_volume` and the
//! session setup shared by the Unix-socket and vsock paths) against a local
//! server. They need neither root nor a VM.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use common::{cleanup, unique_paths, FuseMount};
use fuse_pipe::MountSettings;
use nix::unistd::{lseek, Whence};

/// A read-only volume in a VM that leaves the VM-wide writeback switch alone.
fn read_only_volume() -> MountSettings {
    MountSettings::for_volume(true, false)
}

/// A file the backing directory rewrites longer in place is read whole, with
/// its new size and mtime, once the 1 s attribute timeout has passed.
///
/// RED BEFORE THE FIX: the mount had the writeback cache, under which the
/// kernel keeps the size and mtime it first cached. stat kept saying 8 bytes
/// and the read stopped there.
#[test]
fn a_read_only_mount_follows_a_file_rewritten_longer_in_place() {
    let (data_dir, mount_dir) = unique_paths("fuse-ro-follow");
    let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, read_only_volume());
    let backing = data_dir.join("grows.txt");
    let mounted = fuse.mount_path().join("grows.txt");

    fs::write(&backing, "8 bytes\n").expect("write the first version");
    // An old mtime, so the rewrite below cannot land on the same timestamp.
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options()
        .write(true)
        .open(&backing)
        .and_then(|file| file.set_modified(old))
        .expect("set the first version's mtime");
    let inode = fs::metadata(&backing).expect("stat the backing file").ino();

    // The mount caches the file's size, mtime and pages.
    assert_eq!(
        fs::read_to_string(&mounted).expect("first read through the mount"),
        "8 bytes\n"
    );
    assert_eq!(
        fs::metadata(&mounted)
            .and_then(|meta| meta.modified())
            .expect("first stat through the mount"),
        old
    );

    let longer = "x".repeat(91) + "\n";
    fs::write(&backing, &longer).expect("rewrite the backing file longer");
    let rewritten = fs::metadata(&backing).expect("stat the rewritten backing file");
    assert_eq!(rewritten.ino(), inode, "the rewrite must keep the inode");
    let new = rewritten.modified().expect("the rewritten file's mtime");
    assert_ne!(new, old);

    let deadline = Instant::now() + Duration::from_secs(5);
    let seen = loop {
        let meta = fs::metadata(&mounted).expect("stat through the mount");
        let seen = (
            meta.len(),
            meta.modified().expect("mtime through the mount"),
            fs::read_to_string(&mounted).expect("read through the mount"),
        );
        if seen.0 == rewritten.len() || Instant::now() >= deadline {
            break seen;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        seen,
        (rewritten.len(), new, longer),
        "size, mtime and content through the mount, up to 5 s after the backing file was \
         rewritten in place from 8 to 92 bytes"
    );

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Reads a file through the mount until it holds `want` or 5 s have passed, and
/// returns what the last read gave.
fn read_until(mounted: &Path, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = fs::read_to_string(mounted).expect("read through the mount");
        if seen == want || Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A file rewritten in place with content of the same length is read new once
/// the attribute timeout has passed. A read-only mount keeps a file's pages from
/// one open to the next, so the changed mtime is the only thing that tells the
/// kernel to drop them (FUSE_AUTO_INVAL_DATA).
#[test]
fn a_read_only_mount_follows_a_file_rewritten_with_the_same_length() {
    let (data_dir, mount_dir) = unique_paths("fuse-ro-same-length");
    let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, read_only_volume());
    let backing = data_dir.join("same.txt");
    let mounted = fuse.mount_path().join("same.txt");

    fs::write(&backing, "version-1\n").expect("write the first version");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options()
        .write(true)
        .open(&backing)
        .and_then(|file| file.set_modified(old))
        .expect("set the first version's mtime");
    // Read it twice, so its pages are cached across an open.
    for _ in 0..2 {
        assert_eq!(
            fs::read_to_string(&mounted).expect("read through the mount"),
            "version-1\n"
        );
    }

    fs::write(&backing, "version-2\n").expect("rewrite the backing file in place");
    assert_eq!(
        read_until(&mounted, "version-2\n"),
        "version-2\n",
        "content through the mount, up to 5 s after the backing file was rewritten in place \
         with content of the same length"
    );

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// A file replaced by renaming another file over it, the way a tool that writes
/// a temporary file and renames it updates a file, is read new once the entry
/// timeout has passed.
#[test]
fn a_read_only_mount_follows_a_file_replaced_by_rename() {
    let (data_dir, mount_dir) = unique_paths("fuse-ro-rename");
    let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, read_only_volume());
    let backing = data_dir.join("replaced.txt");
    let mounted = fuse.mount_path().join("replaced.txt");

    fs::write(&backing, "original-v1\n").expect("write the first version");
    for _ in 0..2 {
        assert_eq!(
            fs::read_to_string(&mounted).expect("read through the mount"),
            "original-v1\n"
        );
    }

    let temporary = data_dir.join(".replaced.txt.tmp");
    fs::write(&temporary, "replaced-v2\n").expect("write the replacement");
    fs::rename(&temporary, &backing).expect("rename the replacement over the file");
    assert_eq!(
        read_until(&mounted, "replaced-v2\n"),
        "replaced-v2\n",
        "content through the mount, up to 5 s after a file was renamed over the backing file"
    );

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// SEEK_DATA and SEEK_HOLE work through a read-only mount. Its server opens
/// nothing, so it has no file handle to seek with; it answers LSEEK with ENOSYS,
/// and the kernel then seeks by itself, taking the whole file for data.
///
/// RED BEFORE THE FIX: the server looked up handle 0 and answered EBADF.
#[test]
fn a_read_only_mount_seeks_for_data_and_holes() {
    let (data_dir, mount_dir) = unique_paths("fuse-ro-seek");
    let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, read_only_volume());
    let content = "x".repeat(10_000);
    fs::write(data_dir.join("seek.txt"), &content).expect("write the backing file");
    let file = fs::File::open(fuse.mount_path().join("seek.txt")).expect("open through the mount");
    let seen = (
        lseek(&file, 0, Whence::SeekData),
        lseek(&file, 0, Whence::SeekHole),
    );
    assert_eq!(
        seen,
        (Ok(0), Ok(content.len() as i64)),
        "SEEK_DATA and SEEK_HOLE from 0 through a read-only mount"
    );

    drop(file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Reading a file again and again through a read-only mount asks the server
/// nothing after the first time: no OPEN, FLUSH or RELEASE, and the content
/// comes from the page cache. A read-write mount of the same file is the
/// control: it opens, flushes and releases through the server every time.
///
/// RED BEFORE THE FIX: the read-only volume's server opened files like a
/// read-write one's, so every read was an OPEN, a READ, a FLUSH and a RELEASE.
#[test]
fn a_read_only_mount_reads_a_file_again_without_asking_the_server() {
    const READS: u64 = 50;
    let content = "the same small file, read on every request\n";
    let requests = |settings: MountSettings, prefix: &str| {
        let (data_dir, mount_dir) = unique_paths(prefix);
        let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, settings);
        fs::write(data_dir.join("small.txt"), content).expect("write the backing file");
        let mounted = fuse.mount_path().join("small.txt");
        for _ in 0..READS {
            assert_eq!(
                fs::read_to_string(&mounted).expect("read through the mount"),
                content
            );
        }
        // RELEASE is a background request, so the last ones can reach the server after close()
        // returns. Read the counts once they have stopped changing for a second.
        let counts =
            || ["open", "read", "flush", "release"].map(|op| (op, fuse.server_requests(op)));
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut seen, mut since) = (counts(), Instant::now());
        while since.elapsed() < Duration::from_secs(1) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            let now = counts();
            if now != seen {
                (seen, since) = (now, Instant::now());
            }
        }
        drop(fuse);
        cleanup(&data_dir, &mount_dir);
        seen
    };

    let read_write = requests(MountSettings::for_volume(false, false), "fuse-rw-opens");
    let read_only = requests(read_only_volume(), "fuse-ro-opens");
    assert!(
        read_write
            .iter()
            .all(|(op, count)| *op == "read" || *count == READS),
        "the control: a read-write mount should send one OPEN, FLUSH and RELEASE per read \
         ({READS} reads), got {read_write:?}"
    );
    assert_eq!(
        read_only,
        [("open", 1), ("read", 1), ("flush", 1), ("release", 0)],
        "requests the server handled for {READS} reads of one file through a read-only mount: \
         the first OPEN and FLUSH are answered ENOSYS and the kernel sends neither again, \
         RELEASE never follows an open answered ENOSYS, and later reads come from the page cache"
    );
}

/// The options of the mount at `mount_point`, per mount and of its superblock,
/// from /proc/self/mountinfo.
fn mount_options_of(mount_point: &Path) -> (Vec<String>, Vec<String>) {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").expect("read mountinfo");
    let options = |text: &str| text.split(',').map(str::to_string).collect::<Vec<_>>();
    mountinfo
        .lines()
        .filter_map(|line| {
            // id parent major:minor root mount-point options [tags] - type source super-options
            let fields: Vec<&str> = line.split_whitespace().collect();
            if Path::new(fields.get(4)?) != mount_point {
                return None;
            }
            let separator = fields.iter().position(|field| *field == "-")?;
            Some((options(fields[5]), options(fields.get(separator + 3)?)))
        })
        .next_back()
        .unwrap_or_else(|| panic!("no mount at {} in:\n{mountinfo}", mount_point.display()))
}

/// The mount table says `ro`, every write through the mount fails with EROFS,
/// and the backing directory does not change. Reads still work.
///
/// RED BEFORE THE FIX: the mount was read-write, so the create, the overwrite
/// and the mkdir all succeeded and reached the backing directory.
#[test]
fn a_read_only_mount_refuses_writes_with_erofs() {
    let (data_dir, mount_dir) = unique_paths("fuse-ro-write");
    let fuse = FuseMount::with_settings(&data_dir, &mount_dir, 1, read_only_volume());
    let mount = fuse.mount_path();
    fs::write(data_dir.join("existing.txt"), "from the backing directory")
        .expect("write the backing file");

    let mut wrong: Vec<String> = Vec::new();

    let (per_mount, superblock) = mount_options_of(mount);
    if !per_mount.iter().any(|o| o == "ro") || !superblock.iter().any(|o| o == "ro") {
        wrong.push(format!(
            "the mount table has it as {} (superblock {}), not ro",
            per_mount.join(","),
            superblock.join(",")
        ));
    }

    let attempts = [
        (
            "creating a file",
            fs::write(mount.join("created.txt"), "through the mount"),
        ),
        (
            "overwriting a file",
            fs::write(mount.join("existing.txt"), "through the mount"),
        ),
        ("creating a directory", fs::create_dir(mount.join("dir"))),
    ];
    for (what, result) in attempts {
        if result.as_ref().err().and_then(|e| e.raw_os_error()) != Some(libc::EROFS) {
            wrong.push(format!("{what} did not fail with EROFS: {result:?}"));
        }
    }

    let mut backing: Vec<String> = fs::read_dir(&data_dir)
        .expect("list the backing directory")
        .map(|entry| entry.expect("backing directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    backing.sort();
    if backing != ["existing.txt"] {
        wrong.push(format!(
            "the backing directory holds {backing:?}, not only existing.txt"
        ));
    }
    let through_mount = fs::read_to_string(mount.join("existing.txt"));
    if through_mount.as_deref().ok() != Some("from the backing directory") {
        wrong.push(format!(
            "existing.txt read through the mount: {through_mount:?}"
        ));
    }

    assert!(
        wrong.is_empty(),
        "a mount made with a read-only volume's settings is not read-only:\n{}",
        wrong.join("\n")
    );

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}
