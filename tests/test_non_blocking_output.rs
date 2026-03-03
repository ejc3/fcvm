//! Verifies that --non-blocking-output delivers container output best-effort.
//!
//! In non-blocking mode, fc-agent uses try_send (drops if channel full) and
//! the host listener uses a bounded sync_channel with try_send. This prevents
//! a slow pipe reader from deadlocking the container, but output lines that
//! can't be written fast enough are silently dropped.
//!
//! This test verifies that under NORMAL load (not flooding), all output lines
//! arrive on the host — proving the non-blocking path works end-to-end without
//! gratuitously dropping messages.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

#[tokio::test]
async fn test_non_blocking_output_delivers_lines() -> Result<()> {
    println!("\nNon-blocking output: best-effort delivery");
    println!("==========================================");

    let (vm_name, _, _, _) = common::unique_names("nbout");

    // Write 100 numbered lines with a small delay between each (not flooding).
    // At this rate, the non-blocking channel (4096 deep) should never fill up,
    // so ALL lines should arrive on the host.
    let cmd = "i=0; while [ $i -lt 100 ]; do echo \"NB_LINE_$i\"; i=$((i+1)); sleep 0.01; done; echo NB_DONE; sleep 120";

    println!("  Starting VM with --non-blocking-output...");
    let (mut child, fcvm_pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--non-blocking-output",
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            cmd,
        ],
        &vm_name,
    )
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Log path: {}", log_path.display());
    println!("  Waiting for healthy...");

    common::poll_health_by_pid(fcvm_pid, 300).await?;
    println!("  VM healthy");

    // Wait for the output to complete (100 lines * 10ms = ~1s, plus margin)
    println!("  Waiting for output to complete...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Verify container is responsive
    let result = common::exec_in_container(fcvm_pid, &["echo", "alive"]).await?;
    assert!(
        result.contains("alive"),
        "container should be responsive, got: {}",
        result.trim()
    );
    println!("  Container responsive");

    // Stop VM before reading logs (ensures all output is flushed)
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    // Read the log file and count NB_LINE_ occurrences
    let log_content = tokio::fs::read_to_string(&log_path)
        .await
        .context("reading log file")?;

    let mut found_lines: Vec<u32> = Vec::new();
    let mut found_done = false;
    for line in log_content.lines() {
        if let Some(rest) = line.split("NB_LINE_").last() {
            if let Ok(n) = rest.trim().parse::<u32>() {
                found_lines.push(n);
            }
        }
        if line.contains("NB_DONE") {
            found_done = true;
        }
    }

    // Verify they arrived in order (no reordering in the pipeline).
    // Must check before sort/dedup, otherwise the assertion is vacuous.
    let is_sorted = found_lines.windows(2).all(|w| w[0] <= w[1]);
    assert!(is_sorted, "output lines should be in order");

    found_lines.sort();
    found_lines.dedup();

    println!("  Found {}/100 NB_LINE markers", found_lines.len());
    println!("  Found NB_DONE: {}", found_done);

    // We expect ALL 100 lines to arrive since we're not flooding.
    // Allow a small margin (95%) in case of timing edge cases.
    assert!(
        found_lines.len() >= 95,
        "expected at least 95/100 lines in non-blocking mode, got {}/100\n\
         missing: {:?}",
        found_lines.len(),
        (0..100u32)
            .filter(|n| !found_lines.contains(n))
            .collect::<Vec<_>>()
    );
    assert!(
        found_done,
        "NB_DONE marker should appear in output (container completed)"
    );

    println!(
        "  PASSED: {}/100 lines delivered in non-blocking mode",
        found_lines.len()
    );
    Ok(())
}

/// Verify that non-blocking mode prevents container deadlock under back-pressure.
///
/// Simulates a flooding container (no delays between writes). Even if some lines
/// are dropped, the container must NOT deadlock — it should complete and remain
/// responsive.
#[tokio::test]
async fn test_non_blocking_output_no_deadlock_under_flood() -> Result<()> {
    println!("\nNon-blocking output: no deadlock under flood");
    println!("=============================================");

    let (vm_name, _, _, _) = common::unique_names("nbflood");

    // Write 50000 lines as fast as possible, then signal completion
    let cmd = "i=0; while [ $i -lt 50000 ]; do echo \"FLOOD_$i\"; i=$((i+1)); done; echo FLOOD_COMPLETE; sleep 120";

    println!("  Starting VM with --non-blocking-output (flood test)...");
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--non-blocking-output",
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            cmd,
        ],
        &vm_name,
    )
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for healthy...");

    common::poll_health_by_pid(fcvm_pid, 300).await?;
    println!("  VM healthy");

    // Wait for flood to complete
    println!("  Waiting for flood (50000 lines)...");
    tokio::time::sleep(Duration::from_secs(30)).await;

    // The key assertion: container must still be responsive (not deadlocked)
    println!("  Checking container responsiveness after flood...");
    let result = common::exec_in_container(fcvm_pid, &["echo", "survived"]).await?;
    assert!(
        result.contains("survived"),
        "container should survive flood without deadlock, got: {}",
        result.trim()
    );

    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("  PASSED: container survived 50000-line flood without deadlock");
    Ok(())
}
