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
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_graceful_shutdown() -> Result<()> {
    use std::time::Duration;

    println!("\nGraceful shutdown test");
    println!("======================");
    println!("Verifies VM exits cleanly when container finishes (no SIGTERM)");

    let (vm_name, _, _, _) = common::unique_names("graceful");

    // Start VM with alpine:latest which exits immediately
    println!("Starting VM with container that exits immediately...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "alpine:latest", // Exits immediately with code 0
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
                println!("  VM exited after {:.1}s with status: {}", elapsed.as_secs_f32(), status);

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
    assert!(events.iter().any(|e| e.contains("kvm_exit")), "Should have kvm_exit event");

    // Enable some events
    tracer.enable_events(&["kvm:kvm_exit", "kvm:kvm_entry"])?;
    println!("  Enabled kvm_exit and kvm_entry events");

    // Start tracing
    tracer.start()?;

    // Run a quick VM to generate trace events
    let (vm_name, _, _, _) = common::unique_names("ftrace-test");
    let (mut child, _) = common::spawn_fcvm(&[
        "podman", "run", "--name", &vm_name, "--network", "bridged",
        "alpine:latest", "true",
    ]).await?;

    // Wait for exit
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        child.wait()
    ).await??;

    // Stop and read
    tracer.stop()?;
    let trace = tracer.read_grep("kvm_exit", 20)?;
    println!("  Trace output (last 20 kvm_exit lines):");
    for line in trace.lines().take(5) {
        println!("    {}", line);
    }

    assert!(trace.contains("kvm_exit"), "Should have captured kvm_exit events");
    println!("✅ FTRACE SANITY PASSED!");
    Ok(())
}

/// Test trailing args syntax: fcvm podman run ... image cmd args
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_trailing_args_command() -> Result<()> {
    use std::time::Duration;

    println!("\nTest trailing args command syntax");
    println!("==================================");

    let (vm_name, _, _, _) = common::unique_names("trailing-args");

    // Use trailing args: alpine:latest echo "test-marker-12345"
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "alpine:latest",
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
