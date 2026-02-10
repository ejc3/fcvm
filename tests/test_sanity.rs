//! Sanity integration test - verifies basic VM startup and health checks
//!
//! Uses common::spawn_fcvm() to prevent pipe buffer deadlock.
//! See CLAUDE.md "Pipe Buffer Deadlock in Tests" for details.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_sanity_bridged() -> Result<()> {
    sanity_test_impl("bridged").await
}

#[tokio::test]
async fn test_sanity_rootless() -> Result<()> {
    sanity_test_impl("rootless").await
}

async fn sanity_test_impl(network: &str) -> Result<()> {
    use std::time::Duration;

    println!("\nfcvm sanity test (network: {})", network);
    println!("================");
    println!("Starting a single VM to verify health checks work");

    // Start the VM using spawn_fcvm helper (uses Stdio::inherit to prevent deadlock)
    println!("Starting VM...");
    let (vm_name, _, _, _) = common::unique_names(&format!("sanity-{}", network));
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        network,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run")?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    println!("  Waiting for VM to become healthy...");

    // Spawn health check task
    // Use 300 second timeout to account for rootfs creation on first run
    // (cloud image download ~7s, virt-customize ~10-60s, extraction ~30s, packages ~60s)
    let health_task = tokio::spawn(common::poll_health_by_pid(fcvm_pid, 300));

    // Monitor process for unexpected exits
    let monitor_task: tokio::task::JoinHandle<Result<(), anyhow::Error>> =
        tokio::spawn(async move {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return Err(anyhow::anyhow!(
                            "fcvm process exited unexpectedly with status: {}",
                            status
                        ));
                    }
                    Ok(None) => {
                        // Still running
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to check process status: {}", e));
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

    // Wait for either health check or process exit
    let result = tokio::select! {
        health_result = health_task => {
            match health_result {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("Health check task panicked: {}", e)),
            }
        }
        monitor_result = monitor_task => {
            match monitor_result {
                Ok(Err(e)) => Err(e),
                Ok(Ok(_)) => unreachable!("Monitor task should never return Ok"),
                Err(e) => Err(anyhow::anyhow!("Monitor task panicked: {}", e)),
            }
        }
    };

    // Cleanup
    println!("  Stopping fcvm process...");
    common::kill_process(fcvm_pid).await;

    // Print result
    match &result {
        Ok(_) => {
            println!("✅ SANITY TEST PASSED!");
            println!("  Health checks are working correctly!");
        }
        Err(e) => {
            println!("❌ SANITY TEST FAILED!");
            println!("  Error: {}", e);
        }
    }

    result
}

/// Test that VM exits gracefully when container finishes (PSCI shutdown)
/// This tests the full shutdown path: container exit → fc-agent poweroff -f → PSCI SYSTEM_OFF → KVM exit
#[tokio::test]
async fn test_graceful_shutdown() -> Result<()> {
    use std::time::Duration;

    println!("\nGraceful shutdown test");
    println!("======================");
    println!("Verifies VM exits cleanly when container finishes (no SIGTERM)");

    let (vm_name, _, _, _) = common::unique_names("graceful");

    // Start VM with container that exits immediately (rootless mode)
    // Use public ECR image to avoid Docker Hub rate limits
    println!("Starting VM with container that exits immediately...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        common::TEST_IMAGE, // nginx:alpine from ECR
        "true",             // Exit immediately with code 0
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to exit gracefully (max 60s)...");

    // Wait for process to exit on its own (NO kill!)
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        match child.try_wait()? {
            Some(status) => {
                let elapsed = start.elapsed();
                println!(
                    "  VM exited after {:.1}s with status: {}",
                    elapsed.as_secs_f32(),
                    status
                );

                if status.success() {
                    println!("✅ GRACEFUL SHUTDOWN PASSED!");
                    println!("  PSCI shutdown worked correctly");
                    return Ok(());
                } else {
                    anyhow::bail!("VM exited with non-zero status: {}", status);
                }
            }
            None => {
                if start.elapsed() > timeout {
                    // Kill the stuck process before failing
                    common::kill_process(fcvm_pid).await;
                    anyhow::bail!(
                        "VM did not exit within {}s - PSCI shutdown is broken!",
                        timeout.as_secs()
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Test Ftrace utility works for kernel tracing
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_ftrace_sanity() -> Result<()> {
    println!("\nTest Ftrace utility");
    println!("===================");

    // Create tracer
    let tracer = common::Ftrace::new().context("creating Ftrace")?;

    // List KVM events
    let events = tracer.list_kvm_events()?;
    println!("  Available KVM events: {}", events.len());
    assert!(!events.is_empty(), "Should have KVM events");
    assert!(
        events.iter().any(|e| e.contains("kvm_exit")),
        "Should have kvm_exit event"
    );

    // Enable some events
    tracer.enable_events(&["kvm:kvm_exit", "kvm:kvm_entry"])?;
    println!("  Enabled kvm_exit and kvm_entry events");

    // Start tracing
    tracer.start()?;

    // Run a quick VM to generate trace events
    let (vm_name, _, _, _) = common::unique_names("ftrace-test");
    let (mut child, _) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        common::TEST_IMAGE, // Use ECR to avoid Docker Hub rate limits
        "true",
    ])
    .await?;

    // Wait for exit
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await??;

    // Stop and read
    tracer.stop()?;
    let trace = tracer.read_grep("kvm_exit", 20)?;
    println!("  Trace output (last 20 kvm_exit lines):");
    for line in trace.lines().take(5) {
        println!("    {}", line);
    }

    assert!(
        trace.contains("kvm_exit"),
        "Should have captured kvm_exit events"
    );
    println!("✅ FTRACE SANITY PASSED!");
    Ok(())
}

/// Test trailing args syntax: fcvm podman run ... image cmd args
#[tokio::test]
async fn test_trailing_args_command() -> Result<()> {
    use std::time::Duration;

    println!("\nTest trailing args command syntax");
    println!("==================================");

    let (vm_name, _, _, _) = common::unique_names("trailing-args");

    // Use trailing args: image echo "test-marker-12345" (rootless mode)
    // Use ECR image to avoid Docker Hub rate limits
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        common::TEST_IMAGE,
        "echo",
        "test-marker-12345",
    ])
    .await
    .context("spawning fcvm with trailing args")?;

    println!("  fcvm PID: {}", fcvm_pid);

    // Wait for exit
    let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .context("timeout waiting for VM")?
        .context("waiting for child")?;

    println!("  Exit status: {}", status);
    assert!(status.success(), "VM should exit successfully");
    println!("✅ TRAILING ARGS TEST PASSED!");
    Ok(())
}

/// Test that container stdout streams to host after snapshot.
///
/// Snapshot creation resets all vsock connections (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET).
/// The output listener must re-accept connections so container output continues flowing.
/// Without the fix, output after snapshot is silently lost.
#[tokio::test]
async fn test_output_survives_snapshot() -> Result<()> {
    use std::time::Duration;

    println!("\nOutput after snapshot test");
    println!("=========================");
    println!("Verifies container stdout streams to host after snapshot vsock reset");

    let (vm_name, _, _, _) = common::unique_names("output-snap");
    let marker = format!("SNAPSHOT-OUTPUT-MARKER-{}", std::process::id());

    // Run container that prints output before and after the snapshot window.
    // Snapshots are enabled by default, so the output listener must survive
    // the vsock reset after snapshot creation.
    //
    // Timeline:
    //   0s: container starts, prints pre-snapshot lines immediately
    //   ~2-5s: snapshot happens (image already cached from previous test)
    //   5s: container prints post-snapshot marker
    //   6s: container prints many lines to stress pipe buffer
    //
    // This catches both the vsock reconnect bug AND the pipe buffer deadlock.
    let script = format!(
        "echo 'PRE-SNAPSHOT-LINE-1'; \
         echo 'PRE-SNAPSHOT-LINE-2'; \
         sleep 5; \
         echo '{}'; \
         for i in $(seq 1 100); do echo \"OUTPUT-LINE-$i\"; done; \
         echo 'ALL-OUTPUT-DONE'",
        marker
    );
    println!("  Starting VM with marker: {}", marker);
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        common::ALPINE_IMAGE,
        "sh",
        "-c",
        &script,
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to exit (max 120s)...");

    // Wait for process to exit
    let status = tokio::time::timeout(Duration::from_secs(120), child.wait())
        .await
        .context("timeout waiting for VM")?
        .context("waiting for child")?;

    println!("  Exit status: {}", status);
    assert!(status.success(), "VM should exit successfully");

    // Check the debug log for our marker in actual container output lines.
    // The marker must appear as stdout from the output listener (prefixed "[name]"),
    // NOT just in the command args or plan response body.
    let log_dir = "/tmp/fcvm-test-logs";
    let mut found_marker = false;
    if let Ok(entries) = std::fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.contains("output-snap") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    // Look for the marker in stdout lines (output listener forwards as
                    // "[name] content" for stdout). Exclude lines containing "args=",
                    // "plan response", or "cmd" which just echo the command, not output.
                    for line in contents.lines() {
                        if line.contains(&marker)
                            && !line.contains("args=")
                            && !line.contains("plan response")
                            && !line.contains("\"cmd\"")
                        {
                            println!("  Found marker in container output: {}", line.trim());
                            found_marker = true;
                            break;
                        }
                    }
                    if found_marker {
                        break;
                    }
                }
            }
        }
    }

    assert!(
        found_marker,
        "Container output marker '{}' not found in test logs — output listener \
         did not survive snapshot vsock reset",
        marker
    );

    // Also verify the bulk output didn't get stuck (pipe buffer deadlock)
    let mut found_done = false;
    if let Ok(entries) = std::fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.contains("output-snap") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if contents.contains("ALL-OUTPUT-DONE") {
                        found_done = true;
                        break;
                    }
                }
            }
        }
    }
    assert!(
        found_done,
        "ALL-OUTPUT-DONE sentinel not found — pipe buffer deadlock after snapshot"
    );

    println!("✅ OUTPUT AFTER SNAPSHOT PASSED!");
    println!("  Container stdout survived snapshot vsock reset");
    println!("  100 lines + sentinel all received (no pipe deadlock)");
    Ok(())
}

