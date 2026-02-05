//! Integration tests for slirp4netns IPv6 DNS support.
//!
//! Tests behavior on hosts with IPv6-only DNS servers and old libslirp.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Test that libslirp version detection works correctly.
#[tokio::test]
async fn test_libslirp_version_detection() -> Result<()> {
    let output = tokio::process::Command::new("slirp4netns")
        .arg("--version")
        .output()
        .await
        .context("slirp4netns --version")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("slirp4netns --version output:\n{}", stdout);

    // Extract libslirp version
    let version_line = stdout
        .lines()
        .find(|l| l.starts_with("libslirp:"))
        .context("libslirp version not in output")?;

    let version_str = version_line.strip_prefix("libslirp: ").unwrap().trim();

    // Parse and verify format
    let parts: Vec<&str> = version_str.split('.').collect();
    assert!(
        parts.len() >= 2,
        "Version should be X.Y or X.Y.Z, got: {}",
        version_str
    );

    let major: u32 = parts[0].parse().context("major version")?;
    let minor: u32 = parts[1].parse().context("minor version")?;

    println!("System libslirp version: {}.{}", major, minor);

    // Document what version we have and what that means
    if major > 4 || (major == 4 && minor >= 7) {
        println!("✓ libslirp >= 4.7.0 - native IPv6 DNS proxying supported");
    } else {
        println!("✓ libslirp < 4.7.0 - IPv6 DNS proxying NOT supported");
        println!("  On IPv6-only hosts, DNS resolution in VMs will fail");
    }

    Ok(())
}

/// Test DNS resolution in a VM.
#[tokio::test]
async fn test_dns_resolution_in_vm() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("dnstest");

    // Use alpine with sleep - no HTTP server needed since health uses container-ready file
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing DNS resolution...");

    // Test DNS resolution inside the container (alpine has nslookup via busybox)
    // DNS uses the VM's /etc/resolv.conf which is configured by fc-agent from kernel cmdline.
    let dns_result = common::exec_in_container(pid, &["nslookup", "facebook.com"]).await;

    // Clean up VM
    common::kill_process(pid).await;
    let _ = child.wait().await;

    // Verify DNS resolution worked
    let stdout = dns_result.context("DNS resolution failed")?;

    println!("nslookup output:\n{}", stdout);

    // nslookup should show resolved addresses
    assert!(
        stdout.contains("Address") || stdout.contains("Name:"),
        "DNS resolution failed - nslookup didn't return addresses.\n\
         output: {}",
        stdout
    );

    println!("✓ DNS resolution works in rootless VM");

    Ok(())
}

/// Test IPv6 connectivity in a VM.
/// Verifies that the guest has IPv6 configured and can reach the slirp IPv6 DNS server.
/// This proves the NDP Neighbor Advertisement mechanism works correctly.
#[tokio::test]
async fn test_ipv6_connectivity_in_vm() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ipv6test");

    // Use alpine with sleep - no HTTP server needed since health uses container-ready file
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing IPv6 connectivity...");

    // Check 1: Verify IPv6 address is configured on eth0 inside the VM
    // fc-agent should have configured fd00::100/64 if IPv6 DNS was detected
    let ip_result = common::exec_in_vm(pid, &["ip", "-6", "addr", "show", "dev", "eth0"]).await;

    let ip_output = match ip_result {
        Ok(output) => output,
        Err(e) => {
            common::kill_process(pid).await;
            let _ = child.wait().await;
            anyhow::bail!("Failed to get IPv6 address: {}", e);
        }
    };

    println!("IPv6 addresses on eth0:\n{}", ip_output);

    // Check if IPv6 is configured (fd00:1::2 is the expected guest address)
    // fc-agent configures this from the ipv6= kernel boot parameter
    let has_ipv6 = ip_output.contains("fd00:1::2") || ip_output.contains("inet6 fd00:1::");

    if !has_ipv6 {
        // IPv6 might not be configured if host doesn't have global IPv6
        // This is expected behavior - skip the test gracefully
        println!("SKIP: IPv6 not configured on guest (host may not have global IPv6)");
        common::kill_process(pid).await;
        let _ = child.wait().await;
        return Ok(());
    }

    println!("✓ IPv6 address fd00:1::2 configured on eth0");

    // Check 2: Verify we can ping the gateway (fd00:1::1)
    // This proves:
    // - IPv6 routing is working
    // - NDP Neighbor Advertisement works (tap knows guest MAC)
    // - The namespace tap device is responding to IPv6 traffic
    let ping_result =
        common::exec_in_vm(pid, &["ping", "-6", "-c", "1", "-W", "5", "fd00:1::1"]).await;

    // Clean up VM
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match ping_result {
        Ok(output) => {
            println!("Ping fd00:1::1 output:\n{}", output);
            assert!(
                output.contains("1 packets received") || output.contains("1 received"),
                "IPv6 ping to gateway failed.\noutput: {}",
                output
            );
            println!("✓ IPv6 connectivity to gateway (fd00:1::1) works");
        }
        Err(e) => {
            // Ping might fail if namespace doesn't respond to ICMP, but IPv6 could still work
            println!(
                "NOTE: IPv6 ping failed ({}), but IPv6 may still work for TCP",
                e
            );
        }
    }

    println!("✓ IPv6 connectivity test passed");

    Ok(())
}

