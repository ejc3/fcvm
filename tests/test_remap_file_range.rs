//! Integration tests for remap_file_range (FICLONE/FICLONERANGE) in VM.
//!
//! Tests that FUSE passthrough of reflink operations works end-to-end:
//! Container → FUSE client → vsock → FUSE server → btrfs
//!
//! Similar to test_fuse_in_vm_matrix.rs but tests reflink operations.
//!
//! Requires:
//! - btrfs filesystem at /mnt/fcvm-btrfs
//! - nested kernel profile set up (via fcvm setup --kernel-profile nested)
//!
//! Uses the nested kernel profile which includes the FUSE remap_file_range patch.
//! Run with: `make test-root FILTER=remap`

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Instant;

/// Check if btrfs is available
fn has_btrfs() -> bool {
    std::path::Path::new("/mnt/fcvm-btrfs").exists()
}

/// Run remap_file_range tests in a VM.
/// Uses nested kernel profile which has the FUSE remap patch.
async fn run_remap_test_in_vm(test_name: &str, test_script: &str) -> Result<()> {
    if !has_btrfs() {
        eprintln!("SKIP: {} requires btrfs at /mnt/fcvm-btrfs", test_name);
        return Ok(());
    }

    let start = Instant::now();
    let test_id = format!("remap-{}-{}", test_name, std::process::id());
    let vm_name = format!("remap-{}-{}", test_name, std::process::id());

    // Create logger for file output
    let logger = common::TestLogger::new(&format!("remap-{}", test_name));

    // Create btrfs-backed temp directory
    let data_dir = format!("/mnt/fcvm-btrfs/test-{}", test_id);
    tokio::fs::create_dir_all(&data_dir).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777)).await?;
    }

    let map_arg = format!("{}:/data", data_dir);
    let fcvm_path = common::find_fcvm_binary()?;

    // Start VM with nested kernel profile
    let mut cmd = tokio::process::Command::new(&fcvm_path);
    let args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        // No pre-start snapshot: this is a one-shot test that never restores, so
        // the snapshot is pure overhead. Worse, creating it pauses/resumes the
        // NV2 nested-kernel VM, which intermittently wedges vsock recovery on
        // resume (exec/status connections drop and only recover at the host's
        // 300s status timeout, blowing the test budget). The sibling nested test
        // test_copy_file_range_in_vm disables snapshots for the same reason.
        "--no-snapshot",
        "--map",
        &map_arg,
        "--cmd",
        test_script,
        common::TEST_IMAGE, // Use ECR to avoid Docker Hub rate limits
    ];
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    common::set_test_pdeathsig(&mut cmd);
    let mut child = cmd.spawn().context("spawning VM")?;
    let vm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no VM PID"))?;
    logger.info(&format!("Spawned VM PID={}", vm_pid));

    // Consume output with file logging
    common::spawn_log_consumer_with_logger(
        child.stdout.take(),
        &format!("remap-{}", test_name),
        logger.clone(),
    );
    common::spawn_log_consumer_stderr_with_logger(
        child.stderr.take(),
        &format!("remap-{}", test_name),
        logger.clone(),
    );

    // Wait for completion. This single budget covers the whole VM lifecycle (cold
    // bridged boot, in-guest image/container startup, the test script, and
    // shutdown), so keep it just under the 600s nextest slow-timeout for VM tests.
    let timeout = std::time::Duration::from_secs(570);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    let exit_status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let _ = tokio::fs::remove_dir_all(&data_dir).await;
            anyhow::bail!("Error waiting for VM: {}", e)
        }
        Err(_) => {
            common::kill_process(vm_pid).await;
            let _ = tokio::fs::remove_dir_all(&data_dir).await;
            anyhow::bail!("VM timeout after {} seconds", timeout.as_secs());
        }
    };

    let duration = start.elapsed();

    // Check for shared extents before cleanup
    if exit_status.success() {
        verify_shared_extents(&data_dir);
    }

    // Cleanup
    let _ = tokio::fs::remove_dir_all(&data_dir).await;

    if !exit_status.success() {
        let code = exit_status.code().unwrap_or(-1);
        anyhow::bail!(
            "{} failed: exit={} ({:.1}s)",
            test_name,
            code,
            duration.as_secs_f64()
        );
    }

    println!(
        "[REMAP-VM] ✓ {} ({:.1}s)",
        test_name,
        duration.as_secs_f64()
    );

    Ok(())
}

