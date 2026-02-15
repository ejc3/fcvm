//! Integration test for localhost/ container images
//!
//! Tests both import paths:
//! - Default (direct mount): Pre-built podman storage mounted as additionalImageStore
//! - Legacy (--no-direct-image-mount): Docker archive imported via podman load

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Test with default direct image mount path
#[tokio::test]
async fn test_localhost_hello_world() -> Result<()> {
    run_localhost_test(false).await
}

/// Test with legacy podman load path (--no-direct-image-mount)
#[tokio::test]
async fn test_localhost_hello_world_legacy_import() -> Result<()> {
    run_localhost_test(true).await
}

async fn run_localhost_test(no_direct_image_mount: bool) -> Result<()> {
    let mode = if no_direct_image_mount {
        "legacy (podman load)"
    } else {
        "direct mount (additionalImageStore)"
    };

    println!("\nLocalhost Image Test ({})", mode);
    println!("====================");

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;
    let suffix = if no_direct_image_mount {
        "localhost-legacy"
    } else {
        "localhost-direct"
    };
    let (vm_name, _, _, _) = common::unique_names(suffix);

    // Step 1: Build a test container image on the host
    println!("Step 1: Building test container image localhost/test-hello...");
    build_test_image().await?;

    // Step 2: Start VM with localhost image (rootless mode)
    println!(
        "Step 2: Starting VM with localhost/test-hello image ({})...",
        mode
    );
    let mut args = vec!["podman", "run", "--name", &vm_name];
    if no_direct_image_mount {
        args.push("--no-direct-image-mount");
    }
    args.push("localhost/test-hello");

    let mut child = tokio::process::Command::new(&fcvm_path)
        .args(&args)
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
        println!("\n  LOCALHOST IMAGE TEST PASSED! ({})", mode);
        println!("  - Container ran and printed: Hello from localhost container!");
        if container_exited_zero {
            println!("  - Container exited with code 0");
        }
        Ok(())
    } else {
        println!("\n  LOCALHOST IMAGE TEST FAILED! ({})", mode);
        println!("  - Did not find expected output: 'Hello from localhost container!'");
        println!("  - Check logs above for error details");
        anyhow::bail!("Localhost image test failed ({})", mode)
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
