//! Integration test for localhost/ container images
//!
//! This test verifies that locally-built container images can be run inside VMs.
//! The image is exported from the host using skopeo, mounted into the VM via FUSE,
//! and then imported by fc-agent using skopeo before running with podman.

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Test that a localhost/ container image can be built and run in a VM
#[tokio::test]
async fn test_localhost_hello_world() -> Result<()> {
    println!("\nLocalhost Image Test");
    println!("====================");
    println!("Testing that localhost/ container images work via skopeo");

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Build a test container image on the host
    println!("Step 1: Building test container image localhost/test-hello...");
    build_test_image().await?;

    // Step 2: Start VM with localhost image
    println!("Step 2: Starting VM with localhost/test-hello image...");
    let mut child = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            "test-localhost-hello",
            "--network",
            "bridged",
            "localhost/test-hello",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm podman run")?;

    let fcvm_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get child PID"))?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Collect output to check for "Hello from localhost container!"
    let mut found_hello = false;
    let mut container_exited = false;

    // Spawn task to collect stdout
    let stdout = child.stdout.take();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[VM stdout] {}", line);
            }
        }
    });

    // Monitor stderr for the expected output
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut found = false;
        let mut exited = false;
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[VM stderr] {}", line);
                if line.contains("Hello from localhost container!") {
                    found = true;
                }
                if line.contains("container exited successfully") {
                    exited = true;
                }
            }
        }
        (found, exited)
    });

    // Wait for the process to exit (with timeout)
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
            println!("  Timeout waiting for VM ({}s), killing...", timeout.as_secs());
            common::kill_process(fcvm_pid).await;
        }
    }

    // Wait for output tasks
    let _ = stdout_task.await;
    if let Ok((found, exited)) = stderr_task.await {
        found_hello = found;
        container_exited = exited;
    }

    // Check results
    if found_hello && container_exited {
        println!("\n✅ LOCALHOST IMAGE TEST PASSED!");
        println!("  - Image exported via skopeo on host");
        println!("  - Image imported via skopeo in guest");
        println!("  - Container ran and printed expected output");
        Ok(())
    } else {
        println!("\n❌ LOCALHOST IMAGE TEST FAILED!");
        if !found_hello {
            println!("  - Did not find expected output: 'Hello from localhost container!'");
        }
        if !container_exited {
            println!("  - Container did not exit successfully");
        }
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

    // Write a simple Containerfile
    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM alpine:latest
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
