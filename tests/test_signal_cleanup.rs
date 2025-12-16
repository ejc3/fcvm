//! Tests for signal handling and cleanup
//!
//! Verifies that when fcvm receives SIGINT/SIGTERM, it properly cleans up
//! child processes (firecracker, slirp4netns, etc.)

mod common;

use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;

/// Check if a process with the given PID exists
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Find firecracker process spawned by a given fcvm PID
fn find_firecracker_pid(_fcvm_pid: u32) -> Option<u32> {
    // Look for firecracker processes
    let output = Command::new("pgrep")
        .args(["-f", "firecracker.*--api-sock"])
        .output()
        .ok()?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Return the most recent firecracker (highest PID, likely ours)
        stdout
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .max()
    } else {
        None
    }
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
#[test]
fn test_sigint_kills_firecracker() -> Result<()> {
    // This test requires root for bridged networking
    if !nix::unistd::geteuid().is_root() {
        eprintln!("Skipping test_sigint_kills_firecracker: requires root");
        return Ok(());
    }

    println!("\ntest_sigint_kills_firecracker");

    // Get initial firecracker count
    let initial_fc_count = Command::new("pgrep")
        .args(["-c", "firecracker"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    println!("Initial firecracker count: {}", initial_fc_count);

    // Start fcvm in background
    let fcvm_path = common::find_fcvm_binary()?;
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            "signal-test",
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
        std::thread::sleep(Duration::from_secs(2));

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

    // Find the firecracker process
    let fc_pid = find_firecracker_pid(fcvm_pid);
    println!("Firecracker PID: {:?}", fc_pid);

    // Verify firecracker is running
    if let Some(pid) = fc_pid {
        assert!(
            process_exists(pid),
            "firecracker should be running before SIGINT"
        );
    }

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
                std::thread::sleep(Duration::from_millis(100));
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
    std::thread::sleep(Duration::from_secs(2));

    // Check if firecracker is still running
    if let Some(pid) = fc_pid {
        let still_running = process_exists(pid);
        if still_running {
            // This is the bug - firecracker should have been killed
            println!(
                "BUG: firecracker (PID {}) is still running after fcvm exit!",
                pid
            );

            // Clean up for the test
            let _ = send_signal(pid, "KILL");
        }
        assert!(
            !still_running,
            "firecracker should be killed when fcvm receives SIGINT"
        );
    }

    // Verify no new orphan firecrackers
    let final_fc_count = Command::new("pgrep")
        .args(["-c", "firecracker"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    println!("Final firecracker count: {}", final_fc_count);
    assert!(
        final_fc_count <= initial_fc_count,
        "should not leave orphan firecracker processes (initial: {}, final: {})",
        initial_fc_count,
        final_fc_count
    );

    println!("test_sigint_kills_firecracker PASSED");
    Ok(())
}

/// Test that SIGTERM properly kills the VM and cleans up firecracker
#[test]
fn test_sigterm_kills_firecracker() -> Result<()> {
    // This test requires root for bridged networking
    if !nix::unistd::geteuid().is_root() {
        eprintln!("Skipping test_sigterm_kills_firecracker: requires root");
        return Ok(());
    }

    println!("\ntest_sigterm_kills_firecracker");

    // Start fcvm in background
    let fcvm_path = common::find_fcvm_binary()?;
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            "signal-test-term",
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
        std::thread::sleep(Duration::from_secs(2));

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

    // Find the firecracker process
    let fc_pid = find_firecracker_pid(fcvm_pid);
    println!("Firecracker PID: {:?}", fc_pid);

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
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }

    // Give a moment for cleanup
    std::thread::sleep(Duration::from_secs(2));

    // Check if firecracker is still running
    if let Some(pid) = fc_pid {
        let still_running = process_exists(pid);
        if still_running {
            println!(
                "BUG: firecracker (PID {}) is still running after fcvm exit!",
                pid
            );
            let _ = send_signal(pid, "KILL");
        }
        assert!(
            !still_running,
            "firecracker should be killed when fcvm receives SIGTERM"
        );
    }

    println!("test_sigterm_kills_firecracker PASSED");
    Ok(())
}