/// Verify shared extents using filefrag
fn verify_shared_extents(data_dir: &str) {
    let src = format!("{}/source.bin", data_dir);
    let dst = format!("{}/dest.bin", data_dir);

    if !std::path::Path::new(&src).exists() || !std::path::Path::new(&dst).exists() {
        return;
    }

    if let Ok(output) = std::process::Command::new("filefrag")
        .args(["-v", &src, &dst])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("shared") {
            println!("  ✓ Verified: files share physical extents (true reflink)");
        }
    }
}

/// Test FICLONE (whole file clone) via cp --reflink=always
#[tokio::test]
async fn test_ficlone_cp_reflink_in_vm() {
    // Shell script that tests cp --reflink=always, including the request-length
    // path above u32::MAX. The running host kernel may predate this branch, but
    // this VM boots the exact 7.1.7 profile kernel produced by the branch.
    // Alpine's busybox cp doesn't support --reflink, so we install coreutils first
    // Note: --cmd is passed directly to container, so we need sh -c wrapper
    let script = r#"sh -c 'set -e; apk add --no-cache coreutils >/dev/null 2>&1; cd /data; size=5368709120; printf fcvm-remap > source.bin; truncate -s "$size" source.bin; printf Z | dd of=source.bin bs=1 seek=$((size - 1)) conv=notrunc status=none; cp --reflink=always source.bin dest.bin; actual=$(stat -c %s dest.bin); test "$actual" -eq "$size" || { echo "FICLONE size mismatch: got $actual expected $size" >&2; exit 1; }; head -c 10 source.bin > source.head; head -c 10 dest.bin > dest.head; cmp source.head dest.head; tail -c 1 source.bin > source.tail; tail -c 1 dest.bin > dest.tail; cmp source.tail dest.tail; echo FICLONE 5GiB test passed'"#;

    run_remap_test_in_vm("ficlone", script)
        .await
        .expect("FICLONE test failed");
}

/// Test libfuse remap_file_range via container.
///
/// Runs the localhost/libfuse-remap-test container which:
/// 1. Creates a btrfs loopback filesystem
/// 2. Runs passthrough_ll (patched libfuse) on top of it
/// 3. Tests FICLONE through FUSE -> btrfs
///
/// Build container first:
///   podman build -t localhost/libfuse-remap-test -f Containerfile.libfuse-remap .
///
/// Gated by libfuse-test feature since it requires the container to be pre-built.
/// Uses nested kernel profile which has the FUSE remap patch.
#[tokio::test]
#[cfg(feature = "libfuse-test")]
async fn test_libfuse_remap_container() {
    // Create logger for file output
    let logger = common::TestLogger::new("libfuse-remap");

    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary");
    let vm_name = format!("libfuse-remap-{}", std::process::id());

    let mut cmd = tokio::process::Command::new(&fcvm_path);
    let args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--privileged",
        "localhost/libfuse-remap-test",
    ];
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    common::set_test_pdeathsig(&mut cmd);
    let mut child = cmd.spawn().expect("spawning VM");
    let vm_pid = child.id().expect("VM PID");
    logger.info(&format!("Spawned VM PID={}", vm_pid));

    common::spawn_log_consumer_with_logger(child.stdout.take(), "libfuse-remap", logger.clone());
    common::spawn_log_consumer_stderr_with_logger(
        child.stderr.take(),
        "libfuse-remap",
        logger.clone(),
    );

    let timeout = std::time::Duration::from_secs(180);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    let exit_status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => panic!("Error waiting for VM: {}", e),
        Err(_) => {
            common::kill_process(vm_pid).await;
            panic!("VM timeout after {} seconds", timeout.as_secs());
        }
    };

    assert!(
        exit_status.success(),
        "libfuse-remap-test container failed with exit code {:?}",
        exit_status.code()
    );

    println!("[REMAP-VM] ✓ libfuse container test passed");
}
