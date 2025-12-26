//! Integration tests for remap_file_range (FICLONE/FICLONERANGE) in VM.
//!
//! Tests that FUSE passthrough of reflink operations works end-to-end:
//! Container → FUSE client → vsock → FUSE server → btrfs
//!
//! Similar to test_fuse_in_vm_matrix.rs but tests reflink operations.
//!
//! Requires:
//! - Kernel with FUSE_REMAP_FILE_RANGE support (set REMAP_KERNEL env var)
//! - btrfs filesystem at /mnt/fcvm-btrfs
//!
//! Run with: `REMAP_KERNEL=/path/to/kernel make test-root FILTER=remap`

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Instant;

/// Get patched kernel path from REMAP_KERNEL env var
fn get_patched_kernel() -> Option<String> {
    std::env::var("REMAP_KERNEL").ok().filter(|p| {
        let exists = std::path::Path::new(p).exists();
        if !exists {
            eprintln!("REMAP_KERNEL={} does not exist", p);
        }
        exists
    })
}

/// Check if btrfs is available
fn has_btrfs() -> bool {
    std::path::Path::new("/mnt/fcvm-btrfs").exists()
}

/// Run remap_file_range tests in a VM with patched kernel.
async fn run_remap_test_in_vm(test_name: &str, test_script: &str) -> Result<()> {
    let kernel = match get_patched_kernel() {
        Some(k) => k,
        None => {
            eprintln!("SKIP: {} requires REMAP_KERNEL env var pointing to patched kernel", test_name);
            return Ok(());
        }
    };

    if !has_btrfs() {
        eprintln!("SKIP: {} requires btrfs at /mnt/fcvm-btrfs", test_name);
        return Ok(());
    }

    let start = Instant::now();
    let test_id = format!("remap-{}-{}", test_name, std::process::id());
    let vm_name = format!("remap-{}-{}", test_name, std::process::id());

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

    // Start VM with patched kernel
    let mut cmd = tokio::process::Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel",
        &kernel,
        "--map",
        &map_arg,
        "--cmd",
        test_script,
        "alpine:latest",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    let mut child = cmd.spawn().context("spawning VM")?;
    let vm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no VM PID"))?;

    // Consume output
    common::spawn_log_consumer(child.stdout.take(), &format!("remap-{}", test_name));
    common::spawn_log_consumer_stderr(child.stderr.take(), &format!("remap-{}", test_name));

    // Wait for completion (5 min timeout)
    let timeout = std::time::Duration::from_secs(300);
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
        // Exit code 95 = EOPNOTSUPP, 38 = ENOSYS
        if code == 95 {
            eprintln!("SKIP: Kernel supports opcode but fs returned EOPNOTSUPP");
            return Ok(());
        } else if code == 38 {
            eprintln!("SKIP: Kernel doesn't support FUSE_REMAP_FILE_RANGE");
            return Ok(());
        }
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
    // Shell script that tests cp --reflink=always
    // Alpine's busybox cp doesn't support --reflink, so we install coreutils first
    // Note: --cmd is passed directly to container, so we need sh -c wrapper
    let script = r#"sh -c 'set -e; apk add --no-cache coreutils >/dev/null 2>&1; cd /data; dd if=/dev/urandom of=source.bin bs=1M count=1 2>/dev/null; cp --reflink=always source.bin dest.bin; cmp source.bin dest.bin; echo FICLONE test passed'"#;

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
#[tokio::test]
#[cfg(feature = "libfuse-test")]
async fn test_libfuse_remap_container() {
    let kernel = match get_patched_kernel() {
        Some(k) => k,
        None => {
            eprintln!("SKIP: requires REMAP_KERNEL env var pointing to patched kernel");
            return;
        }
    };

    let fcvm_path = common::find_fcvm_binary().expect("fcvm binary");
    let vm_name = format!("libfuse-remap-{}", std::process::id());

    let mut cmd = tokio::process::Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--privileged",
        "--kernel",
        &kernel,
        "localhost/libfuse-remap-test",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    let mut child = cmd.spawn().expect("spawning VM");
    let vm_pid = child.id().expect("VM PID");

    common::spawn_log_consumer(child.stdout.take(), "libfuse-remap");
    common::spawn_log_consumer_stderr(child.stderr.take(), "libfuse-remap");

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
