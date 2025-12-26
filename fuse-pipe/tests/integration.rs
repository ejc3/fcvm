//! Integration tests for fuse-pipe filesystem operations.
//!
//! These tests verify FUSE operations work correctly through the in-process
//! fuse-pipe server/client stack. These tests do NOT require root.
//!
//! Root-requiring tests are in integration_root.rs.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

mod common;

use std::fs;
use std::os::unix::io::AsRawFd;

use common::{cleanup, unique_paths, FuseMount};
use nix::unistd::{lseek, Whence};

#[test]
fn test_create_and_read_file() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

    let test_file = fuse.mount_path().join("test.txt");
    fs::write(&test_file, "Hello, fuse-pipe!\n").expect("write file");
    let content = fs::read_to_string(&test_file).expect("read file");
    assert_eq!(content, "Hello, fuse-pipe!\n");
    fs::remove_file(&test_file).expect("remove file");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_create_directory() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

    let test_dir = fuse.mount_path().join("testdir");
    fs::create_dir(&test_dir).expect("create dir");
    assert!(test_dir.is_dir());
    fs::remove_dir(&test_dir).expect("remove dir");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_list_directory() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
    let mount = fuse.mount_path();

    fs::write(mount.join("a.txt"), "a").expect("write a");
    fs::write(mount.join("b.txt"), "b").expect("write b");
    fs::create_dir(mount.join("subdir")).expect("create subdir");

    let entries: Vec<_> = fs::read_dir(mount)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(entries.contains(&"a.txt".to_string()));
    assert!(entries.contains(&"b.txt".to_string()));
    assert!(entries.contains(&"subdir".to_string()));

    fs::remove_file(mount.join("a.txt")).expect("remove a");
    fs::remove_file(mount.join("b.txt")).expect("remove b");
    fs::remove_dir(mount.join("subdir")).expect("remove subdir");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_nested_file() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

    let subdir = fuse.mount_path().join("nested");
    let subfile = subdir.join("file.txt");

    fs::create_dir(&subdir).expect("create subdir");
    fs::write(&subfile, "Nested content\n").expect("write nested file");

    let content = fs::read_to_string(&subfile).expect("read nested file");
    assert_eq!(content, "Nested content\n");

    fs::remove_file(&subfile).expect("remove file");
    fs::remove_dir(&subdir).expect("remove dir");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_file_metadata() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

    let test_file = fuse.mount_path().join("meta.txt");
    let content = "Some content here";

    fs::write(&test_file, content).expect("write file");

    let meta = fs::metadata(&test_file).expect("get metadata");
    assert!(meta.is_file());
    assert_eq!(meta.len(), content.len() as u64);

    fs::remove_file(&test_file).expect("remove file");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_rename_across_directories() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
    let mount = fuse.mount_path();

    let dir1 = mount.join("dir1");
    let dir2 = mount.join("dir2");
    fs::create_dir(&dir1).expect("create dir1");
    fs::create_dir(&dir2).expect("create dir2");

    let file1 = dir1.join("file.txt");
    let file2 = dir2.join("renamed.txt");
    fs::write(&file1, "rename me").expect("write file");

    fs::rename(&file1, &file2).expect("rename across dirs");

    assert!(!file1.exists(), "old path should not exist");
    let contents = fs::read_to_string(&file2).expect("read renamed");
    assert_eq!(contents, "rename me");

    fs::remove_file(&file2).expect("cleanup file");
    fs::remove_dir(&dir1).expect("cleanup dir1");
    fs::remove_dir(&dir2).expect("cleanup dir2");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_symlink_and_readlink() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
    let mount = fuse.mount_path();

    let target = mount.join("target.txt");
    let link = mount.join("link.txt");

    fs::write(&target, "hello").expect("write target");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");

    let link_contents = fs::read_to_string(&link).expect("read via link");
    assert_eq!(link_contents, "hello");

    let link_target = fs::read_link(&link).expect("readlink");
    assert_eq!(link_target, target);

    fs::remove_file(&link).expect("remove link");
    fs::remove_file(&target).expect("remove target");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_hardlink_survives_source_removal() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
    let mount = fuse.mount_path();

    let source = mount.join("source.txt");
    let link = mount.join("link.txt");
    fs::write(&source, "hardlink").expect("write source");
    fs::hard_link(&source, &link).expect("create hardlink");

    fs::remove_file(&source).expect("remove source");

    let content = fs::read_to_string(&link).expect("read hardlink");
    assert_eq!(content, "hardlink");

    fs::remove_file(&link).expect("cleanup");

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

#[test]
fn test_multi_reader_mount_basic_io() {
    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path().to_path_buf();

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let m = mount.clone();
            std::thread::spawn(move || {
                let path = m.join(format!("multi-{}.txt", i));
                let data = format!("payload-{}", i);
                fs::write(&path, data.as_bytes()).expect("write");
                let read_back = fs::read_to_string(&path).expect("read");
                assert!(read_back.starts_with("payload-"));
                fs::remove_file(&path).ok();
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test that lseek supports negative offsets relative to SEEK_END.
#[test]
fn test_lseek_supports_negative_offsets() {
    common::increase_ulimit();

    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
    let mount = fuse.mount_path();

    let path = mount.join("seek-file");
    fs::write(&path, b"abcdef").expect("write seek file");

    let file = fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open for lseek");

    let pos = lseek(file.as_raw_fd(), -2, Whence::SeekEnd).expect("lseek");
    assert_eq!(pos, 4, "should allow negative offsets relative to SEEK_END");

    // Must drop file handle before unmounting to avoid hanging
    drop(file);
    let _ = fs::remove_file(&path);

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}