/// Test that VM shuts down when container fails to start (e.g., invalid image)
///
/// This is critical: if the container can't start (image pull fails, etc.),
/// the VM should exit instead of hanging indefinitely.
#[tokio::test]
async fn test_container_startup_failure_triggers_shutdown() -> Result<()> {
    use std::time::Duration;

    println!("\nContainer startup failure test");
    println!("===============================");
    println!("Verifies VM exits when container fails to start (no hang)");

    let (vm_name, _, _, _) = common::unique_names("startup-fail");

    // Use a nonexistent image that will definitely fail to pull
    // This tests that fc-agent properly triggers VM shutdown on error
    println!("Starting VM with nonexistent image...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "nonexistent.invalid/this-image-does-not-exist:v999",
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to exit (max 120s)...");

    // Wait for process to exit - should NOT hang
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(120);

    loop {
        match child.try_wait()? {
            Some(status) => {
                let elapsed = start.elapsed();
                println!(
                    "  VM exited after {:.1}s with status: {}",
                    elapsed.as_secs_f32(),
                    status
                );

                // VM should exit with non-zero status (container failed to start)
                assert!(
                    !status.success(),
                    "VM should exit with error status when container fails to start"
                );
                println!("✅ STARTUP FAILURE SHUTDOWN PASSED!");
                println!("  fc-agent correctly triggered VM shutdown on error");
                return Ok(());
            }
            None => {
                if start.elapsed() > timeout {
                    // Kill the stuck process before failing
                    common::kill_process(fcvm_pid).await;
                    anyhow::bail!(
                        "VM did not exit within {}s - fc-agent is NOT shutting down on startup failure!",
                        timeout.as_secs()
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