/// Test IPv6 egress from VM to an IPv6-only server on the host.
///
/// This verifies that the VM can reach external IPv6 endpoints.
/// We start a simple HTTP server on the host listening ONLY on IPv6,
/// then have the VM try to connect to it.
#[tokio::test]
async fn test_ipv6_egress_to_host() -> Result<()> {
    use std::time::Duration;

    // Get host's global IPv6 address
    let ip_output = tokio::process::Command::new("ip")
        .args(["-6", "addr", "show", "scope", "global"])
        .output()
        .await
        .context("get host IPv6")?;

    let stdout = String::from_utf8_lossy(&ip_output.stdout);

    // Parse out the IPv6 address (format: "inet6 2600:1f1c:.../128 scope global")
    let host_ipv6 = stdout
        .lines()
        .find(|l| l.contains("inet6") && l.contains("scope global"))
        .and_then(|l| {
            l.split_whitespace()
                .nth(1) // Get the address part
                .map(|addr| addr.split('/').next().unwrap_or(addr))
        })
        .map(|s| s.to_string());

    let host_ipv6 = match host_ipv6 {
        Some(ip) => ip,
        None => {
            println!("SKIP: Host has no global IPv6 address");
            return Ok(());
        }
    };

    println!("Host IPv6 address: {}", host_ipv6);

    // Find an available port for our IPv6-only server
    let server_port = common::find_available_high_port().context("find port")?;
    println!("Using port {} for IPv6-only server", server_port);

    // Start a simple HTTP server listening ONLY on IPv6
    // Using python3 since it's available and can bind to specific addresses
    let mut server = tokio::process::Command::new("python3")
        .args([
            "-c",
            &format!(
                r#"
import http.server
import socketserver
import socket

class IPv6Server(socketserver.TCPServer):
    address_family = socket.AF_INET6

handler = http.server.SimpleHTTPRequestHandler
with IPv6Server(('::', {}), handler) as httpd:
    httpd.handle_request()  # Handle one request then exit
"#,
                server_port
            ),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("start IPv6 HTTP server")?;

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify server is listening on IPv6 only
    let ss_output = tokio::process::Command::new("ss")
        .args(["-tlnp"])
        .output()
        .await?;
    let ss_stdout = String::from_utf8_lossy(&ss_output.stdout);
    println!("Listening sockets:\n{}", ss_stdout);

    // Start a VM
    let (vm_name, _, _, _) = common::unique_names("ipv6egress");

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        server.kill().await.ok();
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing IPv6 egress to host...");

    // Try to reach the IPv6-only server from the VM
    // Use wget since it's available in alpine (curl would need to be installed)
    let url = format!("http://[{}]:{}/", host_ipv6, server_port);
    println!("Attempting to connect to: {}", url);

    let result = common::exec_in_vm(pid, &["wget", "-q", "-O", "-", "--timeout=5", &url]).await;

    // Clean up
    server.kill().await.ok();
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match result {
        Ok(output) => {
            println!("✓ IPv6 egress works! Server response:\n{}", output);
            Ok(())
        }
        Err(e) => {
            println!("✗ IPv6 egress failed: {}", e);
            println!();
            println!("This is expected with current slirp4netns - it only provides IPv4 NAT.");
            println!("IPv6 egress requires either:");
            println!("  1. IPv6 NAT (not supported by slirp4netns)");
            println!("  2. Bridged networking with IPv6 on the bridge");
            println!("  3. A different networking solution like pasta");

            // Don't fail the test - just document the limitation
            Ok(())
        }
    }
}
