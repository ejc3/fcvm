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
            "nginx:alpine",
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

    while start.elapsed() < Duration::from_secs(60) {
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
        let _ = fcvm.kill();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Test 1: Direct access to guest IP should work
    println!("Testing direct access to guest...");
    let output = Command::new("curl")
        .args(["-s", "--max-time", "5", &format!("http://{}:80", guest_ip)])
        .output()
        .context("curl to guest")?;

    let direct_works = output.status.success() && !output.stdout.is_empty();
    println!(
        "Direct access: {}",
        if direct_works { "OK" } else { "FAIL" }
    );

    if direct_works {
        println!(
            "Response: {}",
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
        );
    }

    // Test 2: Access via port forwarding (veth's host IP)
    // DNAT rules are scoped to the veth IP, so this is what we test
    println!(
        "Testing port forwarding via veth IP {}:{}...",
        veth_host_ip, host_port
    );
    let output = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "5",
            &format!("http://{}:{}", veth_host_ip, host_port),
        ])
        .output()
        .context("curl to forwarded port")?;

    let forward_works = output.status.success() && !output.stdout.is_empty();
    println!(
        "Port forwarding (veth IP): {}",
        if forward_works { "OK" } else { "FAIL" }
    );

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

    // Start VM with rootless networking and port forwarding
    // Rootless uses unique loopback IPs (127.x.y.z) per VM, so port 8080 is fine
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--publish",
            "8080:80",
            "nginx:alpine",
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
        let _ = fcvm.kill();
        anyhow::bail!("VM did not become healthy within 90 seconds");
    }

    // Test: Access via loopback IP and forwarded port
    // In rootless mode, each VM gets a unique 127.x.y.z IP
    println!("Testing access via loopback IP {}:8080...", loopback_ip);
    let output = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "5",
            &format!("http://{}:8080", loopback_ip),
        ])
        .output()
        .context("curl to loopback")?;

    let loopback_works = output.status.success() && !output.stdout.is_empty();
    println!(
        "Loopback access: {}",
        if loopback_works { "OK" } else { "FAIL" }
    );

    if loopback_works {
        println!(
            "Response: {}",
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
        );
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
