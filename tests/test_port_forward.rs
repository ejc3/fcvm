//! Tests for port forwarding functionality
//!
//! Verifies that --publish correctly forwards ports from host to guest

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Command;
use std::time::Duration;

/// Deserialization helper for VM state from fcvm ls --json
/// Uses the actual fcvm types for correct parsing
#[derive(Deserialize)]
struct VmDisplay {
    #[serde(flatten)]
    vm: fcvm::state::VmState,
    #[allow(dead_code)]
    stale: bool,
}

/// Test port forwarding with bridged networking
#[cfg(feature = "privileged-tests")]
#[test]
fn test_port_forward_bridged() -> Result<()> {
    println!("\ntest_port_forward_bridged");

    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("port-bridged-{}", std::process::id());

    // Port 8080:80 - DNAT is scoped to veth IP so same port works across parallel VMs
    let host_port: u16 = 8080;

    // Start VM with port forwarding
    // Use --health-check to wait for nginx to be ready (not just container running)
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            "--publish",
            "8080:80",
            "--health-check",
            "http://localhost/",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy
    let start = std::time::Instant::now();
    let mut healthy = false;
    let mut guest_ip = String::new();
    let mut veth_host_ip = String::new();

    while start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Parse JSON using actual fcvm types
        let vms: Vec<VmDisplay> = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Find our VM and check health (filtered by PID so should be only one)
        if let Some(display) = vms.first() {
            if matches!(display.vm.health_status, fcvm::state::HealthStatus::Healthy) {
                // Extract guest_ip and host_ip (veth's host IP) from config.network
                if let Some(ref ip) = display.vm.config.network.guest_ip {
                    guest_ip = ip.clone();
                }
                if let Some(ref ip) = display.vm.config.network.host_ip {
                    veth_host_ip = ip.clone();
                }
                healthy = true;
                println!(
                    "VM is healthy, guest_ip: {}, veth_host_ip: {}",
                    guest_ip, veth_host_ip
                );
                break;
            }
        }
    }

    if !healthy {
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Test 1: Direct access to guest IP should work
    // Retry loop: Container is marked healthy when it starts, but nginx needs a moment to bind to port 80
    println!("Testing direct access to guest...");
    let mut direct_works = false;
    let retry_start = std::time::Instant::now();
    while retry_start.elapsed() < Duration::from_secs(30) {
        let output = Command::new("curl")
            .args(["-s", "--max-time", "2", &format!("http://{}:80", guest_ip)])
            .output()
            .context("curl to guest")?;

        if output.status.success() && !output.stdout.is_empty() {
            direct_works = true;
            println!("Direct access: OK");
            println!(
                "Response: {}",
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if !direct_works {
        println!("Direct access: FAIL (timed out after 30s)");
    }

    // Test 2: Access via port forwarding (veth's host IP)
    // DNAT rules are scoped to the veth IP, so this is what we test
    println!(
        "Testing port forwarding via veth IP {}:{}...",
        veth_host_ip, host_port
    );
    let mut forward_works = false;
    let retry_start = std::time::Instant::now();
    while retry_start.elapsed() < Duration::from_secs(30) {
        let output = Command::new("curl")
            .args([
                "-s",
                "--max-time",
                "2",
                &format!("http://{}:{}", veth_host_ip, host_port),
            ])
            .output()
            .context("curl to forwarded port")?;

        if output.status.success() && !output.stdout.is_empty() {
            forward_works = true;
            println!("Port forwarding (veth IP): OK");
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if !forward_works {
        println!("Port forwarding (veth IP): FAIL (timed out after 30s)");
    }

    // Cleanup
    println!("Cleaning up...");
    let _ = Command::new("kill")
        .args(["-TERM", &fcvm_pid.to_string()])
        .output();

    std::thread::sleep(common::POLL_INTERVAL);
    let _ = fcvm.wait();

    // Assertions - both direct and port forwarding must work
    assert!(direct_works, "Direct access to guest should work");
    assert!(forward_works, "Port forwarding via veth IP should work");

    println!("test_port_forward_bridged PASSED");
    Ok(())
}

/// Test port forwarding with rootless (slirp4netns) networking
///
/// Rootless mode uses unique loopback IPs (127.x.y.z) for each VM,
/// allowing multiple VMs to all forward the same port.
#[test]
fn test_port_forward_rootless() -> Result<()> {
    println!("\ntest_port_forward_rootless");

    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("port-rootless-{}", std::process::id());

    // Use dynamic port to avoid conflicts with system services
    let host_port = common::find_available_high_port().context("finding available port")?;
    let publish_arg = format!("{}:80", host_port);

    // Start VM with rootless networking and port forwarding
    // Rootless uses unique loopback IPs (127.x.y.z) per VM
    // Use --health-check to wait for nginx to be ready (not just container running)
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--publish",
            &publish_arg,
            "--health-check",
            "http://localhost/",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy
    let start = std::time::Instant::now();
    let mut healthy = false;
    let mut loopback_ip = String::new();

    while start.elapsed() < Duration::from_secs(90) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Parse JSON using actual fcvm types
        let vms: Vec<VmDisplay> = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Find our VM and check health (filtered by PID so should be only one)
        if let Some(display) = vms.first() {
            if matches!(display.vm.health_status, fcvm::state::HealthStatus::Healthy) {
                // Extract loopback_ip from config.network (used for port forwarding in rootless)
                if let Some(ref ip) = display.vm.config.network.loopback_ip {
                    loopback_ip = ip.clone();
                }
                healthy = true;
                println!("VM is healthy, loopback_ip: {}", loopback_ip);
                break;
            }
        }
    }

    if !healthy {
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 90 seconds");
    }

    // Test: Access via loopback IP and forwarded port
    // In rootless mode, each VM gets a unique 127.x.y.z IP
    // Retry loop: Container is marked healthy when it starts, but nginx needs a moment to bind to port 80
    println!(
        "Testing access via loopback IP {}:{}...",
        loopback_ip, host_port
    );
    let mut loopback_works = false;
    let retry_start = std::time::Instant::now();
    while retry_start.elapsed() < Duration::from_secs(30) {
        let output = Command::new("curl")
            .args([
                "-s",
                "--max-time",
                "2",
                &format!("http://{}:{}", loopback_ip, host_port),
            ])
            .output()
            .context("curl to loopback")?;

        if output.status.success() && !output.stdout.is_empty() {
            loopback_works = true;
            println!("Loopback access: OK");
            println!(
                "Response: {}",
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if !loopback_works {
        println!("Loopback access: FAIL (timed out after 30s)");
    }

    // Cleanup
    println!("Cleaning up...");
    let _ = Command::new("kill")
        .args(["-TERM", &fcvm_pid.to_string()])
        .output();

    std::thread::sleep(common::POLL_INTERVAL);
    let _ = fcvm.wait();

    // Assertions
    assert!(
        loopback_works,
        "Rootless port forwarding via loopback IP should work"
    );

    println!("test_port_forward_rootless PASSED");
    Ok(())
}
