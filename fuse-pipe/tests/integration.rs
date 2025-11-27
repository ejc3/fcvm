//! Integration tests for fuse-pipe filesystem operations.
//!
//! These tests verify basic FUSE operations work correctly through
//! the fuse-pipe server/client stack.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn find_stress_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("get current exe");
    let deps_dir = exe.parent().unwrap();

    for entry in std::fs::read_dir(deps_dir).expect("read deps") {
        if let Ok(entry) = entry {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("stress-") && !name.contains('.') {
                return entry.path();
            }
        }
    }

    panic!("Could not find stress binary. Run: cargo test --test stress --test integration");
}

struct FuseFixture {
    server: Child,
    client: Child,
    data_dir: PathBuf,
    mount_dir: PathBuf,
    socket: String,
}

impl FuseFixture {
    async fn new() -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let socket = format!("/tmp/fuse-integ-{}-{}.sock", pid, id);
        let data_dir = PathBuf::from(format!("/tmp/fuse-integ-data-{}-{}", pid, id));
        let mount_dir = PathBuf::from(format!("/tmp/fuse-integ-mount-{}-{}", pid, id));

        std::fs::create_dir_all(&data_dir).expect("create data dir");
        std::fs::create_dir_all(&mount_dir).expect("create mount dir");
        let _ = std::fs::remove_file(&socket);

        let stress_exe = find_stress_binary();

        let server = Command::new(&stress_exe)
            .args(["server", "--socket", &socket, "--root", data_dir.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start server");

        sleep(Duration::from_millis(500)).await;

        let client = Command::new(&stress_exe)
            .args(["client", "--socket", &socket, "--mount", mount_dir.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start client");

        sleep(Duration::from_millis(500)).await;

        Self { server, client, data_dir, mount_dir, socket }
    }

    fn mount(&self) -> &PathBuf {
        &self.mount_dir
    }

    async fn cleanup(mut self) {
        let _ = self.client.kill().await;
        let _ = self.server.kill().await;
        let _ = std::fs::remove_file(&self.socket);
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
