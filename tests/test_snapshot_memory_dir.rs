//! `snapshot create --memory-dir`: the memory file lives in another directory (a tmpfs for
//! the potato players' sleeps), behind the snapshot's `memory.bin` link.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The memory files `--memory-dir` wrote in `dir`.
fn memory_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("fcvm-memory-"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

async fn sleep(fcvm: &Path, pid: u32, tag: &str, memory_dir: &Path) -> Result<()> {
    let output = tokio::process::Command::new(fcvm)
        .args([
            "snapshot",
            "create",
            "--pid",
            &pid.to_string(),
            "--tag",
            tag,
            "--stop",
        ])
        .arg("--memory-dir")
        .arg(memory_dir)
        .output()
        .await
        .context("running snapshot create")?;
    anyhow::ensure!(
        output.status.success(),
        "snapshot create --memory-dir failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// A VM put to sleep with its memory outside the store wakes from it, a second sleep of the woken
/// clone replaces the first generation and its memory file, and a snapshot whose memory file
/// is gone (a reboot clears a tmpfs) is refused with that reason before anything starts.
#[tokio::test]
async fn test_snapshot_memory_dir_sleeps_and_wakes() -> Result<()> {
    let (base, clone, tag, _) = common::unique_names("memdir");
    let fcvm = common::find_fcvm_binary()?;
    // /tmp, not /dev/shm: a container's /dev/shm is 64 MiB, less than the guest's memory, and
    // what this test checks does not depend on the directory being a tmpfs
    let memory_dir = tempfile::Builder::new()
        .prefix("fcvm-memdir-test-")
        .tempdir_in("/tmp")
        .context("making a memory directory in /tmp")?;
    // --memory-dir refuses a directory group or others could write, whatever the umask
    std::fs::set_permissions(
        memory_dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;

    let (_base_child, base_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &base, common::TEST_IMAGE],
        &base,
    )
    .await?;
    common::poll_health_by_pid(base_pid, 120).await?;
    sleep(&fcvm, base_pid, &tag, memory_dir.path()).await?;

    let link = fcvm::paths::snapshot_dir().join(&tag).join("memory.bin");
    let first = memory_files(memory_dir.path());
    assert_eq!(first.len(), 1, "one memory file: {first:?}");
    assert_eq!(std::fs::read_link(&link)?, first[0]);
    let config = fcvm::storage::SnapshotManager::new(fcvm::paths::snapshot_dir())
        .load_snapshot(&tag)
        .await?;
    assert_eq!(
        std::fs::metadata(&first[0])?.len(),
        u64::from(config.metadata.memory_mib) << 20,
        "a full snapshot of the guest's memory"
    );

    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--snapshot", &tag, "--name", &clone],
        &clone,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    let echo = common::exec_in_vm(clone_pid, &["echo", "MEMDIR_WOKE"]).await?;
    assert!(echo.contains("MEMDIR_WOKE"), "{echo}");

    // the woken clone sleeps again: the new generation replaces the first and its file
    sleep(&fcvm, clone_pid, &tag, memory_dir.path()).await?;
    let second = memory_files(memory_dir.path());
    assert_eq!(
        second.len(),
        1,
        "the replaced generation's file is removed: {second:?}"
    );
    assert_ne!(second, first);
    assert_eq!(std::fs::read_link(&link)?, second[0]);

    // the memory is lost, as a reboot loses a tmpfs: the wake is refused, naming why
    std::fs::remove_file(&second[0])?;
    let output = tokio::process::Command::new(&fcvm)
        .args([
            "snapshot",
            "run",
            "--snapshot",
            &tag,
            "--name",
            &format!("{clone}-2"),
        ])
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a wake without its memory must fail"
    );
    assert!(
        stderr.contains(&format!("the memory file of snapshot {tag} is gone")),
        "{stderr}"
    );
    Ok(())
}
