//! Network debugging test - comprehensive network analysis for diagnosing connectivity issues
//!
//! This test dumps ALL network configuration from inside the VM to help diagnose
//! why networking works on EC2 but fails on CI runners like BuildJet.
//!
//! Run with: cargo test --release -p fcvm --test test_network_debug -- --nocapture

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Run a command in the VM and print it with a header
async fn dump_command(pid: u32, title: &str, cmd: &[&str]) -> Result<()> {
    println!("\n=== {} ===", title);
    println!("$ {}", cmd.join(" "));
    match common::exec_in_vm(pid, cmd).await {
        Ok(output) => println!("{}", output),
        Err(e) => println!("ERROR: {}", e),
    }
    Ok(())
}

/// Comprehensive network debug for bridged mode
#[tokio::test]
async fn test_network_debug_bridged() -> Result<()> {
    network_debug_impl("bridged").await
}

/// Comprehensive network debug for rootless mode
#[tokio::test]
async fn test_network_debug_rootless() -> Result<()> {
    network_debug_impl("rootless").await
}

async fn network_debug_impl(network: &str) -> Result<()> {
    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  COMPREHENSIVE NETWORK DEBUG TEST                            ║");
    println!("║  Network mode: {:46} ║", network);
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Start the VM
    println!("\n>>> Starting VM...");
    let vm_name = format!("netdebug-{}", network);
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
    println!(">>> fcvm process started (PID: {})", fcvm_pid);

    println!(">>> Waiting for VM to become healthy (timeout: 300 seconds)...");

    // Use longer timeout for first run with rootfs creation
    let health_task = tokio::spawn(common::poll_health_by_pid(fcvm_pid, 300));

    // Also spawn a task to watch for early exit
    let mut child_for_monitor = child;
    let monitor_task: tokio::task::JoinHandle<Result<(), anyhow::Error>> =
        tokio::spawn(async move {
            loop {
                match child_for_monitor.try_wait() {
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
    let health_result = tokio::select! {
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

    if let Err(e) = &health_result {
        println!("\n!!! VM FAILED TO BECOME HEALTHY: {}", e);
        println!("!!! Cannot run network diagnostics without healthy VM");
        common::kill_process(fcvm_pid).await;
        return Err(anyhow::anyhow!("VM failed to become healthy: {}", e));
    }

    println!(">>> VM is healthy! Starting network diagnostics...\n");

    // ============================================================================
    // SECTION 1: Kernel Boot Parameters
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 1: KERNEL BOOT PARAMETERS                          │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "Kernel Command Line", &["cat", "/proc/cmdline"]).await?;

    // ============================================================================
    // SECTION 2: DNS Configuration
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 2: DNS CONFIGURATION                               │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "/etc/resolv.conf contents", &["cat", "/etc/resolv.conf"]).await?;
    dump_command(fcvm_pid, "/etc/resolv.conf symlink target", &["ls", "-la", "/etc/resolv.conf"]).await?;
    dump_command(fcvm_pid, "systemd-resolved status", &["systemctl", "status", "systemd-resolved", "||", "echo", "not running"]).await?;
    dump_command(fcvm_pid, "resolvectl status", &["resolvectl", "status", "||", "echo", "resolvectl not available"]).await?;

    // ============================================================================
    // SECTION 3: Network Interfaces
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 3: NETWORK INTERFACES                              │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "ip addr show", &["ip", "addr", "show"]).await?;
    dump_command(fcvm_pid, "ip link show", &["ip", "link", "show"]).await?;

    // ============================================================================
    // SECTION 4: Routing
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 4: ROUTING                                         │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "ip route show", &["ip", "route", "show"]).await?;
    dump_command(fcvm_pid, "ip route get 8.8.8.8", &["ip", "route", "get", "8.8.8.8"]).await?;

    // ============================================================================
    // SECTION 5: systemd-networkd Status
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 5: SYSTEMD-NETWORKD STATUS                         │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "systemctl status systemd-networkd", &["systemctl", "status", "systemd-networkd"]).await?;
    dump_command(fcvm_pid, "networkctl status", &["networkctl", "status", "||", "echo", "networkctl not available"]).await?;
    dump_command(fcvm_pid, "networkctl list", &["networkctl", "list", "||", "echo", "networkctl not available"]).await?;

    // ============================================================================
    // SECTION 6: DNS Setup Service Status
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 6: DNS SETUP SERVICE STATUS                        │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "fcvm-setup-dns service status", &["systemctl", "status", "fcvm-setup-dns", "||", "echo", "service not found"]).await?;
    dump_command(fcvm_pid, "fcvm-setup-dns service logs", &["journalctl", "-u", "fcvm-setup-dns", "--no-pager", "||", "echo", "no logs"]).await?;
    dump_command(fcvm_pid, "DNS setup script contents", &["cat", "/usr/local/bin/fcvm-setup-dns", "||", "echo", "script not found"]).await?;

    // ============================================================================
    // SECTION 7: Connectivity Tests
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 7: CONNECTIVITY TESTS                              │");
    println!("└──────────────────────────────────────────────────────────────┘");

    // Find gateway from routing table for ping test
    dump_command(fcvm_pid, "Ping gateway (from route)", &["sh", "-c", "GATEWAY=$(ip route | grep default | awk '{print $3}'); echo \"Gateway: $GATEWAY\"; ping -c 3 $GATEWAY 2>&1 || echo 'ping failed'"]).await?;
    dump_command(fcvm_pid, "Ping 8.8.8.8", &["ping", "-c", "3", "8.8.8.8", "||", "echo", "ping failed"]).await?;
    dump_command(fcvm_pid, "Ping 1.1.1.1", &["ping", "-c", "3", "1.1.1.1", "||", "echo", "ping failed"]).await?;

    // ============================================================================
    // SECTION 8: DNS Resolution Tests
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 8: DNS RESOLUTION TESTS                            │");
    println!("└──────────────────────────────────────────────────────────────┘");

    // Test with different DNS servers
    dump_command(fcvm_pid, "nslookup google.com (default)", &["nslookup", "google.com", "||", "echo", "nslookup failed"]).await?;
    dump_command(fcvm_pid, "nslookup google.com @ gateway", &["sh", "-c", "GATEWAY=$(ip route | grep default | awk '{print $3}'); nslookup google.com $GATEWAY 2>&1 || echo 'nslookup failed'"]).await?;
    dump_command(fcvm_pid, "nslookup google.com @ 8.8.8.8", &["nslookup", "google.com", "8.8.8.8", "||", "echo", "nslookup failed"]).await?;

    // Test dig if available
    dump_command(fcvm_pid, "dig google.com (if available)", &["dig", "google.com", "+short", "||", "echo", "dig not available"]).await?;

    // ============================================================================
    // SECTION 9: HTTP Connectivity Tests
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 9: HTTP CONNECTIVITY TESTS                         │");
    println!("└──────────────────────────────────────────────────────────────┘");

    // Test curl with various endpoints
    dump_command(fcvm_pid, "curl ifconfig.me", &["curl", "-s", "--max-time", "10", "http://ifconfig.me", "||", "echo", "curl failed"]).await?;
    dump_command(fcvm_pid, "curl https://ifconfig.me (SSL)", &["curl", "-s", "--max-time", "10", "https://ifconfig.me", "||", "echo", "curl failed"]).await?;

    // Test container registry access
    dump_command(fcvm_pid, "curl public.ecr.aws (DNS test)", &["curl", "-s", "--max-time", "10", "-I", "https://public.ecr.aws", "||", "echo", "curl failed"]).await?;

    // ============================================================================
    // SECTION 10: dmesg/kernel messages
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 10: KERNEL MESSAGES (last 50 lines)                │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "dmesg | tail -50", &["dmesg", "|", "tail", "-50"]).await?;

    // ============================================================================
    // SECTION 11: fc-agent logs
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 11: FC-AGENT LOGS                                  │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "journalctl -u fc-agent", &["journalctl", "-u", "fc-agent", "--no-pager", "-n", "50"]).await?;

    // ============================================================================
    // SECTION 12: Podman container status (if running)
    // ============================================================================
    println!("\n");
    println!("┌──────────────────────────────────────────────────────────────┐");
    println!("│  SECTION 12: PODMAN CONTAINER STATUS                        │");
    println!("└──────────────────────────────────────────────────────────────┘");

    dump_command(fcvm_pid, "podman ps", &["podman", "ps", "-a"]).await?;
    dump_command(fcvm_pid, "podman logs (first container)", &["sh", "-c", "podman logs $(podman ps -q | head -1) 2>&1 | tail -30 || echo 'no containers'"]).await?;

    // ============================================================================
    // CLEANUP
    // ============================================================================
    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  NETWORK DEBUG COMPLETE                                      ║");
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Cleanup
    println!("\n>>> Stopping fcvm process...");
    common::kill_process(fcvm_pid).await;
    println!(">>> Done!");

    Ok(())
}
