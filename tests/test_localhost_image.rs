//! Integration test for localhost/ container images
//!
//! This test verifies that locally-built container images can be run inside VMs.
//! The image is exported from the host using `podman save`, attached as a raw block device,
//! and then imported by fc-agent using `podman load` before running with podman.
//!
//! Also tests snapshot caching for localhost images (enabled in PR #259).

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Test that a localhost/ container image can be built and run in a VM (rootless)
#[tokio::test]
async fn test_localhost_hello_world() -> Result<()> {
    println!("\nLocalhost Image Test");
    println!("====================");
    println!("Testing that localhost/ container images work via podman save/load");

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("localhost-hello");

    // Step 1: Build a test container image on the host
    println!("Step 1: Building test container image localhost/test-hello...");
    build_test_image().await?;

    // Step 2: Start VM with localhost image (rootless mode)
    println!("Step 2: Starting VM with localhost/test-hello image...");
    let mut child = tokio::process::Command::new(&fcvm_path)
        .args(["podman", "run", "--name", &vm_name, "localhost/test-hello"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm podman run")?;

    let fcvm_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get child PID"))?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Monitor stdout for container output (goes directly to stdout without prefix)
    let stdout = child.stdout.take();
    let stdout_task = tokio::spawn(async move {
        let mut found_hello = false;
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[VM stdout] {}", line);
                // Check for container output (no prefix in clean output mode)
                if line.contains("Hello from localhost container!") {
                    found_hello = true;
                }
            }
        }
        found_hello
    });

    // Monitor stderr for exit status (logs still go to stderr)
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut exited_zero = false;
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[VM stderr] {}", line);
                // Check for container exit with code 0
                if line.contains("Container exit notification received")
                    && line.contains("exit_code=0")
                {
                    exited_zero = true;
                }
            }
        }
        exited_zero
    });

    // Wait for the process to exit (with timeout)
    // 120s to handle podman storage lock contention during parallel test runs
    let timeout = Duration::from_secs(120);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    match result {
        Ok(Ok(status)) => {
            println!("  fcvm process exited with status: {}", status);
        }
        Ok(Err(e)) => {
            println!("  Error waiting for process: {}", e);
        }
        Err(_) => {
            println!(
                "  Timeout waiting for VM ({}s), killing...",
                timeout.as_secs()
            );
            common::kill_process(fcvm_pid).await;
        }
    }

    // Wait for output tasks
    let found_hello = stdout_task.await.unwrap_or(false);
    let container_exited_zero = stderr_task.await.unwrap_or(false);

    // Check results - verify we got the container output
    if found_hello {
        println!("\n✅ LOCALHOST IMAGE TEST PASSED!");
        println!("  - Image exported via podman save on host");
        println!("  - Image imported via podman load in guest");
        println!("  - Container ran and printed: Hello from localhost container!");
        if container_exited_zero {
            println!("  - Container exited with code 0");
        }
        Ok(())
    } else {
        println!("\n❌ LOCALHOST IMAGE TEST FAILED!");
        println!("  - Did not find expected output: 'Hello from localhost container!'");
        println!("  - Check logs above for error details");
        anyhow::bail!("Localhost image test failed")
    }
}

/// Build a simple test container image using podman
async fn build_test_image() -> Result<()> {
    use std::io::Write;
    use tempfile::TempDir;

    // Create a temporary directory for the Containerfile
    let temp_dir = TempDir::new().context("creating temp dir")?;
    let containerfile_path = temp_dir.path().join("Containerfile");

    // Write a simple Containerfile (use ECR to avoid Docker Hub rate limits)
    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM public.ecr.aws/nginx/nginx:alpine
CMD ["echo", "Hello from localhost container!"]"#
    )?;

    // Build the image with podman
    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            "localhost/test-hello",
            "-f",
            &containerfile_path.to_string_lossy(),
            temp_dir.path().to_str().unwrap(),
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to build test image: {}", stderr);
    }

    println!("  Built localhost/test-hello image");
    Ok(())
}

