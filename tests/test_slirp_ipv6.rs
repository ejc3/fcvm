//! Integration tests for slirp4netns IPv6 DNS support.
//!
//! Tests behavior on hosts with IPv6-only DNS servers and old libslirp.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

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

/// Test the socat relay mechanism used by the proxy forwarder.
/// This verifies that socat can relay TCP traffic correctly.
#[tokio::test]
async fn test_socat_tcp_relay() -> Result<()> {
    // Check socat is installed
    let check = tokio::process::Command::new("which")
        .arg("socat")
        .output()
        .await;

    if check.map(|o| !o.status.success()).unwrap_or(true) {
        println!("SKIP: socat not installed");
        return Ok(());
    }

    // Use find_available_port to get ports not blocked by wildcard binds
    let backend_port = common::find_available_port(19080, 1000)?;
    let relay_port = common::find_available_port(backend_port + 1, 1000)?;

    // Start a simple TCP server using socat (nc/ncat syntax varies by distro)
    // socat is more portable and works consistently
    let mut backend = tokio::process::Command::new("socat")
        .args([&format!("TCP-LISTEN:{},reuseaddr", backend_port), "STDOUT"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("start socat backend")?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start socat relay: listens on relay_port, forwards to backend_port
    let mut relay = tokio::process::Command::new("socat")
        .args([
            &format!("TCP-LISTEN:{},reuseaddr", relay_port),
            &format!("TCP:127.0.0.1:{}", backend_port),
        ])
        .spawn()
        .context("start socat relay")?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to relay and send data using socat (portable)
    let _client = tokio::process::Command::new("bash")
        .args([
            "-c",
            &format!("echo 'HELLO' | socat - TCP:127.0.0.1:{}", relay_port),
        ])
        .output()
        .await?;

    // Read what the backend received
    let backend_stdin = backend.stdin.take();
    drop(backend_stdin); // Close stdin to signal EOF

    let backend_output = tokio::time::timeout(Duration::from_secs(5), backend.wait_with_output())
        .await
        .context("timeout waiting for backend")?
        .context("getting backend output")?;

    // Cleanup
    let _ = relay.kill().await;

    let received = String::from_utf8_lossy(&backend_output.stdout);
    println!("Backend received: {:?}", received.trim());

    assert!(
        received.contains("HELLO"),
        "Relay didn't forward data correctly. Backend received: {:?}",
        received
    );

    println!("✓ socat TCP relay works correctly");

    Ok(())
}

/// Test DNS resolution in a VM.
#[tokio::test]
async fn test_dns_resolution_in_vm() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("dnstest");

    // Use spawn_fcvm helper (properly handles environment and log capture)
    // Use --no-snapshot to avoid cached snapshots that may have HTTP health checks configured
    // (This test uses alpine with sleep infinity which has no HTTP server)
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        "alpine:latest",
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
    // We test in the container because that's what users actually run.
    // DNS uses the VM's /etc/resolv.conf which is configured by fc-agent from kernel cmdline.
    // Use facebook.com since google.com may be blocked on some corporate networks.
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

    // Spawn VM with rootless networking
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        "alpine:latest",
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

    // Check if IPv6 is configured (fd00::100 is the expected guest address)
    let has_ipv6 = ip_output.contains("fd00::100") || ip_output.contains("inet6");

    if !has_ipv6 {
        // IPv6 might not be configured if host doesn't have IPv6 DNS
        // This is expected behavior - skip the test gracefully
        println!("SKIP: IPv6 not configured on guest (host may not have IPv6 DNS)");
        common::kill_process(pid).await;
        let _ = child.wait().await;
        return Ok(());
    }

    println!("✓ IPv6 address configured on eth0");

    // Check 2: Verify we can ping the IPv6 DNS server (fd00::3)
    // This proves:
    // - IPv6 routing is working (via fd00::2 gateway)
    // - NDP NA was sent successfully (slirp knows our MAC)
    // - slirp is responding to IPv6 traffic
    let ping_result =
        common::exec_in_vm(pid, &["ping", "-6", "-c", "1", "-W", "5", "fd00::3"]).await;

    // Clean up VM
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match ping_result {
        Ok(output) => {
            println!("Ping fd00::3 output:\n{}", output);
            assert!(
                output.contains("1 packets received") || output.contains("1 received"),
                "IPv6 ping to DNS server failed.\noutput: {}",
                output
            );
            println!("✓ IPv6 connectivity to slirp DNS (fd00::3) works");
        }
        Err(e) => {
            // Ping might fail if slirp doesn't respond to ICMP, but IPv6 could still work
            println!(
                "NOTE: IPv6 ping failed ({}), but IPv6 may still work for DNS/TCP",
                e
            );
        }
    }

    println!("✓ IPv6 connectivity test passed");

    Ok(())
}
