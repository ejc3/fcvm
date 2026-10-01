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
