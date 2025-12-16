//! Sanity integration test - verifies basic VM startup and health checks
//!
//! Uses common::spawn_fcvm() to prevent pipe buffer deadlock.
//! See CLAUDE.md "Pipe Buffer Deadlock in Tests" for details.

mod common;

use anyhow::{Context, Result};

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
    let vm_name = format!("sanity-test-{}", network);
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
    let health_task = tokio::spawn(common::poll_health_by_pid(fcvm_pid, 60));

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
