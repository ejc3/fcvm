//! Test that the guest serial console works after snapshot restore.
//!
//! After snapshot restore, systemd-journald crashes because its journal file was
//! mid-write during the snapshot. Since fc-agent.service uses journal+console,
//! stderr goes to a journal socket — with journald dead, that socket goes nowhere
//! and serial output stops flowing.
//!
//! Fix: fc-agent calls redirect_stderr_to_console() at startup, which opens
//! /dev/console directly and dup2's it to fd 1 and fd 2. This bypasses journald
//! entirely — eprintln!() writes go straight to /dev/console → ttyS0 → Firecracker
//! serial → host stdout. The console write path uses polled I/O (busy-wait on THRE),
//! so it works even with stale UART IER register after restore.
//!
//! Architecture:
//!   Guest eprintln!() → fd 2 (dup2'd to /dev/console) → kernel console driver
//!   → /dev/ttyS0 → Firecracker UART emulation → host stdout → tracing → log file

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Verify serial console works after warm start (snapshot restore to new Firecracker).
///
/// Phase 1: Boot VM to ensure snapshot is cached (cold start creates cache).
/// Phase 2: Boot second VM with same config → warm start from cache.
///          Write a marker to /dev/ttyS0 and verify it appears in host log.
#[tokio::test]
async fn test_serial_console_after_restore() -> Result<()> {
    println!("\nSerial console after restore test");
    println!("==================================");

    let marker = format!("UART-TEST-MARKER-{}", std::process::id());

    // Phase 1: Ensure a cached snapshot exists by running a VM to completion.
    // If the snapshot is already cached from a previous test run, this phase
    // still runs but completes faster (warm start). Either way, after this
    // phase there's a cached snapshot for Phase 2.
    println!("Phase 1: Ensuring cached snapshot exists...");
    let (vm_name_1, _, _, _) = common::unique_names("uart-p1");
    let (mut child1, pid1) =
        common::spawn_fcvm(&["podman", "run", "--name", &vm_name_1, common::TEST_IMAGE])
            .await
            .context("spawning phase 1 VM")?;

    common::poll_health_by_pid(pid1, 300).await?;
    println!("  Phase 1 VM healthy (PID: {})", pid1);

    common::kill_process(pid1).await;
    let _ = child1.wait().await;
    println!("  Phase 1 VM stopped, snapshot should be cached");

    // Brief pause to let the snapshot finalize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Phase 2: Start VM from cached snapshot → warm start (new Firecracker process).
    // Capture logs to verify serial console output.
    println!("Phase 2: Starting VM from cached snapshot...");
    let (vm_name_2, _, _, _) = common::unique_names("uart-p2");
    let (mut child2, pid2, log_path) = common::spawn_fcvm_with_log_path(
        &["podman", "run", "--name", &vm_name_2, common::TEST_IMAGE],
        &vm_name_2,
    )
    .await
    .context("spawning phase 2 VM")?;

    common::poll_health_by_pid(pid2, 120).await?;
    println!("  Phase 2 VM healthy (PID: {})", pid2);

    // Write a unique marker to /dev/ttyS0 from inside the VM.
    // If UART works, the marker flows through:
    //   echo → /dev/ttyS0 → Firecracker UART → Firecracker stdout → host log
    println!("  Writing marker to /dev/ttyS0: {}", marker);
    common::exec_in_vm(pid2, &[&format!("echo '{}' > /dev/ttyS0", marker)])
        .await
        .context("writing marker to /dev/ttyS0")?;

    // Give time for the serial output to flow through the pipeline
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Stop the VM
    common::kill_process(pid2).await;
    let _ = child2.wait().await;

    // Wait a moment for the log consumers to flush
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Analyze the log file
    let contents = std::fs::read_to_string(&log_path)
        .with_context(|| format!("reading log file: {}", log_path.display()))?;

    println!(
        "  Log file: {} ({} bytes)",
        log_path.display(),
        contents.len()
    );

    // Check if this was actually a warm start (restored from cached snapshot)
    let was_warm_start = contents.contains("Pre-start snapshot hit")
        || contents.contains("Startup snapshot hit")
        || contents.contains("Restoring from cached snapshot");
    println!(
        "  Was warm start (restored from snapshot): {}",
        was_warm_start
    );

    // Count fc-agent serial console lines — these come from eprintln!() in the
    // guest, flow through /dev/ttyS0, and appear in Firecracker's stdout.
    // After warm start, fc-agent prints restore messages (e.g. "handling restore").
    let fc_agent_serial_lines: Vec<&str> = contents
        .lines()
        .filter(|line| {
            // Lines from Firecracker's serial console containing fc-agent output
            line.contains("firecracker") && line.contains("[fc-agent]")
        })
        .collect();
    println!(
        "  fc-agent serial console lines: {}",
        fc_agent_serial_lines.len()
    );
    for line in &fc_agent_serial_lines {
        println!("    {}", line.trim());
    }

    // Check for our explicit marker
    let found_marker = contents.lines().any(|line| {
        line.contains(&marker)
            && !line.contains("args=")
            && !line.contains("\"cmd\"")
            && !line.contains("exec")
    });
    println!("  UART marker found in host log: {}", found_marker);

    // Check for fc-agent restore messages (only relevant on warm start)
    let found_restore_msg = contents.contains("[fc-agent] handling restore")
        || contents.contains("[fc-agent] restore complete");
    println!(
        "  fc-agent restore messages in host log: {}",
        found_restore_msg
    );

    // Assertions
    if was_warm_start {
        // On warm start, fc-agent prints restore messages via eprintln!().
        // If UART works, these appear in the host log.
        assert!(
            found_restore_msg,
            "fc-agent restore messages not found in host log after warm start — \
             serial console (UART) is broken after snapshot restore. \
             fc-agent IS running (VM is healthy, vsock works), but eprintln!() \
             output to /dev/ttyS0 never reaches Firecracker's stdout.\n\
             Log: {}",
            log_path.display()
        );

        assert!(
            found_marker,
            "UART marker '{}' not found in host log after warm start — \
             serial console is broken after snapshot restore.\n\
             Log: {}",
            marker,
            log_path.display()
        );
    } else {
        // Cold start — UART should definitely work
        assert!(
            found_marker,
            "UART marker '{}' not found even on cold start — \
             serial console is fundamentally broken.\n\
             Log: {}",
            marker,
            log_path.display()
        );
        println!("  NOTE: This was a cold start. Run the test again to test the restore path.");
    }

    println!("✅ SERIAL CONSOLE TEST PASSED!");
    Ok(())
}
