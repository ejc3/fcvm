//! FUSE-in-VM integration test
//!
//! Tests fuse-pipe by running pjdfstest inside a Firecracker VM:
//! 1. Create temp directory with test data
//! 2. Start VM with --map to mount the directory via fuse-pipe
//! 3. Run pjdfstest container inside VM against the FUSE mount
//! 4. Verify all tests pass
//!
//! This tests the full fuse-pipe stack:
//! - Host: VolumeServer serving directory via vsock
//! - Guest: fc-agent mounting via fuse-pipe FuseClient
//! - Guest: pjdfstest container running against the mount

mod common;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Quick smoke test - run just posix_fallocate category (~100 tests)
#[tokio::test]
async fn test_fuse_in_vm_smoke() -> Result<()> {
    fuse_in_vm_test_impl("posix_fallocate", 8).await
}

/// Full pjdfstest suite in VM (8789 tests)
/// Run with: cargo test --test test_fuse_in_vm test_fuse_in_vm_full -- --ignored
#[tokio::test]
#[ignore]
async fn test_fuse_in_vm_full() -> Result<()> {
    fuse_in_vm_test_impl("all", 64).await
}

async fn fuse_in_vm_test_impl(category: &str, jobs: usize) -> Result<()> {
    // Full test suite needs privileged mode for mknod tests
    let privileged = category == "all";
    fuse_in_vm_test_impl_inner(category, jobs, privileged).await
}

async fn fuse_in_vm_test_impl_inner(category: &str, jobs: usize, privileged: bool) -> Result<()> {
    let test_id = format!("fuse-vm-{}", std::process::id());
    let test_start = Instant::now();

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     FUSE-in-VM Test: {} ({} jobs)                    ║",
        category, jobs
    );
    if privileged {
        println!("║     [PRIVILEGED MODE]                                         ║");
    }
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Paths
    let data_dir = PathBuf::from(format!("/tmp/fuse-{}-data", test_id));
    let vm_name = format!("fuse-vm-{}", std::process::id());

    // Cleanup from previous runs
    let _ = tokio::fs::remove_dir_all(&data_dir).await;

    // Create data directory for the FUSE mount
    tokio::fs::create_dir_all(&data_dir).await?;

    // Set permissions for pjdfstest (needs write access)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777)).await?;
    }

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;

    // =========================================================================
    // Step 1: Build pjdfstest container if needed
    // =========================================================================
    println!("Step 1: Ensuring pjdfstest container exists...");
    let step1_start = Instant::now();

    // Check if pjdfstest container exists (in root's storage)
    let check_output = tokio::process::Command::new("podman")
        .args(["image", "exists", "localhost/pjdfstest"])
        .output()
        .await?;

    if !check_output.status.success() {
        println!("  Building pjdfstest container (sudo podman build)...");
        let build_output = tokio::process::Command::new("podman")
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

        if !build_output.status.success() {
            anyhow::bail!(
                "Failed to build pjdfstest container: {}",
                String::from_utf8_lossy(&build_output.stderr)
            );
        }
    }
    println!(
        "  ✓ pjdfstest container ready (took {:.1}s)",
        step1_start.elapsed().as_secs_f64()
    );

    // =========================================================================
    // Step 2: Start VM with FUSE mount
    // =========================================================================
    println!("\nStep 2: Starting VM with FUSE-mounted directory...");
    let step2_start = Instant::now();

    // Map the data directory into the VM via fuse-pipe
    // The guest will mount it at /mnt/volumes/0 (default for first volume)
    let map_arg = format!("{}:/testdir", data_dir.display());

    // Build the pjdfstest command
    // Select tests based on category
    let prove_cmd = if category == "all" {
        format!("prove -v -j {} -r /opt/pjdfstest/tests/", jobs)
    } else {
        format!("prove -v -j {} -r /opt/pjdfstest/tests/{}/", jobs, category)
    };

    // Preserve SUDO_USER from the outer sudo (if any) so that fcvm can
    // find containers in the correct user's storage
    let mut cmd = tokio::process::Command::new(fcvm_path);
    let mut args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--map",
        &map_arg,
        "--cmd",
        &prove_cmd,
    ];
    // Add --privileged for full test suite (needed for mknod tests)
    if privileged {
        args.push("--privileged");
    }
    args.push("localhost/pjdfstest");
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // If SUDO_USER is set (we're running under sudo), preserve it
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    let mut vm_child = cmd.spawn().context("spawning VM")?;

    let vm_pid = vm_child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get VM PID"))?;

    // Spawn log consumers
    common::spawn_log_consumer(vm_child.stdout.take(), "vm");
    common::spawn_log_consumer_stderr(vm_child.stderr.take(), "vm");

    println!(
        "  ✓ VM started (PID: {}, took {:.1}s)",
        vm_pid,
        step2_start.elapsed().as_secs_f64()
    );

    // =========================================================================
    // Step 3: Wait for VM to complete
    // =========================================================================
    println!("\nStep 3: Waiting for pjdfstest to complete...");
    let step3_start = Instant::now();

    // Wait for VM process with timeout
    let timeout = if category == "all" {
        Duration::from_secs(3600) // 1 hour for full test
    } else {
        Duration::from_secs(600) // 10 minutes for single category
    };

    let result = tokio::time::timeout(timeout, vm_child.wait()).await;

    let exit_status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => anyhow::bail!("Error waiting for VM: {}", e),
        Err(_) => {
            common::kill_process(vm_pid).await;
            anyhow::bail!("VM timeout after {} seconds", timeout.as_secs());
        }
    };

    let test_time = step3_start.elapsed();
    println!(
        "  VM exited with status: {} (took {:.1}s)",
        exit_status,
        test_time.as_secs_f64()
    );

    // =========================================================================
    // Cleanup
    // =========================================================================
    println!("\nCleaning up...");
    let _ = tokio::fs::remove_dir_all(&data_dir).await;

    let total_time = test_start.elapsed();

    // =========================================================================
    // Results
    // =========================================================================
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Category:    {:>10}                                      ║",
        category
    );
    println!(
        "║  Jobs:        {:>10}                                      ║",
        jobs
    );
    println!(
        "║  Test time:   {:>10.1}s                                     ║",
        test_time.as_secs_f64()
    );
    println!(
        "║  Total time:  {:>10.1}s                                     ║",
        total_time.as_secs_f64()
    );
    println!(
        "║  Exit status: {:>10}                                      ║",
        exit_status.code().unwrap_or(-1)
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if !exit_status.success() {
        anyhow::bail!(
            "pjdfstest failed with exit code: {}",
            exit_status.code().unwrap_or(-1)
        );
    }

    println!("\n✅ FUSE-IN-VM TEST PASSED!");
    Ok(())
}
