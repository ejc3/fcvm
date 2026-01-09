//! In-VM pjdfstest matrix - runs pjdfstest categories inside VMs
//!
//! Each category is a separate test, allowing nextest to run all 17 in parallel.
//! Tests the full stack: host VolumeServer → vsock → guest FUSE mount.
//!
//! See also: fuse-pipe/tests/pjdfstest_matrix_root.rs (host-side matrix, tests fuse-pipe directly)
//!
//! Run with: cargo nextest run --test test_fuse_in_vm_matrix --features privileged-tests

#![cfg(all(feature = "privileged-tests", feature = "integration-slow"))]

mod common;

use anyhow::{Context, Result};
use fs2::FileExt;
use std::process::Stdio;
use std::time::Instant;

/// Number of parallel jobs within prove (inside VM)
const JOBS: usize = 8;

/// Run a single pjdfstest category inside a VM
async fn run_category_in_vm(category: &str) -> Result<()> {
    let test_id = format!("pjdfs-vm-{}-{}", category, std::process::id());
    let vm_name = format!("pjdfs-{}-{}", category, std::process::id());
    let start = Instant::now();

    // Create logger for this test
    let logger = common::TestLogger::new(&format!("pjdfs-vm-{}", category));

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;

    // Build prove command for this category
    let prove_cmd = format!("prove -v -j {} -r /opt/pjdfstest/tests/{}/", JOBS, category);

    // Use file lock to prevent parallel builds of pjdfstest container
    // (17 tests run in parallel via nextest)
    let lock_path = "/tmp/pjdfstest-build.lock";
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(lock_path)
        .context("opening build lock file")?;
    lock_file.lock_exclusive().context("acquiring build lock")?;

    // Check if pjdfstest container exists (inside lock to prevent race)
    let check = tokio::process::Command::new("podman")
        .args(["image", "exists", "localhost/pjdfstest"])
        .output()
        .await?;

    if !check.status.success() {
        // Build pjdfstest container
        let build = tokio::process::Command::new("podman")
            .args([
                "build",
                "-t",
                "pjdfstest",
                "-f",
                "Containerfile.pjdfstest",
                ".",
            ])
            .output()
            .await
            .context("building pjdfstest container")?;

        if !build.status.success() {
            // lock_file is dropped here, releasing lock
            anyhow::bail!(
                "Failed to build pjdfstest: {}",
                String::from_utf8_lossy(&build.stderr)
            );
        }
    }
    // Release lock - image is now available
    drop(lock_file);

    // Create temp directory for FUSE mount
    let data_dir = format!("/tmp/fuse-{}-data", test_id);
    tokio::fs::create_dir_all(&data_dir).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777)).await?;
    }

    let map_arg = format!("{}:/testdir", data_dir);

    // Start VM with pjdfstest container
    let mut cmd = tokio::process::Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--map",
        &map_arg,
        "--cmd",
        &prove_cmd,
        "--privileged", // Needed for mknod tests
        "localhost/pjdfstest",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    // Preserve SUDO_USER if set
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    let mut child = cmd.spawn().context("spawning VM")?;
    let vm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no VM PID"))?;

    logger.info(&format!("Spawned VM PID={}", vm_pid));

    // Consume output with file logging
    common::spawn_log_consumer_with_logger(
        child.stdout.take(),
        &format!("vm-{}", category),
        logger.clone(),
    );
    common::spawn_log_consumer_stderr_with_logger(
        child.stderr.take(),
        &format!("vm-{}", category),
        logger.clone(),
    );

    // Wait for completion (15 min timeout per category)
    // Note: skopeo import can take ~6 min on x86_64, plus test execution time
    let timeout = std::time::Duration::from_secs(900);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    // Cleanup
    let _ = tokio::fs::remove_dir_all(&data_dir).await;

    let exit_status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => anyhow::bail!("Error waiting for VM: {}", e),
        Err(_) => {
            common::kill_process(vm_pid).await;
            anyhow::bail!("VM timeout after {} seconds", timeout.as_secs());
        }
    };

    let duration = start.elapsed();

    if !exit_status.success() {
        anyhow::bail!(
            "pjdfstest category {} failed in VM: exit={} ({:.1}s)",
            category,
            exit_status.code().unwrap_or(-1),
            duration.as_secs_f64()
        );
    }

    println!(
        "[FUSE-VM] \u{2713} {} ({:.1}s)",
        category,
        duration.as_secs_f64()
    );

    Ok(())
}

macro_rules! pjdfstest_vm_category {
    ($name:ident, $category:literal) => {
        #[tokio::test]
        async fn $name() {
            run_category_in_vm($category).await.expect(concat!(
                "pjdfstest category ",
                $category,
                " failed in VM"
            ));
        }
    };
}

// All 17 pjdfstest categories - each runs in a separate VM
pjdfstest_vm_category!(test_pjdfstest_vm_chflags, "chflags");
pjdfstest_vm_category!(test_pjdfstest_vm_chmod, "chmod");
pjdfstest_vm_category!(test_pjdfstest_vm_chown, "chown");
pjdfstest_vm_category!(test_pjdfstest_vm_ftruncate, "ftruncate");
pjdfstest_vm_category!(test_pjdfstest_vm_granular, "granular");
pjdfstest_vm_category!(test_pjdfstest_vm_link, "link");
pjdfstest_vm_category!(test_pjdfstest_vm_mkdir, "mkdir");
pjdfstest_vm_category!(test_pjdfstest_vm_mkfifo, "mkfifo");
pjdfstest_vm_category!(test_pjdfstest_vm_mknod, "mknod");
// DISABLED: open test fails with FUSE_WRITEBACK_CACHE (O_WRONLY → O_RDWR promotion)
// See fuse-pipe/tests/pjdfstest_matrix_root.rs for detailed explanation.
// pjdfstest_vm_category!(test_pjdfstest_vm_open, "open");
pjdfstest_vm_category!(test_pjdfstest_vm_posix_fallocate, "posix_fallocate");
pjdfstest_vm_category!(test_pjdfstest_vm_rename, "rename");
pjdfstest_vm_category!(test_pjdfstest_vm_rmdir, "rmdir");
pjdfstest_vm_category!(test_pjdfstest_vm_symlink, "symlink");
pjdfstest_vm_category!(test_pjdfstest_vm_truncate, "truncate");
pjdfstest_vm_category!(test_pjdfstest_vm_unlink, "unlink");

// NOTE: utimensat requires kernel patch 0002-fuse-fix-utimensat-with-default-permissions.patch
// Tested in: tests/test_utimensat_fix.rs (runs with nested kernel that has the patch)
// pjdfstest_vm_category!(test_pjdfstest_vm_utimensat, "utimensat");
