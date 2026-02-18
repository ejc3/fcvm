//! Verifies that a container producing high-velocity stdout output doesn't deadlock.
//!
//! Root cause of the original bug: fc-agent's log writes to stderr (serial console)
//! were synchronous, blocking the tokio runtime. Under heavy FUSE traffic, thousands
//! of INFO messages/sec starved the async output handler that drains the container's
//! stdout pipe. The container's entrypoint deadlocked on pipe write.
//!
//! Fix: fc-agent uses tracing_appender::non_blocking for stderr, so log writes
//! never block the tokio runtime regardless of serial console throughput.
//!
//! This test runs a container that writes ~1000 lines/sec to stdout for 10 seconds,
//! then verifies the container is still responsive (not deadlocked).

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

#[tokio::test]
async fn test_high_velocity_stdout_no_deadlock() -> Result<()> {
    println!("\nHigh-velocity stdout: no deadlock");
    println!("==================================");

    let (vm_name, _, _, _) = common::unique_names("logflood");

    // Container command: write 1000 lines/sec for 10s, then sleep
    // Uses sh -c with a loop that prints timestamps at high rate
    let flood_cmd = "i=0; while [ $i -lt 10000 ]; do echo \"log line $i at $(date +%s%N)\"; i=$((i+1)); done; echo FLOOD_DONE; sleep 120";

    println!("  Starting VM with high-velocity logger...");
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            flood_cmd,
        ],
        &vm_name,
    )
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for healthy...");

    common::poll_health_by_pid(fcvm_pid, 300).await?;
    println!("  VM healthy");

    // Wait for the flood to complete (10000 lines should take ~2-5 seconds)
    println!("  Waiting for log flood to complete...");
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Verify container is still responsive (not deadlocked)
    println!("  Checking container responsiveness...");
    let result = common::exec_in_container(fcvm_pid, &["echo", "alive"]).await?;
    println!("  Container response: {}", result.trim());
    assert!(
        result.contains("alive"),
        "container should respond after log flood, got: {}",
        result.trim()
    );

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("  PASSED: high-velocity stdout didn't deadlock");
    Ok(())
}
