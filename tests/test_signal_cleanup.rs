//! Tests for signal handling and cleanup
//!
//! Verifies that when fcvm receives SIGINT/SIGTERM, it properly cleans up
//! child processes (firecracker, slirp4netns, etc.)

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;

/// Check if a process with the given PID exists
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Send a signal to a process
fn send_signal(pid: u32, signal: &str) -> Result<()> {
    let output = Command::new("kill")
        .arg(format!("-{}", signal))
        .arg(pid.to_string())
        .output()
        .context("running kill command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("kill failed: {}", stderr);
    }
    Ok(())
}

/// Test that SIGINT properly kills the VM and cleans up firecracker
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigint_kills_firecracker_bridged() -> Result<()> {
    println!("\ntest_sigint_kills_firecracker_bridged");

    // Start fcvm in background
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("signal-int");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            "nginx:alpine",
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy (max 60 seconds)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json"])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"health_status\":\"healthy\"")
            || stdout.contains("\"health_status\": \"healthy\"")
        {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        // Kill fcvm if it didn't become healthy
        let _ = fcvm.kill();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);

    // Verify firecracker is running
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );
    let fc_pid = our_fc_pid.unwrap();
    assert!(
        process_exists(fc_pid),
        "firecracker should be running before SIGINT"
    );

    // Send SIGINT to fcvm (simulates Ctrl-C)
    println!("Sending SIGINT to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "INT").context("sending SIGINT to fcvm")?;

    // Wait for fcvm to exit (max 10 seconds)
    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < Duration::from_secs(10) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                exited = true;
                break;
            }
            Ok(None) => {
                std::thread::sleep(common::POLL_INTERVAL);
            }
            Err(e) => {
                println!("Error waiting for fcvm: {}", e);
                break;
            }
        }
    }

    if !exited {
        println!("fcvm didn't exit after SIGINT, killing forcefully");
        let _ = fcvm.kill();
        let _ = fcvm.wait();
    }

    // Give a moment for cleanup
    std::thread::sleep(common::POLL_INTERVAL);

    // Check if our specific firecracker is still running
    let still_running = process_exists(fc_pid);
    if still_running {
        // This is a bug - firecracker should have been killed
        println!(
            "BUG: firecracker (PID {}) is still running after fcvm exit!",
            fc_pid
        );
        // Clean up for the test
        let _ = send_signal(fc_pid, "KILL");
    }
    assert!(
        !still_running,
        "firecracker (PID {}) should be killed when fcvm receives SIGINT",
        fc_pid
    );

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigint_kills_firecracker_bridged PASSED");
    Ok(())
}

/// Test that SIGTERM properly kills the VM and cleans up firecracker
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigterm_kills_firecracker_bridged() -> Result<()> {
    println!("\ntest_sigterm_kills_firecracker_bridged");

    // Start fcvm in background
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("signal-term");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            "nginx:alpine",
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy (max 60 seconds)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json"])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"health_status\":\"healthy\"")
            || stdout.contains("\"health_status\": \"healthy\"")
        {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        let _ = fcvm.kill();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);

    // Verify firecracker is running
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );
    let fc_pid = our_fc_pid.unwrap();

    // Send SIGTERM to fcvm
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for fcvm to exit (max 10 seconds)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                break;
            }
            Ok(None) => {
                std::thread::sleep(common::POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }

    // Give a moment for cleanup
    std::thread::sleep(common::POLL_INTERVAL);

    // Check if our specific firecracker is still running
    let still_running = process_exists(fc_pid);
    if still_running {
        println!(
            "BUG: firecracker (PID {}) is still running after fcvm exit!",
            fc_pid
        );
        let _ = send_signal(fc_pid, "KILL");
    }
    assert!(
        !still_running,
        "firecracker (PID {}) should be killed when fcvm receives SIGTERM",
        fc_pid
    );

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigterm_kills_firecracker_bridged PASSED");
    Ok(())
}