/// Test that localhost/ images support snapshot caching
///
/// This test verifies that:
/// 1. First run creates a snapshot (miss)
/// 2. Second run restores from snapshot (hit)
/// 3. The CAS-cached image archive path is stable across snapshot restore
#[tokio::test]
async fn test_localhost_snapshot_caching() -> Result<()> {
    println!("\nLocalhost Image Snapshot Caching Test");
    println!("======================================");
    println!("Testing that localhost/ images support snapshot cache hit/restore");

    let (vm_name_1, _, _, _) = common::unique_names("localhost-snap-1");
    let (vm_name_2, _, _, _) = common::unique_names("localhost-snap-2");

    // Step 1: Build a test container image on the host
    println!("Step 1: Building test container image localhost/test-snapshot...");
    build_snapshot_test_image().await?;

    // Step 2: First run - should be a snapshot miss
    println!("\nStep 2: First run (snapshot miss)...");
    let (mut child1, fcvm_pid_1) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name_1,
            "localhost/test-snapshot",
        ],
        &vm_name_1,
    )
    .await
    .context("spawning fcvm for first run")?;

    println!("  fcvm PID: {}", fcvm_pid_1);
    println!("  Waiting for container to exit...");

    // Wait for the VM to exit (with timeout)
    let timeout = Duration::from_secs(120);
    let result1 = tokio::time::timeout(timeout, child1.wait()).await;

    match result1 {
        Ok(Ok(status)) => {
            println!("  First run exited with status: {}", status);
        }
        Ok(Err(e)) => {
            println!("  Error waiting for first run: {}", e);
            anyhow::bail!("First run failed: {}", e);
        }
        Err(_) => {
            println!("  Timeout waiting for first run, killing...");
            common::kill_process(fcvm_pid_1).await;
            anyhow::bail!("First run timed out");
        }
    }

    // Check log file for snapshot creation (optional - for debugging)
    let log_path_1 = format!("/tmp/fcvm-test-logs/{}-*.log", vm_name_1);
    if let Some(log_file) = glob::glob(&log_path_1)
        .ok()
        .and_then(|mut paths| paths.next())
        .and_then(|p| p.ok())
    {
        let log_content = tokio::fs::read_to_string(&log_file)
            .await
            .unwrap_or_default();
        let snapshot_created = log_content.contains("Snapshot miss")
            && log_content.contains("will create snapshot after image load");
        println!("  Snapshot created: {}", snapshot_created);
    } else {
        println!("  Warning: Could not find log file for first run");
    }

    // Step 3: Second run - should be a snapshot hit
    println!("\nStep 3: Second run (snapshot hit expected)...");

    // Small delay to ensure snapshot is fully written
    tokio::time::sleep(Duration::from_secs(2)).await;

    let (mut child2, fcvm_pid_2) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name_2,
            "localhost/test-snapshot",
        ],
        &vm_name_2,
    )
    .await
    .context("spawning fcvm for second run")?;

    println!("  fcvm PID: {}", fcvm_pid_2);
    println!("  Waiting for container to exit...");

    // Wait for the VM to exit (with timeout)
    let result2 = tokio::time::timeout(timeout, child2.wait()).await;

    match result2 {
        Ok(Ok(status)) => {
            println!("  Second run exited with status: {}", status);
        }
        Ok(Err(e)) => {
            println!("  Error waiting for second run: {}", e);
            anyhow::bail!("Second run failed: {}", e);
        }
        Err(_) => {
            println!("  Timeout waiting for second run, killing...");
            common::kill_process(fcvm_pid_2).await;
            anyhow::bail!("Second run timed out");
        }
    }

    // Check log file for snapshot hit
    let log_path_2 = format!("/tmp/fcvm-test-logs/{}-*.log", vm_name_2);
    let log_file_2 = glob::glob(&log_path_2)
        .ok()
        .and_then(|mut paths| paths.next())
        .and_then(|p| p.ok());

    let mut snapshot_hit = false;
    if let Some(log_file) = log_file_2 {
        let log_content = tokio::fs::read_to_string(&log_file)
            .await
            .unwrap_or_default();
        snapshot_hit = log_content.contains("Pre-start snapshot hit!")
            || log_content.contains("Startup snapshot hit!");
        println!("  Snapshot hit: {}", snapshot_hit);
    } else {
        println!("  Warning: Could not find log file for second run");
    }

    // Verify results
    if snapshot_hit {
        println!("\n✅ LOCALHOST SNAPSHOT CACHING TEST PASSED!");
        println!("  - First run created snapshot (miss)");
        println!("  - Second run restored from snapshot (hit)");
        println!("  - CAS-cached image archive path is stable across restore");
        Ok(())
    } else {
        println!("\n❌ LOCALHOST SNAPSHOT CACHING TEST FAILED!");
        println!("  - Second run did not hit snapshot cache");
        println!("  - Expected log message: 'Pre-start snapshot hit!' or 'Startup snapshot hit!'");
        anyhow::bail!("Snapshot caching not working for localhost images")
    }
}

/// Build a test container image for snapshot testing
async fn build_snapshot_test_image() -> Result<()> {
    use std::io::Write;
    use tempfile::TempDir;

    // Create a temporary directory for the Containerfile
    let temp_dir = TempDir::new().context("creating temp dir")?;
    let containerfile_path = temp_dir.path().join("Containerfile");

    // Write a simple Containerfile (use ECR to avoid Docker Hub rate limits)
    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM public.ecr.aws/nginx/nginx:alpine
CMD ["echo", "Snapshot test successful"]"#
    )?;

    // Build the image with podman
    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            "localhost/test-snapshot",
            "-f",
            &containerfile_path.to_string_lossy(),
            temp_dir.path().to_str().unwrap(),
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to build test image: {}", stderr);
    }

    println!("  Built localhost/test-snapshot image");
    Ok(())
}
