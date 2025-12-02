//! Integration tests for fuse-pipe filesystem operations.
//!
//! These tests verify basic FUSE operations work correctly through
//! the fuse-pipe server/client stack.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

#[path = "stress/fixture.rs"]
mod fixture;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{sleep, Duration};

use fixture::FuseMount;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Async wrapper around the synchronous FuseMount fixture.
struct FuseFixture {
    inner: Option<FuseMount>,
    data_dir: PathBuf,
    mount_dir: PathBuf,
}

impl FuseFixture {
    async fn new() -> Self {
        Self::new_with_readers(1).await
    }

    async fn new_with_readers(readers: usize) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let data_dir = PathBuf::from(format!("/tmp/fuse-integ-data-{}-{}", pid, id));
        let mount_dir = PathBuf::from(format!("/tmp/fuse-integ-mount-{}-{}", pid, id));

        // FuseMount handles creation and cleanup
        let inner = FuseMount::new(&data_dir, &mount_dir, readers);

        // Give mount a bit more time to stabilize for async tests
        sleep(Duration::from_millis(100)).await;

        Self {
            inner: Some(inner),
            data_dir,
            mount_dir,
        }
    }

    fn mount(&self) -> &PathBuf {
        &self.mount_dir
    }

    async fn cleanup(mut self) {
        // Drop the inner FuseMount to trigger cleanup
        self.inner.take();
        sleep(Duration::from_millis(100)).await;

        // Additional cleanup of directories
        let _ = std::fs::remove_dir_all(&self.data_dir);
        let _ = std::fs::remove_dir_all(&self.mount_dir);
    }
}

#[tokio::test]
async fn test_create_and_read_file() {
    let fixture = FuseFixture::new().await;
    let test_file = fixture.mount().join("test.txt");

    std::fs::write(&test_file, "Hello, fuse-pipe!\n").expect("write file");
    let content = std::fs::read_to_string(&test_file).expect("read file");
    assert_eq!(content, "Hello, fuse-pipe!\n");
    std::fs::remove_file(&test_file).expect("remove file");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_create_directory() {
    let fixture = FuseFixture::new().await;
    let test_dir = fixture.mount().join("testdir");

    std::fs::create_dir(&test_dir).expect("create dir");
    assert!(test_dir.is_dir());
    std::fs::remove_dir(&test_dir).expect("remove dir");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_list_directory() {
    let fixture = FuseFixture::new().await;
    let mount = fixture.mount();

    std::fs::write(mount.join("a.txt"), "a").expect("write a");
    std::fs::write(mount.join("b.txt"), "b").expect("write b");
    std::fs::create_dir(mount.join("subdir")).expect("create subdir");

    let entries: Vec<_> = std::fs::read_dir(mount)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    assert!(entries.contains(&"a.txt".to_string()));
    assert!(entries.contains(&"b.txt".to_string()));
    assert!(entries.contains(&"subdir".to_string()));

    std::fs::remove_file(mount.join("a.txt")).expect("remove a");
    std::fs::remove_file(mount.join("b.txt")).expect("remove b");
    std::fs::remove_dir(mount.join("subdir")).expect("remove subdir");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_nested_file() {
    let fixture = FuseFixture::new().await;
    let subdir = fixture.mount().join("nested");
    let subfile = subdir.join("file.txt");

    std::fs::create_dir(&subdir).expect("create subdir");
    std::fs::write(&subfile, "Nested content\n").expect("write nested file");

    let content = std::fs::read_to_string(&subfile).expect("read nested file");
    assert_eq!(content, "Nested content\n");

    std::fs::remove_file(&subfile).expect("remove file");
    std::fs::remove_dir(&subdir).expect("remove dir");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_file_metadata() {
    let fixture = FuseFixture::new().await;
    let test_file = fixture.mount().join("meta.txt");
    let content = "Some content here";

    std::fs::write(&test_file, content).expect("write file");

    let meta = std::fs::metadata(&test_file).expect("get metadata");
    assert!(meta.is_file());
    assert_eq!(meta.len(), content.len() as u64);

    std::fs::remove_file(&test_file).expect("remove file");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_rename_across_directories() {
    let fixture = FuseFixture::new().await;
    let mount = fixture.mount();

    let dir1 = mount.join("dir1");
    let dir2 = mount.join("dir2");
    std::fs::create_dir(&dir1).expect("create dir1");
    std::fs::create_dir(&dir2).expect("create dir2");

    let file1 = dir1.join("file.txt");
    let file2 = dir2.join("renamed.txt");
    std::fs::write(&file1, "rename me").expect("write file");

    std::fs::rename(&file1, &file2).expect("rename across dirs");

    assert!(!file1.exists(), "old path should not exist");
    let contents = std::fs::read_to_string(&file2).expect("read renamed");
    assert_eq!(contents, "rename me");

    std::fs::remove_file(&file2).expect("cleanup file");
    std::fs::remove_dir(&dir1).expect("cleanup dir1");
    std::fs::remove_dir(&dir2).expect("cleanup dir2");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_symlink_and_readlink() {
    let fixture = FuseFixture::new().await;
    let mount = fixture.mount();

    let target = mount.join("target.txt");
    let link = mount.join("link.txt");

    std::fs::write(&target, "hello").expect("write target");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");

    let link_contents = std::fs::read_to_string(&link).expect("read via link");
    assert_eq!(link_contents, "hello");

    let link_target = std::fs::read_link(&link).expect("readlink");
    assert_eq!(link_target, target);

    std::fs::remove_file(&link).expect("remove link");
    std::fs::remove_file(&target).expect("remove target");

    fixture.cleanup().await;
}

#[tokio::test]
async fn test_hardlink_survives_source_removal() {
    let fixture = FuseFixture::new().await;
    let mount = fixture.mount();

    let source = mount.join("source.txt");
    let link = mount.join("link.txt");
    std::fs::write(&source, "hardlink").expect("write source");
    std::fs::hard_link(&source, &link).expect("create hardlink");

    std::fs::remove_file(&source).expect("remove source");

    let content = std::fs::read_to_string(&link).expect("read hardlink");
    assert_eq!(content, "hardlink");

    std::fs::remove_file(&link).expect("cleanup");
    fixture.cleanup().await;
}

#[tokio::test]
async fn test_multi_reader_mount_basic_io() {
    let fixture = FuseFixture::new_with_readers(3).await;
    let mount = fixture.mount();

    let files: Vec<_> = (0..6)
        .map(|i| mount.join(format!("multi-{i}.txt")))
        .collect();

    let handles: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let data = format!("payload-{i}");
            let p = path.clone();
            tokio::spawn(async move {
                std::fs::write(&p, data.as_bytes()).expect("write");
                let read_back = std::fs::read_to_string(&p).expect("read");
                assert!(read_back.starts_with("payload-"));
            })
        })
        .collect();

    for h in handles {
        h.await.unwrap();
    }

    for path in files {
        std::fs::remove_file(path).ok();
    }

    fixture.cleanup().await;
}