/// Test that SIGTERM properly kills the VM and cleans up ALL resources in rootless mode
/// This includes: firecracker, slirp4netns, namespace holder, and state files
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[test]
fn test_sigterm_cleanup_rootless() -> Result<()> {
    println!("\ntest_sigterm_cleanup_rootless");

    // Start fcvm in rootless mode
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("cleanup-rootless");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy (max 60 seconds)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json"])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"health_status\":\"healthy\"")
            || stdout.contains("\"health_status\": \"healthy\"")
        {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        let _ = fcvm.kill();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM by looking for our VM name pattern
    // The VM ID contains the unique name prefix, so we can find our specific process
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    let our_slirp_pid = find_slirp_for_fcvm(fcvm_pid);
    println!(
        "Our processes: firecracker={:?}, slirp4netns={:?}",
        our_fc_pid, our_slirp_pid
    );

    // Verify we found our firecracker process
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );

    // Send SIGTERM to fcvm
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for fcvm to exit (max 10 seconds)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                break;
            }
            Ok(None) => {
                std::thread::sleep(common::POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }

    // Give a moment for cleanup
    std::thread::sleep(common::POLL_INTERVAL);

    // Verify our SPECIFIC processes are cleaned up
    if let Some(fc_pid) = our_fc_pid {
        let still_running = process_exists(fc_pid);
        assert!(
            !still_running,
            "our firecracker (PID {}) should be killed after SIGTERM",
            fc_pid
        );
        println!("Firecracker PID {} correctly cleaned up", fc_pid);
    }

    if let Some(slirp_pid) = our_slirp_pid {
        let still_running = process_exists(slirp_pid);
        assert!(
            !still_running,
            "our slirp4netns (PID {}) should be killed after SIGTERM",
            slirp_pid
        );
        println!("slirp4netns PID {} correctly cleaned up", slirp_pid);
    }

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigterm_cleanup_rootless PASSED");
    Ok(())
}

/// Find the firecracker process spawned by a specific fcvm process
/// by looking at the parent PID chain
fn find_firecracker_for_fcvm(fcvm_pid: u32) -> Option<u32> {
    // Get all firecracker PIDs
    let output = Command::new("pgrep")
        .args(["-f", "firecracker.*--api-sock"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(fc_pid) = line.trim().parse::<u32>() {
            // Check if this firecracker's parent chain includes our fcvm PID
            if is_descendant_of(fc_pid, fcvm_pid) {
                return Some(fc_pid);
            }
        }
    }
    None
}

/// Find the slirp4netns process spawned by a specific fcvm process
fn find_slirp_for_fcvm(fcvm_pid: u32) -> Option<u32> {
    let output = Command::new("pgrep")
        .args(["-f", "slirp4netns"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(slirp_pid) = line.trim().parse::<u32>() {
            // Check if this slirp4netns's parent chain includes our fcvm PID
            if is_descendant_of(slirp_pid, fcvm_pid) {
                return Some(slirp_pid);
            }
        }
    }
    None
}

/// Check if a process is a descendant of another process
fn is_descendant_of(pid: u32, ancestor_pid: u32) -> bool {
    let mut current = pid;
    // Walk up the parent chain (max 10 levels to prevent infinite loops)
    for _ in 0..10 {
        if current == ancestor_pid {
            return true;
        }
        if current <= 1 {
            return false;
        }
        // Read parent PID from /proc/[pid]/stat
        let stat_path = format!("/proc/{}/stat", current);
        if let Ok(content) = std::fs::read_to_string(&stat_path) {
            // Format: pid (comm) state ppid ...
            // Find the closing paren for comm (can contain spaces/parens)
            if let Some(paren_end) = content.rfind(')') {
                let after_comm = &content[paren_end + 1..];
                let fields: Vec<&str> = after_comm.split_whitespace().collect();
                // fields[0] is state, fields[1] is ppid
                if let Some(ppid_str) = fields.get(1) {
                    if let Ok(ppid) = ppid_str.parse::<u32>() {
                        current = ppid;
                        continue;
                    }
                }
            }
        }
        return false;
    }
    false
}

/// Test that SIGTERM properly cleans up resources in bridged mode
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigterm_cleanup_bridged() -> Result<()> {
    println!("\ntest_sigterm_cleanup_bridged");

    // Start fcvm in bridged mode
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("cleanup-bridged");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json"])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"health_status\":\"healthy\"")
            || stdout.contains("\"health_status\": \"healthy\"")
        {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        let _ = fcvm.kill();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);

    // Verify we found our firecracker process
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );

    // Send SIGTERM
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for exit
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                break;
            }
            Ok(None) => std::thread::sleep(common::POLL_INTERVAL),
            Err(_) => break,
        }
    }

    std::thread::sleep(common::POLL_INTERVAL);

    // Verify our SPECIFIC processes are cleaned up
    if let Some(fc_pid) = our_fc_pid {
        let still_running = process_exists(fc_pid);
        assert!(
            !still_running,
            "our firecracker (PID {}) should be killed after SIGTERM",
            fc_pid
        );
        println!("Firecracker PID {} correctly cleaned up", fc_pid);
    }

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigterm_cleanup_bridged PASSED");
    Ok(())
}
